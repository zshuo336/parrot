#![cfg(feature = "runtime")]

use parrot_wasm::{ComponentError, HostCtx, WasmComponent, WasmConfig, WasmRuntime};
use std::path::PathBuf;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-component.wasm")
}

fn runtime(memory_limit_mb: usize, fuel_per_message: u64) -> WasmRuntime {
    WasmRuntime::new(WasmConfig {
        memory_limit_mb,
        fuel_per_message,
        epoch_deadline: 1_000_000,
    })
    .unwrap()
}

fn pages(component: &mut WasmComponent) -> u64 {
    u64::from_le_bytes(
        component
            .handle("memory-pages", &[])
            .unwrap()
            .try_into()
            .unwrap(),
    )
}

fn grow(component: &mut WasmComponent, count: u32) -> u64 {
    u64::from_le_bytes(
        component
            .handle("grow", &count.to_le_bytes())
            .unwrap()
            .try_into()
            .unwrap(),
    )
}

#[test]
fn initial_guest_memory_must_fit_the_configured_budget() {
    let limited = runtime(1, 100_000);
    assert!(limited.instantiate(&fixture(), HostCtx::default()).is_err());
    let enough = runtime(2, 100_000);
    assert!(enough.instantiate(&fixture(), HostCtx::default()).is_ok());
}

#[test]
fn growth_beyond_the_budget_is_rejected_without_corrupting_the_guest() {
    let rt = runtime(2, 100_000);
    let mut guest = rt.instantiate(&fixture(), HostCtx::default()).unwrap();
    let before = pages(&mut guest);
    assert!(before <= 32);
    assert_eq!(grow(&mut guest, 33), u32::MAX as u64);
    assert_eq!(pages(&mut guest), before);
    assert_eq!(
        guest.handle("echo", b"still usable").unwrap(),
        b"still usable"
    );
}

#[test]
fn memory_budget_is_enforced_again_after_trap_rebuild() {
    let rt = runtime(2, 100_000);
    let mut guest = rt.instantiate(&fixture(), HostCtx::default()).unwrap();
    assert_eq!(
        guest.handle("spin", &[]).unwrap_err(),
        ComponentError::OutOfFuel
    );
    assert_eq!(grow(&mut guest, 33), u32::MAX as u64);
    assert!(pages(&mut guest) <= 32);
}

#[test]
fn zero_and_overflowing_memory_configuration_are_rejected() {
    for memory_limit_mb in [0, usize::MAX] {
        assert!(WasmRuntime::new(WasmConfig {
            memory_limit_mb,
            ..Default::default()
        })
        .is_err());
    }
}

#[test]
fn maximum_fuel_does_not_panic_or_wrap_instantiation_budget() {
    let result = std::panic::catch_unwind(|| {
        let rt = runtime(2, u64::MAX);
        let mut guest = rt.instantiate(&fixture(), HostCtx::default()).unwrap();
        assert_eq!(guest.handle("echo", b"high fuel").unwrap(), b"high fuel");
    });
    assert!(result.is_ok(), "large valid fuel must not cause host panic");
}

#[test]
fn lifecycle_hooks_can_recover_after_a_guest_trap() {
    let rt = runtime(2, 100_000);
    let mut guest = rt.instantiate(&fixture(), HostCtx::default()).unwrap();
    assert_eq!(
        guest.handle("spin", &[]).unwrap_err(),
        ComponentError::OutOfFuel
    );
    guest
        .on_start()
        .expect("start must enter a fresh bounded scope");
    assert_eq!(guest.handle("started", &[]).unwrap(), [1]);
    assert_eq!(
        guest.handle("spin", &[]).unwrap_err(),
        ComponentError::OutOfFuel
    );
    guest
        .on_drain()
        .expect("drain must enter a fresh bounded scope");
    assert_eq!(guest.handle("drained", &[]).unwrap(), [1]);
}

#[test]
fn rebuild_restores_successful_start_before_accepting_business_messages() {
    let rt = runtime(2, 100_000);
    let mut guest = rt.instantiate(&fixture(), HostCtx::default()).unwrap();
    guest.on_start().unwrap();
    assert_eq!(guest.handle("started", &[]).unwrap(), [1]);
    assert_eq!(
        guest.handle("spin", &[]).unwrap_err(),
        ComponentError::OutOfFuel
    );
    assert_eq!(guest.handle("started", &[]).unwrap(), [1]);
}

#[test]
fn denied_growth_does_not_spend_capacity_and_exact_boundary_is_usable() {
    let rt = runtime(2, 100_000);
    let mut guest = rt.instantiate(&fixture(), HostCtx::default()).unwrap();
    let before = pages(&mut guest);
    assert_eq!(grow(&mut guest, 33), u32::MAX as u64);
    assert_eq!(grow(&mut guest, 32 - before as u32), before);
    assert_eq!(pages(&mut guest), 32);
    assert_eq!(grow(&mut guest, 1), u32::MAX as u64);
    assert_eq!(guest.handle("echo", b"at limit").unwrap(), b"at limit");
}

#[test]
fn each_lifecycle_hook_receives_the_configured_fuel_budget() {
    let rt = runtime(2, 1);
    let mut guest = rt.instantiate(&fixture(), HostCtx::default()).unwrap();
    for _ in 0..2 {
        assert_eq!(guest.on_start().unwrap_err(), ComponentError::OutOfFuel);
        assert_eq!(guest.on_drain().unwrap_err(), ComponentError::OutOfFuel);
    }
    assert_eq!(guest.take_metrics().out_of_fuel_count, 4);
}
