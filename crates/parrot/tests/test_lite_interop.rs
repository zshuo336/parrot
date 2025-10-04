//! Rust ↔ TS(parrot-lite) 握手协商互测（DEV_03 §2.4——CI matrix 形态）。
//!
//! #[ignore] 手动门禁：spawn `node interop/typescript-lite/tests/interop_with_rust.mjs`
//! 对拉本测试内启动的 TCP echo 节点。
//!
//! 跑法：cargo test -p parrot --test test_lite_interop -- --ignored --nocapture

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
struct LiteEcho(pub Vec<u8>);

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

impl LiteEcho {
    fn from_bytes(b: Vec<u8>) -> Self {
        LiteEcho(b)
    }
}

reg!(LiteEcho, "bin:u:Echo");

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
            if let Some(LiteEcho(v)) = msg.downcast_ref::<LiteEcho>() {
                return Ok(Box::new(LiteEcho(v.clone())) as BoxedMessage);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual gate: needs node + interop/typescript-lite built (DEV_00 §4.2 P3)"]
async fn lite_handshake_negotiation_with_rust() {
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
    let mut cfg = RemoteConfig::tcp("interop-lite-rust", Some(bind));
    cfg.extra_caps = parrot_remote::handshake::caps::PB; // lite pb-only
    let server = Arc::new(RemoteActorSystem::new(cfg, Arc::new(Lookup { facade })).unwrap());
    server.start().await.unwrap();
    let port = server.local_addr().expect("bound").port();
    println!("PARROT_LITE_PORT={port}");

    // spawn node 互测脚本（lite build 后 dist 存在）
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../interop/typescript-lite/tests/interop_with_rust.mjs");
    assert!(
        manifest.exists(),
        "lite interop script missing: {}",
        manifest.display()
    );
    let out = std::process::Command::new("node")
        .arg(manifest)
        .arg(port.to_string())
        .output()
        .expect("spawn node");
    let stdout = String::from_utf8_lossy(&out.stdout);
    print!("{stdout}");
    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "lite interop script failed");
    assert!(stdout.contains("LITE-INTEROP PASS"));

    server.shutdown().await.ok();
}

/// Rust 侧自环（不依赖 node——CI 基线）：echo 语义回归锚点。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lite_echo_selftest() {
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
        echo.send(Box::new(LiteEcho(vec![1, 2, 3]))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(r.downcast_ref::<LiteEcho>().unwrap().0, vec![1, 2, 3]);
}
