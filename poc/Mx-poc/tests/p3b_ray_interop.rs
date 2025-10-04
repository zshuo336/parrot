//! P3b 联邦 POC：Rust 节点 <-> Ray(Python) 网关互通。
//! 覆盖：rust→ray ask（ray 方言 n+2 / a+b+1000）、ray→rust 反向 ask、
//! 未知服务错误回传、TELL。JVM 网关已由 p3_akka_interop 覆盖。

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
                Ok(Box::new(Pong(n * 3)) as BoxedMessage) // rust 方言 *3（区别 ray 的 +2）
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
async fn p3b_rust_ray_interop_over_tcp() {
    CodecRegistry::reset();
    install_poc_messages();

    let gw_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../remote-poc/ray-adapter");
    let port = 9843u16;

    let mut py = std::process::Command::new("python3")
        .current_dir(gw_dir)
        .args(["ray_gw.py", &port.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("python3 启动失败");
    let stdout = py.stdout.take().unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let reader = tokio::task::spawn_blocking(move || {
        let r = std::io::BufReader::new(stdout);
        let mut ready_tx = Some(ready_tx);
        for line in r.lines().flatten() {
            println!("[ray] {line}");
            if let Some(tx) = ready_tx.take() {
                if line.contains("listening on") {
                    let _ = tx.send(());
                } else {
                    ready_tx = Some(tx); // 未发送，留待下一行
                }
            }
            // 已发送后继续 drain（后续 ask rust 结果行）
        }
    });
    tokio::time::timeout(Duration::from_secs(60), ready_rx)
        .await
        .expect("ray 网关 60s 未就绪（ray.init 慢）")
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

    // rust → ray：Ping（ray 方言 +2）
    let ray_ref = ep.remote_ref("/user/ray_worker");
    let pong = ray_ref.send(Box::new(Ping(40))).await.unwrap();
    assert_eq!(pong.downcast_ref::<Pong>().unwrap().0, 42, "ray 方言应为 n+2");

    // rust → ray：Add（ray 方言 a+b+1000）
    let sum = ray_ref.send(Box::new(Add(20, 22))).await.unwrap();
    assert_eq!(*sum.downcast_ref::<u64>().unwrap(), 1042, "ray 方言应为 a+b+1000");

    // 未知服务 → REPLY_ERR
    CodecRegistry::install::<SecretMsg>(
        "bin:u:Secret",
        |m| match m.downcast_ref::<SecretMsg>() {
            Some(_) => Ok(vec![1]),
            None => Err("down".into()),
        },
        |b| Ok(Box::new(SecretMsg(b.first().copied().unwrap_or(0)))),
    );
    let err = ray_ref.send(Box::new(SecretMsg(1))).await.unwrap_err();
    assert!(err.to_string().contains("unknown key"), "got {err:?}");

    // 等 ray→rust 反向 ask 完成（ray 侧 1s 定时发起；日志会打 -> 300）
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // TELL 路径
    let _ = ray_ref.deliver(Box::new(Ping(1))).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let _ = reader.is_finished();
    let _ = py.kill();
}

#[derive(Debug)]
struct SecretMsg(u8);
