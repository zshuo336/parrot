//! Bounded acceptance groups. Never publish a sequence or start its delivery
//! until every successful append in the group has crossed the same sync barrier.
use super::*;

pub(super) async fn accept(
    wal: &Wal,
    identity: &Identity,
    first: Command,
    rx: &mut mpsc::Receiver<Command>,
    stop: &CancellationToken,
    status: &watch::Sender<OutboxStatus>,
    config: &OutboxConfig,
) -> Result<(), OutboxError> {
    let span = tracing::trace_span!(target: TARGET, "outbox.acceptance",
        source = %identity.source, session = %identity.sender,
        receiver = %identity.receiver, path = %identity.path,
        last_enqueued = status.borrow().last_enqueued);
    let collect = Phase::new(span.clone(), "outbox.group_collect");
    let mut commands = vec![first];
    let deadline = tokio::time::Instant::now() + config.group_commit_interval;
    while commands.len() < config.group_commit_max_records {
        // Already queued work needs no timer round trip. Still check the fixed
        // deadline so a continuous producer cannot postpone syncing forever.
        if stop.is_cancelled()
            || (!config.group_commit_interval.is_zero() && tokio::time::Instant::now() >= deadline)
        {
            break;
        }
        match rx.try_recv() {
            Ok(command) => commands.push(command),
            Err(mpsc::error::TryRecvError::Disconnected) => break,
            Err(mpsc::error::TryRecvError::Empty) => {
                if config.group_commit_interval.is_zero() {
                    break;
                }
                tokio::select! { biased;
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep_until(deadline) => break,
                    command = rx.recv() => match command {
                        Some(command) => commands.push(command),
                        None => break,
                    },
                }
            }
        }
    }
    collect.finish(!stop.is_cancelled());
    if stop.is_cancelled() {
        for command in commands {
            let _ = command.reply.send(Err(OutboxError::Closed));
        }
        return Ok(());
    }
    let timing = Phase::new(span, "outbox.acceptance_sync");
    let result = persist(wal, identity, commands, status, Wal::sync);
    timing.finish(result.is_ok());
    result
}

// The sync operation is passed explicitly so unit tests can hold this exact
// publication boundary. Production always passes the real Wal::sync operation.
fn persist(
    wal: &Wal,
    identity: &Identity,
    commands: Vec<Command>,
    status: &watch::Sender<OutboxStatus>,
    sync: impl FnOnce(&Wal) -> Result<(), String>,
) -> Result<(), OutboxError> {
    let mut last = status.borrow().last_enqueued;
    let mut staged = Vec::new();
    let mut appended = 0;
    let mut commands = commands.into_iter();
    while let Some(command) = commands.next() {
        // Cancellation before append is unambiguous and consumes no sequence.
        // Once append begins, cancellation cannot roll back durable work.
        if command.reply.is_closed() {
            continue;
        }
        let result = append_for_group(wal, identity, &command, last.checked_add(1).unwrap_or(0));
        match result {
            Ok((seq, is_new)) => {
                if is_new {
                    last = seq;
                    appended += 1;
                }
                staged.push((seq, command));
            }
            Err(error) => {
                if wal.state().is_err() {
                    let error = OutboxError::Storage(error);
                    let _ = command.reply.send(Err(error.clone()));
                    for command in commands.chain(staged.into_iter().map(|(_, command)| command)) {
                        let _ = command.reply.send(Err(error.clone()));
                    }
                    return Err(error);
                }
                let _ = command.reply.send(Err(OutboxError::Rejected(error)));
            }
        }
    }
    if staged.is_empty() {
        return Ok(());
    }
    if let Err(error) = sync(wal) {
        let error = OutboxError::Storage(error);
        for (_, command) in staged {
            let _ = command.reply.send(Err(error.clone()));
        }
        return Err(error);
    }
    let receipt_count = wal.state().map_err(OutboxError::Storage)?.receipts.len();
    status.send_modify(|state| {
        state.last_enqueued = last;
        state.pending += appended;
        state.submission_receipts = receipt_count;
        state.acceptance_syncs = state.acceptance_syncs.saturating_add(1);
        state.last_group_records = appended;
    });
    for (seq, command) in staged {
        let _ = command.reply.send(Ok(seq));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "parrot-group-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn identity() -> Identity {
        Identity {
            version: 1,
            transactional: false,
            source: "source".into(),
            receiver: "target".into(),
            path: "/sink".into(),
            sender: "session".into(),
        }
    }
    fn command(
        bytes: &Arc<Semaphore>,
        value: u8,
    ) -> (Command, oneshot::Receiver<Result<u64, OutboxError>>) {
        let (reply, result) = oneshot::channel();
        (
            Command {
                parent: None,
                request_id: None,
                key: "k".into(),
                payload: Bytes::from(vec![value]),
                _bytes: bytes.clone().try_acquire_many_owned(2).unwrap(),
                reply,
            },
            result,
        )
    }

    #[test]
    fn success_and_status_cannot_escape_the_real_group_sync_barrier() {
        let scratch = Scratch::new();
        let wal = Wal::open(&scratch.0).unwrap();
        let bytes = Arc::new(Semaphore::new(8));
        let (status, view) = watch::channel(OutboxStatus::default());
        let (commands, mut replies): (Vec<_>, Vec<_>) = (1..=3).map(|n| command(&bytes, n)).unzip();
        let mut syncs = 0;
        persist(&wal, &identity(), commands, &status, |wal| {
            syncs += 1;
            assert_eq!(view.borrow().last_enqueued, 0);
            assert_eq!(view.borrow().pending, 0);
            assert_eq!(view.borrow().acceptance_syncs, 0);
            assert_eq!(
                bytes.available_permits(),
                2,
                "staged commands retain byte permits"
            );
            for reply in &mut replies {
                assert!(matches!(
                    reply.try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ));
            }
            assert_eq!(
                wal.replay("target")
                    .unwrap()
                    .iter()
                    .map(|r| (r.seq, r.payload[0]))
                    .collect::<Vec<_>>(),
                vec![(1, 1), (2, 2), (3, 3)]
            );
            wal.sync()
        })
        .unwrap();
        assert_eq!(syncs, 1);
        for (index, reply) in replies.iter_mut().enumerate() {
            assert_eq!(reply.try_recv().unwrap().unwrap(), index as u64 + 1);
        }
        assert_eq!(view.borrow().last_enqueued, 3);
        assert_eq!(view.borrow().pending, 3);
        assert_eq!(view.borrow().acceptance_syncs, 1);
        assert_eq!(view.borrow().last_group_records, 3);
        assert_eq!(bytes.available_permits(), 8);
        drop(wal);
        assert_eq!(
            Wal::open(&scratch.0)
                .unwrap()
                .replay("target")
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn sync_failure_publishes_no_acceptance_and_releases_all_command_permits() {
        let scratch = Scratch::new();
        let wal = Wal::open(&scratch.0).unwrap();
        let bytes = Arc::new(Semaphore::new(8));
        let (status, view) = watch::channel(OutboxStatus::default());
        let (commands, replies): (Vec<_>, Vec<_>) = (1..=3).map(|n| command(&bytes, n)).unzip();
        let result = persist(&wal, &identity(), commands, &status, |_| {
            Err("injected sync failure".into())
        });
        assert!(matches!(result, Err(OutboxError::Storage(_))));
        for mut reply in replies {
            assert!(matches!(
                reply.try_recv().unwrap(),
                Err(OutboxError::Storage(_))
            ));
        }
        assert_eq!(view.borrow().last_enqueued, 0);
        assert_eq!(view.borrow().pending, 0);
        assert_eq!(view.borrow().acceptance_syncs, 0);
        assert_eq!(bytes.available_permits(), 8);
        // A failed sync does not imply rollback: the complete prefix can survive.
        assert_eq!(wal.replay("target").unwrap().len(), 3);
    }

    #[test]
    fn cancelled_and_rejected_members_do_not_create_sequence_holes() {
        let scratch = Scratch::new();
        let wal = Wal::open_with_limits(
            &scratch.0,
            1024,
            WalLimits {
                max_record_bytes: 128,
                ..Default::default()
            },
        )
        .unwrap();
        let bytes = Arc::new(Semaphore::new(8));
        let (status, _) = watch::channel(OutboxStatus::default());
        let (first, mut a) = command(&bytes, 10);
        let (cancelled, b) = command(&bytes, 20);
        drop(b);
        let (mut rejected, mut c) = command(&bytes, 30);
        rejected.payload = Bytes::from(vec![0; 512]);
        let (last, mut d) = command(&bytes, 40);
        persist(
            &wal,
            &identity(),
            vec![first, cancelled, rejected, last],
            &status,
            Wal::sync,
        )
        .unwrap();
        assert_eq!(a.try_recv().unwrap().unwrap(), 1);
        assert!(matches!(
            c.try_recv().unwrap(),
            Err(OutboxError::Rejected(_))
        ));
        assert_eq!(d.try_recv().unwrap().unwrap(), 2);
        let records = wal.replay("target").unwrap();
        assert_eq!(
            records
                .iter()
                .map(|r| (r.seq, r.payload[0]))
                .collect::<Vec<_>>(),
            vec![(1, 10), (2, 40)]
        );
        assert_eq!(status.borrow().last_group_records, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn collection_count_and_deadline_are_bounded_and_shutdown_cancels_before_append() {
        let scratch = Scratch::new();
        let wal = Wal::open(&scratch.0).unwrap();
        let bytes = Arc::new(Semaphore::new(8));
        let (status, _) = watch::channel(OutboxStatus::default());
        let (tx, mut rx) = mpsc::channel(4);
        let (first, mut a) = command(&bytes, 1);
        let (second, mut b) = command(&bytes, 2);
        tx.try_send(second).unwrap();
        let (third, mut c) = command(&bytes, 3);
        tx.try_send(third).unwrap();
        let config = OutboxConfig {
            group_commit_max_records: 2,
            group_commit_interval: Duration::from_millis(500),
            ..Default::default()
        };
        let stop = CancellationToken::new();
        let start = tokio::time::Instant::now();
        accept(&wal, &identity(), first, &mut rx, &stop, &status, &config)
            .await
            .unwrap();
        assert_eq!(
            tokio::time::Instant::now(),
            start,
            "full group must not await the interval"
        );
        assert_eq!(a.try_recv().unwrap().unwrap(), 1);
        assert_eq!(b.try_recv().unwrap().unwrap(), 2);
        assert_eq!(rx.len(), 1);
        assert!(c.try_recv().is_err());
        let third = rx.recv().await.unwrap();
        accept(&wal, &identity(), third, &mut rx, &stop, &status, &config)
            .await
            .unwrap();
        assert_eq!(
            tokio::time::Instant::now() - start,
            config.group_commit_interval
        );
        assert_eq!(c.try_recv().unwrap().unwrap(), 3);
        let (fourth, mut d) = command(&bytes, 4);
        let id = identity();
        let future = accept(&wal, &id, fourth, &mut rx, &stop, &status, &config);
        tokio::pin!(future);
        assert!(futures::poll!(&mut future).is_pending());
        stop.cancel();
        future.await.unwrap();
        assert!(matches!(d.try_recv().unwrap(), Err(OutboxError::Closed)));
        assert_eq!(wal.pending_len().unwrap(), 3);
        assert_eq!(status.borrow().acceptance_syncs, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn arrivals_cannot_slide_the_first_deadline_and_zero_interval_never_waits() {
        let scratch = Scratch::new();
        let wal = Wal::open(&scratch.0).unwrap();
        let bytes = Arc::new(Semaphore::new(8));
        let (status, _) = watch::channel(OutboxStatus::default());
        let (tx, mut rx) = mpsc::channel(4);
        let (first, mut a) = command(&bytes, 1);
        let (second, mut b) = command(&bytes, 2);
        let (third, mut c) = command(&bytes, 3);
        let config = OutboxConfig {
            group_commit_interval: Duration::from_millis(100),
            ..Default::default()
        };
        let stop = CancellationToken::new();
        let id = identity();
        let start = tokio::time::Instant::now();
        let send = async {
            tokio::time::sleep(Duration::from_millis(25)).await;
            tx.send(second).await.unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
            tx.send(third).await.unwrap();
        };
        let (result, ()) = tokio::join!(
            accept(&wal, &id, first, &mut rx, &stop, &status, &config),
            send
        );
        result.unwrap();
        assert_eq!(
            tokio::time::Instant::now() - start,
            Duration::from_millis(100)
        );
        for (index, reply) in [&mut a, &mut b, &mut c].into_iter().enumerate() {
            assert_eq!(reply.try_recv().unwrap().unwrap(), index as u64 + 1);
        }
        let (fourth, mut d) = command(&bytes, 4);
        let config = OutboxConfig {
            group_commit_interval: Duration::ZERO,
            ..config
        };
        let start = tokio::time::Instant::now();
        accept(&wal, &id, fourth, &mut rx, &stop, &status, &config)
            .await
            .unwrap();
        assert_eq!(tokio::time::Instant::now(), start);
        assert_eq!(d.try_recv().unwrap().unwrap(), 4);
    }
    #[test]
    fn same_group_keyed_retries_wait_for_one_sync_and_fail_together_on_sync_error() {
        for fail in [false, true] {
            let scratch = Scratch::new();
            let wal = Wal::open(&scratch.0).unwrap();
            let bytes = Arc::new(Semaphore::new(8));
            let (mut first, mut a) = command(&bytes, 7);
            first.request_id = Some("id".into());
            let (mut duplicate, mut b) = command(&bytes, 7);
            duplicate.request_id = Some("id".into());
            let (next, mut c) = command(&bytes, 8);
            let (status, view) = watch::channel(OutboxStatus::default());
            let result = persist(
                &wal,
                &identity(),
                vec![first, duplicate, next],
                &status,
                |wal| {
                    for reply in [&mut a, &mut b, &mut c] {
                        assert!(matches!(
                            reply.try_recv(),
                            Err(oneshot::error::TryRecvError::Empty)
                        ));
                    }
                    assert_eq!(view.borrow().last_enqueued, 0);
                    assert_eq!(wal.pending_len().unwrap(), 2);
                    if fail {
                        Err("injected sync failure".into())
                    } else {
                        wal.sync()
                    }
                },
            );
            if fail {
                assert!(result.is_err());
                for mut reply in [a, b, c] {
                    assert!(matches!(
                        reply.try_recv().unwrap(),
                        Err(OutboxError::Storage(_))
                    ));
                }
                assert_eq!(view.borrow().last_enqueued, 0);
            } else {
                result.unwrap();
                assert_eq!(a.try_recv().unwrap().unwrap(), 1);
                assert_eq!(b.try_recv().unwrap().unwrap(), 1);
                assert_eq!(c.try_recv().unwrap().unwrap(), 2);
                assert_eq!(view.borrow().pending, 2);
                assert_eq!(view.borrow().last_group_records, 2);
                assert_eq!(view.borrow().submission_receipts, 1);
            }
            assert_eq!(bytes.available_permits(), 8);
            drop(wal);
            let wal = Wal::open(&scratch.0).unwrap();
            let (mut retry, mut reply) = command(&bytes, 7);
            retry.request_id = Some("id".into());
            let (status, _) = watch::channel(OutboxStatus {
                last_enqueued: 2,
                pending: 2,
                ..Default::default()
            });
            persist(&wal, &identity(), vec![retry], &status, Wal::sync).unwrap();
            assert_eq!(reply.try_recv().unwrap().unwrap(), 1);
            assert_eq!(wal.pending_len().unwrap(), 2);
            assert_eq!(status.borrow().last_group_records, 0);
            assert_eq!(status.borrow().acceptance_syncs, 1);
        }
    }
    #[test]
    fn receipt_only_history_cannot_create_a_new_directory_binding() {
        let scratch = Scratch::new();
        let mut identity = identity();
        identity.version = 2;
        let rec = WalRecord {
            endpoint: identity.receiver.clone(),
            sender: identity.sender.clone(),
            path: identity.path.clone(),
            seq: 1,
            type_key: "k".into(),
            payload: Bytes::from_static(&[7]),
        };
        let (_, receipt) = crate::durable::submission::wrap(&rec, "id").unwrap();
        let data = crate::durable::encode_record(&receipt.record().unwrap(), WalLimits::default())
            .unwrap();
        let path = scratch.0.join("seg-00000001.log");
        std::fs::write(&path, &data).unwrap();
        let wal = Wal::open(&scratch.0).unwrap();
        assert!(bind_and_recover(&wal, &identity).is_err());
        assert!(!scratch.0.join("outbox.identity").exists());
        assert_eq!(std::fs::read(path).unwrap(), data);
    }
    #[test]
    fn exhausted_sequence_space_still_allows_an_existing_keyed_receipt() {
        let scratch = Scratch::new();
        let wal = Wal::open(&scratch.0).unwrap();
        let identity = identity();
        let rec = WalRecord {
            endpoint: identity.receiver.clone(),
            sender: identity.sender.clone(),
            path: identity.path.clone(),
            seq: u64::MAX,
            type_key: "k".into(),
            payload: Bytes::from_static(&[7]),
        };
        wal.append_submission(rec, Some("last")).unwrap();
        wal.ack(&identity.receiver, &identity.sender, u64::MAX)
            .unwrap();
        wal.sync().unwrap();
        let bytes = Arc::new(Semaphore::new(8));
        let (mut retry, mut a) = command(&bytes, 7);
        retry.request_id = Some("last".into());
        let (new, mut b) = command(&bytes, 8);
        let (status, view) = watch::channel(OutboxStatus {
            last_enqueued: u64::MAX,
            last_acked: u64::MAX,
            ..Default::default()
        });
        persist(&wal, &identity, vec![retry, new], &status, Wal::sync).unwrap();
        assert_eq!(a.try_recv().unwrap().unwrap(), u64::MAX);
        assert!(matches!(
            b.try_recv().unwrap(),
            Err(OutboxError::Rejected(_))
        ));
        assert_eq!(view.borrow().pending, 0);
        assert_eq!(view.borrow().last_enqueued, u64::MAX);
    }
    #[test]
    fn cancelled_traced_member_cannot_claim_idempotency_or_parent_of_a_later_member() {
        let scratch = Scratch::new();
        let wal = Wal::open(&scratch.0).unwrap();
        let bytes = Arc::new(Semaphore::new(8));
        let (status, _) = watch::channel(OutboxStatus::default());
        let (mut cancelled, reply) = command(&bytes, 1);
        cancelled.request_id = Some("same".into());
        cancelled.parent = Some(parrot_api::trace::TraceParent::new([1; 16], [2; 8]).unwrap());
        drop(reply);
        let (mut accepted, mut reply) = command(&bytes, 1);
        accepted.request_id = Some("same".into());
        let expected = parrot_api::trace::TraceParent::new([3; 16], [4; 8]).unwrap();
        accepted.parent = Some(expected);
        let mut identity = identity();
        identity.version = 4;
        persist(
            &wal,
            &identity,
            vec![cancelled, accepted],
            &status,
            Wal::sync,
        )
        .unwrap();
        assert_eq!(reply.try_recv().unwrap().unwrap(), 1);
        assert_eq!(status.borrow().last_enqueued, 1);
        assert_eq!(bytes.available_permits(), 8);
        drop(wal);
        let wal = Wal::open(&scratch.0).unwrap();
        let state = wal.state().unwrap();
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.receipts.len(), 1);
        assert_eq!(state.pending[0].parent, Some(expected));
    }
}
