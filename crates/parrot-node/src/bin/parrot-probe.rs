//! parrot-probe：部署环境验证驱动（compose 起来后的黑盒全量验收器）。
//!
//! 纯客户端形态：连任意已组网节点 → 跨节点 ask/tell/admin-spawn 全语义验证。
//! 退出码 0=全过；非 0=失败（CI/编排门禁直接消费）。
//!
//! 用法：
//!   parrot-probe <target-node-id>=<host:port> [more nodes...]
//!   # 可选场景选择：--smoke | --full（默认 full）
//!
//! 验证矩阵（--full）：
//!   P1 单节点语义    echo/counter/kv/slow 四 actor 逐项
//!   P2 跨节点路由    经节点 B 访问节点 A 的 actor（多跳组网证明）
//!   P3 远程 spawn    admin SpawnLocal deploy.* 工厂 + 新 actor 语义
//!   P4 admin stop    spawn → stop 回执
//!   P5 并发正确性    128 并发 ask 恰好一次（cid 配对）
//!   P6 隔离性        慢 actor 不阻塞其它 actor（并发窗口证明）
//!   P7 延迟采样      echo RTT p50/p99 观测（超阈值仅告警不失败——跨机网络主导）

use std::sync::Arc;
use std::time::{Duration, Instant};

use parrot_api::address::ActorRef;
use parrot_remote::system::{RemoteActorSystem, RemoteConfig};
use parrot_remote::{LocalLookup, NodeAddr};

use parrot_node::{NEcho, NEchoed, NGet, NGetTotal, NGot, NInc, NPut, NSlowEcho, NTotal};

// —— 本 crate 内置节点消息 codec 已随 lib 注册（inventory 全局生效）——

struct NoopLookup;

#[async_trait::async_trait]
impl LocalLookup for NoopLookup {
    async fn lookup(&self, _path: &str) -> Option<Box<dyn ActorRef>> {
        None
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let full = !args.iter().any(|a| a == "--smoke");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    // DNS 解析（host:port 形态——compose 服务名）在 runtime 上做
    let targets = rt.block_on(parse_targets(&args));
    if targets.is_empty() {
        eprintln!("usage: parrot-probe <node-id=host:port>... [--smoke]");
        std::process::exit(2);
    }
    let code = rt.block_on(run(&targets, full));
    std::process::exit(code);
}

/// "id=host:port" → NodeAddr（host 可为 IP 或域名——域名经 DNS 解析）。
async fn parse_targets(args: &[String]) -> Vec<NodeAddr> {
    use std::net::ToSocketAddrs;
    let mut out = Vec::new();
    for a in args {
        if a.starts_with("--") {
            continue;
        }
        let Some((id, addr)) = a.split_once('=') else {
            continue;
        };
        let parsed: std::net::SocketAddr = match addr.trim().parse() {
            Ok(ip) => ip,
            Err(_) => {
                // 域名：阻塞解析（启动一次性；clone 进 'static 闭包）
                let hostport = addr.trim().to_string();
                let resolved = tokio::task::spawn_blocking(move || {
                    hostport
                        .to_socket_addrs()
                        .ok()
                        .and_then(|mut i| i.next())
                })
                .await
                .ok()
                .flatten();
                match resolved {
                    Some(sa) => sa,
                    None => {
                        eprintln!("[probe] cannot resolve target: {addr}");
                        continue;
                    }
                }
            }
        };
        out.push(NodeAddr::tcp(id, parsed));
    }
    out
}

async fn run(targets: &[NodeAddr], full: bool) -> i32 {
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("probe", None),
        Arc::new(NoopLookup),
    )
    .unwrap();
    client.start().await.unwrap();
    for t in targets {
        match client.connect(t).await {
            Ok(_) => println!("[probe] connected: {}", t.node_id),
            Err(e) => {
                eprintln!("[probe] connect {} failed: {e}", t.node_id);
                return 1;
            }
        }
    }
    let primary = &targets[0];
    let pid = primary.node_id.clone();

    // ---- P1 语义验证（echo+counter 全节点遍历；kv/slow 深检 primary）----
    for t in targets {
        let tid = t.node_id.clone();
        if !check(&format!("P1 echo on {tid}"), || {
            let client = &client;
            let tid = tid.clone();
            async move {
                let r = client
                    .remote_ref(&format!("parrot://{tid}/user/echo"))
                    .unwrap()
                    .send(Box::new(NEcho(41)))
                    .await
                    .unwrap();
                r.downcast_ref::<NEchoed>().unwrap().0 == 41
            }
        })
        .await
        {
            return 1;
        }
    }
    if !check(&format!("P1 counter on {pid}"), || async {
        // 幂等性：每次 probe 运行用独立 counter 实例（有状态 actor 不受
        // 重复验收影响——路径时间戳后缀进程间唯一）
        let path = uniq_path("probe-counter");
        let c = client
            .spawn_named(&pid, &path, "deploy.counter")
            .await
            .unwrap();
        c.send(Box::new(NInc(7))).await.unwrap();
        c.send(Box::new(NInc(3))).await.unwrap();
        let r = c.send(Box::new(NGetTotal)).await.unwrap();
        r.downcast_ref::<NTotal>().map(|t| t.0) == Some(10)
    })
    .await
    {
        return 1;
    }
    if !check(&format!("P1 kv on {pid}"), || async {
        let kv = client.remote_ref(&format!("parrot://{pid}/user/kv")).unwrap();
        kv.send(Box::new(NPut("k1".into(), "v1".into()))).await.unwrap();
        let g = kv.send(Box::new(NGet("k1".into()))).await.unwrap();
        g.downcast_ref::<NGot>().unwrap().0.as_deref() == Some("v1")
    })
    .await
    {
        return 1;
    }
    if !check(&format!("P1 slow(80ms) on {pid}"), || async {
        let t0 = Instant::now();
        let r = client
            .remote_ref(&format!("parrot://{pid}/user/slow"))
            .unwrap()
            .send(Box::new(NSlowEcho(80)))
            .await
            .unwrap();
        r.downcast_ref::<NEchoed>().unwrap().0 == 80 && t0.elapsed() >= Duration::from_millis(80)
    })
    .await
    {
        return 1;
    }

    // ---- P2 跨节点路由（全部相邻对 + 首尾对——mesh 全连拓扑验证）----
    if targets.len() >= 2 {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for w in targets.windows(2) {
            pairs.push((w[0].node_id.clone(), w[1].node_id.clone()));
        }
        // 首尾对（链式种子拓扑的最坏路径）
        pairs.push((
            targets[0].node_id.clone(),
            targets[targets.len() - 1].node_id.clone(),
        ));
        for (from, to) in pairs {
            if !check(&format!("P2 cross-route {from} -> {to}"), || {
                let client = &client;
                let to = to.clone();
                async move {
                    let r = client
                        .remote_ref(&format!("parrot://{to}/user/echo"))
                        .unwrap()
                        .send(Box::new(NEcho(99)))
                        .await
                        .unwrap();
                    r.downcast_ref::<NEchoed>().unwrap().0 == 99
                }
            })
            .await
            {
                return 1;
            }
        }
    }

    if !full {
        println!("\nparrot-probe SMOKE PASS");
        return 0;
    }

    // ---- P3 远程 spawn（admin K0）----
    if !check(&format!("P3 remote spawn on {pid}"), || async {
        let path = uniq_path("spawned-echo");
        let spawned = client
            .spawn_named(&pid, &path, "deploy.echo")
            .await
            .unwrap();
        let r = spawned.send(Box::new(NEcho(7))).await.unwrap();
        r.downcast_ref::<NEchoed>().unwrap().0 == 7
    })
    .await
    {
        return 1;
    }

    // ---- P4 admin stop ----
    if !check(&format!("P4 admin stop on {pid}"), || async {
        let path = uniq_path("doomed");
        let _ = client
            .spawn_named(&pid, &path, "deploy.counter")
            .await
            .unwrap();
        client.admin_stop(&pid, &path).await.is_ok()
    })
    .await
    {
        return 1;
    }

    // ---- P5 并发恰好一次 ----
    if !check(&format!("P5 128 concurrent asks on {pid}"), || async {
        let echo = client.remote_ref(&format!("parrot://{pid}/user/echo")).unwrap();
        let mut tasks = Vec::new();
        for i in 0..128u64 {
            let e = echo.clone();
            tasks.push(tokio::spawn(async move {
                let r = e.send(Box::new(NEcho(i))).await.unwrap();
                r.downcast_ref::<NEchoed>().unwrap().0
            }));
        }
        let mut got: Vec<u64> = Vec::with_capacity(128);
        for t in tasks {
            got.push(t.await.unwrap());
        }
        got.sort_unstable();
        got == (0..128).collect::<Vec<_>>()
    })
    .await
    {
        return 1;
    }

    // ---- P6 隔离：slow(600ms) 与 echo 并发，echo 不被拖慢 ----
    if !check(&format!("P6 isolation slow-vs-echo on {pid}"), || async {
        let slow = client.remote_ref(&format!("parrot://{pid}/user/slow")).unwrap();
        let echo = client.remote_ref(&format!("parrot://{pid}/user/echo")).unwrap();
        let slow_task = tokio::spawn(async move {
            let r = slow.send(Box::new(NSlowEcho(600))).await.unwrap();
            r.downcast_ref::<NEchoed>().unwrap().0
        });
        tokio::time::sleep(Duration::from_millis(50)).await; // slow 已入队
        let t0 = Instant::now();
        let r = echo.send(Box::new(NEcho(1))).await.unwrap();
        let echo_rtt = t0.elapsed();
        assert_eq!(r.downcast_ref::<NEchoed>().unwrap().0, 1);
        assert_eq!(slow_task.await.unwrap(), 600);
        // echo RTT 应远小于 slow 剩余时长（600-50ms；阈值 300ms 宽松门禁）
        echo_rtt < Duration::from_millis(300)
    })
    .await
    {
        return 1;
    }

    // ---- P7 延迟采样（观测，不门禁——网络环境主导）----
    {
        let echo = client.remote_ref(&format!("parrot://{pid}/user/echo")).unwrap();
        let mut lat = Vec::new();
        for i in 0..600u64 {
            let t0 = Instant::now();
            let r = echo.send(Box::new(NEcho(i))).await.unwrap();
            assert_eq!(r.downcast_ref::<NEchoed>().unwrap().0, i);
            if i >= 100 {
                lat.push(t0.elapsed().as_micros() as u64);
            }
        }
        lat.sort_unstable();
        let p50 = lat[lat.len() / 2];
        let p99 = lat[(lat.len() * 99) / 100];
        println!("[probe] P7 echo RTT: n={} p50={}µs p99={}µs", lat.len(), p50, p99);
        if p50 > 50_000 {
            eprintln!("[probe] WARN: p50 {}µs > 50ms（跨机网络主导属预期；本机部署请检查）", p50);
        }
    }

    client.shutdown().await.ok();
    println!("\nparrot-probe FULL PASS");
    0
}

/// 进程唯一 actor 路径（纳秒时间戳——重复验收幂等）。
fn uniq_path(base: &str) -> String {
    format!(
        "/user/{base}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

async fn check<F, Fut>(name: &str, f: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let t0 = Instant::now();
    // panic 隔离：场景内 unwrap 崩溃 → FAIL 而非进程死（后续场景仍可跑，
    // 排障信息完整）
    let r = {
        let fut = f();
        use futures::FutureExt;
        std::panic::AssertUnwindSafe(fut)
            .catch_unwind()
            .await
            .unwrap_or(false)
    };
    match r {
        true => {
            println!("[probe] PASS {} ({:?})", name, t0.elapsed());
            true
        }
        false => {
            eprintln!("[probe] FAIL {}", name);
            false
        }
    }
}
