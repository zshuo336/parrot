//! C 阶段基准门禁测试（DEV_09 §3.3 / TECH_DESIGN_09 §11.2.4）。
//!
//! 门禁语义：**三形态 ask 开销**——Wasm 增量（每消息）< 10µs。
//! （"增量" = 相对 Props 原生基线的每消息附加开销；实例化一次性
//! 成本不在此门禁——部署路径由 deploy 回执 P99 <1s 覆盖。）
//!
//! 与 benches/wasm_bench.rs 分离：cargo test 需独立执行 #[test] 门禁
//! （criterion harness 只跑 bench 循环不跑测试）。

#![cfg(feature = "runtime")]

use parrot_wasm::runtime::WasmRuntime;
use parrot_wasm::{HostCtx, WasmConfig};
use std::time::Instant;

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-component.wasm")
}

/// 门禁：Wasm 每消息 ask 开销 p50 < 10µs（TECH_DESIGN_09 §11.2.4）。
#[test]
fn bench_gate_wasm_ask_overhead_under_10us() {
    let rt = WasmRuntime::new(WasmConfig::default()).unwrap();
    let mut c = rt.instantiate(&fixture_path(), HostCtx::default()).unwrap();
    // 预热（JIT/类型检查缓存）
    for _ in 0..100 {
        let _ = c.handle("echo", b"warmup").unwrap();
    }
    // 采样 1000 次取 p50/p99
    let mut samples = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let t0 = Instant::now();
        let out = c.handle("echo", b"x").unwrap();
        samples.push(t0.elapsed());
        assert_eq!(out, b"x");
    }
    samples.sort();
    let p50 = samples[samples.len() / 2];
    let p99 = samples[samples.len() * 99 / 100];
    assert!(
        p50.as_micros() < 10,
        "wasm ask overhead p50 = {:?} exceeds 10µs gate",
        p50
    );
    eprintln!("wasm ask overhead: p50={p50:?} p99={p99:?}");
}

/// 热路径分解报告（up 计算型 + config 宿主回调型——记录用）。
#[test]
fn bench_message_variants_report() {
    let rt = WasmRuntime::new(WasmConfig::default()).unwrap();
    let mut ctx = HostCtx::default();
    ctx.config_overlay.insert("k", "v");
    let mut c = rt.instantiate(&fixture_path(), ctx).unwrap();
    for (name, key, payload) in [
        ("up", "up", 41u64.to_le_bytes().to_vec()),
        ("config", "config", b"k".to_vec()),
        ("self", "self", vec![]),
    ] {
        let mut total = std::time::Duration::ZERO;
        let n = 500;
        for _ in 0..n {
            let t0 = Instant::now();
            let _ = c.handle(key, &payload).unwrap();
            total += t0.elapsed();
        }
        eprintln!("{name} avg: {:?}", total / n);
    }
}
