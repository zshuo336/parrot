//! Cancellation and completion shared by managed Actix actor references.
use parrot_api::{
    errors::ActorError,
    typed_channel::{TypedActorLifecycle, TypedChannelState},
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, watch},
    time::Instant,
};

const MAX_DEATH_WATCHERS: usize = 1024;

/// Execution failure and actual resource disposal are separate receipts. A child
/// failure alone must not turn its parent's successful cleanup into a failure.
pub(crate) struct ActorExit {
    pub outcome: ActorResult<()>,
    pub cleanup: ActorResult<()>,
}

#[derive(Clone)]
pub(crate) enum Outcome {
    Stopped,
    Timeout(String),
    Panic(String),
    Failed(crate::error_receipt::ActorReceipt),
}
impl Outcome {
    pub(crate) fn from_result(result: ActorResult<()>) -> Self {
        match result {
            Ok(()) => Self::Stopped,
            Err(ActorError::Timeout) => Self::Timeout("actor stop deadline elapsed".into()),
            Err(ActorError::TimeoutDetail(s)) => Self::Timeout(s),
            Err(ActorError::Panic(s)) => Self::Panic(s),
            Err(e) => Self::Failed(crate::error_receipt::ActorReceipt::new(Arc::new(e))),
        }
    }
    pub(crate) fn result(self) -> ActorResult<()> {
        match self {
            Self::Stopped => Ok(()),
            Self::Timeout(s) => Err(ActorError::TimeoutDetail(s)),
            Self::Panic(s) => Err(ActorError::Panic(s)),
            Self::Failed(error) => Err(error.into_error()),
        }
    }
}

pub(crate) fn death_reason_from_result(
    result: &ActorResult<()>,
    deadline_enforced: bool,
) -> parrot_api::supervisor::DeathReason {
    match result {
        Ok(()) => parrot_api::supervisor::DeathReason::Normal,
        Err(ActorError::Timeout) | Err(ActorError::TimeoutDetail(_)) if deadline_enforced => {
            parrot_api::supervisor::DeathReason::Killed
        }
        Err(ActorError::Panic(message)) => {
            parrot_api::supervisor::DeathReason::Panic(message.clone())
        }
        Err(error) => {
            parrot_api::supervisor::DeathReason::Failed(crate::error_receipt::actor_summary(error))
        }
    }
}
pub(crate) type Completion = watch::Receiver<Option<Outcome>>;
pub(crate) async fn await_completion(mut completion: Completion) -> ActorResult<()> {
    loop {
        if let Some(result) = completion.borrow_and_update().clone() {
            return result.result();
        }
        completion.changed().await.map_err(|_| {
            ActorError::InternalError(
                "Actix lifecycle owner ended without reporting completion".into(),
            )
        })?;
    }
}

pub(crate) struct ActorControl {
    // A failed stop hook is actual cleanup failure, independently of a prior
    // message failure. The monitor consumes this after ContextFut disposal.
    pub(crate) stop_failure: Mutex<Option<crate::error_receipt::ActorReceipt>>,
    pub(crate) dynamic_escalation: crate::escalation_pause::EscalationPause,
    pub(crate) typed_tree: OnceLock<Arc<crate::typed_context::TypedTree>>,
    pub(crate) typed_escalation_changed: Notify,
    pub channel: TypedChannelState,
    pub(crate) initialization_gate: Option<Completion>,
    pub(crate) group_activation: Option<Arc<crate::children::recovery::Recovery>>,
    pub(crate) upstream_stop: Option<Weak<ActorControl>>,
    pub(crate) stop_hook: OnceLock<Arc<dyn Fn(Instant) + Send + Sync>>,
    pub tasks: Arc<crate::context_tasks::ContextTasks>,
    pub children: Arc<crate::children::ChildScope>,
    pub lifetime: Arc<crate::children::ChildLifetime>,
    deadline: Mutex<Option<Instant>>,
    deadline_enforced: AtomicBool,
    pub(crate) supervision_failed: AtomicBool,
    // The outer Option distinguishes no terminal failure from an untraced one.
    failure_context: Mutex<Option<Option<parrot_api::trace::TraceContext>>>,
    pub(crate) explicit_stop: AtomicBool,
    pub(crate) escalated: Mutex<Option<Arc<ActorError>>>,
    changed: Notify,
    pub completion: Completion,
    pub timeout: Duration,
    pub death_watch: parrot_api::death_watch::DeathWatch,
}
impl ActorControl {
    pub fn new(timeout: Duration) -> (Arc<Self>, watch::Sender<Option<Outcome>>) {
        Self::with_task_limit(timeout, crate::context_tasks::DEFAULT_MAX_CONTEXT_TASKS)
    }
    pub fn with_task_limit(
        timeout: Duration,
        limit: usize,
    ) -> (Arc<Self>, watch::Sender<Option<Outcome>>) {
        Self::with_lifetime(
            timeout,
            limit,
            None,
            Arc::new(tokio::sync::Semaphore::new(1)),
        )
    }
    pub(crate) fn with_lifetime(
        timeout: Duration,
        limit: usize,
        lifetime: Option<Arc<crate::children::ChildLifetime>>,
        decision_slots: Arc<tokio::sync::Semaphore>,
    ) -> (Arc<Self>, watch::Sender<Option<Outcome>>) {
        let (finished, completion) = watch::channel(None);
        (
            Arc::new(Self {
                dynamic_escalation: Default::default(),
                stop_failure: Mutex::new(None),
                failure_context: Mutex::new(None),
                typed_tree: OnceLock::new(),
                typed_escalation_changed: Notify::new(),
                children: crate::children::ChildScope::new(timeout, decision_slots),
                lifetime: lifetime
                    .unwrap_or_else(|| crate::children::ChildLifetime::new(std::sync::Weak::new())),
                channel: TypedChannelState::default(),
                initialization_gate: None,
                group_activation: None,
                upstream_stop: None,
                stop_hook: OnceLock::new(),
                tasks: crate::context_tasks::ContextTasks::new(limit),
                deadline: Mutex::new(None),
                deadline_enforced: AtomicBool::new(false),
                supervision_failed: AtomicBool::new(false),
                explicit_stop: AtomicBool::new(false),
                escalated: Mutex::new(None),
                changed: Notify::new(),
                completion,
                timeout,
                death_watch: parrot_api::death_watch::DeathWatch::new(MAX_DEATH_WATCHERS),
            }),
            finished,
        )
    }
    pub(crate) async fn supervise_message(
        self: &Arc<Self>,
        result: ActorResult<parrot_api::types::BoxedMessage>,
        unmanaged: bool,
        runtime: &tokio::runtime::Handle,
    ) -> (
        ActorResult<parrot_api::types::BoxedMessage>,
        Option<Arc<ActorError>>,
    ) {
        let error = match result {
            Ok(reply) => return (Ok(reply), None),
            Err(error) => Arc::new(error),
        };
        if self.is_stopping() {
            return (Err(crate::supervision_policy::reply_error(error)), None);
        }
        let resume = self
            .lifetime
            .resume_message(
                error.clone(),
                Instant::now() + self.timeout,
                unmanaged && !matches!(error.as_ref(), ActorError::Panic(_)),
                runtime,
            )
            .await;
        let failure = if resume && !self.is_stopping() {
            None
        } else {
            self.record_failure_context();
            Some(error.clone())
        };
        (Err(crate::supervision_policy::reply_error(error)), failure)
    }
    pub(crate) fn failure_context(&self) -> Option<parrot_api::trace::TraceContext> {
        self.failure_context.lock().unwrap().clone().flatten()
    }
    pub(crate) fn record_failure_context(&self) {
        self.failure_context
            .lock()
            .unwrap()
            .get_or_insert_with(parrot_api::trace::TraceContext::current);
    }
    pub(crate) fn is_stopping(&self) -> bool {
        self.channel.is_sealed()
            || self
                .upstream_stop
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|logical| logical.channel.is_sealed())
    }
    pub(crate) async fn cancellation(&self) {
        if let Some(logical) = self.upstream_stop.as_ref().and_then(Weak::upgrade) {
            tokio::select! {
                biased;
                _ = self.channel.closed() => {},
                _ = logical.channel.closed() => {},
            }
        } else {
            self.channel.closed().await;
        }
    }
    pub fn request_stop(&self, deadline: Instant) {
        self.explicit_stop.store(true, Ordering::Release);
        self.begin_cleanup(deadline);
        if let Some(hook) = self.stop_hook.get() {
            hook(deadline);
        }
    }
    pub(crate) fn begin_cleanup(&self, deadline: Instant) {
        if let Some(tree) = self.typed_tree.get() {
            self.lifetime.cancel_recovery();
            tree.close();
        }
        let deadline = self
            .lifetime
            .decision_deadline()
            .map_or(deadline, |decision| decision.min(deadline));
        self.children.close();
        self.death_watch.close();
        {
            let mut current = self.deadline.lock().unwrap();
            *current = Some(current.map_or(deadline, |old| old.min(deadline)));
        }
        self.tasks.close();
        self.channel.seal();
        self.changed.notify_waiters();
    }

    /// A recoverable operation temporarily bounds the logical actor without
    /// closing its mailbox. Explicit stop and expired operations remain final.
    pub(super) fn begin_construction(&self, deadline: Instant) -> Instant {
        let mut current = self.deadline.lock().unwrap();
        let deadline = current.map_or(deadline, |old| old.min(deadline));
        *current = Some(deadline);
        self.changed.notify_waiters();
        deadline
    }
    pub(super) fn construction_complete(&self, deadline: Instant) -> ActorResult<bool> {
        let mut current = self.deadline.lock().unwrap();
        if self.explicit_stop.load(Ordering::Acquire)
            || self.lifetime.is_stopping()
            || self.channel.is_sealed()
        {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            drop(current);
            self.begin_cleanup(deadline);
            return Err(self.enforce_deadline());
        }
        if *current == Some(deadline) {
            *current = None;
        }
        self.changed.notify_waiters();
        Ok(true)
    }

    pub(crate) fn cleanup_deadline(&self) -> Option<Instant> {
        *self.deadline.lock().unwrap()
    }
    pub fn expired_now(&self) -> bool {
        self.deadline
            .lock()
            .unwrap()
            .is_some_and(|deadline| deadline <= Instant::now())
    }
    /// Only the execution loop records this when it actually interrupts cleanup.
    /// A caller timing out, or user code returning Timeout, is not a forced stop.
    pub fn enforce_deadline(&self) -> ActorError {
        self.deadline_enforced.store(true, Ordering::Release);
        ActorError::Timeout
    }
    pub fn deadline_was_enforced(&self) -> bool {
        self.deadline_enforced.load(Ordering::Acquire)
    }
    pub async fn expired(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let deadline = *self.deadline.lock().unwrap();
            if let Some(deadline) = deadline {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {
                        if self.deadline.lock().unwrap().is_some_and(|current| current <= Instant::now()) { return; }
                    },
                    _ = changed => {},
                }
            } else {
                changed.await;
            }
        }
    }
    pub(crate) fn typed_tree(&self) -> &crate::typed_context::TypedTree {
        self.typed_tree
            .get()
            .expect("managed static actor has a typed tree")
    }
    pub(crate) async fn group_recovery(&self) -> Arc<crate::children::recovery::Recovery> {
        loop {
            let changed = self.typed_escalation_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(group) = self.lifetime.recovery() {
                return group;
            }
            changed.await;
        }
    }
    pub(super) async fn wait_typed_stage<F: std::future::Future>(
        &self,
        future: F,
        runtime: &tokio::runtime::Handle,
    ) -> crate::typed_supervision::Step<F::Output> {
        use crate::typed_supervision::Step;
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                _ = self.channel.closed() => return Step::Stopped,
                group = self.group_recovery() => return Step::Recovery(group),
                error = self.typed_escalation() => {
                    let deadline = self.cleanup_deadline().unwrap_or_else(|| tokio::time::Instant::now() + self.timeout);
                    let resume = tokio::select! {
                        biased;
                        _ = self.channel.closed() => false,
                        _ = self.group_recovery() => false,
                        resume = self.lifetime.resume_escalation(error.clone(), deadline, runtime) => resume,
                    };
                    let resumed = if resume {
                        // Coordinate clearing the signal and its timer with the
                        // producer. A concurrent signal cannot rearm a stale timer.
                        let mut signal = self.escalated.lock().unwrap();
                        if signal.as_ref().is_some_and(|pending| Arc::ptr_eq(pending, &error))
                            && self.construction_complete(deadline).unwrap_or(false) {
                            signal.take();
                            true
                        } else { false }
                    } else { false };
                    if !resumed { return Step::Escalated(error); }
                    // Resume the same pinned future: no replay, lost reply,
                    // partial initialization activation or generation change.
                },
                result = &mut future => return Step::Ready(result),
            }
        }
    }

    pub(crate) async fn typed_escalation(&self) -> Arc<ActorError> {
        loop {
            let changed = self.typed_escalation_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(error) = self.escalated.lock().unwrap().clone() {
                return error;
            }
            changed.await;
        }
    }
}
impl TypedActorLifecycle for ActorControl {
    fn death_watch(&self) -> Option<parrot_api::death_watch::DeathWatch> {
        Some(self.death_watch.clone())
    }
    fn channel(&self) -> &TypedChannelState {
        &self.channel
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.request_stop(Instant::now() + self.timeout);
            await_completion(self.completion.clone()).await
        })
    }
}

pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    crate::owned_payload::panic_message(payload)
}
