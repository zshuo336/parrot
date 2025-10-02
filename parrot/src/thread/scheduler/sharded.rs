//! # Sharded (thread-affinity) scheduler
//!
//! A fixed pool of OS threads where **each thread owns exactly one shard
//! queue**. Actors (or messages) tagged with an affinity key are hashed to a
//! shard and processed **only** by that shard's thread.
//!
//! ## Why this is faster than the shared pool (ADR-14)
//!
//! Shared pool per-message costs: global `SegQueue` push/pop (CAS on a
//! shared, cache-line-contended head), `Notify` wakeup, worker steal
//! arbitration. Sharded scheduler costs: one MPSC push to a thread-private
//! queue; the owning thread is already spinning on it — **no global
//! synchronization at all** after the initial routing decision.
//!
//! Wins: zero cross-thread cache-line contention on the hot path, better
//! cache locality (same affinity domain ⇒ same thread ⇒ warm L1/L2 for the
//! actor's state), no wakeup latency under sustained load (thread parks only
//! when its shard is truly empty), and strict per-shard FIFO that gives
//! tail-latency isolation between affinity domains.
//!
//! Trade-offs: idle shards cannot help overloaded ones (no work stealing).
//! Use when affinity domains are known and roughly balanced (sharded state,
//! partitioned topics, per-core session pinning).
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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use flume::{unbounded, Receiver, Sender};
use tokio::runtime::Builder;

use crate::thread::mailbox::Mailbox;

/// A scheduled item: a mailbox that became ready (has messages to process).
struct ShardItem {
    mailbox: Arc<dyn Mailbox>,
}

/// Per-shard handle shared with producers (wake hooks).
struct Shard {
    tx: Sender<ShardItem>,
    /// Approximate queue length (for metrics/负载观测).
    len: AtomicUsize,
}

impl Shard {
    fn new() -> Self {
        let (tx, _) = unbounded();
        Self { tx, len: AtomicUsize::new(0) }
    }
}

/// Fixed-size pool of affinity shards.
///
/// `shards` must be created via [`ShardedScheduler::start`].
pub struct ShardedScheduler {
    shards: Vec<Arc<Shard>>,
    shutdown: Arc<AtomicBool>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// Registry of scheduled actor paths -> shard index (for deschedule).
    assigned: Mutex<HashMap<String, usize>>,
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
        let mut shards = Vec::with_capacity(n);
        let mut threads = Vec::with_capacity(n);

        for i in 0..n {
            let (tx, rx) = unbounded::<ShardItem>();
            let tx2 = tx.clone();
            let shard = Arc::new(Shard { tx, len: AtomicUsize::new(0) });
            shards.push(shard);

            let shutdown = shutdown.clone();
            let batch_size = batch_size;
            threads.push(
                std::thread::Builder::new()
                    .name(format!("parrot-shard-{}", i))
                    .spawn(move || {
                        shard_loop(i, rx, tx2, shutdown, batch_size);
                    })
                    .expect("spawn shard thread"),
            );
        }

        Arc::new(Self {
            shards,
            shutdown,
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
        {
            let mut assigned = self.assigned.lock().unwrap();
            assigned.insert(path.to_string(), idx);
        }

        // Re-arm the wake hook: every message arrival pushes this mailbox
        // into its home shard (subject to the schedule-slot single-entry
        // invariant).
        let shard = self.shards[idx].clone();
        let weak = Arc::downgrade(&mailbox);
        let shutting = self.shutdown.clone();
        mailbox.set_wake_hook(Arc::new(move || {
            if shutting.load(Ordering::Relaxed) {
                return;
            }
            if let Some(strong) = weak.upgrade() {
                if strong.schedule_state().try_enqueue() {
                    shard.len.fetch_add(1, Ordering::Relaxed);
                    let _ = shard.tx.send(ShardItem { mailbox: strong });
                }
            }
        }));

        // Initial enqueue so an already-nonempty mailbox is drained.
        if mailbox.schedule_state().try_enqueue() {
            let shard = self.shards[idx].clone();
            shard.len.fetch_add(1, Ordering::Relaxed);
            let _ = shard.tx.send(ShardItem { mailbox: mailbox.clone() });
        }
        Ok(())
    }

    /// Deschedule an actor (stop routing its wake-ups).
    pub fn deschedule(&self, path: &str) -> Result<(), String> {
        self.assigned.lock().unwrap().remove(path);
        Ok(())
    }

    /// Shard lengths (for metrics).
    pub fn shard_lengths(&self) -> Vec<usize> {
        self.shards.iter().map(|s| s.len.load(Ordering::Relaxed)).collect()
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Graceful stop: set the flag and join all threads.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
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
}

/// The per-shard event loop: pop mailboxes, run a batch, re-enqueue while
/// more messages remain.
fn shard_loop(
    shard_id: usize,
    rx: Receiver<ShardItem>,
    tx: Sender<ShardItem>,
    shutdown: Arc<AtomicBool>,
    batch_size: usize,
) {
    // Current-thread runtime: actor futures on THIS thread only.
    let rt = Builder::new_current_thread().enable_all().build().expect("shard runtime");
    let _guard = rt.enter();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(item) => {
                let mailbox = item.mailbox;
                if let Some(processor) = mailbox.get_processor() {
                    if !processor.is_initialized() {
                        let p = processor.clone();
                        if let Err(e) = rt.block_on(async move { p.initialize_and_start_erased().await }) {
                            eprintln!("[shard-{}] init failed for {:?}: {}", shard_id, mailbox.path().path, e);
                            mailbox.schedule_state().force_release();
                            continue;
                        }
                    }
                    let p = processor.clone();
                    let mb = mailbox.clone();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        p.process_batch_erased(mb.clone(), batch_size, false)
                    }));
                    match result {
                        Ok(fut) => {
                            if let Err(e) = rt.block_on(fut) {
                                eprintln!("[shard-{}] batch error for {:?}: {}", shard_id, mailbox.path().path, e);
                                mailbox.schedule_state().force_release();
                                continue;
                            }
                        }
                        Err(panic) => {
                            let msg = panic
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                                .unwrap_or_else(|| "<non-string panic>".into());
                            eprintln!("[shard-{}] PANIC in {:?}: {}", shard_id, mailbox.path().path, msg);
                            mailbox.schedule_state().force_release();
                            continue;
                        }
                    }
                    // Re-enqueue while more work remains (same shard only).
                    let has_more = rt.block_on(async { mailbox.has_more_messages().await });
                    if mailbox.schedule_state().release(has_more) {
                        let _ = tx.send(ShardItem { mailbox });
                    }
                } else {
                    mailbox.schedule_state().force_release();
                }
            }
            Err(flume::RecvTimeoutError::Timeout) => continue,
            Err(flume::RecvTimeoutError::Disconnected) => break,
        }
    }
}
