// SPDX-License-Identifier: AGPL-3.0-only
use super::Identity;
use crate::{actor_path, error::ErrCode, frame::Frame};
use ring::hmac;

const MAX_BYTES: usize = 32 * 1024;
const MAX_EDGES: usize = 16;
pub(crate) const UNSUPPORTED: &str = "next hop does not support ordered alias routes";

pub(super) fn decode_receipt(
    bytes: &[u8],
    challenge: Identity,
    expected: &str,
) -> Option<(super::OrderedEndpoint, String)> {
    if bytes.len() > 32 + MAX_BYTES {
        return None;
    }
    let (receiver, route) = super::OrderedEndpoint::fields(bytes, challenge)?;
    if head(route) != expected {
        return None;
    }
    let endpoint = super::OrderedEndpoint {
        receiver,
        path: terminal(route).ok()?.to_owned(),
    };
    Some((endpoint, route.to_owned()))
}

// '#' cannot occur in a canonical actor path. Each edge has the form
// current#HMAC#next. Ordinary ordered frames keep their original path.
pub(crate) fn head(path: &str) -> &str {
    path.split_once('#').map_or(path, |(first, _)| first)
}

pub(crate) fn required(frame: &Frame) -> bool {
    match frame.type_key.as_str() {
        super::CONTROL_KEY => {
            frame.path.contains('#')
                || frame
                    .borrowed_ask_payload()
                    .is_ok_and(|p| p.starts_with(&[2, 6]))
        }
        super::ADMIT_KEY => frame.path.contains('#'),
        _ => false,
    }
}

pub(crate) fn terminal(route: &str) -> Result<&str, ErrCode> {
    if route.len() > MAX_BYTES {
        return Err(ErrCode::Overloaded);
    }
    let mut parts = route.split('#');
    let mut path = parts.next().ok_or(ErrCode::ProtocolViolation)?;
    let mut edges = 0;
    loop {
        actor_path::split(path).ok_or(ErrCode::ProtocolViolation)?;
        let Some(tag) = parts.next() else {
            return Ok(path);
        };
        if tag.len() != 64 || !tag.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ErrCode::ProtocolViolation);
        }
        edges += 1;
        if edges > MAX_EDGES {
            return Err(ErrCode::Overloaded);
        }
        path = parts.next().ok_or(ErrCode::ProtocolViolation)?;
    }
}

fn edge_input(source: &str, incarnation: Identity, from: &str, to: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(64 + source.len() + from.len() + to.len());
    bytes.extend_from_slice(b"parrot:ordered-alias-edge#v1\0");
    bytes.extend_from_slice(&incarnation);
    for field in [source, from, to] {
        bytes.extend_from_slice(&(field.len() as u32).to_le_bytes());
        bytes.extend_from_slice(field.as_bytes());
    }
    bytes
}

pub(crate) fn extend(
    route: &str,
    to: &str,
    source: &str,
    incarnation: Identity,
    key: &hmac::Key,
) -> Result<String, ErrCode> {
    let from = terminal(route)?;
    actor_path::split(to).ok_or(ErrCode::ProtocolViolation)?;
    let tag = hmac::sign(key, &edge_input(source, incarnation, from, to));
    let extended = format!("{route}#{}#{to}", hex::encode(tag.as_ref()));
    terminal(&extended)?;
    Ok(extended)
}

pub(crate) fn peel<'a>(
    route: &'a str,
    source: &str,
    incarnation: Identity,
    key: &hmac::Key,
) -> Result<&'a str, ErrCode> {
    terminal(route)?;
    let (from, rest) = route.split_once('#').ok_or(ErrCode::ProtocolViolation)?;
    let (tag, next) = rest.split_once('#').ok_or(ErrCode::ProtocolViolation)?;
    let mut signature = [0; 32];
    hex::decode_to_slice(tag, &mut signature).map_err(|_| ErrCode::ProtocolViolation)?;
    hmac::verify(
        key,
        &edge_input(source, incarnation, from, head(next)),
        &signature,
    )
    .map_err(|_| ErrCode::Forbidden)?;
    Ok(next)
}

// During discovery the current destination precedes the accumulated route.
pub(crate) fn discovery(path: &str, hops: u8) -> Result<(&str, &str), ErrCode> {
    if path.len() > MAX_BYTES + 4097 {
        return Err(ErrCode::Overloaded);
    }
    let (current, trail) = path.split_once('#').unwrap_or((path, path));
    if terminal(trail)? != current || (hops == 0 && current != path) {
        return Err(ErrCode::ProtocolViolation);
    }
    Ok((current, trail))
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "parrot://a/alias";
    const B: &str = "parrot://b/target";

    #[test]
    fn each_edge_requires_its_issuer_origin_incarnation_and_paths() {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &[9; 32]);
        let other = hmac::Key::new(hmac::HMAC_SHA256, &[8; 32]);
        let signed = extend(A, B, "source", [1; 16], &key).unwrap();
        assert_eq!(peel(&signed, "source", [1; 16], &key), Ok(B));
        for result in [
            peel(&signed, "other", [1; 16], &key),
            peel(&signed, "source", [2; 16], &key),
            peel(&signed, "source", [1; 16], &other),
        ] {
            assert_eq!(result, Err(ErrCode::Forbidden));
        }
        for changed in [
            signed.replace("/alias", "/other"),
            signed.replace("/target", "/other"),
        ] {
            assert_eq!(
                peel(&changed, "source", [1; 16], &key),
                Err(ErrCode::Forbidden)
            );
        }
        let receipt = super::super::OrderedEndpoint {
            path: signed.clone(),
            receiver: [2; 16],
        }
        .encode([3; 16]);
        assert_eq!(decode_receipt(&receipt, [3; 16], A).unwrap().0.path, B);
        assert!(decode_receipt(&receipt, [3; 16], B).is_none());
        assert!(decode_receipt(&receipt, [4; 16], A).is_none());
        let mut bytes = receipt.to_vec();
        bytes[16..32].fill(0);
        assert!(decode_receipt(&bytes, [3; 16], A).is_none());
        let discovery_path = format!("{B}#{signed}");
        assert!(discovery(&discovery_path, 0).is_err());
        assert!(discovery(&discovery_path, 1).is_ok());
    }

    #[test]
    fn discovery_rejects_excess_edges_and_bytes_before_publication() {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &[9; 32]);
        let mut route = A.to_owned();
        for _ in 0..MAX_EDGES {
            route = extend(&route, B, "source", [1; 16], &key).unwrap();
        }
        assert_eq!(
            extend(&route, B, "source", [1; 16], &key),
            Err(ErrCode::Overloaded)
        );
        assert_eq!(
            terminal(&"x".repeat(MAX_BYTES + 1)),
            Err(ErrCode::Overloaded)
        );
    }
}
