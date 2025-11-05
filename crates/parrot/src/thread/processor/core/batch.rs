//! Opt-in contiguous business batches. Reply channels stay with the processor;
//! engine controls and actor-declared barriers never enter a business batch.
use super::*;
use crate::thread::{envelope::AskEnvelope, mailbox::MailboxItem};
use futures::FutureExt;
use tokio::sync::oneshot;

struct Pending {
    trace_context: Option<parrot_api::trace::TraceContext>,
    message: BoxedMessage,
    reply: Option<oneshot::Sender<ActorResult<BoxedMessage>>>,
}
impl Pending {
    fn from_item(item: MailboxItem) -> Self {
        match item {
            MailboxItem::Ask(mut envelope) => {
                let reply = envelope.take_reply();
                Self {
                    trace_context: envelope.trace_context,
                    message: envelope.payload.into_boxed(),
                    reply,
                }
            }
            MailboxItem::Plain(message) => match message.downcast::<AskEnvelope>() {
                Ok(envelope) => Self::from_item(MailboxItem::Ask(*envelope)),
                Err(message) => {
                    let (message, trace_context) =
                        crate::thread::envelope::TraceEnvelope::unpack(message);
                    Self {
                        message,
                        trace_context,
                        reply: None,
                    }
                }
            },
        }
    }
}

impl<A> ActorProcessor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    pub(super) async fn process_coalesced(
        &self,
        mailbox: &Arc<dyn Mailbox>,
        max_messages: usize,
        yield_after_each_message: bool,
    ) -> Result<(usize, usize), SystemError> {
        if self.get_status() != ProcessorStatus::Running {
            return Err(SystemError::WorkerStateError(format!(
                "Actor at {} is not running",
                self.path
            )));
        }
        let limit = if yield_after_each_message {
            1
        } else {
            self.config.message_batch_size.unwrap_or(1).clamp(1, 256)
        };
        let mut processed = 0;
        let mut errors = 0;
        let mut lookahead: Option<Pending> = None;
        while processed < max_messages {
            self.escalation.ready().await;
            if self.get_status() != ProcessorStatus::Running || mailbox.is_closed().await {
                if let Some(reply) = lookahead.take().and_then(|item| item.reply) {
                    let _ = reply.send(Err(ActorError::Stopped));
                }
                break;
            }
            let first = match lookahead.take() {
                Some(item) => item,
                None => match mailbox.pop_item().await {
                    Some(item) => Pending::from_item(item),
                    None => break,
                },
            };
            let mut actor_slot = self.actor.lock().await;
            let Some(actor) = actor_slot.as_mut() else {
                if let Some(reply) = first.reply {
                    let _ = reply.send(Err(ActorError::Stopped));
                }
                break;
            };
            let parent = first.trace_context.clone();
            let trace_context = parent.as_ref().map(parrot_api::trace::TraceContext::child);
            let eligible = panic::catch_unwind(AssertUnwindSafe(|| {
                parrot_api::trace::in_scope(trace_context.clone(), || {
                    actor.can_batch_message(&first.message)
                })
            }));
            if matches!(eligible, Ok(false)) {
                drop(actor_slot);
                let outcome = self
                    .process_message_with_context(first.message, trace_context)
                    .await;
                let fatal = if self.message_failed() {
                    outcome
                        .as_ref()
                        .err()
                        .map(crate::error_receipt::actor_summary)
                } else {
                    None
                };
                if outcome.is_err() {
                    errors += 1;
                } else {
                    self.consecutive_reductions.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(reply) = first.reply {
                    let _ = reply.send(outcome);
                }
                processed += 1;
                if let Some(reason) = fatal {
                    return Err(SystemError::WorkerStateError(reason));
                }
                if yield_after_each_message {
                    tokio::task::yield_now().await;
                }
                continue;
            }
            let mut fatal = eligible.err().map(|e| panic_message(e, &self.path));
            let mut group = vec![first];
            let group_limit = limit.min(max_messages - processed);
            while fatal.is_none() && group.len() < group_limit {
                if self.get_status() != ProcessorStatus::Running || mailbox.is_closed().await {
                    break;
                }
                let Some(item) = mailbox.pop_item().await else {
                    break;
                };
                let next = Pending::from_item(item);
                // A business batch is one invocation. Never invent one parent
                // for messages from distinct causal operations or untraced work.
                if next.trace_context != parent {
                    lookahead = Some(next);
                    break;
                }
                match panic::catch_unwind(AssertUnwindSafe(|| {
                    parrot_api::trace::in_scope(trace_context.clone(), || {
                        actor.can_batch_message(&next.message)
                    })
                })) {
                    Ok(true) => group.push(next),
                    Ok(false) => {
                        lookahead = Some(next);
                        break;
                    }
                    Err(e) => {
                        group.push(next);
                        fatal = Some(panic_message(e, &self.path));
                    }
                }
            }
            let count = group.len();
            let (messages, replies): (Vec<_>, Vec<_>) = group
                .into_iter()
                .map(|item| (item.message, item.reply))
                .unzip();
            let mut invalid_count = false;
            let outcomes = if let Some(reason) = &fatal {
                (0..count)
                    .map(|_| Err(ActorError::Panic(reason.clone())))
                    .collect::<Vec<_>>()
            } else if self.get_status() != ProcessorStatus::Running {
                (0..count).map(|_| Err(ActorError::Stopped)).collect()
            } else {
                let mut context_slot = self.context.lock().await;
                let context = context_slot
                    .as_mut()
                    .ok_or_else(|| SystemError::ActorNotFound(self.path.clone()))?;
                match parrot_api::trace::scope(trace_context.clone(), async {
                    let recording = parrot_api::trace::SpanGuard::current(
                        parrot_api::trace::SpanKind::BusinessBatch,
                        &self.path,
                    );
                    let result = AssertUnwindSafe(async {
                        actor.process_business_batch(messages, context).await
                    })
                    .catch_unwind()
                    .await;
                    let status = match &result {
                        Err(_) => parrot_api::trace::SpanStatus::Panicked,
                        Ok(outcomes) => {
                            if outcomes
                                .iter()
                                .any(|outcome| matches!(outcome, Err(ActorError::Panic(_))))
                            {
                                parrot_api::trace::SpanStatus::Panicked
                            } else if outcomes.len() != count || outcomes.iter().any(Result::is_err)
                            {
                                parrot_api::trace::SpanStatus::Error
                            } else {
                                parrot_api::trace::SpanStatus::Ok
                            }
                        }
                    };
                    recording.finish(status);
                    result
                })
                .await
                {
                    Ok(outcomes) if outcomes.len() == count => outcomes,
                    Ok(outcomes) => {
                        invalid_count = true;
                        let reason = format!(
                            "batch handler at {} returned {} results for {count} messages",
                            self.path,
                            outcomes.len()
                        );
                        (0..count)
                            .map(|_| Err(ActorError::MessageHandlingError(reason.clone())))
                            .collect()
                    }
                    Err(e) => {
                        let reason = panic_message(e, &self.path);
                        (0..count)
                            .map(|_| Err(ActorError::Panic(reason.clone())))
                            .collect()
                    }
                }
            };
            drop(actor_slot);
            self.stats
                .messages_processed
                .fetch_add(count, Ordering::SeqCst);
            processed += count;
            let mut decided = false;
            fatal = None;
            for (mut outcome, reply) in outcomes.into_iter().zip(replies) {
                if outcome.is_err() {
                    errors += 1;
                    self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
                    if !decided {
                        let error = outcome.unwrap_err();
                        // The batch invocation has completed, but its first error
                        // still owns the supervision and replacement identity.
                        let error = parrot_api::trace::scope(trace_context.clone(), async {
                            if invalid_count {
                                let error = Arc::new(error);
                                self.record_failure(error.clone());
                                crate::supervision_policy::reply_error(error)
                            } else {
                                self.supervise_message_error(error).await
                            }
                        })
                        .await;
                        if self.message_failed() {
                            fatal = Some(crate::error_receipt::actor_summary(&error));
                        }
                        outcome = Err(error);
                        decided = true;
                    }
                } else {
                    self.consecutive_reductions.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(reply) = reply {
                    let _ = reply.send(outcome);
                }
            }
            if let Some(reason) = fatal {
                if let Some(reply) = lookahead.take().and_then(|item| item.reply) {
                    let _ = reply.send(Err(ActorError::Stopped));
                }
                return Err(SystemError::WorkerStateError(reason));
            }
            if yield_after_each_message {
                tokio::task::yield_now().await;
            }
        }
        debug_assert!(
            lookahead.is_none(),
            "lookahead stays within the scheduling budget"
        );
        if mailbox.is_empty().await {
            self.consecutive_reductions.store(0, Ordering::Relaxed);
        }
        Ok((processed, errors))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::{
        actor::{ActorState, EmptyConfig},
        address::ActorPath,
    };
    use std::sync::Mutex;

    #[derive(Debug)]
    struct Probe {
        events: Arc<Mutex<Vec<Vec<u64>>>>,
        mode: &'static str,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    impl Actor for Probe {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn can_batch_message(&self, message: &BoxedMessage) -> bool {
            if self.mode == "predicate" && message.downcast_ref::<u64>() == Some(&2) {
                panic!("predicate panic");
            }
            // Deliberately allow engine controls: the wrapper must still reject them.
            !message.is::<&'static str>()
        }
        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if message.is::<&'static str>() {
                    self.events.lock().unwrap().push(vec![99]);
                    return Ok(Box::new(99u64) as BoxedMessage);
                }
                panic!("eligible message must reach batch handler");
            })
        }
        fn receive_batch<'a>(
            &'a mut self,
            messages: Vec<BoxedMessage>,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
            if self.mode == "constructor" {
                panic!("batch future construction panic");
            }
            Box::pin(async move {
                if self.mode == "poll" {
                    tokio::task::yield_now().await;
                    panic!("batch future poll panic");
                }
                if self.mode == "gate" {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                let values: Vec<_> = messages
                    .iter()
                    .map(|m| {
                        *m.downcast_ref::<u64>()
                            .expect("control must not enter batch")
                    })
                    .collect();
                self.events.lock().unwrap().push(values);
                if self.mode == "short" {
                    return vec![];
                }
                messages
                    .into_iter()
                    .map(|message| {
                        if message.downcast_ref::<u64>() == Some(&13) {
                            Err(ActorError::MessageHandlingError("unlucky".into()))
                        } else {
                            Ok(message)
                        }
                    })
                    .collect()
            })
        }
    }
    struct Fixture {
        p: Arc<ActorProcessor<Probe>>,
        mailbox: Arc<dyn Mailbox>,
        events: Arc<Mutex<Vec<Vec<u64>>>>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    async fn fixture(mode: &'static str, limit: usize) -> Fixture {
        let events = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let actor = Probe {
            events: events.clone(),
            mode,
            entered: entered.clone(),
            release: release.clone(),
        };
        let processor = Arc::new(ActorProcessor::new(
            ThreadActor::new_for_test(actor),
            ThreadContext::new_for_test("test/batch"),
            "test/batch".into(),
            ThreadActorConfig {
                message_batch_size: Some(limit),
                ..Default::default()
            },
        ));
        processor
            .clone()
            .initialize_and_start_erased()
            .await
            .unwrap();
        let mailbox: Arc<dyn Mailbox> =
            Arc::new(MpscMailbox::new(32, ActorPath::placeholder("test/batch")));
        Fixture {
            p: processor,
            mailbox,
            events,
            entered,
            release,
        }
    }
    async fn request(
        mailbox: &Arc<dyn Mailbox>,
        message: BoxedMessage,
        legacy: bool,
    ) -> oneshot::Receiver<ActorResult<BoxedMessage>> {
        let (envelope, reply) = AskEnvelope::with_boxed(message);
        if legacy {
            mailbox
                .push(
                    Box::new(envelope),
                    crate::thread::BackpressureStrategy::Block,
                )
                .await
                .unwrap();
        } else {
            mailbox
                .push_ask(envelope, crate::thread::BackpressureStrategy::Block)
                .await
                .unwrap();
        }
        reply
    }
    async fn number(reply: oneshot::Receiver<ActorResult<BoxedMessage>>) -> u64 {
        *reply.await.unwrap().unwrap().downcast::<u64>().unwrap()
    }

    #[tokio::test]
    async fn batches_preserve_reply_mapping_barriers_controls_and_scheduler_budget() {
        let Fixture {
            p, mailbox, events, ..
        } = fixture("normal", 4).await;
        let a = request(&mailbox, Box::new(1u64), false).await;
        let b = request(&mailbox, Box::new(2u64), true).await;
        let barrier = request(&mailbox, Box::new("barrier"), false).await;
        let c = request(&mailbox, Box::new(3u64), false).await;
        let health = request(&mailbox, Box::new(ControlMessage::HealthCheck), false).await;
        let d = request(&mailbox, Box::new(4u64), false).await;
        assert_eq!(
            p.process_coalesced(&mailbox, 3, false).await.unwrap(),
            (3, 0)
        );
        assert_eq!(
            (number(a).await, number(b).await, number(barrier).await),
            (1, 2, 99)
        );
        assert_eq!(*events.lock().unwrap(), vec![vec![1, 2], vec![99]]);
        assert_eq!(
            p.process_coalesced(&mailbox, 3, false).await.unwrap(),
            (3, 0)
        );
        assert_eq!((number(c).await, number(d).await), (3, 4));
        assert_eq!(
            *health
                .await
                .unwrap()
                .unwrap()
                .downcast::<ActorState>()
                .unwrap(),
            ActorState::Running
        );
        assert_eq!(
            *events.lock().unwrap(),
            vec![vec![1, 2], vec![99], vec![3], vec![4]]
        );
        assert!(mailbox.is_empty().await);
        assert_eq!(p.stats.messages_processed.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn batch_completion_is_a_reply_barrier_and_one_error_does_not_erase_neighbours() {
        let Fixture {
            p,
            mailbox,
            events,
            entered,
            release,
        } = fixture("gate", 3).await;
        let mut a = request(&mailbox, Box::new(1u64), false).await;
        let bad = request(&mailbox, Box::new(13u64), false).await;
        let c = request(&mailbox, Box::new(3u64), true).await;
        let run = {
            let p = p.clone();
            let mailbox = mailbox.clone();
            tokio::spawn(async move { p.process_coalesced(&mailbox, 3, false).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        assert!(matches!(
            a.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(events.lock().unwrap().is_empty());
        release.notify_one();
        assert_eq!(run.await.unwrap().unwrap(), (3, 1));
        assert_eq!((number(a).await, number(c).await), (1, 3));
        assert!(matches!(
            bad.await.unwrap(),
            Err(ActorError::MessageHandlingError(_))
        ));
        assert_eq!(p.stats.errors_encountered.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn predicate_constructor_poll_panics_and_invalid_result_counts_fail_all_consumed_asks() {
        for mode in ["predicate", "constructor", "poll", "short"] {
            let Fixture { p, mailbox, .. } = fixture(mode, 3).await;
            let a = request(&mailbox, Box::new(1u64), false).await;
            let b = request(&mailbox, Box::new(2u64), true).await;
            assert!(
                p.process_coalesced(&mailbox, 3, false).await.is_err(),
                "{mode}"
            );
            for reply in [a, b] {
                let error = reply.await.unwrap().unwrap_err();
                if mode == "short" {
                    assert!(matches!(error, ActorError::MessageHandlingError(_)));
                } else {
                    assert!(matches!(error, ActorError::Panic(_)));
                }
            }
        }
    }

    #[tokio::test]
    async fn cancellation_drops_inflight_replies_and_does_not_run_a_prefetched_barrier() {
        let Fixture {
            p,
            mailbox,
            events,
            entered,
            ..
        } = fixture("gate", 4).await;
        let a = request(&mailbox, Box::new(1u64), false).await;
        let b = request(&mailbox, Box::new(2u64), true).await;
        let barrier = request(&mailbox, Box::new("barrier"), false).await;
        let task = {
            let p = p.clone();
            let mailbox = mailbox.clone();
            tokio::spawn(async move { p.process_coalesced(&mailbox, 4, false).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        for reply in [a, b, barrier] {
            assert!(reply.await.is_err());
        }
        assert!(events.lock().unwrap().is_empty());
        p.clone().stop_erased().await.unwrap();
    }

    #[tokio::test]
    async fn explicit_per_message_yield_keeps_handler_groups_to_one() {
        let Fixture {
            p, mailbox, events, ..
        } = fixture("normal", 4).await;
        let a = request(&mailbox, Box::new(1u64), false).await;
        let b = request(&mailbox, Box::new(2u64), false).await;
        assert_eq!(
            p.process_coalesced(&mailbox, 2, true).await.unwrap(),
            (2, 0)
        );
        assert_eq!((number(a).await, number(b).await), (1, 2));
        assert_eq!(*events.lock().unwrap(), vec![vec![1], vec![2]]);
    }

    #[tokio::test]
    async fn default_batch_handler_stops_after_an_explicit_fatal_error() {
        struct Serial(Vec<u64>);
        impl Actor for Serial {
            type Config = EmptyConfig;
            type Context = ();
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                _: &'a mut (),
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    let n = *message.downcast_ref::<u64>().unwrap();
                    self.0.push(n);
                    if n == 2 {
                        Err(ActorError::Panic("fatal".into()))
                    } else {
                        Ok(message)
                    }
                })
            }
        }
        let mut actor = Serial(vec![]);
        let results = actor
            .receive_batch(
                (1..=3u64).map(|n| Box::new(n) as BoxedMessage).collect(),
                &mut (),
            )
            .await;
        assert_eq!(actor.0, vec![1, 2]);
        assert!(results[0].is_ok());
        assert!(matches!(results[1], Err(ActorError::Panic(_))));
        assert!(matches!(results[2], Err(ActorError::Stopped)));
    }
    #[tokio::test]
    async fn failed_dispatch_cannot_execute_a_new_scheduler_run_before_retirement() {
        for limit in [1, 3] {
            let Fixture {
                p, mailbox, events, ..
            } = fixture("poll", limit).await;
            let failure = request(&mailbox, Box::new(1u64), false).await;
            assert!(
                p.process_batch_of_messages(&mailbox, 3, false)
                    .await
                    .is_err()
            );
            assert!(failure.await.unwrap().is_err());
            let pending = request(&mailbox, Box::new("barrier"), false).await;
            let next = p.process_batch_of_messages(&mailbox, 3, false).await;
            assert!(
                next.is_err(),
                "a new scheduler admission must not revive a failed processor: {next:?}"
            );
            assert!(
                events.lock().unwrap().is_empty(),
                "the later handler must never run"
            );
            drop(pending);
            p.clone().retire_erased().await.unwrap();
        }
    }
    #[tokio::test]
    async fn recording_batch_outcomes_distinguish_handler_errors_panics_and_bad_reply_count() {
        use parrot_api::trace::{SpanKind, SpanStatus, TraceRecorder};
        for (mode, status) in [
            ("normal", SpanStatus::Error),
            ("constructor", SpanStatus::Panicked),
            ("poll", SpanStatus::Panicked),
            ("short", SpanStatus::Error),
        ] {
            let Fixture { p, mailbox, .. } = fixture(mode, 3).await;
            let recorder = TraceRecorder::new(4).unwrap();
            let root = recorder.root();
            let a = root.scope(request(&mailbox, Box::new(1u64), false)).await;
            let b = root.scope(request(&mailbox, Box::new(13u64), true)).await;
            let outcome = p.process_coalesced(&mailbox, 3, false).await;
            if mode == "normal" {
                assert_eq!(outcome.unwrap(), (2, 1));
                assert_eq!(number(a).await, 1);
            } else {
                assert!(outcome.is_err());
                assert!(a.await.unwrap().is_err());
            }
            assert!(b.await.unwrap().is_err());
            p.clone().retire_erased().await.unwrap();
            recorder.close();
            let snapshot = recorder.drain();
            assert!(snapshot.terminal);
            assert_eq!(snapshot.records.len(), 1, "{mode}");
            let record = &snapshot.records[0];
            assert_eq!(record.kind, SpanKind::BusinessBatch);
            assert_eq!(record.status, status);
            assert_eq!(record.parent_span_id, Some(root.span_id()));
            assert_eq!(snapshot.counts.completed, 1);
            assert_eq!(snapshot.counts.overwritten, 0);
        }
    }

    #[tokio::test]
    async fn recording_cancelled_batch_retires_record_with_real_handler_future() {
        use parrot_api::trace::{SpanStatus, TraceRecorder};
        let Fixture {
            p,
            mailbox,
            entered,
            ..
        } = fixture("gate", 4).await;
        let recorder = TraceRecorder::new(4).unwrap();
        let root = recorder.root();
        let a = root.scope(request(&mailbox, Box::new(1u64), false)).await;
        let b = root.scope(request(&mailbox, Box::new(2u64), true)).await;
        let task = {
            let p = p.clone();
            let mailbox = mailbox.clone();
            tokio::spawn(async move { p.process_coalesced(&mailbox, 4, false).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        assert_eq!(recorder.snapshot().counts.in_flight, 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(a.await.is_err());
        assert!(b.await.is_err());
        p.clone().retire_erased().await.unwrap();
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].status, SpanStatus::Cancelled);
        assert_eq!(snapshot.counts.cancelled, 1);
        assert_eq!(snapshot.counts.panicked, 0);
    }
}
