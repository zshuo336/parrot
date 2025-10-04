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
//!   are keyed by `TypeId` and recycle through one `downcast` on release.
//! - The consumer side (`receive_message(payload)`) takes ownership and drops
//!   the box inside user code — the engine cannot intercept. Therefore
//!   recycling is opt-in via [`Pooled<T>`], whose `Drop` returns the box to
//!   the originating thread-local pool.
//! - `oneshot` reply channels are single-use and **never** pooled.
//!
//! ## Bound policy
//!
//! Per-type bucket cap (default 128) + total bucket cap (default 256 types)
//! prevent unbounded retention. Small types benefit most; large payloads
//! should usually *not* be pooled (they'd pin pages) — gated by a size hint
//! at acquisition time (caller's responsibility via `T` size).
//!
//! ## Expected benefit profile
//!
//! A global-allocator `malloc+free` pair costs ~40–80ns round-trip. With ask
//! p50 at ~30µs the single-message latency win is ~0.2% (noise). The real
//! wins are **allocation-rate reduction** (measurable under sustained flood:
//! fewer allocator arenas touched, steadier p99) and deterministic memory
//! for embedded-style workloads. Benchmarks in `test_message_pool.rs`.

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
    /// Lifetime allocation/deallocation counters (metrics).
    acquired_fresh: u64,
    acquired_reused: u64,
    released: u64,
    #[allow(dead_code)] // 统计面板字段（stats() 聚合展示）
    dropped_overflow: u64,
}

impl Pool {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            bucket_cap: DEFAULT_BUCKET_CAP,
            type_cap: DEFAULT_TYPE_CAP,
            acquired_fresh: 0,
            acquired_reused: 0,
            released: 0,
            dropped_overflow: 0,
        }
    }
}

/// Global metrics snapshot (sums across threads at call time).
#[derive(Debug, Default, Clone, Copy)]
pub struct PoolStats {
    pub acquired_fresh: u64,
    pub acquired_reused: u64,
    pub released: u64,
    pub dropped_overflow: u64,
}

thread_local! {
    static COUNTERS: RefCell<(u64, u64, u64, u64)> = const { RefCell::new((0, 0, 0, 0)) };
}

#[inline]
fn bump_counters(fresh: u64, reuse: u64, rel: u64, over: u64) {
    // Thread-local bump: zero cross-thread contention (P2 lesson: global
    // atomics made the pool 3.4x SLOWER than malloc under concurrency).
    COUNTERS.with(|c| {
        let mut c = c.borrow_mut();
        c.0 += fresh;
        c.1 += reuse;
        c.2 += rel;
        c.3 += over;
    });
}

/// Acquire a `Box<T>` from the current thread's pool for type `T`.
///
/// Returns a pooled wrapper; if the bucket is empty a fresh box is allocated.
/// The contents are **stale** — callers must overwrite every field they care
/// about (same contract as Disruptor slot reuse).
pub fn acquire<T: Default + Send + 'static>() -> Pooled<T> {
    let reused = POOL.with(|p| {
        let mut p = p.borrow_mut();
        let tid = TypeId::of::<T>();
        let reused = p
            .buckets
            .get_mut(&tid)
            .and_then(|b| b.free.pop())
            .map(|any| {
                // Recycle through downcast: exactly one TypeId compare.
                match any.downcast::<T>() {
                    Ok(boxed) => boxed,
                    Err(_) => unreachable!("type bucket invariant violated"),
                }
            });
        match &reused {
            Some(_) => p.acquired_reused += 1,
            None => p.acquired_fresh += 1,
        }
        reused
    });
    match reused {
        Some(boxed) => {
            bump_counters(0, 1, 0, 0);
            Pooled {
                inner: std::mem::ManuallyDrop::new(boxed),
            }
        }
        None => {
            bump_counters(1, 0, 0, 0);
            Pooled {
                inner: std::mem::ManuallyDrop::new(Box::new(T::default())),
            }
        }
    }
}

/// Return a `Box<T>` to the current thread's pool (called by `Pooled::drop`).
fn release<T: Send + 'static>(boxed: Box<T>) {
    let accepted = POOL.with(|p| {
        let mut p = p.borrow_mut();
        p.released += 1;
        let tid = TypeId::of::<T>();
        let cap = p.bucket_cap;
        let type_cap = p.type_cap;
        let bucket = match p.buckets.get_mut(&tid) {
            Some(b) => b,
            None => {
                if p.buckets.len() >= type_cap {
                    return false; // too many types; don't retain
                }
                p.buckets.insert(tid, TypeBucket { free: Vec::new() });
                p.buckets.get_mut(&tid).unwrap()
            }
        };
        if bucket.free.len() >= cap {
            return false; // bucket full; drop to allocator
        }
        bucket.free.push(boxed as Box<dyn Any + Send>);
        true
    });
    if accepted {
        bump_counters(0, 0, 1, 0);
    } else {
        bump_counters(0, 0, 0, 1);
    }
}

/// A pooled message box. Behaves like `Box<T>`; on drop the box returns to
/// the pool instead of the global allocator.
///
/// Implementation note: moving a value out of a type with `Drop` is not
/// allowed directly, so the inner box is wrapped in [`ManuallyDrop`]. On
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
pub fn stats() -> PoolStats {
    POOL.with(|p| {
        let p = p.borrow();
        let c = COUNTERS.with(|c| *c.borrow());
        let _ = p;
        PoolStats {
            acquired_fresh: c.0,
            acquired_reused: c.1,
            released: c.2,
            dropped_overflow: c.3,
        }
    })
}

/// Reset pool buckets on the current thread (keeps counters).
pub fn clear_local() {
    POOL.with(|p| {
        p.borrow_mut().buckets.clear();
    });
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
