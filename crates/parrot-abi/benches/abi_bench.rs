//! D 阶段基准（DEV_09 §5.6：`cargo bench -p parrot-abi --features loader -- --gate d2`）。
//!
//! 门禁语义：**Dylib 增量（每调用）< 1µs**（TECH_DESIGN_09 §11.2.4
//! 三形态 ask 开销）。门禁断言在 tests/loader_tests.rs 的
//! `bench_gate_dylib_call_overhead_under_1us`（cargo test 可跑）；
//! 本文件为 criterion 报告曲线。

use criterion::{criterion_group, criterion_main, Criterion};
use parrot_abi::{AbiMsg, AbiReplyBuf, AbiStr, DylibLoader};

fn fixture() -> std::path::PathBuf {
    let ext = match std::env::consts::OS {
        "macos" => "dylib",
        "linux" => "so",
        _ => "dll",
    };
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/target/release/libparrot_abi_testcomp.{ext}"))
}

fn bench_dylib_call(c: &mut Criterion) {
    let h = DylibLoader::load(&fixture(), "").expect("load testcomp");
    let comp = DylibLoader::construct(&DylibLoader, &h, AbiStr::of_str("{}")).expect("construct");
    let mut group = c.benchmark_group("dylib_call");
    group.sample_size(100);
    group.bench_function("handle_msg_echo", |b| {
        b.iter(|| {
            let mut buf = [0u8; 4096];
            let mut rb = AbiReplyBuf::new(&mut buf);
            unsafe {
                DylibLoader::handle_msg(
                    &DylibLoader,
                    &h,
                    comp,
                    AbiMsg::new("echo", b"x"),
                    &mut rb,
                )
                .unwrap();
            }
        })
    });
    group.finish();
    // unload 不在 bench 内（destructive——句柄 bench 后进程退出自然回收）
    std::mem::forget(h);
}

criterion_group!(benches, bench_dylib_call);
criterion_main!(benches);
