use super::*;
use std::{
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Arc, Weak,
    },
    task::{Context, Wake, Waker},
};
use tokio::sync::{mpsc, oneshot};

struct Probe {
    registry: Weak<CallbackRegistry>,
    calls: AtomicUsize,
    locked: AtomicBool,
    panic: AtomicBool,
    reenter: bool,
    nested: Mutex<Option<oneshot::Receiver<ReplyPayload>>>,
}
impl Wake for Probe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(registry) = self.registry.upgrade() {
            let unlocked = registry.slots.try_lock().is_ok();
            self.locked.fetch_or(!unlocked, Ordering::SeqCst);
            if self.reenter && unlocked {
                // Reuse the removed ID: retirement must commit before notification.
                let (tx, rx) = oneshot::channel();
                registry.insert(1, "new-generation", tx).unwrap();
                *self.nested.lock().unwrap() = Some(rx);
            }
        }
        assert!(
            !self.panic.load(Ordering::SeqCst),
            "controlled reply wake panic"
        );
    }
}
fn probe(registry: &Arc<CallbackRegistry>, panic: bool, reenter: bool) -> Arc<Probe> {
    Arc::new(Probe {
        registry: Arc::downgrade(registry),
        calls: AtomicUsize::new(0),
        locked: AtomicBool::new(false),
        panic: AtomicBool::new(panic),
        reenter,
        nested: Mutex::new(None),
    })
}
fn park(rx: &mut oneshot::Receiver<ReplyPayload>, probe: &Arc<Probe>) {
    let waker = Waker::from(probe.clone());
    assert!(Pin::new(rx)
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
}
fn connection() -> (FrameSender, mpsc::Receiver<crate::Frame>) {
    let (tx, rx) = mpsc::channel(1);
    (FrameSender::new(tx, "peer"), rx)
}
fn reply() -> ReplyPayload {
    ReplyPayload::Ok(bytes::Bytes::from_static(b"answer"), "reply-key".into())
}
fn check_reply(value: Result<ReplyPayload, oneshot::error::TryRecvError>) {
    match value.unwrap() {
        ReplyPayload::Ok(bytes, key) => {
            assert_eq!(&bytes[..], b"answer");
            assert_eq!(key, "reply-key");
        }
        other => panic!("wrong reply: {other:?}"),
    }
}
fn check_failure(rx: &mut oneshot::Receiver<ReplyPayload>) {
    assert!(matches!(
        rx.try_recv().unwrap(),
        ReplyPayload::Err(crate::ErrCode::ConnectionLost, detail) if detail == "closed"
    ));
}
fn unlocked(probes: &[&Arc<Probe>]) {
    for probe in probes {
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        assert!(
            !probe.locked.load(Ordering::SeqCst),
            "reply wake held registry lock"
        );
    }
}

#[test]
fn local_completion_notifies_after_commit_and_allows_id_reuse() {
    let registry = Arc::new(CallbackRegistry::new(4));
    let (tx, mut rx) = oneshot::channel();
    registry.insert(1, "peer", tx).unwrap();
    let probe = probe(&registry, false, true);
    park(&mut rx, &probe);
    assert!(registry.complete(1, reply()));
    check_reply(rx.try_recv());
    unlocked(&[&probe]);
    assert_eq!(registry.len(), 1);
    assert!(registry.complete(1, reply()));
    check_reply(probe.nested.lock().unwrap().as_mut().unwrap().try_recv());
}

#[test]
fn network_completion_keeps_source_fence_and_allows_reentry() {
    let registry = Arc::new(CallbackRegistry::new(4));
    let (source, _writer) = connection();
    let (wrong, _other_writer) = connection();
    let (tx, mut rx) = oneshot::channel();
    registry
        .insert_on(1, "logical-target", tx, &source)
        .unwrap();
    let probe = probe(&registry, false, true);
    park(&mut rx, &probe);
    assert!(!registry.complete_from(1, reply(), &wrong));
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    assert_eq!(registry.len(), 1);
    assert!(registry.complete_from(1, reply(), &source));
    check_reply(rx.try_recv());
    unlocked(&[&probe]);
    assert_eq!(registry.len(), 1);
    assert!(registry.complete(1, reply()));
    check_reply(probe.nested.lock().unwrap().as_mut().unwrap().try_recv());
}

#[test]
fn remove_closes_outside_lock_and_preserves_reentrant_registration() {
    let registry = Arc::new(CallbackRegistry::new(4));
    let (tx, mut rx) = oneshot::channel();
    registry.insert(1, "peer", tx).unwrap();
    let probe = probe(&registry, false, true);
    park(&mut rx, &probe);
    assert!(registry.remove(1));
    assert_eq!(
        rx.try_recv().unwrap_err(),
        oneshot::error::TryRecvError::Closed
    );
    unlocked(&[&probe]);
    assert_eq!(registry.len(), 1);
    assert!(registry.complete(1, reply()));
    check_reply(probe.nested.lock().unwrap().as_mut().unwrap().try_recv());
}

fn bulk_failure(mode: &str, panic: bool) {
    let registry = Arc::new(CallbackRegistry::new(8));
    let (source, _writer) = connection();
    let (other, _other_writer) = connection();
    let mut receivers = Vec::new();
    let mut probes = Vec::new();
    for cid in 1..=3 {
        let (tx, mut rx) = oneshot::channel();
        registry.insert_on(cid, "selected", tx, &source).unwrap();
        let probe = probe(&registry, panic, !panic && cid == 1);
        park(&mut rx, &probe);
        receivers.push(rx);
        probes.push(probe);
    }
    let (tx, mut unselected) = oneshot::channel();
    registry.insert_on(4, "other", tx, &other).unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| match mode {
        "connection" => registry.fail_connection(&source, crate::ErrCode::ConnectionLost, "closed"),
        "node" => registry.fail_node("selected", crate::ErrCode::ConnectionLost, "closed"),
        "all" => registry.fail_all(crate::ErrCode::ConnectionLost, "closed"),
        _ => unreachable!(),
    }));
    // Keep test-failure unwinding separate from the notification under test.
    for probe in &probes {
        probe.panic.store(false, Ordering::SeqCst);
    }
    assert!(
        !registry.slots.is_poisoned(),
        "notification poisoned registry"
    );
    assert_eq!(result.unwrap(), if mode == "all" { 4 } else { 3 });
    for rx in &mut receivers {
        check_failure(rx);
    }
    unlocked(&probes.iter().collect::<Vec<_>>());
    if mode == "all" {
        check_failure(&mut unselected);
    } else {
        assert!(matches!(
            unselected.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(registry.complete_from(4, reply(), &other));
        check_reply(unselected.try_recv());
    }
    assert_eq!(registry.len(), usize::from(!panic));
    if !panic {
        assert!(registry.complete(1, reply()));
        check_reply(
            probes[0]
                .nested
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .try_recv(),
        );
    }
}

macro_rules! bulk_contract {
    ($name:ident, $mode:literal, $panic:literal) => {
        #[tokio::test]
        async fn $name() {
            crate::system::diagnostic_contracts::isolated(
                concat!("registry::notification_contracts::", stringify!($name)),
                &[],
                false,
                0,
                async {
                    bulk_failure($mode, $panic);
                },
            )
            .await;
        }
    };
}
bulk_contract!(
    connection_failure_commits_before_reentry,
    "connection",
    false
);
bulk_contract!(node_failure_commits_before_reentry, "node", false);
bulk_contract!(all_failure_commits_before_reentry, "all", false);
bulk_contract!(
    connection_failure_survives_every_wake_panic,
    "connection",
    true
);
bulk_contract!(node_failure_survives_every_wake_panic, "node", true);
bulk_contract!(all_failure_survives_every_wake_panic, "all", true);

#[test]
fn completion_and_cancellation_survive_wake_panic_after_commit() {
    for mode in ["local", "network", "remove"] {
        let registry = Arc::new(CallbackRegistry::new(1));
        let (source, _writer) = connection();
        let (tx, mut rx) = oneshot::channel();
        registry.insert_on(1, "peer", tx, &source).unwrap();
        let probe = probe(&registry, true, false);
        park(&mut rx, &probe);
        let result = catch_unwind(AssertUnwindSafe(|| match mode {
            "local" => registry.complete(1, reply()),
            "network" => registry.complete_from(1, reply(), &source),
            "remove" => registry.remove(1),
            _ => unreachable!(),
        }));
        assert!(
            !registry.slots.is_poisoned(),
            "notification poisoned registry: {mode}"
        );
        assert!(result.unwrap());
        unlocked(&[&probe]);
        assert!(registry.is_empty());
        if mode == "remove" {
            assert_eq!(
                rx.try_recv().unwrap_err(),
                oneshot::error::TryRecvError::Closed
            );
        } else {
            check_reply(rx.try_recv());
        }
    }
}

#[tokio::test]
async fn registry_drop_closes_every_waiter_despite_wake_panics() {
    crate::system::diagnostic_contracts::isolated(
        "registry::notification_contracts::registry_drop_closes_every_waiter_despite_wake_panics",
        &[],
        false,
        0,
        async {
            let registry = Arc::new(CallbackRegistry::new(3));
            let mut receivers = Vec::new();
            let mut probes = Vec::new();
            for cid in 1..=3 {
                let (tx, mut rx) = oneshot::channel();
                registry.insert(cid, "peer", tx).unwrap();
                let probe = probe(&registry, true, false);
                park(&mut rx, &probe);
                receivers.push(rx);
                probes.push(probe);
            }
            drop(registry);
            for (probe, mut rx) in probes.into_iter().zip(receivers) {
                assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
                assert_eq!(
                    rx.try_recv().unwrap_err(),
                    oneshot::error::TryRecvError::Closed
                );
            }
        },
    )
    .await;
}

fn retired_guard_preserves_replacement(mode: &str) {
    let registry = Arc::new(CallbackRegistry::new(1));
    let (source, _writer) = connection();
    let (mut rx, pending) = registry.register_on(1, "peer", &source).unwrap();
    let probe = probe(&registry, false, true);
    park(&mut rx, &probe);
    match mode {
        "local" => assert!(registry.complete(1, reply())),
        "network" => assert!(registry.complete_from(1, reply(), &source)),
        "remove" => assert!(registry.remove(1)),
        "connection" => assert_eq!(
            registry.fail_connection(&source, crate::ErrCode::ConnectionLost, "closed"),
            1
        ),
        "node" => assert_eq!(
            registry.fail_node("peer", crate::ErrCode::ConnectionLost, "closed"),
            1
        ),
        "all" => assert_eq!(
            registry.fail_all(crate::ErrCode::ConnectionLost, "closed"),
            1
        ),
        _ => unreachable!(),
    }
    unlocked(&[&probe]);
    match mode {
        "local" | "network" => check_reply(rx.try_recv()),
        "remove" => assert_eq!(
            rx.try_recv().unwrap_err(),
            oneshot::error::TryRecvError::Closed
        ),
        _ => check_failure(&mut rx),
    }
    assert_eq!(registry.len(), 1);
    drop(pending);
    assert_eq!(registry.len(), 1, "old guard removed replacement: {mode}");
    assert!(registry.complete(1, reply()));
    check_reply(probe.nested.lock().unwrap().as_mut().unwrap().try_recv());
}

macro_rules! guard_contract {
    ($name:ident, $mode:literal) => {
        #[test]
        fn $name() {
            retired_guard_preserves_replacement($mode);
        }
    };
}
guard_contract!(
    old_guard_preserves_id_reused_during_local_completion,
    "local"
);
guard_contract!(
    old_guard_preserves_id_reused_during_network_completion,
    "network"
);
guard_contract!(old_guard_preserves_id_reused_during_removal, "remove");
guard_contract!(
    old_guard_preserves_id_reused_during_connection_failure,
    "connection"
);
guard_contract!(old_guard_preserves_id_reused_during_node_failure, "node");
guard_contract!(old_guard_preserves_id_reused_during_system_failure, "all");

#[test]
fn current_guard_cancels_only_its_slot_and_preserves_notification_reentry() {
    let registry = Arc::new(CallbackRegistry::new(1));
    let (source, _writer) = connection();
    let (mut rx, pending) = registry.register_on(1, "peer", &source).unwrap();
    let probe = probe(&registry, true, true);
    park(&mut rx, &probe);
    let result = catch_unwind(AssertUnwindSafe(|| drop(pending)));
    probe.panic.store(false, Ordering::SeqCst);
    result.unwrap();
    unlocked(&[&probe]);
    assert_eq!(
        rx.try_recv().unwrap_err(),
        oneshot::error::TryRecvError::Closed
    );
    assert_eq!(registry.len(), 1);
    assert!(registry.complete(1, reply()));
    check_reply(probe.nested.lock().unwrap().as_mut().unwrap().try_recv());
}

#[test]
fn guard_created_after_retirement_keeps_the_insertion_identity() {
    let registry = CallbackRegistry::new(1);
    let (source, _writer) = connection();
    let (tx, mut rx) = oneshot::channel();
    let identity = registry
        .insert_slot(1, "peer", tx, Some(source.clone()))
        .unwrap();
    assert!(registry.complete_from(1, reply(), &source));
    check_reply(rx.try_recv());
    let (tx, mut replacement) = oneshot::channel();
    registry
        .insert_on(1, "new-generation", tx, &source)
        .unwrap();
    // Model the registering thread resuming after another thread completed and
    // replaced its slot, before it could construct the caller's guard.
    drop(PendingCallback {
        registry: &registry,
        cid: 1,
        identity,
    });
    assert_eq!(registry.len(), 1, "guard captured a later registration");
    assert!(registry.complete_from(1, reply(), &source));
    check_reply(replacement.try_recv());
}

fn rejected_sender_isolated(mode: &str) {
    let registry = Arc::new(CallbackRegistry::new(1));
    let (source, _writer) = connection();
    let (live_tx, mut live_rx) = oneshot::channel();
    if mode != "closed" {
        registry.insert_on(1, "peer", live_tx, &source).unwrap();
    } else {
        drop(live_tx);
        source.close();
    }
    let (tx, mut rx) = oneshot::channel();
    let probe = probe(&registry, true, false);
    park(&mut rx, &probe);
    let cid = if mode == "duplicate" { 1 } else { 2 };
    let result = catch_unwind(AssertUnwindSafe(|| {
        registry.insert_on(cid, "peer", tx, &source)
    }));
    probe.panic.store(false, Ordering::SeqCst);
    let error = result
        .expect("rejected sender wake escaped registration")
        .unwrap_err();
    match mode {
        "duplicate" => assert!(
            matches!(error, RemoteError::Transport(detail) if detail == "duplicate correlation id")
        ),
        "full" => assert!(matches!(error, RemoteError::CallbacksFull(1))),
        "closed" => assert!(
            matches!(error, RemoteError::Transport(detail) if detail == "connection already closed")
        ),
        _ => unreachable!(),
    }
    unlocked(&[&probe]);
    assert_eq!(
        rx.try_recv().unwrap_err(),
        oneshot::error::TryRecvError::Closed
    );
    assert!(!registry.slots.is_poisoned());
    if mode != "closed" {
        assert!(registry.complete_from(1, reply(), &source));
        check_reply(live_rx.try_recv());
    }
    assert!(registry.is_empty());
}
#[test]
fn duplicate_registration_isolates_rejected_sender() {
    rejected_sender_isolated("duplicate");
}
#[test]
fn full_registration_isolates_rejected_sender() {
    rejected_sender_isolated("full");
}
#[test]
fn closed_connection_registration_isolates_rejected_sender() {
    rejected_sender_isolated("closed");
}
