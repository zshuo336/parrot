use parrot_api::message::{serde_remote_deserialize, serde_remote_serialize};
use parrot_remote::pb::AkkaEnvelope;
use prost::Message;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn unhex(text: &str) -> Vec<u8> {
    let (pairs, remainder) = text.as_bytes().as_chunks::<2>();
    assert!(
        remainder.is_empty(),
        "odd length in independent test fixture"
    );
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn row(out: &mut String, kind: &str, name: &str, bytes: &[u8], expected: Option<Vec<u8>>) {
    out.push_str(&format!(
        "{kind}\t{name}\t{}\t{}\n",
        hex(bytes),
        expected.map(|b| hex(&b)).unwrap_or_else(|| "ERR".into())
    ));
}
pub fn generate() -> String {
    let mut out=String::from("# Rust bincode standard u64 + checked increment; prost AkkaEnvelope decode/canonical encode.\n# kind\tname\tinput_hex\texpected_hex_or_ERR\n");
    for value in [
        0,
        1,
        41,
        126,
        127,
        128,
        129,
        249,
        250,
        251,
        252,
        65534,
        65535,
        65536,
        u32::MAX as u64 - 1,
        u32::MAX as u64,
        u32::MAX as u64 + 1,
        i64::MAX as u64 - 1,
        i64::MAX as u64,
        i64::MAX as u64 + 1,
        u64::MAX - 1,
        u64::MAX,
    ] {
        let bytes = serde_remote_serialize(&value).unwrap();
        row(
            &mut out,
            "cpu",
            &value.to_string(),
            &bytes,
            value
                .checked_add(1)
                .map(|v| serde_remote_serialize(&v).unwrap()),
        );
    }
    for (name, text) in [
        ("empty", ""),
        ("trailing", "0100"),
        ("short-u16", "fb01"),
        ("short-u32", "fc010000"),
        ("short-u64", "fd01000000000000"),
        ("u128-tag", "fe00000000000000000000000000000000"),
        ("reserved-tag", "ff"),
        ("wide-u16-one", "fb0100"),
        ("wide-u32-one", "fc01000000"),
        ("wide-u64-one", "fd0100000000000000"),
    ] {
        let bytes = unhex(text);
        let expected = serde_remote_deserialize::<u64>(&bytes)
            .ok()
            .and_then(|v| v.checked_add(1))
            .map(|v| serde_remote_serialize(&v).unwrap());
        row(&mut out, "cpu", name, &bytes, expected);
    }
    for (name, message) in [
        ("empty", AkkaEnvelope::default()),
        (
            "simple",
            AkkaEnvelope {
                akka_path: "a".into(),
                kind: "b".into(),
                payload: vec![3],
            },
        ),
        (
            "unicode",
            AkkaEnvelope {
                akka_path: "用户/".repeat(48),
                kind: "raw".into(),
                payload: (0..=255).collect(),
            },
        ),
    ] {
        let bytes = message.encode_to_vec();
        row(&mut out, "pb", name, &bytes, Some(bytes.clone()));
    }
    for (name, text) in [
        ("duplicate-path", "0a01610a0162"),
        ("unknown-varint", "2001"),
        ("unknown-u64-max", "28ffffffffffffffffff01"),
        ("unknown-fixed64", "210000000000000000"),
        ("unknown-fixed32", "2500000000"),
        ("unknown-bytes", "220161"),
        ("unknown-group", "23200124"),
        ("max-field", "f8ffffff0f01"),
        ("explicit-empty", "0a0012001a00"),
        ("wide-zero-length", "0a8000"),
        ("zero-tag", "00"),
        ("truncated-string", "0a0261"),
        ("invalid-utf8", "0a01ff"),
        ("truncated-length", "0a80"),
        ("huge-length", "0affffffffffffffffff01"),
        ("wide-tag", "8a808080100161"),
        ("wrapped-length", "0a818080801061"),
        ("duplicate-payload", "1a01611a0162"),
        ("top-end-group", "24"),
        ("unclosed-group", "23"),
        ("wrong-end-group", "232c"),
        ("bad-wire-six", "26"),
        ("bad-wire-seven", "27"),
        ("known-wrong-wire", "0801"),
        ("u64-overflow", "20ffffffffffffffffff02"),
        ("unterminated-value", "208080808080808080808000"),
    ] {
        let bytes = unhex(text);
        let expected = AkkaEnvelope::decode(bytes.as_slice())
            .ok()
            .map(|v| v.encode_to_vec());
        row(&mut out, "pb", name, &bytes, expected);
    }
    for depth in [98, 99, 100, 101] {
        let mut bytes = vec![35; depth];
        bytes.extend(vec![36; depth]);
        let expected = AkkaEnvelope::decode(bytes.as_slice())
            .ok()
            .map(|v| v.encode_to_vec());
        row(&mut out, "pb", &format!("groups-{depth}"), &bytes, expected);
    }
    for (name, text) in [
        ("utf8-overlong", "0a02c080"),
        ("utf8-surrogate", "0a03eda080"),
        ("utf8-above-max", "0a04f4908080"),
        ("utf8-partial", "0a02e282"),
    ] {
        let bytes = unhex(text);
        let expected = AkkaEnvelope::decode(bytes.as_slice())
            .ok()
            .map(|v| v.encode_to_vec());
        row(&mut out, "pb", name, &bytes, expected);
    }
    let seed = AkkaEnvelope {
        akka_path: "a".into(),
        kind: "b".into(),
        payload: vec![3],
    }
    .encode_to_vec();
    for (index, _) in seed.iter().enumerate() {
        for replacement in [0, 127, 128, 255] {
            let mut bytes = seed.clone();
            bytes[index] = replacement;
            let expected = AkkaEnvelope::decode(bytes.as_slice())
                .ok()
                .map(|v| v.encode_to_vec());
            row(
                &mut out,
                "pb",
                &format!("mutation-{index}-{replacement}"),
                &bytes,
                expected,
            );
        }
    }
    out
}
