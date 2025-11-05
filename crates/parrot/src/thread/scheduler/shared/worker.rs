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
                        // Own the dequeued schedule slot even if this queued
                        // async job is dropped before its first poll.
                        let lease = SharedLease::new(mailbox.clone());
                        running.push(async move {
                            let owned_mailbox = mailbox.clone();
                            let work = poll_isolated(async move {
                                worker.process_mailbox_with_lease(owned_mailbox, worker.batch_size, &lease).await
                            }, permit, slots.clone(), poll_finished.clone());
                            if let Err(error) = work.await {
                                let message = error.to_string();
                                if let Some(processor) = mailbox.get_processor() {
                                    let notice = message.clone();
                                    // Fallback callbacks can block or panic, too. They consume
                                    // this worker's poll permit and never run on its async driver.
                                    let permit = slots.clone().acquire_owned().await.expect("worker semaphore stays open");
                                    let _ = poll_isolated(async move {
                                        notify_shared_failure(&*processor, notice);
                                        Ok(())
                                    }, permit, slots, poll_finished).await;
                                }
                                error!(path = %mailbox.path().path, error = %message, "worker actor failed");
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

    #[cfg(test)]
    async fn process_mailbox(
        &self,
        mailbox: Arc<dyn Mailbox>,
        max_messages: usize,
    ) -> anyhow::Result<()> {
        let lease = SharedLease::new(mailbox.clone());
        self.process_mailbox_with_lease(mailbox, max_messages, &lease)
            .await
    }

    /// Process one owned mailbox admission. Its lease outlives user disposal.
    async fn process_mailbox_with_lease(
        &self,
        mailbox: Arc<dyn Mailbox>,
        max_messages: usize,
        lease: &SharedLease,
    ) -> anyhow::Result<()> {
        let actor_path = mailbox.path().path.clone();
        let worker_name = format!("worker-{}", self.id);

        let processor = match mailbox.get_processor() {
            Some(p) => p,
            None => {
                lease.release();
                warn!(
                    "[{}] Mailbox for actor {} has no processor attached; skipping",
                    worker_name, actor_path
                );
                return Ok(());
            }
        };

        let work = async {
            if !processor.is_initialized() {
                match SharedStage::new(
                    "shared actor initialization",
                    processor.clone(),
                    lease,
                    || processor.clone().initialize_and_start_erased(),
                ) {
                    Ok(stage) => stage.await?,
                    Err(error) => return Err(error),
                }
            }
            let max_for_run = if self.scheduling_queue.len() > 1
                && processor.consecutive_reductions() >= max_messages.saturating_mul(4)
            {
                processor.reset_reduction_budget();
                1.min(max_messages)
            } else {
                max_messages
            };
            let (processed, errors) =
                match SharedStage::new("shared actor batch", processor.clone(), lease, || {
                    processor.clone().process_batch_erased(
                        mailbox.clone(),
                        max_for_run,
                        self.config.yield_after_each_message,
                    )
                }) {
                    Ok(stage) => stage.await?,
                    Err(error) => return Err(error),
                };
            if self.config.enable_detailed_logging {
                debug!(
                    "[{}] Processed {} messages ({} errors) for {}",
                    worker_name, processed, errors, actor_path
                );
            }
            Ok(())
        };
        let result = match SharedWork::new(&*processor, lease, Box::pin(work)) {
            Ok(work) => work.await,
            Err(error) => Err(error),
        };
        match result {
            Ok(true) => {}
            Ok(false) => {
                lease.release();
                return Ok(());
            }
            Err(error) => {
                lease.release();
                notify_shared_error(&*processor, error);
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
            lease.release();
            return Ok(());
        }
        let has_more = mailbox.has_more_messages().await;
        let requeue = mailbox.schedule_state().release(has_more);
        lease.hand_off();
        if requeue {
            let lane = if mailbox.has_high_priority_messages() {
                crate::thread::scheduler::queue::Lane::High
            } else {
                crate::thread::scheduler::queue::Lane::Normal
            };
            self.scheduling_queue.enqueue_registered(mailbox, lane);
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

// An actor admission remains exclusive through queued polls and cancellation
// disposal. A failure callback may reacquire the slot after release; subsequent
// owner drops must not release that newer admission.
struct SharedLease {
    mailbox: Arc<dyn Mailbox>,
    owned: std::sync::atomic::AtomicBool,
}
impl SharedLease {
    fn new(mailbox: Arc<dyn Mailbox>) -> Self {
        Self {
            mailbox,
            owned: std::sync::atomic::AtomicBool::new(true),
        }
    }
    fn release(&self) {
        if self.owned.swap(false, Ordering::AcqRel) {
            self.mailbox.schedule_state().force_release();
        }
    }
    fn hand_off(&self) {
        self.owned.store(false, Ordering::Release);
    }
}
impl Drop for SharedLease {
    fn drop(&mut self) {
        self.release();
    }
}

// Both futures remain independently owned. In particular, shutdown disposes
// the observer before a data-stage destructor can release the mailbox lease.
struct SharedWork<'a> {
    work: Option<parrot_api::types::BoxedFuture<'a, Result<(), crate::thread::error::SystemError>>>,
    observer: super::super::CancellationObserver<'a>,
    processor: &'a dyn crate::thread::processor::ProcessorInterface,
    lease: &'a SharedLease,
    finished: bool,
}
impl<'a> SharedWork<'a> {
    fn new(
        processor: &'a dyn crate::thread::processor::ProcessorInterface,
        lease: &'a SharedLease,
        work: parrot_api::types::BoxedFuture<'a, Result<(), crate::thread::error::SystemError>>,
    ) -> Result<Self, crate::thread::error::SystemError> {
        let observer = super::super::CancellationObserver::for_stage(processor, "shared")?;
        Ok(Self {
            work: Some(work),
            observer,
            processor,
            lease,
            finished: false,
        })
    }
    fn finish(
        &mut self,
        result: Result<bool, crate::thread::error::SystemError>,
    ) -> Result<bool, crate::thread::error::SystemError> {
        self.finished = true;
        let completed = result.as_ref().copied().unwrap_or(false);
        let result = self.observer.finish(result.map(|_| ()));
        let disposal =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.work.take())))
                .err()
                .map(crate::owned_payload::panic_message);
        if let Some(disposal) = disposal {
            let message = match result {
                Ok(()) => format!("shared work disposal panicked: {disposal}"),
                Err(error) => {
                    let receipt = crate::error_receipt::SystemReceipt::new(error);
                    format!(
                        "{}; shared work disposal panicked: {disposal}",
                        crate::error_receipt::system_summary(receipt.error())
                    )
                }
            };
            return Err(crate::thread::error::SystemError::Panic(message));
        }
        result.map(|_| completed)
    }
}
impl Future for SharedWork<'_> {
    type Output = Result<bool, crate::thread::error::SystemError>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let owner = self.get_mut();
        let result = match std::pin::Pin::new(&mut owner.observer).poll(cx) {
            Poll::Ready(result) => result.map(|_| false),
            Poll::Pending => {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    owner
                        .work
                        .as_mut()
                        .expect("live shared work")
                        .as_mut()
                        .poll(cx)
                })) {
                    Ok(Poll::Pending) => return Poll::Pending,
                    Ok(Poll::Ready(result)) => result.map(|_| true),
                    Err(payload) => Err(crate::thread::error::SystemError::Panic(format!(
                        "shared work panicked: {}",
                        crate::owned_payload::panic_message(payload)
                    ))),
                }
            }
        };
        Poll::Ready(owner.finish(result))
    }
}
impl Drop for SharedWork<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let result = self.finish(Ok(false));
            self.lease.release();
            if let Err(error) = result {
                notify_shared_error(self.processor, error);
            }
        }
    }
}

// Own each user future outside the surrounding async state machine. Catching
// only that state machine can drop the user future during its poll unwind and
// abort the process when the future's destructor also panics.
struct SharedStage<'a, T: Copy> {
    future: Option<
        parrot_api::types::BoxedFuture<'static, Result<T, crate::thread::error::SystemError>>,
    >,
    phase: &'static str,
    lease: &'a SharedLease,
    processor: Arc<dyn crate::thread::processor::ProcessorInterface>,
}
impl<'a, T: Copy> SharedStage<'a, T> {
    fn new(
        phase: &'static str,
        processor: Arc<dyn crate::thread::processor::ProcessorInterface>,
        lease: &'a SharedLease,
        construct: impl FnOnce() -> parrot_api::types::BoxedFuture<
            'static,
            Result<T, crate::thread::error::SystemError>,
        >,
    ) -> Result<Self, crate::thread::error::SystemError> {
        let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(construct)).map_err(
            |payload| {
                crate::thread::error::SystemError::Panic(format!(
                    "{phase} construction panicked: {}",
                    panic_message(payload, &lease.mailbox.path().path)
                ))
            },
        )?;
        Ok(Self {
            future: Some(future),
            phase,
            lease,
            processor,
        })
    }
    fn retire(&mut self) -> Option<String> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.future.take())))
            .err()
            .map(crate::owned_payload::panic_message)
    }
}
impl<T: Copy> std::future::Future for SharedStage<'_, T> {
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
                .expect("live shared stage")
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
impl<T: Copy> Drop for SharedStage<'_, T> {
    fn drop(&mut self) {
        if self.future.is_some() {
            let failure = self.retire();
            self.lease.release();
            if let Some(message) = failure {
                notify_shared_failure(
                    &*self.processor,
                    format!("{} future disposal panicked: {message}", self.phase),
                );
            }
        }
    }
}
fn notify_shared_failure(
    processor: &dyn crate::thread::processor::ProcessorInterface,
    message: String,
) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        processor.notify_system_panic(message);
    })) {
        let message = crate::owned_payload::panic_message(payload);
        tracing::error!(%message, "shared failure notification panicked");
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

/// User future destruction can itself block. Keep that work off async runtime
/// threads even if the isolated poll is still queued when its owner is cancelled.
struct IsolatedFuture<F: Future + Send + 'static> {
    future: Option<std::pin::Pin<Box<F>>>,
    runtime: tokio::runtime::Handle,
}
impl<F> IsolatedFuture<F>
where
    F: Future + Send + 'static,
{
    fn new(future: F) -> Self {
        Self {
            future: Some(Box::pin(future)),
            runtime: tokio::runtime::Handle::current(),
        }
    }
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<F::Output> {
        self.future
            .as_mut()
            .expect("isolated future already disposed")
            .as_mut()
            .poll(cx)
    }
}
impl<F: Future + Send + 'static> Drop for IsolatedFuture<F> {
    fn drop(&mut self) {
        if let Some(future) = self.future.take() {
            let disposal = CaughtDisposal(Some(future));
            // A queued blocking job can itself be destroyed under Tokio's
            // blocking-pool shutdown mutex. Never re-enter spawn_blocking from
            // that destructor. The async bridge submits cleanup only while the
            // runtime can poll it; a rejected/cancelled bridge disposes safely
            // during host-runtime shutdown, where no progress is promised.
            self.runtime.spawn(async move {
                let _ = tokio::task::spawn_blocking(move || drop(disposal)).await;
            });
        }
    }
}

struct CaughtDisposal<T>(Option<T>);
impl<T> Drop for CaughtDisposal<T> {
    fn drop(&mut self) {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.0.take())))
        {
            let _ = crate::owned_payload::panic_message(payload);
        }
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
    let mut future = IsolatedFuture::new(future);
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
            let mut result = future.poll(&mut Context::from_waker(&waker));
            if result.is_ready() {
                // Completed futures are disposed on this same blocking poll;
                // only cancelled Pending work needs the owned fallback task.
                if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(future.future.take())
                })) {
                    result = Poll::Ready(Err(anyhow::anyhow!(
                        "isolated future destructor panicked: {}",
                        crate::owned_payload::panic_message(payload)
                    )));
                }
            }
            drop(slot);
            finished.notify_one();
            (future, result)
        });
        let cancel_on_drop = AbortQueuedPoll(task.abort_handle());
        let (returned_future, result) = task
            .await
            .map_err(|error| anyhow::anyhow!(crate::owned_payload::join_error(error)))?;
        drop(cancel_on_drop);
        future = returned_future;
        match result {
            Poll::Ready(result) => return result,
            Poll::Pending => wake.0.notified().await,
        }
    }
}

fn notify_shared_error(
    processor: &dyn crate::thread::processor::ProcessorInterface,
    error: crate::thread::error::SystemError,
) {
    let receipt = crate::error_receipt::SystemReceipt::new(error);
    notify_shared_failure(
        processor,
        crate::error_receipt::system_summary(receipt.error()),
    );
}

/// Format a caught panic payload into a stable error string.
fn panic_message(payload: Box<dyn Any + Send>, path: &str) -> String {
    format!(
        "Panic in actor {}: {}",
        path,
        crate::owned_payload::panic_message(payload)
    )
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
    fn cancelled_queued_poll_does_not_deadlock_host_runtime_shutdown() {
        const CHILD: &str = "PARROT_QUEUED_DISPOSAL_SHUTDOWN_CHILD";
        const READY: &str = "parrot-queued-disposal-ready";
        if std::env::var_os(CHILD).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .max_blocking_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let (release, blocked) = std::sync::mpsc::channel();
            runtime.block_on(async {
                let (entered, started) = tokio::sync::oneshot::channel();
                tokio::task::spawn_blocking(move || {
                    let _ = entered.send(());
                    let _ = blocked.recv();
                });
                started.await.unwrap();
                let slots = Arc::new(Semaphore::new(1));
                let permit = slots.clone().acquire_owned().await.unwrap();
                let mut queued = Box::pin(poll_isolated(
                    async { Ok::<_, anyhow::Error>(()) },
                    permit,
                    slots,
                    Arc::new(Notify::new()),
                ));
                let waker = futures::task::noop_waker();
                assert!(
                    queued
                        .as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending()
                );
                drop(queued);
            });
            // Parent starts the exit deadline after this marker, excluding slow
            // platform loader startup from the runtime-shutdown assertion.
            println!("{READY}");
            let unlock = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                release.send(()).unwrap();
            });
            drop(runtime);
            unlock.join().unwrap();
            return;
        }
        use std::io::BufRead;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "thread::scheduler::shared::worker::tests::cancelled_queued_poll_does_not_deadlock_host_runtime_shutdown", "--nocapture"])
            .env(CHILD, "1").stdout(std::process::Stdio::piped()).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut reader = std::io::BufReader::new(stdout);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                panic!(
                    "shutdown child exited before probe startup: {:?}",
                    child.wait()
                );
            }
            if line.trim() == READY {
                break;
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "shutdown child failed: {status}");
                break;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("queued poll disposal deadlocked host runtime shutdown");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

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

#[cfg(test)]
mod recursive_contracts {
    use super::*;
    use futures::FutureExt;
    use std::panic::{AssertUnwindSafe, catch_unwind, panic_any};
    use std::pin::Pin;

    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("secondary payload destructor");
        }
    }
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic_any(Secondary);
        }
    }
    struct Work {
        drops: Arc<AtomicUsize>,
        payload_drops: Arc<AtomicUsize>,
        panic_poll: bool,
        panic_drop: bool,
    }
    impl Future for Work {
        type Output = anyhow::Result<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            if self.panic_poll {
                panic_any(Payload(self.payload_drops.clone()));
            }
            Poll::Ready(Ok(()))
        }
    }
    impl Drop for Work {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            if self.panic_drop {
                panic_any(Payload(self.payload_drops.clone()));
            }
        }
    }
    fn assert_no_unwind<T>(result: std::thread::Result<T>) -> T {
        match result {
            Ok(value) => value,
            Err(payload) => {
                // Baseline failures must not recursively panic while reporting.
                std::mem::forget(payload);
                panic!("recursive payload escaped the worker boundary");
            }
        }
    }
    #[test]
    fn recursive_batch_panic_retains_actor_path_and_retires_payload() {
        let drops = Arc::new(AtomicUsize::new(0));
        let message = assert_no_unwind(catch_unwind(AssertUnwindSafe(|| {
            panic_message(Box::new(Payload(drops.clone())), "/actual-worker-actor")
        })));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(message.contains("/actual-worker-actor"));
        assert!(message.contains("panic payload destructor panicked"));
    }
    #[test]
    fn recursive_cancelled_future_disposal_does_not_unwind_owner() {
        let drops = Arc::new(AtomicUsize::new(0));
        let payload_drops = Arc::new(AtomicUsize::new(0));
        assert_no_unwind(catch_unwind(AssertUnwindSafe(|| {
            drop(CaughtDisposal(Some(Work {
                drops: drops.clone(),
                payload_drops: payload_drops.clone(),
                panic_poll: false,
                panic_drop: true,
            })));
        })));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(payload_drops.load(Ordering::SeqCst), 1);
    }
    async fn isolate_failure(panic_poll: bool) {
        let drops = Arc::new(AtomicUsize::new(0));
        let payload_drops = Arc::new(AtomicUsize::new(0));
        let slots = Arc::new(Semaphore::new(1));
        let finished = Arc::new(Notify::new());
        let permit = slots.clone().acquire_owned().await.unwrap();
        let work = Work {
            drops: drops.clone(),
            payload_drops: payload_drops.clone(),
            panic_poll,
            panic_drop: !panic_poll,
        };
        let text = assert_no_unwind(
            AssertUnwindSafe(async {
                let result = poll_isolated(work, permit, slots.clone(), finished.clone()).await;
                let error = result.expect_err("isolation must report actual failure");
                let text = error.to_string();
                drop(error);
                text
            })
            .catch_unwind()
            .await,
        );
        assert!(text.contains("panic payload destructor panicked"), "{text}");
        if !panic_poll {
            assert!(text.contains("isolated future destructor panicked"));
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while drops.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(payload_drops.load(Ordering::SeqCst), 1);
        assert_eq!(slots.available_permits(), 1);
        let permit = slots.clone().acquire_owned().await.unwrap();
        let result = poll_isolated(
            async { Ok::<_, anyhow::Error>(73) },
            permit,
            slots,
            finished,
        )
        .await;
        assert_eq!(result.unwrap(), 73, "same executor still makes progress");
    }
    #[tokio::test]
    async fn recursive_completed_future_drop_reports_failure_and_releases_capacity() {
        isolate_failure(false).await;
    }
    #[tokio::test]
    async fn recursive_poll_panic_reports_failure_without_hostile_join_payload() {
        isolate_failure(true).await;
    }
    #[tokio::test]
    async fn successful_poll_preserves_value_and_exactly_once_disposal() {
        let drops = Arc::new(AtomicUsize::new(0));
        let payload_drops = Arc::new(AtomicUsize::new(0));
        let work = Work {
            drops: drops.clone(),
            payload_drops: payload_drops.clone(),
            panic_poll: false,
            panic_drop: false,
        };
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.unwrap();
        poll_isolated(work, permit, slots.clone(), Arc::new(Notify::new()))
            .await
            .unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(payload_drops.load(Ordering::SeqCst), 0);
        assert_eq!(slots.available_permits(), 1);
        assert_eq!(
            panic_message(Box::new("plain panic"), "/p"),
            "Panic in actor /p: plain panic"
        );
    }
}

#[cfg(test)]
mod stage_contracts {
    use super::*;
    use crate::thread::{
        error::SystemError,
        mailbox::mpsc::MpscMailbox,
        processor::{ProcessorInterface, ProcessorStats, ProcessorStatsTrait},
    };
    use parrot_api::{address::ActorPath, types::BoxedFuture};
    use std::{pin::Pin, sync::atomic::AtomicBool};
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Construct,
        PollDrop,
        ReadyDrop,
        Business,
        BusinessDrop,
        PendingDrop,
        Healthy,
    }
    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("recursive shared payload");
        }
    }
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::panic::panic_any(Secondary);
        }
    }
    #[derive(Debug)]
    struct Business {
        display: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        disposed: Arc<Notify>,
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
    struct Processor {
        mode: Mode,
        batch: bool,
        panic_notice: bool,
        created: AtomicUsize,
        drops: AtomicUsize,
        payloads: Arc<AtomicUsize>,
        notices: Mutex<Vec<String>>,
        entered: Notify,
        completed: Notify,
        disposed: Notify,
        cancelled: AtomicBool,
        cancel_notify: Notify,
        display: Arc<AtomicUsize>,
        error_drops: Arc<AtomicUsize>,
        error_disposed: Arc<Notify>,
        constructed_on: Mutex<Option<std::thread::ThreadId>>,
        polled_on: Mutex<Option<std::thread::ThreadId>>,
        dropped_on: Mutex<Option<std::thread::ThreadId>>,
    }
    impl Processor {
        fn new(mode: Mode, batch: bool, panic_notice: bool) -> Arc<Self> {
            Arc::new(Self {
                mode,
                batch,
                panic_notice,
                created: AtomicUsize::new(0),
                drops: AtomicUsize::new(0),
                payloads: Arc::new(AtomicUsize::new(0)),
                notices: Mutex::new(Vec::new()),
                entered: Notify::new(),
                completed: Notify::new(),
                disposed: Notify::new(),
                cancelled: AtomicBool::new(false),
                cancel_notify: Notify::new(),
                display: Arc::new(AtomicUsize::new(0)),
                error_drops: Arc::new(AtomicUsize::new(0)),
                error_disposed: Arc::new(Notify::new()),
                constructed_on: Mutex::new(None),
                polled_on: Mutex::new(None),
                dropped_on: Mutex::new(None),
            })
        }
        fn construct<T: Copy + Send + 'static>(
            self: Arc<Self>,
            value: T,
            mailbox: Option<Arc<dyn Mailbox>>,
        ) -> BoxedFuture<'static, Result<T, SystemError>> {
            self.created.fetch_add(1, Ordering::SeqCst);
            *self.constructed_on.lock().unwrap() = Some(std::thread::current().id());
            if self.mode == Mode::Construct {
                std::panic::panic_any(Payload(self.payloads.clone()));
            }
            let p = self.clone();
            Box::pin(Work {
                p: self,
                future: Box::pin(async move {
                    *p.polled_on.lock().unwrap() = Some(std::thread::current().id());
                    p.entered.notify_one();
                    if p.mode == Mode::PollDrop {
                        panic!("original shared stage poll fault");
                    }
                    if p.mode == Mode::PendingDrop {
                        std::future::pending::<()>().await;
                    }
                    if let Some(mailbox) = mailbox {
                        assert!(mailbox.pop().await.is_some());
                    }
                    if matches!(p.mode, Mode::Business | Mode::BusinessDrop) {
                        return Err(SystemError::Other(anyhow::Error::new(Business {
                            display: p.display.clone(),
                            drops: p.error_drops.clone(),
                            disposed: p.error_disposed.clone(),
                        })));
                    }
                    p.completed.notify_one();
                    Ok(value)
                }),
            })
        }
    }
    struct Work<T> {
        p: Arc<Processor>,
        future: BoxedFuture<'static, Result<T, SystemError>>,
    }
    impl<T> Future for Work<T> {
        type Output = Result<T, SystemError>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.future.as_mut().poll(cx)
        }
    }
    impl<T> Drop for Work<T> {
        fn drop(&mut self) {
            self.p.drops.fetch_add(1, Ordering::SeqCst);
            *self.p.dropped_on.lock().unwrap() = Some(std::thread::current().id());
            self.p.disposed.notify_one();
            if matches!(
                self.p.mode,
                Mode::PollDrop | Mode::ReadyDrop | Mode::BusinessDrop | Mode::PendingDrop
            ) {
                std::panic::panic_any(Payload(self.p.payloads.clone()));
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
            self.batch
        }
        fn initialize_and_start_erased(
            self: Arc<Self>,
        ) -> BoxedFuture<'static, Result<(), SystemError>> {
            self.construct((), None)
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
            if self.batch {
                self.construct((1, 0), Some(mailbox))
            } else {
                Box::pin(async move {
                    assert!(mailbox.pop().await.is_some());
                    Ok((1, 0))
                })
            }
        }
        fn processing_cancelled(&self) -> BoxedFuture<'_, ()> {
            Box::pin(async move {
                if !self.cancelled.load(Ordering::Acquire) {
                    self.cancel_notify.notified().await;
                }
            })
        }
        fn cancel_processing(&self) {
            self.cancelled.store(true, Ordering::Release);
            self.cancel_notify.notify_one();
        }
        fn notify_system_panic(&self, message: String) {
            self.notices.lock().unwrap().push(message);
            if self.panic_notice {
                std::panic::panic_any(Payload(self.payloads.clone()));
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
    async fn schedule(
        queue: &SchedulingQueue,
        path: &str,
        p: &Arc<Processor>,
        messages: usize,
    ) -> Arc<MpscMailbox> {
        let mailbox = Arc::new(MpscMailbox::new(4, ActorPath::placeholder(path)));
        mailbox.set_processor(p.clone());
        for _ in 0..messages {
            mailbox
                .push(
                    Box::new(1u64),
                    crate::thread::config::BackpressureStrategy::Block,
                )
                .await
                .unwrap();
        }
        let registered: Arc<dyn Mailbox> = mailbox.clone();
        assert!(queue.register(&registered));
        assert!(mailbox.schedule_state().try_enqueue());
        assert!(
            queue.enqueue_registered(registered, crate::thread::scheduler::queue::Lane::Normal)
        );
        mailbox
    }
    async fn run(mode: Mode, batch: bool, panic_notice: bool) {
        let caller = std::thread::current().id();
        let queue = Arc::new(SchedulingQueue::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = Worker::new(
            0,
            Handle::current(),
            queue.clone(),
            shutdown.clone(),
            WorkerConfig {
                batch_size: 1,
                ..Default::default()
            },
        )
        .spawn();
        let p = Processor::new(mode, batch, panic_notice);
        let mailbox = schedule(
            &queue,
            "/fault",
            &p,
            if mode == Mode::Healthy { 1 } else { 2 },
        )
        .await;
        if mode == Mode::PendingDrop {
            tokio::time::timeout(Duration::from_secs(2), p.entered.notified())
                .await
                .unwrap();
            p.cancel_processing();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if (mode == Mode::Healthy && p.drops.load(Ordering::SeqCst) == 1)
                    || !p.notices.lock().unwrap().is_empty()
                    || worker.is_finished()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let healthy = Processor::new(Mode::Healthy, true, false);
        schedule(&queue, "/healthy", &healthy, 1).await;
        let progress =
            tokio::time::timeout(Duration::from_secs(2), healthy.completed.notified()).await;
        shutdown.store(true, Ordering::Release);
        let result = tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap();
        if let Err(error) = result {
            panic!(
                "shared worker died: {}",
                crate::owned_payload::join_error(error)
            );
        }
        progress.expect("shared stage failure prevented another mailbox from running");
        if mode != Mode::Construct {
            tokio::time::timeout(Duration::from_secs(2), p.disposed.notified())
                .await
                .unwrap();
        }
        assert_eq!(
            p.created.load(Ordering::SeqCst),
            1,
            "failed stage requeued remaining messages"
        );
        assert_eq!(
            p.drops.load(Ordering::SeqCst),
            usize::from(mode != Mode::Construct)
        );
        assert_ne!(*p.constructed_on.lock().unwrap(), Some(caller));
        if mode != Mode::Construct {
            assert_ne!(*p.polled_on.lock().unwrap(), Some(caller));
            assert_ne!(*p.dropped_on.lock().unwrap(), Some(caller));
        }
        {
            let notices = p.notices.lock().unwrap();
            assert_eq!(notices.len(), usize::from(mode != Mode::Healthy));
            if mode == Mode::PollDrop {
                assert!(notices[0].contains("original shared stage poll fault"));
                assert!(notices[0].contains("panic payload destructor panicked"));
            }
            if matches!(mode, Mode::Business | Mode::BusinessDrop) {
                assert!(notices[0].contains("original error retained"));
            }
        }
        if matches!(mode, Mode::Business | Mode::BusinessDrop) {
            tokio::time::timeout(Duration::from_secs(2), p.error_disposed.notified())
                .await
                .unwrap();
            assert_eq!(p.error_drops.load(Ordering::SeqCst), 1);
            assert_eq!(p.display.load(Ordering::SeqCst), 0);
        }
        assert_eq!(
            p.payloads.load(Ordering::SeqCst),
            usize::from(matches!(
                mode,
                Mode::Construct
                    | Mode::PollDrop
                    | Mode::ReadyDrop
                    | Mode::BusinessDrop
                    | Mode::PendingDrop
            )) + usize::from(panic_notice)
        );
        assert!(
            mailbox.schedule_state().try_enqueue(),
            "failed work retained its mailbox lease"
        );
        mailbox.schedule_state().force_release();
    }
    async fn isolated(mode: Mode, batch: bool, panic_notice: bool, name: &str) {
        const CHILD: &str = "PARROT_SHARED_STAGE_CONTRACT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            run(mode, batch, panic_notice).await;
            return;
        }
        let name = format!("thread::scheduler::shared::worker::stage_contracts::{name}");
        let output = tokio::time::timeout(
            Duration::from_secs(8),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env(CHILD, "1")
                .env("RUST_BACKTRACE", "0")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"),
            "stage child failed: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[tokio::test]
    async fn initialization_construct() {
        isolated(Mode::Construct, false, false, "initialization_construct").await;
    }
    #[tokio::test]
    async fn initialization_polldrop() {
        isolated(Mode::PollDrop, false, false, "initialization_polldrop").await;
    }
    #[tokio::test]
    async fn initialization_readydrop() {
        isolated(Mode::ReadyDrop, false, false, "initialization_readydrop").await;
    }
    #[tokio::test]
    async fn initialization_business() {
        isolated(Mode::Business, false, false, "initialization_business").await;
    }
    #[tokio::test]
    async fn initialization_businessdrop() {
        isolated(
            Mode::BusinessDrop,
            false,
            false,
            "initialization_businessdrop",
        )
        .await;
    }
    #[tokio::test]
    async fn initialization_pendingdrop() {
        isolated(
            Mode::PendingDrop,
            false,
            false,
            "initialization_pendingdrop",
        )
        .await;
    }
    #[tokio::test]
    async fn initialization_healthy() {
        isolated(Mode::Healthy, false, false, "initialization_healthy").await;
    }
    #[tokio::test]
    async fn batch_construct() {
        isolated(Mode::Construct, true, false, "batch_construct").await;
    }
    #[tokio::test]
    async fn batch_polldrop() {
        isolated(Mode::PollDrop, true, false, "batch_polldrop").await;
    }
    #[tokio::test]
    async fn batch_readydrop() {
        isolated(Mode::ReadyDrop, true, false, "batch_readydrop").await;
    }
    #[tokio::test]
    async fn batch_business() {
        isolated(Mode::Business, true, false, "batch_business").await;
    }
    #[tokio::test]
    async fn batch_businessdrop() {
        isolated(Mode::BusinessDrop, true, false, "batch_businessdrop").await;
    }
    #[tokio::test]
    async fn batch_pendingdrop() {
        isolated(Mode::PendingDrop, true, false, "batch_pendingdrop").await;
    }
    #[tokio::test]
    async fn batch_healthy() {
        isolated(Mode::Healthy, true, false, "batch_healthy").await;
    }
    #[tokio::test]
    async fn notification_panic_is_contained() {
        isolated(
            Mode::ReadyDrop,
            true,
            true,
            "notification_panic_is_contained",
        )
        .await;
    }
}

#[cfg(test)]
mod lease_contracts {
    use super::*;
    use crate::thread::{
        error::SystemError,
        mailbox::mpsc::MpscMailbox,
        processor::{ProcessorInterface, ProcessorStats, ProcessorStatsTrait},
    };
    use parrot_api::{address::ActorPath, types::BoxedFuture};
    use std::{pin::Pin, sync::atomic::AtomicBool};
    #[derive(Clone, Copy, PartialEq)]
    enum Block {
        Poll,
        Drop,
        None,
        Failure,
    }
    struct Release(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }
    struct Processor {
        block: Block,
        gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        entered: Notify,
        drop_entered: Notify,
        disposed: Notify,
        cancelled: AtomicBool,
        cancel_notify: Notify,
        cancel_observed: Notify,
        created: AtomicUsize,
        drops: AtomicUsize,
        reserve: Mutex<Option<std::sync::Weak<dyn Mailbox>>>,
        reserved: AtomicBool,
        notified: Notify,
    }
    impl Processor {
        fn block(&self) {
            if let Some(rx) = self.gate.lock().unwrap().take() {
                rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
    }
    struct Work(Arc<Processor>);
    impl Future for Work {
        type Output = Result<(usize, usize), SystemError>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.entered.notify_one();
            if self.0.block == Block::Failure {
                return Poll::Ready(Err(SystemError::WorkerStateError(
                    "lease callback probe".into(),
                )));
            }
            if self.0.block == Block::Poll {
                self.0.block();
            }
            Poll::Pending
        }
    }
    impl Drop for Work {
        fn drop(&mut self) {
            self.0.drop_entered.notify_one();
            if self.0.block == Block::Drop {
                self.0.block();
            }
            self.0.drops.fetch_add(1, Ordering::SeqCst);
            self.0.disposed.notify_one();
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
            _: Arc<dyn Mailbox>,
            _: usize,
            _: bool,
        ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
            self.created.fetch_add(1, Ordering::SeqCst);
            Box::pin(Work(self))
        }
        fn processing_cancelled(&self) -> BoxedFuture<'_, ()> {
            Box::pin(async move {
                if !self.cancelled.load(Ordering::Acquire) {
                    self.cancel_notify.notified().await;
                }
                self.cancel_observed.notify_one();
            })
        }
        fn cancel_processing(&self) {
            self.cancelled.store(true, Ordering::Release);
            self.cancel_notify.notify_one();
        }
        fn notify_system_panic(&self, _: String) {
            if let Some(mailbox) = self
                .reserve
                .lock()
                .unwrap()
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
            {
                self.reserved
                    .store(mailbox.schedule_state().try_enqueue(), Ordering::Release);
            }
            self.notified.notify_one();
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
    async fn run(block: Block) {
        let queue = Arc::new(SchedulingQueue::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = Worker::new(
            0,
            Handle::current(),
            queue.clone(),
            shutdown.clone(),
            WorkerConfig {
                batch_size: 1,
                ..Default::default()
            },
        )
        .spawn();
        let mut worker = Some(worker);
        let (tx, rx) = std::sync::mpsc::channel();
        let release = Release(Some(tx));
        let p = Arc::new(Processor {
            block,
            gate: Mutex::new(Some(rx)),
            entered: Notify::new(),
            drop_entered: Notify::new(),
            disposed: Notify::new(),
            cancelled: AtomicBool::new(false),
            cancel_notify: Notify::new(),
            cancel_observed: Notify::new(),
            created: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            reserve: Mutex::new(None),
            reserved: AtomicBool::new(false),
            notified: Notify::new(),
        });
        let mailbox = Arc::new(MpscMailbox::new(4, ActorPath::placeholder("/leased")));
        mailbox.set_processor(p.clone());
        mailbox
            .push(
                Box::new(1u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        let registered: Arc<dyn Mailbox> = mailbox.clone();
        assert!(queue.register(&registered));
        assert!(mailbox.schedule_state().try_enqueue());
        assert!(
            queue.enqueue_registered(registered, crate::thread::scheduler::queue::Lane::Normal)
        );
        tokio::time::timeout(Duration::from_secs(2), p.entered.notified())
            .await
            .unwrap();
        p.cancel_processing();
        if block == Block::Poll {
            shutdown.store(true, Ordering::Release);
            tokio::time::timeout(Duration::from_secs(2), worker.take().unwrap())
                .await
                .unwrap()
                .unwrap();
        } else {
            tokio::time::timeout(Duration::from_secs(2), p.cancel_observed.notified())
                .await
                .unwrap();
        }
        if block == Block::Drop {
            tokio::time::timeout(Duration::from_secs(2), p.drop_entered.notified())
                .await
                .unwrap();
        }
        // The async waiter or cancellation observer has completed. The blocked poll or
        // destructor remains behind its synchronous gate, without a time guess.
        let prematurely_available = if block != Block::None {
            let available = mailbox.schedule_state().try_enqueue();
            if available {
                mailbox.schedule_state().force_release();
            }
            available
        } else {
            false
        };
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), p.disposed.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mailbox.schedule_state().try_enqueue() {
                    mailbox.schedule_state().force_release();
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        shutdown.store(true, Ordering::Release);
        if let Some(worker) = worker {
            tokio::time::timeout(Duration::from_secs(2), worker)
                .await
                .unwrap()
                .unwrap();
        }
        assert!(
            !prematurely_available,
            "actor slot was released while its old poll/destructor was still running"
        );
        assert_eq!(p.created.load(Ordering::SeqCst), 1);
        assert_eq!(p.drops.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn cancelled_blocking_poll_retains_mailbox_exclusivity() {
        run(Block::Poll).await;
    }
    #[tokio::test]
    async fn cancelled_blocking_disposal_retains_mailbox_exclusivity() {
        run(Block::Drop).await;
    }
    #[tokio::test]
    async fn ordinary_cancel_releases_after_disposal() {
        run(Block::None).await;
    }
    #[tokio::test]
    async fn completed_owner_cannot_release_callback_reacquired_slot() {
        let queue = Arc::new(SchedulingQueue::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = Worker::new(
            0,
            Handle::current(),
            queue.clone(),
            shutdown.clone(),
            WorkerConfig {
                batch_size: 1,
                ..Default::default()
            },
        );
        let status = worker.status();
        let worker = worker.spawn();
        let p = Arc::new(Processor {
            block: Block::Failure,
            gate: Mutex::new(None),
            entered: Notify::new(),
            drop_entered: Notify::new(),
            disposed: Notify::new(),
            cancelled: AtomicBool::new(false),
            cancel_notify: Notify::new(),
            cancel_observed: Notify::new(),
            created: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            reserve: Mutex::new(None),
            reserved: AtomicBool::new(false),
            notified: Notify::new(),
        });
        let mailbox = Arc::new(MpscMailbox::new(4, ActorPath::placeholder("/reacquired")));
        mailbox.set_processor(p.clone());
        mailbox
            .push(
                Box::new(1u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        let registered: Arc<dyn Mailbox> = mailbox.clone();
        *p.reserve.lock().unwrap() = Some(Arc::downgrade(&registered));
        assert!(queue.register(&registered));
        assert!(mailbox.schedule_state().try_enqueue());
        assert!(
            queue.enqueue_registered(registered, crate::thread::scheduler::queue::Lane::Normal)
        );
        tokio::time::timeout(Duration::from_secs(2), p.notified.notified())
            .await
            .unwrap();
        // Idle is published only after FuturesUnordered has removed the completed
        // admission. Thus its stage and lease destructors have both finished.
        tokio::time::timeout(Duration::from_secs(2), async {
            while status.load(Ordering::Acquire) != WorkerStatus::Idle as usize {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        shutdown.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(
            p.reserved.load(Ordering::Acquire),
            "callback could not reserve the released slot"
        );
        assert!(
            !mailbox.schedule_state().try_enqueue(),
            "old admission's destructor released callback's newer slot"
        );
        mailbox.schedule_state().force_release();
        assert_eq!(p.created.load(Ordering::SeqCst), 1);
        assert_eq!(p.drops.load(Ordering::SeqCst), 1);
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
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicBool, AtomicUsize},
        task::{Context, Poll},
        time::Duration,
    };
    use tokio::sync::Notify;
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        BlockingDrop,
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
            panic!("secondary shared observer payload");
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
        gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        observer_drop_entered: Notify,
        observer_retired: Notify,
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
                gate: Mutex::new(None),
                observer_drop_entered: Notify::new(),
                observer_retired: Notify::new(),
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
                Mode::Plain => panic!("original shared observer failure"),
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
            if p.mode == Mode::BlockingDrop {
                p.observer_drop_entered.notify_one();
                p.gate
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                p.observer_retired.notify_one();
            }
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
            self.cancelled.store(true, Ordering::Release);
            self.waker.wake();
        }
        fn notify_system_panic(&self, message: String) {
            self.notices.lock().unwrap().push(message);
            if self.panic_notice {
                std::panic::panic_any(Payload(self.payloads.clone()));
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
    async fn schedule(
        queue: &SchedulingQueue,
        path: &str,
        p: &Arc<Processor>,
        messages: usize,
    ) -> Arc<MpscMailbox> {
        let mailbox = Arc::new(MpscMailbox::new(4, ActorPath::placeholder(path)));
        mailbox.set_processor(p.clone());
        for _ in 0..messages {
            mailbox
                .push(
                    Box::new(1u64),
                    crate::thread::config::BackpressureStrategy::Block,
                )
                .await
                .unwrap();
        }
        let registered: Arc<dyn Mailbox> = mailbox.clone();
        assert!(queue.register(&registered));
        assert!(mailbox.schedule_state().try_enqueue());
        assert!(
            queue.enqueue_registered(registered, crate::thread::scheduler::queue::Lane::Normal)
        );
        mailbox
    }
    async fn run(
        mode: Mode,
        batch_ready: bool,
        business: bool,
        panic_notice: bool,
        shutdown_early: bool,
        backlog: bool,
    ) {
        let caller = std::thread::current().id();
        let queue = Arc::new(SchedulingQueue::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = Worker::new(
            0,
            Handle::current(),
            queue.clone(),
            shutdown.clone(),
            WorkerConfig {
                batch_size: 1,
                ..Default::default()
            },
        )
        .spawn();
        let mut worker = Some(worker);
        let p = Processor::new(mode, batch_ready, business, panic_notice);
        let mailbox = schedule(&queue, "/observer", &p, if backlog { 2 } else { 1 }).await;
        tokio::time::timeout(Duration::from_secs(2), p.entered.notified())
            .await
            .unwrap();
        if !batch_ready && mode == Mode::PendingDrop {
            tokio::time::timeout(Duration::from_secs(2), p.batch_entered.notified())
                .await
                .unwrap();
            if shutdown_early {
                shutdown.store(true, Ordering::Release);
                worker.take().unwrap().await.unwrap();
            } else {
                p.cancel_processing();
            }
        }
        let failing = !matches!(mode, Mode::Healthy | Mode::Ready);
        if failing {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if !p.notices.lock().unwrap().is_empty()
                        || worker.as_ref().is_some_and(|w| w.is_finished())
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        }
        if let Some(worker) = worker.take() {
            let healthy = Processor::new(Mode::Healthy, true, false, false);
            schedule(&queue, "/healthy", &healthy, 1).await;
            let progress =
                tokio::time::timeout(Duration::from_secs(2), healthy.completed.notified()).await;
            shutdown.store(true, Ordering::Release);
            let result = tokio::time::timeout(Duration::from_secs(2), worker)
                .await
                .unwrap();
            if let Err(error) = result {
                panic!(
                    "observer killed shared worker: {}",
                    crate::owned_payload::join_error(error)
                );
            }
            progress.expect("observer failure prevented another actor's progress");
        }
        assert_eq!(p.created.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.drops.load(Ordering::SeqCst),
            usize::from(mode != Mode::Construct)
        );
        let batch = usize::from(matches!(mode, Mode::PendingDrop | Mode::Healthy));
        assert_eq!(p.batch_created.load(Ordering::SeqCst), batch);
        assert_eq!(p.batch_drops.load(Ordering::SeqCst), batch);
        {
            let notices = p.notices.lock().unwrap();
            assert_eq!(notices.len(), usize::from(failing));
            if failing {
                assert!(notices[0].contains("shared cancellation observer"));
            }
            if mode == Mode::Plain {
                assert!(notices[0].contains("original shared observer failure"));
            }
            if business {
                assert!(notices[0].contains("original error retained"));
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
        assert_ne!(
            *p.constructed_on.lock().unwrap(),
            Some(caller),
            "observer constructor ran on async driver"
        );
        if mode != Mode::Construct {
            assert_ne!(*p.polled_on.lock().unwrap(), Some(caller));
            assert_ne!(*p.dropped_on.lock().unwrap(), Some(caller));
        }
        assert!(mailbox.schedule_state().try_enqueue());
        mailbox.schedule_state().force_release();
        if backlog {
            assert_eq!(
                mailbox.len().await,
                1,
                "failed observer requeued remaining messages"
            );
        }
        if business {
            tokio::time::timeout(Duration::from_secs(2), p.error_disposed.notified())
                .await
                .unwrap();
            assert_eq!(p.error_drops.load(Ordering::SeqCst), 1);
            assert_eq!(p.display.load(Ordering::SeqCst), 0);
        }
    }
    async fn isolated(
        mode: Mode,
        batch_ready: bool,
        business: bool,
        panic_notice: bool,
        shutdown: bool,
        backlog: bool,
        name: &str,
    ) {
        const CHILD: &str = "PARROT_SHARED_OBSERVER_CONTRACT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            run(mode, batch_ready, business, panic_notice, shutdown, backlog).await;
            return;
        }
        let name = format!("thread::scheduler::shared::worker::cancellation_contracts::{name}");
        let output = tokio::time::timeout(
            Duration::from_secs(8),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env(CHILD, "1")
                .env("RUST_BACKTRACE", "0")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
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
    async fn construction() {
        isolated(
            Mode::Construct,
            false,
            false,
            false,
            false,
            false,
            "construction",
        )
        .await;
    }
    #[tokio::test]
    async fn plain_poll() {
        isolated(Mode::Plain, false, false, false, false, false, "plain_poll").await;
    }
    #[tokio::test]
    async fn recursive_poll() {
        isolated(
            Mode::Poll,
            false,
            false,
            false,
            false,
            false,
            "recursive_poll",
        )
        .await;
    }
    #[tokio::test]
    async fn poll_drop() {
        isolated(
            Mode::PollDrop,
            false,
            false,
            false,
            false,
            false,
            "poll_drop",
        )
        .await;
    }
    #[tokio::test]
    async fn ready_drop() {
        isolated(
            Mode::ReadyDrop,
            false,
            false,
            false,
            false,
            false,
            "ready_drop",
        )
        .await;
    }
    #[tokio::test]
    async fn losing_drop() {
        isolated(
            Mode::PendingDrop,
            true,
            false,
            false,
            false,
            false,
            "losing_drop",
        )
        .await;
    }
    #[tokio::test]
    async fn cancelled_drop() {
        isolated(
            Mode::PendingDrop,
            false,
            false,
            false,
            false,
            false,
            "cancelled_drop",
        )
        .await;
    }
    #[tokio::test]
    async fn shutdown_drop() {
        isolated(
            Mode::PendingDrop,
            false,
            false,
            false,
            true,
            false,
            "shutdown_drop",
        )
        .await;
    }
    #[tokio::test]
    async fn notice_panic() {
        isolated(
            Mode::Plain,
            false,
            false,
            true,
            false,
            false,
            "notice_panic",
        )
        .await;
    }
    #[tokio::test]
    async fn shutdown_notice_panic() {
        isolated(
            Mode::PendingDrop,
            false,
            false,
            true,
            true,
            false,
            "shutdown_notice_panic",
        )
        .await;
    }
    #[tokio::test]
    async fn business_drop() {
        isolated(
            Mode::PendingDrop,
            true,
            true,
            false,
            false,
            false,
            "business_drop",
        )
        .await;
    }
    #[tokio::test]
    async fn healthy_ready() {
        isolated(
            Mode::Ready,
            false,
            false,
            false,
            false,
            false,
            "healthy_ready",
        )
        .await;
    }
    #[tokio::test]
    async fn healthy_work() {
        isolated(
            Mode::Healthy,
            true,
            false,
            false,
            false,
            false,
            "healthy_work",
        )
        .await;
    }
    #[tokio::test]
    async fn no_requeue() {
        isolated(
            Mode::PendingDrop,
            true,
            false,
            false,
            false,
            true,
            "no_requeue",
        )
        .await;
    }
    struct Release(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    #[tokio::test]
    async fn shutdown_finishes_observer_before_data_and_lease() {
        const CHILD: &str = "PARROT_SHARED_OBSERVER_ORDER_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output=tokio::time::timeout(Duration::from_secs(8),tokio::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","thread::scheduler::shared::worker::cancellation_contracts::shutdown_finishes_observer_before_data_and_lease","--nocapture"]).env(CHILD,"1").env("RUST_BACKTRACE","0").kill_on_drop(true).output()).await.unwrap().unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"),
                "ordered cleanup child failed: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let queue = Arc::new(SchedulingQueue::new(10));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = Worker::new(
            0,
            Handle::current(),
            queue.clone(),
            shutdown.clone(),
            WorkerConfig::default(),
        )
        .spawn();
        let p = Processor::new(Mode::BlockingDrop, false, false, false);
        let (tx, rx) = std::sync::mpsc::channel();
        let release = Release(Some(tx));
        *p.gate.lock().unwrap() = Some(rx);
        let mailbox = schedule(&queue, "/ordered-disposal", &p, 1).await;
        tokio::time::timeout(Duration::from_secs(2), p.batch_entered.notified())
            .await
            .unwrap();
        shutdown.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), p.observer_drop_entered.notified())
            .await
            .unwrap();
        let released = mailbox.schedule_state().try_enqueue();
        if released {
            mailbox.schedule_state().force_release();
        }
        let data_dropped = p.batch_drops.load(Ordering::SeqCst);
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), p.observer_retired.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while p.batch_drops.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !mailbox.schedule_state().try_enqueue() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        mailbox.schedule_state().force_release();
        assert!(
            !released,
            "observer still owned user state when mailbox was released"
        );
        assert_eq!(
            data_dropped, 0,
            "data destructor ran before observer retirement"
        );
    }
}
