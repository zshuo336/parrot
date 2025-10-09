use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::task::{Context, Poll};

use futures::{
    StreamExt,
    stream::FuturesUnordered,
    task::{ArcWake, waker_ref},
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time;
use tracing::{debug, error, warn};

use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::queue::SchedulingQueue;

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

    /// Owns suspended actor work but has capacity to poll another actor.
    Waiting = 4,
}

impl WorkerStatus {
    pub fn from_usize(v: usize) -> Self {
        match v {
            0 => WorkerStatus::Idle,
            1 => WorkerStatus::Processing,
            2 => WorkerStatus::ShuttingDown,
            4 => WorkerStatus::Waiting,
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

    /// Launches a core worker on the runtime.
    pub fn spawn(self) -> JoinHandle<()> {
        self.spawn_inner(None, None)
    }

    pub(super) fn spawn_burst(
        self,
        idle_timeout: Duration,
        alive: Arc<AtomicUsize>,
    ) -> JoinHandle<()> {
        self.spawn_inner(Some(idle_timeout), Some(BurstGuard(alive)))
    }

    fn spawn_inner(
        self,
        idle_timeout: Option<Duration>,
        guard: Option<BurstGuard>,
    ) -> JoinHandle<()> {
        let runtime_handle = self.runtime_handle.clone();
        runtime_handle.spawn(async move {
            let _guard = guard;
            Arc::new(self).run_loop(idle_timeout).await;
        })
    }

    /// Multiplex suspended mailboxes, allowing one blocking poll at a time.
    async fn run_loop(self: &Arc<Self>, idle_timeout: Option<Duration>) {
        let mut idle_since = time::Instant::now();
        let slots = Arc::new(Semaphore::new(1));
        let poll_finished = Arc::new(Notify::new());
        let queue_notify = self.scheduling_queue.notify_handle();
        let mut running = FuturesUnordered::new();
        while !self.shutdown_flag.load(Ordering::Acquire) {
            if !running.is_empty() || !self.scheduling_queue.is_empty() {
                idle_since = time::Instant::now();
            } else if idle_timeout.is_some_and(|timeout| idle_since.elapsed() >= timeout) {
                break;
            }
            let status = if slots.available_permits() == 0 {
                WorkerStatus::Processing
            } else if running.is_empty() {
                WorkerStatus::Idle
            } else {
                WorkerStatus::Waiting
            };
            self.status.store(status as usize, Ordering::Relaxed);
            tokio::select! {
                // Reserve capacity BEFORE dequeuing, keeping CPU backlog visible
                // to priority selection, other workers and the elastic controller.
                permit = slots.clone().acquire_owned(), if !self.scheduling_queue.is_empty() => {
                    let permit = permit.expect("worker semaphore stays open");
                    if let Some(mailbox) = self.scheduling_queue.try_pop() {
                        let worker = self.clone();
                        let slots = slots.clone();
                        let poll_finished = poll_finished.clone();
                        running.push(async move {
                            let owned_mailbox = mailbox.clone();
                            let processor = mailbox.get_processor();
                            let work = poll_isolated(async move {
                                worker.process_mailbox(owned_mailbox, worker.batch_size).await
                            }, permit, slots, poll_finished);
                            let result = if let Some(processor) = &processor {
                                tokio::select! {
                                    biased;
                                    _ = processor.processing_cancelled() => {
                                        mailbox.schedule_state().force_release();
                                        return;
                                    }
                                    result = work => result,
                                }
                            } else {
                                work.await
                            };
                            if let Err(error) = result {
                                mailbox.schedule_state().force_release();
                                if let Some(processor) = mailbox.get_processor() {
                                    processor.notify_system_panic(error.to_string());
                                }
                                error!(path = %mailbox.path().path, %error, "worker actor failed");
                            }
                        });
                    }
                }
                _ = running.next(), if !running.is_empty() => {}
                _ = poll_finished.notified() => {}
                _ = queue_notify.notified() => {}
                _ = time::sleep(self.config.idle_sleep_duration) => {}
            }
        }
        self.status
            .store(WorkerStatus::ShuttingDown as usize, Ordering::Relaxed);
        // Dropping running cancels suspended futures; an executing synchronous
        // poll cannot be preempted and releases its owned future when it returns.
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
                mailbox.schedule_state().force_release();
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
                mailbox.schedule_state().force_release();
                return Err(anyhow::anyhow!(e.to_string()));
            }
        }

        // Process a batch under panic isolation.
        let batch_result = {
            let processor = processor.clone();
            let mailbox_for_panics = mailbox.clone();
            let path_for_panics = actor_path.clone();
            // M2 fairness yield: truncate this run to a single message when
            // the actor is over its reduction budget AND other mailboxes
            // are waiting in the scheduling queue (yielding to an empty
            // queue — i.e. yielding to yourself — is pure churn). The
            // budget resets after the yield (POC beam-sched semantics).
            let max_for_run = if self.scheduling_queue.len() > 1
                && processor.consecutive_reductions() >= max_messages.saturating_mul(4)
            {
                processor.reset_reduction_budget();
                1.min(max_messages)
            } else {
                max_messages
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Only the future *creation* happens here; actual actor code
                // runs when awaited below. Panics inside the future are caught
                // by the JoinHandle of the spawned inner task.
                processor.process_batch_erased(
                    mailbox_for_panics.clone(),
                    max_for_run,
                    self.config.yield_after_each_message,
                )
            }));
            match result {
                Ok(fut) => fut.await,
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
                error!(
                    "[{}] Error processing batch for {}: {}",
                    worker_name, actor_path, e
                );
                // Release the schedule slot so future pushes can re-enqueue.
                mailbox.schedule_state().force_release();
                // M3 supervision: notify the system (detached) so the
                // supervision state machine decides restart/stop/escalate.
                processor.notify_system_panic(e.to_string());
                // Do not re-queue a panicking mailbox.
                self.record_panic(&actor_path);
                return Ok(());
            }
        }

        // Re-queue while there is more work, preserving the single-owner
        // invariant through the schedule slot state machine.
        //
        // M2: mailboxes that still hold high-priority messages re-enter the
        // scheduler's High lane, jumping over any normal-lane backlog.
        if self.shutdown_flag.load(Ordering::Acquire) || mailbox.is_closed().await {
            mailbox.schedule_state().force_release();
            return Ok(());
        }
        let has_more = mailbox.has_more_messages().await;
        if mailbox.schedule_state().release(has_more) {
            let lane = if mailbox.has_high_priority_messages() {
                crate::thread::scheduler::queue::Lane::High
            } else {
                crate::thread::scheduler::queue::Lane::Normal
            };
            self.scheduling_queue.push_with_lane(mailbox, lane);
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

struct BurstGuard(Arc<AtomicUsize>);
impl Drop for BurstGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A retained notification covers wakeups before, during and after a poll.
struct PollWake(Notify);
impl ArcWake for PollWake {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.notify_one();
    }
}

/// Cancels a blocking job that has not started when its async owner is dropped.
/// Tokio cannot preempt a synchronous poll that is already executing.
struct AbortQueuedPoll(tokio::task::AbortHandle);
impl Drop for AbortQueuedPoll {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Isolate CPU-bound user code without retaining a blocking thread across
/// Pending. Each worker owns one permit, bounding simultaneous CPU polls.
pub(crate) async fn poll_isolated<F, T>(
    future: F,
    first_permit: OwnedSemaphorePermit,
    slots: Arc<Semaphore>,
    poll_finished: Arc<Notify>,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let mut future = Box::pin(future);
    let wake = Arc::new(PollWake(Notify::new()));
    let mut permit = Some(first_permit);
    loop {
        let slot = match permit.take() {
            Some(slot) => slot,
            None => slots.clone().acquire_owned().await?,
        };
        let poll_wake = wake.clone();
        let finished = poll_finished.clone();
        let task = tokio::task::spawn_blocking(move || {
            let waker = waker_ref(&poll_wake);
            let result = future.as_mut().poll(&mut Context::from_waker(&waker));
            drop(slot);
            finished.notify_one();
            (future, result)
        });
        let cancel_on_drop = AbortQueuedPoll(task.abort_handle());
        let (returned_future, result) = task.await?;
        drop(cancel_on_drop);
        future = returned_future;
        match result {
            Poll::Ready(result) => return result,
            Poll::Pending => wake.0.notified().await,
        }
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
    use crate::thread::actor::ThreadActor;
    use crate::thread::config::ThreadActorConfig;
    use crate::thread::context::ThreadContext;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use crate::thread::processor::ActorProcessor;
    use crate::thread::scheduler::queue::SchedulingQueue;
    use async_trait::async_trait;
    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::address::{ActorPath, ActorRef};
    use parrot_api::types::{
        ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage, WeakActorTarget,
    };
    use std::time::Duration;

    #[test]
    fn cancelled_isolated_poll_does_not_run_later_from_the_blocking_queue() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            entered_rx.await.unwrap();
            let polls = Arc::new(AtomicUsize::new(0));
            let dropped = Arc::new(AtomicUsize::new(0));
            struct Guard(Arc<AtomicUsize>);
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }
            let guard = Guard(dropped.clone());
            let observed_polls = polls.clone();
            let work = async move {
                let _guard = guard;
                observed_polls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            };
            let slots = Arc::new(Semaphore::new(1));
            let permit = slots.clone().acquire_owned().await.unwrap();
            let mut isolated =
                Box::pin(poll_isolated(work, permit, slots, Arc::new(Notify::new())));
            assert!(futures::poll!(&mut isolated).is_pending());
            drop(isolated);
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            // Observe disposal directly; do not rely on executor queue order.
            tokio::time::timeout(Duration::from_secs(1), async {
                while dropped.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                polls.load(Ordering::SeqCst),
                0,
                "cancelled work must never begin"
            );
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
        });
    }

    #[derive(Debug)]
    struct EchoActor;

    impl Actor for EchoActor {
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
            Box::pin(async move { Ok(msg) })
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
            fn deliver<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async { Ok(()) })
            }
            fn send<'a>(
                &'a self,
                _msg: BoxedMessage,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async { Ok(Box::new(()) as BoxedMessage) })
            }
            fn send_with_timeout<'a>(
                &'a self,
                _msg: BoxedMessage,
                _t: Option<Duration>,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
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
            WorkerConfig {
                batch_size: 4,
                ..Default::default()
            },
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
                .push(
                    Box::new(format!("m{}", i)) as BoxedMessage,
                    crate::thread::config::BackpressureStrategy::Block,
                )
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
            .push(
                Box::new("x") as BoxedMessage,
                crate::thread::config::BackpressureStrategy::Block,
            )
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
                .push(
                    Box::new(i as u64) as BoxedMessage,
                    crate::thread::config::BackpressureStrategy::Block,
                )
                .await
                .unwrap();
        }
        mailbox
            .push(
                Box::new(u64::MAX) as BoxedMessage,
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();

        let t0 = std::time::Instant::now();
        while !mailbox.is_empty().await {
            worker
                .process_mailbox(mailbox.clone(), batch)
                .await
                .unwrap();
        }
        t0.elapsed()
    }

    #[tokio::test]
    async fn batch_size_hol_experiment() {
        let backlog = 200usize;
        let d1 = drain_with_batch(1, backlog).await;
        let d10 = drain_with_batch(10, backlog).await;
        let d50 = drain_with_batch(50, backlog).await;

        println!(
            "\n==== ADR-4 batch_size HOL experiment (backlog={}, ~20µs/msg) ====",
            backlog
        );
        println!("batch_size= 1  drain={:?}", d1);
        println!("batch_size=10  drain={:?}", d10);
        println!("batch_size=50  drain={:?}", d50);
        let (a, b, c) = (d1.as_secs_f64(), d10.as_secs_f64(), d50.as_secs_f64());
        println!("ratios: b10/b1={:.2} b50/b1={:.2}", b / a, c / a);

        // Expect near-invariance for a single serialized actor.
        assert!(
            c / a < 3.0 && a / c < 3.0,
            "unexpected batch-size sensitivity: d1={:?} d10={:?} d50={:?}",
            d1,
            d10,
            d50
        );
    }
}
