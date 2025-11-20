//! Resolve and retirement notifications observe committed state without locks.
use super::*;
use futures::FutureExt;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Weak,
    },
    task::{Context, Wake, Waker},
    time::Duration,
};

const WAITERS: usize = 40;
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
fn ingress() -> Arc<Ingress> {
    Arc::new(
        Ingress::new(Arc::new(Empty), Arc::new(CallbackRegistry::new(64)))
            .with_control_policy(crate::ControlPolicy::InsecureDevelopment),
    )
}
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("controlled ingress payload disposal");
    }
}
struct Probe {
    ingress: Weak<Ingress>,
    calls: AtomicUsize,
    locked: AtomicBool,
    premature: AtomicBool,
    throwing: u8,
    retired: Arc<AtomicUsize>,
    closing: bool,
    full: bool,
    cache: Option<(FrameSender, String)>,
}
impl Wake for Probe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(ingress) = self.ingress.upgrade() {
            let unlocked = ingress.ask_open.try_lock().is_ok()
                && ingress.resolve_pending.try_lock().is_ok()
                && ingress.resolve_cache.try_lock().is_ok()
                && ingress.reorder_tx.try_lock().is_ok();
            self.locked.fetch_or(!unlocked, Ordering::SeqCst);
            if unlocked {
                if self.closing {
                    self.premature.fetch_or(
                        *ingress.ask_open.lock().unwrap() || !ingress.ask_tasks.is_closed(),
                        Ordering::SeqCst,
                    );
                    if self.full {
                        self.premature.fetch_or(
                            !ingress.reorder_tasks.is_closed()
                                || ingress.reorder_tx_of("reentrant").is_some(),
                            Ordering::SeqCst,
                        );
                        ingress.close();
                    } else {
                        ingress.close_asks();
                    }
                }
                if let Some((sender, key)) = &self.cache {
                    self.premature.fetch_or(
                        ingress
                            .cached_resolve(sender, key, std::time::Instant::now())
                            .is_none(),
                        Ordering::SeqCst,
                    );
                }
                let _ = ingress.pending_resolves();
            }
        }
        match self.throwing {
            1 => panic!("controlled ingress notification panic"),
            2 => std::panic::panic_any(Payload(self.retired.clone())),
            _ => {}
        }
    }
}
fn probe(ingress: &Arc<Ingress>, throwing: u8, closing: bool, full: bool) -> Arc<Probe> {
    Arc::new(Probe {
        ingress: Arc::downgrade(ingress),
        calls: 0.into(),
        locked: false.into(),
        premature: false.into(),
        throwing,
        retired: Arc::new(0.into()),
        closing,
        full,
        cache: None,
    })
}
fn park<T>(future: Pin<&mut impl Future<Output = T>>, probe: &Arc<Probe>) {
    assert!(future
        .poll(&mut Context::from_waker(&Waker::from(probe.clone())))
        .is_pending());
}
fn check(probe: &Probe) {
    let calls = probe.calls.load(Ordering::SeqCst);
    assert!(calls > 0, "waiter lost its wake");
    assert!(
        !probe.locked.load(Ordering::SeqCst),
        "notification held ingress lock"
    );
    assert!(
        !probe.premature.load(Ordering::SeqCst),
        "notification preceded state commit"
    );
    assert_eq!(
        probe.retired.load(Ordering::SeqCst),
        calls * usize::from(probe.throwing == 2)
    );
}
fn clear_test_poison(ingress: &Ingress) {
    ingress.ask_open.clear_poison();
    ingress.resolve_pending.clear_poison();
    ingress.resolve_cache.clear_poison();
    ingress.reorder_tx.clear_poison();
}
async fn cancellation(full: bool, throwing: u8) {
    let ingress = ingress();
    let (tx, mut frames) = tokio::sync::mpsc::channel(WAITERS + 1);
    let sender = FrameSender::new(tx, "resolve-authority");
    let keys: Vec<_> = (0..WAITERS).map(|i| format!("/route/{i}")).collect();
    let probes: Vec<_> = (0..WAITERS)
        .map(|_| probe(&ingress, throwing, true, full))
        .collect();
    let mut operations: Vec<_> = keys
        .iter()
        .map(|key| Box::pin(ingress.resolve_remote(&sender, key, 0, Duration::from_secs(5))))
        .collect();
    for (operation, probe) in operations.iter_mut().zip(&probes) {
        park(operation.as_mut(), probe);
    }
    for key in &keys {
        let frame = frames.try_recv().unwrap();
        assert_eq!(
            crate::resolve::decode_query(&frame.payload).unwrap().key,
            *key
        );
    }
    assert_eq!(ingress.pending_resolves(), WAITERS);
    let cleanup = Cleanup::default();
    let result = cleanup.call(|| {
        if full {
            ingress.close();
        } else {
            ingress.close_asks();
        }
    });
    clear_test_poison(&ingress);
    let outcomes: Vec<_> = operations
        .into_iter()
        .map(|mut operation| {
            let outcome =
                cleanup.call(|| matches!(operation.as_mut().now_or_never(), Some(Err(_))));
            cleanup.dispose(operation);
            outcome
        })
        .collect();
    assert_eq!(result, Ok(()), "close interrupted");
    for (outcome, probe) in outcomes.into_iter().zip(&probes) {
        assert_eq!(outcome, Ok(true));
        check(probe);
    }
    assert_eq!(ingress.pending_resolves(), 0);
    assert!(ingress
        .resolve_remote(&sender, "/late", 0, Duration::from_secs(1))
        .await
        .is_err());
    assert!(frames.try_recv().is_err());
    ingress.close();
    ingress.wait_closed().await;
}
async fn barriers(reorder: bool, active: bool, throwing: u8) {
    let ingress = ingress();
    if reorder {
        ingress.close_asks();
    }
    let tracker = if reorder {
        &ingress.reorder_tasks
    } else {
        &ingress.ask_tasks
    };
    let token = active.then(|| tracker.token());
    let probes: Vec<_> = (0..WAITERS)
        .map(|_| probe(&ingress, throwing, true, reorder))
        .collect();
    let mut waits: Vec<_> = (0..WAITERS)
        .map(|_| {
            Box::pin(async {
                if reorder {
                    ingress.wait_closed().await;
                } else {
                    ingress.wait_for_asks().await;
                }
            })
        })
        .collect();
    for (wait, probe) in waits.iter_mut().zip(&probes) {
        park(wait.as_mut(), probe);
    }
    let cleanup = Cleanup::default();
    let closed = cleanup.call(|| {
        if reorder {
            ingress.close();
        } else {
            ingress.close_asks();
        }
    });
    clear_test_poison(&ingress);
    if active {
        assert!(probes
            .iter()
            .all(|probe| probe.calls.load(Ordering::SeqCst) == 0));
        for (wait, probe) in waits.iter_mut().zip(&probes) {
            park(wait.as_mut(), probe);
        }
    }
    let retired = cleanup.call(|| drop(token));
    let outcomes: Vec<_> = waits
        .into_iter()
        .map(|mut wait| {
            let result = cleanup.call(|| wait.as_mut().now_or_never());
            cleanup.dispose(wait);
            result
        })
        .collect();
    assert_eq!(closed, Ok(()), "barrier close interrupted");
    assert_eq!(retired, Ok(()), "last task retirement interrupted");
    for (outcome, probe) in outcomes.into_iter().zip(&probes) {
        assert_eq!(outcome, Ok(Some(())));
        check(probe);
    }
    ingress.close();
    ingress.wait_closed().await;
}
fn slot(
    ingress: &Arc<Ingress>,
    sender: &FrameSender,
    cid: u64,
) -> tokio::sync::oneshot::Receiver<ReplyPayload> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    ingress.resolve_pending.lock().unwrap().insert(
        cid,
        ResolvePending {
            tx,
            connection: sender.clone(),
            key: "/route".into(),
            cache_key: resolve_cache_key(sender, "/route"),
            min_version: 0,
            invalidated: false,
            identity: Arc::new(()),
        },
    );
    rx
}
async fn notification(mode: u8, throwing: u8) {
    let ingress = ingress();
    let (tx, _frames) = tokio::sync::mpsc::channel(4);
    let sender = FrameSender::new(tx, "resolve-authority");
    let mut rx = slot(&ingress, &sender, 1);
    let mut probe = probe(&ingress, throwing, mode == 3, mode == 3);
    if mode == 0 {
        Arc::get_mut(&mut probe).unwrap().cache = Some((sender.clone(), "/route".into()));
    }
    park(Pin::new(&mut rx), &probe);
    let cleanup = Cleanup::default();
    let result = cleanup.call(|| match mode {
        0 => {
            let payload = crate::resolve::encode_response(&crate::resolve::Response {
                key: "/route".into(),
                entry: Some(crate::roles::directory::DirEntry {
                    endpoints: vec!["tcp://127.0.0.1:1234".into()],
                    version: 9,
                    ttl_s: 60,
                }),
            })
            .unwrap();
            let mut frame = Frame::reply(1, "/route", crate::resolve::RESPONSE_KEY, payload.into());
            frame.header.frame_type = frame_type::RESOLVE_R;
            ingress.on_resolve_response(frame, &sender);
        }
        1 => {
            assert!(ingress.fail_resolve(1, &sender, ErrCode::Timeout, "test deadline"));
        }
        2 => ingress.connection_closed(&sender),
        3 => ingress.close(),
        _ => unreachable!(),
    });
    clear_test_poison(&ingress);
    let reply = rx.try_recv();
    assert_eq!(result, Ok(()), "resolve notification interrupted operation");
    check(&probe);
    match mode {
        0 => assert!(matches!(reply, Ok(ReplyPayload::Ok(bytes, _))
            if crate::resolve::decode_response(&bytes).unwrap().entry.unwrap().version == 9)),
        1 => assert!(
            matches!(reply, Ok(ReplyPayload::Err(ErrCode::Timeout, detail)) if detail == "test deadline")
        ),
        2 => assert!(matches!(
            reply,
            Ok(ReplyPayload::Err(ErrCode::ConnectionLost, _))
        )),
        3 => assert!(matches!(reply, Ok(ReplyPayload::Err(ErrCode::Stopped, _)))),
        _ => unreachable!(),
    }
    assert_eq!(ingress.pending_resolves(), 0);
    ingress.close();
    ingress.wait_closed().await;
}

async fn siblings(full: bool, throwing: u8) {
    let ingress = ingress();
    let (tx, _frames) = tokio::sync::mpsc::channel(4);
    let sender = FrameSender::new(tx, "resolve-authority");
    let mut receivers: Vec<_> = (1..=3).map(|cid| slot(&ingress, &sender, cid)).collect();
    let probes: Vec<_> = (0..3)
        .map(|_| probe(&ingress, throwing, full, full))
        .collect();
    for (rx, probe) in receivers.iter_mut().zip(&probes) {
        park(Pin::new(rx), probe);
    }
    let result = Cleanup::default().call(|| {
        if full {
            ingress.close();
        } else {
            ingress.connection_closed(&sender);
        }
    });
    clear_test_poison(&ingress);
    assert_eq!(
        result,
        Ok(()),
        "one resolve receiver interrupted its siblings"
    );
    assert_eq!(ingress.pending_resolves(), 0);
    for (mut rx, probe) in receivers.into_iter().zip(&probes) {
        assert!(matches!(rx.try_recv(), Ok(ReplyPayload::Err(code, _))
            if code == if full { ErrCode::Stopped } else { ErrCode::ConnectionLost }));
        check(probe);
    }
    ingress.close();
    ingress.wait_closed().await;
}

struct ResolveLookup;
#[async_trait::async_trait]
impl LocalLookup for ResolveLookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
    async fn resolve_directory(
        &self,
        key: &str,
        min_version: u64,
    ) -> Result<Option<crate::roles::directory::DirEntry>, String> {
        assert_eq!(key, "/route");
        assert!(min_version <= 9);
        Ok(Some(crate::roles::directory::DirEntry {
            endpoints: vec!["tcp://127.0.0.1:1234".into()],
            version: 9,
            ttl_s: 60,
        }))
    }
}
async fn network(throwing: u8) {
    let id = uuid::Uuid::new_v4();
    let config = |name: &str| {
        let mut config =
            crate::RemoteConfig::tcp(format!("{name}-{id}"), Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development();
        config.swim.gossip_interval = Duration::from_secs(3600);
        config.swim.probe_interval = Duration::from_secs(3600);
        config
    };
    let source = crate::RemoteActorSystem::new(config("resolve-source"), Arc::new(Empty)).unwrap();
    let target =
        crate::RemoteActorSystem::new(config("resolve-target"), Arc::new(ResolveLookup)).unwrap();
    source.start().await.unwrap();
    target.start().await.unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        source
            .connect(&crate::NodeAddr::tcp(
                &target.config.node_id,
                target.local_addr().unwrap(),
            ))
            .await
            .unwrap();
        let original = source
            .links_snapshot()
            .await
            .into_iter()
            .find(|(id, _, _)| id == &target.config.node_id)
            .unwrap()
            .1;
        for min_version in [1, 9] {
            let mut probe = probe(&source.ingress, throwing, false, false);
            Arc::get_mut(&mut probe).unwrap().cache = Some((original.clone(), "/route".into()));
            let mut query = Box::pin(source.ingress.resolve_remote(
                &original,
                "/route",
                min_version,
                Duration::from_secs(3),
            ));
            park(query.as_mut(), &probe);
            tokio::time::timeout(Duration::from_secs(3), async {
                while probe.calls.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("actual TCP resolve did not wake its caller");
            let reply = query
                .as_mut()
                .now_or_never()
                .expect("resolve reply was not ready")
                .unwrap()
                .unwrap();
            assert_eq!(reply.version, 9);
            assert_eq!(reply.endpoints, ["tcp://127.0.0.1:1234"]);
            check(&probe);
            assert!(!original.is_closed());
            let links = source.links_snapshot().await;
            assert_eq!(links.len(), 1);
            assert!(links[0].1.same_connection(&original));
            assert_eq!(source.ingress.pending_resolves(), 0);
        }
    })
    .catch_unwind()
    .await;
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

struct PendingLookup {
    entered: AtomicUsize,
    retired: AtomicUsize,
}
struct ActiveLookup<'a>(&'a PendingLookup);
impl Drop for ActiveLookup<'_> {
    fn drop(&mut self) {
        self.0.retired.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait::async_trait]
impl LocalLookup for PendingLookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let _active = ActiveLookup(self);
        std::future::pending().await
    }
}
async fn accepted(reorder: bool, throwing: u8) {
    let local = Arc::new(PendingLookup {
        entered: 0.into(),
        retired: 0.into(),
    });
    let ingress = Arc::new(Ingress::new(
        local.clone(),
        Arc::new(CallbackRegistry::new(4)),
    ));
    let (tx, mut replies) = tokio::sync::mpsc::channel(4);
    let sender = FrameSender::new(tx, "pending-source");
    let (key, payload) = CodecRegistry::global()
        .encode_outgoing(&(Box::new(TestAsk(77)) as parrot_api::types::BoxedMessage))
        .unwrap();
    let frame = if reorder {
        Frame::tell_with_seq("/target", &key, payload.into(), 1)
    } else {
        Frame::ask(1, "/target", &key, payload.into(), None)
    };
    ingress.dispatch(frame, &sender, "pending-source").await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while local.entered.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    if reorder {
        ingress.close_asks();
    }
    let probe = probe(&ingress, throwing, true, reorder);
    let mut wait = Box::pin(async {
        if reorder {
            ingress.wait_closed().await;
        } else {
            ingress.wait_for_asks().await;
        }
    });
    park(wait.as_mut(), &probe);
    if reorder {
        ingress.close();
    } else {
        ingress.close_asks();
    }
    assert_eq!(local.retired.load(Ordering::SeqCst), 0);
    park(wait.as_mut(), &probe);
    tokio::time::timeout(Duration::from_secs(2), async {
        while probe.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accepted work did not retire");
    assert_eq!(wait.as_mut().now_or_never(), Some(()));
    assert_eq!(local.retired.load(Ordering::SeqCst), 1);
    assert_eq!(local.entered.load(Ordering::SeqCst), 1);
    assert!(replies.try_recv().is_err());
    assert!(ingress.reorder_tx.lock().unwrap().is_empty());
    check(&probe);
    ingress.close();
    ingress.wait_closed().await;
}

async fn direct(throwing: u8) {
    let local = Arc::new(PendingLookup {
        entered: 0.into(),
        retired: 0.into(),
    });
    let ingress = Arc::new(Ingress::new(
        local.clone(),
        Arc::new(CallbackRegistry::new(4)),
    ));
    let (tx, _frames) = tokio::sync::mpsc::channel(4);
    let sender = FrameSender::new(tx, "direct-source");
    let (key, payload) = CodecRegistry::global()
        .encode_outgoing(&(Box::new(TestAsk(88)) as parrot_api::types::BoxedMessage))
        .unwrap();
    let probes: Vec<_> = (0..WAITERS)
        .map(|_| probe(&ingress, throwing, true, true))
        .collect();
    let mut operations: Vec<_> = (0..WAITERS)
        .map(|_| {
            Box::pin(ingress.dispatch(
                Frame::tell("/target", &key, payload.clone().into()),
                &sender,
                "direct-source",
            ))
        })
        .collect();
    for (operation, probe) in operations.iter_mut().zip(&probes) {
        park(operation.as_mut(), probe);
    }
    assert_eq!(local.entered.load(Ordering::SeqCst), WAITERS);
    let cleanup = Cleanup::default();
    let result = cleanup.call(|| ingress.close());
    clear_test_poison(&ingress);
    let outcomes: Vec<_> = operations
        .into_iter()
        .map(|mut operation| {
            let outcome = cleanup.call(|| operation.as_mut().now_or_never());
            cleanup.dispose(operation);
            outcome
        })
        .collect();
    assert_eq!(result, Ok(()));
    for (outcome, probe) in outcomes.into_iter().zip(&probes) {
        assert_eq!(outcome, Ok(Some(())));
        check(probe);
    }
    assert_eq!(local.retired.load(Ordering::SeqCst), WAITERS);
    ingress.wait_closed().await;
}

async fn retirement(lease: bool, throwing: u8) {
    let ingress = ingress();
    let (tx, mut frames) = tokio::sync::mpsc::channel(1);
    let sender = FrameSender::new(tx, "last-resolve-owner");
    let mut receipt = slot(&ingress, &sender, 1);
    let identity = ingress
        .resolve_pending
        .lock()
        .unwrap()
        .get(&1)
        .unwrap()
        .identity
        .clone();
    let receipt_probe = probe(&ingress, throwing, false, false);
    let owner_probe = probe(&ingress, throwing, false, false);
    park(Pin::new(&mut receipt), &receipt_probe);
    assert!(frames
        .poll_recv(&mut Context::from_waker(&Waker::from(owner_probe.clone())))
        .is_pending());
    drop(sender);
    let guard = ResolvePendingLease {
        table: ingress.resolve_pending.clone(),
        cid: 1,
        identity,
    };
    let cleanup = Cleanup::default();
    let result = cleanup.call(|| {
        if lease {
            drop(guard);
        } else {
            ingress.close();
            drop(guard);
        }
    });
    clear_test_poison(&ingress);
    assert_eq!(
        result,
        Ok(()),
        "independent resolve owners interrupted retirement"
    );
    check(&receipt_probe);
    check(&owner_probe);
    assert_eq!(ingress.pending_resolves(), 0);
    assert!(matches!(
        frames.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
    if lease {
        assert!(matches!(
            receipt.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
    } else {
        assert!(matches!(
            receipt.try_recv(),
            Ok(ReplyPayload::Err(ErrCode::Stopped, _))
        ));
    }
    ingress.close();
    ingress.wait_closed().await;
}

macro_rules! contract {
    ($name:ident, $exercise:expr) => {
        #[tokio::test]
        async fn $name() {
            crate::system::diagnostic_contracts::isolated(
                concat!("ingress::notification_contracts::", stringify!($name)),
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
    ask_close_releases_reentrant_resolvers,
    cancellation(false, 0).await
);
contract!(
    ask_close_isolates_throwing_resolvers,
    cancellation(false, 1).await
);
contract!(
    ask_close_isolates_throwing_resolver_payloads,
    cancellation(false, 2).await
);
contract!(
    full_close_fences_before_reentrant_resolvers,
    cancellation(true, 0).await
);
contract!(
    full_close_isolates_throwing_resolvers,
    cancellation(true, 1).await
);
contract!(
    full_close_isolates_throwing_resolver_payloads,
    cancellation(true, 2).await
);
contract!(
    empty_ask_barrier_isolates_wakes,
    barriers(false, false, 1).await
);
contract!(
    empty_ask_barrier_isolates_payloads,
    barriers(false, false, 2).await
);
contract!(
    last_ask_barrier_isolates_wakes,
    barriers(false, true, 1).await
);
contract!(
    last_ask_barrier_isolates_payloads,
    barriers(false, true, 2).await
);
contract!(
    last_reorder_barrier_isolates_wakes,
    barriers(true, true, 1).await
);
contract!(
    last_reorder_barrier_isolates_payloads,
    barriers(true, true, 2).await
);
contract!(
    response_commits_cache_before_reentrant_notification,
    notification(0, 0).await
);
contract!(response_isolates_throwing_payload, notification(0, 2).await);
contract!(
    failure_notifies_outside_registry_lock,
    notification(1, 0).await
);
contract!(failure_isolates_throwing_payload, notification(1, 2).await);
contract!(
    disconnect_notifies_outside_registry_lock,
    notification(2, 0).await
);
contract!(
    disconnect_isolates_throwing_payload,
    notification(2, 2).await
);
contract!(
    close_fences_reorder_before_resolve_receipt,
    notification(3, 0).await
);
contract!(
    close_isolates_throwing_resolve_receipt,
    notification(3, 2).await
);
contract!(
    disconnect_preserves_every_throwing_resolve_receiver,
    siblings(false, 1).await
);
contract!(
    disconnect_preserves_every_throwing_resolve_payload,
    siblings(false, 2).await
);
contract!(
    close_preserves_every_throwing_resolve_receiver,
    siblings(true, 1).await
);
contract!(
    close_preserves_every_throwing_resolve_payload,
    siblings(true, 2).await
);
contract!(
    tcp_resolve_progress_survives_throwing_wake,
    network(1).await
);
contract!(
    tcp_resolve_progress_survives_throwing_payload,
    network(2).await
);
contract!(
    accepted_ask_retires_before_throwing_barrier_wake,
    accepted(false, 1).await
);
contract!(
    accepted_ask_retires_before_throwing_barrier_payload,
    accepted(false, 2).await
);
contract!(
    accepted_reorder_retires_before_throwing_barrier_wake,
    accepted(true, 1).await
);
contract!(
    accepted_reorder_retires_before_throwing_barrier_payload,
    accepted(true, 2).await
);
contract!(
    direct_tell_close_isolates_every_throwing_wake,
    direct(1).await
);
contract!(
    direct_tell_close_isolates_every_throwing_payload,
    direct(2).await
);
contract!(
    lease_retires_independent_throwing_owners,
    retirement(true, 1).await
);
contract!(
    lease_retires_independent_throwing_payloads,
    retirement(true, 2).await
);
contract!(
    close_retires_independent_throwing_owners,
    retirement(false, 1).await
);
contract!(
    close_retires_independent_throwing_payloads,
    retirement(false, 2).await
);
