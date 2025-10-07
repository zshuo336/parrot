//! parrot-obs —— parrot 框架观测套件（trace / debug / metric）。
//!
//! 三层能力：
//!   1. 协议层：admin-v2 第五命令 MetricsReport（拉网关全量指标快照）
//!   2. CLI：`parrot-obs <子命令>` ——status/ping/metrics/watch/trace
//!   3. Web：`parrot-probe web` ——单页控制台（拓扑 + 实时指标 + 组件表）
//!
//! 用法：
//!   parrot-probe status  erl=127.0.0.1:19871 [ray=… jvm=…]   # 连通+组件状态
//!   parrot-probe ping    erl=… --rounds 5                     # RTT 探测
//!   parrot-probe metrics erl=… ray=… jvm=…                    # 指标快照表
//!   parrot-probe watch   erl=… --interval 2                   # 持续采样（Ctrl-C 停）
//!   parrot-probe trace   erl=… --seconds 10                   # 10s 差分=吞吐速率
//!   parrot-probe web     erl=… ray=… jvm=… --web-port 8190    # Web 控制台
//!
//! 节点参数三形态与 websearch 一致：erl=addr ray=addr jvm=addr。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use parrot_remote::admin_v2::MetricsSnapshot;
use parrot_api::address::ActorRef as _;

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
            Ok(Box::new(ProbePong(v)) as parrot_api::types::BoxedMessage)
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
        type_id: std::any::TypeId::of::<ProbePong>(),
        encode: |msg: &parrot_api::types::BoxedMessage| {
            let m = msg.downcast_ref::<ProbePong>().ok_or("downcast Pong")?;
            use parrot_remote::bytes::BufMut;
            let mut b = parrot_remote::bytes::BytesMut::new();
            b.put_u64_le(m.0);
            Ok(b.freeze().to_vec())
        },
        decode: |b: &[u8]| {
            // JVM echo 原样回显（payload 即所发 Ping 值）
            let v = b
                .try_into()
                .ok()
                .map(u64::from_le_bytes)
                .unwrap_or(0);
            Ok(Box::new(ProbePong(v)) as parrot_api::types::BoxedMessage)
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

#[derive(Clone)]
struct NodeSpec {
    name: String,
    addr: String,
}

/// 解析 `erl=… ray=… jvm=…` 参数（任意子集）。
fn parse_nodes(args: &[String]) -> Vec<NodeSpec> {
    // 兼容三种传参形态（zsh 标量 $NODES 不分词——单串整包也认）：
    //   erl=1.2.3.4:1 ray=... jvm=...     （多参数——bash/显式）
    //   "erl=1.2.3.4:1 ray=... jvm=..."   （单串空格分隔——zsh $NODES）
    //   "erl=...,ray=...,jvm=..."         （单串逗号分隔）
    let mut out = Vec::new();
    for a in args {
        for token in a.split(|c: char| c == ',' || c.is_whitespace()) {
            let Some((k, v)) = token.split_once('=') else { continue };
            if !["erl", "ray", "jvm"].contains(&k) {
                continue;
            }
            out.push(NodeSpec {
                name: match k {
                    "erl" => "erl-gw-1".to_string(),
                    "ray" => "ray-gw-1".to_string(),
                    "jvm" => "jvm-search-1".to_string(),
                    _ => k.to_string(),
                },
                addr: v.to_string(),
            });
        }
    }
    out
}

async fn connect(nodes: &[NodeSpec]) -> anyhow::Result<Arc<RemoteActorSystem>> {
    let mut cfg = RemoteConfig::tcp("parrot-obs", None);
    // 三方言网关 pb-only（erl/ray/jvm 无 bincode codec 面——握手 cap 对齐）
    cfg.extra_caps = parrot_remote::handshake::caps::PB;
    let client = RemoteActorSystem::new(cfg, Arc::new(NoopLookup))?;
    client.start().await?;
    let mut ok = 0;
    for n in nodes {
        let addr = match n.addr.parse() {
            Ok(a) => a,
            Err(e) => {
                eprintln!("⚠ 节点 {}={} 地址非法（期望 host:port）：{e}——跳过", n.name, n.addr);
                continue;
            }
        };
        match client.connect(&NodeAddr::tcp(n.name.clone(), addr)).await {
            Ok(_) => ok += 1,
            Err(e) => eprintln!(
                "⚠ 节点 {}({}) 连接失败：{e}\n  该节点将显示为不可达；先起网关：tools/parrot-obs/gw.sh up",
                n.name, n.addr
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
fn status_line(name: &str, m: &MetricsSnapshot) -> String {
    format!(
        "{:<14} {:<18} up {:>10}  conn={} asks={} replies={} err={} io={} rss={}",
        name,
        m.runtime,
        uptime_hms(m.uptime_secs()),
        m.connections,
        m.asks_rx,
        m.replies_tx,
        m.reply_errs,
        format!("{}/{}", human_bytes(m.bytes_rx), human_bytes(m.bytes_tx)),
        human_bytes(m.memory_rss),
    )
}

/// 全宽指标表（metrics 子命令）。
fn metrics_table(rows: &[(String, MetricsSnapshot)]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n┌{:-<14}┬{:-<18}┬{:>9}┬{:>9}┬{:>9}┬{:>9}┬{:>11}┬{:>11}┬{:>9}┬{:>9}┐\n",
        "─", "─", "─", "─", "─", "─", "─", "─", "─", "─"
    ));
    out.push_str(&format!(
        "│{:<14}│{:<18}│{:>9}│{:>9}│{:>9}│{:>9}│{:>11}│{:>11}│{:>9}│{:>9}│\n",
        "节点", "运行时", "uptime", "conn", "asks", "tells", "bytes_rx", "bytes_tx", "replies", "errs"
    ));
    for (name, m) in rows {
        out.push_str(&format!(
            "│{:<14}│{:<18}│{:>9}│{:>9}│{:>9}│{:>9}│{:>11}│{:>11}│{:>9}│{:>9}│\n",
            name,
            m.runtime,
            uptime_hms(m.uptime_secs()),
            m.connections,
            m.asks_rx,
            m.tells_rx,
            human_bytes(m.bytes_rx),
            human_bytes(m.bytes_tx),
            m.replies_tx,
            m.reply_errs
        ));
    }
    out.push_str(&format!(
        "└{:-<14}┴{:-<18}┴{:>9}┴{:>9}┴{:>9}┴{:>9}┴{:>11}┴{:>11}┴{:>9}┴{:>9}┘\n",
        "─", "─", "─", "─", "─", "─", "─", "─", "─", "─"
    ));
    // 组件明细
    for (name, m) in rows {
        if !m.component_states.is_empty() {
            out.push_str(&format!("\n[{name}] 组件 ({}):\n", m.component_states.len()));
            for c in &m.component_states {
                out.push_str(&format!(
                    "  {:<40} {:<10} {}\n",
                    c.path, c.state, c.version
                ));
            }
        }
    }
    out
}

/// 两次快照差分 → 速率表（trace 子命令）。
fn rates_table(before: &[(String, MetricsSnapshot)], after: &[(String, MetricsSnapshot)], secs: f64) -> String {
    let mut out = String::from(format!("\n≈ {secs:.1}s 差分速率：\n"));
    out.push_str(&format!(
        "{:<14}{:>12}{:>12}{:>14}{:>14}{:>12}\n",
        "节点", "asks/s", "tells/s", "rx/s", "tx/s", "hb/s"
    ));
    for (n, a) in after {
        let b = before.iter().find(|(bn, _)| bn == n);
        let (asks, tells, rx, tx, hb) = match b {
            Some((_, bm)) => (
                (a.asks_rx.saturating_sub(bm.asks_rx)) as f64 / secs,
                (a.tells_rx.saturating_sub(bm.tells_rx)) as f64 / secs,
                (a.bytes_rx.saturating_sub(bm.bytes_rx)) as f64 / secs,
                (a.bytes_tx.saturating_sub(bm.bytes_tx)) as f64 / secs,
                (a.heartbeats_rx.saturating_sub(bm.heartbeats_rx)) as f64 / secs,
            ),
            None => (0.0, 0.0, 0.0, 0.0, 0.0),
        };
        out.push_str(&format!(
            "{:<14}{:>12.1}{:>12.1}{:>14}{:>14}{:>12.1}\n",
            n, asks, tells, human_bytes(rx as u64), human_bytes(tx as u64), hb
        ));
    }
    out
}

// ───────────────────────── 采集 ─────────────────────────

async fn collect_all(
    client: &Arc<RemoteActorSystem>,
    nodes: &[NodeSpec],
) -> Vec<(String, Result<MetricsSnapshot, String>)> {
    let mut out = Vec::new();
    for n in nodes {
        let r = client.metrics_report(&n.name).await;
        out.push((
            n.name.clone(),
            r.map_err(|e| format!("{e:?}")),
        ));
    }
    out
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        // --name=value 形态
        if let Some(v) = a.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
    }
    None
}

fn flag_num(args: &[String], name: &str, default: u64) -> u64 {
    flag(args, name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

// ───────────────────────── Web 控制台 ─────────────────────────

async fn serve_web(client: Arc<RemoteActorSystem>, nodes: Vec<NodeSpec>, port: u16) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    println!("[probe] Web 控制台 → http://localhost:{port}  (Ctrl-C 停)");
    loop {
        let (mut sock, _) = listener.accept().await?;
        let client = client.clone();
        let nodes = nodes.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let n = match sock.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (status, ctype, body): (&str, &str, String) = match path.as_str() {
                "/api/metrics" => {
                    let rows = collect_all(&client, &nodes).await;
                    let mut arr = serde_json::Map::new();
                    for (name, r) in rows {
                        arr.insert(
                            name,
                            match r {
                                Ok(m) => serde_json::to_value(&m).unwrap(),
                                Err(e) => serde_json::json!({"error": e}),
                            },
                        );
                    }
                    ("200 OK".into(), "application/json".into(), serde_json::to_string(&arr).unwrap())
                }
                "/" | "/index.html" => ("200 OK".into(), "text/html; charset=utf-8".into(), DASHBOARD_HTML.to_string()),
                _ => ("404 Not Found".into(), "text/plain".into(), "not found".into()),
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
    }
}

/// 控制台单页（原生 JS 轮询 /api/metrics——零依赖）。
const DASHBOARD_HTML: &str = r##"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8">
<title>parrot-obs 控制台</title><style>
body{font-family:-apple-system,"PingFang SC",monospace;margin:0;background:#0d1117;color:#c9d1d9}
.hd{padding:14px 24px;border-bottom:1px solid #21262d;display:flex;justify-content:space-between;align-items:center}
.hd h1{font-size:18px;margin:0;color:#58a6ff}
.meta{font-size:12px;color:#8b949e}
table{border-collapse:collapse;width:100%;font-size:13px}
th,td{padding:8px 14px;border-bottom:1px solid #21262d;text-align:left}
th{color:#8b949e;font-weight:600;background:#161b22;position:sticky;top:0}
.ok{color:#3fb950}.err{color:#f85149}.num{color:#79c0ff}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(340px,1fr));gap:14px;padding:14px 24px}
.card{background:#161b22;border:1px solid #21262d;border-radius:8px;padding:14px}
.card h3{margin:0 0 10px;font-size:14px;color:#58a6ff}
.kv{display:flex;justify-content:space-between;font-size:12px;padding:3px 0;border-bottom:1px dashed #21262d}
.kv b{color:#e6edf3;font-weight:500}
.spark{font-size:11px;color:#8b949e;margin-top:8px;min-height:16px}
.comps{font-size:12px;color:#8b949e;padding:2px 0}
.comps code{color:#7ee787;background:#1b2a1f;padding:1px 6px;border-radius:4px;margin-right:6px}
</style></head><body>
<div class="hd"><h1>🦜 parrot-obs</h1><div class="meta" id="meta">connecting…</div></div>
<table id="tbl"><thead><tr><th>节点</th><th>运行时</th><th>uptime</th><th>conn</th><th>asks</th>
<th>tells</th><th>replies</th><th>errs</th><th>rx</th><th>tx</th><th>rss</th><th>速率 asks/s</th></tr></thead>
<tbody id="rows"></tbody></table>
<div class="grid" id="cards"></div>
<script>
let prev={},hist={};
const hz=b=>b>=1<<30?(b/2**30).toFixed(2)+'G':b>=1<<20?(b/2**20).toFixed(1)+'M':b>=1024?(b/1024).toFixed(1)+'K':b+'B';
const hms=s=>Math.floor(s/3600)+'h'+String(Math.floor(s%3600/60)).padStart(2,'0')+'m'+String(s%60).padStart(2,'0')+'s';
async function tick(){
 try{
  const d=await(await fetch('/api/metrics')).json();
  const now=Date.now();
  let rows='',cards='';
  for(const[name,m]of Object.entries(d)){
   if(m.error){rows+=`<tr><td>${name}</td><td colspan="11" class="err">${m.error}</td></tr>`;continue}
   const dt=prev[name]?(now-prev[name].t)/1000:0;
   const rate=dt>0?((m.asks_rx-prev[name].asks)/dt).toFixed(1):'—';
   rows+=`<tr><td><b>${name}</b></td><td>${m.runtime}</td><td>${hms(m.uptime_secs??0)}</td>
    <td class="num">${m.connections}</td><td class="num">${m.asks_rx}</td><td class="num">${m.tells_rx}</td>
    <td class="num">${m.replies_tx}</td><td class="${m.reply_errs>0?'err':'num'}">${m.reply_errs}</td>
    <td class="num">${hz(m.bytes_rx)}</td><td class="num">${hz(m.bytes_tx)}</td><td class="num">${hz(m.memory_rss)}</td>
    <td class="num">${rate}</td></tr>`;
   (hist[name]=hist[name]||[]).push([now,m.asks_rx]);
   hist[name]=hist[name].filter(x=>now-x[0]<120000);
   const sp=hist[name].length>1?spark(name):'';
   const compHtml=(m.component_states||[]).map(c=>`<div class="comps"><code>${c.path}</code>${c.state} ${c.version}</div>`).join('');
   cards+=`<div class="card"><h3>${name} · ${m.runtime}</h3>
    <div class="kv"><span>uptime</span><b>${hms(m.uptime_secs??0)}</b></div>
    <div class="kv"><span>handshakes ok/fail</span><b>${m.handshakes_ok}/${m.handshakes_failed}</b></div>
    <div class="kv"><span>heartbeats_rx</span><b>${m.heartbeats_rx}</b></div>
    <div class="kv"><span>processes</span><b>${m.processes||'—'}</b></div>
    <div class="kv"><span>memory_rss</span><b>${hz(m.memory_rss)}</b></div>
    ${compHtml}<div class="spark" id="sp-${name}">${sp}</div></div>`;
   prev[name]={t:now,asks:m.asks_rx};
  }
  document.getElementById('rows').innerHTML=rows;
  document.getElementById('cards').innerHTML=cards;
  document.getElementById('meta').textContent='每 2s 刷新 · '+new Date().toLocaleTimeString();
 }catch(e){document.getElementById('meta').textContent='断开：'+e}
 setTimeout(tick,2000);
}
function spark(name){
 const h=hist[name];if(h.length<2)return'';
 const w=100,ht=14,t0=h[0][0],t1=h[h.length-1][0];
 const v0=h[0][1],v1=h[h.length-1][1];
 if(t1<=t0||v1<v0)return'';
 const pts=h.map(([t,v])=>`${((t-t0)/(t1-t0)*w).toFixed(1)},${(ht-(v-v0)/Math.max(1,v1-v0)*ht).toFixed(1)}`).join(' ');
 return`<svg width="${w}" height="${ht}" style="vertical-align:middle"><polyline fill="none" stroke="#58a6ff" stroke-width="1.2" points="${pts}"/></svg> asks ${v1-v0}/窗`;
}
tick();
</script></body></html>"##;

// ───────────────────────── main ─────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 日志（RUST_LOG=parrot_remote=debug 可看协议层 warn——排障用）
    if std::env::var("RUST_LOG").is_ok() {
        parrot::logging::init_default(false);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_else(|| "help".into());
    let rest: Vec<String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).cloned().collect();
    let nodes = parse_nodes(&rest);
    if nodes.is_empty() {
        eprintln!("parrot-obs {cmd}：未给节点（erl=… ray=… jvm=… 至少一个）");
        std::process::exit(2);
    }
    let client = connect(&nodes).await?;

    match cmd.as_str() {
        "status" => {
            for (name, r) in collect_all(&client, &nodes).await {
                match r {
                    Ok(m) => println!("✓ {}", status_line(&name, &m)),
                    Err(e) => println!("✗ {name:<14} 不可达：{e}"),
                }
            }
        }
        "metrics" => {
            let mut rows = Vec::new();
            let mut down = Vec::new();
            for (name, r) in collect_all(&client, &nodes).await {
                match r {
                    Ok(m) => rows.push((name, m)),
                    Err(e) => down.push((name, e)),
                }
            }
            print!("{}", metrics_table(&rows));
            for (name, e) in down {
                println!("✗ {name:<14} 不可达：{e}");
            }
        }
        "ping" => {
            let rounds = flag_num(&args, "--rounds", 5);
            for n in &nodes {
                print!("{:<14} ", n.name);
                for _ in 0..rounds {
                    let t = Instant::now();
                    match client.metrics_report(&n.name).await {
                        Ok(_) => print!("{:>5.0}ms ", t.elapsed().as_millis()),
                        Err(e) => print!(" FAIL({e:?}) "),
                    }
                }
                println!();
            }
        }
        "ask" => {
            // debug 探测：bin:u:Ping → 期望各方言 Pong（echo 探针）
            let rounds = flag_num(&args, "--rounds", 5);
            for n in &nodes {
                let path = format!("parrot://{}/{}", n.name, probe_path(&n.name));
                print!("{:<14} ", n.name);
                for i in 0..rounds {
                    let t = Instant::now();
                    match client.remote_ref(&path) {
                        Ok(r) => match r.send(Box::new(ProbePing(i as u64))).await {
                            Ok(_) => print!(" ✓{:>4}ms", t.elapsed().as_millis()),
                            Err(e) => print!(" ERR({e}) "),
                        },
                        Err(e) => print!(" REF({e}) "),
                    }
                }
                println!();
            }
        }
        "watch" => {
            let iv = flag_num(&args, "--interval", 2);
            println!("每 {iv}s 采样（Ctrl-C 停）");
            let mut last: HashMap<String, MetricsSnapshot> = HashMap::new();
            loop {
                let now = Instant::now();
                for (name, r) in collect_all(&client, &nodes).await {
                    if let Ok(m) = r {
                        if let Some(prev) = last.get(&name) {
                            let dt = 2.0;
                            println!(
                                "[{:>9}s] {name:<14} asks={:>6} (+{:.1}/s) replies={} hb={}",
                                now.elapsed().as_secs(),
                                m.asks_rx,
                                (m.asks_rx.saturating_sub(prev.asks_rx)) as f64 / dt,
                                m.replies_tx,
                                m.heartbeats_rx,
                            );
                        }
                        last.insert(name, m);
                    }
                }
                tokio::time::sleep(Duration::from_secs(iv)).await;
            }
        }
        "trace" => {
            let secs = flag_num(&args, "--seconds", 10);
            let before: Vec<_> = collect_all(&client, &nodes)
                .await
                .into_iter()
                .filter_map(|(n, r)| r.ok().map(|m| (n, m)))
                .collect();
            println!("采样 {secs}s（期间请制造负载）…");
            tokio::time::sleep(Duration::from_secs(secs)).await;
            let after: Vec<_> = collect_all(&client, &nodes)
                .await
                .into_iter()
                .filter_map(|(n, r)| r.ok().map(|m| (n, m)))
                .collect();
            print!("{}", rates_table(&before, &after, secs as f64));
        }
        "load" => {
            // 压测：进程内持续 ask（并发 N 路 × 持续 S 秒——吞吐/延迟分布）
            let secs = flag_num(&args, "--seconds", 10);
            let conc = flag_num(&args, "--conc", 4) as usize;
            println!("压测 {secs}s × {conc} 并发（Ctrl-C 提前停）");
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut handles = Vec::new();
            for _w in 0..conc {
                let stop = stop.clone();
                let client = client.clone();
                let nodes = nodes.clone();
                handles.push(tokio::spawn(async move {
                    let mut lat = Vec::new();
                    let mut n_ok = 0u64;
                    let mut n_err = 0u64;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        for nn in &nodes {
                            let path = format!("parrot://{}/{}", nn.name, probe_path(&nn.name));
                            let t = Instant::now();
                            match client.remote_ref(&path) {
                                Ok(r) => match r.send(Box::new(ProbePing(n_ok))).await {
                                    Ok(_) => {
                                        lat.push(t.elapsed().as_micros() as u64);
                                        n_ok += 1;
                                    }
                                    Err(_) => n_err += 1,
                                },
                                Err(_) => n_err += 1,
                            }
                        }
                    }
                    (n_ok, n_err, lat)
                }));
            }
            tokio::time::sleep(Duration::from_secs(secs)).await;
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let (mut tot_ok, mut tot_err, mut all_lat) = (0u64, 0u64, Vec::new());
            for h in handles {
                if let Ok((ok, err, lat)) = h.await {
                    tot_ok += ok;
                    tot_err += err;
                    all_lat.extend(lat);
                }
            }
            all_lat.sort_unstable();
            let p = |q: f64| -> String {
                if all_lat.is_empty() {
                    "—".into()
                } else {
                    format!("{:.2}ms", all_lat[((all_lat.len() as f64 - 1.0) * q) as usize] as f64 / 1000.0)
                }
            };
            println!(
                "完成 ok={} err={} 吞吐={:.1}/s  延迟 P50={} P95={} P99={}",
                tot_ok,
                tot_err,
                tot_ok as f64 / secs as f64,
                p(0.50),
                p(0.95),
                p(0.99)
            );
        }
        "web" => {
            let port = flag_num(&args, "--web-port", 8190) as u16;
            serve_web(client.clone(), nodes, port).await?;
        }
        _ => {
            eprintln!("parrot-obs —— parrot 框架观测套件");
            eprintln!();
            eprintln!("用法：parrot-obs <子命令> erl=<addr> [ray=<addr> jvm=<addr>] [选项]");
            eprintln!();
            eprintln!("子命令：");
            eprintln!("  status    连通性 + 一行摘要（uptime/conn/计数）");
            eprintln!("  metrics   全宽指标表 + 组件明细");
            eprintln!("  ping      admin 通道 RTT 探测（--rounds N，默认 5）");
            eprintln!("  ask       业务通道 ASK 探测（echo 探针 --rounds N）");
            eprintln!("  load      压测（--seconds S --conc N——吞吐+P50/95/99 延迟）");
            eprintln!("  watch     持续采样差分（--interval S，默认 2）");
            eprintln!("  trace     窗口差分吞吐速率（--seconds S，默认 10）");
            eprintln!("  web       Web 控制台（--web-port P，默认 8190）");
            std::process::exit(if cmd == "help" { 0 } else { 2 });
        }
    }
    client.shutdown().await?;
    Ok(())
}
