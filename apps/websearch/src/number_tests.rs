use super::*;
use parrot_remote::codec_registry::CodecRegistry;

macro_rules! strict_width {
    ($name:ident, $key:literal, $size:expr) => {
        #[test]
        fn $name() {
            let valid = vec![0; $size];
            let registry = CodecRegistry::global();
            assert!(registry.decode_incoming($key, &valid).is_ok());
            for length in 0..$size {
                assert!(
                    registry.decode_incoming($key, &valid[..length]).is_err(),
                    "{} accepted truncated length {}",
                    $key,
                    length
                );
            }
            let mut extra = valid;
            extra.push(0);
            assert!(
                registry.decode_incoming($key, &extra).is_err(),
                "{} accepted trailing byte",
                $key
            );
        }
    };
}
strict_width!(push_ack_has_exact_width, "bin:ws/PushAck", 4);
strict_width!(size_reply_has_exact_width, "bin:ws/SizeR", 8);
strict_width!(index_ack_has_exact_width, "bin:ws/IndexAck", 4);
strict_width!(doc_ack_has_exact_width, "bin:ws/DocAck", 4);
strict_width!(documents_ack_has_exact_width, "bin:ws/DocumentsAck", 4);
strict_width!(flush_ack_has_exact_width, "bin:ws/FlushAck", 4);
strict_width!(clear_ack_has_exact_width, "bin:ws/ClearAck", 4);
strict_width!(index_stats_has_exact_width, "bin:ws/IndexStatsR", 12);

#[tokio::test]
async fn truncated_numeric_reply_is_a_tcp_error_and_later_valid_reply_succeeds() {
    let (bad, good) = reply_peer::malformed_then_valid(
        || Box::new(WsFlush(vec![])),
        "bin:ws/FlushAck",
        vec![],
        vec![1, 0, 0, 0],
    )
    .await;
    assert!(
        bad.is_err(),
        "truncated FlushAck reached business code as success"
    );
    assert!(good.unwrap().is::<WsFlushAck>());
}

#[test]
fn numeric_reply_roundtrips_preserve_values_and_unsigned_extremes() {
    for (key, bytes) in [
        ("bin:ws/PushAck", u32::MAX.to_le_bytes().to_vec()),
        ("bin:ws/IndexAck", 42u32.to_le_bytes().to_vec()),
        ("bin:ws/DocAck", 1u32.to_le_bytes().to_vec()),
        ("bin:ws/DocumentsAck", 8u32.to_le_bytes().to_vec()),
        ("bin:ws/FlushAck", 7u32.to_le_bytes().to_vec()),
        ("bin:ws/ClearAck", 0u32.to_le_bytes().to_vec()),
        (
            "bin:ws/SizeR",
            [23u32.to_le_bytes(), u32::MAX.to_le_bytes()].concat(),
        ),
        (
            "bin:ws/IndexStatsR",
            [
                u32::MAX.to_le_bytes().as_slice(),
                u64::MAX.to_le_bytes().as_slice(),
            ]
            .concat(),
        ),
    ] {
        let value = CodecRegistry::global()
            .decode_incoming(key, &bytes)
            .unwrap();
        let (encoded_key, encoded) = CodecRegistry::global().encode_outgoing(&value).unwrap();
        assert_eq!(encoded_key, key);
        assert_eq!(encoded, bytes);
        if key == "bin:ws/SizeR" {
            assert_eq!(value.downcast_ref::<WsSizeR>().unwrap().0, (23, u32::MAX));
        }
        if key == "bin:ws/IndexStatsR" {
            assert_eq!(
                value.downcast_ref::<WsIndexStatsR>().unwrap().0,
                (u32::MAX, u64::MAX)
            );
        }
    }
}

#[tokio::test]
async fn rejected_push_does_not_mark_a_url_seen_or_create_a_journal() {
    reply_peer::with_script(
        "bin:ws/PushAck",
        vec![vec![]],
        |_client, target| async move {
            let directory = JournalDirectory::new();
            let file = directory.file();
            assert!(!file.exists());
            let mut seen = HashSet::new();
            let result =
                inject_seeds(&["https://example.test/".into()], &mut seen, &file, &target).await;
            assert!(result.unwrap_err().contains("enqueue URLs"));
            assert!(seen.is_empty());
            assert!(!file.exists());
        },
    )
    .await;
}

#[tokio::test]
async fn accepted_push_records_each_url_once_and_returns_actual_admission_count() {
    reply_peer::with_script(
        "bin:ws/PushAck",
        vec![1u32.to_le_bytes().to_vec()],
        |_client, target| async move {
            let directory = JournalDirectory::new();
            let file = directory.file();
            assert!(!file.exists());
            let mut seen = HashSet::new();
            let result = inject_seeds(
                &[
                    "https://example.test/".into(),
                    "https://example.test/".into(),
                ],
                &mut seen,
                &file,
                &target,
            )
            .await;
            let journal = std::fs::read_to_string(&file).unwrap();
            std::fs::remove_file(&file).unwrap();
            assert_eq!(result.unwrap(), 1);
            assert_eq!(seen.len(), 1);
            assert_eq!(journal, "https://example.test/\n");
        },
    )
    .await;
}

#[tokio::test]
async fn wrong_numeric_reply_type_is_an_operation_error() {
    reply_peer::with_script(
        "bin:ws/FlushAck",
        vec![1u32.to_le_bytes().to_vec()],
        |_client, target| async move {
            let result: Result<WsDocAck, _> =
                checked_reply(&target, "write document", Box::new(WsDocMeta(vec![]))).await;
            assert!(result.unwrap_err().contains("unexpected gateway reply"));
        },
    )
    .await;
}

struct JournalDirectory(std::path::PathBuf);
impl JournalDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "parrot-number-contract-{}-{nonce}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn file(&self) -> std::path::PathBuf {
        self.0.join("dedupe.tsv")
    }
}
impl Drop for JournalDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn impossible_push_count_and_journal_failure_do_not_commit_memory_state() {
    for invalid_count in [true, false] {
        let count = if invalid_count { u32::MAX } else { 1 };
        reply_peer::with_script(
            "bin:ws/PushAck",
            vec![count.to_le_bytes().to_vec()],
            |_client, target| async move {
                let directory = JournalDirectory::new();
                let file = directory.file();
                if !invalid_count {
                    std::fs::create_dir(&file).unwrap();
                }
                let mut seen = HashSet::new();
                let error =
                    inject_seeds(&["https://example.test/".into()], &mut seen, &file, &target)
                        .await
                        .unwrap_err();
                assert!(
                    error.contains(if invalid_count {
                        "acknowledged"
                    } else {
                        "persist admitted URLs"
                    }),
                    "{error}"
                );
                assert!(seen.is_empty());
                if invalid_count {
                    assert!(!file.exists());
                }
            },
        )
        .await;
    }
}

#[tokio::test]
async fn remote_duplicate_is_a_valid_zero_admission_acknowledgement() {
    reply_peer::with_script(
        "bin:ws/PushAck",
        vec![0u32.to_le_bytes().to_vec()],
        |_client, target| async move {
            let directory = JournalDirectory::new();
            let mut seen = HashSet::new();
            let admitted = inject_seeds(
                &["https://example.test/".into()],
                &mut seen,
                &directory.file(),
                &target,
            )
            .await
            .unwrap();
            assert_eq!(admitted, 0);
            assert_eq!(seen.len(), 1);
            assert_eq!(
                std::fs::read_to_string(directory.file()).unwrap(),
                "https://example.test/\n"
            );
        },
    )
    .await;
}

#[tokio::test]
async fn pending_replay_reaches_new_frontier_without_growing_legacy_projection() {
    reply_peer::with_script(
        "bin:ws/PushAck",
        vec![1u32.to_le_bytes().to_vec()],
        |_client, target| async move {
            let directory = JournalDirectory::new();
            let path = directory.file();
            let url = "https://example.test/pending".to_string();
            std::fs::write(&path, format!("{url}\n")).unwrap();
            let before = std::fs::read(&path).unwrap();
            let mut projection = HashSet::from([url.clone()]);
            assert_eq!(
                push_pending(&target, &mut projection, &path, vec![(url, 3)])
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(std::fs::read(path).unwrap(), before);
        },
    )
    .await;
}
