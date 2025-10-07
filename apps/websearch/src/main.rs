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
wire_msg!(WsClear, "bin:ws/Clear");
wire_msg!(WsClearAck, "bin:ws/ClearAck");
wire_msg!(WsListDocs, "bin:ws/ListDocs");
wire_msg!(WsListDocsR, "bin:ws/ListDocsR");
wire_msg!(WsListTerms, "bin:ws/ListTerms");
wire_msg!(WsListTermsR, "bin:ws/ListTermsR");

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
                _ => a
                    .split(|c: char| c.is_whitespace() || c == '>')
                    .next()
                    .unwrap_or(""),
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
    Some(format!(
        "{}://{}{}{}",
        scheme,
        host,
        path,
        query.unwrap_or_default()
    ))
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
        Self {
            client,
            robots: Default::default(),
        }
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
                Ok(out) if out.status.success() => {
                    String::from_utf8_lossy(&out.stdout).into_owned()
                }
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
    let (mut pages, mut max_depth, mut web_port) = (50000u64, 10u16, 8080u16);
    let mut sites_target = 100usize; // 站点目标制：爬够 N 个不同 host 才允许停
    let mut data_dir = PathBuf::from("./data");
    let mut gw_addrs: Vec<(&str, String)> = Vec::new(); // (tag, host:port)
    let mut serve_only = false; // 只起检索服务（不爬——重启后回放索引的独立运行形态）
                                // 远程部署形态（跨网络）：
                                //   --bind 0.0.0.0:19870  应用监听，三网关主动反拨注册（registry——
                                //                         应用零网关地址知识，网关可分布任意机器）
                                //   --node-root <dir>     制品根目录（file:// uri 指此处——远程节点上
                                //                         该目录须存在同构制品：erlang/ python/ jvm/target/…）
                                //   --artifacts http://…  制品源（预分发形态：节点侧已就位，deploy 只传
                                //                         uri 引用不依赖本机路径）
    let mut bind_addr: Option<std::net::SocketAddr> = None;
    let mut wait_secs: u64 = 60;
    let mut node_root: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pages" => {
                pages = args[i + 1].parse().unwrap();
                i += 2;
            }
            "--sites" => {
                sites_target = args[i + 1].parse().unwrap();
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
            "--bind" => {
                bind_addr = Some(args[i + 1].parse().unwrap());
                i += 2;
            }
            "--wait" => {
                wait_secs = args[i + 1].parse().unwrap();
                i += 2;
            }
            "--node-root" => {
                node_root = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--serve-only" => {
                serve_only = true;
                i += 1;
            }
            "--spawn-gateways" => {
                i += 1;
            }
            s if s
                .split_once('=')
                .is_some_and(|(t, _)| matches!(t, "erl" | "ray" | "jvm")) =>
            {
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
    /// 内置多样化种子（站点目标制 ≥100 站——队列耗尽/无种子时自动回填）。
    const BUILTIN_SEEDS: &[&str] = &[
        // 中文技术/科普/社区
        "https://www.runoob.com",
        "https://developer.mozilla.org/zh-CN/",
        "https://www.zhihu.com",
        "https://www.cnblogs.com",
        "https://juejin.cn",
        "https://segmentfault.com",
        "https://www.oschina.net",
        "https://www.infoq.cn",
        "https://www.imooc.com",
        "https://www.liaoxuefeng.com",
        // 门户/百科（出链丰富——快速扩散 host 多样性）
        "https://www.wikipedia.org",
        "https://zh.wikipedia.org",
        "https://baike.baidu.com",
        "https://www.hao123.com",
        "https://www.qq.com",
        "https://www.sina.com.cn",
        "https://www.sohu.com",
        "https://www.163.com",
        "https://www.ifeng.com",
        "https://www.people.com.cn",
        // 开发者/文档站
        "https://github.com",
        "https://stackoverflow.com",
        "https://docs.python.org",
        "https://www.rust-lang.org",
        "https://go.dev",
        "https://nodejs.org",
        "https://www.erlang.org",
        "https://akka.io",
        "https://ray.io",
        "https://redis.io",
        "https://www.postgresql.org",
        "https://nginx.org",
        "https://httpd.apache.org",
        "https://maven.apache.org",
        "https://gradle.org",
        "https://www.docker.com",
        "https://kubernetes.io",
        // 高校/机构（外链丰富）
        "https://www.tsinghua.edu.cn",
        "https://www.pku.edu.cn",
        "https://www.ustc.edu.cn",
        "https://www.fudan.edu.cn",
        "https://www.sjtu.edu.cn",
        "https://www.nju.edu.cn",
        "https://www.zju.edu.cn",
        "https://www.cas.cn",
        "https://www.cctv.com",
        "https://www.gov.cn",
    ];

    /// 种子注入公用：过滤 → 去重表 → frontier（返回实际入队条数）。
    async fn inject_seeds(
        raw: &[String],
        dedupe: &mut HashSet<String>,
        dedupe_path: &std::path::Path,
        frontier: &parrot_remote::RemoteActorRef,
    ) -> usize {
        let pairs: Vec<(String, u16)> = raw
            .iter()
            .filter_map(|s| normalize_url(s, s))
            .map(|u| (u, 0u16))
            .collect();
        let fresh: Vec<(String, u16)> = pairs
            .into_iter()
            .filter(|(u, _)| dedupe.insert(u.clone()))
            .collect();
        if fresh.is_empty() {
            return 0;
        }
        {
            let mut out = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dedupe_path)
                .unwrap();
            for (u, _) in &fresh {
                let _ = writeln!(out, "{u}");
            }
        }
        let n = fresh.len();
        frontier.send(Box::new(WsPush(enc_push(&fresh)))).await.ok();
        n
    }
    // 网关缺省（run.sh 前置拉起——同 crawler-lab direct 模式端口）
    let erl_addr = gw_addr(&gw_addrs, "erl", "127.0.0.1:19871");
    let ray_addr = gw_addr(&gw_addrs, "ray", "127.0.0.1:19873");
    let jvm_addr = gw_addr(&gw_addrs, "jvm", "127.0.0.1:19872");

    // Web 端口预检（启动即报——不爬 60s 后才发现端口冲突）。
    // 预检=瞬时 bind+drop；serve_web 内仍有顺延兜底（预检与真绑间窗口极小）。
    {
        use tokio::net::TcpListener;
        match TcpListener::bind(("0.0.0.0", web_port)).await {
            Ok(_) => {}
            Err(e) => eprintln!(
                "[ws] ⚠ Web 端口 {web_port} 当前被占用（{e}）——启动后将自动顺延至下一个可用端口"
            ),
        }
    }

    // 数据目录策略：--keep 续爬（回放旧段+去重表）；默认全新爬取清空旧数据
    // （否则历次残留索引混入 /docs 页面统计——用户实测 docs=175 即此因）
    let mut keep_data = false;
    {
        let mut j = 0;
        let argv: Vec<String> = std::env::args().skip(1).collect();
        while j < argv.len() {
            if argv[j] == "--keep" {
                keep_data = true;
            }
            j += 1;
        }
    }
    if serve_only {
        keep_data = true; // serve-only 永远回放既有索引
    }
    std::fs::create_dir_all(&data_dir).unwrap();
    let dedupe_path = data_dir.join("dedupe.tsv");
    let mut dedupe: HashSet<String> = HashSet::new();
    if keep_data {
        if dedupe_path.exists() {
            let txt = std::fs::read_to_string(&dedupe_path).unwrap_or_default();
            for l in txt.lines() {
                if !l.trim().is_empty() {
                    dedupe.insert(l.trim().to_string());
                }
            }
            println!("[ws] 续爬模式（--keep）：恢复去重表 {} URLs", dedupe.len());
        }
    } else {
        // 全新爬取：清旧去重表 + 旧索引段（JVM 回放时即为空库）
        if dedupe_path.exists() {
            let _ = std::fs::remove_file(&dedupe_path);
        }
        let index_dir = data_dir.join("index");
        if index_dir.exists() {
            let n = std::fs::read_dir(&index_dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.path().extension().is_some_and(|x| x == "segment"))
                        .count()
                })
                .unwrap_or(0);
            if n > 0 {
                let _ = std::fs::remove_dir_all(&index_dir);
                println!("[ws] 全新爬取：清除旧索引 {n} 段（--keep 可保留续爬）");
            }
        }
    }

    // ── 组网：双模式 ───────────────────────────────────────────────
    //   direct   ——应用主动拨号三网关（erl=/ray=/jvm= host:port，可跨机器）
    //   registry ——--bind 监听，三网关主动反拨注册（生产形态：应用零网关
    //              地址知识；网关可分布任意机器——跨网络部署）
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("websearch", bind_addr),
        Arc::new(NoopLookup),
    )
    .unwrap();
    client.start().await.unwrap();
    if bind_addr.is_some() {
        let local = client.local_addr().expect("bound");
        println!("[ws] 组网模式：registry（应用监听 {local}，等待三网关反拨注册… ≤{wait_secs}s）");
        let need: &[&str] = if serve_only {
            &["jvm-search-1"]
        } else {
            &["erl-gw-1", "ray-gw-1", "jvm-search-1"]
        };
        let deadline = Instant::now() + Duration::from_secs(wait_secs);
        loop {
            let ready = need
                .iter()
                .filter(|n| client.nodes.get(n).is_some())
                .count();
            if ready == need.len() {
                println!("[ws] 网关已全部注册：{need:?}");
                break;
            }
            if Instant::now() > deadline {
                eprintln!(
                    "[ws] 等待网关注册超时（{wait_secs}s，就绪 {ready}/{}）",
                    need.len()
                );
                std::process::exit(3);
            }
            if ready > 0 {
                println!("[ws]   已注册 {ready}/{} …", need.len());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // register_link 完成窗口（sender 就绪）
        let need_links = need.len();
        for _ in 0..50 {
            if client.links_snapshot().await.len() >= need_links {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    } else if serve_only {
        let sa: std::net::SocketAddr = jvm_addr.parse().unwrap();
        // serve-only 重试连（网关可能比应用晚起——30s 窗口）
        let mut linked = false;
        for attempt in 1..=15 {
            match client.connect(&NodeAddr::tcp("jvm-search-1", sa)).await {
                Ok(()) => {
                    linked = true;
                    break;
                }
                Err(e) => {
                    if attempt == 1 {
                        eprintln!("[ws] jvm 网关未就绪（{e}）——重试中（需先起网关，见 run.sh 或运维文档 §3.2）");
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
        if !linked {
            eprintln!(
                "[ws] ✗ 30s 内未连上 jvm 网关 {jvm_addr}——请先起网关：\n  \
                 (cd interop/jvm/target && env WS_DATA=$PWD/apps/websearch/data java -cp \"parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)\" parrot.protocol.jvm.ParrotGatewayMain 19872 node=jvm-search-1 7200)"
            );
            std::process::exit(3);
        }
        println!("[ws] connected: jvm-search-1（serve-only）");
    } else {
        for (id, addr) in [
            ("erl-gw-1", &erl_addr),
            ("ray-gw-1", &ray_addr),
            ("jvm-search-1", &jvm_addr),
        ] {
            let sa = addr.parse().unwrap();
            client
                .connect(&NodeAddr::tcp(id, sa))
                .await
                .expect("connect");
            println!("[ws] connected: {id}");
        }
    }

    // ── 组件部署（R4 闭环——app 制品动态载入三网关）────────────────
    // 制品根：--node-root 显式指定（远程节点上的制品目录）> 本机构建根
    // （file:// uri 语义 = 目标网关节点本地路径——跨机部署时各节点预置同构制品）
    let app_root = node_root.unwrap_or_else(app_root_dir);
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
            // 调用方决定 panic/幂等重试——闭包不再吞错
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
                    app_root
                        .join("jvm/target/websearch-jvm-1.0.0.jar")
                        .display()
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
                        app_root
                            .join("jvm/target/websearch-jvm-1.0.0.jar")
                            .display()
                    )),
                },
            ),
        ]
    };
    // akka 组件的数据目录经环境变量（WS_DATA）——网关进程继承
    for (node, artifact) in deploys {
        let artifact_retry = artifact.clone();
        let name = match &artifact {
            AdminArtifactRef::Beam { .. } => "frontier",
            AdminArtifactRef::PyModule { .. } => "tokenizer",
            _ => "search",
        };
        // deploy 幂等：网关进程长存（重启应用不重启网关）——同名组件已
        // 在位时先 Drain 再 Deploy（serve-only/应用重启场景否则 name 冲突）
        let r = match deploy(&node, name, artifact).await {
            Ok(r) => r,
            Err(e) if format!("{e:?}").contains("not unique") => {
                println!("[ws] {node}/{name} 已在位——drain 后重部署");
                let _ = client
                    .drain_component(&node, name, Duration::from_secs(30))
                    .await
                    .map_err(|e| eprintln!("[ws] drain {node}/{name}: {e:?}"));
                deploy(&node, name, artifact_retry)
                    .await
                    .unwrap_or_else(|e2| panic!("redeploy {node}: {e2:?}"))
            }
            Err(e) => panic!("deploy {node}: {e:?}"),
        };
        println!("[ws] deploy {node}/{name} → {r:?}");
    }

    let searcher = client
        .remote_ref("parrot://jvm-search-1/jvm/user/search")
        .unwrap();
    // 全新爬取：同步清 JVM 内存索引（网关早于应用启动已回放旧段——内存有残留）
    if !serve_only && !keep_data {
        match searcher.send(Box::new(WsClear(vec![]))).await {
            Ok(r) => {
                let n = get_u32(&r.downcast_ref::<WsClearAck>().unwrap().0, &mut 0);
                println!("[ws] 已清空检索端内存索引（{n} 段残留）");
            }
            Err(e) => eprintln!("[ws] ⚠ 清索引失败（/docs 可能有旧数据）：{e:?}"),
        }
    }
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
    let frontier = client
        .remote_ref("parrot://erl-gw-1/user/frontier")
        .unwrap();
    let tokenizer = client
        .remote_ref("parrot://ray-gw-1/user/tokenizer")
        .unwrap();

    // ── 种子注入（去重表前置过滤——用户种子 + 内置集合并注入）────────
    let builtin: Vec<String> = BUILTIN_SEEDS.iter().map(|s| s.to_string()).collect();
    if seeds.is_empty() {
        println!(
            "[ws] 未指定种子——启用内置多样化种子集（{} 条）",
            builtin.len()
        );
    }
    let n_user = inject_seeds(&seeds, &mut dedupe, &dedupe_path, &frontier).await;
    let n_builtin = if seeds.is_empty() {
        inject_seeds(&builtin, &mut dedupe, &dedupe_path, &frontier).await
    } else {
        0
    };
    // 内置种子游标：队列耗尽且目标未达时回填下一条（单种子死路自愈）
    let mut builtin_cursor = 0usize;
    println!("[ws] 种子注入：用户 {n_user} 条 + 内置 {n_builtin} 条");

    // ── 漫爬主循环（并发抓取 + 双路索引 + 出链回注）──────────────────
    // Web 服务先行启动（站点目标制耗时较长——边爬边查）
    {
        let port = web_port;
        let client2 = client.clone();
        let searcher2 = searcher.clone();
        tokio::spawn(async move {
            serve_web(port, client2, searcher2).await;
        });
    }
    println!("[ws] 浏览器打开 http://localhost:{web_port} 开始搜索（爬取后台持续——站点/分词表页实时可见）");
    let fetcher = Arc::new(Fetcher::new());
    let fetched = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let mut pending: Vec<(String, u16)> = Vec::new();
    let mut in_flight: usize = 0;
    let mut frontier_drained_just_now = false; // 最近一次取批为空（队列耗尽信号）
    let concurrency = 8usize;
    let mut host_last: HashMap<String, Instant> = HashMap::new();
    let mut terms_buf: Vec<(String, u64, u32)> = Vec::new();
    let mut pages_buf: Vec<(u64, String)> = Vec::new();
    // 站点目标制（用户裁定 3）：已成功抓取的 host 集 + 每 host 已达最大深度
    let mut hosts_done: HashSet<String> = HashSet::new();
    let mut host_max_depth: HashMap<String, u16> = HashMap::new();
    let t0 = Instant::now();
    let mut last_log = Instant::now();
    let mut last_flush = Instant::now();

    println!(
        "[ws] 爬取目标：≥{sites_target} 个站点 · 每站深度 ≥{max_depth}（安全页数上限 {pages}）"
    );

    loop {
        // 进度打点
        if last_log.elapsed() >= Duration::from_secs(3) {
            let deep_enough = host_max_depth.values().filter(|d| **d >= max_depth).count();
            println!(
                "[ws {:>4}s] fetched={} fail={} sites={}/{} 深度达标={} pending={} inflight={}",
                t0.elapsed().as_secs(),
                fetched.load(Ordering::Relaxed),
                failed.load(Ordering::Relaxed),
                hosts_done.len(),
                sites_target,
                deep_enough,
                pending.len(),
                in_flight
            );
            last_log = Instant::now();
        }
        // 周期段落盘（60s 一次）：崩溃/中断后 serve-only 可回放已爬部分——
        // JVM 侧 flushSegs 全量快照式写段（非增量），周期做不丢数据只多段文件
        if last_flush.elapsed() >= Duration::from_secs(60) {
            if let Ok(r) = searcher.send(Box::new(WsFlush(vec![]))).await {
                let segs = get_u32(&r.downcast_ref::<WsFlushAck>().unwrap().0, &mut 0);
                if segs > 0 {
                    println!("[ws] 周期段落盘：累计 {segs} 段");
                }
            }
            last_flush = Instant::now();
        }
        // 站点目标制终局（用户裁定 3）：
        //   达标 = ≥sites_target 站 且 其中 ≥sites_target 站深度 ≥max_depth
        //   ——用户要求“至少 100 站 × 每站至少 10 深”，按已爬站全达标实现；
        //   队列耗尽且未达标 → 自动回填内置种子续爬；回填源也尽 → 收尾
        let done = fetched.load(Ordering::Relaxed) + failed.load(Ordering::Relaxed);
        let deep_enough = host_max_depth.values().filter(|d| **d >= max_depth).count();
        let goal_hit = hosts_done.len() >= sites_target && deep_enough >= sites_target;
        if in_flight == 0 && pending.is_empty() && (goal_hit || done >= pages) {
            if goal_hit {
                println!(
                    "[ws] 站点目标达成：{} 站 × 深度≥{} —— 收尾",
                    hosts_done.len(),
                    max_depth
                );
            }
            break;
        }
        // 队列耗尽但目标未达 → 回填内置种子（单种子死路/JS 渲染页自愈）
        if in_flight == 0 && pending.is_empty() && frontier_drained_just_now {
            if builtin_cursor < builtin.len() {
                let batch: Vec<String> =
                    builtin[builtin_cursor..(builtin_cursor + 8).min(builtin.len())].to_vec();
                builtin_cursor += batch.len();
                let n = inject_seeds(&batch, &mut dedupe, &dedupe_path, &frontier).await;
                println!(
                    "[ws] 队列耗尽（sites={}/{}）——回填内置种子 +{n}（游标 {}/{}）",
                    hosts_done.len(),
                    sites_target,
                    builtin_cursor,
                    builtin.len()
                );
                frontier_drained_just_now = false; // 新种子已入队——重新等取批
                continue;
            }
            println!(
                "[ws] 内置种子全部耗尽仍 sites={}/{} —— 自然收尾",
                hosts_done.len(),
                sites_target
            );
            break;
        }
        // 补批（frontier 取批 + pending 汇流）——目标达成前持续为 frontier 泵入新 URL
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
                    frontier_drained_just_now = batch.is_empty();
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
            let ok_to_go = !matches!(host_last.get(&host),
                Some(t) if now.duration_since(*t) < Duration::from_millis(200));
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
                // 队列耗尽——不在此 break：留给上方回填/收尾分支处理
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
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
                // 站点目标制记账（用户裁定 3）：host 集合 + 每 host 已探最深深度
                {
                    let h = host_of(&url);
                    hosts_done.insert(h.clone());
                    host_max_depth
                        .entry(h)
                        .and_modify(|d| *d = (*d).max(depth))
                        .or_insert(depth);
                }
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
                    let texts: Vec<(u64, &str)> =
                        pages_buf.iter().map(|(id, t)| (*id, t.as_str())).collect();
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
        "[ws] 爬取完成：fetched={} fail={} 站点={}（最深达 {} 层）耗时 {:?}",
        fetched.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed),
        hosts_done.len(),
        host_max_depth.values().copied().max().unwrap_or(0),
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
    // 站点目标制爬取耗时较长——Web 服务已随爬取启动（spawn），此处常驻不退出
    println!(
        "[ws] 爬取收尾完成——Web 服务常驻：浏览器打开 http://localhost:{web_port}（Ctrl-C 退出）"
    );
    std::future::pending::<()>().await;
}

/// 简易并发 join：等全部完成取首个 Some（简化——批量小）。
#[allow(dead_code)]
async fn futures_buffered(
    tasks: Vec<tokio::task::JoinHandle<Option<(String, u16, String)>>>,
) -> (
    Option<(String, u16, String)>,
    Vec<tokio::task::JoinHandle<Option<(String, u16, String)>>>,
) {
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ═══════════════════════════ Web 查询服务（百度式页面）════════════════════

async fn serve_web(
    port: u16,
    _client: Arc<RemoteActorSystem>,
    searcher: parrot_remote::RemoteActorRef,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // 端口被占不 panic——顺延探测（8080 被常见代理/开发服务占用是常态）
    let (listener, actual) = {
        let mut p = port;
        loop {
            match TcpListener::bind(("0.0.0.0", p)).await {
                Ok(l) => break (l, p),
                Err(e) if p < port + 20 => {
                    eprintln!("[ws] 端口 {p} 被占用（{e}）——尝试 {}", p + 1);
                    p += 1;
                }
                Err(e) => panic!("web 端口 {port}~{} 全部不可用：{e}", port + 20),
            }
        }
    };
    if actual != port {
        println!(
            "[ws] ⚠ Web 服务改用端口 {actual}（{port} 被占）——浏览器打开 http://localhost:{actual}"
        );
    }
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
                .filter_map(|kv| {
                    kv.split_once('=')
                        .map(|(k, v)| (k.to_string(), url_decode(v)))
                })
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
                    let r = match searcher.send(Box::new(WsSearch(enc_search(k, &q)))).await {
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
                            String::from_utf8_lossy(&r.downcast_ref::<WsHealthzR>().unwrap().0)
                                .into_owned()
                        })
                        .unwrap_or_else(|| "{}".into());
                    let _ = sock
                        .write_all(&http_resp(200, "application/json", &json))
                        .await;
                }
                "/docs" | "/terms" => {
                    // 浏览页：站点清单（host 聚合）/ 分词表（df 降序）——分页
                    let is_docs = path == "/docs";
                    let off: u32 = params.get("p").and_then(|s| s.parse().ok()).unwrap_or(0);
                    let limit: u32 = if is_docs { 100 } else { 300 };
                    let mut req = vec![];
                    put_u32(&mut req, off * limit);
                    put_u32(&mut req, limit);
                    let r = if is_docs {
                        searcher.send(Box::new(WsListDocs(req))).await
                    } else {
                        searcher.send(Box::new(WsListTerms(req))).await
                    };
                    match r {
                        Ok(rep) => {
                            let json = if is_docs {
                                String::from_utf8_lossy(
                                    &rep.downcast_ref::<WsListDocsR>().unwrap().0,
                                )
                                .into_owned()
                            } else {
                                String::from_utf8_lossy(
                                    &rep.downcast_ref::<WsListTermsR>().unwrap().0,
                                )
                                .into_owned()
                            };
                            let html = if is_docs {
                                render_docs_page(&json, off)
                            } else {
                                render_terms_page(&json, off)
                            };
                            let _ = sock
                                .write_all(&http_resp(200, "text/html; charset=utf-8", &html))
                                .await;
                        }
                        Err(e) => {
                            let body = format!("list error: {e:?}");
                            let _ = sock
                                .write_all(&http_resp(500, "text/plain; charset=utf-8", &body))
                                .await;
                        }
                    }
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
<div class="hd"><a href="/" class="logo" style="text-decoration:none">🔍 WebSearch</a>
<form action="/search" method="get">
<input type="text" name="q" value="{}" placeholder="输入关键词，回车搜索…" autofocus>
<button>搜一下</button></form><a href="/docs" style="font-size:13px;color:#2932E1;white-space:nowrap">站点</a><a href="/terms" style="font-size:13px;color:#2932E1;white-space:nowrap">分词表</a><span class="stats" id="st"></span></div>
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
<span style="font-size:13px;color:#9195A3">Erlang Frontier · Rust Crawler · Ray jieba 分词 · Akka BM25 检索</span></div>
<div style="text-align:center;margin-top:28px;font-size:14px">
<a href="/docs" style="color:#2932E1">📋 已索引站点</a> ·
<a href="/terms" style="color:#2932E1">📖 分词表</a></div>"#,
    )
}

/// /docs 页：站点清单（JSON → 表格——host 聚合 + 页数 + 样例 URL）。
fn render_docs_page(json: &str, page: u32) -> String {
    let hosts_total = json_f64(json, "hosts").unwrap_or(0.0) as u64;
    let docs_total = json_f64(json, "total_docs").unwrap_or(0.0) as u64;
    let per = 100u64;
    let last_page = hosts_total.div_ceil(per).saturating_sub(1) as u32;
    let page = page.min(last_page); // 越界页号钳制
    let mut rows = String::new();
    // items 数组逐对象取（复用 parse_search_json 的对象切分器）
    for (i, obj) in json_objects(json_array_of(json, "items"))
        .iter()
        .enumerate()
    {
        let host = json_str(obj, "host");
        let pages = json_f64(obj, "pages").unwrap_or(0.0) as u64;
        let sample = json_str(obj, "sample");
        rows.push_str(&format!(
            r#"<tr><td class="n">{}</td><td><b>{}</b></td><td class="n">{pages}</td>
<td><a href="{sample}" target="_blank" style="color:#2440B3;font-size:12px;word-break:break-all">{}</a></td></tr>"#,
            page * 100 + i as u32 + 1,
            esc(&host),
            esc(&sample)
        ));
    }
    if rows.is_empty() {
        rows = r#"<tr><td colspan="4" style="text-align:center;color:#999;padding:40px">暂无索引——先跑一次爬取（run.sh）</td></tr>"#.into();
    }
    let nav = pager_nav("/docs", page, last_page);
    page_shell(
        "",
        &format!(
            r#"<h2 style="font-size:20px;margin:10px 0 4px">已索引站点</h2>
<div class="meta">共 {hosts_total} 个站点 · {docs_total} 个页面（按页面数降序）</div>
<table style="width:100%;border-collapse:collapse;font-size:14px">
<tr style="color:#9195A3;text-align:left"><th style="padding:8px">#</th><th>站点</th><th>页面数</th><th>样例 URL</th></tr>
{rows}</table>{nav}"#
        ),
    )
}

/// /terms 页：分词表（df 降序——jieba 切出的全部词条）。
fn render_terms_page(json: &str, page: u32) -> String {
    let total = json_f64(json, "total").unwrap_or(0.0) as u64;
    let per = 300u64;
    let last_page = total.div_ceil(per).saturating_sub(1) as u32;
    let page = page.min(last_page); // 越界页号钳制
    let mut rows = String::new();
    for (i, obj) in json_objects(json_array_of(json, "items"))
        .iter()
        .enumerate()
    {
        let term = json_str(obj, "term");
        let df = json_f64(obj, "df").unwrap_or(0.0) as u64;
        let q = urlencode(&term);
        rows.push_str(&format!(
            r#"<tr><td class="n">{}</td><td style="font-size:15px">{}</td>
<td class="n">{df}</td><td><a href="/search?q={q}" style="color:#2932E1;font-size:12px">搜索 →</a></td></tr>"#,
            page * 300 + i as u32 + 1,
            esc(&term)
        ));
    }
    if rows.is_empty() {
        rows = r#"<tr><td colspan="4" style="text-align:center;color:#999;padding:40px">暂无词条——先跑一次爬取</td></tr>"#.into();
    }
    let nav = pager_nav("/terms", page, last_page);
    page_shell(
        "",
        &format!(
            r#"<h2 style="font-size:20px;margin:10px 0 4px">分词表</h2>
<div class="meta">共 {total} 个词条（jieba 切出 · 按文档频率 df 降序——点词条直接搜索）</div>
<table style="width:100%;border-collapse:collapse;font-size:14px">
<tr style="color:#9195A3;text-align:left"><th style="padding:8px">#</th><th>词条</th><th>df</th><th></th></tr>
{rows}</table>{nav}"#
        ),
    )
}

/// JSON 里取 "items":[...] 子串（浅找——值本身是数组）。
fn json_array_of<'a>(json: &'a str, key: &str) -> &'a str {
    let pat = format!("\"{key}\":");
    if let Some(i) = json.find(&pat) {
        let rest = &json[i + pat.len()..];
        if let Some(s) = rest.strip_prefix('[') {
            if let Some(e) = s.rfind(']') {
                return &s[..e];
            }
        }
    }
    ""
}

/// 顶层对象数组切分（花括号深度计数——复用于 docs/terms items）。
fn json_objects(arr: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in arr.char_indices() {
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
                    out.push(&arr[start..=i]);
                }
            }
            _ => {}
        }
    }
    out
}

/// 分页导航（首页/上一页/下一页/末页）。
fn pager_nav(base: &str, page: u32, last: u32) -> String {
    let page = page.min(last); // 越界页号钳制（末页对齐）
    if last == 0 && page == 0 {
        return String::new();
    }
    let mut parts = Vec::new();
    if page > 0 {
        parts.push(format!(r#"<a href="{base}?p={}">‹ 上一页</a>"#, page - 1));
    }
    parts.push(format!(
        r#"<span style="color:#9195A3">第 {} / {} 页</span>"#,
        page + 1,
        last + 1
    ));
    if page < last {
        parts.push(format!(r#"<a href="{base}?p={}">下一页 ›</a>"#, page + 1));
    }
    format!(
        r#"<div style="margin:26px 0 60px;text-align:center;font-size:14px">{}</div>"#,
        parts.join("&nbsp;&nbsp;&nbsp;")
    )
}

fn render_results(
    q: &str,
    items: &[(f64, String, String, String)],
    page: usize,
    per_page: usize,
) -> String {
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
