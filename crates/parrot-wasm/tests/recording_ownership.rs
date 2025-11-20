#![cfg(feature = "runtime")]

use parrot_wasm::{
    runtime::{GuestSpan, GuestSpanStatus, GuestTrace, WasmComponent, WasmRuntime},
    ComponentError, HostCtx, TraceParent, WasmConfig,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering::SeqCst},
    Arc, Mutex,
};

#[derive(Debug, Default)]
struct Observed {
    parents: AtomicUsize,
    roots_dropped: AtomicUsize,
    spans_created: AtomicUsize,
    spans_dropped: AtomicUsize,
    finished: Mutex<Vec<(usize, GuestSpanStatus)>>,
}

#[derive(Debug, Clone, Copy)]
enum ParentMode {
    Valid,
    Invalid,
    Panic,
}

#[derive(Debug)]
struct Root {
    observed: Arc<Observed>,
    parent: ParentMode,
    panic_drop: bool,
    bad_span: usize,
}

impl GuestTrace for Root {
    fn parent(&self) -> TraceParent {
        self.observed.parents.fetch_add(1, SeqCst);
        match self.parent {
            ParentMode::Panic => panic!("root parent failure"),
            ParentMode::Invalid => TraceParent {
                trace_id: [0; 16],
                span_id: [2; 8],
            },
            ParentMode::Valid => identity(2),
        }
    }

    fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
        Some(new_span(&self.observed, self.bad_span))
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        self.observed.roots_dropped.fetch_add(1, SeqCst);
        if self.panic_drop {
            panic!("root drop failure");
        }
    }
}

#[derive(Debug)]
struct Span {
    observed: Arc<Observed>,
    id: usize,
    bad_span: usize,
}

fn identity(id: u8) -> TraceParent {
    TraceParent {
        trace_id: [1; 16],
        span_id: [id; 8],
    }
}

fn new_span(observed: &Arc<Observed>, bad_span: usize) -> Box<dyn GuestSpan> {
    let id = observed.spans_created.fetch_add(1, SeqCst) + 1;
    Box::new(Span {
        observed: observed.clone(),
        id,
        bad_span,
    })
}

impl GuestTrace for Span {
    fn parent(&self) -> TraceParent {
        if self.id == self.bad_span {
            panic!("child parent failure");
        }
        identity(2 + self.id as u8)
    }

    fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
        Some(new_span(&self.observed, self.bad_span))
    }
}

impl GuestSpan for Span {
    fn finish(self: Box<Self>, status: GuestSpanStatus) {
        self.observed
            .finished
            .lock()
            .unwrap()
            .push((self.id, status));
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        self.observed.spans_dropped.fetch_add(1, SeqCst);
        if self.id == self.bad_span {
            panic!("child drop failure");
        }
    }
}

#[derive(Clone, Copy)]
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
        root: Box<dyn GuestTrace>,
    ) -> Result<(), ComponentError> {
        match self {
            Self::Ask => guest.handle_with_recording("state", b"", root).map(|_| ()),
            Self::Tell => guest.tell_with_recording("bump", &[3], root),
            Self::Start => guest.on_start_with_recording(root),
            Self::Drain => guest.on_drain_with_recording(root),
        }
    }
}

fn runtime() -> WasmRuntime {
    WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 2_000_000,
        epoch_deadline: 1000,
        ..Default::default()
    })
    .unwrap()
}

fn early_failure(entry: Entry, mode: ParentMode, panic_drop: bool) {
    let runtime = runtime();
    let mut guest = runtime
        .instantiate_bytes(
            include_bytes!("fixtures/fixture-component.wasm"),
            HostCtx::default(),
        )
        .unwrap();
    guest.on_start().unwrap();
    guest.tell("bump", &[7]).unwrap();
    guest.take_metrics();
    let observed = Arc::new(Observed::default());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        entry.call(
            &mut guest,
            Box::new(Root {
                observed: observed.clone(),
                parent: mode,
                panic_drop,
                bad_span: 0,
            }),
        )
    }))
    .expect("recording cleanup escaped the Wasm boundary");
    match mode {
        ParentMode::Invalid => assert_eq!(result, Err(ComponentError::InvalidTraceParent)),
        ParentMode::Panic => assert_eq!(
            result,
            Err(ComponentError::Panic("root parent failure".into()))
        ),
        ParentMode::Valid => unreachable!(),
    }
    assert_eq!(observed.parents.load(SeqCst), 1);
    assert_eq!(observed.roots_dropped.load(SeqCst), 1);
    assert_eq!(observed.spans_created.load(SeqCst), 0);
    let metrics = guest.take_metrics();
    assert_eq!(metrics.messages, 0);
    assert_eq!(metrics.fuel_consumed, 0);
    assert_eq!(metrics.trap_count, 0);
    assert_eq!(guest.handle("counter", b"").unwrap(), 7u64.to_le_bytes());
    assert_eq!(guest.handle("started", b"").unwrap(), [1]);
    assert_eq!(guest.handle("drained", b"").unwrap(), [0]);
}

macro_rules! early_cases {
    ($entry:ident, $invalid:ident, $parent:ident, $control:ident) => {
        #[test]
        fn $invalid() {
            early_failure(Entry::$entry, ParentMode::Invalid, true);
        }
        #[test]
        fn $parent() {
            early_failure(Entry::$entry, ParentMode::Panic, true);
        }
        #[test]
        fn $control() {
            early_failure(Entry::$entry, ParentMode::Invalid, false);
        }
    };
}

early_cases!(
    Ask,
    ask_invalid_parent_contains_root_drop,
    ask_parent_panic_contains_root_drop,
    ask_invalid_parent_control
);
early_cases!(
    Tell,
    tell_invalid_parent_contains_root_drop,
    tell_parent_panic_contains_root_drop,
    tell_invalid_parent_control
);
early_cases!(
    Start,
    start_invalid_parent_contains_root_drop,
    start_parent_panic_contains_root_drop,
    start_invalid_parent_control
);
early_cases!(
    Drain,
    drain_invalid_parent_contains_root_drop,
    drain_parent_panic_contains_root_drop,
    drain_invalid_parent_control
);

fn child_parent_failure(test: &str, bad_span: usize) {
    const CHILD: &str = "PARROT_RECORDING_OWNER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, "1")
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "recording ownership child failed: {status}"
                );
                return;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("recording ownership child timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let runtime = runtime();
    let mut guest = runtime
        .instantiate_bytes(
            include_bytes!("fixtures/recording-component.wasm"),
            HostCtx::default(),
        )
        .unwrap();
    guest.on_start().unwrap();
    let observed = Arc::new(Observed::default());
    assert_eq!(
        guest.handle_with_recording(
            "unfinished-nested",
            b"",
            Box::new(Root {
                observed: observed.clone(),
                parent: ParentMode::Valid,
                panic_drop: false,
                bad_span,
            })
        ),
        Err(ComponentError::Panic("child parent failure".into()))
    );
    assert_eq!(observed.parents.load(SeqCst), 1);
    assert_eq!(observed.roots_dropped.load(SeqCst), 1);
    assert_eq!(observed.spans_created.load(SeqCst), bad_span);
    assert_eq!(observed.spans_dropped.load(SeqCst), bad_span);
    let expected: Vec<_> = (1..bad_span)
        .map(|id| (id, GuestSpanStatus::Panicked))
        .collect();
    assert_eq!(*observed.finished.lock().unwrap(), expected);
    assert_eq!(guest.take_metrics().trap_count, 1);
    assert_eq!(guest.handle("disabled", b"").unwrap(), b"");
    assert_eq!(guest.handle("saved", b"").unwrap(), 0u64.to_le_bytes());
}

#[test]
fn first_child_parent_and_drop_panics_are_contained() {
    child_parent_failure("first_child_parent_and_drop_panics_are_contained", 1);
}

#[test]
fn nested_child_parent_and_drop_panics_retire_prior_spans() {
    child_parent_failure("nested_child_parent_and_drop_panics_retire_prior_spans", 2);
}

#[test]
fn admitted_root_drop_failure_remains_reported_and_vetoes_store_rebuild() {
    let runtime = runtime();
    let mut guest = runtime
        .instantiate_bytes(
            include_bytes!("fixtures/fixture-component.wasm"),
            HostCtx::default(),
        )
        .unwrap();
    guest.on_start().unwrap();
    guest.tell("bump", &[7]).unwrap();
    let observed = Arc::new(Observed::default());
    assert_eq!(
        guest.handle_with_recording(
            "state",
            b"",
            Box::new(Root {
                observed: observed.clone(),
                parent: ParentMode::Valid,
                panic_drop: true,
                bad_span: 0,
            })
        ),
        Err(ComponentError::Panic("root drop failure".into()))
    );
    assert_eq!(observed.roots_dropped.load(SeqCst), 1);
    let error = ComponentError::Panic("root drop failure".into());
    assert_eq!(guest.cleanup_error(), Some(error.clone()));
    assert!(!guest.is_alive());
    assert_eq!(guest.handle("counter", b""), Err(error.clone()));
    assert_eq!(guest.on_start(), Err(error));
}
