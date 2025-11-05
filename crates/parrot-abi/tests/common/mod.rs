//! D 阶段测试共享辅助：fixture dylib 构建与定位。
//!
//! 测试首次运行时以 release 构建 tests/fixtures/（独立 workspace——
//! cdylib），产物路径经环境变量 `PARROT_ABI_FIXTURE_DIR` 可覆盖
//! （CI 预构建直供，跳过本地编译）。

#![cfg(feature = "loader")]

use std::path::PathBuf;
use std::sync::OnceLock;

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// 构建 fixtures（幂等——release 产物已存在则 cargo no-op）。
fn build_fixtures() -> PathBuf {
    let root = fixtures_root();
    let ext = dylib_ext();
    if let Some(directory) = std::env::var_os("PARROT_ABI_FIXTURE_DIR") {
        let directory = PathBuf::from(directory);
        for name in ["testcomp", "violating"] {
            assert!(
                directory
                    .join(format!("libparrot_abi_{name}.{ext}"))
                    .is_file(),
                "missing prebuilt fixture {name}"
            );
        }
        return directory;
    }
    // Always let Cargo check source/dependency freshness. Existing binaries do
    // not prove they were built from the current fixture and ABI source.
    let out = std::process::Command::new("cargo")
        .args(["build", "--locked", "--release", "--target-dir"])
        .arg(root.join("target"))
        .current_dir(&root)
        .output()
        .expect("spawn cargo for fixture build");
    assert!(
        out.status.success(),
        "fixture build failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    root.join("target/release")
}

static ROOT: OnceLock<PathBuf> = OnceLock::new();

fn root() -> &'static PathBuf {
    ROOT.get_or_init(build_fixtures)
}

/// 全局加载锁：macOS dyld 按路径引用计数——dlclose 静态重置断言
/// 需要进程内同库句柄互斥（其它测试的 handle 会保引用）。
/// 所有 load/unload 测试经 `load_mutex()` 串行化。
pub fn load_mutex() -> std::sync::MutexGuard<'static, ()> {
    static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
    M.lock().unwrap_or_else(|p| p.into_inner())
}

/// 规范 fixture 路径。
pub fn testcomp() -> std::path::PathBuf {
    let ext = dylib_ext();
    root().join(format!("libparrot_abi_testcomp.{ext}"))
}

/// 违规 fixture 路径。（loader_tests 消费——lifecycle 场景不用）
#[allow(dead_code)]
pub fn violating() -> std::path::PathBuf {
    let ext = dylib_ext();
    root().join(format!("libparrot_abi_violating.{ext}"))
}

/// testcomp 的 sha256（digest 校验测试）。
#[allow(dead_code)]
pub fn testcomp_sha256() -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(std::fs::read(testcomp()).unwrap()))
}

/// 平台动态库扩展（std::env::consts 无稳定 DYLIB_EXTENSION——手写映射）。
fn dylib_ext() -> &'static str {
    match std::env::consts::OS {
        "macos" => "dylib",
        "linux" => "so",
        "windows" => "dll",
        other => panic!("unsupported fixture platform: {other}"),
    }
}
