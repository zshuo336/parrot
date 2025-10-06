//! C 阶段基准（DEV_09 §3.3 门禁：<10µs 增量实例化）。
//!
//! 计量的"增量实例化" = 同 Engine + 预编译 Component 缓存命中后的
//! `Store::new + linker instantiate`（首编译 ~ms 级不计入门禁——
//! 真实部署每节点一次）。
//!
//! 运行：`cargo bench -p parrot-wasm --features runtime`
//! 门禁断言：p50 < 10µs（同 digest instantiate——缓存命中路径）。

use criterion::{criterion_group, criterion_main, Criterion};
use parrot_wasm::runtime::WasmRuntime;
use parrot_wasm::{HostCtx, WasmConfig};

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-component.wasm")
}

fn bench_instantiate(c: &mut Criterion) {
    let rt = WasmRuntime::new(WasmConfig::default()).unwrap();
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
