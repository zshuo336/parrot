//! Dedicated-thread scheduler: one OS thread per actor.

use std::collections::HashMap;
use std::error::Error;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use tokio::runtime::Handle;
use tracing::{debug, error};

use parrot_api::actor::Actor;

use crate::thread::config::ThreadActorConfig;
use crate::thread::context::ThreadContext;
use crate::thread::error::SystemError;
use crate::thread::mailbox::Mailbox;
use crate::thread::processor::ProcessorInterface;
use crate::thread::scheduler::StopHookRequest;
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
    receipts: Mutex<DedicatedReceipts>,

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
        let scheduler = Self {
            workers: Mutex::new(HashMap::new()),
            receipts: Mutex::new(DedicatedReceipts::default()),
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
        self.schedule_with_startup(path, mailbox, processor, config, || {})
    }

    fn schedule_with_startup<F>(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        processor: Arc<dyn ProcessorInterface>,
        config: ThreadActorConfig,
        startup: F,
    ) -> Result<(), SystemError>
    where
        F: FnOnce() + Send + 'static,
    {
        if self.is_shutting_down.load(Ordering::Relaxed) {
            return Err(SystemError::ShuttingDown);
        }

        self.reap_stopped();

        let mut workers = self.workers.lock().unwrap();
        // Shutdown snapshots this same map after setting the flag. Recheck
        // under its lock so a concurrent admission cannot escape that snapshot.
        if self.is_shutting_down.load(Ordering::Acquire) {
            return Err(SystemError::ShuttingDown);
        }

        if let Some(worker) = workers.get(path) {
            if worker.shutdown_flag.load(Ordering::Acquire) {
                return Err(SystemError::WorkerStateError(format!(
                    "dedicated actor {path} is still exiting"
                )));
            }
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

        worker.start_with_startup(startup).map_err(|e| {
            SystemError::ThreadSetupError(format!(
                "Failed to start dedicated thread for {}: {}",
                path, e
            ))
        })?;

        workers.insert(path.to_string(), worker);
        Ok(())
    }

    /// Deschedule an actor from its dedicated thread (stops the thread).
    pub async fn deschedule(&self, path: &str) -> Result<(), SystemError> {
        self.deschedule_until(path, tokio::time::Instant::now() + Duration::from_secs(5))
            .await
    }

    pub(crate) async fn deschedule_until(
        &self,
        path: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(), SystemError> {
        self.deschedule_generation_until(path, None, deadline).await
    }

    pub(crate) async fn deschedule_processor_until(
        &self,
        path: &str,
        processor: &Arc<dyn ProcessorInterface>,
        deadline: tokio::time::Instant,
    ) -> Result<(), SystemError> {
        self.deschedule_generation_until(path, Some(processor), deadline)
            .await
    }

    async fn deschedule_generation_until(
        &self,
        path: &str,
        processor: Option<&Arc<dyn ProcessorInterface>>,
        deadline: tokio::time::Instant,
    ) -> Result<(), SystemError> {
        let worker = {
            let workers = self.workers.lock().unwrap();
            let worker = workers
                .get(path)
                .filter(|w| processor.is_none_or(|p| Arc::ptr_eq(&w.processor, p)))
                .cloned();
            if worker.is_none() {
                // Keep the registry lock until receipt lookup. Reaping publishes
                // a generation receipt before removing that registry entry.
                return self
                    .receipts
                    .lock()
                    .unwrap()
                    .find(path, processor)
                    .map_or_else(
                        || Err(SystemError::ActorNotFound(path.into())),
                        |r| dedicated_exit(path, r),
                    );
            }
            worker.unwrap()
        };
        let result = worker.stop_until(deadline).await;
        if let Some(joined) = worker.join_result() {
            self.remove_finished(path, &worker, joined);
        }
        result
    }

    pub(crate) fn enqueue_stop_hook(&self, request: StopHookRequest) -> Result<(), SystemError> {
        let worker = self
            .workers
            .lock()
            .unwrap()
            .get(&request.path)
            .cloned()
            .ok_or_else(|| SystemError::ActorNotFound(request.path.clone()))?;
        if !Arc::ptr_eq(&worker.processor, &request.processor) {
            return Err(SystemError::ActorNotFound(request.path));
        }
        if worker.shutdown_flag.load(Ordering::Acquire) {
            return Err(SystemError::ShuttingDown);
        }
        worker
            .stop_tx
            .send(request)
            .map_err(|_| SystemError::ShutdownError("dedicated control channel closed".into()))
    }

    fn remove_finished(&self, path: &str, worker: &Arc<DedicatedWorker>, result: DedicatedExit) {
        let removed = {
            let mut workers = self.workers.lock().unwrap();
            if workers
                .get(path)
                .is_some_and(|current| Arc::ptr_eq(current, worker))
            {
                self.receipts.lock().unwrap().record(worker, result);
                workers.remove(path)
            } else {
                None
            }
        };
        // Releasing actor resources must not hold the scheduler registry lock.
        drop(removed);
    }

    /// Timed-out threads still consume capacity until the OS thread exits.
    /// Retain their join handles so later admission/shutdown can reap them.
    fn reap_stopped(&self) {
        let stopping: Vec<_> = self
            .workers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, worker)| worker.shutdown_flag.load(Ordering::Acquire))
            .map(|(path, worker)| (path.clone(), worker.clone()))
            .collect();
        for (path, worker) in stopping {
            if let Some(result) = worker.join_if_finished() {
                self.remove_finished(&path, &worker, result);
            }
        }
    }

    /// Check if an actor is scheduled.
    pub fn is_scheduled(&self, path: &str) -> bool {
        self.workers
            .lock()
            .unwrap()
            .get(path)
            .is_some_and(|worker| !worker.shutdown_flag.load(Ordering::Acquire))
    }

    /// Shutdown the scheduler and all dedicated threads.
    pub async fn shutdown(&self) -> Result<(), SystemError> {
        self.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
    }

    pub async fn shutdown_until(&self, deadline: tokio::time::Instant) -> Result<(), SystemError> {
        self.status.store(2, Ordering::SeqCst); // ShuttingDown
        self.is_shutting_down.store(true, Ordering::SeqCst);

        let workers: Vec<Arc<DedicatedWorker>> = {
            let workers = self.workers.lock().unwrap();
            workers.values().cloned().collect()
        };

        for worker in &workers {
            worker.request_stop();
        }
        let mut failure = None;
        for worker in workers {
            if let Err(error) = worker.stop_until(deadline).await {
                failure = Some(error);
            }
            if let Some(result) = worker.join_result() {
                self.remove_finished(&worker.path, &worker, result);
            }
        }
        // Reaping must not consume an error or turn repeated close into Ok.
        if failure.is_none()
            && let Some(error) = &self.receipts.lock().unwrap().first_failure
        {
            failure = Some(SystemError::Panic(error.clone()));
        }
        if self.workers.lock().unwrap().is_empty() {
            self.status.store(3, Ordering::SeqCst); // Shutdown
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Current number of dedicated workers.
    pub fn worker_count(&self) -> usize {
        self.reap_stopped();
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
        let processor = mailbox.get_processor().ok_or_else(|| {
            Box::new(SystemError::Other(anyhow::anyhow!(
                "Mailbox for actor {} has no processor attached",
                path
            ))) as Box<dyn Error + Send + Sync>
        })?;

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

type DedicatedExit = Result<(), String>;

// An independent owner completes the real OS join, even if its async waiter or
// queued blocking task is cancelled. Only framework-owned strings cross threads.
struct DedicatedJoinJob {
    handle: Option<std::thread::JoinHandle<DedicatedExit>>,
    receipt: Arc<Mutex<Option<DedicatedExit>>>,
}
impl Drop for DedicatedJoinJob {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.join()))
            .map_err(crate::owned_payload::panic_message)
            .and_then(|joined| joined.map_err(crate::owned_payload::panic_message))
            .and_then(|result| result);
        *self.receipt.lock().unwrap() = Some(result);
    }
}

#[derive(Debug)]
struct RetiredDedicated {
    path: String,
    processor: std::sync::Weak<dyn ProcessorInterface>,
    result: DedicatedExit,
}
#[derive(Default, Debug)]
struct DedicatedReceipts {
    generations: Vec<RetiredDedicated>,
    first_failure: Option<String>,
}
impl DedicatedReceipts {
    fn record(&mut self, worker: &DedicatedWorker, result: DedicatedExit) {
        // Weak identity retains no actor storage. A live caller can still read
        // its own generation after metrics or admission has reaped the worker.
        self.generations.retain(|r| r.processor.strong_count() != 0);
        if let Err(error) = &result {
            self.first_failure.get_or_insert_with(|| {
                format!("dedicated actor {} thread failed: {error}", worker.path)
            });
        }
        self.generations.push(RetiredDedicated {
            path: worker.path.clone(),
            processor: Arc::downgrade(&worker.processor),
            result,
        });
    }
    fn find(
        &self,
        path: &str,
        processor: Option<&Arc<dyn ProcessorInterface>>,
    ) -> Option<DedicatedExit> {
        self.generations
            .iter()
            .rev()
            .find(|r| {
                r.path == path && processor.is_none_or(|p| r.processor.ptr_eq(&Arc::downgrade(p)))
            })
            .map(|r| r.result.clone())
    }
}
fn dedicated_exit(path: &str, result: DedicatedExit) -> Result<(), SystemError> {
    result.map_err(|error| {
        SystemError::Panic(format!("dedicated actor {path} thread failed: {error}"))
    })
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
    shutdown_notify: Arc<tokio::sync::Notify>,

    /// Worker state
    state: Arc<AtomicUsize>,

    /// Join slot for the OS thread
    join_slot: Mutex<Option<std::thread::JoinHandle<DedicatedExit>>>,
    join_claimed: AtomicBool,
    join_receipt: Arc<Mutex<Option<DedicatedExit>>>,
    stop_tx: flume::Sender<StopHookRequest>,
    stop_rx: Mutex<Option<flume::Receiver<StopHookRequest>>>,
}

impl std::fmt::Debug for DedicatedWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DedicatedWorker")
            .field("path", &self.path)
            .field(
                "state",
                &WorkerState::from_usize(self.state.load(Ordering::Relaxed)),
            )
            .finish()
    }
}

// Own each user future outside the surrounding async state machine. Catching
// only that state machine can drop the user future during its poll unwind and
// abort the process when the future's destructor also panics.
struct DedicatedStage<T: Copy> {
    future: Option<
        parrot_api::types::BoxedFuture<'static, Result<T, crate::thread::error::SystemError>>,
    >,
    phase: &'static str,
    processor: Arc<dyn crate::thread::processor::ProcessorInterface>,
}
impl<T: Copy> DedicatedStage<T> {
    fn new(
        phase: &'static str,
        processor: Arc<dyn crate::thread::processor::ProcessorInterface>,
        construct: impl FnOnce() -> parrot_api::types::BoxedFuture<
            'static,
            Result<T, crate::thread::error::SystemError>,
        >,
    ) -> Result<Self, crate::thread::error::SystemError> {
        let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(construct)).map_err(
            |payload| {
                crate::thread::error::SystemError::Panic(format!(
                    "{phase} construction panicked: {}",
                    crate::owned_payload::panic_message(payload)
                ))
            },
        )?;
        Ok(Self {
            future: Some(future),
            phase,
            processor,
        })
    }
    fn retire(&mut self) -> Option<String> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.future.take())))
            .err()
            .map(crate::owned_payload::panic_message)
    }
}
impl<T: Copy> std::future::Future for DedicatedStage<T> {
    type Output = Result<T, crate::thread::error::SystemError>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        let owner = self.get_mut();
        let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner
                .future
                .as_mut()
                .expect("live dedicated stage")
                .as_mut()
                .poll(cx)
        }));
        let result = match polled {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => result,
            Err(payload) => Err(crate::thread::error::SystemError::Panic(format!(
                "{} panicked: {}",
                owner.phase,
                crate::owned_payload::panic_message(payload)
            ))),
        };
        if let Some(disposal) = owner.retire() {
            let message = match result {
                Ok(_) => format!("{} future disposal panicked: {disposal}", owner.phase),
                Err(error) => {
                    let receipt = crate::error_receipt::SystemReceipt::new(error);
                    format!(
                        "{}; {} future disposal panicked: {disposal}",
                        crate::error_receipt::system_summary(receipt.error()),
                        owner.phase
                    )
                }
            };
            return Poll::Ready(Err(crate::thread::error::SystemError::Panic(message)));
        }
        Poll::Ready(result)
    }
}
impl<T: Copy> Drop for DedicatedStage<T> {
    fn drop(&mut self) {
        if self.future.is_some() {
            let failure = self.retire();
            if let Some(message) = failure {
                notify_dedicated_failure(
                    &*self.processor,
                    format!("{} future disposal panicked: {message}", self.phase),
                );
            }
        }
    }
}
fn notify_dedicated_failure(
    processor: &dyn crate::thread::processor::ProcessorInterface,
    message: String,
) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        processor.notify_system_panic(message);
    })) {
        let message = crate::owned_payload::panic_message(payload);
        tracing::error!(%message, "dedicated failure notification panicked");
    }
}

fn notify_dedicated_error(
    processor: &dyn crate::thread::processor::ProcessorInterface,
    error: SystemError,
) {
    let receipt = crate::error_receipt::SystemReceipt::new(error);
    notify_dedicated_failure(
        processor,
        crate::error_receipt::system_summary(receipt.error()),
    );
}

impl DedicatedWorker {
    fn new(
        path: String,
        mailbox: Arc<dyn Mailbox>,
        processor: Arc<dyn ProcessorInterface>,
        config: ThreadActorConfig,
        batch_size: usize,
    ) -> Self {
        let (stop_tx, stop_rx) = flume::unbounded();
        Self {
            path,
            mailbox,
            processor,
            config,
            batch_size,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            state: Arc::new(AtomicUsize::new(WorkerState::Initializing as usize)),
            join_slot: Mutex::new(None),
            join_claimed: AtomicBool::new(false),
            join_receipt: Arc::new(Mutex::new(None)),
            stop_tx,
            stop_rx: Mutex::new(Some(stop_rx)),
        }
    }

    /// Spawn the dedicated OS thread.
    fn start_with_startup<F: FnOnce() + Send + 'static>(&self, startup: F) -> Result<(), String> {
        let stop_rx = self
            .stop_rx
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| "dedicated worker already started".to_owned())?;
        let path = self.path.clone();
        let mailbox = self.mailbox.clone();
        let processor = self.processor.clone();
        let shutdown_flag = self.shutdown_flag.clone();
        let shutdown_notify = self.shutdown_notify.clone();
        let state = self.state.clone();
        let batch_size = self.batch_size;
        let idle_sleep = self
            .config
            .idle_sleep_duration
            .unwrap_or(Duration::from_millis(10));
        let yield_each = self.config.yield_after_each_message.unwrap_or(false);
        let stack_size = self.config.thread_stack_size.unwrap_or(3 * 1024 * 1024);

        let builder = std::thread::Builder::new()
            .name(format!("parrot-dedicated-{}", path))
            .stack_size(stack_size);

        let completed = shutdown_flag.clone();
        let final_state = state.clone();
        let handle = builder
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                startup();
                // Single-tenant current-thread runtime for this OS thread.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("failed to create dedicated runtime for {path}: {e}"))?;

                state.store(WorkerState::Idle as usize, Ordering::Relaxed);

                rt.block_on(async move {
                    let work = async {
                        // Initialize the actor before processing messages.
                        let initialized = match DedicatedStage::new("dedicated actor initialization", processor.clone(), || processor.clone().initialize_and_start_erased()) {
                            Ok(stage) => stage.await,
                            Err(error) => Err(error),
                        };
                        if let Err(error) = initialized {
                            state.store(WorkerState::Error as usize, Ordering::Relaxed);
                            let receipt = crate::error_receipt::SystemReceipt::new(error);
                            let message = crate::error_receipt::system_summary(receipt.error());
                            error!(actor_path = %path, error = %message, "Failed to initialize dedicated actor");
                            notify_dedicated_failure(&*processor, message);
                            return;
                        }

                        while !shutdown_flag.load(Ordering::Relaxed) {
                            if mailbox.is_empty().await {
                                tokio::time::sleep(idle_sleep).await;
                                continue;
                            }

                            state.store(WorkerState::Processing as usize, Ordering::Relaxed);

                            // Poll inline so cancelling work drops the batch on
                            // this owner thread, without leaving a detached task.
                            let batch = match DedicatedStage::new("dedicated actor batch", processor.clone(), || processor.clone().process_batch_erased(mailbox.clone(), batch_size, yield_each)) {
                                Ok(stage) => stage.await,
                                Err(error) => Err(error),
                            };
                            match batch {
                                Ok((processed, errors)) => {
                                    if errors > 0 {
                                        debug!("Dedicated worker {}: processed {} messages with {} errors", path, processed, errors);
                                    }
                                }
                                Err(error) => {
                                    state.store(WorkerState::Error as usize, Ordering::Relaxed);
                                    notify_dedicated_error(&*processor, error);
                                    break;
                                }
                            }

                            if state.load(Ordering::Relaxed) != WorkerState::Error as usize {
                                state.store(WorkerState::Idle as usize, Ordering::Relaxed);
                            }
                        }
                    };
                    let work = std::panic::AssertUnwindSafe(work).catch_unwind();
                    let mut work = Box::pin(work);
                    let mut stopping = FuturesUnordered::new();
                    let mut cancellation = match super::CancellationObserver::for_stage(&*processor, "dedicated") {
                        Ok(observer) => Some(observer),
                        Err(error) => { notify_dedicated_error(&*processor, error); None }
                    };
                    while let Some(observer) = cancellation.as_mut() {
                        if shutdown_flag.load(Ordering::Acquire) { break; }
                        tokio::select! {
                            biased;
                            _ = shutdown_notify.notified() => break,
                            result = observer => {
                                if let Err(error) = result { notify_dedicated_error(&*processor, error); }
                                break;
                            },
                            request = stop_rx.recv_async() => {
                                let Ok(request) = request else { break };
                                stopping.push(request.run());
                            }
                            _ = stopping.next(), if !stopping.is_empty() => {}
                            result = &mut work => {
                                if let Err(payload) = result {
                                    let message = crate::owned_payload::panic_message(payload);
                                    notify_dedicated_failure(&*processor, format!("dedicated actor initialization panicked: {message}"));
                                }
                                break;
                            }
                        }
                    }
                    // Dispose the observer before releasing the work and actor storage.
                    if let Some(observer) = cancellation.as_mut()
                        && let Err(error) = observer.finish(Ok(())) { notify_dedicated_error(&*processor, error); }
                    drop(cancellation);
                    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| processor.cancel_processing())) {
                        let message = crate::owned_payload::panic_message(payload);
                        notify_dedicated_failure(&*processor, format!("dedicated actor cancellation panicked: {message}"));
                    }
                    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(work))) {
                        let message = crate::owned_payload::panic_message(payload);
                        notify_dedicated_failure(&*processor, format!("dedicated actor work disposal panicked: {message}"));
                    }
                    drop(stopping);
                    // No data/hook future retains actor storage past this point.
                    if let Err(error) = super::ControlWork::new(processor.clone(), true).await {
                        notify_dedicated_error(&*processor, error);
                    }
                    shutdown_flag.store(true, Ordering::Release);
                    state.store(WorkerState::ShuttingDown as usize, Ordering::Relaxed);
                });
                Ok(())
                })).map_err(crate::owned_payload::panic_message).and_then(|result| result);
                // Retire any arbitrary panic payload on its original OS thread,
                // before is_finished can release capacity or an implicit owner
                // Drop can inherit that payload.
                completed.store(true, Ordering::Release);
                if result.is_err() { final_state.store(WorkerState::Error as usize, Ordering::Release); }
                result
            })
            .map_err(|e| e.to_string())?;

        *self.join_slot.lock().unwrap() = Some(handle);
        Ok(())
    }

    fn request_stop(&self) {
        self.shutdown_flag.store(true, Ordering::Release);
        self.shutdown_notify.notify_one();
    }

    async fn stop_until(&self, deadline: tokio::time::Instant) -> Result<(), SystemError> {
        self.request_stop();
        loop {
            if let Some(result) = self.join_if_finished() {
                return dedicated_exit(&self.path, result);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SystemError::Timeout(format!(
                    "dedicated actor {} did not exit before the shutdown deadline",
                    self.path
                )));
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + Duration::from_millis(1)),
            )
            .await;
        }
    }

    fn join_result(&self) -> Option<DedicatedExit> {
        self.join_receipt.lock().unwrap().clone()
    }

    fn join_if_finished(&self) -> Option<DedicatedExit> {
        let job = {
            let mut slot = self.join_slot.lock().unwrap();
            if let Some(result) = self.join_result() {
                return Some(result);
            }
            match slot.as_ref() {
                Some(handle) if handle.is_finished() => {
                    self.join_claimed.store(true, Ordering::Release);
                    Some(DedicatedJoinJob {
                        handle: slot.take(),
                        receipt: self.join_receipt.clone(),
                    })
                }
                None if !self.join_claimed.load(Ordering::Acquire) => return Some(Ok(())),
                _ => None,
            }
        };
        if let Some(job) = job {
            // No scheduler/worker mutex and no async executor poll owns join or
            // defensive payload disposal. A waiter only observes its receipt.
            if let Ok(handle) = Handle::try_current() {
                drop(handle.spawn_blocking(move || drop(job)));
            } else {
                drop(job);
            }
        }
        self.join_result()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::actor::ThreadActor;
    use crate::thread::config::SchedulingMode;
    use crate::thread::context::ThreadContext;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use crate::thread::processor::ActorProcessor;
    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::address::ActorPath;
    use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

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

        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if msg.downcast_ref::<u64>().is_some() {
                    self.count += 1;
                    return Ok(Box::new(self.count) as BoxedMessage);
                }
                Ok(msg)
            })
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
                .push(
                    Box::new(i) as BoxedMessage,
                    crate::thread::config::BackpressureStrategy::Block,
                )
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
        assert!(
            mailbox.is_empty().await,
            "dedicated thread should drain the mailbox"
        );

        scheduler
            .deschedule("test/dedicated/counter")
            .await
            .unwrap();
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
            .schedule_with_processor(
                "test/lim/a",
                m1.clone(),
                m1.get_processor().unwrap(),
                ThreadActorConfig::default(),
            )
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

#[cfg(test)]
mod recursive_contracts {
    use super::*;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use crate::thread::processor::{ProcessorStats, ProcessorStatsTrait};
    use parrot_api::address::ActorPath;
    type BoxedProcessorFuture<T> = parrot_api::types::BoxedFuture<'static, T>;
    use std::any::Any;
    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("dedicated secondary destructor");
        }
    }
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::panic::panic_any(Secondary);
        }
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Phase {
        Init,
        BatchCreation,
        BatchPoll,
        Healthy,
        FutureDrop,
    }
    struct PendingWork {
        payload_drops: Arc<AtomicUsize>,
        polled: Arc<tokio::sync::Notify>,
    }
    impl std::future::Future for PendingWork {
        type Output = Result<(usize, usize), SystemError>;
        fn poll(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            self.polled.notify_one();
            std::task::Poll::Pending
        }
    }
    impl Drop for PendingWork {
        fn drop(&mut self) {
            std::panic::panic_any(Payload(self.payload_drops.clone()));
        }
    }
    struct Processor {
        phase: Phase,
        payload_drops: Arc<AtomicUsize>,
        retired: AtomicUsize,
        cancelled: AtomicUsize,
        initialized: tokio::sync::Notify,
        polled: Arc<tokio::sync::Notify>,
        notices: Mutex<Vec<String>>,
    }
    impl ProcessorStatsTrait for Processor {
        fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
            None
        }
    }
    impl ProcessorInterface for Processor {
        fn is_initialized(&self) -> bool {
            true
        }
        fn initialize_and_start_erased(
            self: Arc<Self>,
        ) -> BoxedProcessorFuture<Result<(), SystemError>> {
            Box::pin(async move {
                if self.phase == Phase::Init {
                    std::panic::panic_any(Payload(self.payload_drops.clone()));
                }
                self.initialized.notify_one();
                Ok(())
            })
        }
        fn stop_erased(self: Arc<Self>) -> BoxedProcessorFuture<Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
        }
        fn retire_erased(self: Arc<Self>) -> BoxedProcessorFuture<Result<(), SystemError>> {
            Box::pin(async move {
                self.retired.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
        fn process_batch_erased(
            self: Arc<Self>,
            _: Arc<dyn Mailbox>,
            _: usize,
            _: bool,
        ) -> BoxedProcessorFuture<Result<(usize, usize), SystemError>> {
            if self.phase == Phase::FutureDrop {
                return Box::pin(PendingWork {
                    payload_drops: self.payload_drops.clone(),
                    polled: self.polled.clone(),
                });
            }
            if self.phase == Phase::BatchCreation {
                std::panic::panic_any(Payload(self.payload_drops.clone()));
            }
            Box::pin(async move {
                if self.phase == Phase::BatchPoll {
                    std::panic::panic_any(Payload(self.payload_drops.clone()));
                }
                futures::future::pending().await
            })
        }
        fn cancel_processing(&self) {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
        }
        fn notify_system_panic(&self, message: String) {
            self.notices.lock().unwrap().push(message);
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
    async fn run(phase: Phase) {
        let payload_drops = Arc::new(AtomicUsize::new(0));
        let processor = Arc::new(Processor {
            phase,
            payload_drops: payload_drops.clone(),
            retired: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
            initialized: tokio::sync::Notify::new(),
            polled: Arc::new(tokio::sync::Notify::new()),
            notices: Mutex::new(Vec::new()),
        });
        let mailbox = Arc::new(MpscMailbox::new(
            4,
            ActorPath::placeholder("/dedicated-recursive"),
        ));
        mailbox
            .push(
                Box::new(1u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        let worker = DedicatedWorker::new(
            "/dedicated-recursive".into(),
            mailbox,
            processor.clone(),
            Default::default(),
            1,
        );
        worker.start_with_startup(|| {}).unwrap();
        if phase == Phase::Healthy {
            tokio::time::timeout(Duration::from_secs(3), processor.initialized.notified())
                .await
                .unwrap();
            worker.request_stop();
        }
        if phase == Phase::FutureDrop {
            tokio::time::timeout(Duration::from_secs(3), processor.polled.notified())
                .await
                .unwrap();
            worker.request_stop();
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if worker
                    .join_slot
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .is_finished()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        // Inspect the real worker's OS-thread result without allowing a hostile
        // baseline join payload to recurse while the test reports failure.
        let joined = worker.join_slot.lock().unwrap().take().unwrap().join();
        match joined {
            Ok(result) => assert!(
                result.is_ok(),
                "dedicated worker failed before retirement: {result:?}"
            ),
            Err(payload) => {
                std::mem::forget(payload);
                panic!("dedicated worker escaped panic isolation before retirement");
            }
        }
        assert_eq!(processor.retired.load(Ordering::SeqCst), 1);
        assert_eq!(processor.cancelled.load(Ordering::SeqCst), 1);
        assert!(worker.shutdown_flag.load(Ordering::Acquire));
        assert_eq!(
            worker.state.load(Ordering::Relaxed),
            WorkerState::ShuttingDown as usize
        );
        let notices = processor.notices.lock().unwrap();
        if phase == Phase::Healthy {
            assert!(notices.is_empty());
            assert_eq!(payload_drops.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(payload_drops.load(Ordering::SeqCst), 1);
            assert_eq!(notices.len(), 1);
            assert!(
                notices[0].contains("panic payload destructor panicked"),
                "{}",
                notices[0]
            );
            let stage = if phase == Phase::Init {
                "initialization"
            } else if phase == Phase::FutureDrop {
                "disposal"
            } else {
                "batch"
            };
            assert!(notices[0].contains(stage), "{}", notices[0]);
        }
    }
    #[tokio::test]
    async fn recursive_initialization_panic_preserves_retirement_and_cause() {
        run(Phase::Init).await;
    }
    #[tokio::test]
    async fn recursive_batch_creation_panic_preserves_retirement_and_cause() {
        run(Phase::BatchCreation).await;
    }
    #[tokio::test]
    async fn recursive_batch_poll_panic_preserves_retirement_and_cause() {
        run(Phase::BatchPoll).await;
    }
    #[tokio::test]
    async fn healthy_dedicated_cancellation_retires_once() {
        run(Phase::Healthy).await;
    }
    #[tokio::test]
    async fn recursive_cancelled_batch_drop_cannot_skip_retirement() {
        run(Phase::FutureDrop).await;
    }
}

#[cfg(test)]
mod stage_ownership_contracts {
    use super::*;
    use crate::thread::{
        mailbox::mpsc::MpscMailbox,
        processor::{ProcessorStats, ProcessorStatsTrait},
    };
    use parrot_api::{address::ActorPath, types::BoxedFuture};
    use std::{
        any::Any,
        future::Future,
        panic::panic_any,
        pin::Pin,
        task::{Context, Poll},
    };
    #[derive(Clone, Copy, PartialEq)]
    enum Phase {
        Init = 0,
        Batch = 1,
        Retire = 2,
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Creation,
        Poll,
        PollDrop,
        ReadyDrop,
        PendingDrop,
        Business,
        Plain,
        Healthy,
    }
    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("secondary dedicated stage payload");
        }
    }
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic_any(Secondary);
        }
    }
    #[derive(Debug)]
    struct Business {
        display: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        disposed: Arc<tokio::sync::Notify>,
    }
    impl std::fmt::Display for Business {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.display.fetch_add(1, Ordering::SeqCst);
            panic!("unexpected dedicated business formatting");
        }
    }
    impl std::error::Error for Business {}
    impl Drop for Business {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            self.disposed.notify_one();
            panic_any(Secondary);
        }
    }
    struct Processor {
        phase: Phase,
        mode: Mode,
        created: [AtomicUsize; 3],
        drops: [AtomicUsize; 3],
        cancelled: AtomicUsize,
        payload_drops: Arc<AtomicUsize>,
        display: Arc<AtomicUsize>,
        error_drops: Arc<AtomicUsize>,
        disposed: Arc<tokio::sync::Notify>,
        entered: tokio::sync::Notify,
        running: tokio::sync::Notify,
        notices: Mutex<Vec<String>>,
        threads: Mutex<Vec<(usize, std::thread::ThreadId, bool)>>,
        panic_notice: bool,
        panic_cancel: bool,
    }
    impl Processor {
        fn new(phase: Phase, mode: Mode, panic_notice: bool, panic_cancel: bool) -> Arc<Self> {
            Arc::new(Self {
                phase,
                mode,
                created: std::array::from_fn(|_| AtomicUsize::new(0)),
                drops: std::array::from_fn(|_| AtomicUsize::new(0)),
                cancelled: AtomicUsize::new(0),
                payload_drops: Arc::new(AtomicUsize::new(0)),
                display: Arc::new(AtomicUsize::new(0)),
                error_drops: Arc::new(AtomicUsize::new(0)),
                disposed: Arc::new(tokio::sync::Notify::new()),
                entered: tokio::sync::Notify::new(),
                running: tokio::sync::Notify::new(),
                notices: Mutex::new(Vec::new()),
                threads: Mutex::new(Vec::new()),
                panic_notice,
                panic_cancel,
            })
        }
        fn make<T: Copy + Send + 'static>(
            self: Arc<Self>,
            phase: Phase,
            value: T,
        ) -> BoxedFuture<'static, Result<T, SystemError>> {
            self.created[phase as usize].fetch_add(1, Ordering::SeqCst);
            if self.phase == phase && self.mode == Mode::Creation {
                panic_any(Payload(self.payload_drops.clone()));
            }
            Box::pin(StageFuture {
                processor: self,
                phase,
                value,
            })
        }
    }
    struct StageFuture<T: Copy> {
        processor: Arc<Processor>,
        phase: Phase,
        value: T,
    }
    impl<T: Copy> Future for StageFuture<T> {
        type Output = Result<T, SystemError>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            let p = &self.processor;
            p.threads.lock().unwrap().push((
                self.phase as usize,
                std::thread::current().id(),
                false,
            ));
            if self.phase == Phase::Batch {
                p.running.notify_one();
            }
            if self.phase == p.phase {
                p.entered.notify_one();
                match p.mode {
                    Mode::Poll | Mode::PollDrop => panic_any(Payload(p.payload_drops.clone())),
                    Mode::Plain => panic!("dedicated original stage cause"),
                    Mode::Business => {
                        return Poll::Ready(Err(SystemError::Other(anyhow::Error::new(
                            Business {
                                display: p.display.clone(),
                                drops: p.error_drops.clone(),
                                disposed: p.disposed.clone(),
                            },
                        ))));
                    }
                    Mode::PendingDrop => return Poll::Pending,
                    Mode::ReadyDrop => return Poll::Ready(Ok(self.value)),
                    _ => {}
                }
            }
            if self.phase == Phase::Batch {
                Poll::Pending
            } else {
                Poll::Ready(Ok(self.value))
            }
        }
    }
    impl<T: Copy> Drop for StageFuture<T> {
        fn drop(&mut self) {
            let p = &self.processor;
            p.drops[self.phase as usize].fetch_add(1, Ordering::SeqCst);
            p.threads.lock().unwrap().push((
                self.phase as usize,
                std::thread::current().id(),
                true,
            ));
            if self.phase == p.phase
                && matches!(p.mode, Mode::PollDrop | Mode::ReadyDrop | Mode::PendingDrop)
            {
                panic_any(Payload(p.payload_drops.clone()));
            }
        }
    }
    impl ProcessorStatsTrait for Processor {
        fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
            None
        }
    }
    impl ProcessorInterface for Processor {
        fn is_initialized(&self) -> bool {
            true
        }
        fn initialize_and_start_erased(
            self: Arc<Self>,
        ) -> BoxedFuture<'static, Result<(), SystemError>> {
            self.make(Phase::Init, ())
        }
        fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
        }
        fn retire_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
            self.make(Phase::Retire, ())
        }
        fn process_batch_erased(
            self: Arc<Self>,
            _: Arc<dyn Mailbox>,
            _: usize,
            _: bool,
        ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
            self.make(Phase::Batch, (0, 0))
        }
        fn cancel_processing(&self) {
            self.cancelled.fetch_add(1, Ordering::SeqCst);
            if self.panic_cancel {
                panic_any(Payload(self.payload_drops.clone()));
            }
        }
        fn notify_system_panic(&self, message: String) {
            self.notices.lock().unwrap().push(message);
            if self.panic_notice {
                panic_any(Payload(self.payload_drops.clone()));
            }
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
    async fn mailbox(path: &str) -> Arc<MpscMailbox> {
        let mailbox = Arc::new(MpscMailbox::new(4, ActorPath::placeholder(path)));
        mailbox
            .push(
                Box::new(1u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        mailbox
    }
    async fn run(phase: Phase, mode: Mode, panic_notice: bool, panic_cancel: bool) {
        let p = Processor::new(phase, mode, panic_notice, panic_cancel);
        let worker = DedicatedWorker::new(
            "/stages".into(),
            mailbox("/stages").await,
            p.clone(),
            Default::default(),
            1,
        );
        worker.start_with_startup(|| {}).unwrap();
        if phase == Phase::Retire {
            tokio::time::timeout(Duration::from_secs(3), p.running.notified())
                .await
                .unwrap();
            worker.request_stop();
        } else if matches!(mode, Mode::Healthy | Mode::PendingDrop) {
            tokio::time::timeout(Duration::from_secs(3), p.entered.notified())
                .await
                .unwrap();
            worker.request_stop();
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if worker
                    .join_slot
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .is_finished()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let handle = worker.join_slot.lock().unwrap().take().unwrap();
        match handle.join() {
            Ok(result) => assert!(
                result.is_ok(),
                "dedicated stage failed before retirement: {result:?}"
            ),
            Err(payload) => {
                std::mem::forget(payload);
                panic!("dedicated stage failure escaped real OS worker");
            }
        }
        assert_eq!(p.cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.created[Phase::Retire as usize].load(Ordering::SeqCst),
            1,
            "retirement must be attempted after earlier failures"
        );
        assert!(worker.shutdown_flag.load(Ordering::Acquire));
        assert_eq!(
            worker.state.load(Ordering::Relaxed),
            WorkerState::ShuttingDown as usize
        );
        assert_eq!(
            p.drops[phase as usize].load(Ordering::SeqCst),
            usize::from(mode != Mode::Creation)
        );
        if mode == Mode::Business {
            tokio::time::timeout(Duration::from_secs(3), p.disposed.notified())
                .await
                .unwrap();
            assert_eq!(p.error_drops.load(Ordering::SeqCst), 1);
            assert_eq!(p.display.load(Ordering::SeqCst), 0);
        }
        let notices = p.notices.lock().unwrap();
        assert_eq!(
            notices.len(),
            usize::from(mode != Mode::Healthy || panic_cancel)
        );
        if let Some(message) = notices.first() {
            if mode == Mode::Business {
                assert!(message.contains("original error retained"));
            } else if mode == Mode::Plain {
                assert!(message.contains("dedicated original stage cause"));
            } else {
                assert!(message.contains("panic payload destructor panicked"));
            }
        }
        let rows = p.threads.lock().unwrap();
        for (phase, thread, was_drop) in rows.iter() {
            if *was_drop {
                assert!(
                    rows.iter().any(|(p, t, d)| p == phase && t == thread && !d),
                    "future disposal must stay on its polling thread"
                );
            }
        }
    }
    async fn isolated(phase: Phase, mode: Mode, name: &str) {
        const CHILD: &str = "PARROT_DEDICATED_STAGE_DOUBLE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            run(phase, mode, false, false).await;
            return;
        }
        let full =
            format!("thread::scheduler::dedicated_thread::stage_ownership_contracts::{name}");
        let child = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &full, "--nocapture"])
                .env(CHILD, "1")
                .env("RUST_BACKTRACE", "0")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            child.status.success(),
            "double panic child failed: {}\n{}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
    }
    #[tokio::test]
    async fn initialization_poll_and_drop_cannot_abort() {
        isolated(
            Phase::Init,
            Mode::PollDrop,
            "initialization_poll_and_drop_cannot_abort",
        )
        .await;
    }
    #[tokio::test]
    async fn batch_poll_and_drop_cannot_abort() {
        isolated(
            Phase::Batch,
            Mode::PollDrop,
            "batch_poll_and_drop_cannot_abort",
        )
        .await;
    }
    #[tokio::test]
    async fn retirement_poll_and_drop_cannot_abort() {
        isolated(
            Phase::Retire,
            Mode::PollDrop,
            "retirement_poll_and_drop_cannot_abort",
        )
        .await;
    }
    #[tokio::test]
    async fn completed_initialization_disposal_failure_is_reported() {
        run(Phase::Init, Mode::ReadyDrop, false, false).await;
    }
    #[tokio::test]
    async fn completed_batch_disposal_failure_is_reported() {
        run(Phase::Batch, Mode::ReadyDrop, false, false).await;
    }
    #[tokio::test]
    async fn retirement_construction_recursive_panic_is_reported() {
        run(Phase::Retire, Mode::Creation, false, false).await;
    }
    #[tokio::test]
    async fn retirement_poll_recursive_panic_is_reported() {
        run(Phase::Retire, Mode::Poll, false, false).await;
    }
    #[tokio::test]
    async fn retirement_ready_disposal_failure_is_reported() {
        run(Phase::Retire, Mode::ReadyDrop, false, false).await;
    }
    #[tokio::test]
    async fn initialization_business_error_avoids_display() {
        isolated(
            Phase::Init,
            Mode::Business,
            "initialization_business_error_avoids_display",
        )
        .await;
    }
    #[tokio::test]
    async fn batch_business_error_avoids_display() {
        isolated(
            Phase::Batch,
            Mode::Business,
            "batch_business_error_avoids_display",
        )
        .await;
    }
    #[tokio::test]
    async fn retirement_business_error_avoids_display_and_unsafe_drop() {
        isolated(
            Phase::Retire,
            Mode::Business,
            "retirement_business_error_avoids_display_and_unsafe_drop",
        )
        .await;
    }
    #[tokio::test]
    async fn cancelled_initialization_retires_on_owner_thread() {
        run(Phase::Init, Mode::PendingDrop, false, false).await;
    }
    #[tokio::test]
    async fn cancelled_batch_retires_on_owner_thread() {
        run(Phase::Batch, Mode::PendingDrop, false, false).await;
    }
    #[tokio::test]
    async fn failure_notification_panic_cannot_skip_retirement() {
        run(Phase::Batch, Mode::Plain, true, false).await;
    }
    #[tokio::test]
    async fn cancellation_callback_panic_cannot_skip_retirement() {
        run(Phase::Batch, Mode::Healthy, false, true).await;
    }
    #[tokio::test]
    async fn healthy_stage_lifecycle_still_succeeds() {
        run(Phase::Batch, Mode::Healthy, false, false).await;
    }
    #[tokio::test]
    async fn failed_retirement_does_not_strand_dedicated_capacity() {
        let scheduler = DedicatedThreadScheduler::new(Some(DedicatedThreadConfig {
            max_threads: 1,
            ..Default::default()
        }));
        for (path, mode) in [("/first", Mode::ReadyDrop), ("/next", Mode::Healthy)] {
            let p = Processor::new(Phase::Retire, mode, false, false);
            scheduler
                .schedule_with_processor(path, mailbox(path).await, p.clone(), Default::default())
                .unwrap();
            tokio::time::timeout(Duration::from_secs(3), p.running.notified())
                .await
                .unwrap();
            let result = std::panic::AssertUnwindSafe(scheduler.deschedule(path))
                .catch_unwind()
                .await;
            match result {
                Ok(r) => r.unwrap(),
                Err(payload) => {
                    std::mem::forget(payload);
                    panic!("retirement failure escaped deschedule");
                }
            }
            assert_eq!(scheduler.worker_count(), 0);
            assert_eq!(
                p.notices.lock().unwrap().len(),
                usize::from(mode == Mode::ReadyDrop)
            );
        }
        scheduler.shutdown().await.unwrap();
    }
}

#[cfg(test)]
mod join_contracts {
    use super::*;
    use crate::thread::{
        error::SystemError,
        mailbox::mpsc::MpscMailbox,
        processor::{ProcessorInterface, ProcessorStats, ProcessorStatsTrait},
    };
    use parrot_api::{address::ActorPath, types::BoxedFuture};
    use std::{any::Any, panic::panic_any, sync::atomic::AtomicUsize, time::Duration};
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Plain,
        Recursive,
        Blocking,
        Reenter,
        Healthy,
    }
    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("secondary dedicated join payload");
        }
    }
    struct Payload {
        mode: Mode,
        drops: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
        gate: Option<std::sync::mpsc::Receiver<()>>,
        scheduler: std::sync::Weak<DedicatedThreadScheduler>,
        observed: Arc<AtomicUsize>,
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            if self.mode == Mode::Reenter
                && let Some(scheduler) = self.scheduler.upgrade()
            {
                self.observed
                    .store(scheduler.worker_count(), Ordering::SeqCst);
            }
            self.entered.notify_one();
            if let Some(gate) = self.gate.take() {
                let _ = gate.recv_timeout(Duration::from_secs(10));
            }
            if self.mode == Mode::Recursive {
                panic_any(Secondary);
            }
        }
    }
    struct Release(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    struct Processor {
        mode: Mode,
        entered: tokio::sync::Notify,
        payload_entered: Arc<tokio::sync::Notify>,
        drops: Arc<AtomicUsize>,
        gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        scheduler: std::sync::Weak<DedicatedThreadScheduler>,
        observed: Arc<AtomicUsize>,
    }
    impl ProcessorStatsTrait for Processor {
        fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
            None
        }
    }
    impl ProcessorInterface for Processor {
        fn is_initialized(&self) -> bool {
            true
        }
        fn initialize_and_start_erased(
            self: Arc<Self>,
        ) -> BoxedFuture<'static, Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
        }
        fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
        }
        fn process_batch_erased(
            self: Arc<Self>,
            _: Arc<dyn Mailbox>,
            _: usize,
            _: bool,
        ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
            Box::pin(std::future::pending())
        }
        fn retire_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
        }
        fn retirement(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
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
    impl Processor {
        fn fault(&self) {
            self.entered.notify_one();
            match self.mode {
                Mode::Plain => panic!("original dedicated thread failure"),
                Mode::Healthy => {}
                _ => {
                    let gate = self.gate.lock().unwrap().take();
                    panic_any(Payload {
                        mode: self.mode,
                        drops: self.drops.clone(),
                        entered: self.payload_entered.clone(),
                        gate,
                        scheduler: self.scheduler.clone(),
                        observed: self.observed.clone(),
                    });
                }
            }
        }
    }
    async fn start(mode: Mode) -> (Arc<DedicatedThreadScheduler>, Arc<Processor>, Release) {
        let scheduler = Arc::new(DedicatedThreadScheduler::new(Some(DedicatedThreadConfig {
            max_threads: 1,
            ..Default::default()
        })));
        let (tx, rx) = std::sync::mpsc::channel();
        let p = Arc::new(Processor {
            mode,
            entered: tokio::sync::Notify::new(),
            payload_entered: Arc::new(tokio::sync::Notify::new()),
            drops: Arc::new(AtomicUsize::new(0)),
            gate: Mutex::new(if mode == Mode::Blocking {
                Some(rx)
            } else {
                None
            }),
            scheduler: Arc::downgrade(&scheduler),
            observed: Arc::new(AtomicUsize::new(0)),
        });
        let mailbox = Arc::new(MpscMailbox::new(4, ActorPath::placeholder("/join")));
        mailbox.set_processor(p.clone());
        mailbox
            .push(
                Box::new(1u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        let probe = p.clone();
        scheduler
            .schedule_with_startup("/join", mailbox, p.clone(), Default::default(), move || {
                probe.fault()
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), p.entered.notified())
            .await
            .unwrap();
        (scheduler, p, Release(Some(tx)))
    }
    async fn close(s: &DedicatedThreadScheduler) -> Result<(), SystemError> {
        s.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
    }
    fn expect_failure(result: Result<(), SystemError>, text: &str) {
        let Err(SystemError::Panic(message)) = result else {
            panic!("thread failure was lost or misclassified");
        };
        assert!(message.contains(text), "missing original cause: {message}");
    }
    async fn exercise(mode: Mode, cancel: bool) {
        let (s, p, release) = start(mode).await;
        if mode == Mode::Blocking {
            let first = s.shutdown_until(tokio::time::Instant::now() + Duration::from_millis(20));
            if cancel {
                let mut first = Box::pin(first);
                assert!(futures::poll!(&mut first).is_pending());
                drop(first);
            } else {
                assert!(matches!(first.await, Err(SystemError::Timeout(_))));
            }
            // A blocking panic-payload destructor must not prevent this timer.
            tokio::time::timeout(Duration::from_secs(1), p.payload_entered.notified())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
            assert_eq!(
                s.worker_count(),
                1,
                "pending disposal still owns its thread reservation"
            );
            drop(release);
        } else {
            drop(release);
        }
        expect_failure(
            close(&s).await,
            if mode == Mode::Plain {
                "original dedicated thread failure"
            } else if mode == Mode::Recursive {
                "panic payload destructor panicked"
            } else {
                "non-string panic payload"
            },
        );
        assert_eq!(s.worker_count(), 0);
        expect_failure(close(&s).await, "thread failed");
        if mode != Mode::Plain {
            assert_eq!(p.drops.load(Ordering::SeqCst), 1);
        }
        if mode == Mode::Reenter {
            assert_eq!(p.observed.load(Ordering::SeqCst), 1);
        }
    }
    async fn isolated(mode: Mode, cancel: bool, name: &str) {
        const CHILD: &str = "PARROT_DEDICATED_JOIN_CHILD";
        if std::env::var_os(CHILD).is_some() {
            exercise(mode, cancel).await;
            return;
        }
        let name = format!("thread::scheduler::dedicated_thread::join_contracts::{name}");
        let child = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env(CHILD, "1")
                .env("RUST_BACKTRACE", "0")
                .kill_on_drop(true)
                .output(),
        )
        .await;
        let output = child
            .expect("join blocked its executor or reentered a scheduler lock")
            .unwrap();
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"),
            "join child failed: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[tokio::test]
    async fn plain_thread_panic_retains_error_for_repeated_shutdown() {
        exercise(Mode::Plain, false).await;
    }
    #[tokio::test]
    async fn recursive_join_payload_cannot_escape() {
        isolated(
            Mode::Recursive,
            false,
            "recursive_join_payload_cannot_escape",
        )
        .await;
    }
    #[tokio::test]
    async fn blocking_payload_respects_deadline_and_retains_capacity() {
        isolated(
            Mode::Blocking,
            false,
            "blocking_payload_respects_deadline_and_retains_capacity",
        )
        .await;
    }
    #[tokio::test]
    async fn cancelled_shutdown_waiter_does_not_discard_join_failure() {
        isolated(
            Mode::Blocking,
            true,
            "cancelled_shutdown_waiter_does_not_discard_join_failure",
        )
        .await;
    }
    #[tokio::test]
    async fn payload_disposal_can_reenter_scheduler_metrics() {
        isolated(
            Mode::Reenter,
            false,
            "payload_disposal_can_reenter_scheduler_metrics",
        )
        .await;
    }
    #[tokio::test]
    async fn concurrent_shutdown_callers_share_actual_failure() {
        let (s, _, _release) = start(Mode::Plain).await;
        let (a, b) = tokio::join!(close(&s), close(&s));
        expect_failure(a, "original dedicated thread failure");
        expect_failure(b, "original dedicated thread failure");
        assert_eq!(s.worker_count(), 0);
    }
    #[tokio::test]
    async fn healthy_shutdown_still_completes() {
        let (s, _, _release) = start(Mode::Healthy).await;
        close(&s).await.unwrap();
        close(&s).await.unwrap();
        assert_eq!(s.worker_count(), 0);
    }

    async fn finished(s: &DedicatedThreadScheduler) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let worker = s.workers.lock().unwrap().get("/join").cloned().unwrap();
                if worker
                    .join_slot
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|h| h.is_finished())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn completed_worker_payload_is_safe_when_owner_is_dropped() {
        const CHILD: &str = "PARROT_DEDICATED_DROP_OWNER_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output=tokio::time::timeout(Duration::from_secs(5),tokio::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","thread::scheduler::dedicated_thread::join_contracts::completed_worker_payload_is_safe_when_owner_is_dropped","--nocapture"]).env(CHILD,"1").env("RUST_BACKTRACE","0").kill_on_drop(true).output()).await.unwrap().unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"),
                "owner drop failed: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let (s, p, _release) = start(Mode::Recursive).await;
        finished(&s).await;
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(s))) {
            std::mem::forget(payload);
            panic!("owner inherited arbitrary thread panic payload");
        }
        assert_eq!(p.drops.load(Ordering::SeqCst), 1);
    }
    async fn reap(s: &DedicatedThreadScheduler) {
        s.workers
            .lock()
            .unwrap()
            .get("/join")
            .unwrap()
            .request_stop();
        finished(s).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while s.worker_count() != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn metrics_reaping_preserves_global_failure() {
        let (s, _p, _release) = start(Mode::Plain).await;
        reap(&s).await;
        expect_failure(close(&s).await, "original dedicated thread failure");
        expect_failure(close(&s).await, "original dedicated thread failure");
    }
    #[tokio::test]
    async fn reaped_generation_deschedule_retains_failure() {
        let (s, _p, _release) = start(Mode::Plain).await;
        reap(&s).await;
        expect_failure(
            s.deschedule("/join").await,
            "original dedicated thread failure",
        );
        expect_failure(
            s.deschedule("/join").await,
            "original dedicated thread failure",
        );
    }
    fn group(s: Arc<DedicatedThreadScheduler>) -> super::super::SchedulerGroup {
        let mut group = super::super::ThreadSchedulerFactory::new(Handle::current())
            .create_scheduler_group(None, None);
        group.dedicated_scheduler = s;
        group
    }
    #[tokio::test]
    async fn lifecycle_retirement_returns_original_join_failure() {
        let (s, p, _release) = start(Mode::Plain).await;
        let g = group(s.clone());
        let result = g
            .retire_actor(
                "/join",
                &crate::thread::config::SchedulingMode::DedicatedThread,
                p,
                Arc::new(tokio::sync::Semaphore::new(1)),
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await;
        g.shared_scheduler.shutdown(2000).await.unwrap();
        expect_failure(result, "original dedicated thread failure");
        assert_eq!(s.worker_count(), 0);
    }
    #[tokio::test]
    async fn old_generation_retirement_cannot_stop_same_path_replacement() {
        let (s, old, _release) = start(Mode::Plain).await;
        reap(&s).await;
        let new = Arc::new(Processor {
            mode: Mode::Healthy,
            entered: tokio::sync::Notify::new(),
            payload_entered: Arc::new(tokio::sync::Notify::new()),
            drops: Arc::new(AtomicUsize::new(0)),
            gate: Mutex::new(None),
            scheduler: Arc::downgrade(&s),
            observed: Arc::new(AtomicUsize::new(0)),
        });
        // The processor is reusable in this custom fixture. The new scheduler
        // registration is a separate generation with the identical actor path.
        let mailbox = Arc::new(MpscMailbox::new(4, ActorPath::placeholder("/join")));
        s.schedule_with_processor("/join", mailbox, new.clone(), Default::default())
            .unwrap();
        let g = group(s.clone());
        let result = g
            .retire_actor(
                "/join",
                &crate::thread::config::SchedulingMode::DedicatedThread,
                old,
                Arc::new(tokio::sync::Semaphore::new(1)),
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await;
        let replacement_alive = s.is_scheduled("/join");
        let replacement_stop = s.deschedule("/join").await;
        g.shared_scheduler.shutdown(2000).await.unwrap();
        expect_failure(result, "original dedicated thread failure");
        assert!(
            replacement_alive,
            "old retirement stopped the replacement generation"
        );
        replacement_stop.unwrap();
        expect_failure(close(&s).await, "original dedicated thread failure");
    }
}

#[cfg(test)]
mod join_owner_contracts {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Release(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[test]
    fn cancelled_queued_join_publishes_only_after_real_thread_exit() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (pool_tx, pool_rx) = std::sync::mpsc::channel();
            let pool_release = Release(Some(pool_tx));
            let (started, entered) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                let _ = pool_rx.recv();
            });
            entered.await.unwrap();
            let (actor_tx, actor_rx) = std::sync::mpsc::channel();
            let actor_release = Release(Some(actor_tx));
            let exited = Arc::new(AtomicBool::new(false));
            let flag = exited.clone();
            let handle = std::thread::spawn(move || {
                let _ = actor_rx.recv();
                flag.store(true, Ordering::Release);
                Err("late thread failure".into())
            });
            let report = Arc::new(Mutex::new(None));
            let job = DedicatedJoinJob {
                handle: Some(handle),
                receipt: report.clone(),
            };
            let queued = tokio::task::spawn_blocking(move || drop(job));
            queued.abort();
            drop(pool_release);
            assert!(report.lock().unwrap().clone().is_none());
            assert!(!exited.load(Ordering::Acquire));
            drop(actor_release);
            assert!(queued.await.unwrap_err().is_cancelled());
            blocker.await.unwrap();
            assert!(exited.load(Ordering::Acquire));
            assert_eq!(
                report.lock().unwrap().clone(),
                Some(Err("late thread failure".into()))
            );
        });
    }
    #[test]
    fn unexpected_join_payload_is_owned_through_recursive_disposal() {
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("join payload destructor");
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let count = drops.clone();
        let handle = std::thread::spawn(move || -> DedicatedExit {
            std::panic::panic_any(Payload(count));
        });
        let report = Arc::new(Mutex::new(None));
        drop(DedicatedJoinJob {
            handle: Some(handle),
            receipt: report.clone(),
        });
        let error = report.lock().unwrap().clone().unwrap().unwrap_err();
        assert!(error.contains("panic payload destructor panicked"));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(report.lock().unwrap().clone(), Some(Err(error)));
    }
}

#[cfg(test)]
mod cancellation_contracts {
    use super::*;
    use crate::thread::{
        error::SystemError,
        mailbox::mpsc::MpscMailbox,
        processor::{ProcessorInterface, ProcessorStats, ProcessorStatsTrait},
    };
    use parrot_api::{address::ActorPath, types::BoxedFuture};
    use std::{
        any::Any,
        fmt,
        future::Future,
        pin::Pin,
        sync::atomic::AtomicUsize,
        task::{Context, Poll},
        time::Duration,
    };
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Construct,
        Plain,
        Poll,
        PollDrop,
        ReadyDrop,
        PendingDrop,
        Ready,
        Healthy,
    }
    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("secondary dedicated observer payload");
        }
    }
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::panic::panic_any(Secondary);
        }
    }
    struct Processor {
        mode: Mode,
        batch_ready: bool,
        business: bool,
        panic_notice: bool,
        cancel_calls: AtomicUsize,
        retired: AtomicUsize,
        retired_on: Mutex<Option<std::thread::ThreadId>>,
        created: AtomicUsize,
        drops: AtomicUsize,
        payloads: Arc<AtomicUsize>,
        batch_created: AtomicUsize,
        batch_drops: AtomicUsize,
        notices: Mutex<Vec<String>>,
        entered: tokio::sync::Notify,
        batch_entered: tokio::sync::Notify,
        completed: tokio::sync::Notify,
        cancelled: AtomicBool,
        waker: futures::task::AtomicWaker,
        constructed_on: Mutex<Option<std::thread::ThreadId>>,
        polled_on: Mutex<Option<std::thread::ThreadId>>,
        dropped_on: Mutex<Option<std::thread::ThreadId>>,
        display: Arc<AtomicUsize>,
        error_drops: Arc<AtomicUsize>,
        error_disposed: Arc<tokio::sync::Notify>,
    }
    impl Processor {
        fn new(mode: Mode, batch_ready: bool, business: bool, panic_notice: bool) -> Arc<Self> {
            Arc::new(Self {
                mode,
                batch_ready,
                business,
                panic_notice,
                cancel_calls: AtomicUsize::new(0),
                retired: AtomicUsize::new(0),
                retired_on: Mutex::new(None),
                created: AtomicUsize::new(0),
                drops: AtomicUsize::new(0),
                payloads: Arc::new(AtomicUsize::new(0)),
                batch_created: AtomicUsize::new(0),
                batch_drops: AtomicUsize::new(0),
                notices: Mutex::new(Vec::new()),
                entered: tokio::sync::Notify::new(),
                batch_entered: tokio::sync::Notify::new(),
                completed: tokio::sync::Notify::new(),
                cancelled: AtomicBool::new(false),
                waker: futures::task::AtomicWaker::new(),
                constructed_on: Mutex::new(None),
                polled_on: Mutex::new(None),
                dropped_on: Mutex::new(None),
                display: Arc::new(AtomicUsize::new(0)),
                error_drops: Arc::new(AtomicUsize::new(0)),
                error_disposed: Arc::new(tokio::sync::Notify::new()),
            })
        }
    }
    #[derive(Debug)]
    struct Business {
        display: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        disposed: Arc<tokio::sync::Notify>,
    }
    impl fmt::Display for Business {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.display.fetch_add(1, Ordering::SeqCst);
            panic!("business Display must not run");
        }
    }
    impl std::error::Error for Business {}
    impl Drop for Business {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            self.disposed.notify_one();
        }
    }
    struct Observer<'a>(&'a Processor);
    impl Future for Observer<'_> {
        type Output = ();
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let p = self.0;
            *p.polled_on.lock().unwrap() = Some(std::thread::current().id());
            p.waker.register(cx.waker());
            match p.mode {
                Mode::Plain => panic!("original dedicated observer failure"),
                Mode::Poll | Mode::PollDrop => std::panic::panic_any(Payload(p.payloads.clone())),
                Mode::Ready | Mode::ReadyDrop => Poll::Ready(()),
                _ if p.cancelled.load(Ordering::Acquire) => Poll::Ready(()),
                _ => Poll::Pending,
            }
        }
    }
    impl Drop for Observer<'_> {
        fn drop(&mut self) {
            let p = self.0;
            p.drops.fetch_add(1, Ordering::SeqCst);
            *p.dropped_on.lock().unwrap() = Some(std::thread::current().id());
            if matches!(p.mode, Mode::PollDrop | Mode::ReadyDrop | Mode::PendingDrop) {
                std::panic::panic_any(Payload(p.payloads.clone()));
            }
        }
    }
    struct Batch {
        processor: Arc<Processor>,
        future: BoxedFuture<'static, Result<(usize, usize), SystemError>>,
    }
    impl Future for Batch {
        type Output = Result<(usize, usize), SystemError>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.processor.batch_entered.notify_one();
            self.future.as_mut().poll(cx)
        }
    }
    impl Drop for Batch {
        fn drop(&mut self) {
            self.processor.batch_drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl ProcessorStatsTrait for Processor {
        fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
            None
        }
    }
    impl ProcessorInterface for Processor {
        fn is_initialized(&self) -> bool {
            true
        }
        fn initialize_and_start_erased(
            self: Arc<Self>,
        ) -> BoxedFuture<'static, Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
        }
        fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
        }
        fn process_batch_erased(
            self: Arc<Self>,
            mailbox: Arc<dyn Mailbox>,
            _: usize,
            _: bool,
        ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
            self.batch_created.fetch_add(1, Ordering::SeqCst);
            let p = self.clone();
            let future = Box::pin(async move {
                if !p.batch_ready {
                    std::future::pending::<()>().await;
                }
                assert!(mailbox.pop().await.is_some());
                p.completed.notify_one();
                if p.business {
                    Err(SystemError::Other(anyhow::Error::new(Business {
                        display: p.display.clone(),
                        drops: p.error_drops.clone(),
                        disposed: p.error_disposed.clone(),
                    })))
                } else {
                    Ok((1, 0))
                }
            });
            Box::pin(Batch {
                processor: self,
                future,
            })
        }
        fn processing_cancelled(&self) -> BoxedFuture<'_, ()> {
            self.created.fetch_add(1, Ordering::SeqCst);
            *self.constructed_on.lock().unwrap() = Some(std::thread::current().id());
            self.entered.notify_one();
            if self.mode == Mode::Construct {
                std::panic::panic_any(Payload(self.payloads.clone()));
            }
            Box::pin(Observer(self))
        }
        fn cancel_processing(&self) {
            self.cancel_calls.fetch_add(1, Ordering::SeqCst);
            self.cancelled.store(true, Ordering::Release);
            self.waker.wake();
        }
        fn notify_system_panic(&self, message: String) {
            self.notices.lock().unwrap().push(message);
            if self.panic_notice {
                std::panic::panic_any(Payload(self.payloads.clone()));
            }
        }
        fn retire_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
            Box::pin(async move {
                assert_eq!(
                    self.drops.load(Ordering::SeqCst),
                    usize::from(self.mode != Mode::Construct),
                    "actor retired before observer"
                );
                assert_eq!(
                    self.batch_created.load(Ordering::SeqCst),
                    self.batch_drops.load(Ordering::SeqCst),
                    "actor retired before batch"
                );
                self.retired.fetch_add(1, Ordering::SeqCst);
                *self.retired_on.lock().unwrap() = Some(std::thread::current().id());
                Ok(())
            })
        }
        fn retirement(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
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
    async fn run(
        mode: Mode,
        batch_ready: bool,
        business: bool,
        panic_notice: bool,
        shutdown: bool,
    ) {
        let s = DedicatedThreadScheduler::new(Some(DedicatedThreadConfig {
            max_threads: 1,
            ..Default::default()
        }));
        let p = Processor::new(mode, batch_ready, business, panic_notice);
        let mailbox = Arc::new(MpscMailbox::new(4, ActorPath::placeholder("/observer")));
        mailbox.set_processor(p.clone());
        mailbox
            .push(
                Box::new(1u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        s.schedule_with_processor("/observer", mailbox, p.clone(), Default::default())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), p.entered.notified())
            .await
            .unwrap();
        if matches!(mode, Mode::Healthy | Mode::PendingDrop) && !business {
            tokio::time::timeout(Duration::from_secs(2), p.batch_entered.notified())
                .await
                .unwrap();
            if batch_ready {
                tokio::time::timeout(Duration::from_secs(2), p.completed.notified())
                    .await
                    .unwrap();
            }
            if shutdown {
                s.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
                    .await
                    .expect("actor observer fault escaped as OS thread failure");
            } else {
                p.cancelled.store(true, Ordering::Release);
                p.waker.wake();
            }
        }
        // Wait for actual thread exit, without causing the failure under test by shutdown.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let done = {
                    let workers = s.workers.lock().unwrap();
                    workers.get("/observer").is_none_or(|w| {
                        w.join_slot
                            .lock()
                            .unwrap()
                            .as_ref()
                            .is_none_or(|h| h.is_finished())
                    })
                };
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        s.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .expect("actor observer fault escaped as OS thread failure");
        assert_eq!(s.worker_count(), 0);
        assert_eq!(
            p.created.load(Ordering::SeqCst),
            1,
            "observer must survive select loop iterations"
        );
        assert_eq!(
            p.drops.load(Ordering::SeqCst),
            usize::from(mode != Mode::Construct)
        );
        assert_eq!(p.cancel_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.retired.load(Ordering::SeqCst),
            1,
            "observer failure skipped actor retirement"
        );
        let batch = usize::from(matches!(mode, Mode::PendingDrop | Mode::Healthy));
        assert_eq!(p.batch_created.load(Ordering::SeqCst), batch);
        assert_eq!(p.batch_drops.load(Ordering::SeqCst), batch);
        let failing = !matches!(mode, Mode::Healthy | Mode::Ready);
        {
            let notices = p.notices.lock().unwrap();
            assert_eq!(notices.len(), usize::from(failing) + usize::from(business));
            if failing {
                assert!(
                    notices
                        .iter()
                        .any(|m| m.contains("dedicated cancellation observer"))
                );
            }
            if mode == Mode::Plain {
                assert!(notices[0].contains("original dedicated observer failure"));
            }
        }
        assert_eq!(
            p.payloads.load(Ordering::SeqCst),
            match mode {
                Mode::PollDrop => 2,
                Mode::Construct | Mode::Poll | Mode::ReadyDrop | Mode::PendingDrop => 1,
                _ => 0,
            } + usize::from(panic_notice)
        );
        assert_eq!(
            *p.constructed_on.lock().unwrap(),
            *p.retired_on.lock().unwrap()
        );
        if mode != Mode::Construct {
            assert_eq!(
                *p.constructed_on.lock().unwrap(),
                *p.polled_on.lock().unwrap()
            );
            assert_eq!(*p.polled_on.lock().unwrap(), *p.dropped_on.lock().unwrap());
        }
        if business {
            tokio::time::timeout(Duration::from_secs(2), p.error_disposed.notified())
                .await
                .unwrap();
            assert_eq!(p.error_drops.load(Ordering::SeqCst), 1);
            assert_eq!(p.display.load(Ordering::SeqCst), 0);
        }
    }
    #[tokio::test]
    async fn observer_construction_failure_preserves_retirement() {
        run(Mode::Construct, false, false, false, false).await;
    }
    #[tokio::test]
    async fn observer_poll_failure_preserves_cause_and_retirement() {
        run(Mode::Plain, false, false, false, false).await;
    }
    #[tokio::test]
    async fn recursive_observer_payload_stays_on_owner_thread() {
        run(Mode::Poll, false, false, false, false).await;
    }
    #[tokio::test]
    async fn observer_poll_and_drop_cannot_abort() {
        const CHILD: &str = "PARROT_DEDICATED_OBSERVER_DOUBLE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            run(Mode::PollDrop, false, false, false, false).await;
            return;
        }
        let output=tokio::time::timeout(Duration::from_secs(5),tokio::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","thread::scheduler::dedicated_thread::cancellation_contracts::observer_poll_and_drop_cannot_abort","--nocapture"]).env(CHILD,"1").env("RUST_BACKTRACE","0").kill_on_drop(true).output()).await.unwrap().unwrap();
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"),
            "observer child failed: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[tokio::test]
    async fn completed_observer_disposal_failure_preserves_retirement() {
        run(Mode::ReadyDrop, false, false, false, false).await;
    }
    #[tokio::test]
    async fn losing_observer_disposal_is_reported_after_business_failure() {
        run(Mode::PendingDrop, true, true, false, false).await;
    }
    #[tokio::test]
    async fn cancelled_observer_drops_pending_batch_on_owner_thread() {
        run(Mode::PendingDrop, false, false, false, false).await;
    }
    #[tokio::test]
    async fn shutdown_disposes_pending_observer_and_retires_actor() {
        run(Mode::PendingDrop, false, false, false, true).await;
    }
    #[tokio::test]
    async fn observer_failure_notification_cannot_skip_retirement() {
        run(Mode::Plain, false, false, true, false).await;
    }
    #[tokio::test]
    async fn cancelled_observer_disposal_notification_is_protected() {
        run(Mode::PendingDrop, false, false, true, true).await;
    }
    #[tokio::test]
    async fn healthy_ready_observer_retires_without_processing() {
        run(Mode::Ready, false, false, false, false).await;
    }
    #[tokio::test]
    async fn healthy_pending_observer_allows_message_completion() {
        run(Mode::Healthy, true, false, false, true).await;
    }
    #[tokio::test]
    async fn healthy_cancellation_drops_pending_batch_and_retires() {
        run(Mode::Healthy, false, false, false, false).await;
    }
}
