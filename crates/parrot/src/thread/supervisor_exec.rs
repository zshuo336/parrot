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
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

pub(crate) type Respawn = Arc<
    dyn Fn(
            Arc<ThreadActorSystem>,
            Arc<SupervisionEntry>,
            Arc<dyn Mailbox>,
        ) -> BoxedFuture<'static, Result<(), SystemError>>
        + Send
        + Sync,
>;

pub(crate) struct SupervisionEntry {
    pub strategy: SupervisorStrategy,
    pub restarts: Mutex<Vec<Instant>>,
    pub mailbox: Arc<Mutex<WeakMailboxRef>>,
    pub respawn: Respawn,
}

pub use crate::thread::system::ErasedSpawnBoxTyped;

#[derive(Default)]
pub(crate) struct RestartControl {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}
impl RestartControl {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_one();
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    async fn cancelled(&self) {
        if !self.is_cancelled() {
            self.notify.notified().await;
        }
    }
}

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
    pub(crate) async fn on_child_panic(
        self: &Arc<Self>,
        path: &str,
        expected: WeakMailboxRef,
        panic_msg: String,
    ) {
        let Some(mailbox) = expected.upgrade() else {
            return;
        };
        let mailbox: Arc<dyn Mailbox> = mailbox;
        let (decision, supervision, control, completion) = {
            let mut registry = self.registry.write().unwrap();
            let Some(entry) = registry.get_mut(path).and_then(|entry| entry.dynamic_mut()) else {
                return;
            };
            if !Arc::ptr_eq(&entry.mailbox, &mailbox) || entry.stop_completion.is_some() {
                return;
            }
            let decision = match &entry.supervision {
                Some(supervision) => decide(
                    &supervision.strategy,
                    &mut supervision.restarts.lock().unwrap(),
                    Instant::now(),
                ),
                None => decide(
                    entry
                        .config
                        .supervisor_strategy
                        .as_ref()
                        .unwrap_or(&self.config().default_supervisor_strategy),
                    &mut Vec::new(),
                    Instant::now(),
                ),
            };
            let control = Arc::new(RestartControl::default());
            let (sender, receiver) = tokio::sync::watch::channel(None);
            entry.restart_cancel = Some(control.clone());
            entry.stop_completion = Some(receiver);
            mailbox.seal();
            (decision, entry.supervision.clone(), control, sender)
        };
        let deadline = tokio::time::Instant::now() + self.config().shutdown_timeout;
        let retired = self
            .retire_panicked_actor(path, mailbox.clone(), deadline)
            .await;
        let mut result = retired;
        if result.is_ok()
            && decision == Decision::Restart
            && let Some(entry) = supervision
        {
            // Cancellation from explicit stop/shutdown prevents a factory result
            // from publishing. spawn_at_internal rechecks under the registry lock.
            result = tokio::select! {
                biased;
                _ = control.cancelled() => Ok(()),
                _ = tokio::time::sleep_until(deadline) => Err(SystemError::Timeout(format!("restart of {path} exceeded configured deadline"))),
                result = (entry.respawn)(self.clone(), entry.clone(), mailbox.clone()) => result,
            };
            if result.is_ok() && !control.is_cancelled() {
                completion.send_replace(Some(StopOutcome::Stopped));
                return;
            }
        }
        // An explicit stop can race the final publication check. A cancellation
        // rejection is successful termination, not a failed stop operation.
        if control.is_cancelled() && matches!(result, Err(SystemError::ActorCreationError(_))) {
            result = Ok(());
        }
        if let Err(error) = &result {
            tracing::error!(%path, %error, "supervision recovery failed");
        }
        let notified = self
            .notify_termination_until(path, DeathReason::Panic(panic_msg), deadline)
            .await;
        if let Decision::Escalate(message) = decision
            && !control.is_cancelled()
        {
            self.escalate_to_parents(path, &message).await;
        }
        {
            let mut registry = self.registry.write().unwrap();
            if registry
                .get(path)
                .and_then(|entry| entry.dynamic())
                .is_some_and(|entry| Arc::ptr_eq(&entry.mailbox, &mailbox))
            {
                registry.remove(path);
            }
        }
        completion.send_replace(Some(StopOutcome::from_result(result.and(notified))));
    }

    /// Skip before_stop on the failed instance. Disposal stays owned and
    /// isolated; a timed-out retirement must never start a replacement.
    async fn retire_panicked_actor(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        deadline: tokio::time::Instant,
    ) -> Result<(), SystemError> {
        mailbox.seal();
        let processor = mailbox.get_processor();
        if let Some(processor) = &processor {
            processor.cancel_processing();
        }
        let drained = drain_mailbox(mailbox, Arc::new(tokio::sync::Semaphore::new(1)));
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
        match self
            .scheduler_group
            .dedicated_scheduler
            .deschedule_until(path, deadline)
            .await
        {
            Ok(()) | Err(SystemError::ActorNotFound(_)) => {}
            Err(error) => return Err(error),
        }
        tokio::time::timeout_at(deadline, async {
            drained
                .await
                .map_err(|error| SystemError::ShutdownError(error.to_string()))?
                .map_err(|error| SystemError::ShutdownError(error.to_string()))?;
            if let Some(processor) = processor {
                processor
                    .join_context_tasks()
                    .await
                    .map_err(|error| SystemError::ShutdownError(error.to_string()))?;
            }
            Ok(())
        })
        .await
        .map_err(|_| {
            SystemError::Timeout(format!(
                "failed actor {path} cleanup exceeded configured deadline"
            ))
        })?
    }

    /// Escalate a failure to the failed actor's parent chain.
    ///
    /// Each ancestor receives a `ChildFailure` control message; an ancestor
    /// that itself dies during handling escalates further (walk stops at the
    /// root or when an ancestor's strategy absorbs the failure).
    async fn escalate_to_parents(&self, path: &str, msg: &str) {
        // Determine the parent chain from the path convention
        // ("/user/parent/child" → "/user/parent"). Path-based parenting is
        // the thread engine's convention (contexts hold parent refs, but the
        // supervision walk only needs the chain order).
        let mut current = path.to_string();
        #[allow(clippy::never_loop)] // break 均为终止条件（哨兵路径保护）
        while let Some(pos) = current.rfind('/') {
            // Don't strip past "/user"
            if current == "/user" || current.ends_with("/user") {
                break;
            }
            current.truncate(pos);
            if current.is_empty() || current == "/" {
                break;
            }

            // Deliver a ChildFailure control message to the ancestor.
            let failure = crate::thread::envelope::ControlMessage::ChildFailure {
                path: path.to_string(),
                reason: msg.to_string(),
            };
            let mailbox = self.get_mailbox(&current);
            if let Some(mailbox) = mailbox {
                match tokio::time::timeout(
                    self.config().shutdown_timeout,
                    mailbox.push_with_priority(
                        Box::new(failure),
                        crate::thread::config::BackpressureStrategy::Block,
                        true,
                    ),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        tracing::error!(parent = %current, %error, "supervision escalation delivery failed")
                    }
                    Err(_) => {
                        tracing::error!(parent = %current, "supervision escalation delivery exceeded configured deadline")
                    }
                }
            }
            // Only the immediate parent gets the control message; further
            // escalation happens if THAT parent panics handling it.
            break;
        }
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
    fn decision_resume_maps_to_stop_for_panics() {
        let mut restarts = vec![];
        let d = decide(&SupervisorStrategy::Resume, &mut restarts, Instant::now());
        assert_eq!(d, Decision::Stop);
    }
}
