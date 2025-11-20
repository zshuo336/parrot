//! Probe receipts commit under the table lock and notify outside it.
use super::*;
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Weak,
    },
    task::{Context, Wake, Waker},
};
use tokio::sync::mpsc;

type Request = (RequestGuard, oneshot::Receiver<ProbeOutcome>);
struct Probe {
    state: Weak<ProbeState>,
    source: FrameSender,
    calls: AtomicUsize,
    locked: AtomicBool,
    premature: AtomicBool,
    rejected: AtomicBool,
    reentered: AtomicBool,
    closing: bool,
    throwing: u8,
    retired: Arc<AtomicUsize>,
    nested: Mutex<Option<Request>>,
}
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("controlled probe wake payload retirement");
    }
}
impl Wake for Probe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(state) = self.state.upgrade() {
            let unlocked = state.inner.try_lock().is_ok();
            self.locked.fetch_or(!unlocked, Ordering::SeqCst);
            if unlocked {
                let (empty, closed) = {
                    let inner = state.inner.lock().unwrap();
                    (inner.pending.is_empty(), inner.closed)
                };
                self.premature.fetch_or(
                    !empty || (self.closing && (!closed || !state.helpers.is_closed())),
                    Ordering::SeqCst,
                );
                let request = state.register(&self.source, "nested", true);
                self.rejected.store(request.is_none(), Ordering::SeqCst);
                *self.nested.lock().unwrap() = request;
                if self.closing {
                    state.close();
                }
                self.reentered.store(true, Ordering::SeqCst);
            }
        }
        match self.throwing {
            1 => panic!("controlled probe wake panic"),
            2 => std::panic::panic_any(Payload(self.retired.clone())),
            _ => {}
        }
    }
}
fn source() -> (FrameSender, mpsc::Receiver<Frame>) {
    let (tx, rx) = mpsc::channel(4);
    (FrameSender::new(tx, "probe-peer"), rx)
}
fn probe(state: &Arc<ProbeState>, source: &FrameSender, closing: bool, throwing: u8) -> Arc<Probe> {
    Arc::new(Probe {
        state: Arc::downgrade(state),
        source: source.clone(),
        calls: 0.into(),
        locked: false.into(),
        premature: false.into(),
        rejected: false.into(),
        reentered: false.into(),
        closing,
        throwing,
        retired: Arc::new(AtomicUsize::new(0)),
        nested: Mutex::new(None),
    })
}
fn park(rx: &mut oneshot::Receiver<ProbeOutcome>, probe: &Arc<Probe>) {
    assert!(Pin::new(rx)
        .poll(&mut Context::from_waker(&Waker::from(probe.clone())))
        .is_pending());
}
fn check(probe: &Arc<Probe>) {
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert!(
        !probe.locked.load(Ordering::SeqCst),
        "probe receipt woke under table lock"
    );
    assert!(
        !probe.premature.load(Ordering::SeqCst),
        "probe receipt preceded committed retirement or admission fence"
    );
    assert!(probe.reentered.load(Ordering::SeqCst));
    assert_eq!(probe.rejected.load(Ordering::SeqCst), probe.closing);
    assert_eq!(
        probe.retired.load(Ordering::SeqCst),
        usize::from(probe.throwing == 2)
    );
}
fn complete_nested(state: &Arc<ProbeState>, probe: &Arc<Probe>) {
    let nested = probe.nested.lock().unwrap().take();
    if probe.closing {
        assert!(nested.is_none());
        return;
    }
    let (guard, mut rx) = nested.expect("reentrant request must survive the original notification");
    assert!(state.complete(
        ProbeReply {
            request_id: guard.id,
            target: "nested".into(),
            outcome: ProbeOutcome::Failed,
        },
        true,
        &probe.source
    ));
    assert_eq!(rx.try_recv().unwrap(), ProbeOutcome::Failed);
    drop(guard);
}
fn single(mode: &str, throwing: u8) {
    let state = Arc::new(ProbeState::default());
    let (source, _peer) = source();
    let (guard, mut rx) = state.register(&source, "target", true).unwrap();
    let id = guard.id;
    let mut guard = Some(guard);
    let probe = probe(&state, &source, false, throwing);
    park(&mut rx, &probe);
    let cleanup = crate::ingress::unwind::Cleanup::default();
    let result = cleanup.call(|| match mode {
        "reply" => state.complete(
            ProbeReply {
                request_id: id,
                target: "target".into(),
                outcome: ProbeOutcome::Reachable,
            },
            true,
            &source,
        ),
        "cancel" => {
            drop(guard.take());
            true
        }
        _ => unreachable!(),
    });
    // Keep red-test cleanup independent of a poisoned lock from the old code.
    let poisoned = state.inner.is_poisoned();
    state.inner.clear_poison();
    drop(guard);
    assert!(!poisoned, "probe notification poisoned the registry");
    assert_eq!(
        result,
        Ok(true),
        "probe notification interrupted its caller"
    );
    if mode == "reply" {
        assert_eq!(rx.try_recv().unwrap(), ProbeOutcome::Reachable);
    } else {
        assert_eq!(
            rx.try_recv().unwrap_err(),
            oneshot::error::TryRecvError::Closed
        );
    }
    check(&probe);
    complete_nested(&state, &probe);
    assert!(state.inner.lock().unwrap().pending.is_empty());
    state.close();
}
fn close(throwing: u8) {
    let state = Arc::new(ProbeState::default());
    let (source, _peer) = source();
    let mut requests = Vec::new();
    let mut probes = Vec::new();
    for _ in 0..3 {
        let (guard, mut rx) = state.register(&source, "target", true).unwrap();
        let probe = probe(&state, &source, true, throwing);
        park(&mut rx, &probe);
        requests.push((guard, rx));
        probes.push(probe);
    }
    let cleanup = crate::ingress::unwind::Cleanup::default();
    let result = cleanup.call(|| state.close());
    let poisoned = state.inner.is_poisoned();
    state.inner.clear_poison();
    assert!(!poisoned, "probe close notification poisoned the registry");
    assert_eq!(result, Ok(()), "probe close interrupted its caller");
    for ((guard, mut rx), probe) in requests.into_iter().zip(probes) {
        assert_eq!(
            rx.try_recv().unwrap_err(),
            oneshot::error::TryRecvError::Closed
        );
        check(&probe);
        complete_nested(&state, &probe);
        drop(guard);
    }
    assert!(state.inner.lock().unwrap().pending.is_empty());
    assert!(state.register(&source, "target", true).is_none());
    assert!(state.helpers.clone().try_acquire_owned().is_err());
    state.close();
}

struct SourceWake {
    state: Weak<ProbeState>,
    id: u64,
    calls: AtomicUsize,
    locked: AtomicBool,
    retained: AtomicBool,
    throwing: u8,
    retired: Arc<AtomicUsize>,
}
impl Wake for SourceWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(state) = self.state.upgrade() {
            if let Ok(inner) = state.inner.try_lock() {
                self.retained
                    .store(inner.pending.contains_key(&self.id), Ordering::SeqCst);
            } else {
                self.locked.store(true, Ordering::SeqCst);
            }
        }
        match self.throwing {
            1 => panic!("controlled retired connection receiver wake"),
            2 => std::panic::panic_any(Payload(self.retired.clone())),
            _ => {}
        }
    }
}

fn retire_source(throwing: u8) {
    let state = Arc::new(ProbeState::default());
    let (source, mut peer) = source();
    let (guard, mut rx) = state.register(&source, "target", true).unwrap();
    let (other, _other_peer) = self::source();
    let receipt = probe(&state, &other, false, throwing);
    park(&mut rx, &receipt);
    let connection = Arc::new(SourceWake {
        state: Arc::downgrade(&state),
        id: guard.id,
        calls: 0.into(),
        locked: false.into(),
        retained: false.into(),
        throwing,
        retired: Arc::new(AtomicUsize::new(0)),
    });
    assert!(peer
        .poll_recv(&mut Context::from_waker(&Waker::from(connection.clone())))
        .is_pending());
    drop(source);
    let cleanup = crate::ingress::unwind::Cleanup::default();
    let result = cleanup.call(|| drop(guard));
    let poisoned = state.inner.is_poisoned();
    state.inner.clear_poison();
    assert!(!poisoned);
    assert_eq!(result, Ok(()), "one retiring owner interrupted its sibling");
    assert_eq!(
        rx.try_recv().unwrap_err(),
        oneshot::error::TryRecvError::Closed
    );
    assert_eq!(
        peer.try_recv().unwrap_err(),
        mpsc::error::TryRecvError::Disconnected
    );
    check(&receipt);
    assert_eq!(connection.calls.load(Ordering::SeqCst), 1);
    assert!(!connection.locked.load(Ordering::SeqCst));
    assert!(!connection.retained.load(Ordering::SeqCst));
    assert_eq!(
        connection.retired.load(Ordering::SeqCst),
        usize::from(throwing == 2)
    );
    complete_nested(&state, &receipt);
    state.close();
}

async fn exchange_progress(throwing: u8) {
    let state = Arc::new(ProbeState::default());
    let (source, mut peer) = source();
    let (_stop, stopped) = watch::channel(false);
    let mut previous_id = None;
    for indirect in [false, true] {
        let mut exchange = Box::pin(state.clone().exchange(
            source.clone(),
            "target".into(),
            indirect,
            Duration::from_secs(2),
            stopped.clone(),
        ));
        let receipt = probe(&state, &source, false, throwing);
        let waker = Waker::from(receipt.clone());
        assert!(exchange
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        let frame = peer.try_recv().unwrap();
        let crate::admin::SysEvent::MembershipProbe(event) =
            crate::admin::decode_sys_event(&frame.payload).unwrap()
        else {
            panic!("exchange did not send a membership request");
        };
        let request = match event {
            ProbeEvent::Ping(request) if !indirect => request,
            ProbeEvent::PingReq(request) if indirect => request,
            _ => panic!("wrong probe request kind"),
        };
        assert_eq!(request.target, "target");
        assert_eq!(frame.header.correlation_id, request.request_id);
        assert_ne!(previous_id, Some(request.request_id));
        previous_id = Some(request.request_id);
        let cleanup = crate::ingress::unwind::Cleanup::default();
        let result = cleanup.call(|| {
            state.complete(
                ProbeReply {
                    request_id: request.request_id,
                    target: request.target,
                    outcome: ProbeOutcome::Reachable,
                },
                indirect,
                &source,
            )
        });
        let poisoned = state.inner.is_poisoned();
        state.inner.clear_poison();
        assert!(!poisoned);
        assert_eq!(result, Ok(true));
        assert_eq!(
            exchange.as_mut().poll(&mut Context::from_waker(&waker)),
            std::task::Poll::Ready(ProbeOutcome::Reachable)
        );
        drop(exchange);
        check(&receipt);
        complete_nested(&state, &receipt);
        assert!(state.inner.lock().unwrap().pending.is_empty());
        assert!(!source.is_closed());
    }
    state.close();
    assert!(peer.try_recv().is_err());
}
#[derive(Clone, Debug, Default)]
struct Echo(Arc<Mutex<Vec<u64>>>);
impl ActorRef for Echo {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .push(message.downcast_ref::<crate::ingress::TestAsk>().unwrap().0);
            Ok(message)
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move { self.send(message).await.map(|_| ()) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/probe-echo".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
#[async_trait::async_trait]
impl crate::LocalLookup for Echo {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/probe-echo").then(|| Box::new(self.clone()) as BoxedActorRef)
    }
}
struct NetworkWake {
    calls: AtomicUsize,
    throwing: u8,
    retired: Arc<AtomicUsize>,
}
impl Wake for NetworkWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.throwing == 2 {
            std::panic::panic_any(Payload(self.retired.clone()));
        }
        panic!("controlled actual probe reply wake");
    }
}
async fn network_progress(throwing: u8) {
    use futures::FutureExt;
    let id = uuid::Uuid::new_v4();
    let echo = Echo::default();
    let config = |name: &str| {
        let mut config =
            crate::RemoteConfig::tcp(format!("{name}-{id}"), Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development();
        config.swim.probe_interval = Duration::from_secs(3600);
        config.swim.gossip_interval = Duration::from_secs(3600);
        config
    };
    let source =
        crate::RemoteActorSystem::new(config("observer"), Arc::new(Echo::default())).unwrap();
    let peer = crate::RemoteActorSystem::new(config("observed"), Arc::new(echo.clone())).unwrap();
    source.start().await.unwrap();
    peer.start().await.unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        source
            .connect(&crate::NodeAddr::tcp(
                &peer.config.node_id,
                peer.local_addr().unwrap(),
            ))
            .await
            .unwrap();
        let original = source
            .links_snapshot()
            .await
            .into_iter()
            .find(|(id, _, _)| id == &peer.config.node_id)
            .unwrap()
            .1;
        let actor = source
            .remote_ref(&format!("parrot://{}/probe-echo", peer.config.node_id))
            .unwrap();
        for value in [91_u64, 92] {
            let receipt = Arc::new(NetworkWake {
                calls: 0.into(),
                throwing,
                retired: Arc::new(AtomicUsize::new(0)),
            });
            let waker = Waker::from(receipt.clone());
            let mut observation = Box::pin(source.probe_member(&peer.config.node_id));
            assert!(observation
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
            tokio::time::timeout(Duration::from_secs(5), async {
                while receipt.calls.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("actual TCP probe receipt did not arrive");
            assert_eq!(
                observation.as_mut().poll(&mut Context::from_waker(&waker)),
                std::task::Poll::Ready(ProbeOutcome::Reachable)
            );
            drop(observation);
            assert_eq!(receipt.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                receipt.retired.load(Ordering::SeqCst),
                usize::from(throwing == 2)
            );
            assert!(!original.is_closed());
            let current = source.links_snapshot().await;
            assert_eq!(current.len(), 1);
            assert!(current[0].1.same_connection(&original));
            let reply = actor
                .send_with_timeout(
                    Box::new(crate::ingress::TestAsk(value)),
                    Some(Duration::from_secs(2)),
                )
                .await
                .unwrap();
            assert_eq!(
                reply.downcast_ref::<crate::ingress::TestAsk>().unwrap().0,
                value
            );
            actor
                .deliver_with_ack(
                    Box::new(crate::ingress::TestAsk(value + 1000)),
                    Duration::from_secs(2),
                )
                .await
                .unwrap();
        }
        assert_eq!(*echo.0.lock().unwrap(), [91, 1091, 92, 1092]);
    })
    .catch_unwind()
    .await;
    source.shutdown().await.unwrap();
    peer.shutdown().await.unwrap();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

macro_rules! contract {
    ($name:ident, $exercise:expr) => {
        #[tokio::test]
        async fn $name() {
            crate::system::diagnostic_contracts::isolated(
                concat!("swim::probe::notification_contracts::", stringify!($name)),
                &[],
                false,
                0,
                async { $exercise },
            )
            .await;
        }
    };
}
contract!(
    reply_commits_before_reentrant_notification,
    single("reply", 0)
);
contract!(reply_isolates_throwing_waker, single("reply", 1));
contract!(reply_isolates_throwing_panic_payload, single("reply", 2));
contract!(
    cancellation_commits_before_reentrant_notification,
    single("cancel", 0)
);
contract!(cancellation_isolates_throwing_waker, single("cancel", 1));
contract!(
    cancellation_isolates_throwing_panic_payload,
    single("cancel", 2)
);
contract!(
    close_fences_and_notifies_every_waiter_outside_lock,
    close(0)
);
contract!(close_isolates_each_throwing_waker, close(1));
contract!(close_isolates_each_throwing_panic_payload, close(2));
contract!(
    cancellation_retires_receipt_and_connection_outside_lock,
    retire_source(0)
);
contract!(
    cancellation_isolates_both_independent_throwing_owners,
    retire_source(1)
);
contract!(
    cancellation_isolates_both_independent_throwing_payloads,
    retire_source(2)
);
contract!(
    exchanges_continue_after_throwing_receipt_wakes,
    exchange_progress(1).await
);
contract!(
    exchanges_continue_after_throwing_receipt_payloads,
    exchange_progress(2).await
);
contract!(
    tcp_probe_reply_wake_preserves_original_link_and_business,
    network_progress(1).await
);
contract!(
    tcp_probe_reply_payload_preserves_original_link_and_business,
    network_progress(2).await
);
