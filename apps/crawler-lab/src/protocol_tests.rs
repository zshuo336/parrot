use super::*;
use parrot_remote::codec_registry::CodecRegistry;
#[path = "../../test_support/reply_peer.rs"]
mod reply_peer;
const KEY: &str = "bin:crawl/FrontierBatch";
fn valid() -> Vec<u8> {
    // One (id=7, url=路径🙂, depth=2), independent of enc_push.
    let mut v = vec![1, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 10, 0, 0, 0];
    v.extend_from_slice("路径🙂".as_bytes());
    v.extend_from_slice(&[2, 0]);
    v
}
#[test]
fn reply_codec_rejects_every_truncated_prefix() {
    let bytes = valid();
    for end in 0..bytes.len() {
        assert!(
            CodecRegistry::global()
                .decode_incoming(KEY, &bytes[..end])
                .is_err(),
            "accepted prefix {end}"
        );
    }
}
#[test]
fn reply_codec_rejects_tail_count_and_invalid_utf8() {
    let mut trailing = valid();
    trailing.push(0);
    let mut count = valid();
    count[..4].copy_from_slice(&2u32.to_le_bytes());
    let mut utf8 = valid();
    utf8[16] = 255;
    for bytes in [trailing, count, utf8, u32::MAX.to_le_bytes().to_vec()] {
        assert!(CodecRegistry::global()
            .decode_incoming(KEY, &bytes)
            .is_err());
    }
}
#[test]
fn reply_codec_preserves_unicode_and_empty_wire_bytes() {
    for bytes in [valid(), vec![0; 4]] {
        let message = CodecRegistry::global()
            .decode_incoming(KEY, &bytes)
            .unwrap();
        let (key, encoded) = CodecRegistry::global().encode_outgoing(&message).unwrap();
        assert_eq!(key, KEY);
        assert_eq!(encoded, bytes);
    }
}
#[tokio::test]
async fn malformed_batch_is_a_real_tcp_error_and_next_reply_still_works() {
    let (bad, good) = reply_peer::malformed_then_valid(
        || Box::new(FrontierNext(vec![1, 0, 0, 0])),
        KEY,
        vec![1, 0, 0, 0],
        vec![0; 4],
    )
    .await;
    assert!(
        bad.is_err(),
        "malformed reply reached business code as success"
    );
    assert!(good.unwrap().is::<FrontierBatch>());
}

#[test]
fn reply_count_budget_precedes_collection_allocation() {
    let count = 65_537u32;
    let mut bytes = count.to_le_bytes().to_vec();
    bytes.resize(4 + count as usize * 14, 0);
    assert!(CodecRegistry::global()
        .decode_incoming(KEY, &bytes)
        .is_err());
}
#[test]
fn wrong_reply_type_is_a_regular_error() {
    assert!(expect_reply::<FrontierBatch>(Box::new(FrontierAck(vec![1, 0, 0, 0]))).is_err());
    let data = CodecRegistry::global()
        .decode_incoming(KEY, &valid())
        .unwrap();
    let FrontierBatch(entries) = expect_reply(data).unwrap();
    assert_eq!(entries, vec![(7, "路径🙂".to_owned(), 2)]);
}

#[test]
fn numeric_reply_shapes_reject_truncation_and_tail() {
    let cases = [
        ("bin:crawl/FrontierAck", vec![1, 0, 0, 0], vec![4]),
        ("bin:crawl/IndexAck", vec![0; 8], vec![4, 8]),
        ("bin:crawl/IndexStatsR", vec![0; 12], vec![12]),
    ];
    for (key, valid, permitted) in cases {
        for length in 0..=valid.len() + 1 {
            let mut bytes = valid.clone();
            bytes.resize(length, 0);
            assert_eq!(
                CodecRegistry::global().decode_incoming(key, &bytes).is_ok(),
                permitted.contains(&length),
                "{key}: length {length}"
            );
        }
    }
}

#[test]
fn health_reply_requires_complete_json_and_unsigned_integer_fields() {
    for bytes in [
        b"".as_slice(),
        b"null",
        b"[]",
        b"{}",
        b"{\"terms\":0,\"postings\":0}",
        b"{\"terms\":-1,\"postings\":0,\"queries\":0}",
        b"{\"terms\":1.5,\"postings\":0,\"queries\":0}",
        b"{\"terms\":\"1\",\"postings\":0,\"queries\":0}",
        b"{\"terms\":18446744073709551616,\"postings\":0,\"queries\":0}",
        b"{\"terms\":0,\"terms\":1,\"postings\":0,\"queries\":0}",
        b"{\"terms\":0,\"postings\":0,\"queries\":0}junk",
        b"\xff",
    ] {
        assert!(
            CodecRegistry::global()
                .decode_incoming("bin:crawl/HealthzR", bytes)
                .is_err(),
            "accepted {bytes:?}"
        );
    }
}

#[test]
fn search_reply_requires_typed_hits_and_unambiguous_ranking() {
    for bytes in [
        b"not json".as_slice(),
        b"{}",
        b"[{}]",
        b"[1]",
        b"[null]",
        b"[{\"doc\":1,\"score\":-1}]",
        b"[{\"doc\":1,\"score\":1.5}]",
        b"[{\"doc\":1,\"score\":null}]",
        b"[{\"doc\":1,\"score\":NaN}]",
        b"[{\"doc\":1,\"score\":1,\"score\":2}]",
        b"[{\"doc\":1,\"score\":1},{\"doc\":1,\"score\":1}]",
        b"[{\"doc\":1,\"score\":1},{\"doc\":2,\"score\":2}]",
        b"[{\"doc\":2,\"score\":1},{\"doc\":1,\"score\":1}]",
        b"[] []",
        b"\xff",
    ] {
        assert!(
            CodecRegistry::global()
                .decode_incoming("bin:crawl/SearchResult", bytes)
                .is_err(),
            "accepted {bytes:?}"
        );
    }
}

#[test]
fn valid_numeric_and_json_replies_preserve_independent_vectors() {
    for (key, bytes) in [
        ("bin:crawl/FrontierAck", b"\x01\x00\x00\x00".as_slice()),
        ("bin:crawl/IndexAck", b"\xff\xff\xff\xff"),
        ("bin:crawl/IndexAck", b"\xff\xff\xff\xff\xff\xff\xff\xff"),
        (
            "bin:crawl/IndexStatsR",
            b"\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff",
        ),
        (
            "bin:crawl/HealthzR",
            b"{ \"queries\" : 0, \"postings\" : 18446744073709551615, \"terms\" : 1 }",
        ),
        ("bin:crawl/SearchResult", b"[]"),
        (
            "bin:crawl/SearchResult",
            b"[ {\"score\":18446744073709551615,\"doc\":-1}, {\"doc\":0,\"score\":1} ]",
        ),
    ] {
        let decoded = CodecRegistry::global().decode_incoming(key, bytes).unwrap();
        let (encoded_key, encoded) = CodecRegistry::global().encode_outgoing(&decoded).unwrap();
        assert_eq!(encoded_key, key);
        assert_eq!(encoded, bytes);
    }
}

#[tokio::test]
async fn malformed_numeric_and_json_replies_fail_over_tcp_then_recover() {
    for (key, valid) in [
        ("bin:crawl/FrontierAck", b"\x01\x00\x00\x00".as_slice()),
        ("bin:crawl/IndexAck", b"\x00\x00\x00\x00\x00\x00\x00\x00"),
        (
            "bin:crawl/IndexStatsR",
            b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
        ),
        (
            "bin:crawl/HealthzR",
            b"{\"terms\":0,\"postings\":0,\"queries\":0}",
        ),
        ("bin:crawl/SearchResult", b"[]"),
    ] {
        let (bad, good) = reply_peer::malformed_then_valid(
            || Box::new(Healthz(vec![])),
            key,
            vec![255],
            valid.to_vec(),
        )
        .await;
        assert!(bad.is_err(), "{key} malformed reply reached business code");
        assert!(good.is_ok(), "{key} valid reply failed after malformed one");
    }
}

#[test]
fn acknowledgements_require_the_selected_dialect_and_actual_counts() {
    assert!(expect_ray_ack(Box::new(IndexAck(vec![0; 8])), 0, 0).is_ok());
    assert!(expect_jvm_ack(Box::new(IndexAck(vec![0; 4])), 0).is_ok());
    assert!(expect_ray_ack(Box::new(IndexAck(vec![0; 4])), 0, 0).is_err());
    assert!(expect_jvm_ack(Box::new(IndexAck(vec![0; 8])), 0).is_err());
    assert!(expect_ray_ack(Box::new(IndexAck(vec![0; 8])), 1, 0).is_err());
    assert!(expect_ray_ack(Box::new(IndexAck(vec![0; 8])), 0, 1).is_err());
    assert!(expect_jvm_ack(Box::new(IndexAck(vec![0; 4])), 1).is_err());
    assert!(expect_frontier_ack(Box::new(FrontierAck(vec![0; 4]))).is_err());
    assert!(expect_frontier_ack(Box::new(FrontierAck(vec![1, 0, 0, 0]))).is_ok());
    assert!(expect_ray_ack(Box::new(FrontierAck(vec![1, 0, 0, 0])), 0, 0).is_err());
    assert!(expect_jvm_ack(Box::new(HealthzR(vec![])), 0).is_err());
}

#[test]
fn canonical_json_rejects_every_truncation_and_respects_item_budget() {
    for (key, bytes) in [
        (
            "bin:crawl/HealthzR",
            b"{\"terms\":0,\"postings\":0,\"queries\":0}".as_slice(),
        ),
        ("bin:crawl/SearchResult", b"[{\"doc\":1,\"score\":1}]"),
    ] {
        for end in 0..bytes.len() {
            assert!(
                CodecRegistry::global()
                    .decode_incoming(key, &bytes[..end])
                    .is_err(),
                "{key} prefix {end}"
            );
        }
    }
    let entries: Vec<_> = (0..65_537)
        .map(|doc| protocol::Hit { doc, score: 1 })
        .collect();
    let bytes = serde_json::to_vec(&entries).unwrap();
    assert!(protocol::decode_search(&bytes)
        .unwrap_err()
        .contains("item budget"));
    let bytes = serde_json::to_vec(&entries[..65_536]).unwrap();
    assert_eq!(protocol::decode_search(&bytes).unwrap().len(), 65_536);
}

#[test]
fn local_ranking_handles_shared_terms_ties_and_empty_results() {
    let index = std::collections::HashMap::from([
        (
            "a".to_owned(),
            std::collections::HashMap::from([(1, 2), (2, 1), (3, 1)]),
        ),
        (
            "b".to_owned(),
            std::collections::HashMap::from([(2, 1), (4, 5)]),
        ),
    ]);
    assert_eq!(
        reference_search(&index, &["a", "b"]),
        vec![
            protocol::Hit { doc: 4, score: 5 },
            protocol::Hit { doc: 1, score: 2 },
            protocol::Hit { doc: 2, score: 2 },
            protocol::Hit { doc: 3, score: 1 },
        ]
    );
    assert!(reference_search(&index, &["absent"]).is_empty());
}
