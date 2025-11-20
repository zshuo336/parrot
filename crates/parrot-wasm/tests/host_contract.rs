//! Host contract regressions using real guest imports, not mocked Wasm calls.
#![cfg(feature = "runtime")]

use parrot_wasm::runtime::WasmRuntime;
use parrot_wasm::{ComponentError, HostCtx, WasmConfig};
use std::path::Path;
use std::sync::{Arc, Mutex};
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

struct LogEvents(Arc<Mutex<Vec<tracing::Level>>>);

impl tracing::Subscriber for LogEvents {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target() == "parrot_wasm" {
            self.0.lock().unwrap().push(*event.metadata().level());
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn log_expectations() -> [Vec<tracing::Level>; 5] {
    use tracing::Level;
    [
        vec![Level::ERROR],
        vec![Level::ERROR, Level::WARN],
        vec![Level::ERROR, Level::WARN, Level::INFO],
        vec![Level::ERROR, Level::WARN, Level::INFO, Level::DEBUG],
        vec![
            Level::ERROR,
            Level::WARN,
            Level::INFO,
            Level::DEBUG,
            Level::TRACE,
        ],
    ]
}

#[test]
fn guest_logs_obey_each_host_threshold_before_and_after_recovery() {
    let runtime = runtime();
    for (threshold, expected) in log_expectations().into_iter().enumerate() {
        let mut component = runtime
            .instantiate(
                &fixture(),
                HostCtx {
                    log_level: threshold as u8,
                    ..Default::default()
                },
            )
            .unwrap();
        for round in 0..2 {
            let events = Arc::new(Mutex::new(Vec::new()));
            tracing::subscriber::with_default(LogEvents(events.clone()), || {
                for level in 0..=4 {
                    assert_eq!(component.handle("log", &[level, b'x']).unwrap(), b"logged");
                }
            });
            assert_eq!(
                *events.lock().unwrap(),
                expected,
                "threshold={threshold} round={round}"
            );
            if round == 0 {
                assert_eq!(
                    component.handle("spin", &[]),
                    Err(ComponentError::OutOfFuel)
                );
            }
        }
    }
}

#[tokio::test]
async fn async_guest_logs_obey_each_host_threshold_before_and_after_recovery() {
    use tracing::instrument::WithSubscriber;

    let runtime = runtime();
    let bytes = std::fs::read(fixture()).unwrap();
    for (threshold, expected) in log_expectations().into_iter().enumerate() {
        let mut component = runtime
            .prepare_bytes(&bytes)
            .unwrap()
            .instantiate_async(
                HostCtx {
                    log_level: threshold as u8,
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        for round in 0..2 {
            let events = Arc::new(Mutex::new(Vec::new()));
            async {
                for level in 0..=4 {
                    assert_eq!(
                        component
                            .handle_async("log", &[level, b'x'], None)
                            .await
                            .unwrap(),
                        b"logged"
                    );
                }
            }
            .with_subscriber(LogEvents(events.clone()))
            .await;
            assert_eq!(
                *events.lock().unwrap(),
                expected,
                "threshold={threshold} round={round}"
            );
            if round == 0 {
                assert_eq!(
                    component.handle_async("spin", &[], None).await,
                    Err(ComponentError::OutOfFuel)
                );
            }
        }
    }
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
