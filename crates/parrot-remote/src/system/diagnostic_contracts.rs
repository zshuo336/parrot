//! Global diagnostic callbacks run only in an owned, bounded test child.
use futures::FutureExt;
use std::{
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Self(self.0.clone()));
    }
}

struct FinitePayload(Arc<AtomicUsize>);
impl Drop for FinitePayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("controlled diagnostic payload retirement panic");
    }
}

struct Subscriber {
    targets: &'static [&'static str],
    events: Arc<AtomicUsize>,
    retired: Arc<AtomicUsize>,
    throwing_payload: bool,
    observe: Option<Observer>,
}
type Observer = Arc<dyn Fn(&tracing::Event<'_>) -> bool + Send + Sync>;
impl tracing::Subscriber for Subscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        self.targets.contains(&metadata.target())
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if self.observe.as_ref().is_some_and(|observe| !observe(event)) {
            return;
        }
        self.events.fetch_add(1, Ordering::SeqCst);
        if self.throwing_payload {
            // Background-task runtimes can repeatedly retire recursive payloads.
            // Keep those lifecycle regressions terminal without weakening the
            // original recursive-payload contracts used by isolated().
            if self.observe.is_some() {
                std::panic::panic_any(FinitePayload(self.retired.clone()));
            }
            std::panic::panic_any(Payload(self.retired.clone()));
        }
        panic!("controlled remote diagnostic panic");
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

pub(crate) async fn isolated(
    name: &str,
    targets: &'static [&'static str],
    throwing_payload: bool,
    expected_events: usize,
    exercise: impl std::future::Future<Output = ()>,
) {
    isolated_observed(
        name,
        targets,
        throwing_payload,
        expected_events,
        None,
        exercise,
    )
    .await;
}

pub(crate) async fn isolated_observed(
    name: &str,
    targets: &'static [&'static str],
    throwing_payload: bool,
    expected_events: usize,
    observe: Option<Observer>,
    exercise: impl std::future::Future<Output = ()>,
) {
    const CHILD: &str = "PARROT_REMOTE_DIAGNOSTIC_CHILD";
    if std::env::var_os(CHILD).is_some() {
        // Assertions inspect state and receipts, not nested-panic backtraces.
        std::panic::set_hook(Box::new(|info| eprintln!("{info}")));
        let events = Arc::new(AtomicUsize::new(0));
        let retired = Arc::new(AtomicUsize::new(0));
        tracing::subscriber::set_global_default(Subscriber {
            targets,
            events: events.clone(),
            retired: retired.clone(),
            throwing_payload,
            observe,
        })
        .unwrap();
        if let Err(payload) = AssertUnwindSafe(exercise).catch_unwind().await {
            crate::ingress::unwind::Cleanup::default().dispose(payload);
            panic!("remote diagnostic interrupted the operation");
        }
        assert_eq!(
            events.load(Ordering::SeqCst),
            expected_events,
            "missing or repeated diagnostic"
        );
        assert_eq!(
            retired.load(Ordering::SeqCst),
            expected_events * usize::from(throwing_payload),
            "diagnostic panic payload did not retire exactly once"
        );
        return;
    }
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, "1")
            .env("RUST_BACKTRACE", "0")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("remote diagnostic contract child stalled")
    .unwrap();
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"),
        "remote diagnostic child failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Waiting {
    entered: tokio::sync::Notify,
    retired: Arc<AtomicUsize>,
}
struct Retired(Arc<AtomicUsize>);
impl Drop for Retired {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait::async_trait]
impl crate::LocalLookup for Waiting {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
#[async_trait::async_trait]
impl crate::ingress::SysEventHook for Waiting {
    async fn on_event(&self, _: crate::admin::SysEvent, _: &crate::FrameSender, _: &str) {
        let _retired = Retired(self.retired.clone());
        self.entered.notify_one();
        std::future::pending::<()>().await;
    }
}

async fn dispatcher(budget: bool) {
    use crate::{
        frame::{frame_type, Frame},
        ingress::Ingress,
        registry::{CallbackRegistry, ReplyPayload},
        transport::{FrameSender, RuntimeKnobs},
        ControlPolicy, ErrCode,
    };
    let waiting = Arc::new(Waiting {
        entered: Default::default(),
        retired: Arc::new(AtomicUsize::new(0)),
    });
    let callbacks = Arc::new(CallbackRegistry::new(4));
    let ingress = Arc::new(
        Ingress::new(waiting.clone(), callbacks.clone())
            .with_control_policy(ControlPolicy::InsecureDevelopment),
    );
    *ingress.sys_event_slot() = Some(waiting.clone());
    let (bad_tx, _bad_rx) = tokio::sync::mpsc::channel(4);
    let (good_tx, _good_rx) = tokio::sync::mpsc::channel(4);
    let bad = FrameSender::new(bad_tx, "bad");
    let good = FrameSender::new(good_tx, "good");
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    callbacks.insert_on(17, "good", reply_tx, &good).unwrap();
    let (input, mut receiver) = tokio::sync::mpsc::channel(4);
    let kept = ingress.clone();
    let task = tokio::spawn(async move {
        super::dispatch::run(
            ingress,
            &mut receiver,
            RuntimeKnobs {
                inbound_dispatch_per_connection: 1,
                ..Default::default()
            },
        )
        .await;
    });
    let mut frame = Frame::reply_err(0, "", ErrCode::Timeout, "");
    frame.header.frame_type = frame_type::SYSTEM_EVENT;
    frame.payload = if budget {
        crate::swim::encode_gossip(&crate::swim::MembershipGossip {
            events: vec![],
            seen_from: "bad".into(),
            digest: 0,
            full_sync: None,
        })
    } else {
        bytes::Bytes::from_static(&[0xff])
    };
    input
        .send((frame.clone(), bad.clone(), "bad".into()))
        .await
        .unwrap();
    if budget {
        tokio::time::timeout(Duration::from_secs(5), waiting.entered.notified())
            .await
            .expect("control lane did not enter");
        input
            .send((frame, bad.clone(), "bad".into()))
            .await
            .unwrap();
    }
    input
        .send((
            Frame::reply_err(17, "", ErrCode::Timeout, "healthy"),
            good.clone(),
            "good".into(),
        ))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), reply_rx).await;
    drop(input);
    let completion = tokio::time::timeout(Duration::from_secs(5), task).await;
    kept.close();
    kept.wait_closed().await;
    assert!(
        completion.unwrap().is_ok(),
        "diagnostic killed the shared dispatcher"
    );
    assert!(matches!(
        reply.unwrap().unwrap(),
        ReplyPayload::Err(ErrCode::Timeout, detail) if detail == "healthy"
    ));
    assert!(bad.is_closed(), "offending connection was not closed");
    assert!(!good.is_closed(), "healthy connection was closed");
    assert!(callbacks.is_empty());
    assert_eq!(waiting.retired.load(Ordering::SeqCst), usize::from(budget));
}

macro_rules! dispatch_contract {
    ($name:ident, $payload:literal, $budget:literal) => {
        #[tokio::test]
        async fn $name() {
            isolated(
                concat!("system::diagnostic_contracts::", stringify!($name)),
                if $budget {
                    &["parrot_remote::system::dispatch"]
                } else {
                    &["parrot_remote::ingress", "parrot_remote::system::dispatch"]
                },
                $payload,
                if $budget { 1 } else { 2 },
                dispatcher($budget),
            )
            .await;
        }
    };
}
dispatch_contract!(
    dispatch_diagnostic_panic_preserves_healthy_peer,
    false,
    false
);
dispatch_contract!(
    dispatch_diagnostic_payload_panic_preserves_healthy_peer,
    true,
    false
);
dispatch_contract!(budget_diagnostic_panic_preserves_healthy_peer, false, true);
dispatch_contract!(
    budget_diagnostic_payload_panic_preserves_healthy_peer,
    true,
    true
);

struct ReenterClose {
    system: std::sync::Weak<super::RemoteActorSystem>,
    locked: std::sync::atomic::AtomicBool,
    calls: AtomicUsize,
}
impl std::task::Wake for ReenterClose {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(system) = self.system.upgrade() {
            let unlocked = system.link_gate.try_lock().is_ok();
            self.locked.store(!unlocked, Ordering::SeqCst);
            if unlocked {
                system.begin_close();
            }
        }
    }
}

#[tokio::test]
async fn shutdown_completion_wake_reenters_outside_connection_gate() {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Waker},
    };
    let system = super::RemoteActorSystem::new(
        super::RemoteConfig::tcp("reply-shutdown", None),
        Arc::new(Waiting {
            entered: Default::default(),
            retired: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .unwrap();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    system.callbacks.insert(1, "peer", tx).unwrap();
    let probe = Arc::new(ReenterClose {
        system: Arc::downgrade(&system),
        locked: false.into(),
        calls: AtomicUsize::new(0),
    });
    let waker = Waker::from(probe.clone());
    assert!(Pin::new(&mut rx)
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    system.begin_close();
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert!(
        !probe.locked.load(Ordering::SeqCst),
        "shutdown reply wake held connection gate"
    );
    assert!(
        matches!(rx.try_recv().unwrap(), crate::registry::ReplyPayload::Err(crate::ErrCode::ConnectionLost, detail) if detail == "system shutdown")
    );
    assert!(system.callbacks.is_empty());
    system.shutdown().await.unwrap();
}
