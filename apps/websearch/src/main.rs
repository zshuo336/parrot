//! websearch：真实搜索引擎应用（五运行时分工——同 crawler-lab 原则）。
//!
//! 用户四诉求（真实化）：
//!   1. 真实网页漫爬（reqwest HTTPS + seed URL + BFS + robots 礼貌策略）
//!   2. 索引落盘（akka 侧完整快照 seg-*.segment——恢复最新已提交状态）
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
use parrot_api::types::{BoxedActorRef, BoxedMessage};
use parrot_remote::admin_v2::{AdminArtifactRef, AdminInstancePolicy, ComponentDeploy};
use parrot_remote::system::{RemoteActorSystem, RemoteConfig};
use parrot_remote::{LocalLookup, NodeAddr};
use sha2::{Digest, Sha256};

mod application;
mod cli;
mod component_identity;
mod components;
mod crawl_journal;
#[cfg(test)]
mod fetch_tests;
mod host_runtime;
#[cfg(test)]
mod html_tests;
mod index_preparation;
mod indexing;
mod robots;
#[cfg(test)]
mod robots_tests;
mod shutdown;
mod workload;

// ═══════════════════════════ wire 消息（bin:ws/* 键空间）══════════════════

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

mod protocol;

mod json_protocol;
use json_protocol::{DocumentPage, Health, SearchHit, TermPage};

macro_rules! wire_json_msg {
    ($t:ident, $key:literal, $data:ty) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct $t(pub $data);
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$t>(),
                encode: |msg: &BoxedMessage| {
                    let m = msg.downcast_ref::<$t>().ok_or("downcast fail")?;
                    serde_json::to_vec(&m.0).map_err(|e| e.to_string())
                },
                decode: |b: &[u8]| {
                    json_protocol::decode::<$data>(b).map(|data| Box::new($t(data)) as BoxedMessage)
                },
            }
        }
    };
}

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

macro_rules! wire_number_msg {
    ($t:ident, $key:literal, $value:ty, $decode:path, $encode:expr) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct $t(pub $value);
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$t>(),
                encode: |msg: &BoxedMessage| {
                    let value = msg.downcast_ref::<$t>().ok_or("downcast fail")?;
                    Ok(($encode)(&value.0))
                },
                decode: |bytes: &[u8]| {
                    $decode(bytes).map(|value| Box::new($t(value)) as BoxedMessage)
                        .map_err(|error| error.to_string())
                },
            }
        }
    };
}

wire_msg!(WsCrawlState, "bin:ws/CrawlStateV2");
wire_msg!(WsPrepareIndex, "bin:ws/PrepareIndex");
wire_number_msg!(
    WsPrepareIndexR,
    "bin:ws/PrepareIndexR",
    index_preparation::Progress,
    index_preparation::Progress::decode,
    index_preparation::Progress::encode
);
wire_msg!(WsCheckpoint, "bin:ws/CheckpointV2");
wire_number_msg!(
    WsCrawlStateR,
    "bin:ws/CrawlStateV2R",
    crawl_journal::Checkpoint,
    crawl_journal::Checkpoint::decode,
    crawl_journal::Checkpoint::encode
);
wire_number_msg!(
    WsCheckpointAck,
    "bin:ws/CheckpointV2Ack",
    crawl_journal::Checkpoint,
    crawl_journal::Checkpoint::decode,
    crawl_journal::Checkpoint::encode
);

wire_msg!(WsPush, "bin:ws/Push");
wire_number_msg!(
    WsPushAck,
    "bin:ws/PushAck",
    u32,
    protocol::decode_count,
    |n: &u32| n.to_le_bytes().to_vec()
);
wire_msg!(WsNext, "bin:ws/Next");
wire_list_msg!(
    WsBatch,
    "bin:ws/Batch",
    protocol::FrontierEntry,
    protocol::decode_batch,
    enc_push
);
wire_msg!(WsSize, "bin:ws/Size");
wire_number_msg!(
    WsSizeR,
    "bin:ws/SizeR",
    (u32, u32),
    protocol::decode_size,
    |n: &(u32, u32)| [n.0.to_le_bytes(), n.1.to_le_bytes()].concat()
);
wire_msg!(WsTokenize, "bin:ws/Tokenize");
wire_list_msg!(
    WsTerms,
    "bin:ws/Terms",
    protocol::TermEntry,
    protocol::decode_terms,
    enc_terms
);
wire_msg!(WsIndexStats, "bin:ws/IndexStats");
wire_number_msg!(
    WsIndexStatsR,
    "bin:ws/IndexStatsR",
    (u32, u64),
    protocol::decode_stats,
    |n: &(u32, u64)| {
        let mut out = n.0.to_le_bytes().to_vec();
        out.extend_from_slice(&n.1.to_le_bytes());
        out
    }
);
wire_msg!(WsIndexDocuments, "bin:ws/IndexDocuments");
wire_number_msg!(
    WsDocumentsAck,
    "bin:ws/DocumentsAck",
    u32,
    protocol::decode_count,
    |n: &u32| n.to_le_bytes().to_vec()
);
wire_msg!(WsIndexTerms, "bin:ws/IndexTerms");
wire_number_msg!(
    WsIndexAck,
    "bin:ws/IndexAck",
    u32,
    protocol::decode_count,
    |n: &u32| n.to_le_bytes().to_vec()
);
wire_msg!(WsDocMeta, "bin:ws/DocMeta");
wire_number_msg!(
    WsDocAck,
    "bin:ws/DocAck",
    u32,
    protocol::decode_count,
    |n: &u32| n.to_le_bytes().to_vec()
);
wire_msg!(WsSearch, "bin:ws/Search");
wire_json_msg!(WsSearchResult, "bin:ws/SearchResult", Vec<SearchHit>);
wire_msg!(WsFlush, "bin:ws/Flush");
wire_number_msg!(
    WsFlushAck,
    "bin:ws/FlushAck",
    u32,
    protocol::decode_count,
    |n: &u32| n.to_le_bytes().to_vec()
);
wire_msg!(WsHealthz, "bin:ws/Healthz");
wire_json_msg!(WsHealthzR, "bin:ws/HealthzR", Health);
wire_msg!(WsClear, "bin:ws/Clear");
wire_number_msg!(
    WsClearAck,
    "bin:ws/ClearAck",
    u32,
    protocol::decode_count,
    |n: &u32| n.to_le_bytes().to_vec()
);
wire_msg!(WsListDocs, "bin:ws/ListDocs");
wire_json_msg!(WsListDocsR, "bin:ws/ListDocsR", DocumentPage);
wire_msg!(WsListTerms, "bin:ws/ListTerms");
wire_json_msg!(WsListTermsR, "bin:ws/ListTermsR", TermPage);

// ═══════════════════════════ 编解码（裸 LE）══════════════════════════════

fn put_u32(b: &mut Vec<u8>, n: u32) {
    b.extend_from_slice(&n.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, n: u64) {
    b.extend_from_slice(&n.to_le_bytes());
}
fn put_str(b: &mut Vec<u8>, s: &str) {
    put_u32(b, s.len() as u32);
    b.extend_from_slice(s.as_bytes());
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
    // 摘要源文本截 4096 个 Unicode 字符（akka 侧再裁 280 字符）
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
                let closing = tag.starts_with('/');
                let name: String = tag
                    .strip_prefix('/')
                    .unwrap_or(&tag)
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if let Some(ignored) = &drop_tag {
                    // Only the matching end tag exits ignored content. A prefix
                    // such as </scripture> must not expose embedded text.
                    if closing && name == *ignored {
                        drop_tag = None;
                        text.push(' ');
                    }
                } else if !closing && matches!(name.as_str(), "script" | "style" | "noscript") {
                    drop_tag = Some(name);
                    text.push(' ');
                } else if matches!(
                    name.as_str(),
                    "html"
                        | "head"
                        | "title"
                        | "body"
                        | "p"
                        | "div"
                        | "br"
                        | "hr"
                        | "li"
                        | "ul"
                        | "ol"
                        | "dl"
                        | "dt"
                        | "dd"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "table"
                        | "thead"
                        | "tbody"
                        | "tfoot"
                        | "tr"
                        | "td"
                        | "th"
                        | "article"
                        | "section"
                        | "header"
                        | "footer"
                        | "main"
                        | "aside"
                        | "nav"
                        | "blockquote"
                        | "pre"
                        | "figure"
                        | "figcaption"
                        | "form"
                        | "fieldset"
                        | "legend"
                        | "details"
                        | "summary"
                ) {
                    text.push(' ');
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

/// Resolve standard URL references, remove fragments and known tracking keys.
/// A trailing slash is a distinct resource and changes relative-link resolution.
fn normalize_url(base: &str, link: &str) -> Option<String> {
    let base = robots::crawl_url(base).ok()?;
    let mut url = robots::resolve(&base, link).ok()?;
    if let Some(query) = url.query() {
        // Preserve retained query octets/order instead of form-decoding and
        // re-encoding values (which can change signed or non-form queries).
        let kept: Vec<_> = query
            .split('&')
            .filter(|pair| {
                let key = pair.split('=').next().unwrap_or("").to_ascii_lowercase();
                !key.starts_with("utm_") && key != "fbclid" && key != "gclid"
            })
            .collect();
        let query = if kept.is_empty() {
            None
        } else {
            Some(kept.join("&"))
        };
        url.set_query(query.as_deref());
    }
    Some(url.into())
}

fn host_of(url: &str) -> String {
    let Ok(url) = robots::crawl_url(url) else {
        return String::new();
    };
    let host = url.host().expect("validated HTTP host").to_string();
    url.port()
        .map_or(host.clone(), |port| format!("{host}:{port}"))
}

// ═══════════════════════════ HTTP 爬客（reqwest）══════════════════════════

struct Fetcher {
    client: reqwest::Client,
    robots: robots::RobotsCache,
    allow_private_networks: bool,
    crawl_origins: Vec<url::Origin>,
}

/// Limit bytes after HTTP transfer decoding and gzip decompression, before
/// charset conversion. Reject oversized pages instead of indexing partial HTML.
const MAX_PAGE_BYTES: usize = 1 << 20;

async fn bounded_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let oversized = || format!("body exceeds {limit} byte limit");
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(oversized());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("body {e}"))? {
        if chunk.len() > limit - bytes.len() {
            return Err(oversized());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

impl Fetcher {
    fn client_builder_with_policy(allow_private_networks: bool) -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .user_agent(robots::USER_AGENT)
            .timeout(Duration::from_secs(12))
            .connect_timeout(Duration::from_secs(6))
            // A crawler must never route an untrusted URL through an ambient
            // HTTP(S)_PROXY. This also keeps the resolver policy meaningful.
            .no_proxy()
            .dns_resolver(std::sync::Arc::new(robots::SafeResolver {
                allow_private: allow_private_networks,
            }))
            .redirect(reqwest::redirect::Policy::none())
    }

    fn new() -> Self {
        Self::with_network_policy(false)
    }

    #[cfg(test)]
    fn for_origin(origin: url::Url) -> Self {
        Self::for_origins(vec![origin])
    }

    fn for_origins(origins: Vec<url::Url>) -> Self {
        // Explicit IP origins authorize only their exact destination. Keep DNS
        // resolution public-only, even when a list mixes IPs and hostnames.
        let mut fetcher = Self::new();
        fetcher.crawl_origins = origins.iter().map(url::Url::origin).collect();
        fetcher
            .robots
            .restrict_to_origins(fetcher.crawl_origins.clone());
        fetcher
    }

    #[cfg(test)]
    fn for_tests() -> Self {
        Self::with_network_policy(true)
    }

    fn with_network_policy(allow_private_networks: bool) -> Self {
        Self {
            client: Self::client_builder_with_policy(allow_private_networks)
                .build()
                .expect("reqwest client"),
            robots: if allow_private_networks {
                robots::RobotsCache::with_limits_allow_private(256, 8 * 1024 * 1024, 16)
            } else {
                Default::default()
            },
            allow_private_networks,
            crawl_origins: Vec::new(),
        }
    }

    async fn robots_allowed(&self, url: &str) -> bool {
        let Ok(url) = robots::crawl_url(url) else {
            return false;
        };
        if !robots::in_scope(&url, &self.crawl_origins) {
            return false;
        }
        self.robots.allowed(&self.client, &url).await
    }

    async fn fetch(&self, url: &str) -> Result<(String, String, String), String> {
        tokio::time::timeout(Duration::from_secs(12), self.fetch_inner(url))
            .await
            .map_err(|_| "fetch deadline exceeded".to_string())?
    }

    async fn fetch_inner(&self, url: &str) -> Result<(String, String, String), String> {
        let mut current = robots::crawl_url(url)?;
        if !robots::in_scope(&current, &self.crawl_origins) {
            return Err("page target is outside --crawl-origin".into());
        }
        if !robots::scoped_target_is_allowed(
            &current,
            self.allow_private_networks,
            &self.crawl_origins,
        ) {
            return Err("target resolves to a private or reserved network address".into());
        }
        let mut seen = HashSet::new();
        let mut final_response = None;
        for step in 0..=robots::MAX_REDIRECTS {
            if !robots::in_scope(&current, &self.crawl_origins) {
                return Err("page target is outside --crawl-origin".into());
            }
            if !seen.insert(current.clone()) {
                return Err("page redirect loop".into());
            }
            if !self.robots_allowed(current.as_str()).await {
                return Err(format!(
                    "robots denied {}",
                    current.origin().ascii_serialization()
                ));
            }
            let response = self
                .client
                .get(current.clone())
                .header("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")
                .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
                .send()
                .await
                .map_err(|e| format!("req {e}"))?;
            if let Some(next) = robots::redirect(&response, &current)? {
                if step == robots::MAX_REDIRECTS {
                    return Err("too many page redirects".into());
                }
                if !robots::scoped_target_is_allowed(
                    &next,
                    self.allow_private_networks,
                    &self.crawl_origins,
                ) {
                    return Err("redirect targets a private or reserved network address".into());
                }
                current = next;
                continue;
            }
            final_response = Some(response);
            break;
        }
        let resp = final_response.expect("non-redirect response or early error");
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
        let raw = bounded_body(resp, MAX_PAGE_BYTES).await?;
        let raw = raw.as_slice();
        let body = match ct_charset.as_deref() {
            Some("gbk") | Some("gb2312") | Some("gb18030") => decode_gbk(raw),
            _ => {
                // HTML meta 探测
                let head = String::from_utf8_lossy(&raw[..raw.len().min(2048)]).to_lowercase();
                if head.contains("charset=gbk")
                    || head.contains("charset=gb2312")
                    || head.contains("charset=gb18030")
                {
                    decode_gbk(raw)
                } else {
                    String::from_utf8_lossy(raw).into_owned()
                }
            }
        };
        Ok((body, ct, current.into()))
    }
}

fn decode_gbk(bytes: &[u8]) -> String {
    // The WHATWG GBK decoder also accepts GB2312 and GB18030. In-process
    // decoding avoids blocking the async worker on two mutually full pipes.
    encoding_rs::GBK.decode(bytes).0.into_owned()
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
async fn main() -> std::process::ExitCode {
    match execute().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("websearch: {error}");
            std::process::ExitCode::from(if error.is::<shutdown::StartupError>() {
                3
            } else {
                1
            })
        }
    }
}

async fn execute() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Result<Vec<_>, _> = std::env::args_os()
        .skip(1)
        .map(|s| s.into_string())
        .collect();
    let options = match args
        .map_err(|_| "arguments must be valid UTF-8".to_string())
        .and_then(cli::parse)
    {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!("{}", cli::HELP);
            return Ok(());
        }
        Err(error) => {
            eprintln!("websearch: {error}");
            std::process::exit(2);
        }
    };
    let prepared = if let Some(path) = &options.manifest {
        match application::prepare(path, options.clone()) {
            Ok(prepared) => Some(prepared),
            Err(error) => {
                eprintln!("websearch: {error}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };
    if options.validate_only {
        return Ok(());
    }
    // Register both signals before any network, storage or deployment side effect.
    let mut signals = shutdown::Signals::install()?;
    let stop = shutdown::Stop::new();
    let operation_stop = stop.clone();
    let operation = async move {
        match prepared {
            Some(prepared) => application::run(prepared, operation_stop).await,
            None => run(options, operation_stop).await,
        }
    };
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => result,
        signal = signals.wait() => {
            signal?;
            stop.request();
            eprintln!("[ws] stop requested: finish the current fetch wave and persist its index");
            match tokio::time::timeout(Duration::from_secs(60), &mut operation).await {
                Ok(result) => result,
                Err(_) => Err("websearch shutdown exceeded 60 seconds; persistence and component retirement are unconfirmed".into()),
            }
        }
    }
}

async fn run(
    options: cli::Options,
    stop: shutdown::Stop,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut workload = workload::Workload::prepare(options).await?;
    let _storage_owner = workload.retain_lease()?;
    let cli::Options {
        gw_addrs,
        serve_only,
        bind_addr,
        wait_secs,
        node_root,
        insecure_development,
        index_data,
        ..
    } = workload.options.clone();
    let erl_addr = gw_addr(&gw_addrs, "erl", "127.0.0.1:19871");
    let ray_addr = gw_addr(&gw_addrs, "ray", "127.0.0.1:19873");
    let jvm_addr = gw_addr(&gw_addrs, "jvm", "127.0.0.1:19872");
    let search_config = index_data
        .map(|data_dir| {
            let mut table = toml::Table::new();
            table.insert("data_dir".into(), toml::Value::String(data_dir));
            toml::to_string(&table).map(String::into_bytes)
        })
        .transpose()?;
    // ── 组网：双模式 ───────────────────────────────────────────────
    //   direct   ——应用主动拨号三网关（erl=/ray=/jvm= host:port，可跨机器）
    //   registry ——--bind 监听，三网关主动反拨注册（生产形态：应用零网关
    //              地址知识；网关可分布任意机器——跨网络部署）
    let mut remote_config = RemoteConfig::tcp("websearch", bind_addr);
    if insecure_development {
        remote_config = remote_config.with_insecure_development();
    }
    let client = RemoteActorSystem::new(remote_config, Arc::new(NoopLookup)).unwrap();
    let mut components = components::Components::default();
    let outcome: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
    let identity = &mut workload.identity;
    client.start().await?;
    if bind_addr.is_some() {
        let local = client.local_addr().expect("bound");
        println!("PARROT_WEBSEARCH_REGISTRY_PORT={}", local.port());
        println!("[ws] 组网模式：registry（应用监听 {local}，等待三网关反拨注册… ≤{wait_secs}s）");
        let need: &[&str] = if serve_only {
            &["jvm-search-1"]
        } else {
            &["erl-gw-1", "ray-gw-1", "jvm-search-1"]
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs);
        loop {
            if stop.requested() { return Ok(()); }
            let missing = client.missing_direct_connections(need);
            if tokio::time::Instant::now() >= deadline {
                eprintln!("[ws] 等待网关注册超时（{wait_secs}s，缺少直连：{missing:?}）");
                return Err(shutdown::StartupError(format!("gateway registration timed out: {missing:?}")).into());
            }
            if missing.is_empty() {
                println!("[ws] 网关已全部注册：{need:?}");
                break;
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + Duration::from_millis(100)),
            )
            .await;
        }
    } else if serve_only {
        let sa: std::net::SocketAddr = jvm_addr.parse().unwrap();
        // serve-only 重试连（网关可能比应用晚起——30s 窗口）
        let mut linked = false;
        for attempt in 1..=15 {
            if stop.requested() { return Ok(()); }
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
            return Err(shutdown::StartupError("JVM connection deadline exceeded".into()).into());
        }
        println!("[ws] connected: jvm-search-1（serve-only）");
    } else {
        for (id, addr) in [
            ("erl-gw-1", &erl_addr),
            ("ray-gw-1", &ray_addr),
            ("jvm-search-1", &jvm_addr),
        ] {
            if stop.requested() { return Ok(()); }
            let sa = addr.parse().unwrap();
            if let Err(error) = client.connect(&NodeAddr::tcp(id, sa)).await {
                return Err(format!("connect {id} at {sa}: {error}").into());
            }
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
        let config = if matches!(&artifact, AdminArtifactRef::Jvm { .. }) { search_config.clone() } else { None };
        async move {
            client
                .deploy_component(
                    &node,
                    ComponentDeploy {
                        name,
                        version: "1.0.0".into(),
                        artifact,
                        instances: AdminInstancePolicy::Singleton,
                        config,
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
                uri: Some(artifact_uri(
                    app_root.join("jvm/target/websearch-jvm-1.0.0.jar"),
                )?),
            },
        )]
    } else {
        vec![
            (
                "erl-gw-1".into(),
                AdminArtifactRef::Beam {
                    app: "frontier".into(),
                    uri: Some(artifact_uri(app_root.join("erlang"))?),
                },
            ),
            (
                "ray-gw-1".into(),
                AdminArtifactRef::PyModule {
                    module: "tokenizer".into(),
                    runtime_env: None,
                    uri: Some(artifact_uri(app_root.join("python"))?),
                },
            ),
            (
                "jvm-search-1".into(),
                AdminArtifactRef::Jvm {
                    main_class: "websearch.search.SearchComponent".into(),
                    coords: None,
                    uri: Some(artifact_uri(
                        app_root.join("jvm/target/websearch-jvm-1.0.0.jar"),
                    )?),
                },
            ),
        ]
    };
    // akka 组件的数据目录经环境变量（WS_DATA）——网关进程继承
    for (node, artifact) in deploys {
        if stop.requested() { return Ok(()); }
        let role = match &artifact {
            AdminArtifactRef::Beam { .. } => "frontier",
            AdminArtifactRef::PyModule { .. } => "tokenizer",
            _ => "search",
        };
        let name = identity.component(role);
        // Retire only this durable application's previous runs. A fresh run's
        // addresses never alias an older incarnation's late business messages.
        let prefix = format!("/user/{}", identity.previous(role));
        match client.stop_component(&node, &prefix).await {
            Ok(()) => println!("[ws] {node}/{name} 已停止，重新部署"),
            Err(parrot_remote::RemoteError::AdminRejected {
                code: parrot_remote::admin_v2::v2_err::COMPONENT_NOT_FOUND,
                ..
            }) => {}
            Err(error) => {
                return Err(format!("stop {node}/{name}: {error}").into());
            }
        }
        identity.deploying(role)?;
        components.deploying(&node, &name);
        let r = match deploy(&node, &name, artifact).await {
            Ok(paths) => paths,
            Err(error) => {
                return Err(format!("deploy {node}/{role} ({name}): {error}").into());
            }
        };
        if r != vec![format!("/user/{name}")] {
            return Err(format!("deploy {node}/{role} returned unexpected component paths: {r:?}").into());
        }
        println!("[ws] deploy {node}/{name} → {r:?}");
    }

    if stop.requested() { return Ok(()); }
    let refs = workload::Refs {
        search: Box::new(client.remote_ref(&format!("parrot://jvm-search-1/jvm/user/{}",identity.component("search")))?),
        frontier: if serve_only { None } else { Some(Box::new(client.remote_ref(&format!("parrot://erl-gw-1/user/{}",identity.component("frontier")))?)) },
        tokenizer: if serve_only { None } else { Some(Box::new(client.remote_ref(&format!("parrot://ray-gw-1/user/{}",identity.component("tokenizer")))?)) },
    };
    workload.run(refs, client.clone(), stop.clone()).await
    }.await;
    stop.request();
    let components_result = components.retire(&client).await;
    let retired = client.shutdown().await;
    // Preserve the primary error (including StartupError's public exit code),
    // while reporting every cleanup error even when another error takes priority.
    for error in [
        components_result.as_ref().err().map(ToString::to_string),
        retired.as_ref().err().map(ToString::to_string),
    ]
    .into_iter()
    .flatten()
    {
        eprintln!("[ws] cleanup: {error}");
    }
    outcome?;
    components_result?;
    retired?;
    println!("[ws] shutdown complete");
    Ok(())
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

fn gw_addr(addrs: &[(String, String)], tag: &str, default: &str) -> String {
    addrs
        .iter()
        .find(|(t, _)| *t == tag)
        .map(|(_, a)| a.clone())
        .unwrap_or_else(|| default.to_string())
}

// Paths belong to the destination gateway. Encode without canonicalizing or
// requiring these remote artifacts to exist on the application machine.
fn artifact_uri(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    reqwest::Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "artifact path must be absolute",
            )
        })
}

#[cfg(test)]
mod artifact_uri_tests {
    #[test]
    fn gateway_paths_keep_reserved_characters_and_unicode_without_local_io() {
        let path = std::env::temp_dir().join("not-created space 中文 #?%/artifact.jar");
        let uri = super::artifact_uri(&path).unwrap();
        assert!(
            uri.contains("%20")
                && uri.contains("%23")
                && uri.contains("%3F")
                && uri.contains("%25")
        );
        let parsed = reqwest::Url::parse(&uri).unwrap();
        assert!(parsed.query().is_none() && parsed.fragment().is_none());
        assert_eq!(parsed.to_file_path().unwrap(), path);
        assert!(super::artifact_uri("relative/artifact.jar").is_err());
    }
}

fn app_root_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

// ═══════════════════════════ Web 查询服务（百度式页面）════════════════════

async fn bind_web_listener(port: u16) -> std::io::Result<(tokio::net::TcpListener, u16)> {
    let last = port.saturating_add(20);
    for candidate in port..=last {
        match tokio::net::TcpListener::bind(("0.0.0.0", candidate)).await {
            Ok(listener) => {
                let actual = listener.local_addr()?.port();
                return Ok((listener, actual));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && candidate < last => {}
            Err(error) => return Err(error),
        }
    }
    unreachable!("the inclusive port range is nonempty")
}

async fn serve_web(
    listener: tokio::net::TcpListener,
    _client: Arc<RemoteActorSystem>,
    searcher: BoxedActorRef,
    stop: shutdown::Stop,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let actual = listener.local_addr().expect("bound web listener").port();
    println!("[ws] Web listening on 0.0.0.0:{actual}");
    let query_count = Arc::new(AtomicU64::new(0));
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let accepted = tokio::select! {
            biased;
            _ = stop.wait() => break,
            _ = connections.join_next(), if !connections.is_empty() => continue,
            accepted = listener.accept(), if connections.len() < 128 => accepted,
        };
        let (mut sock, _) = match accepted {
            Ok(x) => x,
            Err(_) => {
                // Resource exhaustion must neither spin nor silently retire
                // the live Web service while the crawler continues running.
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let searcher = searcher.clone_boxed();
        let qc = query_count.clone();
        let stop = stop.clone();
        connections.spawn(async move {
            let request = async move {
                let mut buf = vec![0u8; 8192];
                let read = tokio::select! {
                    biased;
                    _ = stop.wait() => return,
                    read = sock.read(&mut buf) => read,
                };
                let n = match read {
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
                        // The UI exposes ten pages, with at most 100 ranked hits.
                        // Clamp before arithmetic (p=0 used to underflow).
                        let p = params
                            .get("p")
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(1)
                            .clamp(1, 10) as usize;
                        if q.is_empty() {
                            let _ = sock.write_all(&redirect_home()).await;
                            return;
                        }
                        qc.fetch_add(1, Ordering::Relaxed);
                        let per_page = 10usize;
                        let k = (p * per_page) as u32;
                        let items = match searcher
                            .send(Box::new(WsSearch(enc_search(k, &q))))
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(expect_reply::<WsSearchResult>)
                        {
                            Ok(WsSearchResult(items)) => items,
                            Err(e) => {
                                let body = format!("search error: {e:?}");
                                let _ = sock
                                    .write_all(&http_resp(500, "text/plain; charset=utf-8", &body))
                                    .await;
                                return;
                            }
                        };
                        let page_items: Vec<_> = items
                            .into_iter()
                            .map(|hit| (hit.score, hit.url, hit.title, hit.snippet))
                            .skip((p - 1) * per_page)
                            .take(per_page)
                            .collect();
                        let html = render_results(&q, &page_items, p, per_page);
                        let _ = sock
                            .write_all(&http_resp(200, "text/html; charset=utf-8", &html))
                            .await;
                    }
                    "/stats" => {
                        let result = searcher
                            .send(Box::new(WsHealthz(vec![])))
                            .await
                            .map_err(|e| e.to_string())
                            .and_then(expect_reply::<WsHealthzR>)
                            .and_then(|reply| {
                                serde_json::to_string(&reply.0).map_err(|e| e.to_string())
                            });
                        let response = match result {
                            Ok(json) => http_resp(200, "application/json", &json),
                            Err(error) => http_resp(
                                502,
                                "text/plain; charset=utf-8",
                                &format!("stats error: {error}"),
                            ),
                        };
                        let _ = sock.write_all(&response).await;
                    }
                    "/docs" | "/terms" => {
                        // 浏览页：站点清单（host 聚合）/ 分词表（df 降序）——分页
                        let is_docs = path == "/docs";
                        let off = params
                            .get("p")
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(0)
                            .min(u32::MAX as u64) as u32;
                        let limit: u32 = if is_docs { 100 } else { 300 };
                        let mut req = vec![];
                        put_u32(&mut req, off.saturating_mul(limit));
                        put_u32(&mut req, limit);
                        let r = if is_docs {
                            searcher.send(Box::new(WsListDocs(req))).await
                        } else {
                            searcher.send(Box::new(WsListTerms(req))).await
                        };
                        let html = r.map_err(|e| e.to_string()).and_then(|reply| {
                            if is_docs {
                                expect_reply::<WsListDocsR>(reply).map(|r| render_docs_page(&r.0))
                            } else {
                                expect_reply::<WsListTermsR>(reply).map(|r| render_terms_page(&r.0))
                            }
                        });
                        match html {
                            Ok(html) => {
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
            };
            // A client that never reads/writes cannot retain a task indefinitely.
            let _ = tokio::time::timeout(Duration::from_secs(15), request).await;
        });
    }
    drop(listener);
    while connections.join_next().await.is_some() {}
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
        502 => "Bad Gateway",
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
fn render_docs_page(data: &DocumentPage) -> String {
    let hosts_total = data.hosts;
    let docs_total = data.total_docs;
    let per = 100u64;
    let last_page = hosts_total.div_ceil(per).saturating_sub(1);
    let page = data.offset / per;
    let mut rows = String::new();
    for (i, item) in data.items.iter().enumerate() {
        let sample = safe_link(&item.sample, &item.sample);
        rows.push_str(&format!(
            r#"<tr><td class="n">{}</td><td><b>{}</b></td><td class="n">{}</td><td>{sample}</td></tr>"#,
            data.offset.saturating_add(i as u64).saturating_add(1),
            esc(&item.host), item.pages,
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
fn render_terms_page(data: &TermPage) -> String {
    let total = data.total;
    let per = 300u64;
    let last_page = total.div_ceil(per).saturating_sub(1);
    let page = data.offset / per;
    let mut rows = String::new();
    for (i, item) in data.items.iter().enumerate() {
        let q = urlencode(&item.term);
        rows.push_str(&format!(
            r#"<tr><td class="n">{}</td><td style="font-size:15px">{}</td>
<td class="n">{}</td><td><a href="/search?q={q}">搜索 →</a></td></tr>"#,
            data.offset.saturating_add(i as u64).saturating_add(1),
            esc(&item.term),
            item.df,
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

/// 分页导航（首页/上一页/下一页/末页）。
fn pager_nav(base: &str, page: u64, last: u64) -> String {
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
    let rows: String = items
        .iter()
        .map(|(score, url, title, snippet)| {
            let title = if title.is_empty() { url } else { title };
            let snip = highlight(snippet, q);
            let link = safe_link(url, title);
            let url = esc(url);
            format!(
                r#"<div class="item"><div class="t">{link}</div>
<div class="u">{url}</div><div class="s">{snip}</div>
<div style="font-size:12px;color:#9195A3">score {score:.2}</div></div>"#
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

#[cfg(test)]
mod protocol_tests;

/// Escape both link label and attribute and activate only HTTP(S) URLs.
fn safe_link(url: &str, label: &str) -> String {
    let label = esc(label);
    if reqwest::Url::parse(url)
        .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
    {
        format!(
            r#"<a href="{}" rel="noopener noreferrer">{label}</a>"#,
            esc(url)
        )
    } else {
        label
    }
}

/// Match against raw text once; inserted tags never become input to later matches.
fn highlight(text: &str, query: &str) -> String {
    let terms: Vec<_> = query
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .collect();
    let mut output = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let found = terms
            .iter()
            .filter_map(|term| rest.find(term).map(|offset| (offset, *term)))
            .min_by(|(a, x), (b, y)| a.cmp(b).then_with(|| y.len().cmp(&x.len())));
        match found {
            Some((offset, term)) => {
                output.push_str(&esc(&rest[..offset]));
                output.push_str("<mark>");
                output.push_str(&esc(term));
                output.push_str("</mark>");
                rest = &rest[offset + term.len()..];
            }
            None => {
                output.push_str(&esc(rest));
                break;
            }
        }
    }
    output
}

#[cfg(test)]
mod json_tests;

#[cfg(test)]
#[path = "../../test_support/reply_peer.rs"]
mod reply_peer;

#[cfg(test)]
mod number_tests;

async fn checked_reply<T: Send + 'static>(
    target: &dyn ActorRef,
    operation: &str,
    request: BoxedMessage,
) -> Result<T, String> {
    match target
        .send(request)
        .await
        .map_err(|error| error.to_string())
        .and_then(expect_reply::<T>)
    {
        Ok(reply) => Ok(reply),
        Err(error) => Err(format!("{operation} failed: {error}")),
    }
}

fn confirm_count(
    operation: &str,
    actual: u32,
    expected: std::ops::RangeInclusive<u32>,
) -> Result<(), String> {
    if expected.contains(&actual) {
        Ok(())
    } else {
        Err(format!(
            "{operation} acknowledged {actual}, expected {expected:?}"
        ))
    }
}

async fn enqueue_urls(
    frontier: &dyn ActorRef,
    dedupe: &mut HashSet<String>,
    dedupe_path: &std::path::Path,
    pairs: Vec<(String, u16)>,
) -> Result<usize, String> {
    let mut seen = HashSet::new();
    let fresh: Vec<_> = pairs
        .into_iter()
        .filter(|(url, _)| !dedupe.contains(url) && seen.insert(url.clone()))
        .collect();
    if fresh.is_empty() {
        return Ok(0);
    }
    push_pending(frontier, dedupe, dedupe_path, fresh).await
}

async fn push_pending(
    frontier: &dyn ActorRef,
    dedupe: &mut HashSet<String>,
    dedupe_path: &std::path::Path,
    tasks: Vec<(String, u16)>,
) -> Result<usize, String> {
    if tasks.is_empty() {
        return Ok(0);
    }
    let WsPushAck(accepted) =
        checked_reply(frontier, "enqueue URLs", Box::new(WsPush(enc_push(&tasks)))).await?;
    confirm_count("enqueue URLs", accepted, 0..=tasks.len() as u32)?;
    // Replaying a pending task must reach the new frontier even if it already
    // appears in the compatibility projection. Do not append that URL again.
    let fresh: Vec<_> = tasks
        .into_iter()
        .filter(|(url, _)| !dedupe.contains(url))
        .collect();
    if fresh.is_empty() {
        return Ok(accepted as usize);
    }
    let persisted = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dedupe_path)?;
        for (url, _) in &fresh {
            writeln!(file, "{url}")?;
        }
        file.flush()
    })();
    if let Err(error) = persisted {
        return Err(format!("persist admitted URLs failed: {error}"));
    }
    dedupe.extend(fresh.into_iter().map(|(url, _)| url));
    Ok(accepted as usize)
}

#[cfg(test)]
async fn inject_seeds(
    raw: &[String],
    dedupe: &mut HashSet<String>,
    dedupe_path: &std::path::Path,
    frontier: &dyn ActorRef,
) -> Result<usize, String> {
    let pairs = raw
        .iter()
        .filter_map(|url| normalize_url(url, url))
        .map(|url| (url, 0))
        .collect();
    enqueue_urls(frontier, dedupe, dedupe_path, pairs).await
}

async fn inject_durable(
    raw: &[String],
    journal: &mut crawl_journal::Journal,
    dedupe: &mut HashSet<String>,
    path: &std::path::Path,
    frontier: &dyn ActorRef,
) -> Result<usize, String> {
    let pairs = raw
        .iter()
        .filter_map(|url| normalize_url(url, url))
        .map(|url| (url, 0))
        .collect();
    let fresh = journal.discover(pairs)?;
    enqueue_urls(frontier, dedupe, path, fresh).await
}

async fn commit_tasks(
    searcher: &dyn ActorRef,
    journal: &mut crawl_journal::Journal,
    tasks: &mut Vec<String>,
) -> Result<crawl_journal::Checkpoint, String> {
    let previous = journal
        .checkpoint()
        .ok_or("missing durable crawl identity")?;
    let WsCheckpointAck(checkpoint) = checked_reply(
        searcher,
        "durable crawl checkpoint",
        Box::new(WsCheckpoint(previous.encode())),
    )
    .await?;
    if !previous.accepts(checkpoint) || previous.generation == checkpoint.generation {
        return Err("checkpoint acknowledged a stale or foreign index generation".into());
    }
    journal.complete(checkpoint, tasks.clone())?;
    tasks.clear();
    Ok(checkpoint)
}
