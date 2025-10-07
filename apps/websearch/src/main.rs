//! websearch：真实搜索引擎应用（五运行时分工——同 crawler-lab 原则）。
//!
//! 用户四诉求（真实化）：
//!   1. 真实网页漫爬（reqwest HTTPS + seed URL + BFS + robots 礼貌策略）
//!   2. 索引落盘（akka 侧段文件 seg-*.segment——重启回放不丢）
//!   3. 四服务独立（erl frontier / rust crawler / ray 分词索引 / akka 检索）
//!   4. 浏览器百度式查询页（本进程内置 HTTP → akka Search）
//!
//! 五运行时分工：
//!   Erlang  frontier   URL 去重 + BFS 调度（ETS 有序表——调度天生形态）
//!   Rust    crawler    tokio 并发真实抓取 + HTML 抽链/抽文 + 编排 + Web 页
//!   Ray     tokenizer  jieba 中文分词 + 词频（CPU 密集）
//!   Akka    search     倒排 + BM25 + 段落盘 + 查询（高并发短查询）
//!
//! 组网：复用 parrot 真实子进程网关（erl escript / python ray / jvm akka），
//! 组件经 admin-v2 DeployComponent 动态载入（R4 闭环——与 crawler-lab 同）。

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parrot_api::address::ActorRef;
use parrot_api::message::inventory;
use parrot_api::types::BoxedMessage;
use parrot_remote::admin_v2::{AdminArtifactRef, AdminInstancePolicy, ComponentDeploy};
use parrot_remote::system::{RemoteActorSystem, RemoteConfig};
use parrot_remote::{LocalLookup, NodeAddr};
use sha2::{Digest, Sha256};

// ═══════════════════════════ wire 消息（bin:ws/* 键空间）══════════════════

macro_rules! wire_msg {
    ($t:ident, $key:literal) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct $t(pub Vec<u8>);

        inventory::submit! {
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

wire_msg!(WsPush, "bin:ws/Push");
wire_msg!(WsPushAck, "bin:ws/PushAck");
wire_msg!(WsNext, "bin:ws/Next");
wire_msg!(WsBatch, "bin:ws/Batch");
wire_msg!(WsSize, "bin:ws/Size");
wire_msg!(WsSizeR, "bin:ws/SizeR");
wire_msg!(WsTokenize, "bin:ws/Tokenize");
wire_msg!(WsTerms, "bin:ws/Terms");
wire_msg!(WsIndexStats, "bin:ws/IndexStats");
wire_msg!(WsIndexStatsR, "bin:ws/IndexStatsR");
wire_msg!(WsIndexTerms, "bin:ws/IndexTerms");
wire_msg!(WsIndexAck, "bin:ws/IndexAck");
wire_msg!(WsDocMeta, "bin:ws/DocMeta");
wire_msg!(WsDocAck, "bin:ws/DocAck");
wire_msg!(WsSearch, "bin:ws/Search");
wire_msg!(WsSearchResult, "bin:ws/SearchResult");
wire_msg!(WsFlush, "bin:ws/Flush");
wire_msg!(WsFlushAck, "bin:ws/FlushAck");
wire_msg!(WsHealthz, "bin:ws/Healthz");
wire_msg!(WsHealthzR, "bin:ws/HealthzR");

// ═══════════════════════════ 编解码（裸 LE）══════════════════════════════

fn put_u32(b: &mut Vec<u8>, n: u32) {
    b.extend_from_slice(&n.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, n: u64) {
    b.extend_from_slice(&n.to_le_bytes());
}
fn get_u32(b: &[u8], off: &mut usize) -> u32 {
    let v = u32::from_le_bytes(b[*off..*off + 4].try_into().unwrap());
    *off += 4;
    v
}
fn get_u64(b: &[u8], off: &mut usize) -> u64 {
    let v = u64::from_le_bytes(b[*off..*off + 8].try_into().unwrap());
    *off += 8;
    v
}
fn put_str(b: &mut Vec<u8>, s: &str) {
    put_u32(b, s.len() as u32);
    b.extend_from_slice(s.as_bytes());
}
fn get_str<'a>(b: &'a [u8], off: &mut usize) -> &'a str {
    let n = get_u32(b, off) as usize;
    let s = std::str::from_utf8(&b[*off..*off + n]).unwrap_or("");
    *off += n;
    s
}

/// docId = URL sha256 前 8 字节（稳定——重爬幂等）。
fn doc_id_of(url: &str) -> u64 {
    let d = Sha256::digest(url.as_bytes());
    u64::from_le_bytes(d[0..8].try_into().unwrap())
}

/// Push 载荷：[n u32][{len u32|url|depth u16}...]
fn enc_push(urls: &[(String, u16)]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 + urls.len() * 32);
    put_u32(&mut b, urls.len() as u32);
    for (u, d) in urls {
        put_str(&mut b, u);
        b.extend_from_slice(&d.to_le_bytes()); // depth u16
    }
    b
}

/// Batch 解码：[n u32][{len u32|url|depth u16}...]
fn dec_batch(b: &[u8]) -> Vec<(String, u16)> {
    let mut off = 0;
    let n = get_u32(b, &mut off) as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let url = get_str(b, &mut off).to_string();
        let depth = u16::from_le_bytes(b[off..off + 2].try_into().unwrap());
        off += 2;
        out.push((url, depth));
    }
    out
}

/// Tokenize 载荷：[n u32][{docid u64|len u32|text}...]
fn enc_texts(pages: &[(u64, &str)]) -> Vec<u8> {
    let mut b = Vec::new();
    put_u32(&mut b, pages.len() as u32);
    for (id, text) in pages {
        put_u64(&mut b, *id);
        put_str(&mut b, text);
    }
    b
}

/// Terms 解码 → IndexTerms 载荷（透传词条布局一致：[{len|term|docid|tf}]）
fn dec_terms(b: &[u8]) -> Vec<(String, u64, u32)> {
    let mut off = 0;
    let n = get_u32(b, &mut off) as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let term = get_str(b, &mut off).to_string();
        let doc = get_u64(b, &mut off);
        let tf = get_u32(b, &mut off);
        out.push((term, doc, tf));
    }
    out
}

fn enc_terms(entries: &[(String, u64, u32)]) -> Vec<u8> {
    let mut b = Vec::new();
    put_u32(&mut b, entries.len() as u32);
    for (term, doc, tf) in entries {
        put_str(&mut b, term);
        put_u64(&mut b, *doc);
        put_u32(&mut b, *tf);
    }
    b
}

/// DocMeta 载荷：[docid u64|len url|len title|len text]
fn enc_doc_meta(doc: u64, url: &str, title: &str, text: &str) -> Vec<u8> {
    let mut b = Vec::new();
    put_u64(&mut b, doc);
    put_str(&mut b, url);
    put_str(&mut b, title);
    // 摘要源文本截 4KB（akka 侧再裁 280 字符）
    put_str(&mut b, &text.chars().take(4096).collect::<String>());
    b
}

/// Search 载荷：[k u32|len u32|query]
fn enc_search(k: u32, q: &str) -> Vec<u8> {
    let mut b = Vec::new();
    put_u32(&mut b, k);
    put_str(&mut b, q);
    b
}

// ═══════════════════════════ HTML 抽取（零依赖）══════════════════════════

pub struct Extracted {
    pub title: String,
    pub text: String,
    pub links: Vec<String>,
}

fn extract_html(html: &str) -> Extracted {
    // title
    let title = html
        .split_once("<title")
        .and_then(|(_, r)| r.split_once('>').map(|(_, t)| t))
        .and_then(|t| t.split_once("</title").map(|(x, _)| x))
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    // links（href 粗提取——中文站常见单双引号混用）
    let mut links = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("href") {
        let after = &rest[i + 4..];
        let after = after.trim_start();
        if let Some(a) = after.strip_prefix('=') {
            let a = a.trim_start();
            let quote = a.chars().next();
            let link = match quote {
                Some(q @ ('"' | '\'')) => a[1..].split(q).next().unwrap_or(""),
                _ => a.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or(""),
            };
            if !link.is_empty()
                && !link.starts_with("javascript")
                && !link.starts_with("mailto")
                && !link.starts_with('#')
            {
                links.push(link.to_string());
            }
        }
        rest = &rest[i + 4..];
    }

    // 正文（剥标签 + script/style 剔除 + 实体还原）
    let mut text = String::with_capacity(html.len() / 2);
    let mut drop_tag: Option<String> = None;
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < html.len() {
        if bytes[i] == b'<' {
            if let Some(close) = html[i..].find('>') {
                let tag = html[i + 1..i + close].trim().to_lowercase();
                let name: String = tag.chars().take_while(|c| c.is_alphabetic()).collect();
                match name.as_str() {
                    "script" | "style" | "noscript" => drop_tag = Some(name),
                    _ if tag.starts_with(&format!("/{}", drop_tag.clone().unwrap_or_default())) => {
                        drop_tag = None
                    }
                    "p" | "div" | "br" | "li" | "h1" | "h2" | "h3" | "tr" => text.push(' '),
                    _ => {}
                }
                i += close + 1;
            } else {
                break;
            }
        } else {
            if drop_tag.is_none() {
                text.push(html[i..].chars().next().unwrap());
            }
            i += html[i..].chars().next().unwrap().len_utf8();
        }
    }
    let text = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    Extracted { title, text, links }
}

/// URL 规范化（相对解析 + 去 fragment + 去 utm 跟踪参数 + 尾斜杠归一）。
fn normalize_url(base: &str, link: &str) -> Option<String> {
    let abs = if link.starts_with("http://") || link.starts_with("https://") {
        link.to_string()
    } else if link.starts_with("//") {
        format!("https:{}", link)
    } else if link.starts_with('/') {
        let (scheme, rest) = base.split_once("://")?;
        let host = rest.split('/').next()?;
        format!("{}://{}{}", scheme, host, link)
    } else if link.starts_with("..") || !link.contains("://") {
        // 相对路径——逐段解析
        let (scheme, rest) = base.split_once("://")?;
        let mut parts = rest.splitn(2, '/');
        let host = parts.next()?;
        let path = parts.next().unwrap_or("");
        let mut segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for seg in link.split('/') {
            match seg {
                "." | "" => {}
                ".." => {
                    segs.pop();
                }
                s => segs.push(s),
            }
        }
        format!("{}://{}/{}", scheme, host, segs.join("/"))
    } else {
        return None;
    };
    let (scheme, rest) = abs.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (hostport, pathq) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = hostport.to_lowercase();
    let (path, query) = match pathq.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (pathq, None),
    };
    let path = if path.len() > 1 && path.ends_with('/') {
        &path[..path.len() - 1]
    } else {
        path
    };
    let query = query.map(|q| {
        let kept: Vec<&str> = q
            .split('&')
            .filter(|kv| {
                let k = kv.split('=').next().unwrap_or("").to_lowercase();
                !k.starts_with("utm_") && k != "fbclid" && k != "gclid"
            })
            .collect();
        if kept.is_empty() {
            String::new()
        } else {
            format!("?{}", kept.join("&"))
        }
    });
    Some(format!("{}://{}{}{}", scheme, host, path, query.unwrap_or_default()))
}

fn host_of(url: &str) -> String {
    url.split_once("://")
        .map(|(_, r)| r.split('/').next().unwrap_or("").to_lowercase())
        .unwrap_or_default()
}

// ═══════════════════════════ HTTP 爬客（reqwest）══════════════════════════

struct Fetcher {
    client: reqwest::Client,
    robots: tokio::sync::Mutex<HashMap<String, Vec<String>>>, // host → disallow 前缀
}

impl Fetcher {
    fn new() -> Self {
    let client = reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
        )
        .timeout(Duration::from_secs(12))
            .connect_timeout(Duration::from_secs(6))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("reqwest client");
        Self { client, robots: Default::default() }
    }

    async fn robots_allowed(&self, url: &str) -> bool {
        let host = host_of(url);
        let mut cache = self.robots.lock().await;
        let rules = if let Some(r) = cache.get(&host) {
            r.clone()
        } else {
            let rules = match self
                .client
                .get(format!("https://{host}/robots.txt"))
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => match resp.text().await {
                    Ok(body) => parse_robots(&body),
                    Err(_) => vec![],
                },
                _ => vec![], // 无 robots = 全允许
            };
            cache.insert(host.clone(), rules.clone());
            rules
        };
        let path = url
            .split_once("://")
            .and_then(|(_, r)| r.find('/').map(|i| &r[i..]))
            .unwrap_or("/");
        !rules.iter().any(|r| path.starts_with(r.as_str()))
    }

    async fn fetch(&self, url: &str) -> Result<(String, String), String> {
        let resp = self
            .client
            .get(url)
            .header("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
            .send()
            .await
            .map_err(|e| format!("req {e}"))?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(format!("http {status}"));
        }
        // Content-Type 检查（非 HTML 跳过——css/js/图片不爬）
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        if !ct.contains("html") && !ct.contains("xml") && !ct.is_empty() {
            return Err(format!("ct {ct}"));
        }
        // charset 探测（中文站常见 gbk/gb2312）
        let ct_charset = ct
            .split(';')
            .find_map(|p| p.trim().strip_prefix("charset="))
            .map(|s| s.trim_matches('"').to_lowercase());
        let raw = resp.bytes().await.map_err(|e| format!("body {e}"))?;
        // 截断 1MB
        let raw = &raw[..raw.len().min(1 << 20)];
        let body = match ct_charset.as_deref() {
            Some("gbk") | Some("gb2312") | Some("gb18030") => decode_gbk(raw),
            _ => {
                // HTML meta 探测
                let head = String::from_utf8_lossy(&raw[..raw.len().min(2048)]).to_lowercase();
                if head.contains("charset=gbk") || head.contains("charset=gb2312") {
                    decode_gbk(raw)
                } else {
                    String::from_utf8_lossy(raw).into_owned()
                }
            }
        };
        Ok((body, ct))
    }
}

fn decode_gbk(bytes: &[u8]) -> String {
    // GBK → UTF-8：iconv 系统调用（macOS/Linux 通用的兜底；失败降级 lossy）
    use std::process::{Command, Stdio};
    let child = Command::new("iconv")
        .args(["-f", "GBK", "-t", "UTF-8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn();
    match child {
        Ok(mut c) => {
            if let Some(mut si) = c.stdin.take() {
                let _ = si.write_all(bytes);
            }
            match c.wait_with_output() {
                Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
                _ => String::from_utf8_lossy(bytes).into_owned(),
            }
        }
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn parse_robots(body: &str) -> Vec<String> {
    let mut in_star = false;
    let mut dis = Vec::new();
    for raw in body.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_lowercase(), v.trim());
            match k.as_str() {
                "user-agent" => in_star = v == "*",
                "disallow" if in_star && !v.is_empty() => dis.push(v.to_string()),
                _ => {}
            }
        }
    }
    dis
}

// ═══════════════════════════ main：编排 ══════════════════════════════════

struct NoopLookup;
#[async_trait::async_trait]
impl LocalLookup for NoopLookup {
    async fn lookup(&self, _path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut seeds: Vec<String> = Vec::new();
    let (mut pages, mut max_depth, mut web_port) = (200u64, 2u16, 8080u16);
    let mut data_dir = PathBuf::from("./data");
    let mut gw_addrs: Vec<(&str, String)> = Vec::new(); // (tag, host:port)
    let mut serve_only = false; // 只起检索服务（不爬——重启后回放索引的独立运行形态）

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pages" => {
                pages = args[i + 1].parse().unwrap();
                i += 2;
            }
            "--maxdepth" => {
                max_depth = args[i + 1].parse().unwrap();
                i += 2;
            }
            "--port" => {
                web_port = args[i + 1].parse().unwrap();
                i += 2;
            }
            "--data" => {
                data_dir = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--serve-only" => {
                serve_only = true;
                i += 1;
            }
            "--spawn-gateways" => {
                i += 1;
            }
            s if s.split_once('=').is_some_and(|(t, _)| matches!(t, "erl" | "ray" | "jvm")) => {
                let (tag, addr) = s.split_once('=').unwrap();
                gw_addrs.push((tag, addr.to_string()));
                i += 1;
            }
            s if s.starts_with("--") => {
                eprintln!("未知参数 {s}");
                i += 1;
            }
            s => {
                seeds.push(s.to_string());
                i += 1;
            }
        }
    }
    if seeds.is_empty() && !serve_only {
        eprintln!(
            "用法：websearch <seed-url>... [--pages N] [--maxdepth D] [--port P] [--data DIR] [--serve-only]\n\
             示例：websearch https://www.runoob.com --pages 200\n\
                   websearch --serve-only --port 8080（只起检索——回放已落盘索引）"
        );
        std::process::exit(2);
    }
    // 网关缺省（run.sh 前置拉起——同 crawler-lab direct 模式端口）
    let erl_addr = gw_addr(&gw_addrs, "erl", "127.0.0.1:19871");
    let ray_addr = gw_addr(&gw_addrs, "ray", "127.0.0.1:19873");
    let jvm_addr = gw_addr(&gw_addrs, "jvm", "127.0.0.1:19872");

    std::fs::create_dir_all(&data_dir).unwrap();
    let dedupe_path = data_dir.join("dedupe.tsv");
    let mut dedupe: HashSet<String> = HashSet::new();
    if dedupe_path.exists() {
        let txt = std::fs::read_to_string(&dedupe_path).unwrap_or_default();
        for l in txt.lines() {
            if !l.trim().is_empty() {
                dedupe.insert(l.trim().to_string());
            }
        }
        println!("[ws] 恢复去重表：{} URLs", dedupe.len());
    }

    // ── 组网：连三网关 ─────────────────────────────────────────────
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("websearch", None),
        Arc::new(NoopLookup),
    )
    .unwrap();
    client.start().await.unwrap();
    if serve_only {
        let sa = jvm_addr.parse().unwrap();
        client
            .connect(&NodeAddr::tcp("jvm-search-1", sa))
            .await
            .expect("connect jvm-search-1");
        println!("[ws] connected: jvm-search-1（serve-only）");
    } else {
        for (id, addr) in [
            ("erl-gw-1", &erl_addr),
            ("ray-gw-1", &ray_addr),
            ("jvm-search-1", &jvm_addr),
        ] {
            let sa = addr.parse().unwrap();
            client.connect(&NodeAddr::tcp(id, sa)).await.expect("connect");
            println!("[ws] connected: {id}");
        }
    }

    // ── 组件部署（R4 闭环——app 制品动态载入三网关）────────────────
    let app_root = app_root_dir();
    let deploy = |node: &str, name: &str, artifact: AdminArtifactRef| {
        let client = client.clone();
        let node = node.to_string();
        let name = name.to_string();
        async move {
            client
                .deploy_component(
                    &node,
                    ComponentDeploy {
                        name,
                        version: "1.0.0".into(),
                        artifact,
                        instances: AdminInstancePolicy::Singleton,
                        config: None,
                    },
                )
                .await
                .unwrap_or_else(|e| panic!("deploy {node}: {e:?}"))
        }
    };
    let deploys: Vec<(String, AdminArtifactRef)> = if serve_only {
        vec![(
            "jvm-search-1".into(),
            AdminArtifactRef::Jvm {
                main_class: "websearch.search.SearchComponent".into(),
                coords: None,
                uri: Some(format!(
                    "file://{}",
                    app_root.join("jvm/target/websearch-jvm-1.0.0.jar").display()
                )),
            },
        )]
    } else {
        vec![
            (
                "erl-gw-1".into(),
                AdminArtifactRef::Beam {
                    app: "frontier".into(),
                    uri: Some(format!("file://{}", app_root.join("erlang").display())),
                },
            ),
            (
                "ray-gw-1".into(),
                AdminArtifactRef::PyModule {
                    module: "tokenizer".into(),
                    runtime_env: None,
                    uri: Some(format!("file://{}", app_root.join("python").display())),
                },
            ),
            (
                "jvm-search-1".into(),
                AdminArtifactRef::Jvm {
                    main_class: "websearch.search.SearchComponent".into(),
                    coords: None,
                    uri: Some(format!(
                        "file://{}",
                        app_root.join("jvm/target/websearch-jvm-1.0.0.jar").display()
                    )),
                },
            ),
        ]
    };
    // akka 组件的数据目录经环境变量（WS_DATA）——网关进程继承
    for (node, artifact) in deploys {
        let name = match &artifact {
            AdminArtifactRef::Beam { .. } => "frontier",
            AdminArtifactRef::PyModule { .. } => "tokenizer",
            _ => "search",
        };
        let r = deploy(&node, name, artifact).await;
        println!("[ws] deploy {node}/{name} → {r:?}");
    }

    let searcher = client.remote_ref("parrot://jvm-search-1/jvm/user/search").unwrap();
    if serve_only {
        // 只起检索：回放段文件后直接开 Web 服务（搜集/索引/检索各自独立——用户裁定 3）
        if let Ok(r) = searcher.send(Box::new(WsHealthz(vec![]))).await {
            let json = String::from_utf8_lossy(&r.downcast_ref::<WsHealthzR>().unwrap().0);
            println!("[ws] serve-only 回放索引：{json}");
        }
        println!("[ws] 浏览器打开 http://localhost:{web_port} 开始搜索（serve-only）");
        serve_web(web_port, client.clone(), searcher).await;
        return;
    }
    let frontier = client.remote_ref("parrot://erl-gw-1/user/frontier").unwrap();
    let tokenizer = client.remote_ref("parrot://ray-gw-1/user/tokenizer").unwrap();

    // ── 种子注入（去重表前置过滤）─────────────────────────────────
    let seed_pairs: Vec<(String, u16)> = seeds
        .iter()
        .filter_map(|s| normalize_url(s, s))
        .map(|u| (u, 0u16))
        .collect();
    {
        let mut dedupe_out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&dedupe_path)
            .unwrap();
        for (u, _) in &seed_pairs {
            if dedupe.insert(u.clone()) {
                let _ = writeln!(dedupe_out, "{u}");
            }
        }
    }
    frontier
        .send(Box::new(WsPush(enc_push(&seed_pairs))))
        .await
        .unwrap();
    println!("[ws] 种子 {} 条已注入", seed_pairs.len());

    // ── 漫爬主循环（并发抓取 + 双路索引 + 出链回注）──────────────────
    let fetcher = Arc::new(Fetcher::new());
    let fetched = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let mut pending: Vec<(String, u16)> = Vec::new();
    let mut in_flight: usize = 0;
    let concurrency = 8usize;
    let mut host_last: HashMap<String, Instant> = HashMap::new();
    let mut terms_buf: Vec<(String, u64, u32)> = Vec::new();
    let mut pages_buf: Vec<(u64, String)> = Vec::new();
    let t0 = Instant::now();
    let mut last_log = Instant::now();

    loop {
        // 进度打点
        if last_log.elapsed() >= Duration::from_secs(3) {
            println!(
                "[ws {:>4}s] fetched={} fail={} pending={} inflight={}",
                t0.elapsed().as_secs(),
                fetched.load(Ordering::Relaxed),
                failed.load(Ordering::Relaxed),
                pending.len(),
                in_flight
            );
            last_log = Instant::now();
        }
        // 预算完成 → 终局
        let done = fetched.load(Ordering::Relaxed) + failed.load(Ordering::Relaxed);
        if done >= pages && in_flight == 0 {
            break;
        }
        // 补批（frontier 取批 + pending 汇流）
        if pending.len() < concurrency * 2
            && (done + in_flight as u64 + pending.len() as u64) < pages * 2
        {
            match frontier
                .send(Box::new(WsNext({
                    let mut b = vec![];
                    put_u32(&mut b, 16);
                    b
                })))
                .await
            {
                Ok(r) => {
                    let batch = dec_batch(&r.downcast_ref::<WsBatch>().unwrap().0);
                    pending.extend(batch);
                }
                Err(e) => {
                    eprintln!("[ws] frontier next 失败：{e:?}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
        // 派发并发抓取（每域 ≥200ms 间隔 + robots）
        let mut launched = Vec::new();
        let mut throttled = 0usize;
        while in_flight < concurrency && !pending.is_empty() && throttled < pending.len() {
            let (url, depth) = pending.remove(0);
            if depth > max_depth {
                continue;
            }
            let host = host_of(&url);
            let now = Instant::now();
            let ok_to_go = match host_last.get(&host) {
                Some(t) if now.duration_since(*t) < Duration::from_millis(200) => false,
                _ => true,
            };
            if !ok_to_go {
                pending.push((url, depth)); // 队尾重试
                throttled += 1;
                continue;
            }
            throttled = 0;
            host_last.insert(host, now);
            let f = fetcher.clone();
            let url_c = url.clone();
            let fetched_c = fetched.clone();
            let failed_c = failed.clone();
            in_flight += 1;
            launched.push(tokio::spawn(async move {
                if !f.robots_allowed(&url_c).await {
                    failed_c.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                match f.fetch(&url_c).await {
                    Ok((body, _ct)) => {
                        fetched_c.fetch_add(1, Ordering::Relaxed);
                        Some((url_c, depth, body))
                    }
                    Err(e) => {
                        failed_c.fetch_add(1, Ordering::Relaxed);
                        eprintln!("[ws] fetch fail {url_c}: {e}");
                        None
                    }
                }
            }));
        }
        if launched.is_empty() && in_flight == 0 {
            if pending.is_empty() {
                break; // 队列耗尽
            }
            // 全队同域节流中——等窗口
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        // join 全部本轮任务（并发已在 spawn 时形成——顺序 join 只等结果）
        if !launched.is_empty() {
            in_flight -= launched.len();
            let mut results = Vec::with_capacity(launched.len());
            for t in launched {
                if let Ok(v) = t.await {
                    results.push(v);
                }
            }
            for res in results.into_iter().flatten() {
                let (url, depth, body) = res;
                let ex = extract_html(&body);
                let doc_id = doc_id_of(&url);
                // 出链（规范化 + 深度限 + 去重表）
                let mut new_links = Vec::new();
                {
                    let mut dedupe_out = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&dedupe_path)
                        .unwrap();
                    for l in ex.links.iter().take(80) {
                        if let Some(nu) = normalize_url(&url, l) {
                            if dedupe.insert(nu.clone()) {
                                let _ = writeln!(dedupe_out, "{nu}");
                                if depth < max_depth {
                                    new_links.push((nu, depth + 1));
                                }
                            }
                        }
                    }
                }
                if !new_links.is_empty() {
                    frontier
                        .send(Box::new(WsPush(enc_push(&new_links))))
                        .await
                        .ok();
                }
                // 双路索引：ray 分词（jieba）→ akka 倒排 + doc 元数据
                pages_buf.push((doc_id, ex.text.clone()));
                let meta = enc_doc_meta(doc_id, &url, &ex.title, &ex.text);
                let _ = searcher.send(Box::new(WsDocMeta(meta))).await;
                // 缓冲满批量分词
                if pages_buf.len() >= 8 {
                    let texts: Vec<(u64, &str)> = pages_buf
                        .iter()
                        .map(|(id, t)| (*id, t.as_str()))
                        .collect();
                    match tokenizer
                        .send(Box::new(WsTokenize(enc_texts(&texts))))
                        .await
                    {
                        Ok(r) => {
                            let entries = dec_terms(&r.downcast_ref::<WsTerms>().unwrap().0);
                            if !entries.is_empty() {
                                let _ = searcher
                                    .send(Box::new(WsIndexTerms(enc_terms(&entries))))
                                    .await;
                            }
                            terms_buf.extend(entries);
                        }
                        Err(e) => eprintln!("[ws] tokenize 失败：{e:?}"),
                    }
                    pages_buf.clear();
                }
            }
        }
    }
    // 尾批 flush
    if !pages_buf.is_empty() {
        let texts: Vec<(u64, &str)> = pages_buf.iter().map(|(id, t)| (*id, t.as_str())).collect();
        if let Ok(r) = tokenizer
            .send(Box::new(WsTokenize(enc_texts(&texts))))
            .await
        {
            let entries = dec_terms(&r.downcast_ref::<WsTerms>().unwrap().0);
            if !entries.is_empty() {
                let _ = searcher
                    .send(Box::new(WsIndexTerms(enc_terms(&entries))))
                    .await;
            }
            terms_buf.extend(entries);
        }
    }
    println!(
        "[ws] 爬取完成：fetched={} fail={} 耗时 {:?}",
        fetched.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed),
        t0.elapsed()
    );

    // ── 段落盘（索引持久化——重启不丢）──────────────────────────────
    match searcher.send(Box::new(WsFlush(vec![]))).await {
        Ok(r) => {
            let segs = get_u32(&r.downcast_ref::<WsFlushAck>().unwrap().0, &mut 0);
            println!("[ws] 索引段落盘：{segs} 段");
        }
        Err(e) => eprintln!("[ws] flush 失败（索引可能未落盘）：{e:?}"),
    }
    // ray 侧统计（对账）
    match tokenizer.send(Box::new(WsIndexStats(vec![]))).await {
        Ok(r) => {
            let b = &r.downcast_ref::<WsIndexStatsR>().unwrap().0;
            let mut off = 0;
            let terms = get_u32(b, &mut off);
            let postings = get_u64(b, &mut off);
            println!("[ws] ray 分词统计：terms={terms} postings={postings}");
        }
        Err(e) => eprintln!("[ws] ray stats 失败：{e:?}"),
    }
    // akka 侧健康
    if let Ok(r) = searcher.send(Box::new(WsHealthz(vec![]))).await {
        let json = String::from_utf8_lossy(&r.downcast_ref::<WsHealthzR>().unwrap().0);
        println!("[ws] akka 索引：{json}");
    }

    // ── Web 查询服务（百度式——浏览器 http://localhost:port）─────────
    println!("[ws] 浏览器打开 http://localhost:{web_port} 开始搜索");
    serve_web(web_port, client.clone(), searcher).await;
}

/// 简易并发 join：等全部完成取首个 Some（简化——批量小）。
#[allow(dead_code)]
async fn futures_buffered(
    tasks: Vec<tokio::task::JoinHandle<Option<(String, u16, String)>>>,
) -> (Option<(String, u16, String)>, Vec<tokio::task::JoinHandle<Option<(String, u16, String)>>>) {
    // 顺序 join（批量 ≤8——顺序足够；返回剩余空）
    let mut first = None;
    for t in tasks {
        if let Ok(Some(v)) = t.await {
            if first.is_none() {
                first = Some(v);
            }
        }
    }
    (first, Vec::new())
}

fn gw_addr(addrs: &[(&str, String)], tag: &str, default: &str) -> String {
    addrs
        .iter()
        .find(|(t, _)| *t == tag)
        .map(|(_, a)| a.clone())
        .unwrap_or_else(|| default.to_string())
}

fn app_root_dir() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p
}

// ═══════════════════════════ Web 查询服务（百度式页面）════════════════════

async fn serve_web(
    port: u16,
    _client: Arc<RemoteActorSystem>,
    searcher: parrot_remote::RemoteActorRef,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind(("0.0.0.0", port)).await.expect("bind web port");
    let query_count = Arc::new(AtomicU64::new(0));
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let searcher = searcher.clone();
        let qc = query_count.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let n = match sock.read(&mut buf).await {
                Ok(n) if n > 0 => n,
                _ => return,
            };
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let first = req.lines().next().unwrap_or("");
            // 请求行：METHOD PATH?QUERY PROTO —— 先剥协议尾巴
            let target = first.split_whitespace().nth(1).unwrap_or("/");
            let (path, query) = target
                .split_once('?')
                .map(|(p, q)| (p.to_string(), q.to_string()))
                .unwrap_or_else(|| (target.to_string(), String::new()));
            let params: HashMap<String, String> = query
                .split('&')
                .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), url_decode(v))))
                .collect();
            match path.as_str() {
                "/search" => {
                    let q = params.get("q").cloned().unwrap_or_default();
                    let p: usize = params.get("p").and_then(|s| s.parse().ok()).unwrap_or(1);
                    if q.is_empty() {
                        let _ = sock.write_all(&redirect_home()).await;
                        return;
                    }
                    qc.fetch_add(1, Ordering::Relaxed);
                    let per_page = 10usize;
                    let k = (p * per_page) as u32;
                    let r = match searcher
                        .send(Box::new(WsSearch(enc_search(k, &q))))
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            let body = format!("search error: {e:?}");
                            let _ = sock
                                .write_all(&http_resp(500, "text/plain; charset=utf-8", &body))
                                .await;
                            return;
                        }
                    };
                    let json =
                        String::from_utf8_lossy(&r.downcast_ref::<WsSearchResult>().unwrap().0)
                            .into_owned();
                    let items = parse_search_json(&json);
                    let page_items: Vec<_> = items
                        .into_iter()
                        .skip((p - 1) * per_page)
                        .take(per_page)
                        .collect();
                    let html = render_results(&q, &page_items, p, per_page);
                    let _ = sock
                        .write_all(&http_resp(200, "text/html; charset=utf-8", &html))
                        .await;
                }
                "/stats" => {
                    let r = searcher.send(Box::new(WsHealthz(vec![]))).await.ok();
                    let json = r
                        .map(|r| {
                            String::from_utf8_lossy(
                                &r.downcast_ref::<WsHealthzR>().unwrap().0,
                            )
                            .into_owned()
                        })
                        .unwrap_or_else(|| "{}".into());
                    let _ = sock
                        .write_all(&http_resp(200, "application/json", &json))
                        .await;
                }
                _ => {
                    let html = render_home();
                    let _ = sock
                        .write_all(&http_resp(200, "text/html; charset=utf-8", &html))
                        .await;
                }
            }
        });
    }
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 3 <= b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn http_resp(status: u16, ct: &str, body: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        302 => "Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    head.extend_from_slice(body.as_bytes());
    head
}

fn redirect_home() -> Vec<u8> {
    "HTTP/1.1 302 Found\r\nLocation: /\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
}

/// SearchResult json → [(score,url,title,snippet)]（简解——akka 侧已转义）。
fn parse_search_json(json: &str) -> Vec<(f64, String, String, String)> {
    let mut out = Vec::new();
    let mut rest = json.trim();
    rest = rest.strip_prefix('[').unwrap_or(rest);
    rest = rest.strip_suffix(']').unwrap_or(rest);
    if rest.is_empty() {
        return out;
    }
    // 顶层对象切分（简单深度计数——字段值无嵌套对象）
    let mut depth = 0;
    let mut start = 0;
    let mut objs = Vec::new();
    for (i, c) in rest.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    objs.push(&rest[start..=i]);
                }
            }
            _ => {}
        }
    }
    for o in objs {
        let score = json_f64(o, "score").unwrap_or(0.0);
        let url = json_str(o, "url");
        let title = json_str(o, "title");
        let snippet = json_str(o, "snippet");
        out.push((score, url, title, snippet));
    }
    out
}

fn json_str(obj: &str, key: &str) -> String {
    let pat = format!("\"{key}\":\"");
    if let Some(i) = obj.find(&pat) {
        let s = &obj[i + pat.len()..];
        let mut out = String::new();
        let mut esc = false;
        for c in s.chars() {
            if esc {
                out.push(match c {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                break;
            } else {
                out.push(c);
            }
        }
        return out;
    }
    String::new()
}

fn json_f64(obj: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{key}\":");
    let i = obj.find(&pat)?;
    let rest = &obj[i + pat.len()..];
    let num: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    num.parse().ok()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const PAGE_CSS: &str = r#"
body{font-family:-apple-system,"PingFang SC","Microsoft YaHei",sans-serif;margin:0;color:#222}
.hd{padding:20px 28px;border-bottom:1px solid #eee;display:flex;gap:14px;align-items:center}
.logo{font-size:23px;font-weight:700;color:#2932E1;white-space:nowrap}
form{flex:1;display:flex;gap:8px}
input[type=text]{flex:1;max-width:600px;padding:10px 16px;border:2px solid #2932E1;
  border-radius:22px;font-size:16px;outline:none}
button{padding:10px 24px;background:#2932E1;color:#fff;border:0;border-radius:22px;
  font-size:16px;cursor:pointer}
button:hover{background:#1d24b8}
.bd{max-width:740px;margin:24px auto;padding:0 18px}
.meta{color:#9195A3;font-size:13px;margin-bottom:18px}
.item{margin-bottom:24px}
.t a{color:#2440B3;font-size:18px;text-decoration:none}
.t a:hover{text-decoration:underline}
.u{color:#008000;font-size:13px;margin:2px 0;word-break:break-all}
.s{color:#444;font-size:14px;line-height:1.7}
.bar{margin-top:30px;text-align:center}
.bar a,.bar b{color:#2932E1;margin:0 8px}
.empty{color:#666;text-align:center;margin:70px 0;font-size:15px}
.stats{font-size:13px;color:#9195A3;white-space:nowrap}
"#;

fn page_shell(q: &str, body: &str) -> String {
    let title = if q.is_empty() {
        "WebSearch".to_string()
    } else {
        format!("{q} · WebSearch")
    };
    format!(
        r#"<!DOCTYPE html><html lang="zh"><head><meta charset="utf-8"><title>{}</title><style>{PAGE_CSS}</style></head><body>
<div class="hd"><span class="logo">🔍 WebSearch</span>
<form action="/search" method="get">
<input type="text" name="q" value="{}" placeholder="输入关键词，回车搜索…" autofocus>
<button>搜一下</button></form><span class="stats" id="st"></span></div>
<div class="bd">{}</div>
<script>setInterval(async()=>{{try{{const r=await fetch('/stats');const j=await r.json();
const e=document.getElementById('st');
if(e&&j.docs)e.textContent=`已索引 ${{j.docs}} 页 · ${{j.terms}} 词项`;}}catch(_){{}}}},5000)</script>
</body></html>"#,
        esc(&title),
        esc(q),
        body
    )
}

fn render_home() -> String {
    page_shell(
        "",
        r#"<div class="empty" style="margin-top:120px;font-size:17px">五运行时真实搜索引擎<br>
<span style="font-size:13px;color:#9195A3">Erlang Frontier · Rust Crawler · Ray jieba 分词 · Akka BM25 检索</span></div>"#,
    )
}

fn render_results(q: &str, items: &[(f64, String, String, String)], page: usize, per_page: usize) -> String {
    let terms: Vec<&str> = q
        .split_whitespace()
        .flat_map(|w| {
            // 简易高亮词切分（中文按 2-3 字滑窗太碎——取 jieba 不在侧；用空格 + 单字 fallback）
            vec![w]
        })
        .collect();
    let rows: String = items
        .iter()
        .map(|(score, url, title, snippet)| {
            let title = if title.is_empty() { url } else { title };
            let mut snip = esc(snippet);
            for t in &terms {
                if t.len() > 1 {
                    snip = snip.replace(t, &format!("<mark>{t}</mark>"));
                }
            }
            format!(
                r#"<div class="item"><div class="t"><a href="{url}">{}</a></div>
<div class="u">{url}</div><div class="s">{snip}</div>
<div style="font-size:12px;color:#9195A3">score {score:.2}</div></div>"#,
                esc(title)
            )
        })
        .collect();
    let empty = if items.is_empty() {
        r#"<div class="empty">没有找到相关结果<br><span style="font-size:13px">换个关键词试试？</span></div>"#
    } else {
        ""
    };
    let max_p = 10;
    let nav: String = (1..=max_p)
        .map(|p| {
            if p == page {
                format!("<b>[{p}]</b>")
            } else {
                let eq = urlencode(q);
                format!(r#"<a href="/search?q={eq}&p={p}">[{p}]</a>"#)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let _ = per_page;
    page_shell(
        q,
        &format!(
            r#"<div class="meta">找到约 {} 条结果</div>{empty}{rows}<div class="bar">{nav}</div>"#,
            items.len()
        ),
    )
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
