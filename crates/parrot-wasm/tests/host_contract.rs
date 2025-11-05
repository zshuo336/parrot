//! Host contract regressions using real guest imports, not mocked Wasm calls.
#![cfg(feature = "runtime")]

use parrot_wasm::runtime::WasmRuntime;
use parrot_wasm::{ComponentError, HostCtx, WasmConfig};
use std::path::Path;
use std::time::Instant;

fn runtime() -> WasmRuntime {
    WasmRuntime::new(WasmConfig {
        epoch_deadline: 1_000_000,
        ..Default::default()
    })
    .unwrap()
}

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-component.wasm")
}

#[test]
fn epoch_budget_cannot_overflow_wasmtimes_absolute_deadline() {
    for epoch_deadline in [u32::MAX as u64 + 1, u64::MAX] {
        let config = WasmConfig {
            epoch_deadline,
            ..Default::default()
        };
        assert!(matches!(
            WasmRuntime::new(config.clone()),
            Err(ComponentError::InvalidConfig(_))
        ));
        assert!(matches!(
            WasmRuntime::new_with_manual_epoch(config),
            Err(ComponentError::InvalidConfig(_))
        ));
    }
    let runtime = WasmRuntime::new_with_manual_epoch(WasmConfig {
        epoch_deadline: u32::MAX as u64,
        ..Default::default()
    })
    .unwrap();
    runtime.tick_epoch();
    let mut component = runtime.instantiate(&fixture(), HostCtx::default()).unwrap();
    assert_eq!(
        component.handle("echo", b"bounded delta").unwrap(),
        b"bounded delta"
    );
}

#[test]
fn clock_is_elapsed_monotonic_time_across_recovery() {
    let runtime = runtime();
    let mut component = runtime.instantiate(&fixture(), HostCtx::default()).unwrap();
    let elapsed = Instant::now();
    let read = |component: &mut parrot_wasm::runtime::WasmComponent| {
        u64::from_le_bytes(component.handle("clock", &[]).unwrap().try_into().unwrap())
    };
    let first = read(&mut component);
    // This process has just started. A Unix timestamp violates the WIT contract.
    assert!(
        first < 86_400_000,
        "expected process-relative time, got {first}"
    );
    assert_eq!(
        component.handle("spin", &[]),
        Err(ComponentError::OutOfFuel)
    );
    let recovered = read(&mut component);
    assert!(
        recovered >= first,
        "rebuild must not reset the clock origin"
    );
    let upper_bound = elapsed.elapsed().as_millis() as u64 + 1;
    assert!(recovered - first <= upper_bound);
}

struct PanickingLog;

impl tracing::Subscriber for PanickingLog {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        panic!("injected log sink failure")
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn host_callback_panic_is_reported_and_guest_is_rebuilt() {
    let runtime = runtime();
    let mut component = runtime.instantiate(&fixture(), HostCtx::default()).unwrap();
    component.on_start().unwrap();
    component.tell("bump", &[9]).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tracing::subscriber::with_default(PanickingLog, || component.handle("log", b"\x00boom"))
    }))
    .expect("host callback panic must not escape the public runtime API");
    assert_eq!(
        result,
        Err(ComponentError::Panic("injected log sink failure".into()))
    );
    assert_eq!(component.take_metrics().trap_count, 1);
    assert_eq!(
        component.handle("counter", &[]).unwrap(),
        0u64.to_le_bytes()
    );
    assert_eq!(component.handle("started", &[]).unwrap(), [1]);
    assert_eq!(component.handle("echo", b"alive").unwrap(), b"alive");
}
