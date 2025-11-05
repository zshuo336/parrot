use parrot_remote::codec::{PayloadError, PayloadReader};
pub type FrontierEntry = (u64, String, u16);
const MAX_ITEMS: usize = 65_536;
pub fn decode_batch(bytes: &[u8]) -> Result<Vec<FrontierEntry>, PayloadError> {
    let mut r = PayloadReader::new(bytes, parrot_remote::frame::MAX_FRAME_LEN as usize)?;
    let count = r.read_count(MAX_ITEMS, 14)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push((r.read_u64()?, r.read_utf8()?.to_owned(), r.read_u16()?));
    }
    r.finish()?;
    Ok(entries)
}

#[derive(Debug, PartialEq)]
pub enum IndexAcknowledgement {
    Jvm(u32),
    Ray { pages: u32, terms: u32 },
}

pub fn decode_ack(bytes: &[u8]) -> Result<u32, PayloadError> {
    let mut r = PayloadReader::new(bytes, 4)?;
    let value = r.read_u32()?;
    r.finish()?;
    Ok(value)
}

pub fn decode_index_ack(bytes: &[u8]) -> Result<IndexAcknowledgement, PayloadError> {
    let mut r = PayloadReader::new(bytes, 8)?;
    let first = r.read_u32()?;
    let value = if bytes.len() == 4 {
        IndexAcknowledgement::Jvm(first)
    } else {
        IndexAcknowledgement::Ray {
            pages: first,
            terms: r.read_u32()?,
        }
    };
    r.finish()?;
    Ok(value)
}

pub fn decode_stats(bytes: &[u8]) -> Result<(u32, u64), PayloadError> {
    let mut r = PayloadReader::new(bytes, 12)?;
    let value = (r.read_u32()?, r.read_u64()?);
    r.finish()?;
    Ok(value)
}

#[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub terms: u64,
    pub postings: u64,
    pub queries: u64,
}

#[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Hit {
    // JVM Long retains the signed interpretation of the wire doc-id bits.
    pub doc: i64,
    pub score: u64,
}

fn check_json_size(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > parrot_remote::frame::MAX_FRAME_LEN as usize {
        return Err("JSON reply exceeds byte budget".into());
    }
    Ok(())
}

pub fn decode_health(bytes: &[u8]) -> Result<Health, String> {
    check_json_size(bytes)?;
    serde_json::from_slice(bytes).map_err(|e| format!("invalid health reply: {e}"))
}

struct Hits(Vec<Hit>);
impl<'de> serde::Deserialize<'de> for Hits {
    fn deserialize<D: serde::Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Hits;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a bounded array of search hits")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Hits, A::Error> {
                let mut hits = Vec::new();
                while let Some(hit) = seq.next_element::<Hit>()? {
                    if hits.len() == MAX_ITEMS {
                        return Err(serde::de::Error::custom("search reply exceeds item budget"));
                    }
                    hits.push(hit);
                }
                Ok(Hits(hits))
            }
        }
        decoder.deserialize_seq(Visitor)
    }
}

pub fn decode_search(bytes: &[u8]) -> Result<Vec<Hit>, String> {
    check_json_size(bytes)?;
    let Hits(hits) =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid search reply: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    for hit in &hits {
        if !seen.insert(hit.doc) {
            return Err("duplicate document in search reply".into());
        }
    }
    if hits.windows(2).any(|pair| {
        pair[0].score < pair[1].score
            || (pair[0].score == pair[1].score && pair[0].doc > pair[1].doc)
    }) {
        return Err("search reply is not ordered by score descending, doc ascending".into());
    }
    Ok(hits)
}
