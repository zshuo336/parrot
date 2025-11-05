//! Generation-qualified supervision for thread actors.
//!
//! A failure reserves its registry entry until retirement and either atomic
//! replacement or final termination. Budgets belong to the logical actor;
//! callbacks and cancellation belong to a single mailbox generation.
use crate::thread::{
    config::SupervisorStrategy,
    error::SystemError,
    mailbox::{Mailbox, WeakMailboxRef},
    system::{StopOutcome, ThreadActorSystem, drain_mailbox},
};
use parrot_api::{supervisor::DeathReason, types::BoxedFuture};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

pub(crate) type Respawn = Arc<
    dyn Fn(
            Arc<ThreadActorSystem>,
            Arc<SupervisionEntry>,
            Arc<dyn Mailbox>,
            Arc<RestartControl>,
            Option<Arc<crate::children::recovery::Recovery>>,
        ) -> BoxedFuture<'static, Result<(), SystemError>>
        + Send
        + Sync,
>;

pub(crate) struct SupervisionEntry {
    pub stop_request: super::address::StopRequest,
    pub watches: crate::thread::death_watch::ThreadWatches,
    pub strategy: SupervisorStrategy,
    pub restarts: Mutex<Vec<Instant>>,
    pub mailbox: Arc<Mutex<WeakMailboxRef>>,
    pub respawn: Respawn,
    pub factory: Arc<dyn crate::thread::factory::FactoryDisposal>,
}

pub use crate::thread::system::ErasedSpawnBoxTyped;

pub(crate) use crate::constructor::ConstructionControl as RestartControl;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Restart,
    Stop,
    Escalate(String),
}

/// Reserve exactly one attempt. A failed factory still consumes that attempt.
pub(crate) fn decide(
    strategy: &SupervisorStrategy,
    restarts: &mut Vec<Instant>,
    now: Instant,
) -> Decision {
    match strategy {
        // Handler Resume is resolved while the instance is alive. This path
        // only handles generations already committed to retirement.
        SupervisorStrategy::Stop | SupervisorStrategy::Resume => Decision::Stop,
        SupervisorStrategy::Escalate => Decision::Escalate("strategy=Escalate".into()),
        SupervisorStrategy::Restart {
            max_retries,
            within,
        } => {
            restarts.retain(|time| now.saturating_duration_since(*time) < *within);
            if restarts.len() >= *max_retries {
                Decision::Escalate(format!(
                    "restart budget exhausted ({} in {:?})",
                    max_retries, within
                ))
            } else {
                restarts.push(now);
                Decision::Restart
            }
        }
    }
}

impl ThreadActorSystem {
    pub(crate) async fn on_child_failure(
        self: &Arc<Self>,
        path: &str,
        expected: WeakMailboxRef,
        error: Arc<parrot_api::errors::ActorError>,
        reason: DeathReason,
    ) {
        self.retire_generation(path, expected, error, reason, false)
            .await;
    }

    pub(crate) async fn retire_generation(
        self: &Arc<Self>,
        path: &str,
        expected: WeakMailboxRef,
        error: Arc<parrot_api::errors::ActorError>,
        reason: DeathReason,
        healthy: bool,
    ) {
        let Some(mailbox) = expected.upgrade() else {
            return;
        };
        let mailbox: Arc<dyn Mailbox> = mailbox;
        let mut deadline = tokio::time::Instant::now() + self.config().shutdown_timeout;
        let (
            fallback_strategy,
            supervision,
            control,
            completion,
            retired_sender,
            mode,
            watches,
            children,
            lifetime,
        ) = {
            let mut registry = self.registry.write().unwrap();
            let Some(entry) = registry.get_mut(path).and_then(|entry| entry.dynamic_mut()) else {
                return;
            };
            if !Arc::ptr_eq(&entry.mailbox, &mailbox) || entry.stop_completion.is_some() {
                return;
            }
            if let Some(decision) = entry.lifetime.decision_deadline() {
                deadline = deadline.min(decision);
            }
            if let Some(group) = entry.lifetime.recovery() {
                deadline = group.deadline;
            }
            let fallback_strategy = entry
                .config
                .supervisor_strategy
                .clone()
                .unwrap_or_else(|| self.config().default_supervisor_strategy.clone());
            let control = Arc::new(RestartControl::with_deadline(deadline));
            let (sender, receiver) = tokio::sync::watch::channel(None);
            entry.watches.suspend();
            entry.restart_cancel = Some(control.clone());
            entry.stop_completion = Some(receiver);
            let (retired_sender, retirement) = tokio::sync::watch::channel(None);
            entry.retirement_completion = Some(retirement);
            mailbox.seal();
            entry.children.close();
            (
                fallback_strategy,
                entry.supervision.clone(),
                control,
                sender,
                retired_sender,
                entry
                    .config
                    .scheduling_mode
                    .clone()
                    .unwrap_or_else(|| self.config().default_scheduling_mode.clone()),
                entry.watches.clone(),
                entry.children.clone(),
                entry.lifetime.clone(),
            )
        };
        if !healthy && let Some(processor) = mailbox.get_processor() {
            processor.cancel_processing();
        }
        let policy_future =
            lifetime.failure_policy(error.clone(), self.typed_slots.clone(), deadline);
        tokio::pin!(policy_future);
        let (policy, decision_timed_out) = tokio::select! {
            biased;
            policy = &mut policy_future => (policy, false),
            _ = tokio::time::sleep_until(deadline) => {
                completion.send_replace(Some(StopOutcome::TimedOut(format!("failed actor {path} supervision decision exceeded deadline"))));
                (policy_future.await, true)
            }
        };
        let (mut policy, policy_error) = match policy {
            Ok(policy) => (policy, None),
            Err(error) => (None, Some(SystemError::Other(anyhow::Error::new(error)))),
        };
        if let Some(group) = policy.as_ref().and_then(|policy| policy.recovery.as_ref()) {
            deadline = deadline.min(group.deadline);
        }
        if control.is_cancelled() || decision_timed_out {
            policy = None;
        }
        let decision = match policy.as_ref().map(|policy| policy.decision) {
            _ if control.is_cancelled() || decision_timed_out => Decision::Stop,
            Some(
                parrot_api::supervisor::SupervisionDecision::Stop
                | parrot_api::supervisor::SupervisionDecision::Resume,
            ) => Decision::Stop,
            Some(parrot_api::supervisor::SupervisionDecision::Restart) => Decision::Restart,
            Some(parrot_api::supervisor::SupervisionDecision::Escalate) => {
                Decision::Escalate("child policy requested escalation".into())
            }
            None if policy_error.is_some() => Decision::Stop,
            None => match &supervision {
                Some(entry) => decide(
                    &entry.strategy,
                    &mut entry.restarts.lock().unwrap(),
                    Instant::now(),
                ),
                None => decide(&fallback_strategy, &mut Vec::new(), Instant::now()),
            },
        };
        let retirement = async {
            let siblings = async {
                match &policy {
                    Some(policy) => policy.stop_siblings().await,
                    None => Ok(()),
                }
            };
            let (siblings_result, children_result, result) =
                tokio::join!(siblings, children.join(), async {
                    if healthy {
                        super::system::stop_actor_resources(
                            path,
                            mailbox.clone(),
                            mode,
                            self.scheduler_group.clone(),
                            self.cleanup_slots.clone(),
                            deadline,
                        )
                        .await
                    } else {
                        self.retire_panicked_actor(path, mailbox.clone(), mode, deadline)
                            .await
                    }
                });
            let decisions = lifetime
                .settle_live_decision()
                .await
                .map_err(|e| SystemError::Other(anyhow::Error::new(e)));
            result
                .and(children_result.map_err(|e| SystemError::Other(anyhow::Error::new(e))))
                .and(siblings_result.map_err(|e| SystemError::Other(anyhow::Error::new(e))))
                .and(decisions)
        };
        tokio::pin!(retirement);
        let (mut result, timed_out) = tokio::select! {
            biased;
            result = &mut retirement => (result, false),
            _ = tokio::time::sleep_until(deadline) => {
                completion.send_replace(Some(StopOutcome::TimedOut(format!("failed actor {path} cleanup exceeded configured deadline"))));
                (retirement.await, true)
            }
        };
        let mut timed_out = timed_out || decision_timed_out;
        if let Some(error) = policy_error {
            result = Err(error);
        }
        if timed_out || tokio::time::Instant::now() >= deadline {
            result = Err(SystemError::Timeout(format!(
                "failed actor {path} cleanup exceeded configured deadline"
            )));
        }
        if result.is_ok()
            && decision == Decision::Restart
            && let Some(entry) = &supervision
        {
            let restart = (entry.respawn)(
                self.clone(),
                entry.clone(),
                mailbox.clone(),
                control.clone(),
                policy.as_ref().and_then(|policy| policy.recovery.clone()),
            );
            tokio::pin!(restart);
            result = tokio::select! {
                biased;
                result = &mut restart => result,
                _ = tokio::time::sleep_until(deadline) => {
                    timed_out = true;
                    control.cancel();
                    completion.send_replace(Some(StopOutcome::TimedOut(format!("restart of {path} exceeded configured deadline"))));
                    restart.await
                }
            };
            if result.is_ok() {
                if !timed_out {
                    completion.send_replace(Some(StopOutcome::Stopped));
                }
                retired_sender.send_replace(Some(StopOutcome::Stopped));
                return;
            }
        }
        if !timed_out && tokio::time::Instant::now() >= deadline {
            timed_out = true;
            completion.send_replace(Some(StopOutcome::TimedOut(format!(
                "recovery of {path} exceeded configured deadline"
            ))));
        }
        // Only cancellation is a successful stop; a panicking cancelled output
        // or failed capture disposal must remain a reported cleanup failure.
        if control.is_cancelled() && matches!(result, Err(SystemError::ShuttingDown)) {
            result = Ok(());
        }
        if let Some(group) = lifetime.recovery() {
            group.abort("Thread member could not recover");
        }
        let disposal = async {
            let policy = lifetime
                .dispose_policy()
                .await
                .map_err(SystemError::policy_disposal);
            let factory = match &supervision {
                Some(entry) => entry.factory.dispose().await,
                None => Ok(()),
            };
            factory.and(policy)
        };
        let (disposed, disposal_timed_out) =
            super::stop_deadline::join_until(disposal, deadline, || {
                control.cancel();
                completion.send_replace(Some(StopOutcome::TimedOut(format!(
                    "factory disposal for {path} exceeded configured deadline"
                ))));
            })
            .await;
        timed_out |= disposal_timed_out;
        if let Err(error) = disposed {
            result = Err(error);
        }
        if let Err(error) = &result {
            tracing::error!(%path, error = %crate::error_receipt::system_summary(error), "supervision recovery failed");
        }
        // Observers are outside the supervision hierarchy. Their delivery
        // deadline must never consume or suppress a ready escalation decision.
        if let Decision::Escalate(message) = decision
            && !control.is_cancelled()
            && tokio::time::Instant::now() < deadline
            && !lifetime.escalate(error.clone())
        {
            tracing::debug!(%path, %message, "failed actor has no active managed parent for escalation");
        }
        watches.close();
        let reason = match &result {
            Ok(()) => reason,
            Err(SystemError::Panic(message)) => DeathReason::Panic(message.clone()),
            // Expiry while waiting for disposal is not a forced kill. The
            // originating failure still explains why the actor terminated.
            Err(SystemError::Timeout(_)) => reason,
            Err(error) => DeathReason::Failed(crate::error_receipt::system_summary(error)),
        };
        let event = parrot_api::context::ActorTerminated {
            generation: watches.common.id(),
            path: path.into(),
            reason: reason.clone(),
        };
        let (_, notified) = tokio::join!(
            watches
                .common
                .terminated(event, self.config().shutdown_timeout),
            crate::thread::death_watch::notify_legacy(
                &watches.legacy,
                path,
                reason,
                self.config().shutdown_timeout
            )
        );

        {
            let mut registry = self.registry.write().unwrap();
            if let Some(entry) = registry.get(path).and_then(|entry| entry.dynamic())
                && Arc::ptr_eq(&entry.mailbox, &mailbox)
            {
                // Publish the terminal logical owner's receipt before removing
                // its registration. Successful restarts returned above and must
                // not pin an obsolete generation into this shared OnceCell.
                let _ = entry.stop_request.set(completion.subscribe());
                registry.remove(path);
            }
        }
        let result = result.map_err(crate::error_receipt::SystemReceipt::new);
        let notification = lifetime.finish(
            result
                .as_ref()
                .map(|_| ())
                .map_err(|error| error.actor_receipt()),
        );
        let outcome = StopOutcome::from_result(
            result
                .map_err(crate::error_receipt::SystemReceipt::into_error)
                .and(notified),
        );
        if !timed_out {
            completion.send_replace(Some(outcome.clone()));
        }
        retired_sender.send_replace(Some(outcome));
        notification.await;
    }

    /// Skip before_stop on the failed instance. Disposal stays owned and
    /// isolated; a timed-out retirement must never start a replacement.
    pub(crate) async fn retire_panicked_actor(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        mode: crate::thread::config::SchedulingMode,
        deadline: tokio::time::Instant,
    ) -> Result<(), SystemError> {
        mailbox.seal();
        let processor = mailbox.get_processor();
        if let Some(processor) = &processor {
            processor.cancel_processing();
        }
        let slots = self.cleanup_slots.clone();
        let drained = drain_mailbox(mailbox, slots.clone())
            .await
            .map_err(|error| SystemError::ShutdownError(error.to_string()))
            .and_then(|result| {
                result.map_err(|error| SystemError::ShutdownError(error.to_string()))
            });
        let retired = if let Some(processor) = processor {
            self.scheduler_group
                .retire_actor(path, &mode, processor, slots, deadline)
                .await
        } else {
            Ok(())
        };
        let _ = self.scheduler_group.shared_scheduler.deschedule(path).await;
        if let Some(sharded) = self
            .scheduler_group
            .sharded_scheduler
            .lock()
            .unwrap()
            .as_ref()
        {
            let _ = sharded.deschedule(path);
        }
        retired.and(drained)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restart_strategy(max: usize, window: Duration) -> SupervisorStrategy {
        SupervisorStrategy::Restart {
            max_retries: max,
            within: window,
        }
    }

    #[test]
    fn decision_restart_under_budget() {
        let mut restarts = vec![];
        let d = decide(
            &restart_strategy(3, Duration::from_secs(10)),
            &mut restarts,
            Instant::now(),
        );
        assert_eq!(d, Decision::Restart);
        assert_eq!(restarts.len(), 1);
    }

    #[test]
    fn decision_escalate_when_window_exhausted() {
        let mut restarts = vec![Instant::now(); 3];
        let d = decide(
            &restart_strategy(3, Duration::from_secs(10)),
            &mut restarts,
            Instant::now(),
        );
        assert!(matches!(d, Decision::Escalate(_)));
    }

    #[test]
    fn decision_window_slides() {
        // Old restarts fall out of the window → budget available again.
        let old = Instant::now() - Duration::from_secs(11);
        let mut restarts = vec![old; 5];
        let d = decide(
            &restart_strategy(3, Duration::from_secs(10)),
            &mut restarts,
            Instant::now(),
        );
        assert_eq!(d, Decision::Restart);
        assert_eq!(restarts.len(), 1, "stale restarts pruned");
    }

    #[test]
    fn decision_stop_strategy_stops() {
        let mut restarts = vec![];
        let d = decide(&SupervisorStrategy::Stop, &mut restarts, Instant::now());
        assert_eq!(d, Decision::Stop);
    }

    #[test]
    fn decision_escalate_strategy_escalates() {
        let mut restarts = vec![];
        let d = decide(&SupervisorStrategy::Escalate, &mut restarts, Instant::now());
        assert!(matches!(d, Decision::Escalate(_)));
    }

    #[test]
    fn retired_generation_resume_does_not_reconstruct_an_instance() {
        let mut restarts = vec![];
        let d = decide(&SupervisorStrategy::Resume, &mut restarts, Instant::now());
        assert_eq!(d, Decision::Stop);
    }
}

#[cfg(test)]
mod disposal_deadline_tests {
    use super::*;
    use crate::thread::{config::ThreadActorSystemConfig, context::ThreadContext};
    use futures::FutureExt;
    use parrot_api::{
        actor::{Actor, ActorState, EmptyConfig},
        address::ActorRef,
        context::ActorTerminated,
        death_watch::{DeathWatch, TerminationRecipient},
        errors::ActorError,
        system::{ActorSystem, SystemState},
        types::{ActorResult, BoxedMessage},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    struct Probe(Arc<AtomicUsize>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Actor for Probe {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(message) })
        }
    }

    struct Capture {
        drops: Arc<AtomicUsize>,
        panic: bool,
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "supervisor capture disposal probe");
        }
    }

    type DisposalGate = (oneshot::Sender<()>, oneshot::Receiver<()>);
    struct HeldReceipt {
        actual: Arc<dyn crate::thread::factory::FactoryDisposal>,
        gate: Mutex<Option<DisposalGate>>,
    }
    impl crate::thread::factory::FactoryDisposal for HeldReceipt {
        fn dispose(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
            let gate = self.gate.lock().unwrap().take();
            Box::pin(async move {
                // Run the real factory retirement, including its real capture
                // panic handling. Only delivery of its actual result is gated.
                let result = self.actual.dispose().await;
                if let Some((ready, release)) = gate {
                    let _ = ready.send(());
                    let _ = release.await;
                }
                result
            })
        }
    }
    #[derive(Default)]
    struct Events(Mutex<Vec<ActorTerminated>>);
    impl TerminationRecipient for Events {
        fn deliver(&self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async move {
                self.0.lock().unwrap().push(event);
                Ok(())
            })
        }
    }
    #[derive(Clone, Copy)]
    enum Timing {
        Early,
        BothReady,
        TimerFirst,
    }

    async fn check_disposal(timing: Timing, capture_panics: bool) {
        const PATH: &str = "/supervisor-disposal-probe";
        const BUDGET: Duration = Duration::from_secs(30);
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shutdown_timeout: BUDGET,
            ..Default::default()
        });
        let actor_drops = Arc::new(AtomicUsize::new(0));
        let capture_drops = Arc::new(AtomicUsize::new(0));
        let constructions = Arc::new(AtomicUsize::new(0));
        let capture = Capture {
            drops: capture_drops.clone(),
            panic: capture_panics,
        };
        let made = constructions.clone();
        let actors = actor_drops.clone();
        let actor = system
            .spawn_supervised(
                move || {
                    let _ = &capture;
                    made.fetch_add(1, Ordering::SeqCst);
                    Probe(actors.clone())
                },
                PATH,
                SupervisorStrategy::Stop,
            )
            .await
            .unwrap();
        assert_eq!(
            *actor
                .send(Box::new(42_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );

        let watcher = DeathWatch::new(1);
        let events = Arc::new(Events::default());
        watcher.set_recipient(events.clone()).unwrap();
        actor.death_watch().unwrap().watch(&watcher).unwrap();
        let (ready, reached) = oneshot::channel();
        let (release, held) = oneshot::channel();
        let mut release = Some(release);
        let (mailbox, replaced) = {
            let mut registry = system.registry.write().unwrap();
            let entry = registry.get_mut(PATH).unwrap().dynamic_mut().unwrap();
            let old = entry.supervision.as_ref().unwrap().clone();
            let mailbox = old.mailbox.lock().unwrap().clone();
            let replacement = Arc::new(SupervisionEntry {
                stop_request: old.stop_request.clone(),
                watches: old.watches.clone(),
                strategy: old.strategy.clone(),
                restarts: Mutex::new(old.restarts.lock().unwrap().clone()),
                mailbox: old.mailbox.clone(),
                respawn: old.respawn.clone(),
                factory: Arc::new(HeldReceipt {
                    actual: old.factory.clone(),
                    gate: Mutex::new(Some((ready, held))),
                }),
            });
            (mailbox, entry.supervision.replace(replacement))
        };
        drop(replaced);
        // This is the actual terminal owner with a live managed generation,
        // not a copy of its select. Drive it explicitly to control repoll order.
        let owner = system.retire_generation(
            PATH,
            mailbox,
            Arc::new(ActorError::InternalError(
                "terminal generation probe".into(),
            )),
            DeathReason::Failed("terminal generation probe".into()),
            true,
        );
        tokio::pin!(owner);
        let mut completed = false;
        let mut paused = false;
        let checked = std::panic::AssertUnwindSafe(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    _ = owner.as_mut() => { completed = true; panic!("owner completed before disposal receipt release"); },
                    reached = reached => reached.expect("real factory disposal must reach gate"),
                }
            }).await.expect("real retirement must reach the controlled receipt");
            assert_eq!(actor_drops.load(Ordering::SeqCst), 1);
            assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
            let (public, actual) = {
                let registry = system.registry.read().unwrap();
                let entry = registry.get(PATH).unwrap().dynamic().unwrap();
                (entry.stop_completion.clone().unwrap(), entry.retirement_completion.clone().unwrap())
            };
            assert!(public.borrow().is_none());
            assert!(actual.borrow().is_none());
            assert!(events.0.lock().unwrap().is_empty());
            tokio::time::pause();
            paused = true;
            if !matches!(timing, Timing::Early) {
                tokio::time::advance(BUDGET + Duration::from_secs(1)).await;
            }
            if matches!(timing, Timing::TimerFirst) {
                assert!(futures::poll!(owner.as_mut()).is_pending());
                assert!(matches!(*public.borrow(), Some(StopOutcome::TimedOut(_))));
                assert!(actual.borrow().is_none(), "timeout cannot publish retirement early");
                assert!(system.registry.read().unwrap().contains_key(PATH));
                assert!(events.0.lock().unwrap().is_empty(), "timeout is not a termination event");
            }
            release.take().unwrap().send(()).unwrap();
            owner.as_mut().await;
            completed = true;
            tokio::time::resume();
            paused = false;
            assert!(!system.registry.read().unwrap().contains_key(PATH));
            let first = actor.stop().await;
            let repeated = actor.stop().await;
            let actual = crate::thread::system::await_stop(actual).await;
            if matches!(timing, Timing::Early) {
                if capture_panics {
                    assert!(first.is_err(), "early real capture failure must remain public: {first:?}");
                    assert!(first.as_ref().unwrap_err().to_string().contains("supervisor capture disposal probe"));
                } else {
                    assert!(first.is_ok(), "early cleanup: {first:?}");
                }
            } else {
                assert!(
                    matches!(&first, Err(ActorError::TimeoutDetail(_))),
                    "late supervisor disposal must retain public timeout: {first:?}"
                );
            }
            assert_eq!(first.map_err(|e| e.to_string()), repeated.map_err(|e| e.to_string()));
            if capture_panics {
                assert!(actual.is_err(), "actual capture failure must survive timeout: {actual:?}");
                assert!(actual.unwrap_err().to_string().contains("supervisor capture disposal probe"), "actual capture failure must survive timeout");
            } else {
                assert!(actual.is_ok(), "actual successful cleanup must remain independent: {actual:?}");
            }
            let events = events.0.lock().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].path, PATH);
            assert!(matches!(&events[0].reason, DeathReason::Failed(message) if message.contains(if capture_panics { "supervisor capture disposal probe" } else { "terminal generation probe" })));
            assert_eq!(watcher.subscriptions(), (0, 0));
        }).catch_unwind().await;
        if paused {
            tokio::time::resume();
        }
        if let Some(release) = release {
            let _ = release.send(());
        }
        if !completed {
            tokio::time::timeout(Duration::from_secs(5), owner.as_mut())
                .await
                .expect("fixture owner must retire after assertion failure");
        }
        let shutdown = system.shutdown_internal().await;
        assert!(
            shutdown.is_ok(),
            "fixture workers must retire: {shutdown:?}"
        );
        assert_eq!(system.status().state, SystemState::Stopped);
        assert_eq!(actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            constructions.load(Ordering::SeqCst),
            1,
            "terminal stop must not restart"
        );
        if let Err(payload) = checked {
            std::panic::resume_unwind(payload);
        }
    }

    #[tokio::test]
    async fn early_actual_factory_success_and_panic_keep_original_results() {
        for panic in [false, true] {
            check_disposal(Timing::Early, panic).await;
        }
    }
    #[tokio::test]
    async fn late_actual_factory_success_keeps_public_timeout() {
        check_disposal(Timing::BothReady, false).await;
    }
    #[tokio::test]
    async fn late_actual_factory_panic_keeps_public_timeout() {
        check_disposal(Timing::BothReady, true).await;
    }
    #[tokio::test]
    async fn pending_disposal_keeps_registration_and_real_retirement_owned() {
        for panic in [false, true] {
            check_disposal(Timing::TimerFirst, panic).await;
        }
    }
}
