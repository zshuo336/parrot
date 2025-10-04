//! P4b 联邦 POC：**hub 中继路由**（第 4 点"协议之间两两访问"验证）。
//!
//! 拓扑（三运行时全真实进程 + 真实 TCP）：
//!
//! ```text
//! AkkaGw(JVM:9871) ──TCP── parrot hub ──TCP── ErlangGw(OTP:9872)
//!                        (PrefixRouter: "/erl" → erlang 链路)
//! ```
//!
//! 验证目标：akka 网关发起 `ASK /erl/user/erlang_service Ping(100)`，
//! hub 本地未命中 → 前缀路由 → 换 cid 转发给 erlang 网关 → erlang 回
//! REPLY（方言 +3 = 103）→ hub 映射回原 cid → 折返给 akka。
//! **akka 与 erlang 之间没有直接连接，全部流量经 parrot 中继**——这是
//! "任意两运行时互访不必两两直连"的最小证明。
//!
//! 同时保留 rust→akka 直连 ask（方言 +1）验证链路健康，以及
//! akka→rust 反向 ask（方言 *2 = 200）。

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
                Ok(Box::new(Pong(n * 2)) as BoxedMessage) // rust 方言 *2
            } else {
                Err(parrot_api::errors::ActorError::MessageHandlingError("unsupported".into()))
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// panic 安全的子进程守卫：测试无论正常结束还是 panic，都先杀子进程，
/// 防止 stdout reader 挂在 EOF 上卡死 runtime drop。
struct ChildGuard {
    jvm: std::process::Child,
    erl: std::process::Child,
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.jvm.kill();
        let _ = self.erl.kill();
    }
}

/// 读子进程 stdout，等待包含 `needle` 的行（10s 超时）。
async fn wait_for_line(
    reader: &mut tokio::task::JoinHandle<()>,
    rx: &mut tokio::sync::mpsc::Receiver<String>,
    needle: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            panic!("等待 {needle:?} 超时（网关未打印）");
        }
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(line)) if line.contains(needle) => return line,
            Ok(Some(_)) => continue,
            Ok(None) => {
                let _ = reader.await;
                panic!("stdout 提前关闭，未见 {needle:?}");
            }
            Err(_) => panic!("等待 {needle:?} 超时"),
        }
    }
}

#[tokio::test]
async fn p4b_gateway_relay_via_parrot_hub() {
    CodecRegistry::reset();
    install_poc_messages();

    // ---- 1. erlang 网关（9872）----
    let erl_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../remote-poc/erlang-gw");
    if !std::path::Path::new(&format!("{erl_dir}/erlang_gw.beam")).exists() {
        let st = std::process::Command::new("erlc")
            .current_dir(erl_dir)
            .arg("erlang_gw.erl")
            .status()
            .expect("erlc 失败（需要 Erlang）");
        assert!(st.success(), "erlc 编译失败");
    }
    let erl_port = 9872u16;
    let mut erl = std::process::Command::new("erl")
        .current_dir(erl_dir)
        .args([
            "-noshell", "-noinput", "-pa", ".",
            "-eval", &format!("erlang_gw:main([{erl_port}]), halt."),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("erl 启动失败");
    // panic 守卫在此作用域后文统一接管（见 ChildGuard 构造处）
    let erl_stdout = erl.stdout.take().unwrap();
    let (erl_ready_tx, mut erl_ready_rx) = tokio::sync::oneshot::channel::<()>();
    let erl_reader = tokio::task::spawn_blocking(move || {
        let r = std::io::BufReader::new(erl_stdout);
        let mut erl_ready_tx = Some(erl_ready_tx);
        for line in r.lines().flatten() {
            println!("[erl] {line}");
            if let Some(tx) = erl_ready_tx.take() {
                if line.contains("listening on") {
                    let _ = tx.send(());
                } else {
                    erl_ready_tx = Some(tx);
                }
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(30), &mut erl_ready_rx)
        .await
        .expect("erlang 网关 30s 未就绪")
        .unwrap();

    // ---- 2. hub 连接 erlang（客户端角色，erlang 网关 accept 唯一连接）----
    let erl_addr: std::net::SocketAddr = format!("127.0.0.1:{erl_port}").parse().unwrap();
    let erl_link = tokio::time::timeout(Duration::from_secs(5), tcp_connect(erl_addr))
        .await
        .expect("connect erlang timeout")
        .unwrap();

    // ---- 3. akka 网关（9871 + 中继目标参数）----
    let akka_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../remote-poc/akka-gw");
    // 强制重编译（源码刚加过中继参数）
    let st = std::process::Command::new("javac")
        .current_dir(akka_dir)
        .arg("AkkaGw.java")
        .status()
        .expect("javac 失败（需要 JDK）");
    assert!(st.success(), "javac 编译失败");

    let akka_port = 9871u16;
    let mut jvm = std::process::Command::new("java")
        .current_dir(akka_dir)
        .args([
            "AkkaGw",
            &akka_port.to_string(),
            "/erl/user/erlang_service", // P4：中继目标
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("java 启动失败");
    let jvm_stdout = jvm.stdout.take().unwrap();
    // panic 守卫：任何退出路径（含 assert panic）都先杀两个子进程
    let guard = ChildGuard { jvm, erl };
    let (jvm_line_tx, mut jvm_line_rx) = tokio::sync::mpsc::channel::<String>(64);
    let mut jvm_reader = tokio::task::spawn_blocking(move || {
        let r = std::io::BufReader::new(jvm_stdout);
        for line in r.lines().flatten() {
            println!("[jvm] {line}");
            // 阻塞上下文：必须用 blocking_send（send 不 await 会静默丢行）
            let _ = jvm_line_tx.blocking_send(line);
        }
    });
    // 等 listening
    let line = wait_for_line(&mut jvm_reader, &mut jvm_line_rx, "listening on").await;
    assert!(line.contains(&akka_port.to_string()), "akka 监听端口不符: {line}");

    // ---- 4. hub 连接 akka + 组装路由端点 ----
    let akka_addr: std::net::SocketAddr = format!("127.0.0.1:{akka_port}").parse().unwrap();
    let akka_link = tokio::time::timeout(Duration::from_secs(5), tcp_connect(akka_addr))
        .await
        .expect("connect akka timeout")
        .unwrap();

    let router = std::sync::Arc::new(PrefixRouter::new());
    router.add("/erl", erl_link.sender.clone());
    let ep = spawn_endpoint_routed(akka_link, router.clone()).await;

    // erlang 链路的 ingress 泵（消费 erlang 的 REPLY，共用 callbacks/router）
    {
        let local = ep.local.clone();
        let callbacks = ep.callbacks.clone();
        let back = ep.sender();
        let router_c = router.clone();
        tokio::spawn(async move {
            let mut erl_link = erl_link;
            while let Some(f) = erl_link.incoming.recv().await {
                handle_frame_routed(f, &local, &callbacks, &back, Some(&router_c)).await;
            }
        });
    }

    // rust 服务（akka 1s 后主动 ask 的目标；方言 *2）
    let ts = ep.local.get_thread_system("main").unwrap();
    ts.spawn_at::<RustService>(RustService, "/user/rust_service", None, Default::default())
        .await
        .unwrap();

    // ---- 5. akka→rust 反向 ask（方言 *2 = 200，1s 触发，先于 relay）----
    let rust_line = wait_for_line(&mut jvm_reader, &mut jvm_line_rx, "ask rust").await;
    assert!(
        rust_line.contains("200"),
        "akka→rust 应为 100*2=200，got: {rust_line}"
    );

    // ---- 6. 核心断言：akka→erlang 中继（2s 触发，经 parrot hub，无直接连接）----
    // akka 网关发起 relay ask：Ping(100) → erlang 方言 +3 → 103
    let relay_line = wait_for_line(&mut jvm_reader, &mut jvm_line_rx, "relay ask").await;
    println!("relay 结果行: {relay_line}");
    assert!(
        relay_line.contains("103"),
        "中继结果应为 erlang 方言 100+3=103，got: {relay_line}"
    );

    // ---- 7. rust→akka 直连 ask（方言 +1，hub 主链路）----
    let akka_ref = ep.remote_ref("/user/akka_service");
    let pong = akka_ref.send(Box::new(Ping(41))).await.unwrap();
    assert_eq!(pong.downcast_ref::<Pong>().unwrap().0, 42, "akka 方言 echo+1");

    // ---- 8. TELL 中继路径（rust 模拟 akka 侧发 /erl 前缀 TELL → erlang 异步执行）----
    ep.sender()
        .send(Frame::tell("/erl/user/erlang_service", "bin:u:Ping", 42u64.to_le_bytes().to_vec()))
        .await
        .unwrap();
    // TELL at-most-once 无回执：仅验证不阻塞不报错
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 清理：守卫 Drop 时杀子进程；reader 给 2s 收敛
    let _ = tokio::time::timeout(Duration::from_secs(2), jvm_reader).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), erl_reader).await;
    drop(guard);
}
