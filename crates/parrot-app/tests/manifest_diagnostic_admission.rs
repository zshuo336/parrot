//! Rejected diagnostic text must not be fully materialized before admission.
use parrot_app::{
    limits::AdmissionError,
    manifest::{
        validate_with_limits, ArtifactRef, ComponentSpec, EngineKind, ManifestError, WireSpec,
    },
    AppManifest, ApplicationLimits,
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

thread_local! {
    static OBSERVING: Cell<bool> = const { Cell::new(false) };
    static LARGEST: Cell<usize> = const { Cell::new(0) };
    static REQUESTED: Cell<usize> = const { Cell::new(0) };
}
struct Meter;
fn record(bytes: usize) {
    if OBSERVING.try_with(Cell::get).unwrap_or(false) {
        let _ = LARGEST.try_with(|value| value.set(value.get().max(bytes)));
        let _ = REQUESTED.try_with(|value| value.set(value.get().saturating_add(bytes)));
    }
}
// SAFETY: pointer and layout operations are forwarded unchanged to System.
// Const TLS accounting does not allocate and tolerates thread teardown.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, bytes: usize) -> *mut u8 {
        record(bytes);
        unsafe { System.realloc(pointer, layout, bytes) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;

fn measured<T>(run: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            OBSERVING.set(false);
        }
    }
    LARGEST.set(0);
    REQUESTED.set(0);
    OBSERVING.set(true);
    let reset = Reset;
    let result = run();
    drop(reset);
    println!(
        "diagnostic allocation requests: largest={}, cumulative={}",
        LARGEST.get(),
        REQUESTED.get()
    );
    (result, LARGEST.get())
}
fn component(name: &str) -> ComponentSpec {
    ComponentSpec {
        name: name.into(),
        engine: EngineKind::Parrot,
        artifact: ArtifactRef::Props {
            factory: "probe".into(),
        },
        alt_artifact: None,
        instances: Default::default(),
        placement: Default::default(),
        upgrade: Default::default(),
        deps: vec![],
        config: None,
        hooks: Default::default(),
    }
}
fn model() -> AppManifest {
    AppManifest {
        name: "diagnostics".into(),
        version: "1.0.0".into(),
        components: vec![component("worker")],
        wiring: vec![],
        mirrors: vec![],
        config_overlay: None,
    }
}
fn bounded(largest: usize) {
    assert!(
        largest < 128 * 1024,
        "materialized large diagnostic/path: {largest}"
    );
}
fn rejected(model: &AppManifest, limits: &ApplicationLimits) {
    // Ensure a different resource budget cannot make the negative test pass.
    limits.check_manifest(model).unwrap();
    let (result, largest) = measured(|| validate_with_limits(model, limits));
    let errors = result.unwrap_err();
    assert!(
        matches!(
            errors.last(),
            Some(ManifestError::Admission(AdmissionError::Exceeded {
                resource: "diagnostic bytes",
                ..
            }))
        ),
        "{errors:?}"
    );
    bounded(largest);
}

#[test]
fn long_cycle_names_are_borrowed_until_the_diagnostic_is_admitted() {
    let a = "a".repeat(512 * 1024);
    let b = "b".repeat(512 * 1024);
    let mut manifest = model();
    manifest.components = vec![component(&a), component(&b)];
    manifest.components[0].deps.push(b);
    manifest.components[1].deps.push(a);
    rejected(&manifest, &ApplicationLimits::default());
}

#[test]
fn invalid_hook_does_not_format_its_full_text_before_rejection() {
    let mut manifest = model();
    manifest.components[0].hooks.on_start = Some(format!("{}/..", "h".repeat(512 * 1024)));
    rejected(&manifest, &ApplicationLimits::default());
}

#[test]
fn valid_hook_validation_does_not_construct_a_large_resolved_path() {
    let mut manifest = model();
    manifest.components[0].hooks.on_start = Some("h".repeat(512 * 1024));
    let limits = ApplicationLimits {
        max_diagnostic_bytes: 0,
        ..Default::default()
    };
    limits.check_manifest(&manifest).unwrap();
    let (result, largest) = measured(|| validate_with_limits(&manifest, &limits));
    result.unwrap();
    bounded(largest);
}

#[test]
fn missing_wire_separator_does_not_format_the_full_endpoint() {
    let mut manifest = model();
    manifest.wiring.push(WireSpec {
        from: "x".repeat(512 * 1024),
        to: "worker:/user/worker".into(),
        qos: String::new(),
    });
    rejected(&manifest, &ApplicationLimits::default());
}

#[test]
fn malformed_target_wire_does_not_format_the_full_endpoint() {
    let mut manifest = model();
    manifest.wiring.push(WireSpec {
        from: "worker:/user/alias".into(),
        to: format!("worker:/user/{}/..", "x".repeat(512 * 1024)),
        qos: String::new(),
    });
    rejected(&manifest, &ApplicationLimits::default());
}

#[test]
fn unknown_wire_source_lookup_does_not_clone_a_large_component_name() {
    let mut manifest = model();
    manifest.wiring.push(WireSpec {
        from: format!("{}:/user/alias", "x".repeat(512 * 1024)),
        to: "worker:/user/worker".into(),
        qos: String::new(),
    });
    rejected(
        &manifest,
        &ApplicationLimits {
            max_diagnostic_bytes: 0,
            ..Default::default()
        },
    );
}

#[test]
fn unknown_wire_target_lookup_does_not_clone_a_large_component_name() {
    let mut manifest = model();
    manifest.wiring.push(WireSpec {
        from: "worker:/user/alias".into(),
        to: format!("{}:/user/target", "x".repeat(512 * 1024)),
        qos: String::new(),
    });
    rejected(
        &manifest,
        &ApplicationLimits {
            max_diagnostic_bytes: 0,
            ..Default::default()
        },
    );
}

#[test]
fn admitted_hook_and_wire_errors_preserve_legacy_text_and_exact_budget() {
    let header = std::mem::size_of::<ManifestError>() as u64;
    for (hook, text) in [
        (
            "/user/other",
            "hook \"/user/other\" is outside component worker",
        ),
        ("a/..", "invalid hook path \"a/..\" for component worker"),
        ("", "empty hook path/segment for component worker"),
        ("\n", "invalid hook path \"\\n\" for component worker"),
        (
            "路径/..",
            "invalid hook path \"路径/..\" for component worker",
        ),
    ] {
        let mut manifest = model();
        manifest.components[0].hooks.on_start = Some(hook.into());
        let exact = ApplicationLimits {
            max_diagnostic_bytes: header + text.len() as u64,
            ..Default::default()
        };
        assert_eq!(
            validate_with_limits(&manifest, &exact).unwrap_err(),
            vec![ManifestError::BadHook(text.into())]
        );
        rejected(
            &manifest,
            &ApplicationLimits {
                max_diagnostic_bytes: exact.max_diagnostic_bytes - 1,
                ..exact
            },
        );
    }
    for (endpoint, text) in [
        (
            "worker",
            "wire endpoint requires component:/path: \"worker\"",
        ),
        ("worker:/other", "invalid wire endpoint \"worker:/other\""),
        (
            "worker:/user/\n",
            "invalid wire endpoint \"worker:/user/\\n\"",
        ),
    ] {
        let mut manifest = model();
        manifest.wiring.push(WireSpec {
            from: endpoint.into(),
            to: "worker:/user/worker".into(),
            qos: String::new(),
        });
        let exact = ApplicationLimits {
            max_diagnostic_bytes: header + text.len() as u64,
            ..Default::default()
        };
        assert_eq!(
            validate_with_limits(&manifest, &exact).unwrap_err(),
            vec![ManifestError::BadWiring(text.into())]
        );
        rejected(
            &manifest,
            &ApplicationLimits {
                max_diagnostic_bytes: exact.max_diagnostic_bytes - 1,
                ..exact
            },
        );
    }
}

#[test]
fn admitted_cycle_keeps_replay_and_string_header_accounting() {
    let mut manifest = model();
    manifest.components = vec![component("a"), component("b")];
    manifest.components[0].deps.push("b".into());
    manifest.components[1].deps.push("a".into());
    let charge = std::mem::size_of::<ManifestError>() + 3 * (1 + std::mem::size_of::<String>());
    let exact = ApplicationLimits {
        max_diagnostic_bytes: charge as u64,
        ..Default::default()
    };
    assert_eq!(
        validate_with_limits(&manifest, &exact).unwrap_err(),
        vec![ManifestError::CycleDetected(vec![
            "a".into(),
            "b".into(),
            "a".into()
        ])]
    );
    rejected(
        &manifest,
        &ApplicationLimits {
            max_diagnostic_bytes: exact.max_diagnostic_bytes - 1,
            ..exact
        },
    );
}
