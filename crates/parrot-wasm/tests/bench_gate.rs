//! Explicit local microbenchmarks, not CI correctness or three-engine parity gates.
//! Run: cargo test --release -p parrot-wasm --test bench_gate -- --ignored --nocapture
//! Measures direct Wasm calls without actor dispatch or a Props baseline.
//! Manual epochs isolate call cost; production interruption has separate tests.

#![cfg(feature = "runtime")]

use parrot_wasm::runtime::WasmRuntime;
use parrot_wasm::{HostCtx, WasmConfig};
use std::time::Instant;

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fixture-component.wasm")
}

#[test]
#[ignore = "explicit performance measurement; use --release on controlled hardware"]
fn direct_wasm_echo_latency_report() {
    eprintln!("debug_assertions={}", cfg!(debug_assertions));
    let rt = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
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
    eprintln!("direct Wasm echo latency (1000 samples): p50={p50:?} p99={p99:?}");
}

/// 热路径分解报告（up 计算型 + config 宿主回调型——记录用）。
#[test]
#[ignore = "explicit performance measurement; use --release on controlled hardware"]
fn bench_message_variants_report() {
    eprintln!("debug_assertions={}", cfg!(debug_assertions));
    let rt = WasmRuntime::new_with_manual_epoch(WasmConfig::default()).unwrap();
    let mut ctx = HostCtx::default();
    ctx.config_overlay.insert("k", "v");
    let mut c = rt.instantiate(&fixture_path(), ctx).unwrap();
    for (name, key, payload, expected) in [
        (
            "up",
            "up",
            41u64.to_le_bytes().to_vec(),
            42u64.to_le_bytes().to_vec(),
        ),
        ("config", "config", b"k".to_vec(), b"v".to_vec()),
        ("self", "self", vec![], vec![]),
    ] {
        let mut total = std::time::Duration::ZERO;
        let n = 500;
        for _ in 0..n {
            let t0 = Instant::now();
            let output = c.handle(key, &payload).unwrap();
            total += t0.elapsed();
            assert_eq!(output, expected);
        }
        eprintln!("{name} avg: {:?}", total / n);
    }
}
