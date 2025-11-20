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
    // Hook failure and user-future disposal are actual cleanup failures,
    // independently of the execution outcome. Consume after ContextFut disposal.
    pub(crate) cleanup_failure: Mutex<Option<crate::error_receipt::ActorReceipt>>,
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
    stop_context: crate::stop_context::StopContext,
    pub(crate) explicit_stop: AtomicBool,
    pub(crate) escalated: Mutex<Option<crate::error_receipt::ActorReceipt>>,
    changed: Notify,
    pub completion: Completion,
    pub timeout: Duration,
    pub(crate) creation_timeout: Duration,
    pub(crate) message_timeout: Duration,
    creation_limit: Duration,
    initialization_deadline: Mutex<Option<Instant>>,
    pub death_watch: parrot_api::death_watch::DeathWatch,
}

struct MessageOutcome {
    result: Option<Result<parrot_api::types::BoxedMessage, crate::error_receipt::ActorReceipt>>,
    control: Arc<ActorControl>,
}
impl Drop for MessageOutcome {
    fn drop(&mut self) {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.result.take())));
        if let Err(payload) = result {
            let error = crate::error_receipt::ActorReceipt::from_error(ActorError::Panic(
                crate::owned_payload::panic_message(payload),
            ));
            self.control
                .cleanup_failure
                .lock()
                .unwrap()
                .get_or_insert(error);
        }
    }
}
impl ActorControl {
    pub(crate) fn configure_timeouts(
        control: &mut Arc<Self>,
        timeouts: &parrot_api::system::SystemTimeouts,
    ) {
        let control = Arc::get_mut(control).expect("unpublished control");
        control.creation_timeout = if timeouts.actor_creation.is_zero() {
            control.timeout
        } else {
            timeouts.actor_creation
        };
        control.message_timeout = timeouts.message_handling;
        control.creation_limit = timeouts.actor_creation;
        *control.initialization_deadline.get_mut().unwrap() =
            (!timeouts.actor_creation.is_zero()).then(|| Instant::now() + timeouts.actor_creation);
    }
    pub(crate) fn initialization_deadline(&self) -> Option<Instant> {
        *self.initialization_deadline.lock().unwrap()
    }
    pub(crate) fn finish_initialization(&self) {
        self.initialization_deadline.lock().unwrap().take();
    }
    pub(crate) fn reconstruction_deadline(&self, recovery: Instant) -> Instant {
        if self.creation_limit.is_zero() {
            recovery
        } else {
            recovery.min(Instant::now() + self.creation_limit)
        }
    }
    pub(super) fn begin_reconstruction(&self, recovery: Instant) -> Instant {
        let deadline = self.begin_construction_quiet(self.reconstruction_deadline(recovery));
        *self.initialization_deadline.lock().unwrap() =
            (!self.creation_limit.is_zero()).then_some(deadline);
        self.changed.notify_waiters();
        deadline
    }
    pub(crate) fn bound_initialization(&self, deadline: Option<Instant>) {
        if !self.creation_limit.is_zero() {
            let mut current = self.initialization_deadline.lock().unwrap();
            *current = match (*current, deadline) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (left, right) => left.or(right),
            };
        }
    }
    #[cfg(test)]
    pub(crate) fn check_escalation_locks(&self) {
        drop(self.escalated.lock().unwrap());
        drop(self.deadline.lock().unwrap());
    }
    #[cfg(test)]
    pub(crate) async fn escalation_deadline_waiter(&self) {
        self.expired().await;
    }
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
                cleanup_failure: Mutex::new(None),
                failure_context: Mutex::new(None),
                stop_context: Default::default(),
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
                creation_timeout: timeout,
                message_timeout: Duration::ZERO,
                creation_limit: Duration::ZERO,
                initialization_deadline: Mutex::new(None),
                death_watch: parrot_api::death_watch::DeathWatch::new(MAX_DEATH_WATCHERS),
            }),
            finished,
        )
    }
    pub(crate) fn supervise_message(
        self: &Arc<Self>,
        result: ActorResult<parrot_api::types::BoxedMessage>,
        unmanaged: bool,
        runtime: &tokio::runtime::Handle,
    ) -> impl std::future::Future<
        Output = (
            ActorResult<parrot_api::types::BoxedMessage>,
            Option<Arc<ActorError>>,
        ),
    > + Send
    + 'static {
        let result = result.map_err(crate::error_receipt::ActorReceipt::from_error);
        let mut outcome = MessageOutcome {
            result: Some(result),
            control: self.clone(),
        };
        let control = self.clone();
        let runtime = runtime.clone();
        async move {
            let error = match outcome.result.take().expect("unpublished message outcome") {
                Ok(reply) => return (Ok(reply), None),
                Err(error) => error,
            };
            if control.cleanup_failure.lock().unwrap().is_some() {
                control.record_failure_context();
                let failure = error.error().clone();
                return (
                    Err(crate::supervision_policy::reply_error(error)),
                    Some(failure),
                );
            }
            if control.is_stopping() {
                return (Err(crate::supervision_policy::reply_error(error)), None);
            }
            let resume = control
                .lifetime
                .resume_message(
                    error.clone(),
                    Instant::now() + control.timeout,
                    unmanaged && !matches!(error.error().as_ref(), ActorError::Panic(_)),
                    &runtime,
                )
                .await;
            let failure = if resume && !control.is_stopping() {
                None
            } else {
                control.record_failure_context();
                Some(error.error().clone())
            };
            (Err(crate::supervision_policy::reply_error(error)), failure)
        }
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
        let context = self.stop_context.claim_current();
        self.explicit_stop.store(true, Ordering::Release);
        self.begin_cleanup(deadline);
        if let Some(hook) = self.stop_hook.get() {
            parrot_api::trace::in_scope(context, || hook(deadline));
        }
    }
    pub(crate) fn take_stop_context(&self) -> Option<parrot_api::trace::TraceContext> {
        self.stop_context.take_or_else(|| {
            // A factory's cancellation listener can observe its logical seal
            // before the forwarding hook reaches the current generation.
            self.upstream_stop
                .as_ref()
                .and_then(Weak::upgrade)
                .and_then(|logical| logical.stop_context.get())
        })
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
        let deadline = if self.creation_limit.is_zero() {
            deadline
        } else {
            let mut current = self.initialization_deadline.lock().unwrap();
            let deadline =
                deadline.min(current.unwrap_or_else(|| Instant::now() + self.creation_limit));
            *current = Some(deadline);
            deadline
        };
        let deadline = self.begin_construction_quiet(deadline);
        self.changed.notify_waiters();
        deadline
    }
    pub(super) fn begin_construction_quiet(&self, deadline: Instant) -> Instant {
        let mut current = self.deadline.lock().unwrap();
        let deadline = current.map_or(deadline, |old| old.min(deadline));
        *current = Some(deadline);
        deadline
    }
    pub(super) fn construction_complete(&self, deadline: Instant) -> ActorResult<bool> {
        let result = self.construction_complete_quiet(deadline);
        if result.is_err() {
            self.begin_cleanup(deadline);
        }
        self.changed.notify_waiters();
        result
    }
    pub(super) fn construction_complete_quiet(&self, deadline: Instant) -> ActorResult<bool> {
        let mut current = self.deadline.lock().unwrap();
        if self.explicit_stop.load(Ordering::Acquire)
            || self.lifetime.is_stopping()
            || self.channel.is_sealed()
        {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            return Err(self.enforce_deadline());
        }
        if *current == Some(deadline) {
            *current = None;
        }
        Ok(true)
    }

    pub(super) fn construction_changed(&self) {
        self.changed.notify_waiters();
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
                        let completed = if signal.as_ref().is_some_and(|pending| Arc::ptr_eq(pending, &error)) {
                            self.construction_complete_quiet(deadline)
                        } else { Ok(false) };
                        let removed = if matches!(completed, Ok(true)) { signal.take() } else { None };
                        drop(signal);
                        self.changed.notify_waiters();
                        if completed.is_err() { self.begin_cleanup(deadline); }
                        removed.is_some()
                    } else { false };
                    if !resumed { return Step::Escalated(error); }
                    // Resume the same pinned future: no replay, lost reply,
                    // partial initialization activation or generation change.
                },
                result = &mut future => return Step::Ready(result),
            }
        }
    }

    pub(crate) async fn typed_escalation(&self) -> crate::error_receipt::ActorReceipt {
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
    fn retirement_status(&self) -> parrot_api::address::RetirementStatus {
        self.lifetime.retirement_status()
    }
    fn death_watch(&self) -> Option<parrot_api::death_watch::DeathWatch> {
        Some(self.death_watch.clone())
    }
    fn channel(&self) -> &TypedChannelState {
        &self.channel
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.request_stop(Instant::now() + self.timeout);
            await_completion(self.completion.clone()).await
        }))
    }
}

pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    crate::owned_payload::panic_message(payload)
}

#[cfg(test)]
mod typed_signal_contracts {
    use super::*;
    use crate::typed_supervision::Step;
    use std::future::Future;
    use std::sync::atomic::AtomicUsize;
    use std::task::{Context, Poll};

    struct Wakes(AtomicUsize);
    impl futures::task::ArcWake for Wakes {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    async fn recovery_fixture() -> (
        Arc<crate::children::ChildScope>,
        Arc<ActorControl>,
        Arc<crate::children::recovery::Recovery>,
    ) {
        use parrot_api::supervisor::{
            BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType,
        };
        let slots = Arc::new(tokio::sync::Semaphore::new(2));
        let scope = crate::children::ChildScope::new(Duration::from_secs(5), slots.clone());
        scope.set_strategy(SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 1,
            within: Duration::from_secs(30),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
        }));
        let mut members = Vec::new();
        for id in 0..2 {
            scope
                .spawn(|lifetime| {
                    lifetime.bind_recovery(Arc::new(|_| {}));
                    members.push(lifetime.clone());
                    Ok((
                        parrot_api::address::ActorPath::placeholder(format!("/signal/{id}"))
                            .target()
                            .clone_boxed(),
                        Arc::new(move || {
                            let lifetime = lifetime.clone();
                            Box::pin(async move { lifetime.wait().await })
                        }),
                    ))
                })
                .unwrap();
        }
        let (control, _finished) = ActorControl::with_lifetime(
            Duration::from_secs(5),
            1,
            Some(members[0].clone()),
            slots.clone(),
        );
        let recovery = members[0]
            .failure_policy(
                Arc::new(ActorError::InternalError("cohort trigger".into())),
                slots,
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap()
            .unwrap()
            .recovery
            .unwrap();
        (scope, control, recovery)
    }

    #[tokio::test]
    async fn recovery_wins_over_ready_escalation_and_ready_user_result() {
        let (scope, control, recovery) = recovery_fixture().await;
        let polls = AtomicUsize::new(0);
        *control.escalated.lock().unwrap() =
            Some(Arc::new(ActorError::InternalError("pending".into())).into());
        let result = control
            .wait_typed_stage(
                std::future::poll_fn(|_| {
                    polls.fetch_add(1, Ordering::SeqCst);
                    Poll::Ready(42)
                }),
                &tokio::runtime::Handle::current(),
            )
            .await;
        assert!(matches!(result, Step::Recovery(ref actual) if Arc::ptr_eq(actual, &recovery)));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        scope.close();
    }

    #[tokio::test]
    async fn seal_wins_over_ready_recovery() {
        let (scope, control, _recovery) = recovery_fixture().await;
        control.channel.seal();
        let result = control
            .wait_typed_stage(std::future::ready(42), &tokio::runtime::Handle::current())
            .await;
        assert!(matches!(result, Step::Stopped));
        scope.close();
    }

    #[tokio::test]
    async fn pending_escalation_wakes_original_stage_without_polling_user_again() {
        let (control, _finished) = ActorControl::new(Duration::from_secs(5));
        let runtime = tokio::runtime::Handle::current();
        let polls = AtomicUsize::new(0);
        let user = std::future::poll_fn(|_| {
            polls.fetch_add(1, Ordering::SeqCst);
            Poll::<()>::Pending
        });
        let mut stage = Box::pin(control.wait_typed_stage(user, &runtime));
        let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
        let waker = futures::task::waker_ref(&wakes);
        assert!(
            stage
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        let error = Arc::new(ActorError::InternalError("stage escalation".into()));
        *control.escalated.lock().unwrap() = Some(error.clone().into());
        control.typed_escalation_changed.notify_waiters();
        assert!(wakes.0.load(Ordering::SeqCst) > 0);
        let result = tokio::time::timeout(Duration::from_secs(1), stage)
            .await
            .unwrap();
        assert!(matches!(result, Step::Escalated(ref actual) if Arc::ptr_eq(actual, &error)));
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn seal_wins_over_ready_escalation_and_ready_user_result() {
        let (control, _finished) = ActorControl::new(Duration::from_secs(5));
        let polls = AtomicUsize::new(0);
        *control.escalated.lock().unwrap() =
            Some(Arc::new(ActorError::InternalError("pending".into())).into());
        control.channel.seal();
        let result = control
            .wait_typed_stage(
                std::future::poll_fn(|_| {
                    polls.fetch_add(1, Ordering::SeqCst);
                    Poll::Ready(42)
                }),
                &tokio::runtime::Handle::current(),
            )
            .await;
        assert!(matches!(result, Step::Stopped));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn ready_escalation_wins_over_ready_user_result() {
        let (control, _finished) = ActorControl::new(Duration::from_secs(5));
        let polls = AtomicUsize::new(0);
        let error = Arc::new(ActorError::InternalError("pending".into()));
        *control.escalated.lock().unwrap() = Some(error.clone().into());
        let result = control
            .wait_typed_stage(
                std::future::poll_fn(|_| {
                    polls.fetch_add(1, Ordering::SeqCst);
                    Poll::Ready(42)
                }),
                &tokio::runtime::Handle::current(),
            )
            .await;
        assert!(matches!(result, Step::Escalated(ref actual) if Arc::ptr_eq(actual, &error)));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unrelated_broadcast_rearms_original_pending_escalation() {
        let (control, _finished) = ActorControl::new(Duration::from_secs(5));
        let mut signal = Box::pin(control.typed_escalation());
        let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
        let waker = futures::task::waker_ref(&wakes);
        assert!(
            signal
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        control.typed_escalation_changed.notify_waiters();
        assert!(wakes.0.load(Ordering::SeqCst) > 0);
        assert!(
            signal
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        wakes.0.store(0, Ordering::SeqCst);
        let error = Arc::new(ActorError::InternalError("after rearm".into()));
        *control.escalated.lock().unwrap() = Some(error.clone().into());
        control.typed_escalation_changed.notify_waiters();
        assert!(wakes.0.load(Ordering::SeqCst) > 0);
        let result = tokio::time::timeout(Duration::from_secs(1), signal)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&result, &error));
    }
}
