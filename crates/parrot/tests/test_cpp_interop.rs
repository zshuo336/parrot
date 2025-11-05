//! Rust ↔ C++(cpp-lite) 互操作（DEV_04 §5.3——CI matrix 四语言扩 C++）。
//!
//! 编排：Rust 起 TCP echo 节点（同 test_lite_interop 口径）→ spawn
//! C++ 测试二进制（test_interop：pl_connect + pl_ask + vectors 断言）
//! → 断言 CABI-INTEROP PASS。
//!
//! 跑法：cargo test -p parrot --test test_cpp_interop -- --nocapture
//! C++ 编译在测试内完成；需要 CXX 指定的编译器（默认 clang++），缺失会失败。

use std::sync::Arc;
use std::time::Duration;

use parrot::system::ParrotActorSystem;
use parrot::thread::config::ThreadActorConfig;
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, EmptyConfig};
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::system::{ActorSystem, ActorSystemConfig};
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct CppEcho(pub Vec<u8>);

macro_rules! reg {
    ($t:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$t>(),
                encode: |msg: &BoxedMessage| {
                    let m = msg.downcast_ref::<$t>().ok_or(concat!("downcast ", $key))?;
                    Ok(m.0.clone())
                },
                decode: |b: &[u8]| {
                    Ok(Box::new(<$t>::from_bytes(b.to_vec())) as BoxedMessage)
                },
            }
        }
    };
}

impl CppEcho {
    fn from_bytes(b: Vec<u8>) -> Self {
        CppEcho(b)
    }
}

reg!(CppEcho, "bin:u:Echo");

struct EchoActor;

impl Actor for EchoActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(CppEcho(v)) = msg.downcast_ref::<CppEcho>() {
                return Ok(Box::new(CppEcho(v.clone())) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "unhandled".into(),
            ))
        })
    }
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
}

struct Lookup {
    facade: Arc<ParrotActorSystem>,
}

#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
        self.facade.get_actor(&ActorPath::placeholder(path)).await
    }
}

/// Each compiled artifact belongs to one test invocation, including failure paths.
struct CppBuildDir(std::path::PathBuf);
impl CppBuildDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("parrot-cpp-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).expect("create owned C++ build directory");
        Self(path)
    }
}
impl Drop for CppBuildDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn bounded_cpp(command: &mut tokio::process::Command) -> std::process::Output {
    tokio::time::timeout(Duration::from_secs(60), command.kill_on_drop(true).output())
        .await
        .expect("C++ child exceeded 60 seconds")
        .expect("spawn C++ child")
}
/// Await children without occupying a Tokio worker needed by the actual peer.
async fn run_cpp(port: u16, source: &str) -> (bool, String) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/cpp-lite");
    let owned = CppBuildDir::new();
    let bin = owned.0.join("client");
    let cc = std::env::var("CXX").unwrap_or_else(|_| "clang++".into());
    let build = bounded_cpp(
        tokio::process::Command::new(&cc)
            .args(["-std=c++17", "-O2", "-Wall", "-Wextra", "-pthread"])
            .arg("-I")
            .arg(&dir)
            .arg(dir.join(source))
            .arg(dir.join("parrot_lite.cpp"))
            .arg(dir.join("parrot_poll.cpp"))
            .arg("-o")
            .arg(&bin),
    )
    .await;
    assert!(
        build.status.success(),
        "C++ build failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let out = bounded_cpp(tokio::process::Command::new(&bin).arg(port.to_string())).await;
    if source == "test_perf.cpp" && out.status.success() {
        for (value, expected) in [
            ("1000000000000", "requested throughput floor"),
            ("NaN", "performance limit must be a positive finite number"),
        ] {
            let rejected = bounded_cpp(
                tokio::process::Command::new(&bin)
                    .arg(port.to_string())
                    .env("PL_CPP_MIN_QPS", value),
            )
            .await;
            let text = String::from_utf8_lossy(&rejected.stdout);
            assert!(
                !rejected.status.success(),
                "performance guard accepted {value}"
            );
            assert!(
                text.contains(expected)
                    && text.contains("CPP-LITE CHECKS FAIL")
                    && text
                        .lines()
                        .filter(|line| line.starts_with("FAIL "))
                        .count()
                        == 1,
                "unexpected performance rejection: {text} {}",
                String::from_utf8_lossy(&rejected.stderr)
            );
            println!("CPP performance rejection guard {value}: PASS");
        }
    }

    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpp_interop_ask() {
    let facade = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    let ts = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("eng".into(), ts.clone(), true)
        .await
        .unwrap();
    ts.spawn_at(EchoActor, "/user/echo", None, ThreadActorConfig::default())
        .await
        .unwrap();

    let bind: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut cfg = RemoteConfig::tcp("interop-cpp-rust", Some(bind));
    cfg.extra_caps = parrot_remote::handshake::caps::PB; // lite pb-only
    let server = Arc::new(RemoteActorSystem::new(cfg, Arc::new(Lookup { facade })).unwrap());
    server.start().await.unwrap();
    let port = server.local_addr().expect("bound").port();

    let (ok, stdout) = run_cpp(port, "test_interop.cpp").await;
    print!("{stdout}");
    assert!(ok, "cpp interop binary failed");
    assert!(stdout.contains("CABI-INTEROP PASS"), "gate marker missing");

    // 内容／并发配对检查；性能值报告，发布阈值由 check-cpp-perf 显式启用。
    let (pok, pout) = run_cpp(port, "test_perf.cpp").await;
    print!("{pout}");
    assert!(pok, "cpp perf binary failed");
    assert!(
        pout.contains("P1 backend-select PASS")
            && pout.contains("P2 decoder-bulk PASS")
            && pout.contains("P3 concurrent-ask PASS"),
        "perf gate markers missing"
    );

    server.shutdown().await.expect("retire Rust peer");
}

/// Rust 主控 dlopen C++ 共享库直调（§5.3 cabi_smoke——混合体闭环）。
#[tokio::test]
async fn cabi_smoke_dlopen() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/cpp-lite");
    let cc = std::env::var("CXX").unwrap_or_else(|_| "clang++".into());
    let owned = CppBuildDir::new();
    let so = owned
        .0
        .join(format!("libpl_smoke{}", std::env::consts::DLL_SUFFIX));
    let build = bounded_cpp(
        tokio::process::Command::new(&cc)
            .args(["-std=c++17", "-shared", "-fPIC", "-Wall", "-pthread"])
            .arg("-I")
            .arg(&dir)
            .arg(dir.join("parrot_lite.cpp"))
            .arg("-o")
            .arg(&so),
    )
    .await;
    assert!(
        build.status.success(),
        "dylib build failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    // Keep the library owned while calling the actual C ABI. The previous
    // hand-written dlopen declaration omitted its required flags argument.
    let lib = unsafe { libloading::Library::new(&so) }.expect("load C++ shared library");
    type Ask = unsafe extern "C" fn(
        *const std::ffi::c_char,
        *const std::ffi::c_char,
        *const u8,
        usize,
        *mut *mut u8,
        *mut usize,
        std::ffi::c_int,
    ) -> std::ffi::c_int;
    type Tell = unsafe extern "C" fn(
        *const std::ffi::c_char,
        *const std::ffi::c_char,
        *const u8,
        usize,
    ) -> std::ffi::c_int;
    type ThreeStrings = unsafe extern "C" fn(
        *const std::ffi::c_char,
        *const std::ffi::c_char,
        *const std::ffi::c_char,
    ) -> std::ffi::c_int;
    type OnAsk = unsafe extern "C" fn(
        *const std::ffi::c_char,
        *const std::ffi::c_char,
        *const u8,
        usize,
        u64,
    );
    unsafe {
        let ask = lib.get::<Ask>(b"pl_ask\0").unwrap();
        let tell = lib.get::<Tell>(b"pl_tell\0").unwrap();
        let register = lib.get::<ThreeStrings>(b"pl_register\0").unwrap();
        let connect = lib.get::<ThreeStrings>(b"pl_connect\0").unwrap();
        let close = lib.get::<unsafe extern "C" fn()>(b"pl_close\0").unwrap();
        let poll = lib
            .get::<unsafe extern "C" fn(std::ffi::c_int)>(b"pl_poll\0")
            .unwrap();
        let free = lib
            .get::<unsafe extern "C" fn(*mut std::ffi::c_void)>(b"pl_free\0")
            .unwrap();
        let set_callback = lib
            .get::<unsafe extern "C" fn(Option<OnAsk>, *mut std::ffi::c_void)>(b"pl_set_on_ask\0")
            .unwrap();
        let mut out = std::ptr::null_mut();
        let mut out_len = 0;
        assert_eq!(
            ask(
                c"/user/x".as_ptr(),
                c"k".as_ptr(),
                std::ptr::null(),
                0,
                &mut out,
                &mut out_len,
                100
            ),
            -2
        );
        assert!(out.is_null());
        assert_eq!(out_len, 0);
        assert_eq!(
            tell(c"/user/x".as_ptr(), c"k".as_ptr(), std::ptr::null(), 0),
            -2
        );
        assert_eq!(
            register(c"k".as_ptr(), c"/p".as_ptr(), std::ptr::null()),
            -2
        );
        set_callback(None, std::ptr::null_mut());
        poll(0);
        close();
        close();
        free(std::ptr::null_mut());
        assert_eq!(
            connect(c"bogus://x".as_ptr(), c"n".as_ptr(), std::ptr::null()),
            -1
        );
    }
    drop(lib);
    std::fs::remove_file(so).unwrap();
}

/// 自环回归（不依赖 C++ 工具链——CI 基线锚点）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpp_echo_selftest() {
    let facade = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    let ts = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("eng".into(), ts.clone(), true)
        .await
        .unwrap();
    ts.spawn_at(EchoActor, "/user/echo", None, ThreadActorConfig::default())
        .await
        .unwrap();
    let echo = facade
        .get_actor(&ActorPath::placeholder("/user/echo"))
        .await
        .unwrap();
    let r = tokio::time::timeout(
        Duration::from_secs(3),
        echo.send(Box::new(CppEcho(vec![9, 8, 7]))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(r.downcast_ref::<CppEcho>().unwrap().0, vec![9, 8, 7]);
}

/// Exercise actual hostile peers through the C ABI; child ownership bounds hangs.
#[tokio::test]
async fn cpp_transport_adversarial_contracts() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../interop/cpp-lite/test_transport.py");
    let output = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new("python3")
            .arg(script)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("C++ transport contracts exceeded 90 seconds")
    .expect("Python and C++ compiler are required for transport contracts");
    assert!(
        output.status.success(),
        "C++ transport contracts failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    validate_transport_receipt(&String::from_utf8_lossy(&output.stdout))
        .expect("transport suite must execute every selected case without failures or skips");
}

fn validate_transport_receipt(stdout: &str) -> Result<(), String> {
    #[derive(serde::Deserialize, Debug)]
    #[serde(deny_unknown_fields)]
    struct Receipt {
        schema: u64,
        selected: u64,
        executed: u64,
        failures: u64,
        errors: u64,
        skipped: u64,
        expected_failures: u64,
        unexpected_successes: u64,
        successful: bool,
    }
    let mut receipts = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("PARROT_CPP_TEST_RECEIPT "));
    let receipt: Receipt =
        serde_json::from_str(receipts.next().ok_or("missing execution receipt")?)
            .map_err(|e| e.to_string())?;
    if receipts.next().is_some()
        || receipt.schema != 1
        || receipt.selected == 0
        || receipt.selected != receipt.executed
        || receipt.failures != 0
        || receipt.errors != 0
        || receipt.skipped != 0
        || receipt.expected_failures != 0
        || receipt.unexpected_successes != 0
        || !receipt.successful
    {
        return Err(format!("incomplete or unsuccessful execution: {receipt:?}"));
    }
    Ok(())
}

#[test]
fn cpp_receipt_requires_complete_success_without_fixing_the_suite_size() {
    let base = serde_json::json!({
        "schema": 1, "selected": 30, "executed": 30, "failures": 0, "errors": 0,
        "skipped": 0, "expected_failures": 0, "unexpected_successes": 0, "successful": true
    });
    let line = |v: &serde_json::Value| format!("PARROT_CPP_TEST_RECEIPT {v}\n");
    for count in [1, 28, 30, 77] {
        let mut receipt = base.clone();
        receipt["selected"] = count.into();
        receipt["executed"] = count.into();
        validate_transport_receipt(&line(&receipt)).unwrap();
    }
    for (field, value) in [
        ("schema", serde_json::json!(2)),
        ("selected", serde_json::json!(0)),
        ("executed", serde_json::json!(29)),
        ("failures", serde_json::json!(1)),
        ("errors", serde_json::json!(1)),
        ("skipped", serde_json::json!(1)),
        ("expected_failures", serde_json::json!(1)),
        ("unexpected_successes", serde_json::json!(1)),
        ("successful", serde_json::json!(false)),
    ] {
        let mut receipt = base.clone();
        receipt[field] = value;
        assert!(
            validate_transport_receipt(&line(&receipt)).is_err(),
            "accepted {field}"
        );
    }
    assert!(validate_transport_receipt("Ran 30 tests\nOK\n").is_err());
    assert!(validate_transport_receipt(&(line(&base) + &line(&base))).is_err());
    let mut empty = base.clone();
    empty["selected"] = 0.into();
    empty["executed"] = 0.into();
    assert!(validate_transport_receipt(&line(&empty)).is_err());
    let mut missing = base.clone();
    missing.as_object_mut().unwrap().remove("executed");
    assert!(validate_transport_receipt(&line(&missing)).is_err());
}
