//! P3c 联邦 POC：Rust 节点 <-> Erlang/OTP 网关互通。
//! 与 p3(akka)/p3b(ray) 平级同构：双向 ask、方言断言、错误路径、TELL。
//! erlang 方言：Ping(n)→Pong(n+3)；Add(a,b)→a+b+10000。

use parrot::thread::context::ThreadContext;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef as _;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use remote_poc::*;
use std::io::BufRead;
use std::time::Duration;

struct RustService;
impl Actor for RustService {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(Ping(n)) = m.downcast_ref::<Ping>() {
                Ok(Box::new(Pong(n * 5)) as BoxedMessage) // rust 方言 *5（区别 erlang 的 +3）
            } else {
                Err(parrot_api::errors::ActorError::MessageHandlingError("unsupported".into()))
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

#[tokio::test]
async fn p3c_rust_erlang_interop_over_tcp() {
    CodecRegistry::reset();
    install_poc_messages();

    let gw_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../remote-poc/erlang-gw");
    // 编译（无 beam 时）
    if !std::path::Path::new(&format!("{gw_dir}/erlang_gw.beam")).exists() {
        let st = std::process::Command::new("erlc")
            .current_dir(gw_dir)
            .arg("erlang_gw.erl")
            .status()
            .expect("erlc 失败（需要 Erlang）");
        assert!(st.success(), "erlc 编译失败");
    }

    let port = 9853u16;
    let mut erl = std::process::Command::new("erl")
        .current_dir(gw_dir)
        .args([
            "-noshell", "-noinput",
            "-pa", ".",
            "-eval", &format!("erlang_gw:main([{port}]), halt."),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("erl 启动失败");
    let stdout = erl.stdout.take().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let reader = tokio::task::spawn_blocking(move || {
        let r = std::io::BufReader::new(stdout);
        let mut ready_tx = Some(ready_tx);
        for line in r.lines().flatten() {
            println!("[erl] {line}");
            if let Some(tx) = ready_tx.take() {
                if line.contains("listening on") {
                    let _ = tx.send(());
                } else {
                    ready_tx = Some(tx);
                }
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(30), ready_rx)
        .await
        .expect("erlang 网关 30s 未就绪")
        .unwrap();

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let link = tokio::time::timeout(Duration::from_secs(5), tcp_connect(addr))
        .await
        .expect("connect timeout")
        .unwrap();

    let ep = spawn_endpoint(link).await;
    let ts = ep.local.get_thread_system("main").unwrap();
    ts.spawn_at::<RustService>(RustService, "/user/rust_service", None, Default::default())
        .await
        .unwrap();

    // rust → erlang：Ping（erlang 方言 +3）
    let erl_ref = ep.remote_ref("/user/erlang_service");
    let pong = erl_ref.send(Box::new(Ping(39))).await.unwrap();
    assert_eq!(pong.downcast_ref::<Pong>().unwrap().0, 42, "erlang 方言应为 n+3");

    // rust → erlang：Add（erlang 方言 a+b+10000）
    let sum = erl_ref.send(Box::new(Add(20, 22))).await.unwrap();
    assert_eq!(*sum.downcast_ref::<u64>().unwrap(), 10042, "erlang 方言应为 a+b+10000");

    // 未知服务 → REPLY_ERR（erlang 侧 unknown_service 异常透传）
    CodecRegistry::install::<SecretMsg>(
        "bin:u:Secret",
        |m| match m.downcast_ref::<SecretMsg>() {
            Some(_) => Ok(vec![1]),
            None => Err("down".into()),
        },
        |b| Ok(Box::new(SecretMsg(b.first().copied().unwrap_or(0)))),
    );
    let err = erl_ref.send(Box::new(SecretMsg(1))).await.unwrap_err();
    assert!(
        err.to_string().contains("unknown_service"),
        "应为 unknown_service 错误，got {err:?}"
    );

    // 等 erlang→rust 反向 ask（erlang 侧 1s 定时发起，日志打 -> 500）
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // TELL 路径
    let _ = erl_ref.deliver(Box::new(Ping(1))).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let _ = reader.is_finished();
    let _ = erl.kill();
}

#[derive(Debug)]
struct SecretMsg(u8);
