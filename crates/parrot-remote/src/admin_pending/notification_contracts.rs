use super::*;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Weak,
    },
    task::{Context, Wake, Waker},
};

trait Sample: AdminResponse + std::fmt::Debug {
    fn success(id: u64) -> Self;
    fn check(self, id: u64, failed: bool);
}
impl Sample for crate::admin::AdminReply {
    fn success(req_id: u64) -> Self {
        Self::Stopped { req_id }
    }
    fn check(self, id: u64, failed: bool) {
        match self {
            Self::Stopped { req_id } if !failed => assert_eq!(req_id, id),
            Self::Failed {
                req_id,
                code,
                detail,
            } if failed => {
                assert_eq!(req_id, id);
                assert_eq!(code, ErrCode::ConnectionLost as u16);
                assert_eq!(detail, "closed");
            }
            other => panic!("wrong administrative reply: {other:?}"),
        }
    }
}
impl Sample for crate::admin_v2::AdminReplyV2 {
    fn success(req_id: u64) -> Self {
        Self::Stopped { req_id }
    }
    fn check(self, id: u64, failed: bool) {
        match self {
            Self::Stopped { req_id } if !failed => {
                assert_eq!(req_id, id);
            }
            Self::Failed {
                req_id,
                code,
                detail,
            } if failed => {
                assert_eq!(req_id, id);
                assert_eq!(code, ErrCode::ConnectionLost as u16);
                assert_eq!(detail, "closed");
            }
            other => panic!("wrong administrative reply: {other:?}"),
        }
    }
}

struct Probe<R> {
    table: Weak<Pending<R>>,
    calls: AtomicUsize,
    locked: AtomicBool,
    originals_remain: AtomicBool,
    originals: Vec<(u64, Arc<()>)>,
    panic: AtomicBool,
    reenter: bool,
    nested: Mutex<Option<Result<oneshot::Receiver<R>, RemoteError>>>,
}
impl<R: Send + 'static> Wake for Probe<R> {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(table) = self.table.upgrade() {
            let unlocked = match table.state.try_lock() {
                Ok(state) => {
                    self.originals_remain.fetch_or(
                        self.originals.iter().any(|(id, identity)| {
                            state
                                .slots
                                .get(id)
                                .is_some_and(|slot| Arc::ptr_eq(&slot.identity, identity))
                        }),
                        Ordering::SeqCst,
                    );
                    true
                }
                Err(_) => false,
            };
            self.locked.fetch_or(!unlocked, Ordering::SeqCst);
            if self.reenter && unlocked {
                let (tx, rx) = oneshot::channel();
                let result = table.insert(1, tx).map(|()| rx);
                *self.nested.lock().unwrap() = Some(result);
            }
        }
        assert!(
            !self.panic.load(Ordering::SeqCst),
            "controlled administrative wake panic"
        );
    }
}
fn probe<R: Send + 'static>(table: &Arc<Pending<R>>, panic: bool, reenter: bool) -> Arc<Probe<R>> {
    Arc::new(Probe {
        table: Arc::downgrade(table),
        calls: 0.into(),
        locked: false.into(),
        originals_remain: false.into(),
        originals: table
            .state
            .lock()
            .unwrap()
            .slots
            .iter()
            .map(|(id, slot)| (*id, slot.identity.clone()))
            .collect(),
        panic: panic.into(),
        reenter,
        nested: Mutex::new(None),
    })
}
fn park<R: Send + 'static>(rx: &mut oneshot::Receiver<R>, probe: &Arc<Probe<R>>) {
    assert!(Pin::new(rx)
        .poll(&mut Context::from_waker(&Waker::from(probe.clone())))
        .is_pending());
}
fn check_probe<R>(probe: &Probe<R>) {
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert!(
        !probe.locked.load(Ordering::SeqCst),
        "administrative wake held registry lock"
    );
    assert!(
        !probe.originals_remain.load(Ordering::SeqCst),
        "administrative cohort was not committed before notification"
    );
}
fn connection() -> (FrameSender, tokio::sync::mpsc::Receiver<crate::Frame>) {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    (FrameSender::new(tx, "peer"), rx)
}
fn single<R: Sample>(mode: &str) {
    for panic in [false, true] {
        let table = Arc::new(Pending::<R>::new(4));
        let (source, _writer) = connection();
        let (wrong, _wrong_writer) = connection();
        let (mut rx, guard) = table.register_on(1, &source).unwrap();
        let probe = probe(&table, panic, true);
        park(&mut rx, &probe);
        assert!(!table.complete_from(R::success(1), &wrong));
        assert!(!table.fail_from(1, ErrCode::ConnectionLost, "closed", &wrong));
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
        assert_eq!(table.len(), 1);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match mode {
            "local" => {
                assert!(table.complete(R::success(1)));
            }
            "network" => {
                assert!(table.complete_from(R::success(1), &source));
            }
            "failure" => {
                assert!(table.fail_from(1, ErrCode::ConnectionLost, "closed", &source));
            }
            "remove" => {
                assert!(table.remove(1));
            }
            "guard" => {
                drop(guard);
            }
            _ => unreachable!(),
        }));
        probe.panic.store(false, Ordering::SeqCst);
        assert!(
            !table.state.is_poisoned(),
            "administrative notification poisoned registry"
        );
        result.unwrap();
        check_probe(&probe);
        if matches!(mode, "remove" | "guard") {
            assert_eq!(
                rx.try_recv().unwrap_err(),
                oneshot::error::TryRecvError::Closed
            );
        } else {
            rx.try_recv().unwrap().check(1, mode == "failure");
        }
        assert_eq!(table.len(), 1, "reentrant registration was lost");
        assert!(table.complete(R::success(1)));
        probe
            .nested
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .try_recv()
            .unwrap()
            .check(1, false);
    }
}
fn bulk<R: Sample>(mode: &str) {
    for panic in [false, true] {
        let table = Arc::new(Pending::<R>::new(8));
        let (source, _writer) = connection();
        let (other, _other_writer) = connection();
        let mut receivers = Vec::new();
        for id in 1..=3 {
            let (tx, rx) = oneshot::channel();
            table.insert_on(id, tx, &source).unwrap();
            receivers.push(rx);
        }
        let mut probes = Vec::new();
        for (index, rx) in receivers.iter_mut().enumerate() {
            let probe = probe(&table, panic, index == 0 && mode != "drop");
            park(rx, &probe);
            probes.push(probe);
        }
        let (tx, mut unselected) = oneshot::channel();
        table.insert_on(4, tx, &other).unwrap();
        match mode {
            "connection" => table.fail_connection(&source, "closed"),
            "all" => table.fail_all("closed"),
            "close" => table.close("closed"),
            "drop" => drop(table),
            _ => unreachable!(),
        }
        for (index, (probe, rx)) in probes.iter().zip(&mut receivers).enumerate() {
            probe.panic.store(false, Ordering::SeqCst);
            check_probe(probe);
            if mode == "drop" {
                assert_eq!(
                    rx.try_recv().unwrap_err(),
                    oneshot::error::TryRecvError::Closed
                );
            } else {
                rx.try_recv().unwrap().check(index as u64 + 1, true);
            }
        }
        if mode == "drop" {
            assert_eq!(
                unselected.try_recv().unwrap_err(),
                oneshot::error::TryRecvError::Closed
            );
            continue;
        }
        // Reacquire through the probes, so Drop is tested without a strong owner.
        let table = probes[0].table.upgrade().unwrap();
        if mode == "connection" {
            assert!(matches!(
                unselected.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            assert!(table.complete_from(R::success(4), &other));
            unselected.try_recv().unwrap().check(4, false);
        } else {
            unselected.try_recv().unwrap().check(4, true);
        }
        let mut nested = probes[0].nested.lock().unwrap().take().unwrap();
        if mode == "close" {
            assert!(nested.is_err(), "close allowed reentrant admission");
            assert!(table.is_empty());
            let (tx, _rx) = oneshot::channel();
            assert!(table.insert_on(7, tx, &source).is_err());
        } else {
            assert_eq!(table.len(), 1);
            assert!(table.complete(R::success(1)));
            nested.as_mut().unwrap().try_recv().unwrap().check(1, false);
        }
    }
}

fn rejected<R: Sample>(_: &str) {
    for mode in ["duplicate", "capacity", "closed-table", "closed-connection"] {
        let table = Arc::new(Pending::<R>::new(if mode == "capacity" { 1 } else { 4 }));
        let (source, _writer) = connection();
        let (tx, mut original) = oneshot::channel();
        table.insert(1, tx).unwrap();
        if mode == "closed-table" {
            table.close("closed");
        }
        if mode == "closed-connection" {
            source.close();
        }
        let (tx, mut rx) = oneshot::channel();
        let probe = probe(&table, true, false);
        park(&mut rx, &probe);
        let id = if mode == "duplicate" { 1 } else { 2 };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            table.insert_on(id, tx, &source)
        }));
        probe.panic.store(false, Ordering::SeqCst);
        assert!(result
            .expect("rejected administrative sender escaped notification isolation")
            .is_err());
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        assert!(
            !probe.locked.load(Ordering::SeqCst),
            "rejected administrative sender held registry lock"
        );
        assert!(!table.state.is_poisoned());
        assert_eq!(
            rx.try_recv().unwrap_err(),
            oneshot::error::TryRecvError::Closed
        );
        if mode == "closed-table" {
            original.try_recv().unwrap().check(1, true);
            assert!(table.is_empty());
        } else {
            assert_eq!(table.len(), 1);
            assert!(table.complete(R::success(1)));
            original.try_recv().unwrap().check(1, false);
        }
    }
}

macro_rules! contract {
    ($name:ident, $protocol:ty, $exercise:ident, $mode:literal) => {
        #[tokio::test]
        async fn $name() {
            crate::system::diagnostic_contracts::isolated(
                concat!("admin_pending::notification_contracts::", stringify!($name)),
                &[],
                false,
                0,
                async {
                    $exercise::<$protocol>($mode);
                },
            )
            .await;
        }
    };
}
contract!(
    v1_local_completion,
    crate::admin::AdminReply,
    single,
    "local"
);
contract!(
    v2_local_completion,
    crate::admin_v2::AdminReplyV2,
    single,
    "local"
);
contract!(
    v1_network_completion,
    crate::admin::AdminReply,
    single,
    "network"
);
contract!(
    v2_network_completion,
    crate::admin_v2::AdminReplyV2,
    single,
    "network"
);
contract!(
    v1_network_failure,
    crate::admin::AdminReply,
    single,
    "failure"
);
contract!(
    v2_network_failure,
    crate::admin_v2::AdminReplyV2,
    single,
    "failure"
);
contract!(v1_removal, crate::admin::AdminReply, single, "remove");
contract!(v2_removal, crate::admin_v2::AdminReplyV2, single, "remove");
contract!(
    v1_guard_retirement,
    crate::admin::AdminReply,
    single,
    "guard"
);
contract!(
    v2_guard_retirement,
    crate::admin_v2::AdminReplyV2,
    single,
    "guard"
);
contract!(
    v1_connection_failure,
    crate::admin::AdminReply,
    bulk,
    "connection"
);
contract!(
    v2_connection_failure,
    crate::admin_v2::AdminReplyV2,
    bulk,
    "connection"
);
contract!(v1_all_failure, crate::admin::AdminReply, bulk, "all");
contract!(v2_all_failure, crate::admin_v2::AdminReplyV2, bulk, "all");
contract!(v1_close, crate::admin::AdminReply, bulk, "close");
contract!(v2_close, crate::admin_v2::AdminReplyV2, bulk, "close");
contract!(v1_drop, crate::admin::AdminReply, bulk, "drop");
contract!(v2_drop, crate::admin_v2::AdminReplyV2, bulk, "drop");
contract!(
    v1_rejected_admission,
    crate::admin::AdminReply,
    rejected,
    ""
);
contract!(
    v2_rejected_admission,
    crate::admin_v2::AdminReplyV2,
    rejected,
    ""
);

#[derive(Debug)]
struct Custom {
    id: u64,
    owner: Weak<Pending<Self>>,
    dropped: Arc<AtomicUsize>,
    throw_on_drop: bool,
}
static FAILURE_CALLS: AtomicUsize = AtomicUsize::new(0);
static PAYLOAD_DROPS: AtomicUsize = AtomicUsize::new(0);
struct BadPayload;
impl Drop for BadPayload {
    fn drop(&mut self) {
        PAYLOAD_DROPS.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Self);
    }
}
impl AdminResponse for Custom {
    fn request_id(&self) -> u64 {
        if let Some(owner) = self.owner.upgrade() {
            assert!(
                owner.state.try_lock().is_ok(),
                "request identity callback held registry lock"
            );
        }
        self.id
    }
    fn failure(_: u64, _: ErrCode, _: &str) -> Self {
        FAILURE_CALLS.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(BadPayload);
    }
}
impl Drop for Custom {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
        if self.throw_on_drop {
            std::panic::panic_any(BadPayload);
        }
    }
}
#[tokio::test]
async fn identity_callback_runs_before_locking() {
    let table = Arc::new(Pending::<Custom>::new(1));
    let (tx, mut rx) = oneshot::channel();
    table.insert(1, tx).unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    assert!(table.complete(Custom {
        id: 1,
        owner: Arc::downgrade(&table),
        dropped: dropped.clone(),
        throw_on_drop: false
    }));
    drop(rx.try_recv().unwrap());
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(table.is_empty());
}
#[tokio::test]
async fn throwing_failure_constructors_close_every_selected_waiter() {
    crate::system::diagnostic_contracts::isolated(
        "admin_pending::notification_contracts::throwing_failure_constructors_close_every_selected_waiter",
        &[], false, 0, async {
            let table = Arc::new(Pending::<Custom>::new(3));
            let mut receivers = Vec::new();
            for id in 1..=3 {
                let (tx, mut rx) = oneshot::channel();
                table.insert(id, tx).unwrap();
                let probe = probe(&table, true, false);
                park(&mut rx, &probe);
                receivers.push((rx, probe));
            }
            table.close("closed");
            assert!(table.is_empty());
            assert_eq!(FAILURE_CALLS.load(Ordering::SeqCst), 3);
            assert_eq!(PAYLOAD_DROPS.load(Ordering::SeqCst), 3);
            for (mut rx, probe) in receivers {
                check_probe(&probe);
                assert_eq!(rx.try_recv().unwrap_err(), oneshot::error::TryRecvError::Closed);
            }
        },
    ).await;
}
#[tokio::test]
async fn closed_receiver_retirement_preserves_completion_result() {
    crate::system::diagnostic_contracts::isolated(
        "admin_pending::notification_contracts::closed_receiver_retirement_preserves_completion_result",
        &[], false, 0, async {
            let table = Arc::new(Pending::<Custom>::new(1));
            let (tx, rx) = oneshot::channel();
            table.insert(1, tx).unwrap();
            drop(rx);
            let dropped = Arc::new(AtomicUsize::new(0));
            assert!(!table.complete(Custom { id: 1, owner: Weak::new(), dropped: dropped.clone(), throw_on_drop: true }));
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            assert_eq!(PAYLOAD_DROPS.load(Ordering::SeqCst), 1);
            assert!(!table.state.is_poisoned());
            assert!(table.is_empty());
        },
    ).await;
}
