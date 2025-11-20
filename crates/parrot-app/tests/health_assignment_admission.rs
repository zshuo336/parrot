use parrot_app::{orchestrator::HealthWatch, ApplicationLimits};
use parrot_remote::admin_v2::ComponentStateReport;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    collections::BTreeMap,
};

thread_local! {
    static OBSERVING: Cell<bool> = const { Cell::new(false) };
    static REQUESTED: Cell<usize> = const { Cell::new(0) };
}
struct Meter;
fn record(size: usize) {
    if OBSERVING.try_with(Cell::get).unwrap_or(false) {
        let _ = REQUESTED.try_with(|value| value.set(value.get().saturating_add(size)));
    }
}
// SAFETY: all pointer/layout operations are forwarded unchanged to System.
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
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(pointer, layout, size) }
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
    REQUESTED.set(0);
    OBSERVING.set(true);
    let reset = Reset;
    let result = run();
    drop(reset);
    (result, REQUESTED.get())
}
fn budget(result: Result<(), String>, resource: &str) {
    let error = result.expect_err("over-budget assignment was accepted");
    assert!(
        error.contains(resource) && error.contains("budget exceeded"),
        "unexpected error: {error}"
    );
}
fn bounded_requests(requested: usize) {
    println!("observed cumulative allocation requests: {requested} bytes");
    assert!(
        requested < 256 * 1024,
        "rejection copied assignment payloads before admission: {requested} bytes"
    );
}
fn names(prefix: &str, count: usize) -> Vec<String> {
    (0..count).map(|index| format!("{prefix}{index}")).collect()
}
fn rows(component: &str) -> Vec<ComponentStateReport> {
    vec![ComponentStateReport {
        path: format!("/user/{component}"),
        state: "running".into(),
        version: "1.0.0".into(),
    }]
}
fn accept(watch: &mut HealthWatch, node: &str, component: &str) {
    let poll = watch.begin_poll(node, component).unwrap();
    watch.apply_poll(poll, rows(component)).unwrap();
}

#[test]
fn oversized_assignment_projection_rejects_before_node_fanout() {
    let node = "n".repeat(64 * 1024);
    let limits = ApplicationLimits {
        max_metadata_bytes: 1024 * 1024,
        ..Default::default()
    };
    let mut watch = HealthWatch::with_limits(vec![node.clone()], limits).unwrap();
    let components = names("component", 128);
    let (result, requested) = measured(|| watch.register_node_components(&node, components));
    budget(result, "metadata bytes");
    assert!(watch.begin_poll(&node, "component0").is_err());
    assert!(watch.observed().components.is_empty());
    bounded_requests(requested);
}

#[test]
fn component_limit_rejection_does_not_clone_existing_assignments() {
    let node = "n".repeat(16 * 1024);
    let limits = ApplicationLimits {
        max_components: 32,
        max_metadata_bytes: 1024 * 1024,
        ..Default::default()
    };
    let mut watch = HealthWatch::with_limits(vec![node.clone(), "other".into()], limits).unwrap();
    watch
        .register_node_components(&node, names("a", 32))
        .unwrap();
    accept(&mut watch, &node, "a0");
    let poll = watch.begin_poll(&node, "a0").unwrap();
    let components = vec!["extra".into()];
    let (result, requested) = measured(|| watch.register_node_components("other", components));
    budget(result, "health assignments");
    assert_eq!(watch.observed().component_nodes["a0"], node);
    watch.apply_poll(poll, rows("a0")).unwrap();
    assert!(watch.begin_poll("other", "extra").is_err());
    bounded_requests(requested);
}

#[test]
fn rejected_byte_growth_does_not_clone_retained_assignments() {
    let old = "n".repeat(16 * 1024);
    let new = "m".repeat(8 * 1024);
    let limits = ApplicationLimits {
        max_metadata_bytes: 600 * 1024,
        ..Default::default()
    };
    let mut watch = HealthWatch::with_limits(vec![old.clone(), new.clone()], limits).unwrap();
    watch
        .register_node_components(&old, names("a", 32))
        .unwrap();
    accept(&mut watch, &old, "a0");
    let poll = watch.begin_poll(&old, "a0").unwrap();
    let components = names("b", 32);
    let (result, requested) = measured(|| watch.register_node_components(&new, components));
    budget(result, "metadata bytes");
    assert_eq!(watch.observed().component_nodes["a0"], old);
    watch.apply_poll(poll, rows("a0")).unwrap();
    assert!(watch.begin_poll(&new, "b0").is_err());
    bounded_requests(requested);
}

#[test]
fn assignment_metadata_matches_json_map_at_exact_boundary() {
    let node = "node\\\"中文";
    let expected = BTreeMap::from([("a", node), ("b", node)]);
    let bytes = serde_json::to_vec(&expected).unwrap().len() as u64;
    let limits = ApplicationLimits {
        max_metadata_bytes: bytes,
        ..Default::default()
    };
    let mut watch = HealthWatch::with_limits(vec![node.into()], limits).unwrap();
    watch
        .register_node_components(node, vec!["a".into(), "b".into()])
        .unwrap();
    watch.begin_poll(node, "a").unwrap();
    watch.begin_poll(node, "b").unwrap();
    let mut short = HealthWatch::with_limits(
        vec![node.into()],
        ApplicationLimits {
            max_metadata_bytes: bytes - 1,
            ..limits
        },
    )
    .unwrap();
    budget(
        short.register_node_components(node, vec!["a".into(), "b".into()]),
        "metadata bytes",
    );
    assert!(short.begin_poll(node, "a").is_err());
    short
        .register_node_components(node, vec!["a".into()])
        .unwrap();
    short.begin_poll(node, "a").unwrap();
}

#[test]
fn rejected_reassignment_preserves_local_receipt_and_network_ticket() {
    let long = "m".repeat(256);
    let limits = ApplicationLimits {
        max_metadata_bytes: 512,
        ..Default::default()
    };
    let mut watch = HealthWatch::with_limits(vec!["n".into(), long.clone()], limits).unwrap();
    watch
        .register_node_components("n", vec!["a".into()])
        .unwrap();
    accept(&mut watch, "n", "a");
    watch.apply_status("local", rows("local")).unwrap();
    let poll = watch.begin_poll("n", "a").unwrap();
    budget(
        watch.register_node_components(
            &long,
            vec!["a".into(), "local".into(), "x".into(), "y".into()],
        ),
        "metadata bytes",
    );
    let observed = watch.observed();
    assert!(observed.any_running("a"));
    assert!(observed.any_running("local"));
    assert_eq!(observed.component_nodes["a"], "n");
    assert_eq!(observed.components.len(), 2);
    assert!(watch.begin_poll(&long, "a").is_err());
    assert!(watch.begin_poll(&long, "local").is_err());
    watch.apply_status("local", rows("local")).unwrap();
    watch.apply_poll(poll, rows("a")).unwrap();
}

#[test]
fn successful_replacement_invalidates_only_changed_components() {
    let mut watch = HealthWatch::new(vec!["n".into(), "m".into()]).unwrap();
    watch
        .register_node_components("n", vec!["a".into(), "b".into()])
        .unwrap();
    watch
        .register_node_components("m", vec!["c".into()])
        .unwrap();
    for (node, component) in [("n", "a"), ("n", "b"), ("m", "c")] {
        accept(&mut watch, node, component);
    }
    let a = watch.begin_poll("n", "a").unwrap();
    let b = watch.begin_poll("n", "b").unwrap();
    let c = watch.begin_poll("m", "c").unwrap();
    watch
        .register_node_components("n", vec!["b".into(), "c".into()])
        .unwrap();
    let observed = watch.observed();
    assert!(!observed.any_running("a"));
    assert!(observed.any_running("b"));
    assert!(!observed.any_running("c"));
    assert!(watch.apply_poll(a, rows("a")).is_err());
    assert!(watch.apply_poll(c, rows("c")).is_err());
    watch.apply_poll(b, rows("b")).unwrap();
    assert!(watch.begin_poll("m", "c").is_err());
    let c = watch.begin_poll("n", "c").unwrap();
    watch.register_node_components("m", vec![]).unwrap();
    watch.apply_poll(c, rows("c")).unwrap();
    watch.apply_status("a", rows("a")).unwrap();
}

#[test]
fn replacement_budget_counts_final_map_and_preserves_unrelated_assignments() {
    let expected = BTreeMap::from([("a", "node-b"), ("b", "node-b"), ("keep", "node-c")]);
    let limits = ApplicationLimits {
        max_components: 3,
        max_metadata_bytes: serde_json::to_vec(&expected).unwrap().len() as u64,
        ..Default::default()
    };
    let mut watch = HealthWatch::with_limits(
        vec!["node-a".into(), "node-b".into(), "node-c".into()],
        limits,
    )
    .unwrap();
    watch
        .register_node_components("node-a", vec!["a".into(), "b".into()])
        .unwrap();
    watch
        .register_node_components("node-c", vec!["keep".into()])
        .unwrap();
    let keep = watch.begin_poll("node-c", "keep").unwrap();
    watch
        .register_node_components("node-b", vec!["a".into(), "b".into()])
        .unwrap();
    watch.begin_poll("node-b", "a").unwrap();
    watch.begin_poll("node-b", "b").unwrap();
    assert!(watch.begin_poll("node-a", "a").is_err());
    assert!(watch.begin_poll("node-a", "b").is_err());
    // Empty reports are still an accepted reply and consume this exact ticket.
    watch.apply_poll(keep.clone(), vec![]).unwrap();
    assert!(watch.apply_poll(keep, vec![]).is_err());
}
