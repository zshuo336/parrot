//! Dedicated-thread scheduler: one OS thread per actor.

use std::collections::HashMap;
use std::error::Error;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::Duration;

use tokio::runtime::Handle;
use tracing::{debug, error};

use parrot_api::actor::Actor;
use parrot_api::types::BoxedMessage;

use crate::thread::config::{ThreadActorConfig, SchedulingMode};
use crate::thread::context::ThreadContext;
use crate::thread::error::SystemError;
use crate::thread::mailbox::Mailbox;
use crate::thread::processor::ProcessorInterface;
use crate::thread::scheduler::ThreadScheduler;

/// Configuration for the dedicated thread scheduler.
#[derive(Debug, Clone)]
pub struct DedicatedThreadConfig {
    /// Maximum number of dedicated threads allowed.
    pub max_threads: usize,

    /// Duration to sleep when idle before polling for messages again.
    pub idle_sleep_duration: Duration,

    /// Whether to yield after processing each message.
    pub yield_after_each_message: bool,

    /// Default batch size per wake-up.
    pub max_messages_per_batch: usize,

    /// Stack size for dedicated OS threads.
    pub default_thread_stack_size: usize,
}

impl Default for DedicatedThreadConfig {
    fn default() -> Self {
        Self {
            max_threads: 32,
            idle_sleep_duration: Duration::from_millis(10),
            yield_after_each_message: false,
            max_messages_per_batch: 16,
            default_thread_stack_size: 3 * 1024 * 1024, // 3 MB
        }
    }
}

/// Status of a dedicated worker thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Initializing = 0,
    Idle = 1,
    Processing = 2,
    Paused = 3,
    ShuttingDown = 4,
    Error = 5,
}

impl WorkerState {
    pub fn from_usize(v: usize) -> Self {
        match v {
            0 => WorkerState::Initializing,
            1 => WorkerState::Idle,
            2 => WorkerState::Processing,
            3 => WorkerState::Paused,
            4 => WorkerState::ShuttingDown,
            _ => WorkerState::Error,
        }
    }
}

/// Scheduler that runs each actor on its own dedicated OS thread.
///
/// Each worker thread owns a single-tenant, current-thread Tokio runtime and
/// drives the actor's mailbox through its type-erased processor. Batches are
/// processed with panic isolation; a panicking actor stops its thread and the
/// actor is descheduled.
#[derive(Debug)]
pub struct DedicatedThreadScheduler {
    /// Active workers by actor path
    workers: Mutex<HashMap<String, Arc<DedicatedWorker>>>,

    /// Global shutdown flag
    is_shutting_down: Arc<AtomicBool>,

    /// Scheduler status (SchedulerStatus codes)
    status: Arc<AtomicUsize>,

    /// Configuration
    config: DedicatedThreadConfig,

    /// Runtime handle (used to join worker threads through blocking tasks)
    runtime_handle: Option<Handle>,
}

impl DedicatedThreadScheduler {
    /// Create a new dedicated thread scheduler.
    pub fn new(config: Option<DedicatedThreadConfig>) -> Self {
        let config = config.unwrap_or_default();
        let mut scheduler = Self {
            workers: Mutex::new(HashMap::new()),
            is_shutting_down: Arc::new(AtomicBool::new(false)),
            status: Arc::new(AtomicUsize::new(0)),
            config,
            runtime_handle: None,
        };
        scheduler.status.store(1, Ordering::SeqCst); // Running
        scheduler
    }

    /// Attach a runtime handle (used for join bookkeeping).
    pub fn with_runtime_handle(mut self, handle: Handle) -> Self {
        self.runtime_handle = Some(handle);
        self
    }

    /// Schedule an actor (with its processor-carrying mailbox) on a dedicated thread.
    pub fn schedule_with_processor(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        processor: Arc<dyn ProcessorInterface>,
        config: ThreadActorConfig,
    ) -> Result<(), SystemError> {
        if self.is_shutting_down.load(Ordering::Relaxed) {
            return Err(SystemError::ShuttingDown);
        }

        let mut workers = self.workers.lock().unwrap();

        if workers.contains_key(path) {
            return Ok(()); // already scheduled (idempotent)
        }

        if workers.len() >= self.config.max_threads {
            return Err(SystemError::ConfigError(format!(
                "Dedicated thread limit reached ({} threads)",
                self.config.max_threads
            )));
        }

        let worker = Arc::new(DedicatedWorker::new(
            path.to_string(),
            mailbox,
            processor,
            config,
            self.config.max_messages_per_batch,
        ));

        worker.start().map_err(|e| {
            SystemError::ThreadSetupError(format!("Failed to start dedicated thread for {}: {}", path, e))
        })?;

        workers.insert(path.to_string(), worker);
        Ok(())
    }

    /// Deschedule an actor from its dedicated thread (stops the thread).
    pub async fn deschedule(&self, path: &str) -> Result<(), SystemError> {
        let worker = {
            let mut workers = self.workers.lock().unwrap();
            workers.remove(path)
        };

        match worker {
            Some(w) => w.stop().await,
            None => Err(SystemError::ActorNotFound(path.to_string())),
        }
    }

    /// Check if an actor is scheduled.
    pub fn is_scheduled(&self, path: &str) -> bool {
        self.workers.lock().unwrap().contains_key(path)
    }

    /// Shutdown the scheduler and all dedicated threads.
    pub async fn shutdown(&self) -> Result<(), SystemError> {
        self.status.store(2, Ordering::SeqCst); // ShuttingDown
        self.is_shutting_down.store(true, Ordering::SeqCst);

        let workers: Vec<Arc<DedicatedWorker>> = {
            let mut workers = self.workers.lock().unwrap();
            workers.drain().map(|(_, w)| w).collect()
        };

        for worker in workers {
            if let Err(e) = worker.stop().await {
                error!("Failed to stop dedicated worker {}: {}", worker.path, e);
            }
        }

        self.status.store(3, Ordering::SeqCst); // Shutdown
        Ok(())
    }

    /// Current number of dedicated workers.
    pub fn worker_count(&self) -> usize {
        self.workers.lock().unwrap().len()
    }

    /// Scheduler status code (0=Init,1=Running,2=ShuttingDown,3=Shutdown).
    pub fn status_code(&self) -> usize {
        self.status.load(Ordering::Relaxed)
    }
}

impl ThreadScheduler for DedicatedThreadScheduler {
    fn metrics_thread_count(&self) -> usize {
        self.worker_count()
    }

    fn schedule(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // The processor must already be attached to the mailbox.
        let processor = mailbox
            .get_processor()
            .ok_or_else(|| Box::new(SystemError::Other(anyhow::anyhow!(
                "Mailbox for actor {} has no processor attached",
                path
            ))) as Box<dyn Error + Send + Sync>)?;

        let config = config.unwrap_or_default();
        self.schedule_with_processor(path, mailbox, processor, config)
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
    }

    fn deschedule(&self, path: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        if let Some(handle) = &self.runtime_handle {
            return handle
                .block_on(async { self.deschedule(path).await })
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>);
        }
        Handle::current()
            .block_on(async { self.deschedule(path).await })
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
    }

    fn is_scheduled(&self, path: &str) -> bool {
        self.is_scheduled(path)
    }

    fn shutdown(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let fut = async { self.shutdown().await };
        if let Some(handle) = &self.runtime_handle {
            return handle
                .block_on(fut)
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>);
        }
        Handle::current()
            .block_on(fut)
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
    }
}

/// Extension trait: schedule with a pre-built processor (typed entry point).
pub trait TypedThreadSchedulerExt {
    /// Schedule by processor; the typed generic form kept for API compatibility.
    fn schedule_typed_by_processor<A>(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        processor: Arc<dyn ProcessorInterface>,
        config: ThreadActorConfig,
    ) -> Result<(), SystemError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static;
}

impl TypedThreadSchedulerExt for DedicatedThreadScheduler {
    fn schedule_typed_by_processor<A>(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        processor: Arc<dyn ProcessorInterface>,
        config: ThreadActorConfig,
    ) -> Result<(), SystemError>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
    {
        let _ = std::marker::PhantomData::<fn() -> A>; // type tag only
        self.schedule_with_processor(path, mailbox, processor, config)
    }
}

/// A single dedicated worker thread driving one actor.
struct DedicatedWorker {
    /// Actor path
    path: String,

    /// The actor's mailbox
    mailbox: Arc<dyn Mailbox>,

    /// The type-erased processor
    processor: Arc<dyn ProcessorInterface>,

    /// Actor configuration
    config: ThreadActorConfig,

    /// Batch size
    batch_size: usize,

    /// Shutdown flag
    shutdown_flag: Arc<AtomicBool>,

    /// Worker state
    state: Arc<AtomicUsize>,

    /// Join slot for the OS thread
    join_slot: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for DedicatedWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DedicatedWorker")
            .field("path", &self.path)
            .field("state", &WorkerState::from_usize(self.state.load(Ordering::Relaxed)))
            .finish()
    }
}

impl DedicatedWorker {
    fn new(
        path: String,
        mailbox: Arc<dyn Mailbox>,
        processor: Arc<dyn ProcessorInterface>,
        config: ThreadActorConfig,
        batch_size: usize,
    ) -> Self {
        Self {
            path,
            mailbox,
            processor,
            config,
            batch_size,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            state: Arc::new(AtomicUsize::new(WorkerState::Initializing as usize)),
            join_slot: Mutex::new(None),
        }
    }

    /// Spawn the dedicated OS thread.
    fn start(&self) -> Result<(), String> {
        let path = self.path.clone();
        let mailbox = self.mailbox.clone();
        let processor = self.processor.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let state = self.state.clone();
        let batch_size = self.batch_size;
        let idle_sleep = self
            .config
            .idle_sleep_duration
            .unwrap_or(Duration::from_millis(10));
        let yield_each = self.config.yield_after_each_message.unwrap_or(false);
        let stack_size = self
            .config
            .thread_stack_size
            .unwrap_or(3 * 1024 * 1024);

        let builder = std::thread::Builder::new()
            .name(format!("parrot-dedicated-{}", path))
            .stack_size(stack_size);

        let handle = builder
            .spawn(move || {
                // Single-tenant current-thread runtime for this OS thread.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap_or_else(|e| {
                        error!("Failed to create runtime for dedicated worker {}: {}", path, e);
                        std::process::exit(1);
                    });

                state.store(WorkerState::Idle as usize, Ordering::Relaxed);

                rt.block_on(async move {
                    // Initialize the actor before processing messages.
                    if let Err(e) = processor.clone().initialize_and_start_erased().await {
                        error!("Failed to initialize actor {}: {}", path, e);
                        state.store(WorkerState::Error as usize, Ordering::Relaxed);
                        return;
                    }

                    while !shutdown_flag.load(Ordering::Relaxed) {
                        if mailbox.is_empty().await {
                            tokio::time::sleep(idle_sleep).await;
                            continue;
                        }

                        state.store(WorkerState::Processing as usize, Ordering::Relaxed);

                        // Batch under panic isolation (spawned task catches panics).
                        let task = tokio::spawn({
                            let processor = processor.clone();
                            let mailbox = mailbox.clone();
                            async move {
                                processor
                                    .process_batch_erased(mailbox, batch_size, yield_each)
                                    .await
                            }
                        });

                        match task.await {
                            Ok(Ok((processed, errors))) => {
                                if errors > 0 {
                                    debug!(
                                        "Dedicated worker {}: processed {} messages with {} errors",
                                        path, processed, errors
                                    );
                                }
                            }
                            Ok(Err(e)) => {
                                error!("Dedicated worker {} batch failed: {}", path, e);
                            }
                            Err(join_err) => {
                                error!("Dedicated worker {} actor panicked: {}", path, join_err);
                                state.store(WorkerState::Error as usize, Ordering::Relaxed);
                                shutdown_flag.store(true, Ordering::Relaxed);
                            }
                        }

                        if state.load(Ordering::Relaxed) != WorkerState::Error as usize {
                            state.store(WorkerState::Idle as usize, Ordering::Relaxed);
                        }
                    }

                    // Graceful stop.
                    let _ = processor.clone().stop_erased().await;
                    state.store(WorkerState::ShuttingDown as usize, Ordering::Relaxed);
                });
            })
            .map_err(|e| e.to_string())?;

        *self.join_slot.lock().unwrap() = Some(handle);
        Ok(())
    }

    /// Stop the worker and join the thread.
    async fn stop(&self) -> Result<(), SystemError> {
        self.shutdown_flag.store(true, Ordering::Relaxed);

        let handle = self.join_slot.lock().unwrap().take();
        if let Some(handle) = handle {
            // The dedicated thread runs its own runtime; join from blocking pool.
            let _ = tokio::task::spawn_blocking(move || {
                let _ = handle.join();
            })
            .await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use crate::thread::processor::ActorProcessor;
    use crate::thread::actor::ThreadActor;
    use crate::thread::context::ThreadContext;
    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::address::ActorPath;
    use parrot_api::types::{ActorResult, BoxedFuture};
    use std::any::Any;

    #[derive(Debug)]
    struct CounterActor {
        count: usize,
    }

    impl Actor for CounterActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if msg.downcast_ref::<u64>().is_some() {
                    self.count += 1;
                    return Ok(Box::new(self.count) as BoxedMessage);
                }
                Ok(msg)
            })
        }

        fn receive_message_with_engine<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context, _engine_ctx: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
            None
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    fn build_actor_stack(path: &str) -> (Arc<MpscMailbox>, Arc<ActorProcessor<CounterActor>>) {
        let actor_path = ActorPath::placeholder(path);
        let mailbox = Arc::new(MpscMailbox::new(64, actor_path));
        let context = ThreadContext::new_for_test(path);
        let processor = Arc::new(ActorProcessor::<CounterActor>::new(
            ThreadActor::new_for_test(CounterActor { count: 0 }),
            context,
            path.to_string(),
            ThreadActorConfig::default(),
        ));
        mailbox.set_processor(processor.clone() as Arc<dyn ProcessorInterface>);
        (mailbox, processor)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_dedicated_scheduling_processes_messages() {
        let scheduler = DedicatedThreadScheduler::new(Some(DedicatedThreadConfig {
            max_threads: 2,
            idle_sleep_duration: Duration::from_millis(5),
            ..Default::default()
        }));

        let (mailbox, _processor) = build_actor_stack("test/dedicated/counter");

        scheduler
            .schedule_with_processor(
                "test/dedicated/counter",
                mailbox.clone(),
                mailbox.get_processor().unwrap(),
                ThreadActorConfig {
                    scheduling_mode: Some(SchedulingMode::DedicatedThread),
                    ..Default::default()
                },
            )
            .unwrap();

        assert!(scheduler.is_scheduled("test/dedicated/counter"));
        assert_eq!(scheduler.worker_count(), 1);

        // Send messages; the dedicated thread should process them.
        for i in 0..5u64 {
            mailbox
                .push(Box::new(i) as BoxedMessage, crate::thread::config::BackpressureStrategy::Block)
                .await
                .unwrap();
        }

        // Wait for processing.
        for _ in 0..100 {
            if mailbox.is_empty().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(mailbox.is_empty().await, "dedicated thread should drain the mailbox");

        scheduler.deschedule("test/dedicated/counter").await.unwrap();
        assert_eq!(scheduler.worker_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_thread_limit_enforced() {
        let scheduler = DedicatedThreadScheduler::new(Some(DedicatedThreadConfig {
            max_threads: 1,
            ..Default::default()
        }));

        let (m1, _) = build_actor_stack("test/lim/a");
        let (m2, _) = build_actor_stack("test/lim/b");

        scheduler
            .schedule_with_processor("test/lim/a", m1.clone(), m1.get_processor().unwrap(), ThreadActorConfig::default())
            .unwrap();

        let result = scheduler.schedule_with_processor(
            "test/lim/b",
            m2.clone(),
            m2.get_processor().unwrap(),
            ThreadActorConfig::default(),
        );
        assert!(result.is_err(), "thread limit must be enforced");

        scheduler.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_deschedule_unknown_actor() {
        let scheduler = DedicatedThreadScheduler::new(None);
        let result = scheduler.deschedule("nope").await;
        assert!(matches!(result, Err(SystemError::ActorNotFound(_))));
    }
}
