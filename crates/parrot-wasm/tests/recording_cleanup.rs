#![cfg(feature = "runtime")]

use parrot_wasm::{
    runtime::{
        GuestMessaging, GuestSpan, GuestSpanStatus, GuestTrace, MessagingError, MessagingRequest,
        Msg, WasmComponent, WasmRuntime,
    },
    ComponentError, HostCtx, TraceParent, WasmConfig,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc, Mutex,
    },
    task::{Context, Poll},
};

const RECORDING: &[u8] = include_bytes!("fixtures/recording-component.wasm");
const MESSAGING: &[u8] = include_bytes!("fixtures/messaging-component.wasm");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Fault {
    #[default]
    Clean,
    RootDrop,
    ChildDrop,
    ChildFinish,
    RecoveryDrop,
    RecoveryFinish,
}

#[derive(Debug, Default)]
struct Probe {
    fault: Fault,
    child_fault: usize,
    next: AtomicUsize,
    roots: AtomicUsize,
    recoveries: AtomicUsize,
    drops: Mutex<Vec<usize>>,
    statuses: Mutex<Vec<(usize, GuestSpanStatus)>>,
    pending: AtomicBool,
    future_panic: AtomicBool,
    futures: AtomicUsize,
    future_drops: AtomicUsize,
}

impl Probe {
    fn root(self: &Arc<Self>) -> Box<dyn GuestTrace> {
        Box::new(Owner {
            probe: self.clone(),
            id: 0,
            recovery: false,
        })
    }

    fn child(self: &Arc<Self>, recovery: bool) -> Box<dyn GuestSpan> {
        Box::new(Owner {
            probe: self.clone(),
            id: self.next.fetch_add(1, SeqCst) + 1,
            recovery,
        })
    }

    fn error(&self) -> ComponentError {
        ComponentError::Panic(
            match self.fault {
                Fault::RootDrop => "recording root disposal failed",
                Fault::ChildDrop => "recording child disposal failed",
                Fault::ChildFinish => "recording child finish failed",
                Fault::RecoveryDrop => "recording recovery disposal failed",
                Fault::RecoveryFinish => "recording recovery finish failed",
                Fault::Clean => unreachable!(),
            }
            .into(),
        )
    }
}

#[derive(Debug)]
struct Owner {
    probe: Arc<Probe>,
    id: usize,
    recovery: bool,
}

impl GuestTrace for Owner {
    fn parent(&self) -> TraceParent {
        TraceParent {
            trace_id: [1; 16],
            span_id: (self.id as u64 + 1).to_le_bytes(),
        }
    }

    fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
        Some(self.probe.child(false))
    }

    fn start_recovery(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
        self.probe.recoveries.fetch_add(1, SeqCst);
        Some(self.probe.child(true))
    }
}

impl GuestSpan for Owner {
    fn finish(self: Box<Self>, status: GuestSpanStatus) {
        self.probe.statuses.lock().unwrap().push((self.id, status));
        assert!(
            !(self.recovery && self.probe.fault == Fault::RecoveryFinish),
            "recording recovery finish failed"
        );
        assert!(
            !(self.id == self.probe.child_fault && self.probe.fault == Fault::ChildFinish),
            "recording child finish failed"
        );
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        if self.id == 0 {
            self.probe.roots.fetch_add(1, SeqCst);
            assert!(
                self.probe.fault != Fault::RootDrop,
                "recording root disposal failed"
            );
        } else {
            self.probe.drops.lock().unwrap().push(self.id);
            assert!(
                !(self.recovery && self.probe.fault == Fault::RecoveryDrop),
                "recording recovery disposal failed"
            );
            assert!(
                !(self.id == self.probe.child_fault && self.probe.fault == Fault::ChildDrop),
                "recording child disposal failed"
            );
        }
    }
}

struct Wait<'a, T> {
    probe: &'a Probe,
    value: Option<T>,
}

impl<T: Unpin> Future for Wait<'_, T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<T> {
        if self.probe.pending.load(SeqCst) {
            Poll::Pending
        } else {
            Poll::Ready(self.value.take().unwrap())
        }
    }
}

impl<T> Drop for Wait<'_, T> {
    fn drop(&mut self) {
        self.probe.future_drops.fetch_add(1, SeqCst);
        assert!(
            !self.probe.future_panic.load(SeqCst),
            "earlier future disposal failed"
        );
    }
}

impl GuestMessaging for Probe {
    fn ask(
        &self,
        request: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Msg, MessagingError>> + Send + '_>> {
        self.futures.fetch_add(1, SeqCst);
        Box::pin(Wait {
            probe: self,
            value: Some(Ok(request.message)),
        })
    }

    fn tell(
        &self,
        _: MessagingRequest,
    ) -> Pin<Box<dyn Future<Output = Result<(), MessagingError>> + Send + '_>> {
        self.futures.fetch_add(1, SeqCst);
        Box::pin(Wait {
            probe: self,
            value: Some(Ok(())),
        })
    }
}

fn runtime() -> WasmRuntime {
    WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 20_000_000,
        epoch_deadline: 10_000,
        ..Default::default()
    })
    .unwrap()
}

async fn guest(asynchronous: bool, messaging: bool, probe: &Arc<Probe>) -> WasmComponent {
    let runtime = runtime();
    let mut host = HostCtx {
        self_path: "/user/recording-cleanup".into(),
        ..Default::default()
    };
    host.config_overlay
        .insert("recording-on-start-mode", "unfinished");
    host.config_overlay
        .insert("recording-on-drain-mode", "unfinished");
    if messaging {
        host.config_overlay.insert("start-value", "1");
        host.config_overlay.insert("drain-value", "2");
    }
    let bytes = if messaging { MESSAGING } else { RECORDING };
    if asynchronous {
        runtime
            .prepare_bytes(bytes)
            .unwrap()
            .instantiate_async(
                host,
                messaging.then(|| probe.clone() as Arc<dyn GuestMessaging>),
            )
            .await
            .unwrap()
    } else {
        runtime.instantiate_bytes(bytes, host).unwrap()
    }
}

async fn call(
    guest: &mut WasmComponent,
    entry: &str,
    key: &str,
    recording: Option<Box<dyn GuestTrace>>,
) -> Result<(), ComponentError> {
    if guest.uses_async() {
        match entry {
            "tell" => guest.tell_async(key, b"", recording).await,
            "start" => guest.on_start_async(recording).await,
            "drain" => guest.on_drain_async(recording).await,
            _ => guest.handle_async(key, b"", recording).await.map(|_| ()),
        }
    } else {
        match (entry, recording) {
            ("tell", Some(root)) => guest.tell_with_recording(key, b"", root),
            ("start", Some(root)) => guest.on_start_with_recording(root),
            ("drain", Some(root)) => guest.on_drain_with_recording(root),
            (_, Some(root)) => guest.handle_with_recording(key, b"", root).map(|_| ()),
            ("tell", None) => guest.tell(key, b""),
            ("start", None) => guest.on_start(),
            ("drain", None) => guest.on_drain(),
            (_, None) => guest.handle(key, b"").map(|_| ()),
        }
    }
}

async fn denied(guest: &mut WasmComponent, probe: &Probe, expected: ComponentError) {
    assert_eq!(guest.cleanup_error(), Some(expected.clone()));
    assert!(!guest.is_alive());
    let futures = probe.futures.load(SeqCst);
    let owners = probe.next.load(SeqCst);
    let recoveries = probe.recoveries.load(SeqCst);
    guest.take_metrics();
    for entry in ["ask", "tell", "start", "drain"] {
        assert_eq!(
            call(guest, entry, "nested", None).await,
            Err(expected.clone()),
            "{entry}"
        );
    }
    assert_eq!(probe.futures.load(SeqCst), futures);
    assert_eq!(probe.next.load(SeqCst), owners);
    assert_eq!(probe.recoveries.load(SeqCst), recoveries);
    assert_eq!(guest.take_metrics().messages, 0);
}

async fn root_failure(asynchronous: bool) {
    for entry in ["ask", "tell", "start", "drain"] {
        let probe = Arc::new(Probe {
            fault: Fault::RootDrop,
            ..Default::default()
        });
        let mut guest = guest(asynchronous, false, &probe).await;
        assert_eq!(
            call(&mut guest, entry, "unfinished", Some(probe.root())).await,
            Err(probe.error())
        );
        assert_eq!(probe.roots.load(SeqCst), 1);
        assert_eq!(*probe.drops.lock().unwrap(), [1]);
        assert_eq!(
            *probe.statuses.lock().unwrap(),
            [(1, GuestSpanStatus::Cancelled)]
        );
        denied(&mut guest, &probe, probe.error()).await;
    }
}

#[tokio::test]
async fn sync_admitted_root_disposal_failure_vetoes_all_entry_points() {
    root_failure(false).await;
}
#[tokio::test]
async fn async_admitted_root_disposal_failure_vetoes_all_entry_points() {
    root_failure(true).await;
}

async fn explicit_finish(asynchronous: bool) {
    for fault in [Fault::ChildFinish, Fault::ChildDrop] {
        let probe = Arc::new(Probe {
            fault,
            child_fault: 2,
            ..Default::default()
        });
        let mut guest = guest(asynchronous, false, &probe).await;
        assert_eq!(
            call(&mut guest, "ask", "nested", Some(probe.root())).await,
            Err(probe.error())
        );
        assert_eq!(
            *probe.statuses.lock().unwrap(),
            [(2, GuestSpanStatus::Ok), (1, GuestSpanStatus::Panicked)]
        );
        assert_eq!(*probe.drops.lock().unwrap(), [2, 1]);
        assert_eq!(probe.roots.load(SeqCst), 1);
        denied(&mut guest, &probe, probe.error()).await;
    }
}

#[tokio::test]
async fn sync_explicit_finish_failure_retires_siblings_and_vetoes_reuse() {
    explicit_finish(false).await;
}
#[tokio::test]
async fn async_explicit_finish_failure_retires_siblings_and_vetoes_reuse() {
    explicit_finish(true).await;
}

async fn unfinished(asynchronous: bool) {
    for fault in [Fault::ChildFinish, Fault::ChildDrop] {
        let probe = Arc::new(Probe {
            fault,
            child_fault: 1,
            ..Default::default()
        });
        let mut guest = guest(asynchronous, false, &probe).await;
        assert_eq!(
            call(&mut guest, "ask", "unfinished-nested", Some(probe.root())).await,
            Err(probe.error())
        );
        assert_eq!(
            *probe.statuses.lock().unwrap(),
            [
                (1, GuestSpanStatus::Cancelled),
                (2, GuestSpanStatus::Panicked),
                (3, GuestSpanStatus::Panicked)
            ]
        );
        assert_eq!(*probe.drops.lock().unwrap(), [1, 2, 3]);
        assert_eq!(probe.roots.load(SeqCst), 1);
        denied(&mut guest, &probe, probe.error()).await;
    }
}

#[tokio::test]
async fn sync_retirement_failure_marks_later_siblings_panicked() {
    unfinished(false).await;
}
#[tokio::test]
async fn async_retirement_failure_marks_later_siblings_panicked() {
    unfinished(true).await;
}

async fn cancel(entry: &str, recovery: bool, fault: Fault, future_panic: bool) {
    let probe = Arc::new(Probe {
        fault,
        child_fault: if recovery { 2 } else { 1 },
        ..Default::default()
    });
    let mut guest = guest(true, true, &probe).await;
    if recovery {
        guest.on_start_async(None).await.unwrap();
        assert!(guest.handle_async("trap", b"", None).await.is_err());
    }
    probe.pending.store(true, SeqCst);
    probe.future_panic.store(future_panic, SeqCst);
    let before = probe.future_drops.load(SeqCst);
    let mut invocation = Box::pin(call(&mut guest, entry, "ask", Some(probe.root())));
    let mut cx = Context::from_waker(std::task::Waker::noop());
    assert!(invocation.as_mut().poll(&mut cx).is_pending());
    assert!(probe.statuses.lock().unwrap().is_empty());
    drop(invocation);
    assert_eq!(probe.future_drops.load(SeqCst), before + 1);
    assert_eq!(probe.roots.load(SeqCst), 1);
    let statuses = probe.statuses.lock().unwrap().clone();
    assert_eq!(
        statuses[0],
        (
            if recovery { 2 } else { 1 },
            if future_panic {
                GuestSpanStatus::Panicked
            } else {
                GuestSpanStatus::Cancelled
            }
        )
    );
    if recovery {
        assert_eq!(
            statuses[1],
            (
                1,
                if future_panic || matches!(fault, Fault::ChildDrop | Fault::ChildFinish) {
                    GuestSpanStatus::Panicked
                } else {
                    GuestSpanStatus::Cancelled
                }
            )
        );
        assert_eq!(*probe.drops.lock().unwrap(), [2, 1]);
    } else {
        assert_eq!(*probe.drops.lock().unwrap(), [1]);
    }
    let expected = if future_panic {
        ComponentError::Panic("earlier future disposal failed".into())
    } else {
        probe.error()
    };
    denied(&mut guest, &probe, expected).await;
}

#[tokio::test]
async fn cancelled_messages_and_hooks_keep_root_disposal_failure() {
    for entry in ["ask", "tell", "start", "drain"] {
        cancel(entry, false, Fault::RootDrop, false).await;
    }
}

#[tokio::test]
async fn cancelled_messages_and_hooks_keep_child_retirement_failure() {
    for entry in ["ask", "tell", "start", "drain"] {
        for fault in [Fault::ChildDrop, Fault::ChildFinish] {
            cancel(entry, false, fault, false).await;
        }
    }
}

#[tokio::test]
async fn cancelled_recovery_keeps_child_and_root_cleanup_failures() {
    for fault in [
        Fault::ChildDrop,
        Fault::ChildFinish,
        Fault::RecoveryDrop,
        Fault::RecoveryFinish,
    ] {
        cancel("ask", true, fault, false).await;
    }
}

#[tokio::test]
async fn recording_retirement_cannot_replace_an_earlier_future_cleanup_failure() {
    for recovery in [false, true] {
        cancel("ask", recovery, Fault::ChildFinish, true).await;
    }
}

#[tokio::test]
async fn clean_recording_cancellation_allows_recovery_without_inheriting_old_owners() {
    let probe = Arc::new(Probe::default());
    let mut guest = guest(true, true, &probe).await;
    guest.on_start_async(None).await.unwrap();
    probe.pending.store(true, SeqCst);
    let mut invocation = Box::pin(call(&mut guest, "ask", "ask", Some(probe.root())));
    assert!(invocation
        .as_mut()
        .poll(&mut Context::from_waker(std::task::Waker::noop()))
        .is_pending());
    drop(invocation);
    assert_eq!(guest.cleanup_error(), None);
    assert!(guest.is_alive());
    assert_eq!(
        *probe.statuses.lock().unwrap(),
        [(1, GuestSpanStatus::Cancelled)]
    );
    assert_eq!(probe.roots.load(SeqCst), 1);
    probe.pending.store(false, SeqCst);
    assert_eq!(
        guest.handle_async("count", b"", None).await.unwrap(),
        0u64.to_le_bytes()
    );
    assert_eq!(guest.handle_async("started", b"", None).await.unwrap(), [1]);
    assert_eq!(*probe.drops.lock().unwrap(), [1]);
}
