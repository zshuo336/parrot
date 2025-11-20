#![cfg(all(feature = "host", feature = "dylib"))]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
struct ObservedAllocator;
static OBSERVING: AtomicBool = AtomicBool::new(false);
static LARGEST: AtomicUsize = AtomicUsize::new(0);
fn observe(size: usize) {
    if OBSERVING.load(Ordering::SeqCst) {
        LARGEST.fetch_max(size, Ordering::SeqCst);
    }
}
// Forward every operation unchanged. Only allocation sizes are observed; no
// refusal, recursion, memory accounting headers, or allocator replacement policy.
unsafe impl GlobalAlloc for ObservedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        observe(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        observe(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        observe(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: ObservedAllocator = ObservedAllocator;

#[tokio::test]
async fn native_constructor_rejects_expansion_without_allocating_the_full_document() {
    use parrot_app::{
        assemble::{DeployError, ParrotSpawner},
        host::{dylib::MAX_CONFIG_BYTES, ThreadPropsSpawner},
        ArtifactRef,
    };
    let engine = parrot::thread::system::ThreadActorSystem::shared(Default::default());
    let digest = "0".repeat(64);
    // SAFETY: the path is deliberately absent. No native code can be loaded;
    // the constructor configuration must fail before artifact preparation.
    let spawner =
        unsafe { ThreadPropsSpawner::new(engine.clone()).with_trusted_dylib(&digest) }.unwrap();
    let config = toml::Table::from_iter([(
        "p".repeat(8192),
        toml::Value::Table(
            (0..1000)
                .map(|i| (format!("child{i}"), toml::Value::Table(toml::Table::new())))
                .collect(),
        ),
    )]);
    let artifact = ArtifactRef::Dylib {
        digest,
        uri: "file:///parrot-deliberately-missing-native-config.so".into(),
        abi: 1,
    };
    let paths = vec!["/user/native-config-bound".into()];
    LARGEST.store(0, Ordering::SeqCst);
    OBSERVING.store(true, Ordering::SeqCst);
    let result = spawner
        .spawn_artifact(&artifact, &paths, Some(&config))
        .await;
    OBSERVING.store(false, Ordering::SeqCst);
    let largest = LARGEST.load(Ordering::SeqCst);
    engine.shutdown_internal().await.unwrap();
    assert!(
        matches!(result, Err(DeployError::Config(_))),
        "configuration must reject before native loading"
    );
    println!(
        "largest allocation request: {largest} bytes; constructor output limit: {MAX_CONFIG_BYTES}"
    );
    assert!(
        largest <= 4 * MAX_CONFIG_BYTES,
        "a complete expanded document was allocated before rejection: {largest}"
    );
}
