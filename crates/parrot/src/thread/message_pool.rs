//! # Message recycle pool (ADR-15)
//!
//! Per-type, thread-local free-lists for message boxes. High-frequency small
//! messages (`Echo`, ticks, sensor readings …) are acquired from the pool,
//! filled in place, and returned on drop — skipping the global allocator on
//! both ends.
//!
//! ## Design constraints that shaped this
//!
//! - `BoxedMessage = Box<dyn Any + Send>` erases the concrete type, so pools
//!   are keyed by `TypeId` and recycle through one `downcast` on acquisition.
//! - The consumer side (`receive_message(payload)`) takes ownership and drops
//!   the box inside user code — the engine cannot intercept. Therefore
//!   recycling is opt-in via [`Pooled<T>`], whose `Drop` returns the box to
//!   the current thread's pool. Moving a wrapper to another thread therefore
//!   transfers its eventual cache residency to that thread.
//! - `oneshot` reply channels are single-use and **never** pooled.
//!
//! ## Bound policy
//!
//! Per-type bucket cap (default 128) + total bucket cap (default 256 types)
//! prevent unbounded entry counts. There is no automatic byte-size gate:
//! callers must choose whether a type is suitable for pooling. Large payloads
//! can pin pages when retained, so they should usually bypass this API.
//!
//! ## Expected benefit profile
//!
//! Pooling can reduce fresh allocations when a workload reuses boxes on the
//! same thread. Net throughput depends on allocator caches, type lookup, payload
//! initialization and execution topology. The microbenchmark in
//! `test_message_pool.rs` checks exact work and per-worker counter deltas before
//! reporting elapsed time; it does not establish a general speedup or latency
//! claim for actor messaging.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;

/// Default per-type bucket capacity.
pub const DEFAULT_BUCKET_CAP: usize = 128;
/// Default maximum number of distinct pooled types.
pub const DEFAULT_TYPE_CAP: usize = 256;

struct TypeBucket {
    free: Vec<Box<dyn Any + Send>>,
}

thread_local! {
    static POOL: RefCell<Pool> = RefCell::new(Pool::new());
}

struct Pool {
    buckets: HashMap<TypeId, TypeBucket>,
    bucket_cap: usize,
    type_cap: usize,
    counters: PoolStats,
}

impl Pool {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            bucket_cap: DEFAULT_BUCKET_CAP,
            type_cap: DEFAULT_TYPE_CAP,
            counters: PoolStats::default(),
        }
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        retire_buckets(std::mem::take(&mut self.buckets), false);
    }
}

/// Successful acquisitions and returns for this thread; counters saturate at
/// `u64::MAX`. `released` counts cached returns, `dropped_overflow` rejected ones.
#[derive(Debug, Default, Clone, Copy)]
pub struct PoolStats {
    pub acquired_fresh: u64,
    pub acquired_reused: u64,
    pub released: u64,
    pub dropped_overflow: u64,
}

/// Acquire a `Box<T>` from the current thread's pool for type `T`.
///
/// Returns a pooled wrapper; if the bucket is empty a fresh box is allocated.
/// The contents are **stale** — callers must overwrite every field they care
/// about (same contract as Disruptor slot reuse).
pub fn acquire<T: Default + Send + 'static>() -> Pooled<T> {
    let reused = POOL
        .try_with(|p| {
            let mut p = p.borrow_mut();
            let reused = p
                .buckets
                .get_mut(&TypeId::of::<T>())
                .and_then(|b| b.free.pop());
            if reused.is_some() {
                p.counters.acquired_reused = p.counters.acquired_reused.saturating_add(1);
            }
            reused
        })
        .ok()
        .flatten();
    let boxed = match reused {
        Some(any) => match any.downcast::<T>() {
            Ok(boxed) => boxed,
            Err(_) => unreachable!("type bucket invariant violated"),
        },
        None => {
            // Default may reenter or panic. Count only a completed acquisition,
            // and never hold a pool borrow while running user code.
            let boxed = Box::new(T::default());
            let _ = POOL.try_with(|p| {
                let mut p = p.borrow_mut();
                p.counters.acquired_fresh = p.counters.acquired_fresh.saturating_add(1);
            });
            boxed
        }
    };
    Pooled {
        inner: std::mem::ManuallyDrop::new(boxed),
    }
}

fn report_disposal_failure(payload: Box<dyn Any + Send>) {
    let message = crate::owned_payload::panic_message(payload);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tracing::error!(operation = "message_pool_disposal", %message, "message pool disposal failed");
    }));
    drop(crate::owned_payload::PublicationInput::new(result));
}

fn retire_buckets(buckets: HashMap<TypeId, TypeBucket>, propagate: bool) {
    let mut first = crate::owned_payload::PublicationInput::new(None);
    for (_, bucket) in buckets {
        for value in bucket.free {
            if let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value)))
            {
                if propagate && first.value().is_none() {
                    *first.value_mut() = Some(payload);
                } else {
                    report_disposal_failure(payload);
                }
            }
        }
    }
    if let Some(payload) = first.take() {
        std::panic::resume_unwind(payload);
    }
}

/// Return a `Box<T>` to the current thread's pool (called by `Pooled::drop`).
fn release<T: Send + 'static>(boxed: Box<T>) {
    let mut boxed = Some(boxed);
    let _ = POOL.try_with(|p| {
        let mut p = p.borrow_mut();
        let tid = TypeId::of::<T>();
        let cap = p.bucket_cap;
        let type_cap = p.type_cap;
        let bucket = match p.buckets.get_mut(&tid) {
            Some(b) => b,
            None => {
                if p.buckets.len() >= type_cap {
                    p.counters.dropped_overflow = p.counters.dropped_overflow.saturating_add(1);
                    return;
                }
                p.buckets.insert(tid, TypeBucket { free: Vec::new() });
                p.buckets.get_mut(&tid).unwrap()
            }
        };
        if bucket.free.len() >= cap {
            p.counters.dropped_overflow = p.counters.dropped_overflow.saturating_add(1);
            return;
        }
        bucket
            .free
            .push(boxed.take().expect("unreturned box") as Box<dyn Any + Send>);
        p.counters.released = p.counters.released.saturating_add(1);
    });
    // An unavailable TLS slot or a full cache leaves ownership here. Dispose
    // only after the borrow ends, including nested Pooled fields at thread exit.
    if let Some(boxed) = boxed
        && let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(boxed)))
    {
        report_disposal_failure(payload);
    }
}

/// A pooled message box. Behaves like `Box<T>`; on drop the box returns to
/// the pool instead of the global allocator.
///
/// Implementation note: moving a value out of a type with `Drop` is not
/// allowed directly, so the inner box is wrapped in [`std::mem::ManuallyDrop`]. On
/// drop we `take` it out (leaving a logically-empty slot that is never
/// re-observed) and hand it to the pool.
pub struct Pooled<T: Send + 'static> {
    inner: std::mem::ManuallyDrop<Box<T>>,
}

impl<T: Send + 'static> std::ops::Deref for Pooled<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: Send + 'static> std::ops::DerefMut for Pooled<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: Send + 'static> Drop for Pooled<T> {
    fn drop(&mut self) {
        // SAFETY: take() moves the Box out; `self` is being dropped and the
        // ManuallyDrop slot is never read again. The inner `T` itself is NOT
        // dropped here — recycled contents are stale by contract (callers
        // must overwrite what they read), matching Disruptor slot reuse.
        let boxed = unsafe { std::mem::ManuallyDrop::take(&mut self.inner) };
        release::<T>(boxed);
    }
}

/// Snapshot this thread's pool counters (pool itself is thread-local, so
/// per-thread stats are the natural granularity).
/// Returns zeroes if called by a TLS destructor after the pool was destroyed.
pub fn stats() -> PoolStats {
    POOL.try_with(|p| p.borrow().counters).unwrap_or_default()
}

/// Retire the current cache snapshot, keeping counters and any reentrant returns.
/// Every entry is disposed outside the pool borrow. The first original panic is
/// rethrown after all entries retire, unless the caller is already unwinding.
/// Once TLS teardown destroyed the pool, this is a no-op.
pub fn clear_local() {
    let buckets = POOL
        .try_with(|p| std::mem::take(&mut p.borrow_mut().buckets))
        .unwrap_or_default();
    retire_buckets(buckets, !std::thread::panicking());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Msg {
        seq: u64,
        #[allow(dead_code)]
        payload: [u8; 64],
    }

    impl Default for Msg {
        fn default() -> Self {
            Self {
                seq: 0,
                payload: [0; 64],
            }
        }
    }

    #[test]
    fn acquire_release_cycles_reuse() {
        clear_local();
        let before = stats().acquired_reused;
        {
            let mut m = acquire::<Msg>();
            m.seq = 42;
            assert_eq!(m.seq, 42);
        } // dropped -> released to pool
        {
            let m2 = acquire::<Msg>();
            assert_eq!(m2.seq, 42, "stale contents are preserved (Disruptor-style)");
        }
        let after = stats().acquired_reused;
        assert!(after > before, "second acquire must reuse the recycled box");
    }

    #[test]
    fn bucket_cap_prevents_unbounded_growth() {
        clear_local();
        // Hold MORE than cap simultaneously, then drop all: the pool must
        // retain at most DEFAULT_BUCKET_CAP and drop the overflow to the
        // allocator (counted in dropped_overflow).
        let mut held = Vec::new();
        for _ in 0..(DEFAULT_BUCKET_CAP + 64) {
            held.push(acquire::<Msg>());
        }
        drop(held); // mass release -> overflow beyond cap
        let s = stats();
        assert!(
            s.dropped_overflow >= 1,
            "overflow must be counted, got {:?}",
            s
        );
        // Next acquire still works (from the retained bucket).
        let _ = acquire::<Msg>();
    }
}
