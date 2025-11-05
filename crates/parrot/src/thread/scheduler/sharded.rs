//! # Sharded (thread-affinity) scheduler
//!
//! A fixed pool of OS threads where **each thread owns exactly one shard
//! queue**. Actors (or messages) tagged with an affinity key are hashed to a
//! shard and processed **only** by that shard's thread.
//!
//! ## Routing and progress
//!
//! Message wake-ups go directly to the mailbox's assigned shard instead of
//! through the shared pool's central scheduling queue. Each shard multiplexes
//! suspended actor futures on a current-thread Tokio runtime; its normal queue
//! and lifecycle control queue wake that runtime when new work arrives.
//!
//! Initialization, message handling and stop-hook polling stay on the owner
//! thread. A suspended future allows other actors on that shard to progress.
//! Synchronous blocking code still stalls the entire shard; affinity domains
//! therefore need balanced load. Idle shards do not steal another shard's work.
//! Throughput and latency require measurement for the actual workload.
//!
//! Textual diagram (not compiled):
//! `producer --hash(key)--> shard[i] MPSC --> thread[i] loop (pop -> run)`
//!
//! ## Implementation notes
//!
//! - Each shard thread runs its own small Tokio current-thread runtime so
//!   actor futures (including `async` handlers) execute on the owning thread.
//! - The `Mailbox` trait is preserved: shard routing happens at wake-hook
//!   time (message arrival), the processor contract is unchanged.
//! - Backpressure and bounded mailboxes behave exactly as in the shared pool.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use flume::{Receiver, Sender, unbounded};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use tokio::runtime::Builder;

use super::StopHookRequest;
use crate::thread::mailbox::Mailbox;

/// A scheduled item: a mailbox that became ready (has messages to process).
struct ShardItem {
    mailbox: Arc<dyn Mailbox>,
    active: Arc<AtomicBool>,
}

type ShardExit = Result<(), String>;
fn shard_thread_exit(body: impl FnOnce()) -> ShardExit {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body))
        .map_err(crate::owned_payload::panic_message)
}

// Reports are independent of a shutdown caller. A cancelled caller cannot drop
// the OS join or publish completion before the join job actually finishes.
#[derive(Default)]
struct JoinReport {
    result: Mutex<Option<ShardExit>>,
    changed: std::sync::Condvar,
}
impl JoinReport {
    fn result(&self) -> Option<ShardExit> {
        self.result.lock().unwrap().clone()
    }
    fn wait(&self) {
        let mut result = self.result.lock().unwrap();
        while result.is_none() {
            result = self.changed.wait(result).unwrap();
        }
    }
}
struct JoinJob {
    handle: Option<std::thread::JoinHandle<ShardExit>>,
    report: Arc<JoinReport>,
}
impl Drop for JoinJob {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        // Thread bodies already retire arbitrary panic payloads on the owning
        // shard. These defensive boundaries also preserve an unexpected join
        // failure instead of treating it as a successful shutdown.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle.join()))
            .map_err(crate::owned_payload::panic_message)
            .and_then(|result| result.map_err(crate::owned_payload::panic_message))
            .and_then(|result| result);
        *self.report.result.lock().unwrap() = Some(result);
        self.report.changed.notify_all();
    }
}
enum ShardThread {
    Running(std::thread::JoinHandle<ShardExit>),
    Joining {
        id: std::thread::ThreadId,
        report: Arc<JoinReport>,
    },
}
impl ShardThread {
    fn id(&self) -> std::thread::ThreadId {
        match self {
            Self::Running(handle) => handle.thread().id(),
            Self::Joining { id, .. } => *id,
        }
    }
}
#[derive(Default)]
struct ShardThreads {
    slots: Vec<ShardThread>,
    failure: Option<String>,
}
impl ShardThreads {
    fn claim(&mut self, only_finished: bool) -> Vec<JoinJob> {
        let mut jobs = Vec::new();
        for slot in &mut self.slots {
            let ShardThread::Running(handle) = slot else {
                continue;
            };
            if only_finished && !handle.is_finished() {
                continue;
            }
            let id = handle.thread().id();
            if id == std::thread::current().id() {
                continue;
            }
            let report = Arc::new(JoinReport::default());
            let ShardThread::Running(handle) = std::mem::replace(
                slot,
                ShardThread::Joining {
                    id,
                    report: report.clone(),
                },
            ) else {
                unreachable!()
            };
            jobs.push(JoinJob {
                handle: Some(handle),
                report,
            });
        }
        jobs
    }
    fn collect(&mut self) -> Option<Result<(), crate::thread::error::SystemError>> {
        let mut index = 0;
        while index < self.slots.len() {
            let result = match &self.slots[index] {
                ShardThread::Joining { report, .. } => report.result(),
                ShardThread::Running(_) => None,
            };
            if let Some(result) = result {
                if let Err(error) = result {
                    self.failure.get_or_insert(error);
                }
                self.slots.swap_remove(index);
            } else {
                index += 1;
            }
        }
        self.slots.is_empty().then(|| {
            self.failure.as_ref().map_or(Ok(()), |error| {
                Err(crate::thread::error::SystemError::Panic(format!(
                    "shard thread failed: {error}"
                )))
            })
        })
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
            let report = Arc::new(JoinReport::default());
            let job = JoinJob {
                handle: Some(handle),
                report: report.clone(),
            };
            let queued = tokio::task::spawn_blocking(move || drop(job));
            queued.abort();
            drop(pool_release);
            assert!(report.result().is_none());
            assert!(!exited.load(Ordering::Acquire));
            drop(actor_release);
            assert!(queued.await.unwrap_err().is_cancelled());
            blocker.await.unwrap();
            assert!(exited.load(Ordering::Acquire));
            assert_eq!(report.result(), Some(Err("late thread failure".into())));
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
        let handle = std::thread::spawn(move || -> ShardExit {
            std::panic::panic_any(Payload(count));
        });
        let report = Arc::new(JoinReport::default());
        drop(JoinJob {
            handle: Some(handle),
            report: report.clone(),
        });
        let error = report.result().unwrap().unwrap_err();
        assert!(error.contains("panic payload destructor panicked"));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(report.result(), Some(Err(error)));
    }
}

/// Per-shard handle shared with producers (wake hooks).
struct Shard {
    tx: Sender<ShardItem>,
    stop_tx: Sender<StopHookRequest>,
}

/// Fixed-size pool of affinity shards.
///
/// `shards` must be created via [`ShardedScheduler::start`].
pub struct ShardedScheduler {
    shards: Vec<Arc<Shard>>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    threads: Mutex<ShardThreads>,
    /// Registration tokens also invalidate already queued work on deschedule.
    assigned: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Batch size per wake (mirrors shared-pool worker config).
    batch_size: usize,
}

impl fmt::Debug for ShardedScheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShardedScheduler")
            .field("shard_count", &self.shards.len())
            .field("batch_size", &self.batch_size)
            .finish()
    }
}

impl ShardedScheduler {
    /// Start `n` shard threads.
    pub fn start(n: usize, batch_size: usize) -> Arc<Self> {
        Self::start_with_startup(n, batch_size, || {})
    }

    // The empty production startup is a zero-sized closure. Contract tests
    // inject OS startup faults through this exact spawn/entry/join pipeline,
    // independently of isolated actor callbacks.
    fn start_with_startup<F>(n: usize, batch_size: usize, startup: F) -> Arc<Self>
    where
        F: Fn() + Clone + Send + 'static,
    {
        let n = n.max(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_notify = Arc::new(tokio::sync::Notify::new());
        let mut shards = Vec::with_capacity(n);
        let mut threads = Vec::with_capacity(n);

        for i in 0..n {
            let (tx, rx) = unbounded::<ShardItem>();
            let (stop_tx, stop_rx) = unbounded::<StopHookRequest>();
            let tx2 = tx.clone();
            let shard = Arc::new(Shard { tx, stop_tx });
            shards.push(shard);

            let shutdown = shutdown.clone();
            let shutdown_notify = shutdown_notify.clone();
            let startup = startup.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("parrot-shard-{}", i))
                    .spawn(move || {
                        // Dispose a panic payload here, before this OS worker is
                        // considered finished. Detached handles then own only a
                        // framework String, never an arbitrary user destructor.
                        shard_thread_exit(|| {
                            startup();
                            shard_loop(i, rx, tx2, stop_rx, shutdown, shutdown_notify, batch_size);
                        })
                    })
                    .expect("spawn shard thread"),
            );
        }

        Arc::new(Self {
            shards,
            shutdown,
            shutdown_notify,
            threads: Mutex::new(ShardThreads {
                slots: threads.into_iter().map(ShardThread::Running).collect(),
                failure: None,
            }),
            assigned: Mutex::new(HashMap::new()),
            batch_size,
        })
    }

    /// Route key → shard index (uses high-quality fxhash-style mixing so
    /// sequential keys spread evenly).
    fn shard_for(&self, key: u64) -> usize {
        // mix (splitmix64 finalizer) then mod
        let mut z = key.wrapping_add(0x9e3779b97f4a7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        ((z ^ (z >> 31)) as usize) % self.shards.len()
    }

    /// Hash a string affinity key down to a u64 (FNV-1a).
    fn hash_key(key: &str) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in key.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// Schedule an actor onto the shard chosen by its affinity key.
    ///
    /// The mailbox's wake hook is re-armed to route every subsequent message
    /// arrival into the same shard — the actor never migrates.
    pub fn schedule_affinity(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        affinity_key: &str,
    ) -> Result<(), String> {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err("sharded scheduler is shutting down".into());
        }
        if !mailbox.has_processor() {
            return Err(format!("mailbox for {} has no processor", path));
        }

        let idx = self.shard_for(Self::hash_key(affinity_key));
        let active = Arc::new(AtomicBool::new(true));
        {
            let mut assigned = self.assigned.lock().unwrap();
            if assigned.contains_key(path) {
                return Err(format!("actor {path} is already scheduled"));
            }
            assigned.insert(path.to_string(), active.clone());
        }

        // Re-arm the wake hook: every message arrival pushes this mailbox
        // into its home shard (subject to the schedule-slot single-entry
        // invariant).
        let shard = self.shards[idx].clone();
        let weak = Arc::downgrade(&mailbox);
        let shutting = self.shutdown.clone();
        let wake_active = active.clone();
        mailbox.set_wake_hook(Arc::new(move || {
            if shutting.load(Ordering::Relaxed) || !wake_active.load(Ordering::Acquire) {
                return;
            }
            if let Some(strong) = weak.upgrade()
                && strong.schedule_state().try_enqueue()
            {
                let _ = shard.tx.send(ShardItem {
                    mailbox: strong,
                    active: wake_active.clone(),
                });
            }
        }));

        // Initial enqueue so an already-nonempty mailbox is drained.
        if mailbox.schedule_state().try_enqueue() {
            let shard = self.shards[idx].clone();
            let _ = shard.tx.send(ShardItem {
                mailbox: mailbox.clone(),
                active,
            });
        }
        Ok(())
    }

    /// Deschedule an actor (stop routing its wake-ups).
    pub fn deschedule(&self, path: &str) -> Result<(), String> {
        if let Some(active) = self.assigned.lock().unwrap().remove(path) {
            active.store(false, Ordering::Release);
        }
        Ok(())
    }

    pub(crate) fn enqueue_stop_hook(
        &self,
        affinity_key: &str,
        request: StopHookRequest,
    ) -> Result<(), crate::thread::error::SystemError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(crate::thread::error::SystemError::ShuttingDown);
        }
        let index = self.shard_for(Self::hash_key(affinity_key));
        self.shards[index].stop_tx.send(request).map_err(|_| {
            crate::thread::error::SystemError::ShutdownError("shard control channel closed".into())
        })
    }

    /// Shard lengths (for metrics).
    pub fn shard_lengths(&self) -> Vec<usize> {
        self.shards.iter().map(|s| s.tx.len()).collect()
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Owned OS workers, including finished threads awaiting join. Unlike
    /// configured shard_count, this reaches zero after shutdown joins them.
    pub(crate) fn owned_thread_count(&self) -> usize {
        let mut threads = self.threads.lock().unwrap();
        let _ = threads.collect();
        threads.slots.len()
    }

    /// Legacy synchronous close. Use `shutdown_result` or `shutdown_until` to
    /// obtain the retained thread failure receipt.
    pub fn shutdown(&self) {
        if let Err(error) = self.shutdown_result() {
            tracing::error!(%error, "shard shutdown failed");
        }
    }

    /// Synchronously join every owned thread and preserve the first failure.
    /// Calling from one of these threads returns an error instead of self-joining.
    pub fn shutdown_result(&self) -> Result<(), crate::thread::error::SystemError> {
        self.request_shutdown();
        loop {
            let jobs = {
                let mut threads = self.threads.lock().unwrap();
                if threads
                    .slots
                    .iter()
                    .any(|slot| slot.id() == std::thread::current().id())
                {
                    return Err(crate::thread::error::SystemError::ShutdownError(
                        "cannot synchronously join the current shard thread".into(),
                    ));
                }
                if let Some(result) = threads.collect() {
                    return result;
                }
                threads.claim(false)
            };
            // Joining and all user cleanup run outside the scheduler mutex.
            drop(jobs);
            let reports: Vec<_> = {
                let threads = self.threads.lock().unwrap();
                threads
                    .slots
                    .iter()
                    .filter_map(|slot| match slot {
                        ShardThread::Joining { report, .. } => Some(report.clone()),
                        _ => None,
                    })
                    .collect()
            };
            for report in reports {
                report.wait();
            }
        }
    }
    fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.shutdown_notify.notify_waiters();
    }

    /// Cancel suspended work and join finished threads without blocking Tokio.
    /// Synchronous user code cannot be preempted; report it at the deadline.
    pub async fn shutdown_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<(), crate::thread::error::SystemError> {
        self.request_shutdown();
        loop {
            let jobs = {
                let mut threads = self.threads.lock().unwrap();
                if let Some(result) = threads.collect() {
                    return result;
                }
                threads.claim(true)
            };
            for job in jobs {
                // The receipt retains this job after caller cancellation. Even
                // a queued blocking task cancelled by runtime shutdown executes
                // JoinJob::drop, which joins before publishing its result.
                drop(tokio::task::spawn_blocking(move || drop(job)));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(crate::thread::error::SystemError::Timeout(
                    "shard threads did not exit before the shutdown deadline".into(),
                ));
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + std::time::Duration::from_millis(1)),
            )
            .await;
        }
    }
}

/// The per-shard event loop: pop mailboxes, run a batch, re-enqueue while
/// more messages remain.
fn shard_loop(
    shard_id: usize,
    rx: Receiver<ShardItem>,
    tx: Sender<ShardItem>,
    stop_rx: Receiver<StopHookRequest>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    batch_size: usize,
) {
    // Current-thread runtime: actor futures on THIS thread only.
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("shard runtime");
    rt.block_on(async move {
        let mut running = FuturesUnordered::new();
        let mut stopping = FuturesUnordered::new();
        let mut check_shutdown = tokio::time::interval(std::time::Duration::from_millis(50));
        while !shutdown.load(Ordering::Acquire) {
            let notified = shutdown_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if shutdown.load(Ordering::Acquire) {
                break;
            }
            tokio::select! {
                _ = &mut notified => {}
                item = rx.recv_async() => {
                    let Ok(item) = item else { break };
                    running.push(process_item(shard_id, item, tx.clone(), batch_size));
                }
                request = stop_rx.recv_async() => {
                    let Ok(request) = request else { break };
                    stopping.push(request.run());
                }
                _ = running.next(), if !running.is_empty() => {}
                _ = stopping.next(), if !stopping.is_empty() => {}
                _ = check_shutdown.tick() => {}
            }
        }
        // Dropping the futures cancels suspended actor work on this same thread.
    });
}

// Each acquired schedule slot has one release authority. A failure callback may
// immediately re-enqueue the mailbox, so outer cancellation cannot release twice.
struct ScheduleLease {
    mailbox: Arc<dyn Mailbox>,
    owned: AtomicBool,
}
impl ScheduleLease {
    fn release(&self) {
        if self.owned.swap(false, Ordering::AcqRel) {
            self.mailbox.schedule_state().force_release();
        }
    }
    fn hand_off(&self) {
        self.owned.store(false, Ordering::Release);
    }
}
impl Drop for ScheduleLease {
    fn drop(&mut self) {
        self.release();
    }
}

// Own each user future outside the surrounding async state machine. Catching
// only that state machine can drop the user future during its poll unwind and
// abort the process when the future's destructor also panics.
struct ShardStage<'a, T: Copy> {
    future: Option<
        parrot_api::types::BoxedFuture<'static, Result<T, crate::thread::error::SystemError>>,
    >,
    lease: &'a ScheduleLease,
    processor: Arc<dyn crate::thread::processor::ProcessorInterface>,
}
impl<'a, T: Copy> ShardStage<'a, T> {
    fn new(
        lease: &'a ScheduleLease,
        processor: Arc<dyn crate::thread::processor::ProcessorInterface>,
        construct: impl FnOnce() -> parrot_api::types::BoxedFuture<
            'static,
            Result<T, crate::thread::error::SystemError>,
        >,
    ) -> Result<Self, crate::thread::error::SystemError> {
        let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(construct)).map_err(
            |payload| {
                crate::thread::error::SystemError::Panic(crate::owned_payload::panic_message(
                    payload,
                ))
            },
        )?;
        Ok(Self {
            future: Some(future),
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
impl<T: Copy> std::future::Future for ShardStage<'_, T> {
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
                .expect("live shard stage")
                .as_mut()
                .poll(cx)
        }));
        let result = match polled {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => result,
            Err(payload) => Err(crate::thread::error::SystemError::Panic(
                crate::owned_payload::panic_message(payload),
            )),
        };
        if let Some(disposal) = owner.retire() {
            let message = match result {
                Ok(_) => format!("shard work disposal panicked: {disposal}"),
                Err(error) => {
                    let receipt = crate::error_receipt::SystemReceipt::new(error);
                    format!(
                        "{}; shard work disposal panicked: {disposal}",
                        crate::error_receipt::system_summary(receipt.error())
                    )
                }
            };
            return Poll::Ready(Err(crate::thread::error::SystemError::Panic(message)));
        }
        Poll::Ready(result)
    }
}
impl<T: Copy> Drop for ShardStage<'_, T> {
    fn drop(&mut self) {
        if self.future.is_some() {
            let failure = self.retire();
            self.lease.release();
            if let Some(message) = failure {
                notify_failure(
                    &*self.processor,
                    format!("shard work disposal panicked: {message}"),
                );
            }
        }
    }
}
fn notify_failure(processor: &dyn crate::thread::processor::ProcessorInterface, message: String) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        processor.notify_system_panic(message);
    })) {
        let message = crate::owned_payload::panic_message(payload);
        tracing::error!(%message, "shard failure notification panicked");
    }
}

async fn process_item(shard_id: usize, item: ShardItem, tx: Sender<ShardItem>, batch_size: usize) {
    let mailbox = &item.mailbox;
    if !item.active.load(Ordering::Acquire) {
        mailbox.schedule_state().force_release();
        return;
    }
    let Some(processor) = mailbox.get_processor() else {
        mailbox.schedule_state().force_release();
        return;
    };
    let lease = ScheduleLease {
        mailbox: mailbox.clone(),
        owned: AtomicBool::new(true),
    };
    // Catch both future construction and polling panics, including actor init.
    let work = std::panic::AssertUnwindSafe(async {
        if !processor.is_initialized() {
            ShardStage::new(&lease, processor.clone(), || {
                processor.clone().initialize_and_start_erased()
            })?
            .await?;
        }
        ShardStage::new(&lease, processor.clone(), || {
            processor
                .clone()
                .process_batch_erased(mailbox.clone(), batch_size, false)
        })?
        .await
    })
    .catch_unwind();
    let mut cancellation = match super::CancellationObserver::for_stage(&*processor, "shard") {
        Ok(observer) => observer,
        Err(error) => {
            let receipt = crate::error_receipt::SystemReceipt::new(error);
            lease.release();
            notify_failure(
                &*processor,
                crate::error_receipt::system_summary(receipt.error()),
            );
            return;
        }
    };
    let result = tokio::select! {
        biased;
        cancelled = &mut cancellation => {
            match cancelled {
                Ok(()) => { lease.release(); return; }
                Err(error) => Err(error),
            }
        }
        result = work => match result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(payload) => Err(crate::thread::error::SystemError::Panic(format!(
                "actor panicked during shard initialization or processing: {}",
                crate::owned_payload::panic_message(payload),
            ))),
        },
    };
    // Retire a losing observer before releasing/reassigning this actor slot.
    if let Err(error) = cancellation.finish(result) {
        let receipt = crate::error_receipt::SystemReceipt::new(error);
        let error = crate::error_receipt::system_summary(receipt.error());
        tracing::error!(shard_id, path = %mailbox.path().path, %error, "shard actor failed");
        lease.release();
        notify_failure(&*processor, error);
        return;
    }
    if !item.active.load(Ordering::Acquire) {
        lease.release();
        return;
    }
    let has_more = mailbox.has_more_messages().await;
    lease.hand_off();
    if mailbox.schedule_state().release(has_more) {
        let _ = tx.send(item);
    }
}

impl Drop for ShardedScheduler {
    fn drop(&mut self) {
        // Do not leak idle OS threads when the owner is dropped without shutdown.
        // Joining here could deadlock if the last owner drops on a shard thread.
        self.request_shutdown();
    }
}

#[cfg(test)]
mod panic_contracts {
    use super::*;
    use crate::thread::{
        error::SystemError,
        mailbox::mpsc::MpscMailbox,
        processor::{ProcessorInterface, ProcessorStats, ProcessorStatsTrait},
    };
    use parrot_api::{address::ActorPath, types::BoxedFuture};
    use std::panic::{AssertUnwindSafe, catch_unwind, panic_any};
    use std::{
        any::Any,
        future::Future,
        pin::Pin,
        sync::atomic::AtomicUsize,
        task::{Context, Poll},
        time::Duration,
    };
    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("shard secondary destructor");
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
    struct BusinessError(Arc<AtomicUsize>);
    impl fmt::Display for BusinessError {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("business formatting must not run");
        }
    }
    impl std::error::Error for BusinessError {}
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Init,
        Creation,
        Poll,
        PendingDrop,
        PlainPanic,
        Business,
        Healthy,
        ReadyDrop,
        PollDrop,
    }
    struct Processor {
        mode: Mode,
        payload_drops: Arc<AtomicUsize>,
        future_drops: Arc<AtomicUsize>,
        display_calls: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
        processed: AtomicUsize,
        cancelled: AtomicBool,
        cancel: tokio::sync::Notify,
        notices: Mutex<Vec<String>>,
        requeue: Mutex<Option<Arc<dyn Mailbox>>>,
        requeue_acquired: AtomicBool,
    }
    struct PendingWork {
        mode: Mode,
        entered: Arc<tokio::sync::Notify>,
        drops: Arc<AtomicUsize>,
        payload_drops: Arc<AtomicUsize>,
    }
    impl Future for PendingWork {
        type Output = Result<(usize, usize), SystemError>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.entered.notify_one();
            if self.mode == Mode::PollDrop {
                panic_any(Payload(self.payload_drops.clone()));
            }
            if self.mode == Mode::ReadyDrop {
                Poll::Ready(Ok((0, 0)))
            } else {
                Poll::Pending
            }
        }
    }
    impl Drop for PendingWork {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            panic_any(Payload(self.payload_drops.clone()));
        }
    }
    impl Processor {
        fn new(mode: Mode) -> Arc<Self> {
            Arc::new(Self {
                mode,
                payload_drops: Arc::new(AtomicUsize::new(0)),
                future_drops: Arc::new(AtomicUsize::new(0)),
                display_calls: Arc::new(AtomicUsize::new(0)),
                entered: Arc::new(tokio::sync::Notify::new()),
                processed: AtomicUsize::new(0),
                cancelled: AtomicBool::new(false),
                cancel: tokio::sync::Notify::new(),
                notices: Mutex::new(Vec::new()),
                requeue: Mutex::new(None),
                requeue_acquired: AtomicBool::new(false),
            })
        }
    }
    impl ProcessorStatsTrait for Processor {
        fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
            None
        }
    }
    impl ProcessorInterface for Processor {
        fn is_initialized(&self) -> bool {
            self.mode != Mode::Init
        }
        fn initialize_and_start_erased(
            self: Arc<Self>,
        ) -> BoxedFuture<'static, Result<(), SystemError>> {
            Box::pin(async move {
                panic_any(Payload(self.payload_drops.clone()));
            })
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
            if self.mode == Mode::Creation {
                panic_any(Payload(self.payload_drops.clone()));
            }
            if matches!(
                self.mode,
                Mode::PendingDrop | Mode::ReadyDrop | Mode::PollDrop
            ) {
                return Box::pin(PendingWork {
                    mode: self.mode,
                    entered: self.entered.clone(),
                    drops: self.future_drops.clone(),
                    payload_drops: self.payload_drops.clone(),
                });
            }
            Box::pin(async move {
                if self.mode == Mode::Poll {
                    panic_any(Payload(self.payload_drops.clone()));
                }
                if self.mode == Mode::PlainPanic {
                    panic!("original shard cause");
                }
                if self.mode == Mode::Business {
                    return Err(SystemError::Other(anyhow::Error::new(BusinessError(
                        self.display_calls.clone(),
                    ))));
                }
                let mut count = 0;
                while mailbox.pop().await.is_some() {
                    count += 1;
                    self.processed.fetch_add(1, Ordering::SeqCst);
                }
                self.entered.notify_one();
                Ok((count, 0))
            })
        }
        fn cancel_processing(&self) {
            self.cancelled.store(true, Ordering::Release);
            self.cancel.notify_waiters();
        }
        fn processing_cancelled(&self) -> BoxedFuture<'_, ()> {
            Box::pin(async move {
                let notified = self.cancel.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !self.cancelled.load(Ordering::Acquire) {
                    notified.await;
                }
            })
        }
        fn notify_system_panic(&self, message: String) {
            self.notices.lock().unwrap().push(message);
            if let Some(mailbox) = self.requeue.lock().unwrap().as_ref() {
                self.requeue_acquired
                    .store(mailbox.schedule_state().try_enqueue(), Ordering::SeqCst);
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
    fn stack(mode: Mode, path: &str) -> (Arc<Processor>, Arc<MpscMailbox>) {
        let processor = Processor::new(mode);
        let mailbox = Arc::new(MpscMailbox::new(4, ActorPath::placeholder(path)));
        mailbox.set_processor(processor.clone());
        (processor, mailbox)
    }
    async fn seed(mailbox: &Arc<MpscMailbox>) {
        mailbox
            .push(
                Box::new(1u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
    }
    fn no_unwind<T>(result: std::thread::Result<T>) -> T {
        match result {
            Ok(value) => value,
            Err(payload) => {
                std::mem::forget(payload);
                panic!("shard user panic escaped ownership boundary");
            }
        }
    }
    async fn terminal(mode: Mode) {
        let (processor, mailbox) = stack(mode, "/shard-terminal");
        seed(&mailbox).await;
        assert!(mailbox.schedule_state().try_enqueue());
        let (tx, _rx) = flume::unbounded();
        no_unwind(
            AssertUnwindSafe(process_item(
                0,
                ShardItem {
                    mailbox: mailbox.clone(),
                    active: Arc::new(AtomicBool::new(true)),
                },
                tx,
                1,
            ))
            .catch_unwind()
            .await,
        );
        assert!(
            mailbox.schedule_state().try_enqueue(),
            "terminal failure releases schedule ownership"
        );
        mailbox.schedule_state().force_release();
        let notices = processor.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        match mode {
            Mode::PlainPanic => assert!(notices[0].contains("original shard cause")),
            Mode::Business => {
                assert_eq!(processor.display_calls.load(Ordering::SeqCst), 0);
                assert!(notices[0].contains("actor operation failed"));
            }
            _ => {
                assert_eq!(
                    processor.payload_drops.load(Ordering::SeqCst),
                    if mode == Mode::PollDrop { 2 } else { 1 }
                );
                assert!(notices[0].contains("panic payload destructor panicked"));
            }
        }
    }
    #[tokio::test]
    async fn recursive_initialization_panic_keeps_notification_and_releases_slot() {
        terminal(Mode::Init).await;
    }
    #[tokio::test]
    async fn recursive_batch_creation_panic_keeps_notification_and_releases_slot() {
        terminal(Mode::Creation).await;
    }
    #[tokio::test]
    async fn recursive_batch_poll_panic_keeps_notification_and_releases_slot() {
        terminal(Mode::Poll).await;
    }
    #[tokio::test]
    async fn plain_panic_preserves_original_cause() {
        terminal(Mode::PlainPanic).await;
    }
    #[tokio::test]
    async fn business_error_diagnostics_never_invoke_display() {
        terminal(Mode::Business).await;
    }
    async fn cancellation(drop_owner: bool) {
        let (processor, mailbox) = stack(Mode::PendingDrop, "/shard-cancel");
        assert!(mailbox.schedule_state().try_enqueue());
        let (tx, _rx) = flume::unbounded();
        let mut task = Box::pin(process_item(
            0,
            ShardItem {
                mailbox: mailbox.clone(),
                active: Arc::new(AtomicBool::new(true)),
            },
            tx,
            1,
        ));
        assert!(futures::poll!(&mut task).is_pending());
        if drop_owner {
            no_unwind(catch_unwind(AssertUnwindSafe(|| drop(task))));
        } else {
            processor.cancel_processing();
            no_unwind(AssertUnwindSafe(task).catch_unwind().await);
        }
        assert_eq!(processor.future_drops.load(Ordering::SeqCst), 1);
        assert_eq!(processor.payload_drops.load(Ordering::SeqCst), 1);
        assert!(mailbox.schedule_state().try_enqueue());
        mailbox.schedule_state().force_release();
        let notices = processor.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("disposal"));
    }
    #[tokio::test]
    async fn cancelled_batch_drop_cannot_escape_shard() {
        cancellation(false).await;
    }
    #[tokio::test]
    async fn dropped_owner_retires_pending_batch_and_releases_slot() {
        cancellation(true).await;
    }
    #[tokio::test]
    async fn healthy_mailbox_can_be_scheduled_again_after_processing() {
        let (processor, mailbox) = stack(Mode::Healthy, "/healthy-shard");
        let (tx, rx) = flume::unbounded();
        for _ in 0..2 {
            seed(&mailbox).await;
            assert!(mailbox.schedule_state().try_enqueue());
            process_item(
                0,
                ShardItem {
                    mailbox: mailbox.clone(),
                    active: Arc::new(AtomicBool::new(true)),
                },
                tx.clone(),
                1,
            )
            .await;
        }
        assert_eq!(processor.processed.load(Ordering::SeqCst), 2);
        assert!(rx.is_empty());
        assert!(processor.notices.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn actual_single_shard_continues_after_recursive_user_panic() {
        let scheduler = ShardedScheduler::start(1, 1);
        let (bad, bad_mailbox) = stack(Mode::Poll, "/bad-shard");
        seed(&bad_mailbox).await;
        scheduler
            .schedule_affinity("/bad-shard", bad_mailbox, "one")
            .unwrap();
        let (good, good_mailbox) = stack(Mode::Healthy, "/good-shard");
        seed(&good_mailbox).await;
        scheduler
            .schedule_affinity("/good-shard", good_mailbox, "one")
            .unwrap();
        let progressed = tokio::time::timeout(Duration::from_secs(2), good.entered.notified())
            .await
            .is_ok();
        let joined = catch_unwind(AssertUnwindSafe(|| scheduler.shutdown()));
        no_unwind(joined);
        assert!(
            progressed,
            "one actor's payload must not kill the shared shard"
        );
        assert_eq!(good.processed.load(Ordering::SeqCst), 1);
        assert_eq!(bad.payload_drops.load(Ordering::SeqCst), 1);
        assert_eq!(bad.notices.lock().unwrap().len(), 1);
        assert_eq!(scheduler.owned_thread_count(), 0);
    }
    #[tokio::test]
    async fn ready_future_destructor_failure_keeps_notification_and_releases_slot() {
        terminal(Mode::ReadyDrop).await;
    }
    #[tokio::test]
    async fn simultaneous_poll_and_destructor_panics_cannot_abort_process() {
        const CHILD: &str = "PARROT_SHARD_DOUBLE_PANIC_CHILD";
        if std::env::var_os(CHILD).is_some() {
            terminal(Mode::PollDrop).await;
            return;
        }
        let child=tokio::time::timeout(Duration::from_secs(10),tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","thread::scheduler::sharded::panic_contracts::simultaneous_poll_and_destructor_panics_cannot_abort_process","--nocapture"])
            .env(CHILD,"1").env("RUST_BACKTRACE","0").kill_on_drop(true).output()).await.unwrap().unwrap();
        assert!(
            child.status.success(),
            "child failed: {}\n{}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
    }

    #[tokio::test]
    async fn cancellation_notification_cannot_lose_reentrant_schedule_ownership() {
        let (processor, mailbox) = stack(Mode::PendingDrop, "/reentrant-shard");
        *processor.requeue.lock().unwrap() = Some(mailbox.clone());
        assert!(mailbox.schedule_state().try_enqueue());
        let (tx, _rx) = flume::unbounded();
        let mut task = Box::pin(process_item(
            0,
            ShardItem {
                mailbox: mailbox.clone(),
                active: Arc::new(AtomicBool::new(true)),
            },
            tx,
            1,
        ));
        assert!(futures::poll!(&mut task).is_pending());
        processor.cancel_processing();
        let result = AssertUnwindSafe(task).catch_unwind().await;
        processor.requeue.lock().unwrap().take();
        no_unwind(result);
        assert!(
            processor.requeue_acquired.load(Ordering::SeqCst),
            "notification can claim the released slot"
        );
        let incorrectly_free = mailbox.schedule_state().try_enqueue();
        mailbox.schedule_state().force_release();
        assert!(
            !incorrectly_free,
            "outer cancellation cleared the new schedule owner"
        );
    }
}

#[cfg(test)]
mod join_contracts {
    use super::*;
    use crate::thread::{
        error::SystemError,
        processor::{ProcessorInterface, ProcessorStats, ProcessorStatsTrait},
    };
    use parrot_api::types::BoxedFuture;
    use std::{any::Any, panic::panic_any, sync::atomic::AtomicUsize, time::Duration};
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Plain,
        Recursive,
        Blocking,
        Reenter,
        Close,
        Healthy,
    }
    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("secondary shard join payload");
        }
    }
    struct Payload {
        mode: Mode,
        drops: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
        gate: Option<std::sync::mpsc::Receiver<()>>,
        scheduler: std::sync::Weak<ShardedScheduler>,
        observed: Arc<AtomicUsize>,
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            if self.mode == Mode::Reenter
                && let Some(scheduler) = self.scheduler.upgrade()
            {
                self.observed
                    .store(scheduler.owned_thread_count(), Ordering::SeqCst);
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
        scheduler: Mutex<std::sync::Weak<ShardedScheduler>>,
        observed: Arc<AtomicUsize>,
        returned: AtomicBool,
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
                Mode::Plain => panic!("original shard thread failure"),
                Mode::Healthy => {}
                Mode::Close => {
                    let scheduler = self.scheduler.lock().unwrap().upgrade().unwrap();
                    scheduler.shutdown();
                    self.returned.store(true, Ordering::SeqCst);
                }
                _ => {
                    let gate = self.gate.lock().unwrap().take();
                    let scheduler = self.scheduler.lock().unwrap().clone();
                    panic_any(Payload {
                        mode: self.mode,
                        drops: self.drops.clone(),
                        entered: self.payload_entered.clone(),
                        gate,
                        scheduler,
                        observed: self.observed.clone(),
                    });
                }
            }
        }
    }
    async fn start(mode: Mode) -> (Arc<ShardedScheduler>, Arc<Processor>, Release) {
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
            scheduler: Mutex::new(std::sync::Weak::new()),
            observed: Arc::new(AtomicUsize::new(0)),
            returned: AtomicBool::new(false),
        });
        let (begin, ready) = std::sync::mpsc::channel();
        let probe = p.clone();
        let ready = Arc::new(Mutex::new(ready));
        let scheduler = ShardedScheduler::start_with_startup(1, 1, move || {
            ready.lock().unwrap().recv().unwrap();
            probe.fault();
        });
        *p.scheduler.lock().unwrap() = Arc::downgrade(&scheduler);
        begin.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), p.entered.notified())
            .await
            .unwrap();
        (scheduler, p, Release(Some(tx)))
    }
    async fn close(s: &ShardedScheduler) -> Result<(), SystemError> {
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
                s.owned_thread_count(),
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
                "original shard thread failure"
            } else if mode == Mode::Recursive {
                "panic payload destructor panicked"
            } else {
                "non-string panic payload"
            },
        );
        assert_eq!(s.owned_thread_count(), 0);
        expect_failure(close(&s).await, "shard thread failed");
        if mode != Mode::Plain {
            assert_eq!(p.drops.load(Ordering::SeqCst), 1);
        }
        if mode == Mode::Reenter {
            assert_eq!(p.observed.load(Ordering::SeqCst), 1);
        }
    }
    async fn isolated(mode: Mode, cancel: bool, name: &str) {
        const CHILD: &str = "PARROT_SHARD_JOIN_CHILD";
        if std::env::var_os(CHILD).is_some() {
            exercise(mode, cancel).await;
            return;
        }
        let name = format!("thread::scheduler::sharded::join_contracts::{name}");
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
            output.status.success(),
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
        expect_failure(a, "original shard thread failure");
        expect_failure(b, "original shard thread failure");
        assert_eq!(s.owned_thread_count(), 0);
    }
    #[tokio::test]
    async fn legacy_sync_shutdown_retains_failure_for_checked_waiter() {
        let (s, _, _release) = start(Mode::Plain).await;
        s.shutdown();
        expect_failure(close(&s).await, "original shard thread failure");
    }
    #[tokio::test]
    async fn sync_close_from_own_shard_does_not_self_join() {
        let (s, p, _release) = start(Mode::Close).await;
        close(&s).await.unwrap();
        assert!(p.returned.load(Ordering::SeqCst));
        assert_eq!(s.owned_thread_count(), 0);
    }
    #[tokio::test]
    async fn healthy_shutdown_still_completes() {
        let (s, _, _release) = start(Mode::Healthy).await;
        close(&s).await.unwrap();
        close(&s).await.unwrap();
        assert_eq!(s.owned_thread_count(), 0);
    }
    #[tokio::test]
    async fn completed_worker_payload_is_safe_when_owner_is_dropped() {
        const CHILD: &str = "PARROT_SHARD_DROP_OWNER_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "thread::scheduler::sharded::join_contracts::completed_worker_payload_is_safe_when_owner_is_dropped", "--nocapture"])
                    .env(CHILD, "1").env("RUST_BACKTRACE", "0").kill_on_drop(true).output(),
            ).await.unwrap().unwrap();
            assert!(
                output.status.success(),
                "owner drop child failed: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let (scheduler, processor, _release) = start(Mode::Recursive).await;
        // Inspect actual completion, rather than assuming a yield/sleep means
        // the thread has exited. No shutdown method consumes its join handle.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let finished = {
                    let threads = scheduler.threads.lock().unwrap();
                    matches!(threads.slots.as_slice(), [ShardThread::Running(handle)] if handle.is_finished())
                };
                if finished { break; }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.unwrap();
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(scheduler)))
        {
            std::mem::forget(payload);
            panic!("dropping the scheduler inherited an unretired worker panic payload");
        }
        assert_eq!(processor.drops.load(Ordering::SeqCst), 1);
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
            panic!("secondary shard observer payload");
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
                Mode::Plain => panic!("original shard observer failure"),
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
    async fn schedule(s: &ShardedScheduler, path: &str, p: &Arc<Processor>) -> Arc<MpscMailbox> {
        let mailbox = Arc::new(MpscMailbox::new(4, ActorPath::placeholder(path)));
        mailbox.set_processor(p.clone());
        mailbox
            .push(
                Box::new(1u64),
                crate::thread::config::BackpressureStrategy::Block,
            )
            .await
            .unwrap();
        s.schedule_affinity(path, mailbox.clone(), "single")
            .unwrap();
        mailbox
    }
    async fn observe_failure(s: &ShardedScheduler, p: &Processor) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !p.notices.lock().unwrap().is_empty() {
                    break;
                }
                let died = {
                    let threads = s.threads.lock().unwrap();
                    threads
                        .slots
                        .iter()
                        .any(|t| matches!(t,ShardThread::Running(h) if h.is_finished()))
                };
                if died {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !p.notices.lock().unwrap().is_empty(),
            "actor observer failure killed shard instead of notifying its processor"
        );
    }
    async fn run(
        mode: Mode,
        batch_ready: bool,
        business: bool,
        panic_notice: bool,
        shutdown: bool,
    ) {
        let s = ShardedScheduler::start(1, 1);
        let p = Processor::new(mode, batch_ready, business, panic_notice);
        let mailbox = schedule(&s, "/observer", &p).await;
        tokio::time::timeout(Duration::from_secs(2), p.entered.notified())
            .await
            .unwrap();
        if !batch_ready && mode == Mode::PendingDrop {
            tokio::time::timeout(Duration::from_secs(2), p.batch_entered.notified())
                .await
                .unwrap();
            if shutdown {
                s.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
                    .await
                    .unwrap();
            } else {
                p.cancel_processing();
            }
        }
        let failing = !matches!(mode, Mode::Healthy | Mode::Ready);
        if failing && !shutdown {
            observe_failure(&s, &p).await;
        }
        if !shutdown {
            let healthy = Processor::new(Mode::Healthy, true, false, false);
            schedule(&s, "/healthy", &healthy).await;
            tokio::time::timeout(Duration::from_secs(2), healthy.completed.notified())
                .await
                .expect("observer failure prevented unrelated actor progress");
            assert_eq!(
                *p.constructed_on.lock().unwrap(),
                *healthy.constructed_on.lock().unwrap(),
                "both actors must execute on the same shard"
            );
            s.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
                .await
                .unwrap();
        }
        assert_eq!(s.owned_thread_count(), 0);
        assert_eq!(p.created.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.drops.load(Ordering::SeqCst),
            usize::from(mode != Mode::Construct)
        );
        let batch = usize::from(matches!(mode, Mode::PendingDrop | Mode::Healthy));
        assert_eq!(p.batch_created.load(Ordering::SeqCst), batch);
        assert_eq!(p.batch_drops.load(Ordering::SeqCst), batch);
        assert_eq!(
            p.payloads.load(Ordering::SeqCst),
            match mode {
                Mode::PollDrop => 2,
                Mode::Construct | Mode::Poll | Mode::ReadyDrop | Mode::PendingDrop => 1,
                _ => 0,
            } + usize::from(panic_notice)
        );
        {
            let notices = p.notices.lock().unwrap();
            assert_eq!(notices.len(), usize::from(failing));
            if let Some(message) = notices.first() {
                assert!(message.contains("shard cancellation observer"));
                assert!(message.contains(if mode == Mode::Plain {
                    "original shard observer failure"
                } else {
                    "panic payload destructor panicked"
                }));
                if business {
                    assert!(message.contains("original error retained"));
                }
            }
        }
        if mode != Mode::Construct {
            assert_eq!(
                *p.constructed_on.lock().unwrap(),
                *p.polled_on.lock().unwrap()
            );
            assert_eq!(*p.polled_on.lock().unwrap(), *p.dropped_on.lock().unwrap());
        }
        assert!(
            mailbox.schedule_state().try_enqueue(),
            "completed actor retained its schedule lease"
        );
        mailbox.schedule_state().force_release();
        if business {
            tokio::time::timeout(Duration::from_secs(2), p.error_disposed.notified())
                .await
                .unwrap();
            assert_eq!(p.error_drops.load(Ordering::SeqCst), 1);
            assert_eq!(p.display.load(Ordering::SeqCst), 0);
        }
    }
    #[tokio::test]
    async fn observer_construction_failure_does_not_kill_shard() {
        run(Mode::Construct, false, false, false, false).await;
    }
    #[tokio::test]
    async fn observer_poll_failure_preserves_cause_and_shard_progress() {
        run(Mode::Plain, false, false, false, false).await;
    }
    #[tokio::test]
    async fn recursive_observer_payload_stays_on_owner_thread() {
        run(Mode::Poll, false, false, false, false).await;
    }
    #[tokio::test]
    async fn observer_poll_and_drop_cannot_abort_shard() {
        const CHILD: &str = "PARROT_SHARD_OBSERVER_DOUBLE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            run(Mode::PollDrop, false, false, false, false).await;
            return;
        }
        let output=tokio::time::timeout(Duration::from_secs(5),tokio::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","thread::scheduler::sharded::cancellation_contracts::observer_poll_and_drop_cannot_abort_shard","--nocapture"]).env(CHILD,"1").env("RUST_BACKTRACE","0").kill_on_drop(true).output()).await.unwrap().unwrap();
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
    async fn completed_observer_disposal_failure_keeps_shard_alive() {
        run(Mode::ReadyDrop, false, false, false, false).await;
    }
    #[tokio::test]
    async fn losing_observer_disposal_is_not_silent_success() {
        run(Mode::PendingDrop, true, false, false, false).await;
    }
    #[tokio::test]
    async fn cancelled_observer_drops_pending_batch_on_its_shard() {
        run(Mode::PendingDrop, false, false, false, false).await;
    }
    #[tokio::test]
    async fn shutdown_disposes_pending_observer_without_thread_failure() {
        run(Mode::PendingDrop, false, false, false, true).await;
    }
    #[tokio::test]
    async fn observer_failure_notification_cannot_kill_shard() {
        run(Mode::Plain, false, false, true, false).await;
    }
    #[tokio::test]
    async fn cancelled_observer_disposal_notification_is_protected() {
        run(Mode::PendingDrop, false, false, true, true).await;
    }
    #[tokio::test]
    async fn observer_disposal_uses_safe_business_summary() {
        run(Mode::PendingDrop, true, true, false, false).await;
    }
    #[tokio::test]
    async fn healthy_cancellation_keeps_other_actor_progress() {
        run(Mode::Ready, false, false, false, false).await;
    }
    #[tokio::test]
    async fn healthy_pending_observer_allows_message_completion() {
        run(Mode::Healthy, true, false, false, false).await;
    }
    #[tokio::test]
    async fn observer_disposal_failure_prevents_remaining_batch_requeue() {
        let s = ShardedScheduler::start(1, 1);
        let p = Processor::new(Mode::PendingDrop, true, false, false);
        let mailbox = Arc::new(MpscMailbox::new(
            4,
            ActorPath::placeholder("/failed-with-backlog"),
        ));
        mailbox.set_processor(p.clone());
        for value in [1u64, 2] {
            mailbox
                .push(
                    Box::new(value),
                    crate::thread::config::BackpressureStrategy::Block,
                )
                .await
                .unwrap();
        }
        s.schedule_affinity("/failed-with-backlog", mailbox.clone(), "single")
            .unwrap();
        observe_failure(&s, &p).await;
        let healthy = Processor::new(Mode::Healthy, true, false, false);
        schedule(&s, "/after-failure", &healthy).await;
        tokio::time::timeout(Duration::from_secs(2), healthy.completed.notified())
            .await
            .unwrap();
        s.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(
            p.batch_created.load(Ordering::SeqCst),
            1,
            "failed actor was requeued before observer disposal"
        );
        assert_eq!(p.drops.load(Ordering::SeqCst), 1);
        assert_eq!(p.notices.lock().unwrap().len(), 1);
        assert_eq!(mailbox.len().await, 1);
        assert!(mailbox.schedule_state().try_enqueue());
        mailbox.schedule_state().force_release();
    }
}
