use parrot_remote::durable::{DedupDecision, DedupError, DedupLimits, DedupStats, DedupTable};

#[test]
fn default_limit_rejects_unbounded_sequence_gaps() {
    let mut table = DedupTable::new();
    for seq in 2..=65_537 {
        assert_eq!(table.filter("s", seq), Ok(true));
    }
    assert!(table.filter("s", 65_538).is_err());
}

#[test]
fn default_limit_rejects_unbounded_sender_history() {
    let mut table = DedupTable::new();
    for n in 0..4096 {
        assert_eq!(table.filter(&format!("s{n}"), 1), Ok(true));
    }
    assert!(table.filter("another", 1).is_err());
}

#[test]
fn oversized_sender_identity_is_not_retained() {
    let mut table = DedupTable::new();
    assert!(table.filter(&"s".repeat(1025), 1).is_err());
}

#[test]
fn invalid_identity_is_an_error_not_a_successful_duplicate() {
    let mut table = DedupTable::new();
    assert!(table.filter("s", 0).is_err());
    assert!(table.filter("", 1).is_err());
}

fn small() -> DedupTable {
    DedupTable::with_limits(DedupLimits {
        max_senders: 2,
        max_pending_sequences: 2,
        max_sender_bytes: 4,
        max_sender_length: 3,
    })
    .unwrap()
}

fn complete(table: &mut DedupTable, sender: &str, seq: u64) -> u64 {
    match table.begin(sender, seq).unwrap() {
        DedupDecision::Process(permit) => permit.commit(),
        DedupDecision::Duplicate { .. } => panic!("expected new work"),
    }
}

#[test]
fn full_budget_preserves_duplicates_and_allows_missing_prefix_to_make_progress() {
    let mut table = small();
    assert_eq!(complete(&mut table, "s", 3), 0);
    assert_eq!(complete(&mut table, "s", 2), 0);
    let before = table.stats();
    assert_eq!(table.filter("s", 4), Err(DedupError::SequenceLimit));
    assert_eq!(table.stats(), before);
    assert_eq!(table.filter("s", 2), Ok(false));
    assert_eq!(complete(&mut table, "s", 1), 3);
    assert_eq!(table.stats().pending_sequences, 0);
    assert_eq!(complete(&mut table, "s", 4), 4);
}

#[test]
fn sender_and_utf8_byte_budgets_reject_without_eviction_or_mutation() {
    let mut table = small();
    assert_eq!(complete(&mut table, "abc", 1), 1);
    let before = table.stats();
    assert_eq!(table.filter("é", 1), Err(DedupError::SenderBytesLimit));
    assert_eq!(table.stats(), before);
    assert_eq!(table.filter("abcd", 1), Err(DedupError::SenderTooLong));
    assert_eq!(complete(&mut table, "d", 1), 1);
    assert_eq!(table.stats().sender_bytes, 4);
    assert_eq!(table.filter("e", 1), Err(DedupError::SenderLimit));
    assert_eq!(table.filter("abc", 1), Ok(false));
    assert_eq!(table.filter("d", 1), Ok(false));
    assert_eq!(table.watermark("e"), None);
}

#[test]
fn abandoned_new_and_existing_work_do_not_commit_or_consume_logical_capacity() {
    let mut table = small();
    for seq in [1, 9] {
        drop(table.begin("abc", seq).unwrap());
        assert_eq!(table.stats(), DedupStats::default());
        assert_eq!(table.watermark("abc"), None);
    }
    assert_eq!(complete(&mut table, "s", 3), 0);
    let before = table.stats();
    for seq in [1, 2, 4] {
        drop(table.begin("s", seq).unwrap());
        assert_eq!(table.stats(), before);
        assert_eq!(table.watermark("s"), Some(0));
    }
    assert_eq!(complete(&mut table, "s", 1), 1);
    assert_eq!(complete(&mut table, "s", 2), 3);
}

#[test]
fn business_panic_leaves_message_retryable() {
    let mut table = small();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _permit = table.begin("s", 2).unwrap();
        panic!("business failed before effect");
    }));
    assert!(panic.is_err());
    assert_eq!(table.stats(), DedupStats::default());
    assert_eq!(complete(&mut table, "s", 2), 0);
}

#[tokio::test]
async fn cancellation_after_reservation_does_not_suppress_retry() {
    let mut table = small();
    assert_eq!(complete(&mut table, "s", 1), 1);
    let mut work = Box::pin(async {
        let decision = table.begin("s", 3).unwrap();
        std::future::pending::<()>().await;
        if let DedupDecision::Process(permit) = decision {
            permit.commit();
        }
    });
    assert!(futures::poll!(work.as_mut()).is_pending());
    drop(work);
    assert_eq!(table.stats().pending_sequences, 0);
    assert_eq!(complete(&mut table, "s", 3), 1);
}

#[test]
fn forgotten_permit_cannot_turn_unprocessed_work_into_a_completed_duplicate() {
    let mut table = small();
    std::mem::forget(table.begin("s", 3).unwrap());
    assert_eq!(table.watermark("s"), None);
    assert_eq!(complete(&mut table, "s", 1), 1);
    std::mem::forget(table.begin("s", 3).unwrap());
    assert_eq!(complete(&mut table, "s", 3), 1);
}

#[test]
fn sequence_exhaustion_does_not_wrap_or_ack_a_gap() {
    let mut table = small();
    assert_eq!(complete(&mut table, "s", u64::MAX), 0);
    assert_eq!(table.filter("s", u64::MAX), Ok(false));
    assert_eq!(complete(&mut table, "s", 1), 1);
    assert_eq!(table.watermark("s"), Some(1));
    assert_eq!(table.filter("s", 0), Err(DedupError::ZeroSequence));
}

#[test]
fn independent_receiver_tables_and_explicit_sender_sessions_do_not_collide() {
    let mut first = DedupTable::new();
    let mut second = DedupTable::new();
    for table in [&mut first, &mut second] {
        assert_eq!(complete(table, "sender/session-a", 1), 1);
        assert_eq!(complete(table, "sender/session-b", 1), 1);
        assert_eq!(table.filter("sender/session-a", 1), Ok(false));
    }
}

#[test]
fn invalid_limits_are_rejected() {
    let good = DedupLimits::default();
    for bad in [
        DedupLimits {
            max_senders: 0,
            ..good
        },
        DedupLimits {
            max_pending_sequences: 0,
            ..good
        },
        DedupLimits {
            max_sender_bytes: 0,
            ..good
        },
        DedupLimits {
            max_sender_length: 0,
            ..good
        },
        DedupLimits {
            max_senders: usize::MAX,
            ..good
        },
        DedupLimits {
            max_sender_length: good.max_sender_bytes + 1,
            ..good
        },
    ] {
        assert_eq!(
            DedupTable::with_limits(bad).unwrap_err(),
            DedupError::InvalidLimits
        );
    }
}

#[test]
fn completion_history_matches_independent_set_model_under_reorder_and_abandon() {
    use std::collections::{BTreeMap, BTreeSet};
    let mut table = DedupTable::new();
    let mut model = BTreeMap::<String, BTreeSet<u64>>::new();
    let mut random = 0x8a32_974d_125f_u64;
    for n in 0..20_000 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let sender = format!("s{}", random % 4);
        let seq = 1 + (random >> 8) % 64;
        let already = model.get(&sender).is_some_and(|set| set.contains(&seq));
        match table.begin(&sender, seq).unwrap() {
            DedupDecision::Duplicate { watermark } => {
                assert!(already, "unprocessed message suppressed");
                assert!((1..=watermark).all(|i| model[&sender].contains(&i)));
            }
            DedupDecision::Process(permit) => {
                assert!(!already, "completed message readmitted");
                if n % 3 == 0 {
                    drop(permit);
                } else {
                    model.entry(sender.clone()).or_default().insert(seq);
                    let expected =
                        (1..=64).take_while(|i| model[&sender].contains(i)).count() as u64;
                    assert_eq!(permit.commit(), expected);
                }
            }
        }
        let expected_gaps: usize = model
            .values()
            .map(|set| {
                let prefix = (1..=64).take_while(|i| set.contains(i)).count();
                set.len() - prefix
            })
            .sum();
        assert_eq!(table.stats().pending_sequences, expected_gaps);
        assert_eq!(table.stats().senders, model.len());
    }
}
