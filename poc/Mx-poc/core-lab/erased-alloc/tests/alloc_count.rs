//! Measure the calling thread only; no sleeps or assumptions about the harness.
use erased_alloc::{two_alloc_baseline, SingleAllocEnvelope};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::ptr;

#[derive(Clone, Copy)]
struct Counts {
    enabled: bool,
    allocs: usize,
    frees: usize,
    last_ptr: *mut u8,
    watched: *mut u8,
    watched_frees: usize,
}
impl Counts {
    const EMPTY: Self = Self {
        enabled: false,
        allocs: 0,
        frees: 0,
        last_ptr: ptr::null_mut(),
        watched: ptr::null_mut(),
        watched_frees: 0,
    };
}
thread_local! { static COUNTS: Cell<Counts> = const { Cell::new(Counts::EMPTY) }; }
struct CountingAlloc;
fn record_alloc(p: *mut u8) {
    let _ = COUNTS.try_with(|cell| {
        let mut c = cell.get();
        if c.enabled && !p.is_null() {
            c.allocs += 1;
            c.last_ptr = p;
            cell.set(c);
        }
    });
}
fn record_free(p: *mut u8) {
    let _ = COUNTS.try_with(|cell| {
        let mut c = cell.get();
        if c.enabled {
            c.frees += 1;
        }
        if !p.is_null() && p == c.watched {
            c.watched_frees += 1;
        }
        cell.set(c);
    });
}
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        record_alloc(p);
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        record_free(p);
        unsafe { System.dealloc(p, layout) };
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(p, layout, size) };
        if !new.is_null() {
            record_free(p);
            record_alloc(new);
        }
        new
    }
}
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;
struct Window;
impl Drop for Window {
    fn drop(&mut self) {
        COUNTS.with(|cell| {
            let mut c = cell.get();
            c.enabled = false;
            cell.set(c);
        });
    }
}
fn measure<T>(f: impl FnOnce() -> T) -> (T, Counts) {
    COUNTS.with(|cell| {
        cell.set(Counts {
            enabled: true,
            ..Counts::EMPTY
        })
    });
    let window = Window;
    let value = f();
    drop(window);
    (value, COUNTS.with(Cell::get))
}

#[test]
fn exported_baseline_really_allocates_and_frees_two_blocks() {
    let (envelope, construction) = measure(|| black_box(two_alloc_baseline(black_box([7u64; 32]))));
    assert_eq!(
        construction.allocs, 2,
        "measure the exported baseline, not a test-only substitute"
    );
    let (_, retirement) = measure(|| drop(envelope));
    assert_eq!(retirement.frees, 2);
}

#[test]
fn payload_panic_still_deallocates_the_envelope_block() {
    struct Panics(u64);
    impl Drop for Panics {
        fn drop(&mut self) {
            assert_eq!(self.0, 7);
            panic!("intentional payload destructor panic");
        }
    }
    let (envelope, construction) = measure(|| SingleAllocEnvelope::new(Panics(7)));
    assert_eq!(construction.allocs, 1);
    COUNTS.with(|cell| {
        cell.set(Counts {
            watched: construction.last_ptr,
            ..Counts::EMPTY
        })
    });
    let result = std::panic::catch_unwind(|| drop(envelope));
    let freed = COUNTS.with(|cell| {
        let result = cell.get().watched_frees;
        cell.set(Counts::EMPTY);
        result
    });
    assert!(result.is_err(), "payload panic must still propagate");
    assert_eq!(
        freed, 1,
        "the exact allocation holding the payload must be freed during unwind"
    );
}

#[test]
fn single_block_roundtrip_allocates_once_and_downcast_frees_without_allocating() {
    let (envelope, construction) =
        measure(|| black_box(SingleAllocEnvelope::new(black_box([9u64; 32]))));
    assert_eq!((construction.allocs, construction.frees), (1, 0));
    let (message, extraction) = measure(|| envelope.downcast::<[u64; 32]>().ok().unwrap());
    assert_eq!(message, [9; 32]);
    assert_eq!((extraction.allocs, extraction.frees), (0, 1));
}

#[test]
fn wrong_type_keeps_the_original_allocation_and_payload_alive() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Tracked(Arc<AtomicUsize>);
    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let envelope = SingleAllocEnvelope::new(Tracked(drops.clone()));
    let (envelope, mismatch) = measure(|| envelope.downcast::<String>().err().unwrap());
    assert_eq!((mismatch.allocs, mismatch.frees), (0, 0));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let message = envelope.downcast::<Tracked>().ok().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(message);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn zero_sized_payload_still_drops_once_and_header_is_freed() {
    thread_local! { static DROPS: Cell<usize> = const { Cell::new(0) }; }
    #[repr(align(4096))]
    struct AlignedZst;
    impl Drop for AlignedZst {
        fn drop(&mut self) {
            assert_eq!((self as *const Self).align_offset(4096), 0);
            DROPS.with(|c| c.set(c.get() + 1));
        }
    }
    DROPS.with(|c| c.set(0));
    let (envelope, construction) = measure(|| SingleAllocEnvelope::new(AlignedZst));
    assert_eq!(construction.allocs, 1);
    let (_, retirement) = measure(|| drop(envelope));
    assert_eq!((retirement.allocs, retirement.frees), (0, 1));
    assert_eq!(DROPS.with(Cell::get), 1);
    let envelope = SingleAllocEnvelope::new(AlignedZst);
    let (message, extraction) = measure(|| envelope.downcast::<AlignedZst>().ok().unwrap());
    assert_eq!((extraction.allocs, extraction.frees), (0, 1));
    assert_eq!(DROPS.with(Cell::get), 1);
    drop(message);
    assert_eq!(DROPS.with(Cell::get), 2);
}

#[test]
fn over_aligned_payload_keeps_data_and_drops_in_aligned_storage() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    #[repr(align(4096))]
    struct Aligned {
        values: [u64; 32],
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Aligned {
        fn drop(&mut self) {
            assert_eq!((self as *const Self).align_offset(4096), 0);
            assert_eq!(self.values, [42; 32]);
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let (envelope, construction) = measure(|| {
        SingleAllocEnvelope::new(Aligned {
            values: [42; 32],
            drops: drops.clone(),
        })
    });
    assert_eq!(construction.allocs, 1);
    let (_, retirement) = measure(|| drop(envelope));
    assert_eq!(retirement.frees, 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let envelope = SingleAllocEnvelope::new(Aligned {
        values: [42; 32],
        drops: drops.clone(),
    });
    let message = envelope.downcast::<Aligned>().ok().unwrap();
    assert_eq!(((&message) as *const Aligned).align_offset(4096), 0);
    assert_eq!(message.values, [42; 32]);
    drop(message);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn envelope_transfers_send_but_non_sync_payload_to_another_thread() {
    let envelope = SingleAllocEnvelope::new(Cell::new(41u64));
    let result = std::thread::spawn(move || {
        let (value, counts) = measure(|| envelope.downcast::<Cell<u64>>().ok().unwrap());
        assert_eq!((counts.allocs, counts.frees), (0, 1));
        value.set(value.get() + 1);
        value.into_inner()
    })
    .join()
    .unwrap();
    assert_eq!(result, 42);
}

#[test]
fn allocation_window_excludes_other_threads_without_sleep() {
    use std::sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    };
    let phase = Arc::new(AtomicU8::new(0));
    let other_phase = phase.clone();
    let other = std::thread::spawn(move || {
        while other_phase.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        for n in 0..32 {
            drop(black_box(SingleAllocEnvelope::new(black_box(n))));
        }
        other_phase.store(2, Ordering::Release);
    });
    let (_, counts) = measure(|| {
        // A Barrier may lazily allocate parking state on this thread. Use an
        // allocation-free handshake so the measured window contains no such
        // unrelated allocation, while all 32 other-thread allocations occur.
        phase.store(1, Ordering::Release);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while phase.load(Ordering::Acquire) != 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "other thread did not finish"
            );
            std::thread::yield_now();
        }
    });
    other.join().unwrap();
    assert_eq!((counts.allocs, counts.frees), (0, 0));
}

#[test]
fn zero_sized_baseline_has_one_allocation_and_preserves_boxed_type() {
    struct Zst;
    let (envelope, counts) = measure(|| black_box(two_alloc_baseline(black_box(Zst))));
    assert_eq!(counts.allocs, 1);
    let (_, counts) = measure(|| drop(envelope.downcast::<Box<Zst>>().ok().unwrap()));
    assert_eq!((counts.allocs, counts.frees), (0, 1));
}

#[test]
fn ordinary_erasure_coercion_does_not_add_an_allocation() {
    let message = Box::new(black_box([3u64; 32]));
    let (erased, counts) = measure(|| -> Box<dyn std::any::Any + Send> { black_box(message) });
    assert_eq!((counts.allocs, counts.frees), (0, 0));
    assert_eq!(*erased.downcast::<[u64; 32]>().unwrap(), [3; 32]);
}
