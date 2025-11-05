//! Bounded tasks owned by one actor generation, independent of its executor.
use parrot_api::{
    errors::ActorError,
    message::CloneableMessage,
    trace::{SpanGuard, SpanKind, SpanStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::Notify;
use uuid::Uuid;

pub(crate) const DEFAULT_MAX_CONTEXT_TASKS: usize = parrot_config::Defaults::max_context_tasks();

struct RegistrationWorker(Arc<parrot_api::receptionist::RegistrationScope>);
impl Drop for RegistrationWorker {
    fn drop(&mut self) {
        self.0.retire();
    }
}

#[cfg(test)]
mod registration_contracts {
    use super::*;
    use parrot_api::receptionist::{ReceptionistKey, RegistrationScope};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn bind(scope: &RegistrationScope, cleanup: impl FnMut() -> ActorResult<()> + Send + 'static) {
        scope
            .publish(
                (
                    1,
                    ReceptionistKey::new("edge/worker").unwrap(),
                    "/worker".into(),
                ),
                cleanup,
                || (),
            )
            .unwrap();
    }

    #[test]
    fn runtime_cancels_unpolled_registration_worker_with_receipt() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let owner = ContextTasks::new(0);
        let scope = owner.registration_scope(runtime.handle());
        let count = Arc::new(AtomicUsize::new(0));
        let retired = count.clone();
        bind(&scope, move || {
            retired.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        drop(runtime);
        futures::executor::block_on(scope.join()).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(matches!(scope.with_live(|| ()), Err(ActorError::Stopped)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generation_join_waits_for_actual_registration_cleanup() {
        use futures::FutureExt;
        let owner = ContextTasks::new(0);
        let scope = owner.registration_scope(&tokio::runtime::Handle::current());
        let (started, arrived) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        let (release, wait) = std::sync::mpsc::channel();
        bind(&scope, move || {
            started.take().unwrap().send(()).unwrap();
            wait.recv().unwrap();
            Ok(())
        });
        owner.close();
        arrived.await.unwrap();
        assert!(
            owner.join().now_or_never().is_none(),
            "generation cannot finish while service cleanup is blocked"
        );
        release.send(()).unwrap();
        owner.join().await.unwrap();
        assert!(matches!(scope.with_live(|| ()), Err(ActorError::Stopped)));
    }

    #[tokio::test]
    async fn first_registration_after_generation_closure_is_rejected() {
        let owner = ContextTasks::new(0);
        owner.close();
        let scope = owner.registration_scope(&tokio::runtime::Handle::current());
        assert!(matches!(
            scope.publish(
                (
                    1,
                    ReceptionistKey::new("edge/late").unwrap(),
                    "/late".into()
                ),
                || panic!("rejected binding must not execute cleanup"),
                || panic!("closed generation must not publish")
            ),
            Err(ActorError::Stopped)
        ));
        owner.join().await.unwrap();
    }

    #[tokio::test]
    async fn registration_cleanup_panic_is_part_of_generation_receipt() {
        let owner = ContextTasks::new(0);
        let scope = owner.registration_scope(&tokio::runtime::Handle::current());
        bind(&scope, || panic!("generation cleanup panic"));
        owner.close();
        for _ in 0..2 {
            assert!(
                matches!(owner.join().await, Err(ActorError::Panic(message)) if message == "generation cleanup panic")
            );
        }
    }
}

#[derive(Debug, Default)]
struct Status {
    closed: bool,
    active: usize,
    idle_worker: bool,
    panic: Option<String>,
    scheduled: HashMap<Uuid, Arc<ScheduleCancellation>>,
}
#[derive(Debug, Default)]
struct ScheduleCancellation {
    cancelled: std::sync::atomic::AtomicBool,
    notify: Notify,
}
impl ScheduleCancellation {
    fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_waiters();
    }
    async fn wait(&self) {
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            notified.await;
        }
    }
}
#[derive(Debug, Default)]
struct State {
    status: Mutex<Status>,
    cancelled: Notify,
    finished: Notify,
}
impl State {
    fn close(&self) {
        let scheduled = {
            let mut status = self.status.lock().unwrap();
            status.closed = true;
            status
                .scheduled
                .drain()
                .map(|(_, token)| token)
                .collect::<Vec<_>>()
        };
        for token in scheduled {
            token.cancel();
        }
        self.cancelled.notify_waiters();
    }
    async fn cancelled(&self) {
        let notified = self.cancelled.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let closed = self.status.lock().unwrap().closed;
        if !closed {
            notified.await;
        }
    }
    fn record_panic(&self, payload: Box<dyn std::any::Any + Send>) {
        // Payload destruction is user code. Finish it outside the state lock,
        // before releasing the task ticket, and isolate a secondary panic.
        let message = crate::owned_payload::panic_message(payload);
        self.status.lock().unwrap().panic.get_or_insert(message);
    }
}

/// Contexts and lifecycle controllers own this value. Tasks retain only State,
/// so a long-lived timer cannot keep its own cancellation owner alive.
#[derive(Debug)]
pub(crate) struct ContextTasks {
    state: Arc<State>,
    limit: usize,
    registrations: OnceLock<Arc<parrot_api::receptionist::RegistrationScope>>,
}
impl Drop for ContextTasks {
    fn drop(&mut self) {
        self.close();
    }
}
impl ContextTasks {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::default(),
            limit,
            registrations: OnceLock::new(),
        })
    }
    pub(crate) fn close(&self) {
        self.state.close();
        if let Some(scope) = self.registrations.get() {
            scope.close();
        }
    }

    pub(crate) fn registration_scope(
        &self,
        runtime: &tokio::runtime::Handle,
    ) -> Arc<parrot_api::receptionist::RegistrationScope> {
        let status = self.state.status.lock().unwrap();
        let scope = self
            .registrations
            .get_or_init(|| {
                let scope = Arc::new(parrot_api::receptionist::RegistrationScope::default());
                // Construct the guard before spawn: runtime cancellation must also
                // retire a worker that has never been polled. At spawn the scope is
                // still empty, so synchronous rejection cannot call a table Waker.
                let worker = RegistrationWorker(scope.clone());
                runtime.spawn(async move {
                    worker.0.closed().await;
                    drop(worker);
                });
                scope
            })
            .clone();
        let closed = status.closed;
        drop(status);
        if closed {
            scope.close();
        }
        scope
    }
    /// Record cleanup failure even when cancellation removes the call's waiter.
    /// A generation retains its first failure; ordinary returned errors do not
    /// enter this receipt. No user formatting/destruction runs under this lock.
    pub(crate) fn record_message_disposal_failure(&self, message: &str) {
        self.state
            .status
            .lock()
            .unwrap()
            .panic
            .get_or_insert_with(|| message.to_owned());
    }

    pub(crate) async fn join(&self) -> ActorResult<()> {
        let tasks = loop {
            let finished = self.state.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            {
                let state = self.state.status.lock().unwrap();
                if state.active == 0 {
                    break match &state.panic {
                        Some(message) => Err(ActorError::Panic(message.clone())),
                        None => Ok(()),
                    };
                }
            }
            finished.await;
        };
        let registrations = match self.registrations.get() {
            Some(scope) => scope.join().await,
            None => Ok(()),
        };
        tasks.and(registrations)
    }
    pub(crate) fn user_task(
        &self,
        future: impl Future<Output = ActorResult<()>> + Send + 'static,
    ) -> ActorResult<OwnedTask> {
        self.register_owned(future, false, "context.task")
    }
    /// One reserved clock per generation, independent of the user timer quota.
    /// The same cancellation and disposal receipt covers both kinds of task.
    pub(crate) fn idle_worker(
        &self,
        future: impl Future<Output = ActorResult<()>> + Send + 'static,
    ) -> ActorResult<OwnedTask> {
        self.register_owned(future, true, "context.idle")
    }
    fn register_owned(
        &self,
        future: impl Future<Output = ActorResult<()>> + Send + 'static,
        idle_worker: bool,
        label: &'static str,
    ) -> ActorResult<OwnedTask> {
        // Maintenance clocks must not inherit the business call that configured them.
        let context = if idle_worker {
            None
        } else {
            parrot_api::trace::TraceContext::current()
        };
        parrot_api::trace::in_scope(context.clone(), || {
            // No user payload or future is destroyed while holding the admission lock.
            let admission = {
                let mut state = self.state.status.lock().unwrap();
                if state.closed {
                    Err(ActorError::Stopped)
                } else if (idle_worker && state.idle_worker)
                    || (!idle_worker && state.active - usize::from(state.idle_worker) >= self.limit)
                {
                    Err(ActorError::InternalError(format!(
                        "actor context task limit {} reached",
                        self.limit
                    )))
                } else {
                    state.active += 1;
                    state.idle_worker |= idle_worker;
                    Ok(Ticket(self.state.clone(), idle_worker))
                }
            };
            let ticket = match admission {
                Ok(ticket) => ticket,
                Err(error) => {
                    // Rejection never acquired a generation ticket. Retire its
                    // future independently and report actual disposal failure.
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future)))
                        .map_err(|payload| {
                            ActorError::Panic(crate::owned_payload::panic_message(payload))
                        })?;
                    return Err(error);
                }
            };
            let state = self.state.clone();
            Ok(OwnedTask {
                span: (!idle_worker).then(|| SpanGuard::child(SpanKind::ManagedTask, label)),
                outcome: SpanStatus::Cancelled,
                context,
                // Keep user storage outside a select! future: unwinding a poll
                // must not drop the same input inside that unwind. Poll and Drop
                // are isolated separately by OwnedTask before releasing its ticket.
                future: Some(Box::pin(future)),
                cancelled: Some(Box::pin(async move { state.cancelled().await })),
                ticket: Some(ticket),
                input_failures: None,
            })
        })
    }
    pub(crate) fn once(
        &self,
        target: BoxedActorRef,
        message: BoxedMessage,
        delay: Duration,
    ) -> ActorResult<OwnedTask> {
        self.once_with_id(target, message, delay)
            .map(|(_, task)| task)
    }

    pub(crate) fn once_with_id(
        &self,
        target: BoxedActorRef,
        message: BoxedMessage,
        delay: Duration,
    ) -> ActorResult<(Uuid, OwnedTask)> {
        let failures = InputFailures::new();
        let target = ScheduleInput::new(target, failures.clone());
        let message = ScheduleInput::new(message, failures.clone());
        let task_failures = failures.clone();
        let result = (move || {
            let deadline = checked_deadline(delay)?;
            let id = Uuid::new_v4();
            let cancellation = Arc::new(ScheduleCancellation::default());
            self.insert_schedule(id, cancellation.clone())?;
            let state = self.state.clone();
            let delivery_state = state.clone();
            let future = async move {
                tokio::select! {
                    biased;
                    _ = cancellation.wait() => { task_failures.cancelled(); Ok(()) },
                    _ = tokio::time::sleep_until(deadline) => {
                        tokio::select! {
                            biased;
                            _ = cancellation.wait() => { task_failures.cancelled(); Ok(()) },
                            result = ScheduledDelivery::new(target.value().deliver(message.take()), delivery_state, task_failures.clone()) => result,
                        }
                    },
                }
            };
            self.register_owned(
                ScheduledFuture { future, state, id },
                false,
                "context.schedule.once",
            )
            .map(|task| (id, task))
            .inspect_err(|_| {
                self.remove_schedule(id);
            })
        })();
        match result {
            Ok((id, mut task)) => {
                task = task.with_input_failures(failures);
                Ok((id, task))
            }
            Err(error) => match failures.take() {
                Some(message) => Err(ActorError::Panic(message)),
                None => Err(error),
            },
        }
    }
    pub(crate) fn periodic(
        &self,
        target: BoxedActorRef,
        message: CloneableMessage,
        initial_delay: Duration,
        interval: Duration,
    ) -> ActorResult<OwnedTask> {
        self.periodic_with_id(target, message, initial_delay, interval)
            .map(|(_, task)| task)
    }

    pub(crate) fn periodic_with_id(
        &self,
        target: BoxedActorRef,
        message: CloneableMessage,
        initial_delay: Duration,
        interval: Duration,
    ) -> ActorResult<(Uuid, OwnedTask)> {
        let failures = InputFailures::new();
        let target = ScheduleInput::new(target, failures.clone());
        let message = ScheduleInput::new(message, failures.clone());
        let task_failures = failures.clone();
        let result = (move || {
            if interval.is_zero() {
                return Err(ActorError::InternalError(
                    "periodic interval must be positive".into(),
                ));
            }
            let initial = checked_deadline(initial_delay)?;
            checked_deadline(interval)?;
            let id = Uuid::new_v4();
            let cancellation = Arc::new(ScheduleCancellation::default());
            self.insert_schedule(id, cancellation.clone())?;
            let state = self.state.clone();
            let delivery_state = state.clone();
            let future = async move {
                tokio::select! {
                    biased;
                    _ = cancellation.wait() => { task_failures.cancelled(); Ok(()) },
                    _ = tokio::time::sleep_until(initial) => {
                        loop {
                            tokio::select! {
                                biased;
                                _ = cancellation.wait() => { task_failures.cancelled(); return Ok(()) },
                                result = async {
                                    let payload = message.value().clone().into_boxed();
                                    ScheduledDelivery::new(target.value().deliver(payload), delivery_state.clone(), task_failures.clone()).await
                                } => result,
                            }?;
                            tokio::select! {
                                biased;
                                _ = cancellation.wait() => { task_failures.cancelled(); return Ok(()) },
                                _ = tokio::time::sleep_until(checked_deadline(interval)?) => {},
                            }
                        }
                    }
                }
            };
            self.register_owned(
                ScheduledFuture { future, state, id },
                false,
                "context.schedule.periodic",
            )
            .map(|task| (id, task))
            .inspect_err(|_| {
                self.remove_schedule(id);
            })
        })();
        match result {
            Ok((id, mut task)) => {
                task = task.with_input_failures(failures);
                Ok((id, task))
            }
            Err(error) => match failures.take() {
                Some(message) => Err(ActorError::Panic(message)),
                None => Err(error),
            },
        }
    }

    fn insert_schedule(
        &self,
        id: Uuid,
        cancellation: Arc<ScheduleCancellation>,
    ) -> ActorResult<()> {
        let mut status = self.state.status.lock().unwrap();
        if status.closed {
            return Err(ActorError::Stopped);
        }
        status.scheduled.insert(id, cancellation);
        Ok(())
    }

    fn remove_schedule(&self, id: Uuid) {
        self.state.status.lock().unwrap().scheduled.remove(&id);
    }

    pub(crate) fn cancel_schedule(&self, id: Uuid) {
        if let Some(token) = self
            .state
            .status
            .lock()
            .unwrap()
            .scheduled
            .get(&id)
            .cloned()
        {
            token.cancel();
        }
    }
}
fn checked_deadline(delay: Duration) -> ActorResult<tokio::time::Instant> {
    tokio::time::Instant::now()
        .checked_add(delay)
        .ok_or_else(|| ActorError::InternalError("scheduled delay exceeds clock range".into()))
}
/// Independent inputs may move to an actor callback before the worker ends.
/// Defer cleanup reporting until the worker has recorded its primary failure;
/// after retirement, forward late cleanup to the same generation receipt.
#[derive(Clone)]
pub(crate) struct InputFailures(Arc<Mutex<InputFailureState>>);
#[derive(Default)]
struct InputFailureState {
    first: Option<String>,
    generation: std::sync::Weak<State>,
    retired: bool,
    bound: bool,
    panicked: bool,
    cancelled: bool,
}
impl InputFailures {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(InputFailureState::default())))
    }
    fn cancelled(&self) {
        self.0.lock().unwrap().cancelled = true;
    }
    pub(crate) fn record(&self, message: String) {
        let mut failures = self.0.lock().unwrap();
        failures.panicked = true;
        if failures.retired {
            if let Some(generation) = failures.generation.upgrade() {
                generation
                    .status
                    .lock()
                    .unwrap()
                    .panic
                    .get_or_insert(message);
            }
        } else {
            failures.first.get_or_insert(message);
        }
    }
    pub(crate) fn take(&self) -> Option<String> {
        self.0.lock().unwrap().first.take()
    }
    pub(crate) fn take_unadmitted(&self) -> Option<String> {
        let mut failures = self.0.lock().unwrap();
        if failures.bound {
            None
        } else {
            failures.first.take()
        }
    }
    fn bind(&self, generation: &Arc<State>) {
        let mut failures = self.0.lock().unwrap();
        assert!(
            failures.generation.upgrade().is_none(),
            "input failures bound twice"
        );
        failures.generation = Arc::downgrade(generation);
        failures.bound = true;
    }
    fn flush(&self, retire: bool) -> (bool, bool) {
        // All accesses use input-state -> generation-state order. Only owned
        // strings are handled under these locks; no user code runs here.
        let mut failures = self.0.lock().unwrap();
        if let Some(message) = failures.first.take()
            && let Some(generation) = failures.generation.upgrade()
        {
            generation
                .status
                .lock()
                .unwrap()
                .panic
                .get_or_insert(message);
        }
        failures.retired |= retire;
        // Snapshot at ownership retirement; late transferred payload failures
        // still reach the generation receipt, not an already completed span.
        (failures.panicked, failures.cancelled)
    }
}

/// Retire independent schedule inputs separately, including rejected inputs.
/// Keep failures local until admission is known. The outer task publishes them
/// after catching a primary poll panic and before releasing its generation ticket.
struct ScheduleInput<T> {
    value: Option<T>,
    failures: InputFailures,
}
impl<T> ScheduleInput<T> {
    fn new(value: T, failures: InputFailures) -> Self {
        Self {
            value: Some(value),
            failures,
        }
    }
    fn value(&self) -> &T {
        self.value.as_ref().expect("live schedule input")
    }
    fn take(mut self) -> T {
        self.value.take().expect("live schedule input")
    }
}
impl<T> Drop for ScheduleInput<T> {
    fn drop(&mut self) {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.value.take())))
        {
            let message = crate::owned_payload::panic_message(payload);
            self.failures.record(message);
        }
    }
}

struct Ticket(Arc<State>, bool);
impl Drop for Ticket {
    fn drop(&mut self) {
        {
            let mut state = self.0.status.lock().unwrap();
            state.active -= 1;
            if self.1 {
                state.idle_worker = false;
            }
        }
        self.0.finished.notify_waiters();
    }
}

struct ScheduledFuture<F> {
    future: F,
    state: Arc<State>,
    id: Uuid,
}
impl<F> Drop for ScheduledFuture<F> {
    fn drop(&mut self) {
        // Registration belongs to the future, including executor abandonment
        // and unwinding polls. No user destructor runs under the status lock.
        self.state.status.lock().unwrap().scheduled.remove(&self.id);
    }
}
impl<F: Future<Output = ActorResult<()>>> Future for ScheduledFuture<F> {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `future` is pinned together with this wrapper and is never
        // moved after the wrapper is pinned.
        let this = unsafe { self.get_unchecked_mut() };
        let result = unsafe { Pin::new_unchecked(&mut this.future) }.poll(cx);
        if result.is_ready() {
            this.state.status.lock().unwrap().scheduled.remove(&this.id);
        }
        result
    }
}

/// A select! branch must not unwind through its own delivery storage: the
/// delivery's independent Drop can panic too. Catch each phase before returning
/// to the selector, and publish cleanup failures before the task ticket retires.
struct ScheduledDelivery<'a> {
    future: Option<BoxedFuture<'a, ActorResult<()>>>,
    state: Arc<State>,
    failures: InputFailures,
}
impl<'a> ScheduledDelivery<'a> {
    fn new(
        future: BoxedFuture<'a, ActorResult<()>>,
        state: Arc<State>,
        failures: InputFailures,
    ) -> Self {
        Self {
            future: Some(future),
            state,
            failures,
        }
    }
    fn failure(&self, payload: Box<dyn std::any::Any + Send>) -> ActorError {
        let message = crate::owned_payload::panic_message(payload);
        self.state
            .status
            .lock()
            .unwrap()
            .panic
            .get_or_insert(message.clone());
        self.failures.record(message.clone());
        ActorError::Panic(message)
    }
    fn dispose(&mut self) -> ActorResult<()> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.future.take())))
            .map_err(|payload| self.failure(payload))
    }
}
impl Drop for ScheduledDelivery<'_> {
    fn drop(&mut self) {
        // failure() has already recorded the error in the generation receipt.
        let _ = self.dispose();
    }
}
impl Future for ScheduledDelivery<'_> {
    type Output = ActorResult<()>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future
                .as_mut()
                .expect("live scheduled delivery")
                .as_mut()
                .poll(cx)
        }));
        let result = match outcome {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => result,
            Err(payload) => Err(self.failure(payload)),
        };
        // Preserve the first failure; a successful delivery with a failing
        // destructor must still terminate a periodic schedule.
        let disposal = self.dispose();
        Poll::Ready(result.and(disposal))
    }
}

/// Completion follows disposal even when the executor drops a suspended task.
/// Catch both polling and destructor panics before releasing its ticket.
pub(crate) struct OwnedTask {
    span: Option<SpanGuard>,
    outcome: SpanStatus,
    context: Option<parrot_api::trace::TraceContext>,
    future: Option<Pin<Box<dyn Future<Output = ActorResult<()>> + Send>>>,
    cancelled: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    ticket: Option<Ticket>,
    input_failures: Option<InputFailures>,
}
impl OwnedTask {
    pub(crate) fn with_input_failures(mut self, failures: InputFailures) -> Self {
        assert!(
            self.input_failures.is_none(),
            "task input failures installed twice"
        );
        failures.bind(&self.ticket.as_ref().expect("live input owner").0);
        self.input_failures = Some(failures);
        self
    }
    fn flush_input_failure(&self) {
        if let Some(failures) = &self.input_failures {
            failures.flush(false);
        }
    }
    fn record_panic(&mut self, payload: Box<dyn std::any::Any + Send>) {
        self.outcome = SpanStatus::Panicked;
        if let Some(ticket) = &self.ticket {
            ticket.0.record_panic(payload);
        }
    }
    fn dispose(&mut self) {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.future.take())))
        {
            self.record_panic(payload);
        }
        self.flush_input_failure();
        if let Some(failures) = self.input_failures.take() {
            let (panicked, cancelled) = failures.flush(true);
            if panicked {
                self.outcome = SpanStatus::Panicked;
            } else if cancelled && self.outcome != SpanStatus::Panicked {
                self.outcome = SpanStatus::Cancelled;
            }
        }
        drop(self.cancelled.take());
        if let Some(span) = self.span.take() {
            span.finish(self.outcome);
        }
        drop(self.ticket.take());
    }
}
impl Drop for OwnedTask {
    fn drop(&mut self) {
        parrot_api::trace::in_scope(self.context.clone(), || self.dispose());
    }
}
impl Future for OwnedTask {
    type Output = anyhow::Result<()>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        parrot_api::trace::in_scope(self.context.clone(), || self.poll_inner(cx))
    }
}
impl OwnedTask {
    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<anyhow::Result<()>> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if self
                .cancelled
                .as_mut()
                .expect("live context task")
                .as_mut()
                .poll(cx)
                .is_ready()
            {
                return Poll::Ready((true, Ok(())));
            }
            self.future
                .as_mut()
                .expect("context task polled after completion")
                .as_mut()
                .poll(cx)
                .map(|result| (false, result))
        }));
        match result {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready((_, Err(error)))) => {
                self.outcome = if matches!(&error, ActorError::Panic(_)) {
                    SpanStatus::Panicked
                } else {
                    SpanStatus::Error
                };
                self.flush_input_failure();
                // Formatting borrows the error; never destroy it during a
                // formatter/subscriber unwind. Retire it in a separate boundary.
                if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    tracing::warn!(%error, "actor context task failed and terminated");
                })) {
                    self.record_panic(payload);
                }
                if let Err(payload) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(error)))
                {
                    self.record_panic(payload);
                }
            }
            Ok(Poll::Ready((cancelled, Ok(())))) => {
                self.outcome = if cancelled {
                    SpanStatus::Cancelled
                } else {
                    SpanStatus::Ok
                };
            }
            Err(payload) => self.record_panic(payload),
        }
        self.dispose();
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod recursive_panic_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Payload;
    impl Drop for Payload {
        fn drop(&mut self) {
            panic!("context task payload destructor");
        }
    }
    struct Probe {
        poll_panic: bool,
        drops: Arc<AtomicUsize>,
    }
    impl Future for Probe {
        type Output = ActorResult<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            if self.poll_panic {
                std::panic::panic_any(Payload);
            }
            Poll::Pending
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            if !self.poll_panic {
                std::panic::panic_any(Payload);
            }
        }
    }
    async fn check(poll_panic: bool, cancel: bool, poll: bool) {
        let owner = ContextTasks::new(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let mut task = Box::pin(
            owner
                .user_task(Probe {
                    poll_panic,
                    drops: drops.clone(),
                })
                .unwrap(),
        );
        if cancel {
            owner.close();
        }
        if poll {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                task.as_mut()
                    .poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
            }))
            .expect("secondary payload panic escaped context task poll");
            assert!(matches!(result, Poll::Ready(Ok(()))), "{result:?}");
        }
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(task))).is_ok(),
            "secondary payload panic escaped context task drop"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        for _ in 0..2 {
            let result = tokio::time::timeout(Duration::from_secs(2), owner.join())
                .await
                .expect("context task ticket never released");
            let ActorError::Panic(message) = result.unwrap_err() else {
                panic!("wrong context task error class");
            };
            assert!(
                message.contains("panic payload destructor panicked"),
                "{message}"
            );
        }
        if !cancel {
            let replacement = owner
                .user_task(async { Ok(()) })
                .expect("task capacity leaked");
            drop(replacement);
        }
    }
    #[tokio::test]
    async fn polling_secondary_panic_keeps_task_receipt_and_capacity() {
        check(true, false, true).await;
    }
    #[tokio::test]
    async fn unpolled_secondary_panic_keeps_task_receipt_and_capacity() {
        check(false, false, false).await;
    }
    #[tokio::test]
    async fn cancellation_secondary_panic_keeps_task_receipt() {
        check(false, true, true).await;
    }
    async fn rejected(closed: bool) {
        let owner = ContextTasks::new(0);
        if closed {
            owner.close();
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.user_task(Probe {
                poll_panic: false,
                drops: drops.clone(),
            })
        }));
        let result = match result {
            Ok(result) => result,
            Err(payload) => {
                let message = crate::owned_payload::panic_message(payload);
                panic!("rejected context task escaped with a destructor panic: {message}");
            }
        };
        let Err(ActorError::Panic(message)) = result else {
            panic!("rejected task lost cleanup error");
        };
        assert!(
            message.contains("panic payload destructor panicked"),
            "{message}"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        owner.join().await.unwrap(); // It was never admitted to the generation.
    }
    #[tokio::test]
    async fn closed_context_rejection_isolates_future_disposal() {
        rejected(true).await;
    }
    #[tokio::test]
    async fn full_context_rejection_isolates_future_disposal() {
        rejected(false).await;
    }
    #[test]
    fn independent_task_poll_and_drop_panics_preserve_first_receipt() {
        const NAME: &str = "context_tasks::recursive_panic_tests::independent_task_poll_and_drop_panics_preserve_first_receipt";
        const FLAG: &str = "PARROT_CONTEXT_DOUBLE_PANIC";
        if std::env::var(FLAG).as_deref() == Ok("1") {
            struct Both;
            impl Future for Both {
                type Output = ActorResult<()>;
                fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                    panic!("primary task poll panic");
                }
            }
            impl Drop for Both {
                fn drop(&mut self) {
                    panic!("independent task future destructor panic");
                }
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let owner = ContextTasks::new(1);
                    owner.user_task(Both).unwrap().await.unwrap();
                    let error = tokio::time::timeout(Duration::from_secs(2), owner.join())
                        .await
                        .unwrap()
                        .unwrap_err()
                        .to_string();
                    assert!(
                        error.contains("primary task poll panic"),
                        "lost first failure: {error}"
                    );
                    drop(
                        owner
                            .user_task(async { Ok(()) })
                            .expect("task capacity stranded"),
                    );
                    assert_eq!(owner.join().await.unwrap_err().to_string(), error);
                });
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env(FLAG, "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "isolated task panics: {status}");
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("isolated task panics: deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use parrot_api::address::ActorRef;
    use parrot_api::types::BoxedFuture;
    use std::any::Any;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Debug)]
    struct Counter(Arc<AtomicUsize>);
    #[async_trait]
    impl ActorRef for Counter {
        fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Err(ActorError::Stopped) })
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(msg)
        }
        fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            let count = self.0.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            "/context-tasks/counter".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(self.clone())
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn scheduled_task_can_be_cancelled_and_removed_before_delivery() {
        let tasks = ContextTasks::new(2);
        let count = Arc::new(AtomicUsize::new(0));
        let (id, task) = tasks
            .once_with_id(
                Box::new(Counter(count.clone())),
                Box::new(()),
                Duration::from_millis(100),
            )
            .unwrap();
        tokio::spawn(task);
        tasks.cancel_schedule(id);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(tasks.state.status.lock().unwrap().scheduled.is_empty());
        tasks.close();
        tasks.join().await.unwrap();
    }
}

#[cfg(test)]
mod schedule_ownership_tests {
    use super::*;
    use parrot_api::{address::ActorRef, types::BoxedFuture};
    use std::{
        any::Any,
        sync::atomic::{AtomicUsize, Ordering},
    };

    #[derive(Clone, Copy, Debug)]
    enum Mode {
        Pending,
        PollPanic,
        BothPanic,
        DropPanic,
        ReadyDropPanic,
        Success,
        Failure,
    }
    #[derive(Debug, Default)]
    struct Counts {
        polls: AtomicUsize,
        deliveries: AtomicUsize,
        drops: AtomicUsize,
        targets: AtomicUsize,
    }
    #[derive(Debug)]
    struct Target {
        mode: Mode,
        counts: Arc<Counts>,
    }
    impl Drop for Target {
        fn drop(&mut self) {
            self.counts.targets.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Delivery {
        mode: Mode,
        counts: Arc<Counts>,
    }
    impl Future for Delivery {
        type Output = ActorResult<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.counts.polls.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                Mode::PollPanic | Mode::BothPanic => {
                    panic!("primary scheduled delivery poll panic")
                }
                Mode::Pending | Mode::DropPanic => Poll::Pending,
                Mode::Success | Mode::ReadyDropPanic => Poll::Ready(Ok(())),
                Mode::Failure => Poll::Ready(Err(ActorError::Stopped)),
            }
        }
    }
    impl Drop for Delivery {
        fn drop(&mut self) {
            self.counts.drops.fetch_add(1, Ordering::SeqCst);
            if matches!(
                self.mode,
                Mode::BothPanic | Mode::DropPanic | Mode::ReadyDropPanic
            ) {
                panic!("scheduled delivery destructor panic");
            }
        }
    }
    impl ActorRef for Target {
        fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            panic!("timer used ask")
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(msg)
        }
        fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            self.counts.deliveries.fetch_add(1, Ordering::SeqCst);
            Box::pin(Delivery {
                mode: self.mode,
                counts: self.counts.clone(),
            })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            "/schedule-ownership".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(Self {
                mode: self.mode,
                counts: self.counts.clone(),
            })
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    fn schedule(
        owner: &ContextTasks,
        periodic: bool,
        mode: Mode,
        delay: Duration,
    ) -> (Uuid, OwnedTask, Arc<Counts>) {
        let counts = Arc::new(Counts::default());
        let target = Box::new(Target {
            mode,
            counts: counts.clone(),
        });
        let (id, task) = if periodic {
            owner.periodic_with_id(
                target,
                CloneableMessage::from_cloneable(()),
                delay,
                Duration::from_secs(10),
            )
        } else {
            owner.once_with_id(target, Box::new(()), delay)
        }
        .unwrap();
        (id, task, counts)
    }
    fn poll(task: &mut Pin<Box<OwnedTask>>) -> Poll<anyhow::Result<()>> {
        task.as_mut()
            .poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
    }
    async fn retired(
        owner: &ContextTasks,
        counts: &Counts,
        deliveries: usize,
        panic: Option<&str>,
    ) {
        assert_eq!(counts.deliveries.load(Ordering::SeqCst), deliveries);
        assert_eq!(
            counts.drops.load(Ordering::SeqCst),
            deliveries,
            "delivery storage not retired exactly once"
        );
        assert_eq!(
            counts.targets.load(Ordering::SeqCst),
            1,
            "target storage not retired before receipt"
        );
        assert!(
            owner.state.status.lock().unwrap().scheduled.is_empty(),
            "dead schedule id retained"
        );
        for _ in 0..2 {
            let result = tokio::time::timeout(Duration::from_secs(1), owner.join())
                .await
                .expect("stranded task ticket");
            if let Some(expected) = panic {
                let Err(ActorError::Panic(message)) = result else {
                    panic!("lost panic receipt: {result:?}")
                };
                assert!(message.contains(expected), "wrong first failure: {message}");
            } else {
                result.unwrap();
            }
            drop(
                owner
                    .user_task(async { Ok(()) })
                    .expect("capacity not reusable"),
            );
        }
    }
    async fn abandon(periodic: bool, suspended: bool) {
        let owner = ContextTasks::new(1);
        let (_, task, counts) = schedule(&owner, periodic, Mode::Pending, Duration::from_secs(10));
        let mut task = Box::pin(task);
        if suspended {
            assert!(poll(&mut task).is_pending());
        }
        drop(task);
        retired(&owner, &counts, 0, None).await;
    }
    #[tokio::test]
    async fn once_unpolled_abandon_removes_registration() {
        abandon(false, false).await;
    }
    #[tokio::test]
    async fn periodic_unpolled_abandon_removes_registration() {
        abandon(true, false).await;
    }
    #[tokio::test]
    async fn once_suspended_abandon_removes_registration() {
        abandon(false, true).await;
    }
    #[tokio::test]
    async fn periodic_suspended_abandon_removes_registration() {
        abandon(true, true).await;
    }

    async fn delivery_failure(periodic: bool, mode: Mode) {
        let owner = ContextTasks::new(1);
        let (_, task, counts) = schedule(&owner, periodic, mode, Duration::ZERO);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("timer did not terminate")
            .unwrap();
        retired(
            &owner,
            &counts,
            1,
            Some("primary scheduled delivery poll panic"),
        )
        .await;
    }
    #[tokio::test]
    async fn once_poll_panic_retires_registration() {
        delivery_failure(false, Mode::PollPanic).await;
    }
    #[tokio::test]
    async fn periodic_poll_panic_retires_registration() {
        delivery_failure(true, Mode::PollPanic).await;
    }
    fn isolated(periodic: bool, name: &str) {
        const FLAG: &str = "PARROT_SCHEDULE_DOUBLE_PANIC";
        if std::env::var(FLAG).as_deref() == Ok(name) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(delivery_failure(periodic, Mode::BothPanic));
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(FLAG, name)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "isolated schedule: {status}");
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("isolated schedule timeout");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn once_independent_delivery_panics_keep_first_receipt() {
        isolated(
            false,
            "context_tasks::schedule_ownership_tests::once_independent_delivery_panics_keep_first_receipt",
        );
    }
    #[test]
    fn periodic_independent_delivery_panics_keep_first_receipt() {
        isolated(
            true,
            "context_tasks::schedule_ownership_tests::periodic_independent_delivery_panics_keep_first_receipt",
        );
    }

    async fn cancel_delivery(periodic: bool) {
        let owner = ContextTasks::new(1);
        let (id, task, counts) = schedule(&owner, periodic, Mode::DropPanic, Duration::ZERO);
        let mut task = Box::pin(task);
        tokio::time::timeout(Duration::from_secs(2), async {
            while counts.polls.load(Ordering::SeqCst) == 0 {
                assert!(poll(&mut task).is_pending());
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        owner.cancel_schedule(id);
        assert!(matches!(poll(&mut task), Poll::Ready(Ok(()))));
        drop(task);
        retired(
            &owner,
            &counts,
            1,
            Some("scheduled delivery destructor panic"),
        )
        .await;
    }
    #[tokio::test]
    async fn once_cancel_pending_delivery_retires_registration() {
        cancel_delivery(false).await;
    }
    #[tokio::test]
    async fn periodic_cancel_pending_delivery_retires_registration() {
        cancel_delivery(true).await;
    }

    #[tokio::test]
    async fn cancellation_wins_before_delivery_construction() {
        for periodic in [false, true] {
            let owner = ContextTasks::new(1);
            let (id, task, counts) = schedule(&owner, periodic, Mode::BothPanic, Duration::ZERO);
            owner.cancel_schedule(id);
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap();
            retired(&owner, &counts, 0, None).await;
        }
    }
    #[tokio::test]
    async fn once_success_and_delivery_failure_terminate_and_retire() {
        for periodic in [false, true] {
            for mode in [Mode::Success, Mode::Failure] {
                if periodic && matches!(mode, Mode::Success) {
                    continue;
                }
                let owner = ContextTasks::new(1);
                let (_, task, counts) = schedule(&owner, periodic, mode, Duration::ZERO);
                tokio::time::timeout(Duration::from_secs(2), task)
                    .await
                    .unwrap()
                    .unwrap();
                retired(&owner, &counts, 1, None).await;
            }
        }
    }
    async fn ready_drop_panic(periodic: bool) {
        let owner = ContextTasks::new(1);
        let (_, task, counts) = schedule(&owner, periodic, Mode::ReadyDropPanic, Duration::ZERO);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("failed disposal did not terminate timer")
            .unwrap();
        retired(
            &owner,
            &counts,
            1,
            Some("scheduled delivery destructor panic"),
        )
        .await;
    }
    #[tokio::test]
    async fn once_ready_delivery_drop_failure_retires() {
        ready_drop_panic(false).await;
    }
    #[tokio::test]
    async fn periodic_ready_delivery_drop_failure_terminates() {
        ready_drop_panic(true).await;
    }
    #[tokio::test]
    async fn abandonment_during_delivery_preserves_cleanup_receipt() {
        for periodic in [false, true] {
            let owner = ContextTasks::new(1);
            let (_, task, counts) = schedule(&owner, periodic, Mode::DropPanic, Duration::ZERO);
            let mut task = Box::pin(task);
            tokio::time::timeout(Duration::from_secs(1), async {
                while counts.polls.load(Ordering::SeqCst) == 0 {
                    assert!(poll(&mut task).is_pending());
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            drop(task);
            retired(
                &owner,
                &counts,
                1,
                Some("scheduled delivery destructor panic"),
            )
            .await;
        }
    }
    #[tokio::test(start_paused = true)]
    async fn periodic_clones_only_for_due_delivery_without_catchup() {
        struct Message {
            clones: Arc<AtomicUsize>,
            drops: Arc<AtomicUsize>,
        }
        impl Clone for Message {
            fn clone(&self) -> Self {
                self.clones.fetch_add(1, Ordering::SeqCst);
                Self {
                    clones: self.clones.clone(),
                    drops: self.drops.clone(),
                }
            }
        }
        impl Drop for Message {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }
        let owner = ContextTasks::new(1);
        let clones = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let counts = Arc::new(Counts::default());
        let (id, task) = owner
            .periodic_with_id(
                Box::new(Target {
                    mode: Mode::Success,
                    counts: counts.clone(),
                }),
                CloneableMessage::from_cloneable(Message {
                    clones: clones.clone(),
                    drops: drops.clone(),
                }),
                Duration::from_secs(5),
                Duration::from_secs(10),
            )
            .unwrap();
        let mut task = Box::pin(task);
        assert!(poll(&mut task).is_pending());
        assert_eq!(clones.load(Ordering::SeqCst), 0);
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(poll(&mut task).is_pending());
        assert_eq!(counts.deliveries.load(Ordering::SeqCst), 1);
        assert_eq!(clones.load(Ordering::SeqCst), 1);
        // Missing many nominal intervals must produce only one next delivery.
        tokio::time::advance(Duration::from_secs(100)).await;
        assert!(poll(&mut task).is_pending());
        assert_eq!(counts.deliveries.load(Ordering::SeqCst), 2);
        assert_eq!(clones.load(Ordering::SeqCst), 2);
        assert!(poll(&mut task).is_pending());
        assert_eq!(counts.deliveries.load(Ordering::SeqCst), 2);
        owner.cancel_schedule(id);
        assert!(matches!(poll(&mut task), Poll::Ready(Ok(()))));
        drop(task);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            3,
            "two payload clones plus original"
        );
        retired(&owner, &counts, 2, None).await;
    }
}

#[cfg(test)]
mod schedule_input_tests {
    use super::*;
    use parrot_api::address::ActorRef;
    use std::{
        any::Any,
        sync::atomic::{AtomicUsize, Ordering},
    };
    #[derive(Debug, Default)]
    struct Counts {
        messages: AtomicUsize,
        targets: AtomicUsize,
        clones: AtomicUsize,
        errors: Arc<AtomicUsize>,
    }
    struct Payload {
        counts: Arc<Counts>,
        panic_drop: bool,
        panic_clone: bool,
    }
    impl Clone for Payload {
        fn clone(&self) -> Self {
            self.counts.clones.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic_clone, "primary schedule clone panic");
            Self {
                counts: self.counts.clone(),
                panic_drop: self.panic_drop,
                panic_clone: self.panic_clone,
            }
        }
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            self.counts.messages.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic_drop, "schedule message destructor panic");
        }
    }
    #[derive(Debug)]
    struct Target {
        counts: Arc<Counts>,
        panic_drop: bool,
        panic_construct: bool,
        return_error: bool,
    }
    impl Drop for Target {
        fn drop(&mut self) {
            self.counts.targets.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic_drop, "schedule target destructor panic");
        }
    }
    impl ActorRef for Target {
        fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            panic!("unexpected ask")
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(msg)
        }
        fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            drop(msg);
            assert!(
                !self.panic_construct,
                "primary schedule delivery construction panic"
            );
            if self.return_error {
                let error = Error {
                    format: true,
                    destructor: true,
                    drops: self.counts.errors.clone(),
                };
                Box::pin(async move { Err(ActorError::Other(anyhow::Error::new(error))) })
            } else {
                Box::pin(async { Ok(()) })
            }
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            "/input-probe".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            panic!("unexpected target clone")
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    async fn input(case: &str) {
        let periodic = case.starts_with("periodic");
        let rejected = case.contains("invalid")
            || case.contains("zero")
            || case.contains("closed")
            || case.contains("full");
        let owner = ContextTasks::new(usize::from(!case.contains("full")));
        if case.contains("closed") {
            owner.close();
        }
        let counts = Arc::new(Counts::default());
        let target = Box::new(Target {
            counts: counts.clone(),
            panic_drop: true,
            panic_construct: case.contains("construct"),
            return_error: case.contains("error_after"),
        });
        let msg = Payload {
            counts: counts.clone(),
            panic_drop: !case.contains("construct") && !case.contains("error_after"),
            panic_clone: case.contains("clone"),
        };
        let delay = if case.contains("invalid_delay") {
            Duration::MAX
        } else if case.contains("clone")
            || case.contains("construct")
            || case.contains("error_after")
        {
            Duration::ZERO
        } else {
            Duration::from_secs(3600)
        };
        let interval = if case.contains("zero") {
            Duration::ZERO
        } else if case.contains("invalid_interval") {
            Duration::MAX
        } else {
            Duration::from_secs(1)
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if periodic {
                owner.periodic_with_id(
                    target,
                    CloneableMessage::from_cloneable(msg),
                    delay,
                    interval,
                )
            } else {
                owner.once_with_id(target, Box::new(msg), delay)
            }
        }))
        .unwrap_or_else(|p| {
            panic!(
                "registration escaped cleanup boundary: {}",
                crate::owned_payload::panic_message(p)
            )
        });
        if rejected {
            assert!(
                matches!(result, Err(ActorError::Panic(_))),
                "rejection lost input cleanup failure"
            );
        } else {
            let (id, task) = result.unwrap();
            if case.contains("cancel") {
                owner.cancel_schedule(id);
                task.await.unwrap();
            } else if case.contains("clone")
                || case.contains("construct")
                || case.contains("error_after")
            {
                tokio::time::timeout(Duration::from_secs(2), task)
                    .await
                    .unwrap()
                    .unwrap();
            } else {
                drop(task);
            }
        }
        assert_eq!(counts.targets.load(Ordering::SeqCst), 1);
        assert_eq!(counts.messages.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.clones.load(Ordering::SeqCst),
            usize::from(case.contains("clone"))
        );
        assert!(owner.state.status.lock().unwrap().scheduled.is_empty());
        for _ in 0..2 {
            let result = tokio::time::timeout(Duration::from_secs(1), owner.join())
                .await
                .unwrap();
            if rejected {
                result.expect("never-admitted input poisoned generation join");
            } else {
                let Err(ActorError::Panic(message)) = result else {
                    panic!("lost admitted cleanup receipt")
                };
                if case.contains("clone") {
                    assert!(
                        message.contains("primary schedule clone panic"),
                        "{message}"
                    );
                } else if case.contains("construct") {
                    assert!(
                        message.contains("primary schedule delivery construction panic"),
                        "{message}"
                    );
                } else if case.contains("error_after") {
                    assert!(
                        message.contains("schedule target destructor panic"),
                        "earlier input cleanup failure was overwritten: {message}"
                    );
                    assert_eq!(counts.errors.load(Ordering::SeqCst), 1);
                } else {
                    assert!(message.contains("destructor panic"), "{message}");
                }
                drop(owner.user_task(async { Ok(()) }).expect("capacity leaked"));
            }
        }
    }
    #[derive(Debug)]
    struct Error {
        format: bool,
        destructor: bool,
        drops: Arc<AtomicUsize>,
    }
    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            assert!(!self.format, "primary task error formatting panic");
            write!(f, "ordinary task error")
        }
    }
    impl std::error::Error for Error {}
    impl Drop for Error {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            assert!(!self.destructor, "task error destructor panic");
        }
    }
    async fn error(case: &str) {
        let owner = ContextTasks::new(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let value = Error {
            format: case.contains("format"),
            destructor: case.contains("drop"),
            drops: drops.clone(),
        };
        let task = owner
            .user_task(async move { Err(ActorError::Other(anyhow::Error::new(value))) })
            .unwrap();
        task.await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        if case.contains("ordinary") || case.contains("disabled") {
            owner.join().await.unwrap();
            drop(owner.user_task(async { Ok(()) }).unwrap());
            owner.join().await.unwrap();
            return;
        }
        let first = owner.join().await.unwrap_err().to_string();
        assert!(
            first.contains(if case.contains("format") {
                "primary task error formatting panic"
            } else {
                "task error destructor panic"
            }),
            "{first}"
        );
        drop(owner.user_task(async { Ok(()) }).unwrap());
        assert_eq!(owner.join().await.unwrap_err().to_string(), first);
    }
    fn isolated(case: &str) {
        let name = format!("context_tasks::schedule_input_tests::{case}");
        const FLAG: &str = "PARROT_SCHEDULE_INPUT_TEST";
        if std::env::var(FLAG).as_deref() == Ok(case) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            if case.starts_with("error") {
                if case.contains("disabled") {
                    tracing::subscriber::with_default(
                        tracing::subscriber::NoSubscriber::default(),
                        || runtime.block_on(error(case)),
                    );
                } else if case.contains("format") {
                    let subscriber = tracing_subscriber::fmt()
                        .with_max_level(tracing::Level::WARN)
                        .with_writer(std::io::sink)
                        .finish();
                    tracing::subscriber::with_default(subscriber, || runtime.block_on(error(case)));
                } else {
                    runtime.block_on(error(case));
                }
            } else if case.contains("error_after") {
                let subscriber = tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::WARN)
                    .with_writer(std::io::sink)
                    .finish();
                tracing::subscriber::with_default(subscriber, || runtime.block_on(input(case)));
            } else {
                runtime.block_on(input(case));
            }
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env(FLAG, case)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{case}: {status}");
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{case}: deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    macro_rules! case {
        ($name:ident) => {
            #[test]
            fn $name() {
                isolated(stringify!($name));
            }
        };
    }
    case!(once_invalid_delay);
    case!(periodic_invalid_delay);
    case!(periodic_invalid_interval);
    case!(periodic_zero_interval);
    case!(once_closed);
    case!(periodic_closed);
    case!(once_full);
    case!(periodic_full);
    case!(once_abandon);
    case!(periodic_abandon);
    case!(once_cancel);
    case!(periodic_cancel);
    case!(periodic_clone_failure);
    case!(once_construct_failure);
    case!(once_error_after_input_cleanup);
    case!(error_format);
    case!(error_drop);
    case!(error_format_and_drop);
    case!(error_ordinary);
    case!(error_disabled_format);

    #[tokio::test]
    async fn ordinary_rejection_preserves_error_and_retires_inputs() {
        for case in ["closed", "full", "zero", "delay", "interval"] {
            let owner = ContextTasks::new(usize::from(case != "full"));
            if case == "closed" {
                owner.close();
            }
            let counts = Arc::new(Counts::default());
            let target = Box::new(Target {
                counts: counts.clone(),
                panic_drop: false,
                panic_construct: false,
                return_error: false,
            });
            let message = CloneableMessage::from_cloneable(Payload {
                counts: counts.clone(),
                panic_drop: false,
                panic_clone: false,
            });
            let result = owner.periodic_with_id(
                target,
                message,
                if case == "delay" {
                    Duration::MAX
                } else {
                    Duration::from_secs(3600)
                },
                if case == "zero" {
                    Duration::ZERO
                } else if case == "interval" {
                    Duration::MAX
                } else {
                    Duration::from_secs(1)
                },
            );
            let error = match result {
                Ok(_) => panic!("invalid schedule accepted"),
                Err(error) => error,
            };
            match case {
                "closed" => assert!(matches!(error, ActorError::Stopped)),
                "full" => assert!(
                    matches!(error, ActorError::InternalError(ref s) if s.contains("task limit"))
                ),
                "zero" => assert!(
                    matches!(error, ActorError::InternalError(ref s) if s.contains("positive"))
                ),
                _ => assert!(
                    matches!(error, ActorError::InternalError(ref s) if s.contains("clock range"))
                ),
            }
            assert_eq!(counts.targets.load(Ordering::SeqCst), 1);
            assert_eq!(counts.messages.load(Ordering::SeqCst), 1);
            assert!(owner.state.status.lock().unwrap().scheduled.is_empty());
            owner.join().await.unwrap();
        }
    }
}

#[cfg(test)]
mod trace_owner_tests {
    use super::*;
    use parrot_api::trace::{self, TraceContext};
    use std::sync::Mutex;
    type Events = Arc<Mutex<Vec<(&'static str, Option<TraceContext>)>>>;
    struct Probe {
        events: Events,
        mode: u8,
    }
    impl Future for Probe {
        type Output = ActorResult<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.events
                .lock()
                .unwrap()
                .push(("poll", TraceContext::current()));
            match self.mode {
                1 => Poll::Ready(Ok(())),
                2 => panic!("trace task poll failure"),
                _ => Poll::Pending,
            }
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            self.events
                .lock()
                .unwrap()
                .push(("drop", TraceContext::current()));
            if self.mode == 3 {
                panic!("trace task disposal failure");
            }
        }
    }
    fn check(mode: u8, cancel: bool, unpolled: bool, idle: bool) {
        for traced in [true, false] {
            let owner = ContextTasks::new(1);
            let parent = traced.then(TraceContext::root);
            let foreign = TraceContext::root();
            let events: Events = Arc::new(Mutex::new(Vec::new()));
            let mut task = Box::pin(trace::in_scope(parent.clone(), || {
                let probe = Probe {
                    events: events.clone(),
                    mode,
                };
                if idle {
                    owner.idle_worker(probe)
                } else {
                    owner.user_task(probe)
                }
                .unwrap()
            }));
            if cancel {
                owner.close();
            }
            foreign.in_scope(|| {
                if !unpolled {
                    let result = task
                        .as_mut()
                        .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));
                    assert_eq!(result.is_ready(), cancel || mode == 1 || mode == 2);
                    assert_eq!(TraceContext::current(), Some(foreign.clone()));
                }
                drop(task);
                assert_eq!(TraceContext::current(), Some(foreign.clone()));
            });
            assert!(TraceContext::current().is_none());
            let result = futures::executor::block_on(owner.join());
            assert_eq!(result.is_err(), mode == 2 || mode == 3);
            let events = events.lock().unwrap();
            let kinds: Vec<_> = events.iter().map(|e| e.0).collect();
            assert_eq!(
                kinds,
                if unpolled || cancel {
                    vec!["drop"]
                } else {
                    vec!["poll", "drop"]
                }
            );
            for (_, observed) in events.iter() {
                assert_eq!(*observed, if idle { None } else { parent.clone() });
            }
        }
    }
    #[test]
    fn pending_poll_and_drop_restore_owner_context() {
        check(0, false, false, false);
    }
    #[test]
    fn successful_poll_and_drop_restore_owner_context() {
        check(1, false, false, false);
    }
    #[test]
    fn panicking_poll_and_drop_restore_owner_context() {
        check(2, false, false, false);
    }
    #[test]
    fn panicking_drop_restores_owner_context() {
        check(3, false, false, false);
    }
    #[test]
    fn cancelled_task_cleanup_restores_owner_context() {
        check(0, true, false, false);
    }
    #[test]
    fn unpolled_task_cleanup_restores_owner_context() {
        check(0, false, true, false);
    }
    #[test]
    fn idle_worker_explicitly_clears_business_context() {
        check(0, false, false, true);
    }
}

#[cfg(test)]
mod recording_contracts;
