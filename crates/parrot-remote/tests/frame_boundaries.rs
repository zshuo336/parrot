//! The Rust decoder is the protocol authority; keep the same boundary corpus
//! exercised by the Python, TypeScript, C++ and JVM implementations.
use bytes::BytesMut;
use parrot_remote::frame::Frame;
use std::{fs, path::PathBuf};

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/vectors/wire1-boundaries.tsv")
}

#[test]
fn shared_wire_boundary_corpus() {
    let text = fs::read_to_string(corpus()).expect("shared wire corpus");
    let mut seen = 0;
    for line in text
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let mut fields = line.split('\t');
        let name = fields.next().unwrap();
        let expected = fields.next().unwrap();
        let bytes = hex(fields.next().unwrap());
        let mut input = BytesMut::from(bytes.as_slice());
        let result = Frame::decode(&mut input);
        match expected {
            "frame" => assert!(result.is_ok_and(|f| f.is_some()), "{name}"),
            "more" => assert!(result.is_ok_and(|f| f.is_none()), "{name}"),
            "reject" => assert!(result.is_err(), "{name}"),
            other => panic!("unknown expectation {other}"),
        }
        seen += 1;
    }
    assert!(seen >= 47);
}

fn hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            assert_eq!(
                pair.len(),
                2,
                "hex corpus entries must contain complete bytes"
            );
            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()
        })
        .collect()
}

#[test]
fn shared_error_payload_corpus() {
    let text = fs::read_to_string(corpus().with_file_name("error-payloads.tsv")).unwrap();
    let mut seen = 0;
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 5);
        let payload = hex(fields[2]);
        let decoded = parrot_remote::error::decode_err_payload(&payload);
        if fields[1] == "reject" {
            assert!(decoded.is_err(), "{} accepted: {decoded:?}", fields[0]);
        } else {
            assert_eq!(fields[1], "accept");
            let (code, detail) = decoded.unwrap();
            assert_eq!(code as u16, fields[3].parse::<u16>().unwrap());
            assert_eq!(detail.as_bytes(), hex(fields[4]));
            assert_eq!(
                parrot_remote::error::encode_err_payload(code, &detail),
                payload
            );
        }
        seen += 1;
    }
    assert_eq!(seen, 30);
}
