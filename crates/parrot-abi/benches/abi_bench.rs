//! D 阶段基准（DEV_09 §5.6：`cargo bench -p parrot-abi --features loader -- --gate d2`）。
//!
//! Reports total host dispatch + FFI + guest echo latency. No local actor
//! baseline is measured here, so this is not incremental overhead relative to
//! another engine. See the explicit ignored release reports in loader_tests.rs.

use criterion::{criterion_group, criterion_main, Criterion};
use parrot_abi::{AbiMsg, AbiReplyBuf, DylibLoader};

fn fixture() -> std::path::PathBuf {
    let ext = match std::env::consts::OS {
        "macos" => "dylib",
        "linux" => "so",
        _ => "dll",
    };
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/fixtures/target/release/libparrot_abi_testcomp.{ext}"
    ))
}

fn bench_dylib_call(c: &mut Criterion) {
    let h = unsafe { DylibLoader::load(&fixture(), "") }.expect("load testcomp");
    let comp = DylibLoader::construct(&DylibLoader, &h, "{}").expect("construct");
    let mut group = c.benchmark_group("dylib_call");
    group.sample_size(100);
    group.bench_function("handle_msg_echo", |b| {
        b.iter(|| {
            let mut buf = [0u8; 4096];
            let mut rb = AbiReplyBuf::new(&mut buf);
            unsafe {
                DylibLoader::handle_msg(&DylibLoader, &h, comp, AbiMsg::new("echo", b"x"), &mut rb)
                    .unwrap();
            }
        })
    });
    group.finish();
    // Cleanup is outside the timed region.
    drop(h);
}

criterion_group!(benches, bench_dylib_call);
criterion_main!(benches);
