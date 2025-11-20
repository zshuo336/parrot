//! Cache ownership includes eviction, reentrant destructors and TLS teardown.
use parrot::thread::message_pool::{self as pool, PoolStats, Pooled};
use std::{
    cell::RefCell,
    io::Read,
    panic::{AssertUnwindSafe, catch_unwind},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

fn isolated(name: &str, body: impl FnOnce()) {
    const CHILD: &str = "PARROT_POOL_LIFECYCLE_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        body();
        println!("pool lifecycle complete: {name}");
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, name)
        .env("RUST_BACKTRACE", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Drain both pipes while the child runs, including an abort's diagnostic.
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let timed_out = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break true;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let status = child.wait().unwrap();
    let out = out.join().unwrap();
    let err = err.join().unwrap();
    let stdout = String::from_utf8_lossy(&out);
    let stderr = String::from_utf8_lossy(&err);
    assert!(
        !timed_out && status.success(),
        "pool child failed: timeout={timed_out} status={status:?}\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains(&format!("pool lifecycle complete: {name}")));
    assert!(
        stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "child must actually execute its contract"
    );
}

#[derive(Default)]
struct Counts {
    drops: [AtomicUsize; 4],
    payloads: [AtomicUsize; 4],
    order: Mutex<Vec<usize>>,
}
struct RecursivePayload(Arc<Counts>, usize);
impl Drop for RecursivePayload {
    fn drop(&mut self) {
        self.0.payloads[self.1].fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(RecursivePayload(self.0.clone(), self.1));
    }
}
#[derive(Default)]
struct Value {
    counts: Option<Arc<Counts>>,
    index: usize,
    panic: u8,
    nested: Option<Pooled<Value>>,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}
impl Drop for Value {
    fn drop(&mut self) {
        if let Some(counts) = &self.counts {
            counts.drops[self.index].fetch_add(1, Ordering::SeqCst);
            counts.order.lock().unwrap().push(self.index);
        }
        if let Some(action) = self.on_drop.take() {
            action();
        }
        match self.panic {
            0 => {}
            1 => std::panic::panic_any(self.index),
            2 => std::panic::panic_any(RecursivePayload(
                self.counts.as_ref().unwrap().clone(),
                self.index,
            )),
            _ => unreachable!(),
        }
    }
}
fn value(counts: &Arc<Counts>, index: usize, panic: u8) -> Pooled<Value> {
    let mut value = pool::acquire::<Value>();
    value.counts = Some(counts.clone());
    value.index = index;
    value.panic = panic;
    value
}
fn counters(stats: PoolStats) -> [u64; 4] {
    [
        stats.acquired_fresh,
        stats.acquired_reused,
        stats.released,
        stats.dropped_overflow,
    ]
}
fn delta(before: PoolStats) -> [u64; 4] {
    let after = counters(pool::stats());
    let before = counters(before);
    std::array::from_fn(|i| after[i] - before[i])
}
fn dispose_payload(payload: Box<dyn std::any::Any + Send>) {
    if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(secondary);
    }
}

#[test]
fn clear_drop_reenters_pool_and_preserves_new_cache() {
    isolated("clear_drop_reenters_pool_and_preserves_new_cache", || {
        pool::clear_local();
        let counts = Arc::new(Counts::default());
        let mut cached = value(&counts, 0, 0);
        cached.on_drop = Some(Box::new(|| {
            let _ = pool::stats();
            pool::clear_local();
            let mut next = pool::acquire::<u64>();
            *next = 73;
        }));
        drop(cached);
        pool::clear_local();
        assert_eq!(counts.drops[0].load(Ordering::SeqCst), 1);
        let before = pool::stats();
        let next = pool::acquire::<u64>();
        assert_eq!(*next, 73, "reentrant release belongs to the next cache");
        drop(next);
        assert_eq!(delta(before), [0, 1, 1, 0]);
        pool::clear_local();
    });
}

fn clear_panics(recursive: bool) {
    pool::clear_local();
    let counts = Arc::new(Counts::default());
    let held: Vec<_> = (0..4)
        .map(|i| {
            value(
                &counts,
                i,
                if i % 2 == 0 {
                    0
                } else if recursive {
                    2
                } else {
                    1
                },
            )
        })
        .collect();
    drop(held);
    let result = catch_unwind(pool::clear_local);
    let payload = result.expect_err("explicit clear must retain the first cleanup failure");
    if !recursive {
        let order = counts.order.lock().unwrap();
        let first = *order.iter().find(|&&i| i % 2 == 1).unwrap();
        assert_eq!(
            payload.downcast_ref::<usize>(),
            Some(&first),
            "original first panic payload"
        );
    }
    dispose_payload(payload);
    assert_eq!(
        std::array::from_fn::<_, 4, _>(|i| counts.drops[i].load(Ordering::SeqCst)),
        [1; 4]
    );
    if recursive {
        assert_eq!(
            std::array::from_fn::<_, 4, _>(|i| counts.payloads[i].load(Ordering::SeqCst)),
            [0, 1, 0, 1]
        );
    }
    drop(pool::acquire::<u64>());
    pool::clear_local();
}

#[test]
fn clear_retires_all_before_rethrowing_first_panic() {
    isolated("clear_retires_all_before_rethrowing_first_panic", || {
        clear_panics(false)
    });
}
#[test]
fn clear_protects_recursive_panic_payloads() {
    isolated("clear_protects_recursive_panic_payloads", || {
        clear_panics(true)
    });
}
#[test]
fn cached_nested_values_retire_on_thread_exit() {
    isolated("cached_nested_values_retire_on_thread_exit", || {
        let counts = Arc::new(Counts::default());
        let worker = counts.clone();
        std::thread::spawn(move || {
            let mut outer = value(&worker, 0, 0);
            outer.nested = Some(value(&worker, 1, 0));
            drop(outer);
            assert_eq!(worker.drops[0].load(Ordering::SeqCst), 0);
        })
        .join()
        .unwrap();
        assert_eq!(counts.drops[0].load(Ordering::SeqCst), 1);
        assert_eq!(counts.drops[1].load(Ordering::SeqCst), 1);
    });
}

thread_local! { static HELD: RefCell<Option<Pooled<Value>>> = const { RefCell::new(None) }; }
#[test]
fn pooled_values_can_outlive_pool_tls() {
    isolated("pooled_values_can_outlive_pool_tls", || {
        let counts = Arc::new(Counts::default());
        let worker = counts.clone();
        std::thread::spawn(move || {
            HELD.with(|held| assert!(held.borrow().is_none()));
            let mut held = value(&worker, 0, 0);
            held.on_drop = Some(Box::new(|| {
                assert_eq!(
                    counters(pool::stats()),
                    [0; 4],
                    "destroyed cache has no readable counters"
                );
                pool::clear_local();
                drop(pool::acquire::<u64>());
            }));
            HELD.with(|slot| *slot.borrow_mut() = Some(held));
        })
        .join()
        .unwrap();
        assert_eq!(counts.drops[0].load(Ordering::SeqCst), 1);
    });
}
#[test]
fn thread_exit_isolates_cached_panics_and_retires_other_values() {
    isolated(
        "thread_exit_isolates_cached_panics_and_retires_other_values",
        || {
            let counts = Arc::new(Counts::default());
            let worker = counts.clone();
            std::thread::spawn(move || {
                let held: Vec<_> = (0..4)
                    .map(|i| value(&worker, i, if i % 2 == 0 { 0 } else { 2 }))
                    .collect();
                drop(held);
            })
            .join()
            .unwrap();
            assert_eq!(
                std::array::from_fn::<_, 4, _>(|i| counts.drops[i].load(Ordering::SeqCst)),
                [1; 4]
            );
            assert_eq!(
                std::array::from_fn::<_, 4, _>(|i| counts.payloads[i].load(Ordering::SeqCst)),
                [0, 1, 0, 1]
            );
        },
    );
}
#[test]
fn overflow_disposal_is_reentrant_and_does_not_unwind() {
    isolated("overflow_disposal_is_reentrant_and_does_not_unwind", || {
        pool::clear_local();
        let before = pool::stats();
        let counts = Arc::new(Counts::default());
        let held: Vec<_> = (0..pool::DEFAULT_BUCKET_CAP)
            .map(|_| pool::acquire::<Value>())
            .collect();
        let mut rejected = value(&counts, 0, 2);
        rejected.on_drop = Some(Box::new(|| {
            let _ = pool::stats();
            pool::clear_local();
        }));
        drop(held);
        let result = catch_unwind(AssertUnwindSafe(|| drop(rejected)));
        if let Err(payload) = result {
            dispose_payload(payload);
            panic!("overflow destructor escaped the pooled owner");
        }
        assert_eq!(counts.drops[0].load(Ordering::SeqCst), 1);
        assert_eq!(counts.payloads[0].load(Ordering::SeqCst), 1);
        assert_eq!(delta(before), [129, 0, 128, 1]);
        pool::clear_local();
    });
}

struct FailingDefault;
impl Default for FailingDefault {
    fn default() -> Self {
        panic!("constructor rejected")
    }
}
#[test]
fn failed_default_is_not_a_successful_acquisition() {
    isolated("failed_default_is_not_a_successful_acquisition", || {
        pool::clear_local();
        let before = pool::stats();
        assert!(catch_unwind(pool::acquire::<FailingDefault>).is_err());
        assert_eq!(
            delta(before),
            [0; 4],
            "failed constructor must not count as a fresh acquisition"
        );
        drop(pool::acquire::<u64>());
        pool::clear_local();
    });
}
#[test]
fn cross_thread_release_uses_destination_cache_and_exact_counts() {
    isolated(
        "cross_thread_release_uses_destination_cache_and_exact_counts",
        || {
            pool::clear_local();
            let before = pool::stats();
            let mut message = pool::acquire::<u64>();
            *message = 191;
            assert_eq!(delta(before), [1, 0, 0, 0]);
            std::thread::spawn(move || {
                let before = pool::stats();
                drop(message);
                let reused = pool::acquire::<u64>();
                assert_eq!(*reused, 191);
                drop(reused);
                assert_eq!(delta(before), [0, 1, 2, 0]);
                pool::clear_local();
            })
            .join()
            .unwrap();
            assert_eq!(delta(before), [1, 0, 0, 0]);
            pool::clear_local();
        },
    );
}
#[test]
fn bucket_limit_and_clear_counter_contract_are_exact() {
    isolated("bucket_limit_and_clear_counter_contract_are_exact", || {
        pool::clear_local();
        let before = pool::stats();
        let held: Vec<_> = (0..192).map(|_| pool::acquire::<u64>()).collect();
        drop(held);
        assert_eq!(delta(before), [192, 0, 128, 64]);
        pool::clear_local();
        assert_eq!(
            delta(before),
            [192, 0, 128, 64],
            "clear must retain counters"
        );
        drop(pool::acquire::<u64>());
        assert_eq!(delta(before), [193, 0, 129, 64]);
        pool::clear_local();
    });
}

#[derive(Default)]
struct TypeSlot<const N: usize>(Option<Arc<AtomicUsize>>);
impl<const N: usize> Drop for TypeSlot<N> {
    fn drop(&mut self) {
        if let Some(drops) = &self.0 {
            drops.fetch_add(1, Ordering::SeqCst);
        }
    }
}
fn cache_type<const N: usize>(drops: &Arc<AtomicUsize>) {
    let mut value = pool::acquire::<TypeSlot<N>>();
    value.0 = Some(drops.clone());
    drop(value);
}
#[test]
fn type_limit_is_exact_and_rejected_type_is_destroyed() {
    isolated("type_limit_is_exact_and_rejected_type_is_destroyed", || {
        pool::clear_local();
        let before = pool::stats();
        let drops = Arc::new(AtomicUsize::new(0));
        macro_rules! group { ($base:expr; $($n:expr),*) => { $(cache_type::<{$base + $n}>(&drops);)* }; }
        macro_rules! groups { ($($base:expr),*) => { $(group!($base; 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15);)* }; }
        groups!(
            0, 16, 32, 48, 64, 80, 96, 112, 128, 144, 160, 176, 192, 208, 224, 240
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        cache_type::<256>(&drops);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "257th type must not remain cached"
        );
        assert_eq!(delta(before), [257, 0, 256, 1]);
        pool::clear_local();
        assert_eq!(drops.load(Ordering::SeqCst), 257);
    });
}

#[derive(Default)]
struct ReentrantDefault(u64);
impl Drop for ReentrantDefault {
    fn drop(&mut self) {
        assert_eq!(self.0, 47);
    }
}
struct Constructor;
impl Default for Constructor {
    fn default() -> Self {
        let _ = pool::stats();
        pool::clear_local();
        let mut nested = pool::acquire::<ReentrantDefault>();
        nested.0 = 47;
        Self
    }
}
#[test]
fn default_constructor_can_reenter_cache() {
    isolated("default_constructor_can_reenter_cache", || {
        drop(pool::acquire::<Constructor>());
        pool::clear_local();
    });
}
#[test]
fn clear_during_unwind_preserves_original_panic() {
    isolated("clear_during_unwind_preserves_original_panic", || {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                pool::clear_local();
            }
        }
        let counts = Arc::new(Counts::default());
        drop(value(&counts, 0, 2));
        let result = catch_unwind(|| {
            let _clear = Clear;
            panic!("original caller panic");
        });
        let payload = result.unwrap_err();
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"original caller panic")
        );
        dispose_payload(payload);
        assert_eq!(counts.drops[0].load(Ordering::SeqCst), 1);
        assert_eq!(counts.payloads[0].load(Ordering::SeqCst), 1);
    });
}
