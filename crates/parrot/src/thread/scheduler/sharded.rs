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
//! producer --hash(key)--> shard[i] MPSC --> thread[i] loop (pop -> run)
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
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
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
            threads.push(
                std::thread::Builder::new()
                    .name(format!("parrot-shard-{}", i))
                    .spawn(move || {
                        shard_loop(i, rx, tx2, stop_rx, shutdown, shutdown_notify, batch_size);
                    })
                    .expect("spawn shard thread"),
            );
        }

        Arc::new(Self {
            shards,
            shutdown,
            shutdown_notify,
            threads: Mutex::new(threads),
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

    /// Graceful stop: set the flag and join all threads.
    pub fn shutdown(&self) {
        self.request_shutdown();
        loop {
            let next = { self.threads.lock().unwrap().pop() };
            match next {
                Some(t) => {
                    let _ = t.join();
                }
                None => break,
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
            {
                let mut threads = self.threads.lock().unwrap();
                let mut i = 0;
                while i < threads.len() {
                    if threads[i].is_finished() {
                        let _ = threads.swap_remove(i).join();
                    } else {
                        i += 1;
                    }
                }
                if threads.is_empty() {
                    return Ok(());
                }
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
    // Catch both future construction and polling panics, including actor init.
    let work = std::panic::AssertUnwindSafe(async {
        if !processor.is_initialized() {
            processor.clone().initialize_and_start_erased().await?;
        }
        processor
            .clone()
            .process_batch_erased(mailbox.clone(), batch_size, false)
            .await
    })
    .catch_unwind();
    let result = tokio::select! {
        biased;
        _ = processor.processing_cancelled() => {
            mailbox.schedule_state().force_release();
            return;
        }
        result = work => result,
    };
    let failure = match result {
        Ok(Ok(_)) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some("actor panicked during shard initialization or processing".into()),
    };
    if let Some(error) = failure {
        tracing::error!(shard_id, path = %mailbox.path().path, %error, "shard actor failed");
        mailbox.schedule_state().force_release();
        processor.notify_system_panic(error);
        return;
    }
    if !item.active.load(Ordering::Acquire) {
        mailbox.schedule_state().force_release();
        return;
    }
    let has_more = mailbox.has_more_messages().await;
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
