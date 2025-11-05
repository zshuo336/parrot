//! Deterministic crawler business. The host performs I/O; each component owns
//! its visited set, synthesis, term frequencies, outlinks and tick sequencing.
//! All validation and reply-capacity checks precede state publication.
mod site;
pub use site::{outlinks, synth_page, tokenize, url_of};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_BATCH: usize = 256;
pub type FrontierEntry = (u64, String, u16);
pub type Page = (u64, Vec<u8>);
pub type Term = (String, u64, u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Invalid(&'static str),
    Overflow,
}
impl std::fmt::Display for Error {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) => out.write_str(s),
            Self::Overflow => out.write_str("crawler reply exceeds capacity"),
        }
    }
}
impl std::error::Error for Error {}
type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub pages: u64,
    pub depth: u16,
    pub fanout: u64,
    pub batch: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            pages: 500,
            depth: 2,
            fanout: 3,
            batch: 32,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.pages > 1_000_000 || self.fanout > 256 || self.batch == 0 || self.batch > MAX_BATCH
        {
            return Err(Error::Invalid("crawler settings exceed workload bounds"));
        }
        Ok(())
    }
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 4096 {
            return Err(Error::Invalid("crawler settings exceed byte limit"));
        }
        let value: Self = serde_json::from_slice(bytes)
            .map_err(|_| Error::Invalid("invalid crawler settings"))?;
        value.validate()?;
        Ok(value)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Tick {
    pub sequence: u64,
    pub fetched: u64,
    pub pages: Vec<Page>,
    pub terms: Vec<Term>,
    pub outlinks: Vec<FrontierEntry>,
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::Invalid("crawler payload exceeds byte limit"));
        }
        Ok(Self { bytes, at: 0 })
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(Error::Invalid("truncated crawler payload"))?;
        let result = &self.bytes[self.at..end];
        self.at = end;
        Ok(result)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn text(&mut self) -> Result<String> {
        std::str::from_utf8(self.bytes()?)
            .map(str::to_owned)
            .map_err(|_| Error::Invalid("invalid crawler UTF-8"))
    }
    fn count(&mut self, maximum: usize, minimum: usize) -> Result<usize> {
        let n = self.u32()? as usize;
        if n > maximum || n > (self.bytes.len() - self.at) / minimum {
            return Err(Error::Invalid("crawler item count exceeds budget"));
        }
        Ok(n)
    }
    fn finish(self) -> Result<()> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(Error::Invalid("trailing crawler payload"))
        }
    }
}
fn blob(bytes: &[u8], output: &mut Vec<u8>) -> Result<()> {
    if bytes.len() > MAX_BYTES.saturating_sub(output.len()).saturating_sub(4) {
        return Err(Error::Overflow);
    }
    let len = u32::try_from(bytes.len()).map_err(|_| Error::Overflow)?;
    output.extend_from_slice(&len.to_le_bytes());
    output.extend_from_slice(bytes);
    Ok(())
}
pub fn encode_frontier(entries: &[FrontierEntry]) -> Result<Vec<u8>> {
    if entries.len() > MAX_BATCH * 256 {
        return Err(Error::Invalid("frontier batch exceeds budget"));
    }
    let mut output = (entries.len() as u32).to_le_bytes().to_vec();
    for (id, url, depth) in entries {
        output.extend_from_slice(&id.to_le_bytes());
        blob(url.as_bytes(), &mut output)?;
        output.extend_from_slice(&depth.to_le_bytes());
    }
    if output.len() > MAX_BYTES {
        return Err(Error::Overflow);
    }
    Ok(output)
}
fn frontier(reader: &mut Reader<'_>, maximum: usize) -> Result<Vec<FrontierEntry>> {
    let count = reader.count(maximum, 14)?;
    (0..count)
        .map(|_| Ok((reader.u64()?, reader.text()?, reader.u16()?)))
        .collect()
}
pub fn encode_request(sequence: u64, entries: &[FrontierEntry]) -> Result<Vec<u8>> {
    if entries.len() > MAX_BATCH {
        return Err(Error::Invalid("tick batch exceeds budget"));
    }
    let mut output = sequence.to_le_bytes().to_vec();
    output.extend(encode_frontier(entries)?);
    if output.len() > MAX_BYTES {
        return Err(Error::Overflow);
    }
    Ok(output)
}
impl Tick {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.pages.len() > MAX_BATCH || self.terms.len() > MAX_BATCH * 34 {
            return Err(Error::Invalid("tick reply count exceeds budget"));
        }
        let mut output = b"CTK1".to_vec();
        output.extend_from_slice(&self.sequence.to_le_bytes());
        output.extend_from_slice(&self.fetched.to_le_bytes());
        output.extend_from_slice(&(self.pages.len() as u32).to_le_bytes());
        for (id, html) in &self.pages {
            output.extend_from_slice(&id.to_le_bytes());
            blob(html, &mut output)?;
        }
        output.extend_from_slice(&(self.terms.len() as u32).to_le_bytes());
        for (term, id, tf) in &self.terms {
            blob(term.as_bytes(), &mut output)?;
            output.extend_from_slice(&id.to_le_bytes());
            output.extend_from_slice(&tf.to_le_bytes());
        }
        output.extend(encode_frontier(&self.outlinks)?);
        if output.len() > MAX_BYTES {
            return Err(Error::Overflow);
        }
        Ok(output)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes)?;
        if r.take(4)? != b"CTK1" {
            return Err(Error::Invalid("unsupported crawler tick reply"));
        }
        let sequence = r.u64()?;
        let fetched = r.u64()?;
        let count = r.count(MAX_BATCH, 12)?;
        let pages = (0..count)
            .map(|_| Ok((r.u64()?, r.bytes()?.to_vec())))
            .collect::<Result<Vec<_>>>()?;
        let count = r.count(MAX_BATCH * 34, 16)?;
        let terms = (0..count)
            .map(|_| Ok((r.text()?, r.u64()?, r.u32()?)))
            .collect::<Result<Vec<_>>>()?;
        let outlinks = frontier(&mut r, MAX_BATCH * 256)?;
        r.finish()?;
        Ok(Self {
            sequence,
            fetched,
            pages,
            terms,
            outlinks,
        })
    }
}

pub struct Hub {
    config: Config,
    visited: BTreeSet<u64>,
    ticks: u64,
    handled: u64,
    cached_request: Vec<u8>,
    cached_reply: Vec<u8>,
    drained: bool,
}
impl Hub {
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            visited: BTreeSet::new(),
            ticks: 0,
            handled: 0,
            cached_request: vec![],
            cached_reply: vec![],
            drained: false,
        })
    }
    pub fn drain(&mut self) {
        self.drained = true;
    }
    pub fn handle(&mut self, key: &str, payload: &[u8], capacity: usize) -> Result<Vec<u8>> {
        if payload.len() > MAX_BYTES {
            return Err(Error::Invalid("crawler input exceeds byte limit"));
        }
        if self.drained && !matches!(key, "crawl/Stat" | "crawl/Progress") {
            return Err(Error::Invalid("crawler is drained"));
        }
        let delta = if key == "crawl/Bump" {
            if payload.len() != 1 {
                return Err(Error::Invalid("Bump requires one byte"));
            }
            u64::from(payload[0])
        } else {
            1
        };
        let handled = self
            .handled
            .checked_add(delta)
            .ok_or(Error::Invalid("crawler handled counter overflow"))?;
        let mut admitted = Vec::new();
        let mut tick = false;
        let output = match key {
            "crawl/Echo" => payload.to_vec(),
            "crawl/Bump" => Vec::new(),
            "crawl/Stat" | "crawl/Progress" => {
                if !payload.is_empty() {
                    return Err(Error::Invalid("crawler status requires an empty payload"));
                }
                if key == "crawl/Stat" {
                    handled.to_le_bytes().to_vec()
                } else {
                    [
                        self.ticks.to_le_bytes(),
                        (self.visited.len() as u64).to_le_bytes(),
                    ]
                    .concat()
                }
            }
            "crawl/Tick" => {
                let mut r = Reader::new(payload)?;
                let sequence = r.u64()?;
                if sequence == self.ticks && self.ticks != 0 && payload == self.cached_request {
                    if self.cached_reply.len() > capacity {
                        return Err(Error::Overflow);
                    }
                    return Ok(self.cached_reply.clone());
                }
                if self.ticks.checked_add(1) != Some(sequence) {
                    return Err(Error::Invalid("tick sequence is stale or has a gap"));
                }
                let entries = frontier(&mut r, self.config.batch)?;
                r.finish()?;
                for (id, url, depth) in &entries {
                    if *id >= self.config.pages || *url != url_of(*id) || *depth > self.config.depth
                    {
                        return Err(Error::Invalid(
                            "frontier entry is outside the configured synthetic site",
                        ));
                    }
                }
                let mut pages = Vec::new();
                let mut terms = Vec::new();
                let mut outlinks = Vec::new();
                let mut newly_seen = BTreeSet::new();
                for (id, _, depth) in entries {
                    if self.visited.contains(&id) || !newly_seen.insert(id) {
                        continue;
                    }
                    let html = synth_page(id);
                    let mut frequency = BTreeMap::<String, u32>::new();
                    for term in tokenize(&html) {
                        *frequency.entry(term).or_default() += 1;
                    }
                    for (term, tf) in frequency {
                        terms.push((term, id, tf));
                    }
                    pages.push((id, html));
                    if depth < self.config.depth {
                        for target in site::outlinks(id, self.config.fanout, self.config.pages) {
                            outlinks.push((target, url_of(target), depth + 1));
                        }
                    }
                }
                outlinks
                    .retain(|(id, _, _)| !self.visited.contains(id) && !newly_seen.contains(id));
                admitted.extend(newly_seen);
                let output = Tick {
                    sequence,
                    fetched: (self.visited.len() + admitted.len()) as u64,
                    pages,
                    terms,
                    outlinks,
                }
                .encode()?;
                tick = true;
                output
            }
            _ => return Err(Error::Invalid("unknown crawler message")),
        };
        if output.len() > capacity {
            return Err(Error::Overflow);
        }
        // No fallible protocol operations remain after this publication point.
        if tick {
            self.visited.extend(admitted);
            self.ticks += 1;
            self.cached_request = payload.to_vec();
            self.cached_reply = output.clone();
        }
        self.handled = handled;
        Ok(output)
    }
}

#[cfg(test)]
mod tests;
