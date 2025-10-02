use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio::time;
use tracing::{debug, error, warn};

use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::queue::SchedulingQueue;
use crate::thread::scheduler::shared::worker_manager::WorkerManager;
use crate::thread::processor::ProcessorInterface;
use parrot_api::types::BoxedMessage;

/// Worker status codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatus {
    /// Worker is idle, waiting for work
    Idle = 0,

    /// Worker is processing messages
    Processing = 1,

    /// Worker is shutting down
    ShuttingDown = 2,

    /// Worker has encountered an error
    Error = 3,
}

impl WorkerStatus {
    pub fn from_usize(v: usize) -> Self {
        match v {
            0 => WorkerStatus::Idle,
            1 => WorkerStatus::Processing,
            2 => WorkerStatus::ShuttingDown,
            _ => WorkerStatus::Error,
        }
    }
}

/// Configuration for a worker
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Maximum number of messages to process in one batch
    pub batch_size: usize,

    /// Duration to sleep when idle before checking for work again
    pub idle_sleep_duration: Duration,

    /// Whether to yield to the scheduler after processing each message
    pub yield_after_each_message: bool,

    /// Whether to log detailed processing metrics
    pub enable_detailed_logging: bool,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            batch_size: 10,
            idle_sleep_duration: Duration::from_millis(10),
            yield_after_each_message: false,
            enable_detailed_logging: false,
        }
    }
}

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker")
            .field("id", &self.id)
            .field("batch_size", &self.batch_size)
            .field("status", &self.status.load(Ordering::Relaxed))
            .finish()
    }
}

/// A worker in the shared thread pool.
///
/// The worker loop pulls mailboxes from the [`SchedulingQueue`] and runs a
/// batch of messages through the mailbox's attached processor
/// (`Arc<dyn ProcessorInterface>`). If the mailbox still has messages after
/// the batch, it is re-queued for further processing.
pub struct Worker {
    /// Unique worker id
    id: usize,

    /// Tokio runtime handle
    runtime_handle: Handle,

    /// Queue of ready mailboxes
    scheduling_queue: Arc<SchedulingQueue>,

    /// Shutdown flag shared with the pool
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,

    /// Batch size
    batch_size: usize,

    /// Worker status
    status: Arc<AtomicUsize>,

    /// Worker configuration
    config: WorkerConfig,

    /// Mailbox panic counters (path -> consecutive panics)
    panic_counts: Arc<Mutex<HashMap<String, usize>>>,
}

impl Worker {
    /// Creates a new worker with the specified parameters
    ///
    /// ## Parameters
    /// - `id`: Unique identifier for this worker
    /// - `runtime_handle`: Tokio runtime handle for async operations
    /// - `scheduling_queue`: Queue providing mailboxes with messages to process
    /// - `shutdown_flag`: Signal for worker shutdown
    /// - `config`: Worker configuration
    pub fn new(
        id: usize,
        runtime_handle: Handle,
        scheduling_queue: Arc<SchedulingQueue>,
        shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
        config: WorkerConfig,
    ) -> Self {
        let batch_size = config.batch_size;

        Self {
            id,
            runtime_handle,
            scheduling_queue,
            shutdown_flag,
            batch_size,
            status: Arc::new(AtomicUsize::new(WorkerStatus::Idle as usize)),
            config,
            panic_counts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Launches the worker's main loop as a Tokio task
    pub fn spawn(self) -> JoinHandle<()> {
        let runtime_handle = self.runtime_handle.clone();
        runtime_handle.spawn(async move {
            self.run_loop().await;
        })
    }

    /// Main worker loop
    async fn run_loop(&self) {
        let worker_name = format!("worker-{}", self.id);

        while !self.shutdown_flag.load(Ordering::Relaxed) {
            match self.scheduling_queue.try_pop() {
                Some(mailbox) => {
                    self.status.store(WorkerStatus::Processing as usize, Ordering::Relaxed);
                    if let Err(e) = self.process_mailbox(mailbox, self.batch_size).await {
                        error!("[{}] {}", worker_name, e);
                    }
                    self.status.store(WorkerStatus::Idle as usize, Ordering::Relaxed);
                }
                None => {
                    self.status.store(WorkerStatus::Idle as usize, Ordering::Relaxed);

                    // Wait for notification or periodic shutdown check.
                    // Enable the notify permit before selecting so a push
                    // that happens between try_pop and notified() is not missed.
                    let notify = self.scheduling_queue.notify_handle();
                    let notified = notify.notified();
                    tokio::pin!(notified);

                    tokio::select! {
                        _ = &mut notified => {
                            // Work may be available
                        },
                        _ = time::sleep(self.config.idle_sleep_duration) => {
                            // Periodic check for shutdown
                        }
                    }
                }
            }
        }

        self.status.store(WorkerStatus::ShuttingDown as usize, Ordering::Relaxed);
    }

    /// Process a batch of messages from a mailbox using its attached processor.
    ///
    /// Re-queues the mailbox while it still holds messages.
    async fn process_mailbox(
        &self,
        mailbox: Arc<dyn Mailbox>,
        max_messages: usize,
    ) -> anyhow::Result<()> {
        let actor_path = mailbox.path().path.clone();
        let worker_name = format!("worker-{}", self.id);

        let processor = match mailbox.get_processor() {
            Some(p) => p,
            None => {
                warn!(
                    "[{}] Mailbox for actor {} has no processor attached; skipping",
                    worker_name, actor_path
                );
                return Ok(());
            }
        };

        // First schedule: initialize the actor before processing messages.
        if !processor.is_initialized() {
            let init_processor = processor.clone();
            if let Err(e) = init_processor.initialize_and_start_erased().await {
                error!(
                    "[{}] Failed to initialize actor {}: {}",
                    worker_name, actor_path, e
                );
                return Err(anyhow::anyhow!(e.to_string()));
            }
        }

        // Process a batch under panic isolation.
        let batch_result = {
            let processor = processor.clone();
            let mailbox_for_panics = mailbox.clone();
            let path_for_panics = actor_path.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Only the future *creation* happens here; actual actor code
                // runs when awaited below. Panics inside the future are caught
                // by the JoinHandle of the spawned inner task.
                processor.process_batch_erased(
                    mailbox_for_panics.clone(),
                    max_messages,
                    false,
                )
            }));
            match result {
                Ok(fut) => {
                    // Execute on a spawned task so panics surface as JoinError.
                    let task = tokio::spawn(fut);
                    match task.await {
                        Ok(inner) => inner,
                        Err(join_err) => Err(crate::thread::error::SystemError::WorkerStateError(
                            format!("Panic in actor {}: {}", path_for_panics, join_err),
                        )),
                    }
                }
                Err(panic_err) => Err(crate::thread::error::SystemError::WorkerStateError(
                    panic_message(panic_err, &path_for_panics),
                )),
            }
        };

        match batch_result {
            Ok((processed, errors)) => {
                if self.config.enable_detailed_logging {
                    debug!(
                        "[{}] Processed {} messages ({} errors) for {}",
                        worker_name, processed, errors, actor_path
                    );
                }
            }
            Err(e) => {
                error!("[{}] Error processing batch for {}: {}", worker_name, actor_path, e);
                // Release the schedule slot so future pushes can re-enqueue.
                mailbox.schedule_state().force_release();
                // Do not re-queue a panicking mailbox.
                self.record_panic(&actor_path);
                return Ok(());
            }
        }

        // Re-queue while there is more work, preserving the single-owner
        // invariant through the schedule slot state machine.
        let has_more = mailbox.has_more_messages().await;
        if mailbox.schedule_state().release(has_more) {
            self.scheduling_queue.push(mailbox);
        }

        Ok(())
    }

    /// Record a panic for a mailbox path (kept for metrics/future supervision).
    fn record_panic(&self, path: &str) {
        let mut counts = self.panic_counts.lock().unwrap();
        *counts.entry(path.to_string()).or_insert(0) += 1;
    }

    /// Returns a reference to the worker's status
    pub fn status(&self) -> Arc<AtomicUsize> {
        self.status.clone()
    }

    /// Get the current status of the worker
    pub fn get_status(&self) -> WorkerStatus {
        WorkerStatus::from_usize(self.status.load(Ordering::Relaxed))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::scheduler::queue::SchedulingQueue;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use crate::thread::config::ThreadActorConfig;
    use crate::thread::context::ThreadContext;
    use crate::thread::actor::ThreadActor;
    use crate::thread::processor::ActorProcessor;
    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::address::{ActorPath, ActorRef};
    use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, WeakActorTarget};
    use async_trait::async_trait;
    use std::time::Duration;

    #[derive(Debug)]
    struct EchoActor;

    impl Actor for EchoActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(msg) })
        }

        fn receive_message_with_engine<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context, _engine_ctx: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
            None
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    fn mock_actor_path(path: &str) -> ActorPath {
        let path_owned = path.to_string();
        #[derive(Debug)]
        struct MockRef(String);
        #[async_trait]
        impl ActorRef for MockRef {
            fn send<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async { Ok(Box::new(()) as BoxedMessage) })
            }
            fn send_with_timeout<'a>(&'a self, _msg: BoxedMessage, _t: Option<Duration>) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async { Ok(Box::new(()) as BoxedMessage) })
            }
            fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async { Ok(()) })
            }
            fn path(&self) -> String {
                self.0.clone()
            }
            fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
                Box::pin(async { true })
            }
            fn clone_boxed(&self) -> BoxedActorRef {
                Box::new(Self(self.0.clone()))
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        ActorPath {
            path: path.to_string(),
            target: Arc::new(MockRef(path_owned)) as WeakActorTarget,
        }
    }

    #[tokio::test]
    async fn test_worker_processes_mailbox_batch() {
        let queue = Arc::new(SchedulingQueue::new(100));
        let rt = Handle::current();
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = Worker::new(
            0,
            rt,
            queue.clone(),
            shutdown,
            WorkerConfig { batch_size: 4, ..Default::default() },
        );

        // Build a mailbox with an attached processor
        let path = mock_actor_path("test/echo");
        let mailbox = Arc::new(MpscMailbox::new(16, path));
        let context = ThreadContext::new_for_test("test/echo");
        let processor = Arc::new(ActorProcessor::<EchoActor>::new(
            ThreadActor::new_for_test(EchoActor),
            context,
            "test/echo".to_string(),
            ThreadActorConfig::default(),
        ));
        mailbox.set_processor(processor);

        // Enqueue messages
        for i in 0..3 {
            mailbox
                .push(Box::new(format!("m{}", i)) as BoxedMessage, crate::thread::config::BackpressureStrategy::Block)
                .await
                .unwrap();
        }

        // Process one batch manually
        worker.process_mailbox(mailbox.clone(), 4).await.unwrap();

        // All messages should be consumed
        assert!(mailbox.is_empty().await);
    }

    #[tokio::test]
    async fn test_worker_skips_mailbox_without_processor() {
        let queue = Arc::new(SchedulingQueue::new(100));
        let rt = Handle::current();
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = Worker::new(0, rt, queue.clone(), shutdown, WorkerConfig::default());

        let path = mock_actor_path("test/no-proc");
        let mailbox = Arc::new(MpscMailbox::new(16, path));
        mailbox
            .push(Box::new("x") as BoxedMessage, crate::thread::config::BackpressureStrategy::Block)
            .await
            .unwrap();

        // Should not error, just skip
        worker.process_mailbox(mailbox.clone(), 4).await.unwrap();
        // Message remains (no processor)
        assert!(!mailbox.is_empty().await);
    }

    /// ADR-4 feasibility probe: does `batch_size` (default 10) amplify
    /// head-of-line latency for a single actor?
    ///
    /// Setup: one mailbox pre-loaded with a backlog of ~20µs messages plus
    /// one late probe; drain with batch sizes 1 / 10 / 50 and compare
    /// total drain time. Within one actor, messages are serialized
    /// regardless of batching, so the expectation is near-identical drain
    /// times — documenting that ADR-4's cost is code complexity, not
    /// single-actor HOL amplification. (Cross-actor fairness is the real
    /// lever of the knob; see the shared-pool stress scenarios.)
    #[derive(Debug, Default)]
    struct SpinActor {
        sink: u64,
    }

    impl Actor for SpinActor {
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
                let spin = std::time::Instant::now();
                while spin.elapsed().as_micros() < 20 {}
                if msg.downcast_ref::<u64>().is_some() {
                    self.sink += 1;
                }
                Ok(msg)
            })
        }

        fn receive_message_with_engine<'a>(
            &'a mut self,
            _msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
            _engine_ctx: parrot_api::actor::EngineContextHandle,
        ) -> Option<ActorResult<BoxedMessage>> {
            None
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    async fn drain_with_batch(batch: usize, backlog: usize) -> std::time::Duration {
        let worker = Worker::new(
            0,
            Handle::current(),
            Arc::new(SchedulingQueue::new(1000)),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            WorkerConfig {
                batch_size: batch,
                ..Default::default()
            },
        );
        let path = mock_actor_path(&format!("bench/b{}", batch));
        let mailbox = Arc::new(MpscMailbox::new(8192, path));
        let context = ThreadContext::new_for_test("bench");
        let processor = Arc::new(ActorProcessor::<SpinActor>::new(
            ThreadActor::new_for_test(SpinActor::default()),
            context,
            "bench".to_string(),
            ThreadActorConfig::default(),
        ));
        mailbox.set_processor(processor as Arc<dyn crate::thread::processor::ProcessorInterface>);

        for i in 0..backlog {
            mailbox
                .push(Box::new(i as u64) as BoxedMessage, crate::thread::config::BackpressureStrategy::Block)
                .await
                .unwrap();
        }
        mailbox
            .push(Box::new(u64::MAX) as BoxedMessage, crate::thread::config::BackpressureStrategy::Block)
            .await
            .unwrap();

        let t0 = std::time::Instant::now();
        while !mailbox.is_empty().await {
            worker.process_mailbox(mailbox.clone(), batch).await.unwrap();
        }
        t0.elapsed()
    }

    #[tokio::test]
    async fn batch_size_hol_experiment() {
        let backlog = 200usize;
        let d1 = drain_with_batch(1, backlog).await;
        let d10 = drain_with_batch(10, backlog).await;
        let d50 = drain_with_batch(50, backlog).await;

        println!("\n==== ADR-4 batch_size HOL experiment (backlog={}, ~20µs/msg) ====", backlog);
        println!("batch_size= 1  drain={:?}", d1);
        println!("batch_size=10  drain={:?}", d10);
        println!("batch_size=50  drain={:?}", d50);
        let (a, b, c) = (d1.as_secs_f64(), d10.as_secs_f64(), d50.as_secs_f64());
        println!("ratios: b10/b1={:.2} b50/b1={:.2}", b / a, c / a);

        // Expect near-invariance for a single serialized actor.
        assert!(
            c / a < 3.0 && a / c < 3.0,
            "unexpected batch-size sensitivity: d1={:?} d10={:?} d50={:?}",
            d1, d10, d50
        );
    }
}
