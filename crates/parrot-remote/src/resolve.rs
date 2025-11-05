//! Bounded wire payloads for the federation resolve control plane.
//!
//! These payloads deliberately use a small fixed schema instead of the
//! application codec registry.  They are received before an actor/type key is
//! known, and every decoder consumes the complete input before returning.

use crate::roles::directory::DirEntry;

pub const VERSION: u8 = 1;
pub const MAX_KEY: usize = 4096;
pub const MAX_ENDPOINTS: usize = 64;
pub const MAX_ENDPOINT: usize = 2048;
pub const MAX_PAYLOAD: usize = 128 * 1024;
pub const MAX_TTL_S: u32 = 31_536_000;
pub const RESPONSE_KEY: &str = "parrot:resolve#v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub key: String,
    pub min_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub key: String,
    pub entry: Option<DirEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalidate {
    pub key: String,
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("resolve payload exceeds {0} bytes")]
    TooLarge(usize),
    #[error("resolve payload is truncated")]
    Truncated,
    #[error("resolve payload has unsupported version {0}")]
    Version(u8),
    #[error("resolve payload has invalid UTF-8")]
    Utf8,
    #[error("resolve payload has invalid {0} length")]
    Length(&'static str),
    #[error("resolve payload has trailing bytes")]
    Trailing,
}

fn check_len(len: usize) -> Result<(), Error> {
    if len > MAX_PAYLOAD {
        Err(Error::TooLarge(len))
    } else {
        Ok(())
    }
}

fn put_string(out: &mut Vec<u8>, value: &str, what: &'static str) -> Result<(), Error> {
    if value.len() > MAX_KEY && what == "key" {
        return Err(Error::Length(what));
    }
    if value.len() > MAX_ENDPOINT && what == "endpoint" {
        return Err(Error::Length(what));
    }
    let len = u16::try_from(value.len()).map_err(|_| Error::Length(what))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn read_string<'a>(
    input: &'a [u8],
    offset: &mut usize,
    what: &'static str,
    max: usize,
) -> Result<&'a str, Error> {
    let end_len = offset.checked_add(2).ok_or(Error::Truncated)?;
    if end_len > input.len() {
        return Err(Error::Truncated);
    }
    let len = u16::from_le_bytes([input[*offset], input[*offset + 1]]) as usize;
    *offset = end_len;
    if len > max {
        return Err(Error::Length(what));
    }
    let end = offset.checked_add(len).ok_or(Error::Truncated)?;
    if end > input.len() {
        return Err(Error::Truncated);
    }
    let value = std::str::from_utf8(&input[*offset..end]).map_err(|_| Error::Utf8)?;
    *offset = end;
    Ok(value)
}

pub fn encode_query(query: &Query) -> Result<Vec<u8>, Error> {
    let mut out = vec![VERSION, 0];
    put_string(&mut out, &query.key, "key")?;
    out.extend_from_slice(&query.min_version.to_le_bytes());
    check_len(out.len())?;
    Ok(out)
}

pub fn decode_query(input: &[u8]) -> Result<Query, Error> {
    check_len(input.len())?;
    if input.len() < 2 || input[0] != VERSION {
        return Err(if input.is_empty() {
            Error::Truncated
        } else {
            Error::Version(input[0])
        });
    }
    if input[1] != 0 {
        return Err(Error::Length("query flags"));
    }
    let mut offset = 2;
    let key = read_string(input, &mut offset, "key", MAX_KEY)?.to_owned();
    let end = offset.checked_add(8).ok_or(Error::Truncated)?;
    if end > input.len() {
        return Err(Error::Truncated);
    }
    let min_version = u64::from_le_bytes(input[offset..end].try_into().unwrap());
    if end != input.len() {
        return Err(Error::Trailing);
    }
    Ok(Query { key, min_version })
}

pub fn encode_response(response: &Response) -> Result<Vec<u8>, Error> {
    let mut out = vec![VERSION, u8::from(response.entry.is_some())];
    put_string(&mut out, &response.key, "key")?;
    if let Some(entry) = &response.entry {
        out.extend_from_slice(&entry.version.to_le_bytes());
        out.extend_from_slice(&entry.ttl_s.to_le_bytes());
        if entry.ttl_s > MAX_TTL_S {
            return Err(Error::Length("ttl"));
        }
        let count = u16::try_from(entry.endpoints.len()).map_err(|_| Error::Length("endpoints"))?;
        if entry.endpoints.len() > MAX_ENDPOINTS {
            return Err(Error::Length("endpoints"));
        }
        out.extend_from_slice(&count.to_le_bytes());
        for endpoint in &entry.endpoints {
            put_string(&mut out, endpoint, "endpoint")?;
        }
    }
    check_len(out.len())?;
    Ok(out)
}

pub fn decode_response(input: &[u8]) -> Result<Response, Error> {
    check_len(input.len())?;
    if input.len() < 2 || input[0] != VERSION {
        return Err(if input.is_empty() {
            Error::Truncated
        } else {
            Error::Version(input[0])
        });
    }
    if input[1] > 1 {
        return Err(Error::Length("response status"));
    }
    let mut offset = 2;
    let key = read_string(input, &mut offset, "key", MAX_KEY)?.to_owned();
    let entry = if input[1] == 0 {
        None
    } else {
        let fixed = offset.checked_add(8 + 4 + 2).ok_or(Error::Truncated)?;
        if fixed > input.len() {
            return Err(Error::Truncated);
        }
        let version = u64::from_le_bytes(input[offset..offset + 8].try_into().unwrap());
        offset += 8;
        let ttl_s = u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap());
        if ttl_s > MAX_TTL_S {
            return Err(Error::Length("ttl"));
        }
        offset += 4;
        let count = u16::from_le_bytes(input[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        if count > MAX_ENDPOINTS {
            return Err(Error::Length("endpoints"));
        }
        let mut endpoints = Vec::with_capacity(count);
        for _ in 0..count {
            endpoints.push(read_string(input, &mut offset, "endpoint", MAX_ENDPOINT)?.to_owned());
        }
        Some(DirEntry {
            endpoints,
            version,
            ttl_s,
        })
    };
    if offset != input.len() {
        return Err(Error::Trailing);
    }
    Ok(Response { key, entry })
}

pub fn encode_invalidate(value: &Invalidate) -> Result<Vec<u8>, Error> {
    let mut out = vec![VERSION];
    put_string(&mut out, &value.key, "key")?;
    out.extend_from_slice(&value.version.to_le_bytes());
    check_len(out.len())?;
    Ok(out)
}

pub fn decode_invalidate(input: &[u8]) -> Result<Invalidate, Error> {
    check_len(input.len())?;
    if input.first().copied() != Some(VERSION) {
        return Err(if let Some(v) = input.first() {
            Error::Version(*v)
        } else {
            Error::Truncated
        });
    }
    let mut offset = 1;
    let key = read_string(input, &mut offset, "key", MAX_KEY)?.to_owned();
    let end = offset.checked_add(8).ok_or(Error::Truncated)?;
    if end > input.len() {
        return Err(Error::Truncated);
    }
    let version = u64::from_le_bytes(input[offset..end].try_into().unwrap());
    if end != input.len() {
        return Err(Error::Trailing);
    }
    Ok(Invalidate { key, version })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_trailing_rejection() {
        let q = Query {
            key: "n1".into(),
            min_version: 4,
        };
        assert_eq!(decode_query(&encode_query(&q).unwrap()).unwrap(), q);
        let r = Response {
            key: "n1".into(),
            entry: Some(DirEntry {
                endpoints: vec!["tcp://127.0.0.1:1".into()],
                version: 5,
                ttl_s: 60,
            }),
        };
        let mut bytes = encode_response(&r).unwrap();
        bytes.push(0);
        assert!(matches!(decode_response(&bytes), Err(Error::Trailing)));
    }

    #[test]
    fn bounds_and_utf8_are_rejected() {
        let q = Query {
            key: "x".repeat(MAX_KEY + 1),
            min_version: 0,
        };
        assert!(encode_query(&q).is_err());
        assert!(matches!(
            decode_invalidate(&[VERSION, 1, 0, 0xff]),
            Err(Error::Utf8)
        ));
        let too_long = Response {
            key: "x".into(),
            entry: Some(DirEntry {
                endpoints: vec![],
                version: 1,
                ttl_s: MAX_TTL_S + 1,
            }),
        };
        assert!(matches!(
            encode_response(&too_long),
            Err(Error::Length("ttl"))
        ));
    }
}
