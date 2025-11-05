//! parrot-obs —— parrot 框架观测套件（trace / debug / metric）。
//!
//! 三层能力：
//!   1. 协议层：admin-v2 第五命令 MetricsReport（拉网关全量指标快照）
//!   2. CLI：`parrot-obs <子命令>` ——status/ping/metrics/watch/trace
//!   3. Web：`parrot-obs web` ——单页控制台（拓扑 + 实时指标 + 组件表）
//!
//! 用法：
//!   parrot-obs status  erl=127.0.0.1:19871 [ray=… jvm=…]   # 连通+组件状态
//!   parrot-obs ping    erl=… --rounds 5                     # RTT 探测
//!   parrot-obs metrics erl=… ray=… jvm=…                    # 指标快照表
//!   parrot-obs watch   erl=… --interval 2                   # 持续采样（Ctrl-C 停）
//!   parrot-obs trace   erl=… --seconds 10                   # 10s 差分=吞吐速率
//!   parrot-obs web     erl=… ray=… jvm=… --web-port 8190    # Web 控制台
//!
//! 节点参数三形态与 websearch 一致：erl=addr ray=addr jvm=addr。

mod cli;
mod load;
mod sampling;
mod web;
use cli::{NodeSpec, Options};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parrot_api::address::ActorRef as _;
use parrot_remote::metrics::{field, MeasuredMetrics};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};

/// 纯客户端形态（不接收外部 actor 寻址——LocalLookup 返回 None）。
struct NoopLookup;
#[async_trait::async_trait]
impl LocalLookup for NoopLookup {
    async fn lookup(&self, _path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}

// ───────────────────────── ASK 探测载体（bin:u:Ping/Pong——四方言 echo 探针）─────────────────────────

#[derive(Debug)]
pub struct ProbePing(pub u64);
#[derive(Debug)]
pub struct ProbePong(pub u64);
#[derive(Debug)]
pub struct ProbeEchoed(pub u64);

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:u:Ping",
        type_id: std::any::TypeId::of::<ProbePing>(),
        encode: |msg: &parrot_api::types::BoxedMessage| {
            let m = msg.downcast_ref::<ProbePing>().ok_or("downcast Ping")?;
            use parrot_remote::bytes::BufMut;
            let mut b = parrot_remote::bytes::BytesMut::new();
            b.put_u64_le(m.0);
            Ok(b.freeze().to_vec())
        },
        decode: |b: &[u8]| {
            let v = u64::from_le_bytes(b.try_into().map_err(|_| "len!=8")?);
            Ok(Box::new(ProbePing(v)) as parrot_api::types::BoxedMessage)
        },
    }
}

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:u:Pong",
        type_id: std::any::TypeId::of::<ProbePong>(),
        encode: |msg: &parrot_api::types::BoxedMessage| {
            let m = msg.downcast_ref::<ProbePong>().ok_or("downcast Pong")?;
            use parrot_remote::bytes::BufMut;
            let mut b = parrot_remote::bytes::BytesMut::new();
            b.put_u64_le(m.0);
            Ok(b.freeze().to_vec())
        },
        decode: |b: &[u8]| {
            let v = u64::from_le_bytes(b.try_into().map_err(|_| "len!=8")?);
            Ok(Box::new(ProbePong(v)) as parrot_api::types::BoxedMessage)
        },
    }
}

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot.interop.Echoed#v1",
        type_id: std::any::TypeId::of::<ProbeEchoed>(),
        encode: |msg: &parrot_api::types::BoxedMessage| {
            let m = msg.downcast_ref::<ProbeEchoed>().ok_or("downcast Pong")?;
            use parrot_remote::bytes::BufMut;
            let mut b = parrot_remote::bytes::BytesMut::new();
            b.put_u64_le(m.0);
            Ok(b.freeze().to_vec())
        },
        decode: |b: &[u8]| {
            // JVM echo 原样回显（payload 即所发 Ping 值）
            let v = u64::from_le_bytes(b.try_into().map_err(|_| "len!=8")?);
            Ok(Box::new(ProbeEchoed(v)) as parrot_api::types::BoxedMessage)
        },
    }
}

/// 各方言 echo 探针的 wire 路径（path 段——经 remote_ref 全地址）。
fn probe_path(node: &str) -> String {
    if node.starts_with("erl") {
        "erl/user/echo".into()
    } else if node.starts_with("jvm") {
        "jvm/user/echo".into()
    } else {
        "ray/user/echo".into()
    }
}

// ───────────────────────── 组网 ─────────────────────────

async fn connect(nodes: &[NodeSpec], insecure: bool) -> anyhow::Result<Arc<RemoteActorSystem>> {
    let mut cfg = RemoteConfig::tcp("parrot-obs", None);
    if insecure {
        cfg = cfg.with_insecure_development();
    }
    // Gateways support bin and pb; keep pb available during negotiation.
    cfg.extra_caps = parrot_remote::handshake::caps::PB;
    let client = RemoteActorSystem::new(cfg, Arc::new(NoopLookup))?;
    client.start().await?;
    let mut ok = 0;
    for n in nodes {
        match client.connect(&NodeAddr::tcp(n.name.clone(), n.addr)).await {
            Ok(_) => ok += 1,
            Err(e) => eprintln!(
                "⚠ 节点 {}({}) 连接失败：{}\n  该节点将显示为不可达；先起网关：tools/parrot-obs/gw.sh up",
                terminal_text(&n.name), n.addr, terminal_text(&e.to_string())
            ),
        }
    }
    if ok == 0 {
        eprintln!("（全部节点不可达——仍继续运行以便给出明确诊断）");
    }
    Ok(client)
}

// ───────────────────────── CLI 渲染 ─────────────────────────

fn human_bytes(b: u64) -> String {
    const K: u64 = 1024;
    if b >= K * K * K {
        format!("{:.2} GiB", b as f64 / (K * K * K) as f64)
    } else if b >= K * K {
        format!("{:.1} MiB", b as f64 / (K * K) as f64)
    } else if b >= K {
        format!("{:.1} KiB", b as f64 / K as f64)
    } else {
        format!("{b} B")
    }
}

fn uptime_hms(s: u64) -> String {
    format!("{}h{:02}m{:02}s", s / 3600, (s % 3600) / 60, s % 60)
}

/// 单节点一行摘要。
fn terminal_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}
fn metric_text(m: &MeasuredMetrics, bit: u64) -> String {
    m.value(bit)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "n/a".into())
}
fn byte_text(m: &MeasuredMetrics, bit: u64) -> String {
    m.value(bit)
        .map(human_bytes)
        .unwrap_or_else(|| "n/a".into())
}
fn runtime_text(m: &MeasuredMetrics) -> String {
    if m.has(field::RUNTIME) {
        terminal_text(&m.runtime)
    } else {
        "n/a".into()
    }
}
fn uptime_text(m: &MeasuredMetrics) -> String {
    m.value(field::TS_MS)
        .zip(m.value(field::UPTIME_START_MS))
        .and_then(|(now, start)| now.checked_sub(start))
        .map(|ms| uptime_hms(ms / 1000))
        .unwrap_or_else(|| "n/a".into())
}
fn process_text(m: &MeasuredMetrics) -> String {
    m.value(field::PROCESSES)
        .map(|n| format!("{n} {}", terminal_text(&m.process_kind)))
        .unwrap_or_else(|| "n/a".into())
}
fn status_line(name: &str, m: &MeasuredMetrics) -> String {
    format!(
        "{:<14} {:<18} up {:>10}  conn={} asks={} replies={} err={} io={}/{} rss={} processes={}",
        terminal_text(name),
        runtime_text(m),
        uptime_text(m),
        metric_text(m, field::CONNECTIONS),
        metric_text(m, field::ASKS_RX),
        metric_text(m, field::REPLIES_TX),
        metric_text(m, field::REPLY_ERRS),
        byte_text(m, field::BYTES_RX),
        byte_text(m, field::BYTES_TX),
        byte_text(m, field::MEMORY_RSS),
        process_text(m),
    )
}

/// 全宽指标表（metrics 子命令）。
fn metrics_table(rows: &[(String, MeasuredMetrics)]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n┌{:-<14}┬{:-<18}┬{:>9}┬{:>9}┬{:>9}┬{:>9}┬{:>11}┬{:>11}┬{:>9}┬{:>9}┐\n",
        "─", "─", "─", "─", "─", "─", "─", "─", "─", "─"
    ));
    out.push_str(&format!(
        "│{:<14}│{:<18}│{:>9}│{:>9}│{:>9}│{:>9}│{:>11}│{:>11}│{:>9}│{:>9}│\n",
        "节点",
        "运行时",
        "uptime",
        "conn",
        "asks",
        "tells",
        "bytes_rx",
        "bytes_tx",
        "replies",
        "errs"
    ));
    for (name, m) in rows {
        out.push_str(&format!(
            "│{:<14}│{:<18}│{:>9}│{:>9}│{:>9}│{:>9}│{:>11}│{:>11}│{:>9}│{:>9}│\n",
            terminal_text(name),
            runtime_text(m),
            uptime_text(m),
            metric_text(m, field::CONNECTIONS),
            metric_text(m, field::ASKS_RX),
            metric_text(m, field::TELLS_RX),
            byte_text(m, field::BYTES_RX),
            byte_text(m, field::BYTES_TX),
            metric_text(m, field::REPLIES_TX),
            metric_text(m, field::REPLY_ERRS)
        ));
    }
    out.push_str(&format!(
        "└{:-<14}┴{:-<18}┴{:>9}┴{:>9}┴{:>9}┴{:>9}┴{:>11}┴{:>11}┴{:>9}┴{:>9}┘\n",
        "─", "─", "─", "─", "─", "─", "─", "─", "─", "─"
    ));
    // 组件明细
    for (name, m) in rows {
        let name = terminal_text(name);
        out.push_str(&format!(
            "\n[{name}] processes={} rss={} components={}\n",
            process_text(m),
            byte_text(m, field::MEMORY_RSS),
            metric_text(m, field::COMPONENTS)
        ));
        if !m.has(field::COMPONENT_STATES) {
            out.push_str("  component states: n/a\n");
        } else if !m.component_states.is_empty() {
            out.push_str(&format!(
                "\n[{name}] 实例状态 ({}):\n",
                m.component_states.len()
            ));
            for c in &m.component_states {
                out.push_str(&format!(
                    "  {:<40} {:<10} {}\n",
                    terminal_text(&c.path),
                    terminal_text(&c.state),
                    terminal_text(&c.version)
                ));
            }
        }
    }
    out
}

// ───────────────────────── 采集 ─────────────────────────

async fn collect_all(
    client: &Arc<RemoteActorSystem>,
    nodes: &[NodeSpec],
) -> Vec<(String, Result<MeasuredMetrics, String>)> {
    sampling::collect(client, nodes)
        .await
        .into_iter()
        .map(|(name, result)| (name, result.map(|sample| sample.metrics)))
        .collect()
}

fn verify_probe_reply(
    node: &str,
    value: u64,
    reply: &parrot_api::types::BoxedMessage,
) -> anyhow::Result<()> {
    let (actual, offset) = if node.starts_with("jvm") {
        (reply.downcast_ref::<ProbeEchoed>().map(|p| p.0), 0)
    } else if node.starts_with("erl") {
        (reply.downcast_ref::<ProbePong>().map(|p| p.0), 3)
    } else if node.starts_with("ray") {
        (reply.downcast_ref::<ProbePong>().map(|p| p.0), 2)
    } else {
        anyhow::bail!("unsupported probe dialect: {node}");
    };
    let expected = value
        .checked_add(offset)
        .ok_or_else(|| anyhow::anyhow!("probe value overflow"))?;
    anyhow::ensure!(
        actual == Some(expected),
        "{node} returned an invalid reply: expected {expected}, received {actual:?}"
    );
    Ok(())
}

async fn probe_once(
    client: &Arc<RemoteActorSystem>,
    node: &NodeSpec,
    value: u64,
) -> anyhow::Result<()> {
    let path = format!("parrot://{}/{}", node.name, probe_path(node.dialect));
    let actor = client.remote_ref(&path)?;
    let reply = actor
        .send_with_timeout(Box::new(ProbePing(value)), Some(Duration::from_secs(10)))
        .await?;
    verify_probe_reply(node.dialect, value, &reply)
}

const HELP: &str = "parrot-obs <command> erl=IP:port [ray=IP:port jvm=IP:port] [options]
  status / metrics   snapshot and component state
  ping / ask         admin RTT / validated dialect business probe (--rounds 1..10000)
  watch              sampled counters and rates (--interval 1..86400 seconds)
  trace              counter differences, not distributed tracing (--seconds 1..86400)
  load               validated requests (--seconds 1..86400 --conc 1..128)
  web                dashboard + Prometheus /metrics (--web-port 1..65535)
  --insecure-development  explicitly allow unencrypted development gateways
  --help             show help without connecting
Use jvm@node-id=IP:port (also erl/ray) to override a gateway identity.
IPv6 addresses must be bracketed. Quoted space/comma-separated node lists are supported.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = Options::parse(&args)?;
    if opts.command == "help" {
        println!("{HELP}");
        return Ok(());
    }
    if std::env::var("RUST_LOG").is_ok() {
        parrot::logging::init_default(false);
    }
    let client = connect(&opts.nodes, opts.insecure).await?;
    let result = tokio::select! {
        result=run_command(&client,&opts)=>result,
        signal=tokio::signal::ctrl_c()=>signal.map_err(anyhow::Error::from),
    };
    // Always join transport shutdown, including command errors and interruption.
    let shutdown = client.shutdown().await;
    match (result, shutdown) {
        (Err(error), Err(cleanup)) => {
            Err(anyhow::anyhow!("{error}; shutdown also failed: {cleanup}"))
        }
        (Err(error), _) => Err(error),
        (Ok(()), result) => Ok(result?),
    }
}

async fn run_command(client: &Arc<RemoteActorSystem>, opts: &Options) -> anyhow::Result<()> {
    let nodes = &opts.nodes;
    match opts.command.as_str() {
        "status" | "metrics" => {
            let mut rows = Vec::new();
            let mut failures = 0;
            for (name, result) in collect_all(client, nodes).await {
                match result {
                    Ok(m) => {
                        if opts.command == "status" {
                            println!("✓ {}", status_line(&name, &m));
                        }
                        rows.push((name, m));
                    }
                    Err(error) => {
                        failures += 1;
                        eprintln!("✗ {}: {}", terminal_text(&name), terminal_text(&error));
                    }
                }
            }
            if opts.command == "metrics" {
                print!("{}", metrics_table(&rows));
            }
            anyhow::ensure!(failures == 0, "{failures} nodes failed to report metrics");
        }
        "ping" | "ask" => {
            let mut failures = 0;
            for node in nodes {
                print!("{:<14}", terminal_text(&node.name));
                for i in 0..opts.rounds {
                    let started = Instant::now();
                    let result = if opts.command == "ask" {
                        probe_once(client, node, i).await
                    } else {
                        client
                            .measured_metrics(&node.name)
                            .await
                            .map(|_| ())
                            .map_err(anyhow::Error::from)
                    };
                    match result {
                        Ok(()) => print!(" ✓{:.2}ms", started.elapsed().as_secs_f64() * 1000.0),
                        Err(error) => {
                            failures += 1;
                            print!(" FAIL({})", terminal_text(&error.to_string()));
                        }
                    }
                }
                println!();
            }
            anyhow::ensure!(failures == 0, "{failures} probes failed");
        }
        "watch" => {
            let started = Instant::now();
            let mut last = HashMap::new();
            loop {
                for (name, result) in sampling::collect(client, nodes).await {
                    match result {
                        Ok(sample) => {
                            println!(
                                "[{:.1}s] {}",
                                started.elapsed().as_secs_f64(),
                                sampling::line(&name, last.get(&name), &sample)
                            );
                            last.insert(name, sample);
                        }
                        Err(error) => {
                            last.remove(&name);
                            eprintln!("✗ {}: {}", terminal_text(&name), terminal_text(&error));
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(opts.interval)).await;
            }
        }
        "trace" => {
            let mut before = HashMap::new();
            let mut failures = 0;
            for (name, result) in sampling::collect(client, nodes).await {
                match result {
                    Ok(sample) => {
                        before.insert(name, sample);
                    }
                    Err(error) => {
                        failures += 1;
                        eprintln!("✗ {}: {}", terminal_text(&name), terminal_text(&error));
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(opts.seconds)).await;
            for (name, result) in sampling::collect(client, nodes).await {
                match result {
                    Ok(sample) => println!("{}", sampling::line(&name, before.get(&name), &sample)),
                    Err(error) => {
                        failures += 1;
                        eprintln!("✗ {}: {}", terminal_text(&name), terminal_text(&error));
                    }
                }
            }
            anyhow::ensure!(failures == 0, "{failures} metric samples failed");
        }
        "load" => {
            let r = load::run_load(
                client.clone(),
                nodes.clone(),
                Duration::from_secs(opts.seconds),
                opts.concurrency,
            )
            .await?;
            println!("valid={} failed={} elapsed={:.3}s throughput={:.2}/s latency bucket upper bounds P50={} P95={} P99={}",r.ok,r.errors,r.elapsed.as_secs_f64(),r.ok as f64/r.elapsed.as_secs_f64(),r.latencies.text(50),r.latencies.text(95),r.latencies.text(99));
            r.verify()?;
        }
        "web" => web::serve(client.clone(), nodes.clone(), opts.web_port).await?,
        _ => unreachable!("validated command"),
    }
    Ok(())
}

#[cfg(test)]
mod contracts;
