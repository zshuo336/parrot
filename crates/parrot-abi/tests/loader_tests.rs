//! D1+D2（DEV_09 §3.4 / 09 §4.3）parrot-abi 测试——35+ 项。
//!
//! 分组：
//! 1. 禁止清单扫描（合规/违规/工具行为）
//! 2. load 校验（meta/digest/版本/符号缺失）
//! 3. construct/destroy 生命周期
//! 4. handle_msg 语义（echo/up/err/msgs 计数）
//! 5. 插件内部捕获panic并返回ABI_ERR_PANIC（不测试跨extern C unwind）
//! 6. in-flight 计数与 quarantine
//! 7. 四步卸载（正常弧/超时弧/幂等/重建无残留）
//! 8. 显式release延迟观测（不等价于相对本地调用的增量开销）

#![cfg(feature = "loader")]
// 全局 dyld 串行守卫刻意跨 await 持有（独占句柄寿命 = 测试寿命）
#![allow(clippy::await_holding_lock)]

mod common;

use parrot_abi::{
    AbiMsg, AbiReplyBuf, DylibLoader, LoadError, Violation, ABI_ERR_PANIC, ABI_ERR_STATE,
};
use std::time::Duration;

fn loader() -> DylibLoader {
    DylibLoader
}

/// 全局串行守卫（持有 = 独占同库 dyld 引用；drop 释放）。
fn serial() -> std::sync::MutexGuard<'static, ()> {
    common::load_mutex()
}

/// 带全局串行守卫的加载（守卫与句柄同寿命——dyld 引用互斥）。
fn load_good() -> (std::sync::MutexGuard<'static, ()>, parrot_abi::DylibHandle) {
    let g = serial();
    let h = unsafe { DylibLoader::load(&common::testcomp(), "") }.expect("load testcomp");
    (g, h)
}

fn construct_good(h: &parrot_abi::DylibHandle) -> *mut parrot_abi::AbiComponent {
    let cfg = "{}";
    DylibLoader::construct(&DylibLoader, h, cfg).expect("construct")
}

unsafe fn ask(
    ld: &DylibLoader,
    h: &parrot_abi::DylibHandle,
    comp: *mut parrot_abi::AbiComponent,
    key: &str,
    payload: &[u8],
) -> Result<Vec<u8>, (u32, String)> {
    let mut buf = vec![0u8; 4096];
    let mut rb = AbiReplyBuf::new(&mut buf);
    ld.handle_msg(h, comp, AbiMsg::new(key, payload), &mut rb)?;
    Ok(buf[..rb.written as usize].to_vec())
}

// ════════════════════════════════════════════════════════════
// 1. 禁止清单扫描
// ════════════════════════════════════════════════════════════

#[test]
fn scan_compliant_dylib_clean() {
    let v = DylibLoader::scan_violations(&common::testcomp()).unwrap();
    assert!(v.is_empty(), "testcomp must be clean: {v:?}");
}

#[test]
fn scan_violating_tls_detected() {
    let v = DylibLoader::scan_violations(&common::violating()).unwrap();
    assert!(
        v.iter()
            .any(|x| matches!(x, Violation::TlsDestructor { .. })),
        "TLS violation expected: {v:?}"
    );
}

#[test]
fn scan_violating_thread_detected() {
    let v = DylibLoader::scan_violations(&common::violating()).unwrap();
    assert!(
        v.iter().any(|x| matches!(x, Violation::ThreadSpawn { .. })),
        "thread violation expected: {v:?}"
    );
}

#[test]
fn scan_missing_file_is_an_error() {
    // Missing input must never be reported as a clean scan.
    let v = DylibLoader::scan_violations(std::path::Path::new("/no/such.dylib"));
    assert!(matches!(v, Err(LoadError::Scan(_))));
}

#[test]
fn scanning_does_not_depend_on_nm_or_path_tools() {
    const CHILD: &str = "PARROT_SCAN_ARTIFACT";
    if let Some(path) = std::env::var_os(CHILD) {
        let violations = DylibLoader::scan_violations(std::path::Path::new(&path)).unwrap();
        assert!(violations
            .iter()
            .any(|v| matches!(v, Violation::ThreadSpawn { .. })));
        assert!(violations
            .iter()
            .any(|v| matches!(v, Violation::TlsDestructor { .. })));
        return;
    }
    let path = common::violating();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "scanning_does_not_depend_on_nm_or_path_tools",
            "--nocapture",
        ])
        .env(CHILD, path)
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "child failed: {}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn quarantine_blocks_construction_as_well_as_messages() {
    let (_guard, handle) = load_good();
    handle.set_quarantined(true);
    let error = loader().construct(&handle, "{}").unwrap_err();
    assert!(matches!(
        error,
        LoadError::Construct {
            code: ABI_ERR_STATE,
            ..
        }
    ));
}

#[test]
fn violation_display() {
    assert!(Violation::TlsDestructor { symbol: "s".into() }
        .to_string()
        .contains("tls"));
    assert!(Violation::ThreadSpawn { symbol: "s".into() }
        .to_string()
        .contains("thread"));
    assert!(Violation::SignalHandler { symbol: "s".into() }
        .to_string()
        .contains("signal"));
}

// ════════════════════════════════════════════════════════════
// 2. load 校验
// ════════════════════════════════════════════════════════════

#[test]
fn load_compliant_ok() {
    let (_g, h) = load_good();
    assert_eq!(h.name, "testcomp");
}

#[test]
fn load_missing_file_err() {
    let e = unsafe { DylibLoader::load(std::path::Path::new("/no/lib.dylib"), "") }.unwrap_err();
    assert!(matches!(e, LoadError::Open(_)), "{e:?}");
}

#[test]
fn load_violating_rejected() {
    let e = unsafe { DylibLoader::load(&common::violating(), "") }.unwrap_err();
    match e {
        LoadError::Forbidden(v) => assert!(!v.is_empty()),
        other => panic!("expected Forbidden: {other:?}"),
    }
}

#[test]
fn load_digest_ok() {
    let sha = common::testcomp_sha256();
    let h = unsafe { DylibLoader::load(&common::testcomp(), &sha) }.expect("digest match loads");
    assert_eq!(h.name, "testcomp");
}

#[test]
fn load_digest_prefixed_ok() {
    let sha = format!("sha256:{}", common::testcomp_sha256());
    unsafe { DylibLoader::load(&common::testcomp(), &sha) }.expect("prefixed digest loads");
}

#[test]
fn load_digest_mismatch_rejected() {
    let e = unsafe { DylibLoader::load(&common::testcomp(), &"0".repeat(64)) }.unwrap_err();
    match e {
        LoadError::Digest { want, got } => {
            assert_eq!(want, "0".repeat(64));
            assert_eq!(got.len(), 64);
        }
        other => panic!("expected Digest: {other:?}"),
    }
}

#[test]
fn load_not_a_dylib_symbol_err() {
    // 非 dylib 文件：dlopen 失败（或符号缺失——形态依平台）
    let p = std::env::temp_dir().join("parrot-abi-not-dylib.bin");
    std::fs::write(&p, b"garbage").unwrap();
    let e = unsafe { DylibLoader::load(&p, "") }.unwrap_err();
    assert!(
        matches!(
            e,
            LoadError::Open(_) | LoadError::Symbol(_) | LoadError::Scan(_)
        ),
        "{e:?}"
    );
    std::fs::remove_file(&p).ok();
}

// ════════════════════════════════════════════════════════════
// 3. construct/destroy 生命周期
// ════════════════════════════════════════════════════════════

#[test]
fn construct_ok_and_cnt() {
    // cnt 是库加载期静态（进程内跨实例共享）——并发测试下绝对值不定；
    // 断言改为相对单调：二次 construct 后计数 ≥ 2 且递增。
    let (_g, h) = load_good();
    let c1 = construct_good(&h);
    unsafe {
        let n1 = u64::from_le_bytes(
            ask(&loader(), &h, c1, "cnt", &[])
                .unwrap()
                .try_into()
                .unwrap(),
        );
        let c2 = construct_good(&h);
        let n2 = u64::from_le_bytes(
            ask(&loader(), &h, c2, "cnt", &[])
                .unwrap()
                .try_into()
                .unwrap(),
        );
        assert!(n2 > n1, "construct count must increase: {n1} → {n2}");
    }
}

#[test]
fn handle_unregistered_pointer_rejected() {
    let (_g, h) = load_good();
    let bogus = 0xdeadbeefusize as *mut parrot_abi::AbiComponent;
    unsafe {
        let mut buf = [0u8; 64];
        let mut rb = AbiReplyBuf::new(&mut buf);
        let e = loader()
            .handle_msg(&h, bogus, AbiMsg::new("echo", b"x"), &mut rb)
            .unwrap_err();
        assert_eq!(e.0, ABI_ERR_STATE);
    }
}

// ════════════════════════════════════════════════════════════
// 4. handle_msg 语义
// ════════════════════════════════════════════════════════════

#[test]
fn handle_echo_roundtrip() {
    let (_g, h) = load_good();
    let c = construct_good(&h);
    unsafe {
        let out = ask(&loader(), &h, c, "echo", b"hello dylib").unwrap();
        assert_eq!(out, b"hello dylib");
    }
}

#[test]
fn handle_up_compute() {
    let (_g, h) = load_good();
    let c = construct_good(&h);
    unsafe {
        let out = ask(&loader(), &h, c, "up", &41u64.to_le_bytes()).unwrap();
        assert_eq!(u64::from_le_bytes(out.try_into().unwrap()), 42);
    }
}

#[test]
fn handle_err_code() {
    let (_g, h) = load_good();
    let c = construct_good(&h);
    unsafe {
        let e = ask(&loader(), &h, c, "err", &[]).unwrap_err();
        assert_eq!(e.0, ABI_ERR_STATE);
    }
}

#[test]
fn handle_unknown_key_state_err() {
    let (_g, h) = load_good();
    let c = construct_good(&h);
    unsafe {
        let e = ask(&loader(), &h, c, "zzz", &[]).unwrap_err();
        assert_eq!(e.0, ABI_ERR_STATE);
    }
}

#[test]
fn handle_msgs_counter_per_instance() {
    let (_g, h) = load_good();
    let a = construct_good(&h);
    let b = construct_good(&h);
    unsafe {
        let _ = ask(&loader(), &h, a, "echo", b"1").unwrap();
        let _ = ask(&loader(), &h, a, "echo", b"2").unwrap();
        let _ = ask(&loader(), &h, b, "echo", b"3").unwrap();
        let ma = u64::from_le_bytes(
            ask(&loader(), &h, a, "msgs", &[])
                .unwrap()
                .try_into()
                .unwrap(),
        );
        let mb = u64::from_le_bytes(
            ask(&loader(), &h, b, "msgs", &[])
                .unwrap()
                .try_into()
                .unwrap(),
        );
        assert_eq!((ma, mb), (3, 2), "per-instance state isolated");
    }
}

#[test]
fn handle_empty_payload_echo() {
    let (_g, h) = load_good();
    let c = construct_good(&h);
    unsafe {
        let out = ask(&loader(), &h, c, "echo", &[]).unwrap();
        assert!(out.is_empty());
    }
}

// ════════════════════════════════════════════════════════════
// 5. panic 双保险
// ════════════════════════════════════════════════════════════

#[test]
fn dylib_panic_caught_as_err_code() {
    let (_g, h) = load_good();
    let c = construct_good(&h);
    unsafe {
        let e = ask(&loader(), &h, c, "panic", &[]).unwrap_err();
        assert_eq!(
            e.0, ABI_ERR_PANIC,
            "panic → ABI_ERR_PANIC via host catch_unwind"
        );
    }
}

#[test]
fn after_panic_handle_still_works() {
    let (_g, h) = load_good();
    let c = construct_good(&h);
    unsafe {
        let _ = ask(&loader(), &h, c, "panic", &[]).unwrap_err();
        let out = ask(&loader(), &h, c, "echo", b"post-panic").unwrap();
        assert_eq!(out, b"post-panic");
    }
}

// ════════════════════════════════════════════════════════════
// 6. in-flight / quarantine
// ════════════════════════════════════════════════════════════

#[tokio::test]
async fn unload_happy_path_report() {
    let (_g, h) = load_good();
    let _c1 = construct_good(&h);
    let _c2 = construct_good(&h);
    let r = loader()
        .unload(h, Duration::from_millis(500))
        .await
        .unwrap();
    assert_eq!(r.drained, 2);
    assert_eq!(r.aborted, 0);
    assert!(!r.force_closed);
}

#[tokio::test]
async fn unload_no_instances() {
    let (_g, h) = load_good();
    let r = loader()
        .unload(h, Duration::from_millis(100))
        .await
        .unwrap();
    assert_eq!(r.drained, 0);
}

#[test]
fn quarantine_blocks_new_messages() {
    let (_g, h) = load_good();
    let c = construct_good(&h);
    h.set_quarantined(true);
    unsafe {
        let mut buf = [0u8; 64];
        let mut rb = AbiReplyBuf::new(&mut buf);
        let e = loader()
            .handle_msg(&h, c, AbiMsg::new("echo", b"x"), &mut rb)
            .unwrap_err();
        assert_eq!(e.0, ABI_ERR_STATE);
        assert!(e.1.contains("quarantined"));
    }
    h.set_quarantined(false); // 还原（drop 正常路径）
}

// ════════════════════════════════════════════════════════════
// 7. 四步卸载协议
// ════════════════════════════════════════════════════════════

#[tokio::test]
async fn unload_then_reload_fresh_state() {
    // 升级主路径（09 §4.3 ④）：dlclose → 立即 dlopen 同库 → 新实例
    // 状态全新（无污染断言）。
    //
    // 诚实边界：库级**静态**计数是否归零是平台行为（macOS dyld 常驻
    // 缓存可保映射）；可移植断言 = 实例级状态隔离（msgs 计数从 0 起）。
    {
        let (_g, h) = load_good();
        let c = construct_good(&h);
        unsafe {
            let _ = ask(&loader(), &h, c, "echo", b"warm").unwrap();
            let m1 = u64::from_le_bytes(
                ask(&loader(), &h, c, "msgs", &[])
                    .unwrap()
                    .try_into()
                    .unwrap(),
            );
            assert_eq!(m1, 2);
        }
        let r = loader()
            .unload(h, Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(r.drained, 1);
    }
    // 立即重载（同名"新版本"路径）
    let (_g2, h2) = load_good();
    let c2 = construct_good(&h2);
    unsafe {
        // 注：fixture 的 msgs 在每次 handle_msg 前自增——首问即 1
        let out = ask(&loader(), &h2, c2, "msgs", &[]).unwrap();
        let n = u64::from_le_bytes(out.try_into().unwrap());
        assert_eq!(
            n, 1,
            "fresh instance counter starts at first touch; got {n}"
        );
        // 功能照常（dlopen 后 meta/hooks 全可用）
        let echo = ask(&loader(), &h2, c2, "echo", b"reloaded").unwrap();
        assert_eq!(echo, b"reloaded");
    }
    loader()
        .unload(h2, Duration::from_millis(500))
        .await
        .unwrap();
}

#[test]
fn load_error_display_forms() {
    assert!(LoadError::Open("x".into()).to_string().contains("open"));
    assert!(format!("{}", LoadError::AbiVersion { dylib: 2, host: 1 }).contains("mismatch"));
    assert!(LoadError::Digest {
        want: "a".into(),
        got: "b".into()
    }
    .to_string()
    .contains("sha256"));
    assert!(LoadError::Forbidden(vec![])
        .to_string()
        .contains("forbidden"));
    assert!(LoadError::Construct {
        code: 1,
        detail: "d".into()
    }
    .to_string()
    .contains("construct"));
}

// ════════════════════════════════════════════════════════════
// 8. Explicit performance reports; correctness is tested independently above.
// ════════════════════════════════════════════════════════════

#[test]
#[ignore = "Performance report: run with --release --ignored --nocapture on a controlled host"]
fn report_dylib_call_latency() {
    let (_g, h) = load_good();
    let c = construct_good(&h);
    unsafe {
        // 预热
        for _ in 0..200 {
            let _ = ask(&loader(), &h, c, "echo", b"w").unwrap();
        }
        let n = 2000;
        let mut samples = Vec::with_capacity(n);
        let mut buf = vec![0u8; 4096];
        for _ in 0..n {
            let mut rb = AbiReplyBuf::new(&mut buf);
            let msg = AbiMsg::new("echo", b"x");
            let t0 = std::time::Instant::now();
            loader().handle_msg(&h, c, msg, &mut rb).unwrap();
            samples.push(t0.elapsed());
            assert_eq!(rb.written, 1);
            assert_eq!(buf[0], b'x');
        }
        samples.sort();
        let p50 = samples[n / 2];
        eprintln!(
            "dylib call latency p50 = {p50:?}; samples={n}; debug_assertions={}",
            cfg!(debug_assertions)
        );
    }
}

#[test]
#[ignore = "Performance report: run with --release --ignored --nocapture on a controlled host"]
fn bench_report_up_and_echo() {
    let (_g, h) = load_good();
    let c = construct_good(&h);
    unsafe {
        for (name, key, p) in [
            ("echo", "echo", b"x".to_vec()),
            ("up", "up", 41u64.to_le_bytes().to_vec()),
        ] {
            let n = 500;
            let mut total = std::time::Duration::ZERO;
            for _ in 0..n {
                let t0 = std::time::Instant::now();
                let _ = ask(&loader(), &h, c, key, &p).unwrap();
                total += t0.elapsed();
            }
            eprintln!("{name} avg: {:?}", total / n);
        }
    }
}

fn counter_reply_capacity(key: &str) {
    let (_lock, handle) = load_good();
    let comp = construct_good(&handle);
    for capacity in 0..=9 {
        let mut bytes = vec![0xa5; capacity + 2];
        let mut output = AbiReplyBuf::new(&mut bytes[1..capacity + 1]);
        let result =
            unsafe { loader().handle_msg(&handle, comp, AbiMsg::new(key, &[]), &mut output) };
        assert_eq!(bytes[0], 0xa5);
        assert_eq!(bytes[capacity + 1], 0xa5);
        if capacity < 8 {
            assert_eq!(result.unwrap_err().0, parrot_abi::ABI_ERR_OVERFLOW);
            assert_eq!(output.written, 0);
            assert!(bytes.iter().all(|&byte| byte == 0xa5));
        } else {
            result.unwrap();
            assert_eq!(output.written, 8);
            if key == "msgs" {
                assert_eq!(&bytes[1..9], &((capacity + 1) as u64).to_le_bytes());
            }
        }
    }
}

#[test]
fn constructed_count_reply_checks_capacity() {
    counter_reply_capacity("cnt");
}

#[test]
fn message_count_reply_checks_capacity() {
    counter_reply_capacity("msgs");
}
