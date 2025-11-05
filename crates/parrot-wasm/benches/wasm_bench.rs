//! Cached file-based instantiation benchmark: includes reading the artifact,
//! cache lookup, Store creation, linking and dropping the instance.
//! First compilation is warmed up outside the measured loop.
//!
//! 运行：`cargo bench -p parrot-wasm --features runtime`
//! Criterion reports a local baseline; no three-engine overhead gate is claimed.

use criterion::{criterion_group, criterion_main, Criterion};
use parrot_wasm::runtime::WasmRuntime;
use parrot_wasm::{HostCtx, WasmConfig};

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-component.wasm")
}

fn bench_instantiate(c: &mut Criterion) {
    let rt = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
    // 预热编译缓存（首编译——不计入门禁）
    let _ = rt.instantiate(&fixture_path(), HostCtx::default()).unwrap();

    let mut group = c.benchmark_group("wasm_instantiate");
    group.sample_size(50);
    group.bench_function("cached_component", |b| {
        b.iter(|| {
            let c = rt.instantiate(&fixture_path(), HostCtx::default()).unwrap();
            criterion::black_box(c);
        })
    });
    group.finish();
}

criterion_group!(benches, bench_instantiate);
criterion_main!(benches);
