#![cfg(feature = "runtime")]

use parrot_wasm::{
    runtime::{GuestSpan, GuestSpanStatus, GuestTrace, WasmComponent, WasmRuntime},
    ComponentError, HostCtx, TraceParent, WasmConfig,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering::SeqCst},
    Arc, Mutex,
};

#[derive(Debug, Clone, Copy)]
enum Fault {
    ZeroTrace,
    ZeroSpan,
    ForeignTrace,
    RootSpan,
    ReusedSpan,
    None,
}

#[derive(Debug, Default)]
struct Observed {
    parents: AtomicUsize,
    roots_dropped: AtomicUsize,
    created: AtomicUsize,
    dropped: AtomicUsize,
    finished: Mutex<Vec<(usize, GuestSpanStatus)>>,
}

#[derive(Debug)]
struct Provider {
    observed: Arc<Observed>,
    fault: Fault,
    bad_child: usize,
    panic_drop: bool,
}

fn identity(id: u64) -> TraceParent {
    TraceParent {
        trace_id: [1; 16],
        span_id: id.to_be_bytes(),
    }
}

impl Provider {
    fn child(self: &Arc<Self>) -> Box<dyn GuestSpan> {
        let id = self.observed.created.fetch_add(1, SeqCst) + 1;
        let mut parent = identity(id as u64 + 2);
        if id == self.bad_child {
            match self.fault {
                Fault::ZeroTrace => parent.trace_id = [0; 16],
                Fault::ZeroSpan => parent.span_id = [0; 8],
                Fault::ForeignTrace => parent.trace_id = [9; 16],
                Fault::RootSpan => parent = identity(2),
                Fault::ReusedSpan => parent = identity(3),
                Fault::None => {}
            }
        }
        Box::new(Span {
            provider: self.clone(),
            id,
            parent,
        })
    }
}

#[derive(Debug)]
struct Root(Arc<Provider>);

impl GuestTrace for Root {
    fn parent(&self) -> TraceParent {
        self.0.observed.parents.fetch_add(1, SeqCst);
        identity(2)
    }
    fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
        Some(self.0.child())
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        self.0.observed.roots_dropped.fetch_add(1, SeqCst);
    }
}

#[derive(Debug)]
struct Span {
    provider: Arc<Provider>,
    id: usize,
    parent: TraceParent,
}
impl GuestTrace for Span {
    fn parent(&self) -> TraceParent {
        self.parent
    }
    fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
        Some(self.provider.child())
    }
}
impl GuestSpan for Span {
    fn finish(self: Box<Self>, status: GuestSpanStatus) {
        self.provider
            .observed
            .finished
            .lock()
            .unwrap()
            .push((self.id, status));
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        self.provider.observed.dropped.fetch_add(1, SeqCst);
        if self.provider.panic_drop && self.id == self.provider.bad_child {
            panic!("rejected child disposal failed");
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Ask,
    Tell,
    Start,
    Drain,
}
impl Entry {
    fn call(
        self,
        guest: &mut WasmComponent,
        key: &str,
        root: Box<dyn GuestTrace>,
    ) -> Result<(), ComponentError> {
        match self {
            Self::Ask => guest.handle_with_recording(key, b"", root).map(|_| ()),
            Self::Tell => guest.tell_with_recording(key, b"", root),
            Self::Start => guest.on_start_with_recording(root),
            Self::Drain => guest.on_drain_with_recording(root),
        }
    }

    async fn call_async(
        self,
        guest: &mut WasmComponent,
        key: &str,
        root: Box<dyn GuestTrace>,
    ) -> Result<(), ComponentError> {
        match self {
            Self::Ask => guest.handle_async(key, b"", Some(root)).await.map(|_| ()),
            Self::Tell => guest.tell_async(key, b"", Some(root)).await,
            Self::Start => guest.on_start_async(Some(root)).await,
            Self::Drain => guest.on_drain_async(Some(root)).await,
        }
    }
}

fn runtime() -> WasmRuntime {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<WasmComponent>();
    WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 2_000_000,
        epoch_deadline: 10_000,
        ..Default::default()
    })
    .unwrap()
}
const GUEST: &[u8] = include_bytes!("fixtures/recording-component.wasm");

fn capability(
    fault: Fault,
    bad_child: usize,
    panic_drop: bool,
) -> (Arc<Observed>, Box<dyn GuestTrace>) {
    let observed = Arc::new(Observed::default());
    let root = Box::new(Root(Arc::new(Provider {
        observed: observed.clone(),
        fault,
        bad_child,
        panic_drop,
    })));
    (observed, root)
}

fn rejected(
    result: Result<(), ComponentError>,
    observed: &Observed,
    bad_child: usize,
    completed: bool,
) {
    match result {
        Err(ComponentError::Trap(message)) => assert!(
            message.contains("invalid host recording child identity"),
            "wrong trap: {message}"
        ),
        other => panic!("invalid host child identity was not rejected: {other:?}"),
    }
    assert_eq!(observed.parents.load(SeqCst), 1);
    assert_eq!(observed.created.load(SeqCst), bad_child);
    assert_eq!(observed.dropped.load(SeqCst), bad_child);
    assert_eq!(observed.roots_dropped.load(SeqCst), 1);
    let status = if completed {
        GuestSpanStatus::Ok
    } else {
        GuestSpanStatus::Error
    };
    let expected: Vec<_> = (1..bad_child).map(|id| (id, status)).collect();
    assert_eq!(*observed.finished.lock().unwrap(), expected);
}

fn faults(entry: Entry) -> Vec<(Fault, usize)> {
    let mut cases = vec![
        (Fault::ZeroTrace, 1),
        (Fault::ZeroSpan, 1),
        (Fault::ForeignTrace, 1),
        (Fault::RootSpan, 1),
    ];
    if matches!(entry, Entry::Ask | Entry::Tell) {
        cases.extend([(Fault::ReusedSpan, 2), (Fault::ReusedSpan, 3)]);
    }
    cases
}

fn synchronous(entry: Entry) {
    let runtime = runtime();
    for (fault, bad_child) in faults(entry) {
        let mut guest = runtime
            .instantiate_bytes(GUEST, HostCtx::default())
            .unwrap();
        guest.on_start().unwrap();
        guest.take_metrics();
        let (observed, root) = capability(fault, bad_child, false);
        rejected(
            entry.call(&mut guest, "unfinished-nested", root),
            &observed,
            bad_child,
            false,
        );
        assert_eq!(guest.take_metrics().trap_count, 1);
        assert_eq!(guest.handle("saved", b"").unwrap(), 0u64.to_le_bytes());
        assert!(guest.handle("start-identity", b"").unwrap().is_empty());
        assert!(guest.handle("drain-identity", b"").unwrap().is_empty());
        assert!(guest.handle("disabled", b"").unwrap().is_empty());
    }
}

async fn asynchronous(entry: Entry) {
    let runtime = runtime();
    for (fault, bad_child) in faults(entry) {
        let mut guest = runtime
            .prepare_bytes(GUEST)
            .unwrap()
            .instantiate_async(HostCtx::default(), None)
            .await
            .unwrap();
        guest.on_start_async(None).await.unwrap();
        guest.take_metrics();
        let (observed, root) = capability(fault, bad_child, false);
        rejected(
            entry
                .call_async(&mut guest, "unfinished-nested", root)
                .await,
            &observed,
            bad_child,
            false,
        );
        assert_eq!(guest.take_metrics().trap_count, 1);
        assert_eq!(
            guest.handle_async("saved", b"", None).await.unwrap(),
            0u64.to_le_bytes()
        );
        assert!(guest
            .handle_async("start-identity", b"", None)
            .await
            .unwrap()
            .is_empty());
        assert!(guest
            .handle_async("drain-identity", b"", None)
            .await
            .unwrap()
            .is_empty());
        assert!(guest
            .handle_async("disabled", b"", None)
            .await
            .unwrap()
            .is_empty());
    }
}

macro_rules! entries {
    ($sync:ident, $async:ident, $entry:ident) => {
        #[test]
        fn $sync() {
            synchronous(Entry::$entry);
        }
        #[tokio::test]
        async fn $async() {
            asynchronous(Entry::$entry).await;
        }
    };
}
entries!(
    sync_ask_rejects_invalid_children,
    async_ask_rejects_invalid_children,
    Ask
);
entries!(
    sync_tell_rejects_invalid_children,
    async_tell_rejects_invalid_children,
    Tell
);
entries!(
    sync_start_rejects_invalid_children,
    async_start_rejects_invalid_children,
    Start
);
entries!(
    sync_drain_rejects_invalid_children,
    async_drain_rejects_invalid_children,
    Drain
);

#[test]
fn sync_completed_child_identity_cannot_be_reused() {
    let runtime = runtime();
    let mut guest = runtime
        .instantiate_bytes(GUEST, HostCtx::default())
        .unwrap();
    let (observed, root) = capability(Fault::ReusedSpan, 2, false);
    rejected(
        Entry::Ask.call(&mut guest, "statuses", root),
        &observed,
        2,
        true,
    );
}

#[tokio::test]
async fn async_completed_child_identity_cannot_be_reused() {
    let runtime = runtime();
    let mut guest = runtime
        .prepare_bytes(GUEST)
        .unwrap()
        .instantiate_async(HostCtx::default(), None)
        .await
        .unwrap();
    let (observed, root) = capability(Fault::ReusedSpan, 2, false);
    rejected(
        Entry::Ask.call_async(&mut guest, "statuses", root).await,
        &observed,
        2,
        true,
    );
}

fn full_budget(observed: &Observed) {
    assert_eq!(observed.parents.load(SeqCst), 1);
    assert_eq!(observed.created.load(SeqCst), 256);
    assert_eq!(observed.dropped.load(SeqCst), 256);
    assert_eq!(observed.roots_dropped.load(SeqCst), 1);
    assert_eq!(
        *observed.finished.lock().unwrap(),
        (1..=256)
            .map(|id| (id, GuestSpanStatus::Ok))
            .collect::<Vec<_>>()
    );
}

#[test]
fn sync_distinct_children_keep_full_budget_and_next_invocation() {
    let runtime = runtime();
    let mut guest = runtime
        .instantiate_bytes(GUEST, HostCtx::default())
        .unwrap();
    for _ in 0..2 {
        let (observed, root) = capability(Fault::None, 0, false);
        Entry::Ask.call(&mut guest, "budget", root).unwrap();
        full_budget(&observed);
    }
}

#[tokio::test]
async fn async_distinct_children_keep_full_budget_and_next_invocation() {
    let runtime = runtime();
    let mut guest = runtime
        .prepare_bytes(GUEST)
        .unwrap()
        .instantiate_async(HostCtx::default(), None)
        .await
        .unwrap();
    for _ in 0..2 {
        let (observed, root) = capability(Fault::None, 0, false);
        Entry::Ask
            .call_async(&mut guest, "budget", root)
            .await
            .unwrap();
        full_budget(&observed);
    }
}

#[test]
fn rejected_child_drop_panic_preserves_identity_error_and_retires_prior_owners() {
    let runtime = runtime();
    let mut guest = runtime
        .instantiate_bytes(GUEST, HostCtx::default())
        .unwrap();
    let (observed, root) = capability(Fault::ZeroTrace, 2, true);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Entry::Ask.call(&mut guest, "unfinished-nested", root)
    }))
    .expect("rejected child disposal escaped runtime");
    rejected(result, &observed, 2, false);
    assert_eq!(guest.handle("saved", b"").unwrap(), 0u64.to_le_bytes());
}
