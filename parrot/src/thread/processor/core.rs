use std::sync::Arc;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, error};
use anyhow;

use parrot_api::actor::Actor;
use parrot_api::errors::ActorError;
use parrot_api::types::{BoxedMessage, ActorResult, BoxedActorRef};

use crate::thread::actor::ThreadActor;
use crate::thread::context::ThreadContext;
use crate::thread::mailbox::Mailbox;
use crate::thread::envelope::ControlMessage;
use crate::thread::error::SystemError;
use crate::thread::config::ThreadActorConfig;

use std::panic;
use std::panic::AssertUnwindSafe;
use std::any::Any;
use tokio::sync::Mutex as AsyncMutex;

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
    fn initialize_and_start_erased(self: Arc<Self>) -> BoxedProcessorFuture<Result<(), SystemError>>;

    /// Stop the actor gracefully.
    fn stop_erased(self: Arc<Self>) -> BoxedProcessorFuture<Result<(), SystemError>>;

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
}

/// Boxed future used by [`ProcessorInterface`].
pub type BoxedProcessorFuture<T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'static>>;

/// Result type for actor execution: either unit or a boxed reply payload.
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
    actor: AsyncMutex<ThreadActor<A>>,

    /// Actor context (async mutex: guards must be held across `.await`)
    context: AsyncMutex<ThreadContext<A>>,

    /// Actor path (plain string copy, avoids borrow of self-referential ActorPath)
    path: String,

    /// Actor config
    config: ThreadActorConfig,

    /// Processor status
    status: Arc<AtomicUsize>,

    /// Processor stats
    stats: Arc<ProcessorStats>,
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
            actor: AsyncMutex::new(actor),
            context: AsyncMutex::new(context),
            path,
            config,
            status: Arc::new(AtomicUsize::new(ProcessorStatus::Initializing as usize)),
            stats: Arc::new(ProcessorStats::new()),
        }
    }

    /// Initialize the actor (calls `Actor::init`)
    pub async fn initialize_actor(&self) -> Result<(), SystemError> {
        debug!("Initializing actor at {}", self.path);
        let mut actor = self.actor.lock().await;
        let mut context = self.context.lock().await;
        if let Err(e) = actor.initialize(&mut context).await {
            error!("Failed to initialize actor at {}: {}", self.path, e);
            self.status.store(ProcessorStatus::Failed as usize, Ordering::SeqCst);
            self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
            return Err(SystemError::ActorCreationError(format!(
                "Failed to initialize actor at {}: {}",
                self.path, e
            )));
        }
        Ok(())
    }

    /// Send Start control message to the actor (Starting -> Running)
    pub async fn start_actor(&self) -> Result<(), SystemError> {
        debug!("Sending Start message to actor at {}", self.path);
        let start_msg = Box::new(ControlMessage::Start);
        let mut actor = self.actor.lock().await;
        let mut context = self.context.lock().await;
        if let Err(e) = actor.process_message(start_msg, &mut context).await {
            error!("Failed to start actor at {}: {}", self.path, e);
            self.status.store(ProcessorStatus::Failed as usize, Ordering::SeqCst);
            self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
            return Err(SystemError::ActorCreationError(format!(
                "Failed to start actor at {}: {}",
                self.path, e
            )));
        }
        self.status.store(ProcessorStatus::Running as usize, Ordering::SeqCst);
        Ok(())
    }

    /// Send Stop control message to the actor (graceful shutdown)
    pub async fn stop_actor(&self) -> Result<(), SystemError> {
        debug!("Sending Stop message to actor at {}", self.path);
        self.status
            .store(ProcessorStatus::Stopping as usize, Ordering::SeqCst);

        let stop_msg = Box::new(ControlMessage::Stop);
        let mut actor = self.actor.lock().await;
        let mut context = self.context.lock().await;
        if let Err(e) = actor.process_message(stop_msg, &mut context).await {
            error!("Failed to stop actor at {}: {}", self.path, e);
            self.status.store(ProcessorStatus::Failed as usize, Ordering::SeqCst);
            self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
            return Err(SystemError::ShutdownError(format!(
                "Failed to stop actor at {}: {}",
                self.path, e
            )));
        }
        self.status.store(ProcessorStatus::Stopped as usize, Ordering::SeqCst);
        Ok(())
    }

    /// Initialize and start the actor
    pub async fn initialize_and_start(&self) -> Result<(), SystemError> {
        self.initialize_actor().await?;
        self.start_actor().await
    }

    /// Process a single message with the actor under panic isolation.
    async fn process_message(&self, msg: BoxedMessage) -> ActorResult<()> {
        if self.get_status() != ProcessorStatus::Running {
            return Err(ActorError::MessageHandlingError(format!(
                "Actor at {} is not running, current status: {:?}",
                self.path,
                self.get_status()
            )));
        }

        let mut actor = self.actor.lock().await;
        let mut context = self.context.lock().await;

        // Build the future first; `process_message` itself does not run user code,
        // the returned future does. Catch panics that occur while polling it.
        let fut = {
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                actor.process_message(msg, &mut context)
            }));
            match result {
                Ok(fut) => fut,
                Err(panic_error) => {
                    let error_msg = panic_message(panic_error, &self.path);
                    error!("{}", error_msg);
                    return Err(ActorError::Panic(error_msg));
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

        self.stats.messages_processed.fetch_add(1, Ordering::SeqCst);
        match awaited {
            Ok(_) => Ok(()),
            Err(e) => {
                self.stats.errors_encountered.fetch_add(1, Ordering::SeqCst);
                error!("Error processing message for actor {}: {}", self.path, e);
                Err(e)
            }
        }
    }

    /// Process a batch of messages pulled from the given mailbox.
    ///
    /// Returns `(processed, errors)`. Returns `Err` only when the actor panicked;
    /// ordinary message errors are counted in `errors`.
    pub async fn process_batch_of_messages(
        &self,
        mailbox: &Arc<dyn Mailbox>,
        max_messages: usize,
        yield_after_each_message: bool,
    ) -> Result<(usize, usize), SystemError> {
        if self.get_status() != ProcessorStatus::Running {
            return Err(SystemError::WorkerStateError(format!(
                "Actor at {} is not running, current status: {:?}",
                self.path,
                self.get_status()
            )));
        }

        let mut processed = 0usize;
        let mut error_count = 0usize;

        for _ in 0..max_messages {
            if self.get_status() != ProcessorStatus::Running {
                break;
            }

            match mailbox.pop().await {
                Some(msg) => match self.process_message(msg).await {
                    Ok(_) => {
                        processed += 1;
                        if yield_after_each_message {
                            tokio::task::yield_now().await;
                        }
                    }
                    Err(e) => {
                        processed += 1;
                        error_count += 1;
                        // A panic aborts the batch and is reported to the scheduler.
                        if matches!(e, ActorError::Panic(_)) {
                            return Err(SystemError::WorkerStateError(e.to_string()));
                        }
                    }
                },
                None => break, // mailbox drained
            }
        }

        Ok((processed, error_count))
    }

    /// Pause the processor
    pub fn pause(&self) {
        debug!("Pausing actor processor for {}", self.path);
        self.status
            .store(ProcessorStatus::Paused as usize, Ordering::SeqCst);
    }

    /// Resume the processor
    pub fn resume(&self) {
        debug!("Resuming actor processor for {}", self.path);
        self.status
            .store(ProcessorStatus::Running as usize, Ordering::SeqCst);
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
        guard.get_self_ref_opt().map(|r| r.clone_boxed())
    }
}

/// Format a caught panic payload into a stable error string.
fn panic_message(payload: Box<dyn Any + Send>, path: &str) -> String {
    match payload.downcast::<String>() {
        Ok(string) => format!("Panic in actor {}: {}", path, string),
        Err(e) => match e.downcast::<&'static str>() {
            Ok(s) => format!("Panic in actor {}: {}", path, s),
            Err(e) => format!("Panic in actor {}: {:?}", path, e),
        },
    }
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
    fn is_initialized(&self) -> bool {
        let status = self.get_status();
        status != ProcessorStatus::Initializing && status != ProcessorStatus::Failed
    }

    fn initialize_and_start_erased(self: Arc<Self>) -> BoxedProcessorFuture<Result<(), SystemError>> {
        Box::pin(async move { this_initialize_and_start(self).await })
    }

    fn stop_erased(self: Arc<Self>) -> BoxedProcessorFuture<Result<(), SystemError>> {
        Box::pin(async move { this_stop(self).await })
    }

    fn process_batch_erased(
        self: Arc<Self>,
        mailbox: Arc<dyn Mailbox>,
        max_messages: usize,
        yield_after_each_message: bool,
    ) -> BoxedProcessorFuture<Result<(usize, usize), SystemError>> {
        Box::pin(async move {
            this_process_batch(self, mailbox, max_messages, yield_after_each_message).await
        })
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
}

/// Free functions used by the erased trait implementations.
async fn this_initialize_and_start<A>(
    this: Arc<ActorProcessor<A>>,
) -> Result<(), SystemError>
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
    use crate::thread::context::ThreadContext;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use crate::thread::actor::ThreadActor;
    use crate::thread::config::ThreadActorConfig;
    use crate::thread::error::SystemError;
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

        fn init<'a>(
            &'a mut self,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
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
                if msg.downcast_ref::<&str>().map(|s| *s == "boom").unwrap_or(false) {
                    panic!("boom requested");
                }
                Ok(msg)
            })
        }

        fn receive_message_with_engine<'a>(
            &'a mut self,
            _msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
            _engine_ctx: std::ptr::NonNull<dyn Any>,
        ) -> Option<ActorResult<BoxedMessage>> {
            None
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
        processor.clone().initialize_and_start_erased().await.unwrap();

        processor.clone().stop_erased().await.expect("stop should succeed");
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
        processor.clone().initialize_and_start_erased().await.unwrap();

        let mailbox: Arc<dyn Mailbox> = make_mailbox();
        for i in 1..=5u64 {
            mailbox
                .push(Box::new(i), crate::thread::config::BackpressureStrategy::Block)
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
        processor.clone().initialize_and_start_erased().await.unwrap();

        let mailbox: Arc<dyn Mailbox> = make_mailbox();
        mailbox
            .push(Box::new("boom"), crate::thread::config::BackpressureStrategy::Block)
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
        processor.clone().initialize_and_start_erased().await.unwrap();

        let stats = processor.get_statistics().expect("stats present");
        assert_eq!(stats.messages_processed.load(Ordering::SeqCst), 0);

        let mailbox: Arc<dyn Mailbox> = make_mailbox();
        mailbox
            .push(Box::new(1u64), crate::thread::config::BackpressureStrategy::Block)
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
        processor.clone().initialize_and_start_erased().await.unwrap();

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
    async fn test_any_downcast_round_trip() {
        let processor = make_processor();
        // as_any_ref exposes &dyn Any; verify the concrete type is recoverable.
        let any_ref: &dyn Any = processor.as_any_ref();
        assert!(
            any_ref.downcast_ref::<ActorProcessor<TestActor>>().is_some(),
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
