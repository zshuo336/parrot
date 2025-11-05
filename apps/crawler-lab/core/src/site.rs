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
pub fn url_of(id: u64) -> String {
    format!("https://site.example/doc-{id}")
}

/// 页面 id → 合成 HTML（词表确定性采样——文档间词分布有差异，可检索）
pub fn synth_page(id: u64) -> Vec<u8> {
    let mut rng = Rng(id.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    let n_words = 40 + (rng.next() % 60) as usize;
    let title = VOCAB[(id % VOCAB.len() as u64) as usize];
    let mut body = String::new();
    body.push_str(&format!(
        "<html><head><title>{title} page</title></head><body>"
    ));
    for _ in 0..n_words {
        let w = VOCAB[(rng.next() % VOCAB.len() as u64) as usize];
        body.push_str(w);
        body.push(' ');
    }
    body.push_str("</body></html>");
    body.into_bytes()
}

/// 页面外链（fanout 条，id 空间内确定性跳转）
pub fn outlinks(id: u64, fanout: u64, max_id: u64) -> Vec<u64> {
    if max_id == 0 {
        return Vec::new();
    }
    let mut rng = Rng(id.wrapping_mul(0xBF58476D1CE4E5B9) | 1);
    (0..fanout)
        .map(|_| rng.next() % max_id)
        .filter(|&t| t != id)
        .collect()
}

/// 分词（与 Ray 侧 _tokenize 同语义——Rust 侧用于本地对账）
pub fn tokenize(html: &[u8]) -> Vec<String> {
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
