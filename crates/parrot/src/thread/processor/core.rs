mod execution;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tracing::{debug, error};

use parrot_api::actor::Actor;
use parrot_api::errors::ActorError;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};

use crate::thread::actor::ThreadActor;
use crate::thread::config::ThreadActorConfig;
use crate::thread::context::ThreadContext;
use crate::thread::envelope::ControlMessage;
use crate::thread::error::SystemError;
use crate::thread::mailbox::Mailbox;

use std::any::Any;
use std::panic;
use std::panic::AssertUnwindSafe;
use tokio::sync::Mutex as AsyncMutex;

mod batch;

#[derive(Debug)]
struct RecordedFailure {
    error: Arc<ActorError>,
    context: Option<parrot_api::trace::TraceContext>,
}

/// Processor status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessorStatus {
    /// Processor is initializing
    Initializing = 0,

    /// Processor is running normally
    Running = 1,

    /// Processor is paused
    Paused = 2,

    /// Processor is stopping
    Stopping = 3,

    /// Processor is stopped
    Stopped = 4,

    /// Processor has failed
    Failed = 5,
}

impl ProcessorStatus {
    pub fn from_usize(v: usize) -> Self {
        match v {
            0 => ProcessorStatus::Initializing,
            1 => ProcessorStatus::Running,
            2 => ProcessorStatus::Paused,
            3 => ProcessorStatus::Stopping,
            4 => ProcessorStatus::Stopped,
            _ => ProcessorStatus::Failed,
        }
    }
}

/// Worker stats collection
#[derive(Debug, Default)]
pub struct ProcessorStats {
    /// Number of messages processed
    pub messages_processed: AtomicUsize,

    /// Number of errors encountered
    pub errors_encountered: AtomicUsize,

    /// Time spent processing messages (nanoseconds)
    pub processing_time_ns: AtomicUsize,
}

impl ProcessorStats {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Processor stats trait
pub trait ProcessorStatsTrait {
    fn get_statistics(&self) -> Option<Arc<ProcessorStats>>;
}

/// Object-safe processor interface.
///
/// Provides type-erased operations so shared-pool workers (which only hold
/// `Arc<dyn Mailbox>`) can drive an actor without knowing its concrete type `A`.
///
/// The `self: Arc<Self>` receivers make the trait object-safe while allowing
/// the returned futures to own a reference to the processor.
pub trait ProcessorInterface: ProcessorStatsTrait + Any + Send + Sync + 'static {
    /// Whether the actor has completed initialization (Running or later).
    fn is_initialized(&self) -> bool;

    /// Initialize the actor and transition it to Running state.
    fn initialize_and_start_erased(
        self: Arc<Self>,
    ) -> BoxedProcessorFuture<Result<(), SystemError>>;

    /// A non-resumable execution failure makes the normal stop hook inapplicable.
    /// This receipt remains true after storage retirement.
    fn has_failed(&self) -> bool {
        false
    }

    /// Stop the actor gracefully.
    fn stop_erased(self: Arc<Self>) -> BoxedProcessorFuture<Result<(), SystemError>>;

    /// Release actor and context storage on its execution scheduler. Engines
    /// must not infer disposal from a stop deadline or a dropped mailbox Arc.
    fn retire_erased(self: Arc<Self>) -> BoxedProcessorFuture<Result<(), SystemError>> {
        Box::pin(async {
            Err(SystemError::ShutdownError(
                "processor has no disposal implementation".into(),
            ))
        })
    }
    fn retirement(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async {
            Err(SystemError::ShutdownError(
                "processor has no disposal receipt".into(),
            ))
        })
    }

    /// Process up to `max_messages` messages from the mailbox.
    /// Returns (processed, errors). A returned `Err` means the actor panicked.
    fn process_batch_erased(
        self: Arc<Self>,
        mailbox: Arc<dyn Mailbox>,
        max_messages: usize,
        yield_after_each_message: bool,
    ) -> BoxedProcessorFuture<Result<(usize, usize), SystemError>>;

    fn as_any(self: Arc<Self>) -> Arc<dyn Any>;
    fn as_any_ref(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Pause a generation for its first pending descendant escalation.
    fn begin_escalation(&self, _error: Arc<ActorError>) -> bool {
        false
    }
    /// Continue the same generation after its live escalation decision.
    fn resume_after_escalation(&self, _error: &Arc<ActorError>) {}
    /// A non-resumable escalation makes the normal stop hook inapplicable.
    fn fail_from_escalation(&self, _error: Arc<ActorError>) {}

    /// Cancel suspended execution without cancelling unrelated actors.
    fn cancel_processing(&self) {}

    /// Stop actor-owned timers independently of the actor/context lock.
    fn cancel_context_tasks(&self) {}
    fn join_context_tasks(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Resolves once cancellation has been requested. Implementations that
    /// cannot be cancelled retain the default pending future.
    fn processing_cancelled(&self) -> parrot_api::types::BoxedFuture<'_, ()> {
        Box::pin(std::future::pending())
    }

    /// M2 reduction accounting: consecutive messages processed since the
    /// last empty-mailbox drain. Workers consult this for fairness yields.
    fn consecutive_reductions(&self) -> usize {
        0
    }

    /// M2: reset the reduction budget after a fairness yield (BEAM
    /// semantics: yield once, then full budget again).
    fn reset_reduction_budget(&self) {}

    /// M3 supervision: notify the owning system this actor panicked
    /// (detached; never blocks the worker loop). Default: no-op.
    fn notify_system_panic(&self, _panic_msg: String) {}
}

/// Boxed future used by [`ProcessorInterface`].
pub type BoxedProcessorFuture<T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'static>>;

/// Result type for actor execution: either unit or a boxed reply payload.
#[allow(dead_code)] // M2 reduction 记账重构后保留形状
pub type ActorExecutionResult = ActorResult<BoxedMessage>;

/// Actor processor, responsible for managing an Actor's resources and message processing.
///
/// The processor owns the actor instance and its context, guarded by async mutexes so
/// futures can be awaited while holding the guards (no `std` guard-across-await hazards).
#[derive(Debug)]
pub struct ActorProcessor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    /// Actor instance (async mutex: guards must be held across `.await`)
    actor: AsyncMutex<Option<ThreadActor<A>>>,

    /// Actor context (async mutex: guards must be held across `.await`)
    context: AsyncMutex<Option<ThreadContext<A>>>,

    /// Only initialization inherits publication's caller. Take this owner on
    /// first execution, or retire it if initialization is never dispatched.
    initialization_trace: std::sync::Mutex<Option<parrot_api::trace::TraceContext>>,

    /// Actor path (plain string copy, avoids borrow of self-referential ActorPath)
    path: String,

    /// Captured independently of the context lock; failure callbacks must
    /// identify their original generation even after a replacement exists.
    system: crate::thread::context::WeakSystemRef,
    mailbox: Option<crate::thread::mailbox::WeakMailboxRef>,
    tasks: Arc<crate::context_tasks::ContextTasks>,

    /// Actor config
    config: ThreadActorConfig,

    /// Processor status
    status: Arc<AtomicUsize>,

    /// Processor stats
    stats: Arc<ProcessorStats>,

    /// M2 reduction accounting: messages processed since this actor last
    /// yielded the worker *without* an empty mailbox (i.e. consecutive
    /// monopolization count). Used to force a fairness yield so one slow
    /// actor with a permanent backlog cannot starve others.
    consecutive_reductions: AtomicUsize,
    failure_cause: std::sync::Mutex<Option<RecordedFailure>>,
    escalation: crate::escalation_pause::EscalationPause,
    cancelled: AtomicBool,
    cancel_notify: tokio::sync::Notify,
    executions: Arc<execution::Executions>,
    children: Arc<crate::children::ChildScope>,
    group_activation: Option<Arc<crate::children::recovery::Recovery>>,
    retired: tokio::sync::watch::Sender<Option<crate::thread::system::StopOutcome>>,
}

impl<A> ActorProcessor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    /// Create a new Actor processor
    pub fn new(
        actor: ThreadActor<A>,
        context: ThreadContext<A>,
        path: String,
        config: ThreadActorConfig,
    ) -> Self {
        Self {
            system: context.system_weak_ref(),
            mailbox: context.generation_mailbox(),
            tasks: context.tasks(),
            children: context.owned_children.clone(),
            actor: AsyncMutex::new(Some(actor)),
            context: AsyncMutex::new(Some(context)),
            initialization_trace: std::sync::Mutex::new(parrot_api::trace::TraceContext::current()),
            path,
            config,
            status: Arc::new(AtomicUsize::new(ProcessorStatus::Initializing as usize)),
            stats: Arc::new(ProcessorStats::new()),
            consecutive_reductions: AtomicUsize::new(0),
            failure_cause: std::sync::Mutex::new(None),
            escalation: Default::default(),
            cancelled: AtomicBool::new(false),
            cancel_notify: tokio::sync::Notify::new(),
            retired: tokio::sync::watch::channel(None).0,
            executions: Arc::default(),
            group_activation: None,
        }
    }

    pub(crate) fn with_group_activation(
        mut self,
        gate: Option<Arc<crate::children::recovery::Recovery>>,
    ) -> Self {
        self.group_activation = gate;
        self
    }

    /// A rejected scheduler admission has never dispatched this processor.
    /// Its publication owner runs on a bounded blocking executor, outside the
    /// registry lock; no scheduler or lifecycle hook is needed to retire it.
    pub(crate) fn retire_unpublished(&self) -> ActorResult<()> {
        self.initialization_trace.lock().unwrap().take();
        let actor = self
            .actor
            .try_lock()
            .expect("unpublished actor is not executing")
            .take();
        let context = self
            .context
            .try_lock()
            .expect("unpublished context is not executing")
            .take();
        let actor = crate::owned_payload::PublicationInput::new(actor).retire();
        let context = crate::owned_payload::PublicationInput::new(context).retire();
        actor.and(context)
    }

    /// Initialize the actor (calls `Actor::init`)
    pub async fn initialize_actor(&self) -> Result<(), SystemError> {
        let trace = self.initialization_trace.lock().unwrap().take();
        parrot_api::trace::scope(trace, async {
            debug!("Initializing actor at {}", self.path);
            let mut actor_slot = self.actor.lock().await;
            let mut context_slot = self.context.lock().await;
            let actor = actor_slot
                .as_mut()
                .ok_or_else(|| SystemError::ActorNotFound(self.path.clone()))?;
            let context = context_slot
                .as_mut()
                .ok_or_else(|| SystemError::ActorNotFound(self.path.clone()))?;
            use futures::FutureExt;
            let initialized = AssertUnwindSafe(async { actor.initialize(context).await })
                .catch_unwind()
                .await
                .unwrap_or_else(|payload| {
                    Err(ActorError::Panic(panic_message(payload, &self.path)))
                });
            if let Err(e) = initialized {
                error!(
                    "Failed to initialize actor at {}: {}",
                    self.path,
                    crate::error_receipt::actor_summary(&e)
                );
                self.status
                    .store(ProcessorStatus::Failed as usize, Ordering::SeqCst);
                self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
                let message = format!(
                    "Failed to initialize actor at {}: {}",
                    self.path,
                    crate::error_receipt::actor_summary(&e)
                );
                self.record_failure(Arc::new(e));
                return Err(SystemError::ActorCreationError(message));
            }
            Ok(())
        })
        .await
    }

    /// Send Start control message to the actor (Starting -> Running)
    pub async fn start_actor(&self) -> Result<(), SystemError> {
        debug!("Sending Start message to actor at {}", self.path);
        let start_msg = Box::new(ControlMessage::Start);
        let mut actor_slot = self.actor.lock().await;
        let mut context_slot = self.context.lock().await;
        let actor = actor_slot
            .as_mut()
            .ok_or_else(|| SystemError::ActorNotFound(self.path.clone()))?;
        let context = context_slot
            .as_mut()
            .ok_or_else(|| SystemError::ActorNotFound(self.path.clone()))?;
        if let Err(e) = actor.process_message(start_msg, context).await {
            error!("Failed to start actor at {}: {}", self.path, e);
            self.status
                .store(ProcessorStatus::Failed as usize, Ordering::SeqCst);
            self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
            return Err(SystemError::ActorCreationError(format!(
                "Failed to start actor at {}: {}",
                self.path, e
            )));
        }
        self.status
            .store(ProcessorStatus::Running as usize, Ordering::SeqCst);
        Ok(())
    }

    /// Send Stop control message to the actor (graceful shutdown)
    pub async fn stop_actor(&self) -> Result<(), SystemError> {
        debug!("Sending Stop message to actor at {}", self.path);
        self.status
            .store(ProcessorStatus::Stopping as usize, Ordering::SeqCst);

        self.children.close();
        self.tasks.close();
        let children_result = self.children.join().await;
        let tasks_result = self.tasks.join().await.and(children_result);
        let stop_msg = Box::new(ControlMessage::Stop);
        let mut actor_slot = self.actor.lock().await;
        // A cohort stop may have been reserved before an in-flight handler's
        // decision became final. Check under the same lock as that handler:
        // never re-enter a failed instance through its normal stop hook.
        if self.message_failed() {
            return tasks_result.map_err(|error| SystemError::Other(anyhow::Error::new(error)));
        }
        let mut context_slot = self.context.lock().await;
        let actor = actor_slot
            .as_mut()
            .ok_or_else(|| SystemError::ActorNotFound(self.path.clone()))?;
        let context = context_slot
            .as_mut()
            .ok_or_else(|| SystemError::ActorNotFound(self.path.clone()))?;
        if let Err(e) = actor.process_message(stop_msg, context).await {
            error!(
                "Failed to stop actor at {}: {}",
                self.path,
                crate::error_receipt::actor_summary(&e)
            );
            self.status
                .store(ProcessorStatus::Failed as usize, Ordering::SeqCst);
            self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
            return Err(SystemError::Other(anyhow::Error::new(e)));
        }
        self.status
            .store(ProcessorStatus::Stopped as usize, Ordering::SeqCst);
        tasks_result.map_err(|error| SystemError::Other(anyhow::Error::new(error)))
    }

    /// Dispose user storage on the assigned scheduler, independently of the
    /// last processor/mailbox Arc. The receipt follows both destructors.
    async fn retire_storage(&self) -> Result<(), SystemError> {
        self.initialization_trace.lock().unwrap().take();
        self.cancel_processing();
        let children = self
            .children
            .join()
            .await
            .map_err(|e| SystemError::Other(anyhow::Error::new(e)));
        let executions = self.executions.join().await;
        let tasks = self.tasks.join().await.map_err(|error| match error {
            ActorError::Panic(message) => SystemError::Panic(message),
            error => SystemError::Other(anyhow::Error::new(error)),
        });
        let mut actor = self.actor.lock().await;
        if self.retired.borrow().is_some() {
            return crate::thread::system::await_stop(self.retired.subscribe()).await;
        }
        let mut context = self.context.lock().await;
        // Keep the per-actor guards until publication so another retirement
        // cannot observe empty slots and report completion before Drop returns.
        let actor_drop = panic::catch_unwind(AssertUnwindSafe(|| drop(actor.take())))
            .map_err(|error| SystemError::Panic(panic_message(error, &self.path)));
        let context_drop = panic::catch_unwind(AssertUnwindSafe(|| drop(context.take())))
            .map_err(|error| SystemError::Panic(panic_message(error, &self.path)));
        self.status
            .store(ProcessorStatus::Stopped as usize, Ordering::SeqCst);
        let outcome = crate::thread::system::StopOutcome::from_result(
            actor_drop
                .and(context_drop)
                .and(executions)
                .and(tasks)
                .and(children),
        );
        self.retired.send_replace(Some(outcome));
        crate::thread::system::await_stop(self.retired.subscribe()).await
    }

    /// Initialize and start the actor
    pub async fn initialize_and_start(&self) -> Result<(), SystemError> {
        if let Some(gate) = &self.group_activation {
            gate.activation()
                .await
                .map_err(SystemError::ActorCreationError)?;
        }
        self.initialize_actor().await?;
        self.start_actor().await
    }

    /// Process a single message with the actor under panic isolation.
    ///
    /// M5: returns the actor's reply payload (previously discarded; the
    /// by-value ask path needs it to fill the inline reply channel).
    async fn process_message(&self, msg: BoxedMessage) -> ActorResult<BoxedMessage> {
        let (msg, parent) = crate::thread::envelope::TraceEnvelope::unpack(msg);
        let parent = parent.or_else(|| {
            msg.downcast_ref::<crate::thread::envelope::AskEnvelope>()
                .and_then(|envelope| envelope.trace_context.clone())
        });
        self.process_message_with_context(msg, parent.map(|parent| parent.child()))
            .await
    }

    async fn process_message_with_context(
        &self,
        msg: BoxedMessage,
        context: Option<parrot_api::trace::TraceContext>,
    ) -> ActorResult<BoxedMessage> {
        parrot_api::trace::scope(context, self.process_message_inner(msg)).await
    }

    async fn process_message_inner(&self, msg: BoxedMessage) -> ActorResult<BoxedMessage> {
        self.escalation.ready().await;
        if self.get_status() != ProcessorStatus::Running {
            return Err(ActorError::MessageHandlingError(format!(
                "Actor at {} is not running, current status: {:?}",
                self.path,
                self.get_status()
            )));
        }

        let engine_control = msg.is::<crate::thread::envelope::ControlMessage>();
        let mut actor_slot = self.actor.lock().await;
        let mut context_slot = self.context.lock().await;
        let actor = actor_slot.as_mut().ok_or_else(|| ActorError::Stopped)?;
        let context = context_slot.as_mut().ok_or_else(|| ActorError::Stopped)?;

        let recording = parrot_api::trace::SpanGuard::current(
            parrot_api::trace::SpanKind::DynamicHandler,
            &self.path,
        );
        // Build the future first; `process_message` itself does not run user code,
        // the returned future does. Catch panics that occur while polling it.
        let fut = {
            let result =
                panic::catch_unwind(AssertUnwindSafe(|| actor.process_message(msg, context)));
            match result {
                Ok(fut) => fut,
                Err(panic_error) => {
                    recording.finish(parrot_api::trace::SpanStatus::Panicked);
                    let error_msg = panic_message(panic_error, &self.path);
                    error!("{}", error_msg);
                    return Err(self
                        .supervise_message_error(ActorError::Panic(error_msg))
                        .await);
                }
            }
        };

        // Poll the future under panic isolation: catch_unwind must wrap the
        // entire poll, not just the move of the future into place. Using
        // futures::FutureExt::catch_unwind guards every poll including the
        // first one, so sync panics inside async user code are captured.
        // `AssertUnwindSafe` is required because the actor/context guards are
        // not structurally UnwindSafe; the mutexes guarantee exclusive access
        // so resuming after a caught panic remains sound.
        let awaited = {
            use futures::FutureExt;
            match AssertUnwindSafe(fut).catch_unwind().await {
                Ok(result) => result,
                Err(panic_error) => {
                    let error_msg = panic_message(panic_error, &self.path);
                    error!("{}", error_msg);
                    Err(ActorError::Panic(error_msg))
                }
            }
        };

        recording.finish_result(&awaited);
        self.stats.messages_processed.fetch_add(1, Ordering::SeqCst);
        match awaited {
            Ok(reply) => Ok(reply),
            Err(e) => {
                self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
                error!(
                    "Error processing message for actor {}: {}",
                    self.path,
                    crate::error_receipt::actor_summary(&e)
                );
                if engine_control {
                    Err(e)
                } else {
                    Err(self.supervise_message_error(e).await)
                }
            }
        }
    }

    fn message_failed(&self) -> bool {
        self.failure_cause.lock().unwrap().is_some()
    }
    async fn supervise_message_error(&self, error: ActorError) -> ActorError {
        let error = Arc::new(error);
        let mut resume = false;
        if let (Some(system), Some(expected)) = (
            self.system.upgrade(),
            self.mailbox.as_ref().and_then(|mailbox| mailbox.upgrade()),
        ) {
            let expected: Arc<dyn Mailbox> = expected;
            let lifetime = {
                let registry = system.registry.read().unwrap();
                registry
                    .get(&self.path)
                    .and_then(|entry| entry.dynamic())
                    .filter(|entry| {
                        Arc::ptr_eq(&entry.mailbox, &expected) && entry.stop_completion.is_none()
                    })
                    .map(|entry| entry.lifetime.clone())
            };
            if let Some(lifetime) = lifetime {
                let fallback = self
                    .config
                    .supervisor_strategy
                    .as_ref()
                    .unwrap_or(&system.config().default_supervisor_strategy);
                resume = lifetime
                    .resume_message(
                        error.clone(),
                        tokio::time::Instant::now() + system.config().shutdown_timeout,
                        matches!(fallback, crate::thread::config::SupervisorStrategy::Resume),
                        &system.runtime_handle,
                    )
                    .await;
            }
        } else if !matches!(error.as_ref(), ActorError::Panic(_)) {
            // Standalone processor fixtures have no managed supervisor.
            resume = true;
        }
        if !resume {
            self.record_failure(error.clone());
        }
        crate::supervision_policy::reply_error(error)
    }

    /// Process a batch of messages pulled from the given mailbox.
    ///
    /// Returns `(processed, errors)`. A non-resumable managed handler failure
    /// ends the batch so queued work cannot run before retirement.
    pub async fn process_batch_of_messages(
        &self,
        mailbox: &Arc<dyn Mailbox>,
        max_messages: usize,
        yield_after_each_message: bool,
    ) -> Result<(usize, usize), SystemError> {
        if self.config.message_batch_size.unwrap_or(1) > 1 {
            return self
                .process_coalesced(mailbox, max_messages, yield_after_each_message)
                .await;
        }
        if self.get_status() != ProcessorStatus::Running {
            return Err(SystemError::WorkerStateError(format!(
                "Actor at {} is not running, current status: {:?}",
                self.path,
                self.get_status()
            )));
        }

        let mut processed = 0usize;
        let mut error_count = 0usize;
        // M2 reduction budget (BEAM-style, batch adaptation):
        //
        // The actor accumulates `consecutive_reductions` across runs while
        // its mailbox never drains. When the count crosses the threshold,
        // this run is *truncated* (single message) and the counter RESETS —
        // exactly POC beam-sched semantics (`if reductions >= budget {
        // preemption; reductions = 0 }`): yield once, then full budget
        // again.
        //
        // IMPORTANT: the counter is only *read* here as a hint. The actual
        // truncation decision belongs to the worker (see worker.rs): a
        // fairness yield only helps when OTHER mailboxes are waiting, so
        // the worker checks `scheduling_queue.len() > 1` before honoring
        // the budget yield. This keeps single-actor pipelines at full
        // throughput (no self-yield churn) while protecting multi-actor
        // fairness.
        let effective_max = max_messages;
        let budget_exhausted_hint = {
            const YIELD_THRESHOLD_MULTIPLIER: usize = 4;
            let consecutive = self.consecutive_reductions.load(Ordering::Relaxed);
            let threshold = max_messages
                .saturating_mul(YIELD_THRESHOLD_MULTIPLIER)
                .max(max_messages);
            consecutive >= threshold
        };
        let _ = budget_exhausted_hint; // worker consults consecutive_reductions() directly

        for _ in 0..effective_max {
            if self.get_status() != ProcessorStatus::Running || mailbox.is_closed().await {
                break;
            }

            // M5: pop the internal item — ask envelopes arrive by value
            // (single-block path, no envelope boxing in transit).
            let item = mailbox.pop_item().await.map(|item| match item {
                crate::thread::mailbox::MailboxItem::Plain(message) => {
                    match message.downcast::<crate::thread::envelope::AskEnvelope>() {
                        Ok(envelope) => crate::thread::mailbox::MailboxItem::Ask(*envelope),
                        Err(message) => crate::thread::mailbox::MailboxItem::Plain(message),
                    }
                }
                item => item,
            });
            match item {
                Some(crate::thread::mailbox::MailboxItem::Ask(mut envelope)) => {
                    // AskEnvelope：拆出载荷与回复通道，按历史语义处理。
                    // M5 按值路径：回复通道由本层直接回填（不经
                    // ThreadActor::handle_ask_envelope 的装箱往返）。
                    let reply = envelope.take_reply();
                    let payload = std::mem::replace(
                        &mut envelope.payload,
                        crate::thread::envelope::AskPayload::Boxed(Box::new(())),
                    );
                    let msg = payload.into_boxed();
                    let context = envelope.trace_context.take().map(|parent| parent.child());
                    let outcome = self.process_message_with_context(msg, context).await;
                    processed += 1;
                    match outcome {
                        Ok(resp) => {
                            if let Some(reply) = reply {
                                let _ = reply.send(Ok(resp));
                            }
                            self.consecutive_reductions.fetch_add(1, Ordering::Relaxed);
                            if yield_after_each_message {
                                tokio::task::yield_now().await;
                            }
                        }
                        Err(e) => {
                            let must_retire = self.message_failed();
                            let repr = crate::error_receipt::actor_summary(&e);
                            if let Some(reply) = reply {
                                let _ = reply.send(Err(e));
                            }
                            error_count += 1;
                            if must_retire {
                                return Err(SystemError::WorkerStateError(repr));
                            }
                        }
                    }
                }
                Some(crate::thread::mailbox::MailboxItem::Plain(msg)) => {
                    match self.process_message(msg).await {
                        Ok(_) => {
                            processed += 1;
                            self.consecutive_reductions.fetch_add(1, Ordering::Relaxed);
                            if yield_after_each_message {
                                tokio::task::yield_now().await;
                            }
                        }
                        Err(e) => {
                            processed += 1;
                            error_count += 1;
                            // A non-resumable failure aborts this scheduler run.
                            if self.message_failed() {
                                return Err(SystemError::WorkerStateError(
                                    crate::error_receipt::actor_summary(&e),
                                ));
                            }
                        }
                    }
                }
                None => break, // mailbox drained
            }
        }

        // M2: if the mailbox drained (no backlog left), the monopolization
        // streak ends — reset consecutive accounting. A backlog (worker
        // will re-queue) keeps the streak alive for the fairness yield.
        if mailbox.is_empty().await {
            self.consecutive_reductions.store(0, Ordering::Relaxed);
        }

        Ok((processed, error_count))
    }

    /// M3: consecutive reduction count (messages processed since the last
    /// empty-mailbox drain). Exposed for worker-side fairness decisions
    /// and metrics.
    pub fn consecutive_reductions(&self) -> usize {
        self.consecutive_reductions.load(Ordering::Relaxed)
    }

    /// M3 supervision hook: notify the owning system that this actor
    /// panicked. Called by the worker after a caught panic; the supervision
    /// executor (restart / stop / escalate) runs detached on the system
    /// runtime so the worker loop never blocks on the decision.
    fn record_failure(&self, error: Arc<ActorError>) {
        // The worker notifies supervision after the handler's scope has exited.
        self.failure_cause
            .lock()
            .unwrap()
            .get_or_insert_with(|| RecordedFailure {
                error,
                context: parrot_api::trace::TraceContext::current(),
            });
        // Releasing the scheduler slot can race with another producer before
        // the detached retirement owner seals the mailbox. Stop execution now.
        self.status
            .store(ProcessorStatus::Failed as usize, Ordering::SeqCst);
    }

    pub(crate) fn notify_system_panic(&self, panic_msg: String) {
        if let (Some(system), Some(mailbox)) = (self.system.upgrade(), self.mailbox.clone()) {
            let (error, context) = self.failure_cause.lock().unwrap().as_ref().map_or_else(
                || (Arc::new(ActorError::Panic(panic_msg)), None),
                |failure| (failure.error.clone(), failure.context.clone()),
            );
            parrot_api::trace::in_scope(context, || {
                system.runtime_spawn_supervision(self.path.clone(), mailbox, error);
            });
        }
    }

    /// Pause a running processor; other lifecycle states are unchanged.
    pub fn pause(&self) {
        debug!("Pausing actor processor for {}", self.path);
        let _ = self.status.compare_exchange(
            ProcessorStatus::Running as usize,
            ProcessorStatus::Paused as usize,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Resume a paused processor; initialization and terminal states are unchanged.
    pub fn resume(&self) {
        debug!("Resuming actor processor for {}", self.path);
        let _ = self.status.compare_exchange(
            ProcessorStatus::Paused as usize,
            ProcessorStatus::Running as usize,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Get current processor status
    pub fn get_status(&self) -> ProcessorStatus {
        ProcessorStatus::from_usize(self.status.load(Ordering::SeqCst))
    }

    /// Get actor path
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get actor config
    pub fn config(&self) -> &ThreadActorConfig {
        &self.config
    }

    /// Get the actor self reference (if set on the context)
    pub async fn self_ref(&self) -> Option<BoxedActorRef> {
        let guard = self.context.lock().await;
        guard
            .as_ref()
            .and_then(|context| context.get_self_ref_opt())
            .map(|r| r.clone_boxed())
    }
}

/// Format a caught panic payload into a stable error string.
fn panic_message(payload: Box<dyn Any + Send>, path: &str) -> String {
    format!(
        "Panic in actor {}: {}",
        path,
        crate::owned_payload::panic_message(payload)
    )
}

impl<A> ProcessorStatsTrait for ActorProcessor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
        Some(self.stats.clone())
    }
}

impl<A> ProcessorInterface for ActorProcessor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    fn begin_escalation(&self, error: Arc<ActorError>) -> bool {
        !self.cancelled.load(Ordering::Acquire)
            && !self.message_failed()
            && self.escalation.begin(error)
    }
    fn resume_after_escalation(&self, error: &Arc<ActorError>) {
        if !self.cancelled.load(Ordering::Acquire) && !self.message_failed() {
            self.escalation.resume(error);
        }
    }
    fn fail_from_escalation(&self, error: Arc<ActorError>) {
        self.record_failure(error);
    }
    fn cancel_context_tasks(&self) {
        if let Some(error) = self.escalation.pending() {
            self.record_failure(error);
            self.cancel_processing();
        }
        self.children.close();
        self.tasks.close();
    }

    fn join_context_tasks(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move { self.tasks.join().await })
    }

    fn cancel_processing(&self) {
        self.children.close();
        self.tasks.close();
        self.cancelled.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
    }

    fn processing_cancelled(&self) -> parrot_api::types::BoxedFuture<'_, ()> {
        Box::pin(async move {
            let notified = self.cancel_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.cancelled.load(Ordering::Acquire) {
                notified.await;
            }
        })
    }

    fn has_failed(&self) -> bool {
        self.message_failed()
    }

    fn is_initialized(&self) -> bool {
        let status = self.get_status();
        status != ProcessorStatus::Initializing && status != ProcessorStatus::Failed
    }

    fn initialize_and_start_erased(
        self: Arc<Self>,
    ) -> BoxedProcessorFuture<Result<(), SystemError>> {
        self.executions.clone().own(
            Box::pin(async move {
                self.escalation
                    .guard(this_initialize_and_start(self.clone()))
                    .await
            }),
            "actor initialization",
        )
    }

    fn stop_erased(self: Arc<Self>) -> BoxedProcessorFuture<Result<(), SystemError>> {
        self.executions.clone().own(
            Box::pin(async move { this_stop(self).await }),
            "actor stop hook",
        )
    }

    fn retire_erased(self: Arc<Self>) -> BoxedProcessorFuture<Result<(), SystemError>> {
        Box::pin(async move { self.retire_storage().await })
    }
    fn retirement(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(crate::thread::system::await_stop(self.retired.subscribe()))
    }

    fn process_batch_erased(
        self: Arc<Self>,
        mailbox: Arc<dyn Mailbox>,
        max_messages: usize,
        yield_after_each_message: bool,
    ) -> BoxedProcessorFuture<Result<(usize, usize), SystemError>> {
        self.executions.clone().own(
            Box::pin(async move {
                self.escalation
                    .guard(this_process_batch(
                        self.clone(),
                        mailbox,
                        max_messages,
                        yield_after_each_message,
                    ))
                    .await
            }),
            "actor batch",
        )
    }

    fn as_any(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn consecutive_reductions(&self) -> usize {
        ActorProcessor::consecutive_reductions(self)
    }

    fn reset_reduction_budget(&self) {
        self.consecutive_reductions.store(0, Ordering::Relaxed);
    }

    fn notify_system_panic(&self, panic_msg: String) {
        ActorProcessor::notify_system_panic(self, panic_msg)
    }
}

/// Free functions used by the erased trait implementations.
async fn this_initialize_and_start<A>(this: Arc<ActorProcessor<A>>) -> Result<(), SystemError>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    this.initialize_and_start().await
}

async fn this_stop<A>(this: Arc<ActorProcessor<A>>) -> Result<(), SystemError>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    this.stop_actor().await
}

async fn this_process_batch<A>(
    this: Arc<ActorProcessor<A>>,
    mailbox: Arc<dyn Mailbox>,
    max_messages: usize,
    yield_after_each_message: bool,
) -> Result<(usize, usize), SystemError>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    this.process_batch_of_messages(&mailbox, max_messages, yield_after_each_message)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::actor::ThreadActor;
    use crate::thread::config::ThreadActorConfig;
    use crate::thread::context::ThreadContext;
    use crate::thread::error::SystemError;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::address::ActorPath;
    use parrot_api::types::BoxedFuture;
    use std::any::Any;
    use std::sync::Arc;

    /// Actor echoing counts; supports panic-on-demand.
    #[derive(Debug, Default)]
    struct TestActor {
        count: u64,
    }

    impl Actor for TestActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if let Some(v) = msg.downcast_ref::<u64>() {
                    self.count += *v;
                    return Ok(Box::new(self.count) as BoxedMessage);
                }
                if msg
                    .downcast_ref::<&str>()
                    .map(|s| *s == "boom")
                    .unwrap_or(false)
                {
                    panic!("boom requested");
                }
                Ok(msg)
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    fn make_processor() -> Arc<ActorProcessor<TestActor>> {
        let context = ThreadContext::<TestActor>::new_for_test("test/processor");
        Arc::new(ActorProcessor::new(
            ThreadActor::new_for_test(TestActor::default()),
            context,
            "test/processor".to_string(),
            ThreadActorConfig::default(),
        ))
    }

    fn make_mailbox() -> Arc<MpscMailbox> {
        Arc::new(MpscMailbox::new(
            16,
            ActorPath::placeholder("test/processor"),
        ))
    }

    #[tokio::test]
    async fn test_processor_lifecycle_initialize_and_start() {
        let processor = make_processor();

        assert!(!processor.is_initialized());
        assert_eq!(processor.get_status(), ProcessorStatus::Initializing);

        processor
            .clone()
            .initialize_and_start_erased()
            .await
            .expect("init+start should succeed");

        assert!(processor.is_initialized());
        assert_eq!(processor.get_status(), ProcessorStatus::Running);
    }

    #[tokio::test]
    async fn test_processor_stop_transitions_status() {
        let processor = make_processor();
        processor
            .clone()
            .initialize_and_start_erased()
            .await
            .unwrap();

        processor
            .clone()
            .stop_erased()
            .await
            .expect("stop should succeed");
        assert_eq!(processor.get_status(), ProcessorStatus::Stopped);
        // Stopped processors are still "initialized" in the lifecycle sense
        // (they got past initialization); they merely refuse new batches.
        assert!(processor.is_initialized());
    }

    #[tokio::test]
    async fn test_process_batch_rejects_when_not_running() {
        let processor = make_processor();
        let mailbox: Arc<dyn Mailbox> = make_mailbox();

        let result = processor
            .clone()
            .process_batch_erased(mailbox, 4, false)
            .await;
        assert!(matches!(result, Err(SystemError::WorkerStateError(_))));
    }

    #[tokio::test]
    async fn test_process_batch_counts_and_drains() {
        let processor = make_processor();
        processor
            .clone()
            .initialize_and_start_erased()
            .await
            .unwrap();

        let mailbox: Arc<dyn Mailbox> = make_mailbox();
        for i in 1..=5u64 {
            mailbox
                .push(
                    Box::new(i),
                    crate::thread::config::BackpressureStrategy::Block,
                )
                .await
                .unwrap();
        }

        let (processed, errors) = processor
            .clone()
            .process_batch_erased(mailbox.clone(), 3, false)
            .await
            .expect("batch should succeed");
        assert_eq!((processed, errors), (3, 0));
        assert_eq!(mailbox.len().await, 2);

        let (processed2, errors2) = processor
            .clone()
            .process_batch_erased(mailbox.clone(), 10, false)
            .await
            .expect("batch should succeed");
        assert_eq!((processed2, errors2), (2, 0));
        assert!(mailbox.is_empty().await);
    }

    #[tokio::test]
    async fn test_process_batch_isolates_actor_panic() {
        let processor = make_processor();
        processor
            .clone()
            .initialize_and_start_erased()
            .await
            .unwrap();

        let mailbox: Arc<dyn Mailbox> = make_mailbox();
        mailbox
            .push(
                Box::new("boom"),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();

        let result = processor
            .clone()
            .process_batch_erased(mailbox, 4, false)
            .await;
        assert!(
            matches!(result, Err(SystemError::WorkerStateError(_))),
            "actor panic must surface as WorkerStateError, got {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_stats_tracking() {
        let processor = make_processor();
        processor
            .clone()
            .initialize_and_start_erased()
            .await
            .unwrap();

        let stats = processor.get_statistics().expect("stats present");
        assert_eq!(stats.messages_processed.load(Ordering::SeqCst), 0);

        let mailbox: Arc<dyn Mailbox> = make_mailbox();
        mailbox
            .push(
                Box::new(1u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        processor
            .clone()
            .process_batch_erased(mailbox, 4, false)
            .await
            .unwrap();

        assert_eq!(stats.messages_processed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_pause_and_resume() {
        let processor = make_processor();
        processor
            .clone()
            .initialize_and_start_erased()
            .await
            .unwrap();

        processor.pause();
        assert_eq!(processor.get_status(), ProcessorStatus::Paused);

        // Paused processor refuses to process batches.
        let mailbox: Arc<dyn Mailbox> = make_mailbox();
        let result = processor
            .clone()
            .process_batch_erased(mailbox, 4, false)
            .await;
        assert!(matches!(result, Err(SystemError::WorkerStateError(_))));

        processor.resume();
        assert_eq!(processor.get_status(), ProcessorStatus::Running);
    }

    #[tokio::test]
    async fn pause_and_resume_preserve_uninitialized_and_terminal_states() {
        for state in [
            ProcessorStatus::Initializing,
            ProcessorStatus::Stopping,
            ProcessorStatus::Stopped,
            ProcessorStatus::Failed,
        ] {
            for pause in [true, false] {
                let processor = make_processor();
                processor.status.store(state as usize, Ordering::SeqCst);
                if pause {
                    processor.pause();
                } else {
                    processor.resume();
                }
                assert_eq!(
                    processor.get_status(),
                    state,
                    "pause/resume must preserve lifecycle state"
                );
            }
        }
    }

    #[tokio::test]
    async fn cross_thread_pause_resume_cannot_revive_stopping_processor() {
        let processor = make_processor();
        let ready = Arc::new(std::sync::Barrier::new(2));
        let worker = {
            let processor = processor.clone();
            let ready = ready.clone();
            std::thread::spawn(move || {
                ready.wait();
                for _ in 0..1000 {
                    processor.pause();
                    processor.resume();
                }
            })
        };
        processor
            .status
            .store(ProcessorStatus::Stopping as usize, Ordering::SeqCst);
        ready.wait();
        worker.join().unwrap();
        assert_eq!(
            processor.get_status(),
            ProcessorStatus::Stopping,
            "pause/resume must preserve lifecycle state"
        );
    }

    #[tokio::test]
    async fn test_any_downcast_round_trip() {
        let processor = make_processor();
        // as_any_ref exposes &dyn Any; verify the concrete type is recoverable.
        let any_ref: &dyn Any = processor.as_any_ref();
        assert!(
            any_ref
                .downcast_ref::<ActorProcessor<TestActor>>()
                .is_some(),
            "as_any_ref must expose the concrete processor type"
        );
        assert!(processor.path() == "test/processor");
    }

    #[tokio::test]
    async fn test_self_ref_is_none_when_not_set() {
        let processor = make_processor();
        assert!(processor.self_ref().await.is_none());
    }
}

#[cfg(test)]
mod failed_worker_stop_tests {
    use super::*;
    use crate::thread::{
        BackpressureStrategy, SchedulingMode, envelope::AskEnvelope, mailbox::mpsc::MpscMailbox,
        scheduler::ThreadSchedulerFactory,
    };
    use parrot_api::{
        actor::{ActorState, EmptyConfig},
        address::ActorPath,
    };
    use std::time::Duration;

    struct FailedMember {
        hooks: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        panic_drop: bool,
    }
    impl Drop for FailedMember {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic_drop, "failed member destructor panicked");
        }
    }
    impl Actor for FailedMember {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn receive_message<'a>(
            &'a mut self,
            _: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { panic!("member failed while its cohort was retiring") })
        }
        fn before_stop<'a>(
            &'a mut self,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            self.hooks.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cohort_stop_after_failed_dedicated_worker_exit_uses_real_retirement_receipt() {
        for panic_drop in [false, true] {
            let hooks = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let path = "/failed-cohort-member";
            // No system owner: explicitly order the group's stop after the
            // real dedicated worker has processed its panic and exited.
            let processor = Arc::new(ActorProcessor::new(
                ThreadActor::new_for_test(FailedMember {
                    hooks: hooks.clone(),
                    drops: drops.clone(),
                    panic_drop,
                }),
                ThreadContext::new_for_test(path),
                path.into(),
                Default::default(),
            ));
            let mailbox: Arc<dyn Mailbox> =
                Arc::new(MpscMailbox::new(8, ActorPath::placeholder(path)));
            mailbox.set_processor(processor.clone());
            let schedulers = Arc::new(
                ThreadSchedulerFactory::new(tokio::runtime::Handle::current())
                    .create_scheduler_group(None, None),
            );
            schedulers
                .dedicated_scheduler
                .schedule_with_processor(
                    path,
                    mailbox.clone(),
                    processor.clone(),
                    Default::default(),
                )
                .unwrap();
            let (ask, reply) = AskEnvelope::with_boxed(Box::new(()));
            mailbox
                .push_ask(ask, BackpressureStrategy::Block)
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(2), reply)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            let retired = tokio::time::timeout(Duration::from_secs(2), processor.retirement())
                .await
                .unwrap();
            assert_eq!(retired.is_err(), panic_drop);
            schedulers
                .dedicated_scheduler
                .deschedule(path)
                .await
                .unwrap();
            let result = crate::thread::system::stop_actor_resources(
                path,
                mailbox,
                SchedulingMode::DedicatedThread,
                schedulers,
                Arc::new(tokio::sync::Semaphore::new(1)),
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await;
            if panic_drop {
                assert!(matches!(result, Err(SystemError::Panic(_))), "{result:?}");
            } else {
                assert!(
                    result.is_ok(),
                    "failed worker exit is not a stop-hook failure: {result:?}"
                );
            }
            assert_eq!(hooks.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn stop_waiting_for_actor_lock_skips_hook_if_inflight_handler_has_failed() {
        let hooks = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let processor = Arc::new(ActorProcessor::new(
            ThreadActor::new_for_test(FailedMember {
                hooks: hooks.clone(),
                drops: drops.clone(),
                panic_drop: false,
            }),
            ThreadContext::new_for_test("/failed-cohort-member"),
            "/failed-cohort-member".into(),
            Default::default(),
        ));
        processor.initialize_and_start().await.unwrap();
        let actor = processor.actor.lock().await;
        let stop = processor.stop_actor();
        tokio::pin!(stop);
        assert!(futures::poll!(&mut stop).is_pending());
        // A failure was resolved while normal stop was waiting for this lock.
        processor.record_failure(Arc::new(ActorError::Panic("inflight failure".into())));
        drop(actor);
        stop.await.unwrap();
        assert_eq!(
            hooks.load(Ordering::SeqCst),
            0,
            "failed instance ran before_stop"
        );
        processor.retire_storage().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod recursive_panic_tests {
    use super::*;
    use parrot_api::actor::{ActorState, EmptyConfig};
    struct Payload;
    impl Drop for Payload {
        fn drop(&mut self) {
            panic!("initialization payload destructor");
        }
    }
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
        fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { std::panic::panic_any(Payload) })
        }
        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(message) })
        }
    }
    #[tokio::test]
    async fn initialization_secondary_panic_preserves_error_and_storage_retirement() {
        let drops = Arc::new(AtomicUsize::new(0));
        let processor = ActorProcessor::new(
            ThreadActor::new_for_test(Probe(drops.clone())),
            ThreadContext::new_for_test("/recursive-init"),
            "/recursive-init".into(),
            Default::default(),
        );
        let result = processor.initialize_and_start().await;
        let Err(SystemError::ActorCreationError(message)) = result else {
            panic!("wrong initialization outcome: {result:?}");
        };
        assert!(
            message.contains("/recursive-init")
                && message.contains("panic payload destructor panicked"),
            "{message}"
        );
        assert_eq!(processor.get_status(), ProcessorStatus::Failed);
        processor.retire_storage().await.unwrap();
        processor.retire_storage().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
