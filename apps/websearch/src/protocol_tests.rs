use super::*;
use parrot_remote::codec_registry::CodecRegistry;
fn vectors() -> [(&'static str, Vec<u8>); 2] {
    let mut batch = vec![1, 0, 0, 0, 10, 0, 0, 0];
    batch.extend_from_slice("路径🙂".as_bytes());
    batch.extend_from_slice(&[2, 0]);
    let mut terms = vec![1, 0, 0, 0, 6, 0, 0, 0];
    terms.extend_from_slice("词语".as_bytes());
    terms.extend_from_slice(&7u64.to_le_bytes());
    terms.extend_from_slice(&2u32.to_le_bytes());
    [("bin:ws/Batch", batch), ("bin:ws/Terms", terms)]
}
#[test]
fn reply_codecs_reject_every_truncated_prefix() {
    for (key, bytes) in vectors() {
        for end in 0..bytes.len() {
            assert!(
                CodecRegistry::global()
                    .decode_incoming(key, &bytes[..end])
                    .is_err(),
                "{key} accepted prefix {end}"
            );
        }
    }
}
#[test]
fn reply_codecs_reject_tail_count_and_invalid_utf8() {
    for (key, bytes) in vectors() {
        let mut trailing = bytes.clone();
        trailing.push(0);
        let mut count = bytes.clone();
        count[..4].copy_from_slice(&2u32.to_le_bytes());
        let mut utf8 = bytes;
        utf8[8] = 255;
        for bad in [trailing, count, utf8, u32::MAX.to_le_bytes().to_vec()] {
            assert!(
                CodecRegistry::global().decode_incoming(key, &bad).is_err(),
                "accepted {key}"
            );
        }
    }
}
#[test]
fn reply_codecs_preserve_unicode_and_empty_wire_bytes() {
    for (key, valid) in vectors() {
        for bytes in [valid, vec![0; 4]] {
            let message = CodecRegistry::global()
                .decode_incoming(key, &bytes)
                .unwrap();
            let (decoded, encoded) = CodecRegistry::global().encode_outgoing(&message).unwrap();
            assert_eq!(decoded, key);
            assert_eq!(encoded, bytes);
        }
    }
}
#[tokio::test]
async fn malformed_batch_is_a_real_tcp_error_and_next_reply_still_works() {
    let (bad, good) = reply_peer::malformed_then_valid(
        || Box::new(WsNext(vec![1, 0, 0, 0])),
        "bin:ws/Batch",
        vec![1, 0, 0, 0],
        vec![0; 4],
    )
    .await;
    assert!(
        bad.is_err(),
        "malformed reply reached business code as success"
    );
    assert!(good.unwrap().is::<WsBatch>());
}
#[tokio::test]
async fn malformed_terms_is_a_real_tcp_error_and_next_reply_still_works() {
    let (bad, good) = reply_peer::malformed_then_valid(
        || Box::new(WsTokenize(vec![0; 4])),
        "bin:ws/Terms",
        vec![1, 0, 0, 0],
        vec![0; 4],
    )
    .await;
    assert!(
        bad.is_err(),
        "malformed terms reached business code as success"
    );
    assert!(good.unwrap().is::<WsTerms>());
}

#[test]
fn reply_count_budget_precedes_collection_allocation() {
    for (key, width) in [("bin:ws/Batch", 6), ("bin:ws/Terms", 16)] {
        let count = 65_537u32;
        let mut bytes = count.to_le_bytes().to_vec();
        bytes.resize(4 + count as usize * width, 0);
        assert!(CodecRegistry::global()
            .decode_incoming(key, &bytes)
            .is_err());
    }
}
#[test]
fn wrong_reply_type_is_a_regular_error() {
    assert!(expect_reply::<WsBatch>(Box::new(WsFlushAck(1))).is_err());
    assert!(expect_reply::<WsTerms>(Box::new(WsBatch(vec![]))).is_err());
    let data = CodecRegistry::global()
        .decode_incoming("bin:ws/Batch", &vectors()[0].1)
        .unwrap();
    let WsBatch(entries) = expect_reply(data).unwrap();
    assert_eq!(entries, vec![("路径🙂".to_owned(), 2)]);
}
