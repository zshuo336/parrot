#![cfg(feature = "runtime")]

use parrot_wasm::{runtime::WasmRuntime, ComponentError, HostCtx, TraceParent, WasmConfig};

fn runtime() -> WasmRuntime {
    WasmRuntime::new_with_manual_epoch(WasmConfig {
        fuel_per_message: 200_000,
        epoch_deadline: 1000,
        ..Default::default()
    })
    .unwrap()
}

fn fixture(traced: bool) -> std::path::PathBuf {
    if !traced {
        if let Some(path) = std::env::var_os("PARROT_WASM_LEGACY_REBUILT") {
            return path.into();
        }
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(if traced {
        "tests/fixtures/trace-component.wasm"
    } else {
        "tests/fixtures/fixture-component.wasm"
    })
}

fn parents() -> [TraceParent; 2] {
    [
        TraceParent {
            trace_id: [7; 16],
            span_id: [13; 8],
        },
        TraceParent {
            trace_id: [23; 16],
            span_id: [31; 8],
        },
    ]
}

fn bytes(parent: Option<&TraceParent>) -> Vec<u8> {
    parent.map_or_else(Vec::new, |parent| {
        [parent.trace_id.as_slice(), parent.span_id.as_slice()].concat()
    })
}

#[test]
fn legacy_guest_preserves_business_errors_and_tells_with_explicit_parents() {
    let runtime = runtime();
    let mut guest = runtime
        .instantiate(&fixture(false), HostCtx::default())
        .unwrap();
    guest.on_start().unwrap();
    let [first, second] = parents();
    assert_eq!(
        guest
            .handle_with_trace("echo", b"legacy", Some(&first))
            .unwrap(),
        b"legacy"
    );
    assert_eq!(
        guest
            .handle_with_trace("err", b"", Some(&second))
            .unwrap_err(),
        ComponentError::Trap("fixture-err".into())
    );
    guest.tell_with_trace("bump", &[7], Some(&first)).unwrap();
    assert_eq!(guest.handle("counter", b"").unwrap(), 7u64.to_le_bytes());
    guest.on_drain().unwrap();
    assert_eq!(
        guest
            .handle_with_trace("drained", b"", Some(&second))
            .unwrap(),
        [1]
    );
}

#[test]
fn guest_import_observes_each_message_but_lifecycle_has_no_message_parent() {
    let runtime = runtime();
    let mut guest = runtime
        .instantiate(&fixture(true), HostCtx::default())
        .unwrap();
    guest.on_start().unwrap();
    let [first, second] = parents();
    for parent in [Some(&first), None, Some(&second), Some(&first), None] {
        assert_eq!(
            guest.handle_with_trace("parent", b"", parent).unwrap(),
            bytes(parent)
        );
        assert!(guest.handle("parent", b"").unwrap().is_empty());
    }
    assert!(guest
        .handle_with_trace("start-parent", b"", Some(&second))
        .unwrap()
        .is_empty());
    guest.on_drain().unwrap();
    assert!(guest
        .handle_with_trace("drain-parent", b"", Some(&first))
        .unwrap()
        .is_empty());
    guest.on_start().unwrap();
    assert!(guest.handle("start-parent", b"").unwrap().is_empty());
}

#[test]
fn independent_instances_tells_and_business_errors_do_not_leak_call_context() {
    let runtime = runtime();
    let mut a = runtime
        .instantiate(&fixture(true), HostCtx::default())
        .unwrap();
    let mut b = runtime
        .instantiate(&fixture(true), HostCtx::default())
        .unwrap();
    let [first, second] = parents();
    a.tell_with_trace("remember", b"", Some(&first)).unwrap();
    b.tell_with_trace("remember", b"", Some(&second)).unwrap();
    assert_eq!(a.handle("saved-parent", b"").unwrap(), bytes(Some(&first)));
    assert_eq!(b.handle("saved-parent", b"").unwrap(), bytes(Some(&second)));
    assert!(a.handle("parent", b"").unwrap().is_empty());
    assert!(b.handle("parent", b"").unwrap().is_empty());
    assert_eq!(
        a.handle_with_trace("err", b"", Some(&second)).unwrap_err(),
        ComponentError::Trap("fixture-trace-error".into())
    );
    assert_eq!(a.handle("saved-parent", b"").unwrap(), bytes(Some(&second)));
    assert!(a.handle("parent", b"").unwrap().is_empty());
    a.tell("remember", b"").unwrap();
    assert!(a.handle("saved-parent", b"").unwrap().is_empty());
    assert_eq!(b.handle("saved-parent", b"").unwrap(), bytes(Some(&second)));
}

#[test]
fn invalid_ids_have_no_guest_effect_and_trap_rebuild_does_not_inherit_the_parent() {
    let runtime = runtime();
    let mut guest = runtime
        .instantiate(&fixture(true), HostCtx::default())
        .unwrap();
    guest.on_start().unwrap();
    guest.take_metrics();
    let [first, second] = parents();
    for parent in [
        TraceParent {
            trace_id: [0; 16],
            ..first
        },
        TraceParent {
            span_id: [0; 8],
            ..first
        },
    ] {
        assert_eq!(
            guest
                .handle_with_trace("remember", b"", Some(&parent))
                .unwrap_err(),
            ComponentError::InvalidTraceParent
        );
        assert_eq!(
            guest
                .tell_with_trace("remember", b"", Some(&parent))
                .unwrap_err(),
            ComponentError::InvalidTraceParent
        );
    }
    assert_eq!(guest.take_metrics().messages, 0);
    assert_eq!(guest.handle("calls", b"").unwrap(), 0u64.to_le_bytes());
    assert!(matches!(
        guest.handle_with_trace("trap", b"", Some(&first)),
        Err(ComponentError::Trap(_))
    ));
    guest.take_metrics();
    let invalid = TraceParent {
        span_id: [0; 8],
        ..first
    };
    assert_eq!(
        guest
            .handle_with_trace("parent", b"", Some(&invalid))
            .unwrap_err(),
        ComponentError::InvalidTraceParent
    );
    assert_eq!(
        guest.take_metrics().fuel_consumed,
        0,
        "invalid IDs triggered guest recovery/startup"
    );
    assert!(
        guest
            .handle_with_trace("start-parent", b"", Some(&second))
            .unwrap()
            .is_empty(),
        "rebuild startup inherited the message that triggered recovery"
    );
    assert_eq!(
        guest
            .handle_with_trace("parent", b"", Some(&second))
            .unwrap(),
        bytes(Some(&second))
    );
    assert!(
        guest.handle("saved-parent", b"").unwrap().is_empty(),
        "trapped guest state survived rebuild"
    );
    assert_eq!(
        guest
            .handle_with_trace("spin", b"", Some(&first))
            .unwrap_err(),
        ComponentError::OutOfFuel
    );
    assert!(guest.handle("parent", b"").unwrap().is_empty());
    assert!(guest.handle("start-parent", b"").unwrap().is_empty());
}

enum GuestLog {
    Panic,
    AdvanceEpoch(wasmtime::Engine),
}

impl tracing::Subscriber for GuestLog {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == "parrot_wasm"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target() != "parrot_wasm" {
            return;
        }
        match self {
            Self::Panic => panic!("wasm trace log failure"),
            Self::AdvanceEpoch(engine) => {
                for _ in 0..1000 {
                    engine.increment_epoch();
                }
            }
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn caught_host_callback_panic_does_not_escape_or_leak_into_recovery() {
    let runtime = runtime();
    let mut guest = runtime
        .instantiate(&fixture(true), HostCtx::default())
        .unwrap();
    guest.on_start().unwrap();
    let [first, second] = parents();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tracing::subscriber::with_default(GuestLog::Panic, || {
            guest.handle_with_trace("host-panic", b"", Some(&first))
        })
    }))
    .expect("host callback panic escaped the Wasm runtime");
    assert_eq!(
        result.unwrap_err(),
        ComponentError::Panic("wasm trace log failure".into())
    );
    assert!(guest
        .handle_with_trace("start-parent", b"", Some(&second))
        .unwrap()
        .is_empty());
    assert_eq!(
        guest
            .handle_with_trace("parent", b"", Some(&second))
            .unwrap(),
        bytes(Some(&second))
    );
    assert!(guest.handle("saved-parent", b"").unwrap().is_empty());
    assert!(guest.handle("parent", b"").unwrap().is_empty());
}

#[test]
fn epoch_interrupt_after_guest_entry_clears_context_before_recovery() {
    let runtime = runtime();
    let mut guest = runtime
        .instantiate(&fixture(true), HostCtx::default())
        .unwrap();
    guest.on_start().unwrap();
    let [first, second] = parents();
    // The guest log import advances the manual engine clock after the public
    // call has armed its deadline; host scheduling cannot race this gate.
    let result =
        tracing::subscriber::with_default(GuestLog::AdvanceEpoch(runtime.engine().clone()), || {
            guest.handle_with_trace("epoch", b"", Some(&first))
        });
    assert_eq!(result.unwrap_err(), ComponentError::EpochDeadline);
    assert!(guest
        .handle_with_trace("start-parent", b"", Some(&second))
        .unwrap()
        .is_empty());
    assert_eq!(
        guest
            .handle_with_trace("parent", b"", Some(&second))
            .unwrap(),
        bytes(Some(&second))
    );
    assert!(guest.handle("saved-parent", b"").unwrap().is_empty());
    assert!(guest.handle("parent", b"").unwrap().is_empty());
}
