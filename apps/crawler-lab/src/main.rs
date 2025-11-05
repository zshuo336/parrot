//! parrot-crawler-lab：大规模爬虫 + 索引 + Web 检索 四运行时集成实验室。
//!
//! 场景拓扑（星型全互操作——Rust hub，三被动网关辐射）：
//!
//!   Erlang(OTP/ETS)      Ray(Python)         Akka(JVM)
//!   URL Frontier          索引构建            搜索 API
//!   (IO 密集·海量轻进程)   (CPU 密集·分词)      (高并发短查询)
//!        ↑ push/next          ↑ IndexPage        ↑ IndexTerms
//!        │                    │                  │
//!   ┌────┴────────────────────┴──────────────────┴────┐
//!   │  Parrot/Rust: 爬取 worker 集群 + 路由 hub        │
//!   │  (tokio 并发 fetch + 页面合成 + 编排)             │
//!   └──────────────────────────────────────────────────┘
//!
//! 数据流（两两互动全覆盖）：
//!   Rust → Erlang : FrontierPush/FrontierNext（URL 调度）
//!   Rust → Ray    : IndexPage（HTML → 词频）
//!   Rust → JVM    : IndexTerms（词项 → 倒排）+ Search（top-k）+ Healthz
//!
//! 站点模拟：无外网依赖——确定性伪随机合成"网页"（词表笛卡尔采样），
//! 页面间链接关系由 seed 派生（可复现爬取图）。
//!
//! 用法：
//!   parrot-crawler-lab <erl=host:port> <ray=host:port> <jvm=host:port>
//!                      [--pages N] [--depth D] [--fanout F] [--batch B]
//!   # 缺省 N=500 D=2 F=3 B=32

use std::sync::Arc;
use std::time::Instant;

use parrot_api::address::ActorRef;
use parrot_api::types::BoxedMessage;
use parrot_remote::system::{RemoteActorSystem, RemoteConfig};
use parrot_remote::{LocalLookup, NodeAddr};

// ══════════════════════════════════════════════════════════════════════════
// 线上消息（裸 LE 编码——与三网关方言逐字节对齐）
// ══════════════════════════════════════════════════════════════════════════

/// 出站请求与入站回复都是裸字节（网关方言键）——每键独立新类型防注册冲突
/// （同 TypeId 多键会在注册表互串）。
macro_rules! wire_msg {
    ($t:ident, $key:literal) => {
        wire_msg!($t, $key, |_bytes: &[u8]| Ok::<(), String>(()));
    };
    ($t:ident, $key:literal, $validate:expr) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct $t(pub Vec<u8>);

        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$t>(),
                encode: |msg: &BoxedMessage| {
                    let m = msg.downcast_ref::<$t>().ok_or("downcast fail")?;
                    Ok(m.0.clone())
                },
                decode: |b: &[u8]| {
                    ($validate)(b).map_err(|error| error.to_string())?;
                    Ok(Box::new($t(b.to_vec())) as BoxedMessage)
                },
            }
        }
    };
}

mod application;
mod cli;
mod hub;
use crawler_lab_core::{tokenize, url_of};
mod protocol;

// Batch replies are decoded before entering business code, so a malformed
// suffix cannot turn into a successful prefix or an empty queue indication.
macro_rules! wire_list_msg {
    ($t:ident, $key:literal, $entry:ty, $decode:path, $encode:path) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct $t(pub Vec<$entry>);
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$t>(),
                encode: |msg: &BoxedMessage| {
                    let message = msg.downcast_ref::<$t>().ok_or("wrong reply type")?;
                    Ok($encode(&message.0))
                },
                decode: |bytes: &[u8]| Ok(Box::new($t($decode(bytes).map_err(|e| e.to_string())?)) as BoxedMessage),
            }
        }
    };
}

fn expect_reply<T: Send + 'static>(reply: BoxedMessage) -> Result<T, String> {
    reply.downcast::<T>().map(|value| *value).map_err(|_| {
        format!(
            "unexpected gateway reply; expected {}",
            std::any::type_name::<T>()
        )
    })
}

wire_msg!(FrontierPush, "bin:crawl/FrontierPush");
wire_msg!(FrontierNext, "bin:crawl/FrontierNext");
wire_list_msg!(
    FrontierBatch,
    "bin:crawl/FrontierBatch",
    protocol::FrontierEntry,
    protocol::decode_batch,
    enc_push
);
wire_msg!(FrontierAck, "bin:crawl/FrontierAck", protocol::decode_ack);
wire_msg!(IndexPage, "bin:crawl/IndexPage");
wire_msg!(IndexAck, "bin:crawl/IndexAck", protocol::decode_index_ack);
wire_msg!(IndexStatsQuery, "bin:crawl/IndexStats");
wire_msg!(IndexStatsR, "bin:crawl/IndexStatsR", protocol::decode_stats);
wire_msg!(IndexTerms, "bin:crawl/IndexTerms");
wire_msg!(SearchQuery, "bin:crawl/Search");
wire_msg!(
    SearchResult,
    "bin:crawl/SearchResult",
    protocol::decode_search
);
wire_msg!(Healthz, "bin:crawl/Healthz");
wire_msg!(HealthzR, "bin:crawl/HealthzR", protocol::decode_health);

// ══════════════════════════════════════════════════════════════════════════
// 确定性站点模拟
// ══════════════════════════════════════════════════════════════════════════

// ══════════════════════════════════════════════════════════════════════════
// LE 编解码（网关方言）
// ══════════════════════════════════════════════════════════════════════════

/// push 批次：[n u32][{id u64|len u32|url|depth u16}...]
fn enc_push(entries: &[(u64, String, u16)]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (id, url, depth) in entries {
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(&(url.len() as u32).to_le_bytes());
        b.extend_from_slice(url.as_bytes());
        b.extend_from_slice(&depth.to_le_bytes());
    }
    b
}

/// IndexPage 批次：[n u32][{doc u64|len u32|html}...]
fn enc_pages(pages: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    for (doc, html) in pages {
        b.extend_from_slice(&doc.to_le_bytes());
        b.extend_from_slice(&(html.len() as u32).to_le_bytes());
        b.extend_from_slice(html);
    }
    b
}

/// IndexTerms 批次：[n u32][{len u32|term|doc u64|tf u32}...]
fn enc_terms(entries: &[(String, u64, u32)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (term, doc, tf) in entries {
        b.extend_from_slice(&(term.len() as u32).to_le_bytes());
        b.extend_from_slice(term.as_bytes());
        b.extend_from_slice(&doc.to_le_bytes());
        b.extend_from_slice(&tf.to_le_bytes());
    }
    b
}

/// Search 批次：[k u32][{len u32|term}...]
fn enc_query(terms: &[&str]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(terms.len() as u32).to_le_bytes());
    for t in terms {
        b.extend_from_slice(&(t.len() as u32).to_le_bytes());
        b.extend_from_slice(t.as_bytes());
    }
    b
}

// ══════════════════════════════════════════════════════════════════════════
// main
// ══════════════════════════════════════════════════════════════════════════

struct NoopLookup;

#[async_trait::async_trait]
impl LocalLookup for NoopLookup {
    async fn lookup(&self, _path: &str) -> Option<Box<dyn ActorRef>> {
        None
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match cli::parse(&args) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{}", cli::USAGE);
            return;
        }
        Err(error) => {
            eprintln!("crawler-lab: {error}\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("crawler-lab: runtime startup: {error}");
            std::process::exit(1);
        }
    };
    let cli::Options {
        mode,
        pages,
        depth,
        fanout,
        batch,
        hub,
        workload_overrides,
    } = options;
    let workload = crawler_lab_core::Config {
        pages,
        depth,
        fanout,
        batch,
    };
    let bind = match &mode {
        cli::Mode::Registry { bind, .. } => Some(*bind),
        _ => None,
    };
    let config = RemoteConfig::tcp("crawler-lab", bind).with_insecure_development();
    let code = runtime.block_on(async {
        match mode {
            cli::Mode::Manifest {
                path,
                bind,
                query,
                wait_secs,
            } => {
                application::run(
                    &path,
                    bind,
                    query,
                    wait_secs,
                    workload,
                    workload_overrides,
                    hub,
                )
                .await
            }
            cli::Mode::Registry { wait_secs, .. } => {
                run_registry(config, wait_secs, workload, hub).await
            }
            cli::Mode::Direct(targets) => run_direct(config, targets, workload, hub).await,
        }
    });
    std::process::exit(code);
}

struct Metrics {
    t0: Instant,
    pushed: std::sync::atomic::AtomicU64,
    fetched: std::sync::atomic::AtomicU64,
    indexed_ray: std::sync::atomic::AtomicU64,
    indexed_jvm: std::sync::atomic::AtomicU64,
    terms_total: std::sync::atomic::AtomicU64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            t0: Instant::now(),
            pushed: Default::default(),
            fetched: Default::default(),
            indexed_ray: Default::default(),
            indexed_jvm: Default::default(),
            terms_total: Default::default(),
        }
    }
    fn progress(&self, phase: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        println!(
            "[lab {:>5}s] {phase:<14} pushed={:>5} fetched={:>5} ray_pages={:>5} jvm_terms={:>7}",
            self.t0.elapsed().as_secs(),
            self.pushed.load(Relaxed),
            self.fetched.load(Relaxed),
            self.indexed_ray.load(Relaxed),
            self.indexed_jvm.load(Relaxed),
        );
    }
}

async fn start_client(config: RemoteConfig) -> Result<Arc<RemoteActorSystem>, String> {
    let client = RemoteActorSystem::new(config, Arc::new(NoopLookup))
        .map_err(|error| format!("remote configuration: {error}"))?;
    if let Err(error) = client.start().await {
        let _ = client.shutdown().await;
        return Err(format!("remote startup: {error}"));
    }
    Ok(client)
}

/// 模式 A：direct——应用主动连三网关（显式地址；测试形态）。
async fn run_direct(
    config: RemoteConfig,
    targets: [cli::Endpoint; 3],
    workload: crawler_lab_core::Config,
    hub: cli::Hub,
) -> i32 {
    let client = match start_client(config).await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("crawler-lab: {error}");
            return 1;
        }
    };
    for (identity, target) in ["erl-gw-1", "ray-gw-1", "jvm-search-1"]
        .into_iter()
        .zip(targets)
    {
        let resolved = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::lookup_host((target.host.as_str(), target.port)),
        )
        .await;
        let addresses = match resolved {
            Ok(Ok(addresses)) => {
                let mut seen = std::collections::HashSet::new();
                Ok(addresses
                    .filter(|address| seen.insert(*address))
                    .collect::<Vec<_>>())
            }
            Ok(Err(error)) => Err(format!("resolve {}:{}: {error}", target.host, target.port)),
            Err(error) => Err(format!(
                "resolve {}:{} deadline: {error}",
                target.host, target.port
            )),
        };
        let addresses = match addresses {
            Ok(addresses) => addresses,
            Err(error) => {
                eprintln!("crawler-lab: {error}");
                let _ = client.shutdown().await;
                return 1;
            }
        };
        let mut connected = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        for address in addresses {
            let node = NodeAddr::tcp(identity, address);
            match tokio::time::timeout_at(deadline, client.connect(&node)).await {
                Ok(Ok(_)) => {
                    println!("[lab] connected: {identity}");
                    connected = true;
                    break;
                }
                Ok(Err(error)) => {
                    eprintln!("crawler-lab: connect {identity} at {address}: {error}")
                }
                Err(error) => {
                    eprintln!("crawler-lab: connect {identity} deadline: {error}");
                    break;
                }
            }
        }
        if !connected {
            let _ = client.shutdown().await;
            return 1;
        }
    }
    println!("[lab] 组网模式：direct（应用主动拨号网关）");
    run_scenario(&client, workload, hub).await
}

/// 模式 B：registry——应用只监听，网关主动拨号注册（生产形态）。
///
/// 拓扑：应用 = parrot 节点（bind 公网地址）；erl/ray/jvm 网关启动参数
/// 指向应用地址主动 connect；框架 accept 侧同样把对端 node_id/sender 写入
/// NodeTable + links——应用等三网关就绪后开始（零网关地址知识）。
async fn run_registry(
    config: RemoteConfig,
    wait_secs: u64,
    workload: crawler_lab_core::Config,
    hub: cli::Hub,
) -> i32 {
    let client = match start_client(config).await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("crawler-lab: {error}");
            return 1;
        }
    };
    let Some(local) = client.local_addr() else {
        eprintln!("crawler-lab: registry listener has no bound address");
        let _ = client.shutdown().await;
        return 1;
    };
    println!("[lab] 组网模式：registry（应用监听 {local}，等待网关注册…）");
    // The launcher consumes this record from this process's owned output.
    // It proves local listener binding, not that gateways have registered yet.
    println!("PARROT_CRAWLER_PORT={}", local.port());

    // Addresses learned from seeds or gossip are not gateway registrations.
    let need = ["erl-gw-1", "ray-gw-1", "jvm-search-1"];
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
    loop {
        let missing = client.missing_direct_connections(&need);
        if tokio::time::Instant::now() >= deadline {
            eprintln!("crawler-lab: 等待网关注册超时（{wait_secs}s，缺少直连：{missing:?}）");
            let _ = client.shutdown().await;
            return 1;
        }
        if missing.is_empty() {
            println!("[lab] 三网关已全部注册：{need:?}");
            break;
        }
        tokio::time::sleep_until(
            deadline.min(tokio::time::Instant::now() + std::time::Duration::from_millis(100)),
        )
        .await;
    }
    run_scenario(&client, workload, hub).await
}

async fn run_scenario(
    client: &Arc<RemoteActorSystem>,
    workload: crawler_lab_core::Config,
    form: cli::Hub,
) -> i32 {
    let mut errors = Vec::new();
    match hub::BusinessHub::new(form, workload.clone()).await {
        Ok(mut hub) => {
            if let Err(error) = run_scenario_inner(
                client,
                workload.pages,
                workload.depth,
                workload.fanout,
                workload.batch,
                &mut hub,
            )
            .await
            {
                errors.push(error);
            }
            if let Err(error) = hub.close().await {
                errors.push(error);
            }
        }
        Err(error) => errors.push(error),
    }
    if let Err(error) = client.shutdown().await {
        errors.push(error.to_string());
    }
    if errors.is_empty() {
        println!("\ncrawler-lab PASS（四运行时全链集成）");
        0
    } else {
        eprintln!("crawler-lab: {}", errors.join("; "));
        1
    }
}

async fn run_scenario_inner(
    client: &Arc<RemoteActorSystem>,
    pages: u64,
    max_depth: u16,
    fanout: u64,
    batch: usize,
    hub: &mut hub::BusinessHub,
) -> Result<(), String> {
    // node_id 必须与各网关握手自报一致（erl: erl-gw-1 / ray: ray-gw-1 / jvm: jvm-search-1）
    let frontier = client
        .remote_ref("parrot://erl-gw-1/user/frontier")
        .unwrap();
    let indexer = client.remote_ref("parrot://ray-gw-1/user/index").unwrap();
    let searcher = client
        .remote_ref("parrot://jvm-search-1/jvm/user/search")
        .unwrap();

    // ── 阶段 -1：组件部署（R1——业务已迁 apps/crawler-lab，网关为纯宿主；
    //    应用连网后经 admin-v2 deploy 动态载入三组件制品）──────────────
    use parrot_remote::admin_v2::{AdminArtifactRef, AdminInstancePolicy, ComponentDeploy};
    let app_root = {
        // 制品根 = 仓库内 apps/crawler-lab（cargo run 相对路径稳定锚）
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop(); // apps/
        p.pop(); // 仓库根
        p.join("apps/crawler-lab")
    };
    let deploys = [
        (
            "erl-gw-1",
            "frontier",
            AdminArtifactRef::Beam {
                app: "frontier".into(),
                uri: Some(format!("file://{}", app_root.join("erlang").display())),
            },
        ),
        (
            "ray-gw-1",
            "index",
            AdminArtifactRef::PyModule {
                module: "index_builder".into(),
                runtime_env: None,
                uri: Some(format!("file://{}", app_root.join("python").display())),
            },
        ),
        (
            "jvm-search-1",
            "search",
            AdminArtifactRef::Jvm {
                main_class: "crawler.search.SearchComponent".into(),
                coords: None,
                uri: Some(format!(
                    "file://{}",
                    app_root
                        .join("jvm/target/crawler-lab-jvm-1.0.0.jar")
                        .display()
                )),
            },
        ),
    ];
    for (node, name, artifact) in deploys {
        let t0 = Instant::now();
        let instances = client
            .deploy_component(
                node,
                ComponentDeploy {
                    name: name.into(),
                    version: "1.0.0".into(),
                    artifact,
                    instances: AdminInstancePolicy::Singleton,
                    config: None,
                },
            )
            .await
            .map_err(|e| format!("deploy {node}/{name}: {e}"))?;
        println!(
            "[lab] 组件部署 {node}/{name} → {instances:?}（{}ms）",
            t0.elapsed().as_millis()
        );
    }

    run_business(
        &frontier,
        &indexer,
        &searcher,
        &crawler_lab_core::Config {
            pages,
            depth: max_depth,
            fanout,
            batch,
        },
        hub,
    )
    .await
    .map(|_| ())
}

async fn run_business(
    frontier: &dyn ActorRef,
    indexer: &dyn ActorRef,
    searcher: &dyn ActorRef,
    workload: &crawler_lab_core::Config,
    hub: &mut hub::BusinessHub,
) -> Result<Vec<protocol::Hit>, String> {
    let crawler_lab_core::Config {
        pages,
        depth: max_depth,
        fanout,
        batch,
    } = *workload;
    let m = Arc::new(Metrics::new());

    // ── 阶段 0：三网关方言 sanity（爬虫前的 wire 通路证明）────────────
    println!("[lab] === 阶段 0：网关方言 sanity ===");
    // Erlang Ping: +3 方言
    {
        let mut p = vec![];
        p.extend_from_slice(&0u32.to_le_bytes()); // N=0 空探针（不消耗队列）
        let r = frontier.send(Box::new(FrontierNext(p))).await;
        match r {
            Ok(reply) => {
                let FrontierBatch(entries) = expect_reply::<FrontierBatch>(reply)?;
                println!("[lab] erl 网关 alive（FrontierBatch n={}）", entries.len());
            }
            Err(e) => return Err(format!("erl 探活失败: {e:?}")),
        }
    }
    // Ray IndexPage has an eight-byte acknowledgement; JVM IndexTerms uses
    // the same key with a distinct four-byte shape. Check the selected dialect.
    {
        let reply = indexer
            .send(Box::new(IndexPage(enc_pages(&[]))))
            .await
            .map_err(|e| format!("ray empty index probe: {e}"))?;
        expect_ray_ack(reply, 0, 0)?;
        println!("[lab] ray 网关 alive（IndexAck(0,0)）");
    }
    {
        let health = read_health(searcher).await?;
        if health
            != (protocol::Health {
                terms: 0,
                postings: 0,
                queries: 0,
            })
        {
            return Err(format!("new JVM component is not empty: {health:?}"));
        }
        println!(
            "[lab] jvm 网关 alive: {}",
            serde_json::to_string(&health).map_err(|e| e.to_string())?
        );
    }

    // ── 阶段 1：种子注入 Erlang frontier ─────────────────────────────
    println!("[lab] === 阶段 1：种子注入 frontier（pages={pages}）===");
    let t1 = Instant::now();
    let seeds: Vec<(u64, String, u16)> = (0..pages.min(batch as u64 * 2))
        .map(|id| (id, url_of(id), 0u16))
        .collect();
    let reply = frontier
        .send(Box::new(FrontierPush(enc_push(&seeds))))
        .await
        .map_err(|e| format!("frontier seed push: {e}"))?;
    expect_frontier_ack(reply)?;
    m.pushed
        .fetch_add(seeds.len() as u64, std::sync::atomic::Ordering::Relaxed);
    println!("[lab] 种子 {} 条已注入（{:?}）", seeds.len(), t1.elapsed());

    // ── 阶段 2：爬取循环（fetch → 出链回注 → 双路索引）────────────────
    println!("[lab] === 阶段 2：爬取循环（depth≤{max_depth} fanout={fanout}）===");
    let t2 = Instant::now();
    let mut pages_buf: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut terms_buf: Vec<(String, u64, u32)> = Vec::new();
    // 对账基准：Rust 侧累计的去重词表（与 ray/jvm 两路索引必须收敛一致）
    let mut reference_index: std::collections::HashMap<
        String,
        std::collections::HashMap<u64, u32>,
    > = Default::default();
    let mut last_progress = Instant::now();

    loop {
        // 运行状态监控：每 2s 打点（长跑场景观测爬取速率/索引进度）
        if last_progress.elapsed() >= std::time::Duration::from_secs(2) {
            m.progress("crawling");
            last_progress = Instant::now();
        }
        // frontier 取批
        let mut q = vec![];
        q.extend_from_slice(&(batch as u32).to_le_bytes());
        let r = frontier
            .send(Box::new(FrontierNext(q)))
            .await
            .map_err(|e| format!("frontier next: {e}"))?;
        let FrontierBatch(batch_urls) = expect_reply::<FrontierBatch>(r)?;
        if batch_urls.is_empty() {
            break;
        }
        // Actual component business: synthesis, term frequencies, dedup and
        // outlink filtering happen inside Tick (native/Wasm/embedded kernel).
        let tick = hub.tick(&batch_urls).await?;
        for (term, id, tf) in &tick.terms {
            reference_index
                .entry(term.clone())
                .or_default()
                .insert(*id, *tf);
        }
        m.fetched.fetch_add(
            tick.pages.len() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        pages_buf.extend(tick.pages);
        terms_buf.extend(tick.terms);
        // 缓冲满 → 双路索引（ray: CPU 分词统计；jvm: 倒插入库）
        if pages_buf.len() >= batch {
            flush_index(indexer, searcher, &mut pages_buf, &mut terms_buf, &m).await?;
        }
        // Network effects remain host-owned; the component supplies the batch.
        if !tick.outlinks.is_empty() {
            let reply = frontier
                .send(Box::new(FrontierPush(enc_push(&tick.outlinks))))
                .await
                .map_err(|e| format!("frontier outlink push: {e}"))?;
            expect_frontier_ack(reply)?;
            m.pushed.fetch_add(
                tick.outlinks.len() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }
    // 收尾 flush
    flush_index(indexer, searcher, &mut pages_buf, &mut terms_buf, &m).await?;
    println!(
        "[lab] 爬取完成：fetched={}（去重后）pushed_total={} 耗时 {:?}",
        m.fetched.load(std::sync::atomic::Ordering::Relaxed),
        m.pushed.load(std::sync::atomic::Ordering::Relaxed),
        t2.elapsed()
    );

    // ── 阶段 3：双路索引对账（ray 词频视角 vs jvm 倒排视角）───────────
    println!("[lab] === 阶段 3：索引对账 ===");
    let unique_terms_expected = reference_index.len() as u64;
    let postings_expected: u64 = reference_index.values().map(|docs| docs.len() as u64).sum();
    {
        let reply = indexer
            .send(Box::new(IndexStatsQuery(vec![])))
            .await
            .map_err(|e| format!("ray index stats: {e}"))?;
        let IndexStatsR(raw) = expect_reply(reply)?;
        let (terms, postings) = protocol::decode_stats(&raw).map_err(|e| e.to_string())?;
        if u64::from(terms) != unique_terms_expected || postings != postings_expected {
            return Err(format!("Ray index mismatch: ({terms}, {postings}), expected ({unique_terms_expected}, {postings_expected})"));
        }
        println!("[lab] ray 索引: terms={terms} postings={postings}");
        let health = read_health(searcher).await?;
        if health
            != (protocol::Health {
                terms: unique_terms_expected,
                postings: postings_expected,
                queries: 0,
            })
        {
            return Err(format!("JVM index mismatch: {health:?}"));
        }
        println!(
            "[lab] jvm 倒排: {}",
            serde_json::to_string(&health).map_err(|e| e.to_string())?
        );
    }

    // ── 阶段 4：搜索验证（JVM top-k）──────────────────────────────────
    println!("[lab] === 阶段 4：搜索验证 ===");
    let queries: Vec<Vec<&str>> = vec![
        vec!["parrot"],
        vec!["actor", "cluster"],
        vec!["crawler", "index", "search"],
        vec!["raft", "swim"],
    ];
    for q in &queries {
        let reply = searcher
            .send(Box::new(SearchQuery(enc_query(q))))
            .await
            .map_err(|e| format!("JVM search: {e}"))?;
        let SearchResult(raw) = expect_reply(reply)?;
        let hits = protocol::decode_search(&raw)?;
        let expected = reference_search(&reference_index, q);
        if hits != expected {
            return Err(format!(
                "search {q:?} mismatch: {hits:?}, expected {expected:?}"
            ));
        }
        println!(
            "[lab] search {q:?} → {}",
            serde_json::to_string(&hits).map_err(|e| e.to_string())?
        );
    }

    let health = read_health(searcher).await?;
    if health
        != (protocol::Health {
            terms: unique_terms_expected,
            postings: postings_expected,
            queries: queries.len() as u64,
        })
    {
        return Err(format!("final JVM health mismatch: {health:?}"));
    }
    println!(
        "[lab] jvm healthz: {}",
        serde_json::to_string(&health).map_err(|e| e.to_string())?
    );

    // 终态指标
    m.progress("done");
    Ok(reference_search(&reference_index, &["parrot", "actor"]))
}

fn expect_frontier_ack(reply: BoxedMessage) -> Result<(), String> {
    let FrontierAck(raw) = expect_reply(reply)?;
    if protocol::decode_ack(&raw).map_err(|e| e.to_string())? != 1 {
        return Err("frontier did not acknowledge push".into());
    }
    Ok(())
}

fn expect_ray_ack(reply: BoxedMessage, pages: u32, terms: u32) -> Result<(), String> {
    let IndexAck(raw) = expect_reply(reply)?;
    if protocol::decode_index_ack(&raw).map_err(|e| e.to_string())?
        != (protocol::IndexAcknowledgement::Ray { pages, terms })
    {
        return Err(format!(
            "Ray IndexAck has wrong shape/count; expected ({pages}, {terms})"
        ));
    }
    Ok(())
}

fn expect_jvm_ack(reply: BoxedMessage, count: u32) -> Result<(), String> {
    let IndexAck(raw) = expect_reply(reply)?;
    if protocol::decode_index_ack(&raw).map_err(|e| e.to_string())?
        != protocol::IndexAcknowledgement::Jvm(count)
    {
        return Err(format!(
            "JVM IndexAck has wrong shape/count; expected {count}"
        ));
    }
    Ok(())
}

async fn read_health(searcher: &dyn ActorRef) -> Result<protocol::Health, String> {
    let reply = searcher
        .send(Box::new(Healthz(vec![])))
        .await
        .map_err(|e| format!("JVM health: {e}"))?;
    let HealthzR(raw) = expect_reply(reply)?;
    protocol::decode_health(&raw)
}

fn reference_search(
    index: &std::collections::HashMap<String, std::collections::HashMap<u64, u32>>,
    query: &[&str],
) -> Vec<protocol::Hit> {
    let mut scores = std::collections::HashMap::<i64, u64>::new();
    for term in query {
        if let Some(docs) = index.get(*term) {
            for (doc, frequency) in docs {
                *scores.entry(*doc as i64).or_default() += u64::from(*frequency);
            }
        }
    }
    let mut hits: Vec<_> = scores
        .into_iter()
        .map(|(doc, score)| protocol::Hit { doc, score })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.doc.cmp(&b.doc)));
    hits.truncate(5); // SearchComponent's configured top-k.
    hits
}

async fn flush_index(
    indexer: &dyn ActorRef,
    searcher: &dyn ActorRef,
    pages: &mut Vec<(u64, Vec<u8>)>,
    terms: &mut Vec<(String, u64, u32)>,
    m: &Arc<Metrics>,
) -> Result<(), String> {
    use std::sync::atomic::Ordering::Relaxed;
    if pages.is_empty() && terms.is_empty() {
        return Ok(());
    }
    // ray：整页 HTML（CPU 分词在 ray 侧做）
    if !pages.is_empty() {
        let n = pages.len() as u64;
        let expected_words: usize = pages.iter().map(|(_, html)| tokenize(html).len()).sum();
        let expected_pages = u32::try_from(pages.len()).map_err(|_| "page batch too large")?;
        let expected_words = u32::try_from(expected_words).map_err(|_| "token count too large")?;
        let reply = indexer
            .send(Box::new(IndexPage(enc_pages(pages))))
            .await
            .map_err(|e| format!("Ray index batch: {e}"))?;
        expect_ray_ack(reply, expected_pages, expected_words)?;
        m.indexed_ray.fetch_add(n, Relaxed);
        pages.clear();
    }
    // jvm：Rust 侧词频 → 倒排条目
    if !terms.is_empty() {
        let n = terms.len() as u64;
        let expected = u32::try_from(terms.len()).map_err(|_| "term batch too large")?;
        let reply = searcher
            .send(Box::new(IndexTerms(enc_terms(terms))))
            .await
            .map_err(|e| format!("JVM index batch: {e}"))?;
        expect_jvm_ack(reply, expected)?;
        m.indexed_jvm.fetch_add(n, Relaxed);
        m.terms_total.fetch_add(n, Relaxed);
        terms.clear();
    }
    Ok(())
}

#[cfg(test)]
mod protocol_tests;
