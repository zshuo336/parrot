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
                    Ok(Box::new($t(b.to_vec())) as BoxedMessage)
                },
            }
        }
    };
}

wire_msg!(FrontierPush, "bin:crawl/FrontierPush");
wire_msg!(FrontierNext, "bin:crawl/FrontierNext");
wire_msg!(FrontierBatch, "bin:crawl/FrontierBatch");
wire_msg!(FrontierAck, "bin:crawl/FrontierAck");
wire_msg!(IndexPage, "bin:crawl/IndexPage");
wire_msg!(IndexAck, "bin:crawl/IndexAck");
wire_msg!(IndexStatsQuery, "bin:crawl/IndexStats");
wire_msg!(IndexStatsR, "bin:crawl/IndexStatsR");
wire_msg!(IndexTerms, "bin:crawl/IndexTerms");
wire_msg!(SearchQuery, "bin:crawl/Search");
wire_msg!(SearchResult, "bin:crawl/SearchResult");
wire_msg!(Healthz, "bin:crawl/Healthz");
wire_msg!(HealthzR, "bin:crawl/HealthzR");

// ══════════════════════════════════════════════════════════════════════════
// 确定性站点模拟
// ══════════════════════════════════════════════════════════════════════════

/// xorshift64* —— 确定性伪随机（同 seed 同站点）
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

const VOCAB: &[&str] = &[
    "parrot",
    "actor",
    "cluster",
    "federate",
    "wire",
    "gateway",
    "swim",
    "gossip",
    "raft",
    "shard",
    "mailbox",
    "supervise",
    "router",
    "receptionist",
    "durable",
    "backpressure",
    "quic",
    "mtls",
    "relay",
    "border",
    "digest",
    "twin",
    "topology",
    "latency",
    "throughput",
    "crawler",
    "index",
    "search",
    "frontier",
    "tokenize",
    "posting",
    "inverted",
    "query",
];

/// 页面 id → URL（确定性：doc-{id}）
fn url_of(id: u64) -> String {
    format!("https://site.example/doc-{id}")
}

/// 页面 id → 合成 HTML（词表确定性采样——文档间词分布有差异，可检索）
fn synth_page(id: u64) -> Vec<u8> {
    let mut rng = Rng(id.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    let n_words = 40 + (rng.next() % 60) as usize;
    let title = VOCAB[(id as usize) % VOCAB.len()];
    let mut body = String::new();
    body.push_str(&format!(
        "<html><head><title>{title} page</title></head><body>"
    ));
    for _ in 0..n_words {
        let w = VOCAB[(rng.next() as usize) % VOCAB.len()];
        body.push_str(w);
        body.push(' ');
    }
    body.push_str("</body></html>");
    body.into_bytes()
}

/// 页面外链（fanout 条，id 空间内确定性跳转）
fn outlinks(id: u64, fanout: u64, max_id: u64) -> Vec<u64> {
    let mut rng = Rng(id.wrapping_mul(0xBF58476D1CE4E5B9) | 1);
    (0..fanout)
        .map(|_| rng.next() % max_id)
        .filter(|&t| t != id)
        .collect()
}

/// 分词（与 Ray 侧 _tokenize 同语义——Rust 侧用于本地对账）
fn tokenize(html: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(html).to_lowercase();
    let clean: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect();
    const STOP: &[&str] = &[
        "the", "a", "an", "of", "to", "in", "and", "or", "for", "on", "with", "at", "by", "is",
        "it", "as", "be", "html", "head", "title", "page", "body",
    ];
    clean
        .split_whitespace()
        .filter(|t| t.len() > 1 && !STOP.contains(t))
        .map(String::from)
        .collect()
}

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

/// next 批次回复：[n u32][{id u64|len u32|url|depth u16}...]
fn dec_batch(b: &[u8]) -> Vec<(u64, String, u16)> {
    let mut out = Vec::new();
    if b.len() < 4 {
        return out;
    }
    let n = u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize;
    let mut off = 4;
    for _ in 0..n {
        if off + 14 > b.len() {
            break;
        }
        let id = u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
        let ln = u32::from_le_bytes(b[off + 8..off + 12].try_into().unwrap()) as usize;
        off += 12;
        if off + ln + 2 > b.len() {
            break;
        }
        let url = String::from_utf8_lossy(&b[off..off + ln]).into_owned();
        off += ln;
        let depth = u16::from_le_bytes(b[off..off + 2].try_into().unwrap());
        off += 2;
        out.push((id, url, depth));
    }
    out
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
    let mut targets = Vec::new();
    let (mut pages, mut depth, mut fanout, mut batch) = (500u64, 2u16, 3u64, 32usize);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pages" => {
                pages = args[i + 1].parse().unwrap();
                i += 2
            }
            "--depth" => {
                depth = args[i + 1].parse().unwrap();
                i += 2
            }
            "--fanout" => {
                fanout = args[i + 1].parse().unwrap();
                i += 2
            }
            "--batch" => {
                batch = args[i + 1].parse().unwrap();
                i += 2
            }
            _ => {
                if let Some((id, addr)) = args[i].split_once('=') {
                    let sa = addr.parse().unwrap_or_else(|_| {
                        use std::net::ToSocketAddrs;
                        addr.to_socket_addrs()
                            .ok()
                            .and_then(|mut it| it.next())
                            .unwrap_or_else(|| panic!("无法解析目标: {addr}"))
                    });
                    targets.push(NodeAddr::tcp(id, sa));
                }
                i += 1
            }
        }
    }
    let erl = targets
        .iter()
        .find(|t| t.node_id == "erl")
        .cloned()
        .unwrap_or_else(|| panic!("需 erl=host:port"));
    let ray = targets
        .iter()
        .find(|t| t.node_id == "ray")
        .cloned()
        .unwrap_or_else(|| panic!("需 ray=host:port"));
    let jvm = targets
        .iter()
        .find(|t| t.node_id == "jvm")
        .cloned()
        .unwrap_or_else(|| panic!("需 jvm=host:port"));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let code = rt.block_on(run(&erl, &ray, &jvm, pages, depth, fanout, batch));
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

async fn run(
    erl: &NodeAddr,
    ray: &NodeAddr,
    jvm: &NodeAddr,
    pages: u64,
    max_depth: u16,
    fanout: u64,
    batch: usize,
) -> i32 {
    let client =
        RemoteActorSystem::new(RemoteConfig::tcp("crawler-lab", None), Arc::new(NoopLookup))
            .unwrap();
    client.start().await.unwrap();
    for t in [erl, ray, jvm] {
        match client.connect(t).await {
            Ok(_) => println!("[lab] connected: {}", t.node_id),
            Err(e) => {
                eprintln!("[lab] connect {} FAILED: {e}", t.node_id);
                return 1;
            }
        }
    }

    // node_id 必须与各网关握手自报一致（erl: erl-gw-1 / ray: ray-gw-1 / jvm: jvm-search-1）
    let frontier = client
        .remote_ref("parrot://erl-gw-1/user/frontier")
        .unwrap();
    let indexer = client.remote_ref("parrot://ray-gw-1/user/echo").unwrap();
    let searcher = client
        .remote_ref("parrot://jvm-search-1/jvm/user/search")
        .unwrap();

    let m = Arc::new(Metrics::new());

    // ── 阶段 0：三网关方言 sanity（爬虫前的 wire 通路证明）────────────
    println!("[lab] === 阶段 0：网关方言 sanity ===");
    // Erlang Ping: +3 方言
    {
        let mut p = vec![];
        p.extend_from_slice(&0u32.to_le_bytes()); // N=0 空探针（不消耗队列）
        let r = frontier.send(Box::new(FrontierNext(p))).await;
        match r {
            Ok(reply) => println!(
                "[lab] erl 网关 alive（FrontierBatch n={}）",
                u32::from_le_bytes(
                    reply.downcast_ref::<FrontierBatch>().unwrap().0[0..4]
                        .try_into()
                        .unwrap()
                )
            ),
            Err(e) => panic!("erl 探活失败: {e:?}"),
        }
    }
    // Ray Ping: +2 方言（IndexPage 空批次探活——IndexAck(0,0)）
    {
        let r = indexer
            .send(Box::new(IndexPage(enc_pages(&[]))))
            .await
            .unwrap();
        let raw = r.downcast_ref::<IndexAck>().unwrap().0.clone();
        let (a, b) = (
            u32::from_le_bytes(raw[0..4].try_into().unwrap()),
            u32::from_le_bytes(raw[4..8].try_into().unwrap()),
        );
        assert_eq!((a, b), (0, 0), "ray 空批次 IndexAck(0,0)");
        println!("[lab] ray 网关 alive（IndexAck(0,0)）");
    }
    // JVM Healthz
    {
        let r = searcher.send(Box::new(Healthz(vec![]))).await.unwrap();
        let json = String::from_utf8_lossy(&r.downcast_ref::<HealthzR>().unwrap().0).into_owned();
        println!("[lab] jvm 网关 alive: {json}");
    }

    // ── 阶段 1：种子注入 Erlang frontier ─────────────────────────────
    println!("[lab] === 阶段 1：种子注入 frontier（pages={pages}）===");
    let t1 = Instant::now();
    let seeds: Vec<(u64, String, u16)> = (0..pages.min(batch as u64 * 2))
        .map(|id| (id, url_of(id), 0u16))
        .collect();
    frontier
        .send(Box::new(FrontierPush(enc_push(&seeds))))
        .await
        .unwrap();
    m.pushed
        .fetch_add(seeds.len() as u64, std::sync::atomic::Ordering::Relaxed);
    println!("[lab] 种子 {} 条已注入（{:?}）", seeds.len(), t1.elapsed());

    // ── 阶段 2：爬取循环（fetch → 出链回注 → 双路索引）────────────────
    println!("[lab] === 阶段 2：爬取循环（depth≤{max_depth} fanout={fanout}）===");
    let t2 = Instant::now();
    let mut visited: std::collections::HashSet<u64> = Default::default();
    let mut pending_outlinks: Vec<(u64, String, u16)> = Vec::new();
    let mut pages_buf: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut terms_buf: Vec<(String, u64, u32)> = Vec::new();
    // 对账基准：Rust 侧累计的去重词表（与 ray/jvm 两路索引必须收敛一致）
    let mut all_terms: std::collections::HashSet<String> = Default::default();
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
        let r = frontier.send(Box::new(FrontierNext(q))).await.unwrap();
        let batch_urls = dec_batch(&r.downcast_ref::<FrontierBatch>().unwrap().0);
        if batch_urls.is_empty() {
            break;
        }
        // fetch（合成页面）+ 出链收集 + 索引缓冲
        for (id, _url, depth) in batch_urls {
            if !visited.insert(id) {
                continue;
            }
            let html = synth_page(id);
            // 本地分词对账用词频 + JVM 倒排条目
            let mut freq: std::collections::HashMap<String, u32> = Default::default();
            for t in tokenize(&html) {
                *freq.entry(t).or_insert(0) += 1;
            }
            for (term, tf) in &freq {
                terms_buf.push((term.clone(), id, *tf));
                all_terms.insert(term.clone());
            }
            pages_buf.push((id, html));
            m.fetched.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // 出链（深度受限回注）
            if depth < max_depth {
                for t in outlinks(id, fanout, pages) {
                    pending_outlinks.push((t, url_of(t), depth + 1));
                }
            }
        }
        // 缓冲满 → 双路索引（ray: CPU 分词统计；jvm: 倒插入库）
        if pages_buf.len() >= batch {
            flush_index(&indexer, &searcher, &mut pages_buf, &mut terms_buf, &m).await;
        }
        // 出链批量回注 frontier
        if !pending_outlinks.is_empty() {
            // 只回注未访问过的（去重交给 ets seen——但减少无用流量）
            let chunk: Vec<_> = pending_outlinks
                .iter()
                .filter(|(id, _, _)| !visited.contains(id))
                .cloned()
                .collect();
            pending_outlinks.clear();
            if !chunk.is_empty() {
                frontier
                    .send(Box::new(FrontierPush(enc_push(&chunk))))
                    .await
                    .unwrap();
                m.pushed
                    .fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    // 收尾 flush
    flush_index(&indexer, &searcher, &mut pages_buf, &mut terms_buf, &m).await;
    println!(
        "[lab] 爬取完成：fetched={}（去重后）pushed_total={} 耗时 {:?}",
        m.fetched.load(std::sync::atomic::Ordering::Relaxed),
        m.pushed.load(std::sync::atomic::Ordering::Relaxed),
        t2.elapsed()
    );

    // ── 阶段 3：双路索引对账（ray 词频视角 vs jvm 倒排视角）───────────
    println!("[lab] === 阶段 3：索引对账 ===");
    let unique_terms_expected = all_terms.len() as u64;
    {
        // ray stats：IndexStats 键空载 → [terms u32][postings u64]
        let r = indexer
            .send(Box::new(IndexStatsQuery(vec![])))
            .await
            .unwrap();
        let raw = &r.downcast_ref::<IndexStatsR>().unwrap().0;
        let terms = u32::from_le_bytes(raw[0..4].try_into().unwrap());
        let postings = u64::from_le_bytes(raw[4..12].try_into().unwrap());
        let expected_terms = unique_terms_expected;
        println!(
            "[lab] ray 索引: terms={terms} postings={postings}（本地对账 unique_terms={expected_terms}）"
        );
        assert_eq!(terms as u64, expected_terms, "ray 词表数与 Rust 对账一致");
        // jvm healthz：terms/postings/queries
        let r = searcher.send(Box::new(Healthz(vec![]))).await.unwrap();
        let json = String::from_utf8_lossy(&r.downcast_ref::<HealthzR>().unwrap().0).into_owned();
        println!("[lab] jvm 倒排: {json}");
        let jvm_terms = extract_json_u64(&json, "terms");
        assert_eq!(
            jvm_terms, expected_terms,
            "jvm 倒排词数与 ray/Rust 三方一致"
        );
        let jvm_postings = extract_json_u64(&json, "postings");
        assert_eq!(
            jvm_postings, postings,
            "jvm postings 与 ray postings 一致（双路索引收敛）"
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
        let r = searcher
            .send(Box::new(SearchQuery(enc_query(q))))
            .await
            .unwrap();
        let json =
            String::from_utf8_lossy(&r.downcast_ref::<SearchResult>().unwrap().0).into_owned();
        println!("[lab] search {:?} → {}", q, json);
        assert!(!json.is_empty(), "搜索结果非空");
    }

    // healthz 终态
    let r = searcher.send(Box::new(Healthz(vec![]))).await.unwrap();
    let json = String::from_utf8_lossy(&r.downcast_ref::<HealthzR>().unwrap().0).into_owned();
    println!("[lab] jvm healthz: {json}");

    // 终态指标
    m.progress("done");
    client.shutdown().await.ok();
    println!("\ncrawler-lab PASS（四运行时全链集成）");
    0
}

/// 简易 JSON u64 抽取（healthz 返回 {"terms":N,"postings":M,"queries":K}）
fn extract_json_u64(json: &str, key: &str) -> u64 {
    let pat = format!("\"{key}\":");
    let i = json
        .find(&pat)
        .unwrap_or_else(|| panic!("json 缺 {key}: {json}"));
    let rest = &json[i + pat.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap()
}

async fn flush_index(
    indexer: &parrot_remote::RemoteActorRef,
    searcher: &parrot_remote::RemoteActorRef,
    pages: &mut Vec<(u64, Vec<u8>)>,
    terms: &mut Vec<(String, u64, u32)>,
    m: &Arc<Metrics>,
) {
    use std::sync::atomic::Ordering::Relaxed;
    if pages.is_empty() && terms.is_empty() {
        return;
    }
    // ray：整页 HTML（CPU 分词在 ray 侧做）
    if !pages.is_empty() {
        let n = pages.len() as u64;
        indexer
            .send(Box::new(IndexPage(enc_pages(pages))))
            .await
            .unwrap();
        m.indexed_ray.fetch_add(n, Relaxed);
        pages.clear();
    }
    // jvm：Rust 侧词频 → 倒排条目
    if !terms.is_empty() {
        let n = terms.len() as u64;
        searcher
            .send(Box::new(IndexTerms(enc_terms(terms))))
            .await
            .unwrap();
        m.indexed_jvm.fetch_add(n, Relaxed);
        m.terms_total.fetch_add(n, Relaxed);
        terms.clear();
    }
}
