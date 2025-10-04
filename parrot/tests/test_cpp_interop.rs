//! Rust ↔ C++(cpp-lite) 互操作（DEV_04 §5.3——CI matrix 四语言扩 C++）。
//!
//! 编排：Rust 起 TCP echo 节点（同 test_lite_interop 口径）→ spawn
//! C++ 测试二进制（test_interop：pl_connect + pl_ask + vectors 断言）
//! → 断言 CABI-INTEROP PASS。
//!
//! 跑法：cargo test -p parrot --test test_cpp_interop -- --nocapture
//!（C++ 编译在测试内完成——clang++ 缺席则 #[ignore] 语义降级 skip）

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
            Err(parrot_api::errors::ActorError::MessageHandlingError("unhandled".into()))
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

/// 编译并跑 C++ interop 二进制；返回 (exit_ok, stdout)。
fn run_cpp_interop(port: u16) -> (bool, String) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../native/cpp-lite");
    let src = dir.join("test_interop.cpp");
    assert!(src.exists(), "cpp interop source missing: {}", src.display());
    let bin = std::env::temp_dir().join(format!("pl_interop_{}", std::process::id()));
    let cc = std::env::var("CXX").unwrap_or_else(|_| "clang++".into());
    let build = std::process::Command::new(&cc)
        .args(["-std=c++17", "-Wall", "-Wextra"])
        .arg("-I").arg(&dir)
        .arg(&src)
        .arg(dir.join("parrot_lite.cpp"))
        .arg("-o").arg(&bin)
        .output()
        .expect("spawn compiler");
    assert!(
        build.status.success(),
        "C++ build failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let out = std::process::Command::new(&bin)
        .arg(port.to_string())
        .output()
        .expect("run cpp interop");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpp_interop_ask() {
    let facade = Arc::new(ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap());
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

    let (ok, stdout) = run_cpp_interop(port);
    print!("{stdout}");
    assert!(ok, "cpp interop binary failed");
    assert!(stdout.contains("CABI-INTEROP PASS"), "gate marker missing");

    server.shutdown().await.ok();
}

/// Rust 主控 dlopen C++ 共享库直调（§5.3 cabi_smoke——混合体闭环）。
#[test]
fn cabi_smoke_dlopen() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../native/cpp-lite");
    let cc = std::env::var("CXX").unwrap_or_else(|_| "clang++".into());
    let so = std::env::temp_dir().join(format!("libpl_smoke_{}.dylib", std::process::id()));
    let build = std::process::Command::new(&cc)
        .args(["-std=c++17", "-shared", "-fPIC", "-Wall"])
        .arg("-I").arg(&dir)
        .arg(dir.join("parrot_lite.cpp"))
        .arg("-o").arg(&so)
        .output()
        .expect("spawn compiler");
    assert!(
        build.status.success(),
        "dylib build failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    // dlopen + dlsym 符号表核验（C ABI 冻结——pl_* 只增不改不删）
    let path_c = std::ffi::CString::new(so.to_str().unwrap()).unwrap();
    let lib = unsafe { libc_dlopen(path_c.as_ptr()) };
    assert!(!lib.is_null(), "dlopen failed");
    for sym in [
        "pl_connect",
        "pl_register",
        "pl_ask",
        "pl_tell",
        "pl_poll",
        "pl_set_on_ask",
        "pl_free",
        "pl_close",
    ] {
        let name_c = std::ffi::CString::new(sym).unwrap();
        let p = unsafe { libc_dlsym(lib, name_c.as_ptr()) };
        assert!(!p.is_null(), "symbol {sym} missing (ABI frozen)");
    }
    unsafe { libc_dlclose(lib) };

    // 未连接态语义：pl_ask/pl_tell/pl_close 幂等安全
    let (ok, stdout) = run_cpp_interop(0); // port=0 → 仅 ABI 语义冒烟（连接失败路径）
    assert!(ok, "cabi smoke failed");
    assert!(stdout.contains("CABI-SMOKE PASS"));
}

unsafe extern "C" {
    #[link_name = "dlopen"]
    fn libc_dlopen(path: *const std::os::raw::c_char) -> *mut std::os::raw::c_void;
    #[link_name = "dlsym"]
    fn libc_dlsym(
        handle: *mut std::os::raw::c_void,
        name: *const std::os::raw::c_char,
    ) -> *mut std::os::raw::c_void;
    #[link_name = "dlclose"]
    fn libc_dlclose(handle: *mut std::os::raw::c_void) -> std::os::raw::c_int;
}

/// 自环回归（不依赖 C++ 工具链——CI 基线锚点）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpp_echo_selftest() {
    let facade = Arc::new(ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap());
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
    let r = tokio::time::timeout(Duration::from_secs(3), echo.send(Box::new(CppEcho(vec![9, 8, 7]))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.downcast_ref::<CppEcho>().unwrap().0, vec![9, 8, 7]);
}
