//! RFC 9309 policy, fetched per HTTP origin with bounded storage and work.
//! <https://www.rfc-editor.org/rfc/rfc9309.html>
use reqwest::{Client, Url};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

pub(super) const PRODUCT: &str = "ParrotWebSearch";
pub(super) const USER_AGENT: &str = "ParrotWebSearch/1.0";
pub(super) const MAX_REDIRECTS: usize = 5;
const MAX_URL_BYTES: usize = 8192;
const MAX_ROBOTS_BYTES: usize = 500 * 1024;

/// Reject destinations that are commonly used to turn a crawler into an
/// internal network proxy. The check is applied to literal targets and to
/// every address returned by the DNS resolver.
pub(super) fn is_public_target(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 0 && c == 2)
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224
                || (a == 255 && b == 255 && c == 255 && d == 255))
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return is_public_target(IpAddr::V4(mapped));
            }
            let segments = ip.segments();
            let first = segments[0];
            let second = segments[1];
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || (first & 0xfe00) == 0xfc00 // fc00::/7, unique local
                || (first & 0xffc0) == 0xfe80 // fe80::/10, link local
                || (first == 0x2001 && second == 0x0db8) // documentation
                || (first == 0x2001 && second == 0x0002) // benchmarking
                || (first == 0x2001 && (second & 0xfff0) == 0x0010)) // ORCHID
        }
    }
}

pub(super) fn target_is_allowed(url: &Url, allow_private: bool) -> bool {
    if allow_private {
        return true;
    }
    match url
        .host_str()
        .map(|host| host.trim_matches(['[', ']']))
        .and_then(|host| IpAddr::from_str(host).ok())
    {
        Some(ip) => is_public_target(ip),
        None => url.host_str().is_some(),
    }
}

/// Empty scope preserves the default public-web policy.
pub(super) fn in_scope(url: &Url, origins: &[url::Origin]) -> bool {
    origins.is_empty() || origins.contains(&url.origin())
}

pub(super) fn scoped_target_is_allowed(
    url: &Url,
    allow_private: bool,
    origins: &[url::Origin],
) -> bool {
    in_scope(url, origins)
        && target_is_allowed(
            url,
            allow_private
                || (!origins.is_empty()
                    && matches!(url.host(), Some(url::Host::Ipv4(_) | url::Host::Ipv6(_)))),
        )
}

/// Resolver used by every production websearch client. Reqwest may resolve a
/// hostname long after URL validation, so checking here closes the DNS
/// rebinding window by rejecting unsafe addresses at the actual connect step.
#[derive(Debug, Clone, Copy)]
pub(super) struct SafeResolver {
    pub(super) allow_private: bool,
}

impl reqwest::dns::Resolve for SafeResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let allow_private = self.allow_private;
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })?
                .collect::<Vec<SocketAddr>>();
            if addresses.is_empty() {
                return Err(Box::new(io::Error::new(
                    io::ErrorKind::NotFound,
                    "DNS returned no addresses",
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            if !allow_private
                && addresses
                    .iter()
                    .any(|address| !is_public_target(address.ip()))
            {
                return Err(Box::new(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "DNS target resolves to a private or reserved address",
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

pub(super) fn crawl_url(value: &str) -> Result<Url, String> {
    if value.len() > MAX_URL_BYTES {
        return Err("URL too long".into());
    }
    let mut url = Url::parse(value).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("crawl URL must be HTTP(S) without credentials".into());
    }
    url.set_fragment(None);
    if url.as_str().len() > MAX_URL_BYTES {
        return Err("URL too long".into());
    }
    Ok(url)
}

pub(super) fn redirect(response: &reqwest::Response, current: &Url) -> Result<Option<Url>, String> {
    if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or("redirect missing valid Location")?;
    resolve(current, location).map(Some)
}

pub(super) fn resolve(base: &Url, location: &str) -> Result<Url, String> {
    if location.len() > MAX_URL_BYTES {
        return Err("target URL too long".into());
    }
    let target = base
        .join(location)
        .map_err(|e| format!("invalid target URL: {e}"))?;
    crawl_url(target.as_str())
}

#[derive(Debug)]
struct Rule {
    pattern: String,
    allow: bool,
    specificity: usize,
}

#[derive(Debug)]
enum Policy {
    Allow,
    Deny,
    Rules(Vec<Rule>),
}
impl Policy {
    fn allows(&self, url: &Url) -> bool {
        let mut path = url.path().to_owned();
        if let Some(query) = url.query() {
            path.push('?');
            path.push_str(query);
        }
        let path = normalize(&path, false);
        match self {
            Self::Allow => true,
            Self::Deny => false,
            Self::Rules(rules) => rules
                .iter()
                .filter(|rule| matches(&rule.pattern, &path))
                .max_by_key(|rule| (rule.specificity, rule.allow))
                .is_none_or(|rule| rule.allow),
        }
    }
    fn weight(&self) -> usize {
        std::mem::size_of::<Self>()
            + match self {
                Self::Rules(rules) => {
                    rules.capacity() * std::mem::size_of::<Rule>()
                        + rules
                            .iter()
                            .map(|rule| rule.pattern.capacity())
                            .sum::<usize>()
                }
                _ => 0,
            }
    }
}

// Preserve encoded reserved octets; decode unreserved octets and encode UTF-8.
// Literal URI '*'/'$' are encoded, while rule metacharacters remain operators.
fn normalize(value: &str, pattern: bool) -> String {
    const HEX: &[u8] = b"0123456789ABCDEF";
    fn hex(value: u8) -> Option<u8> {
        match value {
            b'0'..=b'9' => Some(value - b'0'),
            b'a'..=b'f' => Some(value - b'a' + 10),
            b'A'..=b'F' => Some(value - b'A' + 10),
            _ => None,
        }
    }
    fn unreserved(value: u8) -> bool {
        value.is_ascii_alphanumeric() || b"-._~".contains(&value)
    }
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let escaped = if byte == b'%' && index + 2 < bytes.len() {
            hex(bytes[index + 1])
                .zip(hex(bytes[index + 2]))
                .map(|(high, low)| high * 16 + low)
        } else {
            None
        };
        if let Some(decoded) = escaped {
            if unreserved(decoded) {
                out.push(decoded as char);
            } else {
                out.push('%');
                out.push(HEX[(decoded >> 4) as usize] as char);
                out.push(HEX[(decoded & 15) as usize] as char);
            }
            index += 3;
            continue;
        }
        let operator = pattern && (byte == b'*' || byte == b'$' && index + 1 == bytes.len());
        if byte.is_ascii_graphic() && byte != b'%' && (!b"*$".contains(&byte) || operator) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 15) as usize] as char);
        }
        index += 1;
    }
    out
}

// Ordered literal segments avoid recursive/backtracking regex matching.
fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = pattern
        .strip_suffix('$')
        .map_or((pattern, false), |p| (p, true));
    let mut segments = pattern.split('*').peekable();
    let Some(mut rest) = path.strip_prefix(segments.next().unwrap()) else {
        return false;
    };
    if segments.peek().is_none() {
        return !anchored || rest.is_empty();
    }
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() && anchored {
            return rest.ends_with(segment);
        }
        let Some(position) = rest.find(segment) else {
            return false;
        };
        rest = &rest[position + segment.len()..];
    }
    true
}

fn parse(body: &str) -> Result<Policy, String> {
    if body.len() > MAX_ROBOTS_BYTES {
        return Err("robots body exceeds parsing limit".into());
    }
    #[derive(Default)]
    struct Group {
        specific: bool,
        star: bool,
        has_agent: bool,
        has_rule: bool,
        rules: Vec<Rule>,
    }
    let mut group = Group::default();
    let (mut specific, mut wildcard) = (Vec::new(), Vec::new());
    let mut matched = false;
    let mut finish = |group: &mut Group| {
        if group.specific {
            matched = true;
            specific.append(&mut group.rules);
        } else if group.star {
            wildcard.append(&mut group.rules);
        }
        *group = Group::default();
    };
    for raw in body.trim_start_matches('\u{feff}').lines() {
        let line = raw.split('#').next().unwrap().trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.eq_ignore_ascii_case("user-agent") {
            if group.has_rule {
                finish(&mut group);
            }
            group.has_agent = true;
            group.star |= value == "*";
            group.specific |= value.eq_ignore_ascii_case(PRODUCT);
        } else if group.has_agent
            && (key.eq_ignore_ascii_case("allow") || key.eq_ignore_ascii_case("disallow"))
        {
            group.has_rule = true;
            if value.is_empty() || !value.starts_with('/') {
                continue;
            }
            let pattern = normalize(value, true);
            let specificity = pattern.bytes().filter(|byte| !b"*$".contains(byte)).count();
            group.rules.push(Rule {
                pattern,
                specificity,
                allow: key.eq_ignore_ascii_case("allow"),
            });
        }
    }
    finish(&mut group);
    Ok(Policy::Rules(if matched { specific } else { wildcard }))
}

struct Cached {
    policy: Arc<Policy>,
    expires: Instant,
}
#[derive(Default)]
struct Entry {
    // Per-origin single flight. Cancellation releases it without a detached loader.
    loading: tokio::sync::Mutex<()>,
    value: Mutex<Option<Cached>>,
}
impl Entry {
    fn fresh(&self) -> Option<Arc<Policy>> {
        self.value
            .lock()
            .unwrap()
            .as_ref()
            .filter(|v| v.expires > Instant::now())
            .map(|v| v.policy.clone())
    }
}
struct Slot {
    entry: Arc<Entry>,
    touched: u64,
    weight: usize,
}
#[derive(Default)]
struct Entries {
    slots: HashMap<String, Slot>,
    clock: u64,
    bytes: usize,
}
pub(super) struct RobotsCache {
    entries: Mutex<Entries>,
    loads: tokio::sync::Semaphore,
    capacity: usize,
    byte_capacity: usize,
    allow_private: bool,
    crawl_origins: Vec<url::Origin>,
}
impl Default for RobotsCache {
    fn default() -> Self {
        Self::with_limits(256, 8 * 1024 * 1024, 16)
    }
}
impl RobotsCache {
    pub(super) fn with_limits(capacity: usize, byte_capacity: usize, loads: usize) -> Self {
        Self::with_limits_policy(capacity, byte_capacity, loads, false)
    }
    pub(super) fn with_limits_allow_private(
        capacity: usize,
        byte_capacity: usize,
        loads: usize,
    ) -> Self {
        Self::with_limits_policy(capacity, byte_capacity, loads, true)
    }
    fn with_limits_policy(
        capacity: usize,
        byte_capacity: usize,
        loads: usize,
        allow_private: bool,
    ) -> Self {
        Self {
            entries: Default::default(),
            loads: tokio::sync::Semaphore::new(loads),
            capacity,
            byte_capacity,
            allow_private,
            crawl_origins: Vec::new(),
        }
    }
    pub(super) fn restrict_to_origins(&mut self, origins: Vec<url::Origin>) {
        self.crawl_origins = origins;
    }
    fn evict(entries: &mut Entries) -> bool {
        let victim = entries
            .slots
            .iter()
            .filter(|(_, slot)| Arc::strong_count(&slot.entry) == 1)
            .min_by_key(|(_, slot)| slot.touched)
            .map(|(origin, _)| origin.clone());
        if let Some(victim) = victim {
            entries.bytes -= entries.slots.remove(&victim).unwrap().weight;
            true
        } else {
            false
        }
    }
    fn entry(&self, origin: &str) -> Option<Arc<Entry>> {
        let mut entries = self.entries.lock().unwrap();
        entries.clock = entries.clock.wrapping_add(1);
        let now = entries.clock;
        if let Some(slot) = entries.slots.get_mut(origin) {
            slot.touched = now;
            return Some(slot.entry.clone());
        }
        if self.capacity == 0 {
            return None;
        }
        while entries.slots.len() >= self.capacity {
            if !Self::evict(&mut entries) {
                return None;
            }
        }
        let entry = Arc::new(Entry::default());
        entries.slots.insert(
            origin.into(),
            Slot {
                entry: entry.clone(),
                touched: now,
                weight: 0,
            },
        );
        Some(entry)
    }
    fn store(&self, origin: &str, entry: &Arc<Entry>, policy: Arc<Policy>, ttl: Duration) -> bool {
        let weight = policy.weight();
        if weight > self.byte_capacity {
            return false;
        }
        let mut entries = self.entries.lock().unwrap();
        let old = entries
            .slots
            .get(origin)
            .expect("active cache entry cannot be evicted")
            .weight;
        while entries.bytes - old + weight > self.byte_capacity {
            if !Self::evict(&mut entries) {
                return false;
            }
        }
        entries.bytes = entries.bytes - old + weight;
        entries.slots.get_mut(origin).unwrap().weight = weight;
        *entry.value.lock().unwrap() = Some(Cached {
            policy,
            expires: Instant::now() + ttl,
        });
        true
    }
    pub(super) async fn allowed(&self, client: &Client, url: &Url) -> bool {
        if !in_scope(url, &self.crawl_origins) {
            return false;
        }
        if !scoped_target_is_allowed(url, self.allow_private, &self.crawl_origins) {
            return false;
        }
        if url.path() == "/robots.txt" && url.query().is_none() {
            return true;
        }
        let origin = url.origin().ascii_serialization();
        let Some(entry) = self.entry(&origin) else {
            return false;
        };
        if let Some(policy) = entry.fresh() {
            return policy.allows(url);
        }
        let _loading = entry.loading.lock().await;
        if let Some(policy) = entry.fresh() {
            return policy.allows(url);
        }
        let Ok(_permit) = self.loads.try_acquire() else {
            return false;
        };
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            download(client, url, self.allow_private, &self.crawl_origins),
        )
        .await;
        let (policy, ttl) = match result {
            Ok(Ok(value)) => value,
            _ => (Policy::Deny, Duration::from_secs(30)),
        };
        let policy = Arc::new(policy);
        self.store(&origin, &entry, policy.clone(), ttl) && policy.allows(url)
    }
    #[cfg(test)]
    pub(super) fn stats(&self) -> (usize, usize) {
        let entries = self.entries.lock().unwrap();
        (entries.slots.len(), entries.bytes)
    }
}

async fn download(
    client: &Client,
    target: &Url,
    allow_private: bool,
    crawl_origins: &[url::Origin],
) -> Result<(Policy, Duration), String> {
    let mut url = target.clone();
    url.set_path("/robots.txt");
    url.set_query(None);
    url.set_fragment(None);
    let mut seen = std::collections::HashSet::new();
    for step in 0..=MAX_REDIRECTS {
        if !in_scope(&url, crawl_origins) {
            return Err("robots target is outside --crawl-origin".into());
        }
        if !scoped_target_is_allowed(&url, allow_private, crawl_origins) {
            return Err("robots target resolves to a private or reserved address".into());
        }
        if !seen.insert(url.clone()) {
            return Err("robots redirect loop".into());
        }
        let response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| format!("robots request: {e}"))?;
        if let Some(next) = redirect(&response, &url)? {
            if step == MAX_REDIRECTS {
                return Err("too many robots redirects".into());
            }
            if !scoped_target_is_allowed(&next, allow_private, crawl_origins) {
                return Err("robots redirect targets a private or reserved address".into());
            }
            url = next;
            continue;
        }
        match response.status().as_u16() {
            200..=299 => {
                let body = super::bounded_body(response, MAX_ROBOTS_BYTES).await?;
                let body = std::str::from_utf8(&body).map_err(|_| "robots is not UTF-8")?;
                return Ok((parse(body)?, Duration::from_secs(24 * 60 * 60)));
            }
            400..=499 if !matches!(response.status().as_u16(), 401 | 403 | 429) => {
                return Ok((Policy::Allow, Duration::from_secs(60 * 60)))
            }
            _ => return Err("robots unavailable or refused".into()),
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::dns::Resolve;
    fn allowed(policy: &Policy, path: &str) -> bool {
        policy.allows(&Url::parse(&format!("https://example.test{path}")).unwrap())
    }
    #[test]
    fn groups_merge_and_specific_agent_overrides_wildcard() {
        let policy = parse("Disallow: /\nUser-agent: *\nDisallow: /fallback\nUser-agent: ParrotWebSearch\nUser-agent: OtherBot\nDisallow: /one\nSitemap: https://example.test/map\nDisallow: /two\nUser-agent: parrotwebsearch\nDisallow: /three\n").unwrap();
        for path in ["/one", "/two", "/three"] {
            assert!(!allowed(&policy, path));
        }
        for path in ["/fallback", "/public"] {
            assert!(allowed(&policy, path));
        }
        let policy =
            parse("User-agent: OtherBot\nDisallow: /\nUser-agent: *\nDisallow: /fallback\n")
                .unwrap();
        assert!(!allowed(&policy, "/fallback"));
        assert!(allowed(&policy, "/public"));
    }
    #[test]
    fn most_specific_rule_wins_and_allow_breaks_ties() {
        let policy = parse("User-agent: *\nDisallow: /private\nAllow: /private/open\nDisallow: /private/open/secret\nDisallow: /tie\nAllow: /tie\n").unwrap();
        for (path, expected) in [
            ("/private", false),
            ("/private/open", true),
            ("/private/open/secret", false),
            ("/Private", true),
            ("/tie", true),
        ] {
            assert_eq!(allowed(&policy, path), expected, "{path}");
        }
    }
    #[test]
    fn wildcard_anchor_and_query_rules_are_applied() {
        let policy = parse("User-agent: *\nDisallow: /*.pdf$\nDisallow: /*?token=*\nAllow: /public/*.pdf$\nDisallow: /a*b*c$\n").unwrap();
        for (path, expected) in [
            ("/x.pdf", false),
            ("/x.pdf?download=1", true),
            ("/x?token=secret", false),
            ("/public/a.pdf", true),
            ("/abbbc", false),
            ("/abbbc/extra", true),
        ] {
            assert_eq!(allowed(&policy, path), expected, "{path}");
        }
    }
    #[test]
    fn percent_encoded_and_unicode_paths_follow_octet_matching() {
        let policy = parse("User-agent: *\nDisallow: /你好\nDisallow: /%62ar\nDisallow: /a%2Fb\nDisallow: /literal%2A\nDisallow: /money%24\n").unwrap();
        for path in [
            "/你好",
            "/%E4%BD%A0%E5%A5%BD",
            "/bar",
            "/%62ar",
            "/a%2fb",
            "/literal*",
            "/money$",
        ] {
            assert!(!allowed(&policy, path), "{path}");
        }
        for path in ["/a/b", "/literal-other", "/BAR"] {
            assert!(allowed(&policy, path), "{path}");
        }
    }
    #[test]
    fn empty_disallow_bom_comments_and_invalid_lines_do_not_block_public_urls() {
        let policy = parse("\u{feff}User-agent: *\nDisallow: # empty\nnot a directive\nDisallow: invalid-relative\nDisallow: /private # comment\n").unwrap();
        assert!(allowed(&policy, "/public"));
        assert!(!allowed(&policy, "/private"));
    }
    #[test]
    fn policy_below_rfc_minimum_byte_budget_is_fully_parsed_even_with_many_rules() {
        let body = format!(
            "User-agent: *\n{}Disallow: /private\n",
            "Allow: /x\n".repeat(50_000)
        );
        assert!(body.len() <= MAX_ROBOTS_BYTES);
        let policy = parse(&body).unwrap();
        assert!(!allowed(&policy, "/private"));
        assert!(allowed(&policy, "/public"));
        assert!(parse(&" ".repeat(MAX_ROBOTS_BYTES + 1)).is_err());
    }
    #[test]
    fn default_ports_and_host_case_share_origin_but_schemes_do_not() {
        let a = crawl_url("http://EXAMPLE.test:80/page#fragment").unwrap();
        let b = crawl_url("http://example.test/else").unwrap();
        assert_eq!(a.origin(), b.origin());
        assert_eq!(a.fragment(), None);
        assert_ne!(
            a.origin(),
            crawl_url("https://example.test/page").unwrap().origin()
        );
    }

    #[test]
    fn private_and_reserved_targets_are_rejected() {
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "198.18.0.1",
            "192.0.2.1",
            "[::1]",
            "[::ffff:127.0.0.1]",
            "[fc00::1]",
            "[fe80::1]",
            "[2001:db8::1]",
        ] {
            let url = Url::parse(&format!("http://{host}/")).unwrap();
            assert!(!target_is_allowed(&url, false), "{host} was allowed");
        }
        let public = Url::parse("https://example.com/").unwrap();
        assert!(target_is_allowed(&public, false));
        assert!(target_is_allowed(
            &Url::parse("http://127.0.0.1/").unwrap(),
            true
        ));
    }

    #[tokio::test]
    async fn resolver_rejects_localhost_dns_results_in_production_mode() {
        let name = reqwest::dns::Name::from_str("localhost").unwrap();
        let result = SafeResolver {
            allow_private: false,
        }
        .resolve(name)
        .await;
        assert!(result.is_err(), "localhost must not be a production target");
    }
}
