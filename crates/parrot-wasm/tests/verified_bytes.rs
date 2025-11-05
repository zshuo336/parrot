//! Byte admission is useful only if the exact verified input reaches the guest.
#![cfg(feature = "runtime")]
use parrot_wasm::{
    runtime::{CompilationLimits, WasmRuntime},
    ComponentError, HostCtx, WasmConfig,
};

const FIXTURE: &[u8] = include_bytes!("fixtures/fixture-component.wasm");

#[test]
fn borrowed_verified_bytes_are_owned_after_instantiation_and_instances_are_isolated() {
    let runtime = WasmRuntime::new(WasmConfig {
        epoch_deadline: 1000,
        ..Default::default()
    })
    .unwrap();
    let mut input = FIXTURE.to_vec();
    let mut first = runtime
        .instantiate_bytes(
            &input,
            HostCtx {
                self_path: "/first".into(),
                ..Default::default()
            },
        )
        .unwrap();
    input.fill(0);
    drop(input);
    first.on_start().unwrap();
    assert_eq!(first.handle("self", &[]).unwrap(), b"/first");
    assert_eq!(first.handle("state", &[]).unwrap(), 1u64.to_le_bytes());
    let mut second = runtime
        .instantiate_bytes(
            FIXTURE,
            HostCtx {
                self_path: "/second".into(),
                ..Default::default()
            },
        )
        .unwrap();
    second.on_start().unwrap();
    assert_eq!(second.handle("self", &[]).unwrap(), b"/second");
    assert_eq!(second.handle("counter", &[]).unwrap(), 0u64.to_le_bytes());
    assert_eq!(first.handle("counter", &[]).unwrap(), 1u64.to_le_bytes());
}

#[test]
fn byte_api_enforces_input_limits_and_contract_validation() {
    let runtime = WasmRuntime::new_with_compilation_limits(
        WasmConfig::default(),
        CompilationLimits {
            max_input_bytes: FIXTURE.len() - 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(
        runtime.instantiate_bytes(FIXTURE, HostCtx::default()),
        Err(ComponentError::ResourceLimit(_))
    ));
    assert!(matches!(
        runtime.instantiate_bytes(b"invalid component", HostCtx::default()),
        Err(ComponentError::AbiVersion(_))
    ));
    assert!(matches!(
        runtime.instantiate_bytes(b"(component)", HostCtx::default()),
        Err(ComponentError::AbiVersion(_))
    ));
}
