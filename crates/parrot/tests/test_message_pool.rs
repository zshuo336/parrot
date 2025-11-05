//! Message pool microbenchmark: verify all work before reporting elapsed time.
//!
//! Serial: 1M acquisitions; concurrent: four workers with 500k acquisitions each.
//! Measurements include loop bookkeeping; concurrent wall time includes worker
//! startup and retirement. Fixed-order samples are not a performance ranking.
use parrot::thread::message_pool::{self, Pooled};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct PooledEcho {
    value: u64,
    _pad: [u8; 64],
}
impl Default for PooledEcho {
    fn default() -> Self {
        Self {
            value: 0,
            _pad: [0; 64],
        }
    }
}
#[derive(Debug)]
struct Work {
    count: u64,
    checksum: u64,
    fresh: u64,
    reused: u64,
    released: u64,
    overflow: u64,
}
// Independent closed-form oracle, outside the timed loops. u128 keeps the
// intermediate arithmetic exact; the result matches modulo-2^64 checksums.
fn expected(count: u64, offset: u64) -> u64 {
    if count == 0 {
        return 0;
    }
    let triangular = u128::from(count) * u128::from(count - 1) / 2;
    let offsets = u128::from(count) * u128::from(offset);
    (triangular as u64).wrapping_add(offsets as u64)
}
fn validate(work: &Work, count: u64, offset: u64, pooled: bool) {
    assert!(count > 0, "empty workload is not a benchmark");
    assert_eq!(work.count, count, "incomplete work");
    assert_eq!(work.checksum, expected(count, offset), "wrong work result");
    if pooled {
        assert_eq!(work.fresh, 1, "cold worker must allocate its first box");
        assert_eq!(work.reused, count - 1, "subsequent acquisitions must reuse");
        assert_eq!(
            work.released, count,
            "last item must be returned before sampling"
        );
        assert_eq!(work.overflow, 0, "one live item must not overflow the pool");
    } else {
        assert_eq!(
            (work.fresh, work.reused, work.released, work.overflow),
            (0, 0, 0, 0),
            "allocator baseline must not use the pool"
        );
    }
}
fn run(pooled: bool, count: u64, offset: u64) -> (Work, Duration) {
    message_pool::clear_local();
    let before = message_pool::stats();
    let mut checksum = 0u64;
    let mut completed = 0;
    let start = Instant::now();
    if pooled {
        for i in 0..count {
            let mut value: Pooled<PooledEcho> = message_pool::acquire();
            value.value = i.wrapping_add(offset);
            checksum = checksum.wrapping_add(std::hint::black_box(&value).value);
            completed += 1;
        }
    } else {
        for i in 0..count {
            let mut value = std::hint::black_box(Box::new(PooledEcho::default()));
            value.value = i.wrapping_add(offset);
            checksum = checksum.wrapping_add(std::hint::black_box(&value).value);
            completed += 1;
        }
    }
    let elapsed = start.elapsed();
    let after = message_pool::stats();
    let work = Work {
        count: completed,
        checksum,
        fresh: after.acquired_fresh - before.acquired_fresh,
        reused: after.acquired_reused - before.acquired_reused,
        released: after.released - before.released,
        overflow: after.dropped_overflow - before.dropped_overflow,
    };
    validate(&work, count, offset, pooled);
    message_pool::clear_local();
    (work, elapsed)
}
fn concurrent(pooled: bool, count: u64, workers: usize) -> (u64, Duration) {
    assert!(workers > 0 && count > 0, "empty concurrent workload");
    let start = Instant::now();
    let threads: Vec<_> = (0..workers)
        .map(|worker| std::thread::spawn(move || run(pooled, count, worker as u64).0))
        .collect();
    // Join every worker before reporting failure; no detached benchmark tasks.
    let results: Vec<_> = threads.into_iter().map(|thread| thread.join()).collect();
    let elapsed = start.elapsed();
    let mut checksum = 0u64;
    for (worker, result) in results.into_iter().enumerate() {
        let work = result.expect("benchmark worker failed");
        validate(&work, count, worker as u64, pooled);
        checksum = checksum.wrapping_add(work.checksum);
    }
    let oracle = (0..workers).fold(0u64, |sum, worker| {
        sum.wrapping_add(expected(count, worker as u64))
    });
    assert_eq!(checksum, oracle);
    (checksum, elapsed)
}
#[test]
#[ignore = "explicit allocator/pool timing experiment; run --release --ignored --nocapture in isolation"]
fn p1_p2_pool_vs_allocator() {
    const N: u64 = 1_000_000;
    let (allocated, alloc_wall) = run(false, N, 0);
    let (pooled, pool_wall) = run(true, N, 0);
    assert_eq!(allocated.checksum, pooled.checksum);
    println!(
        "[P1] serial count={N} checksum={} allocator_ms={:.3} pool_ms={:.3} fresh={} reused={} released={}",
        allocated.checksum,
        alloc_wall.as_secs_f64() * 1000.0,
        pool_wall.as_secs_f64() * 1000.0,
        pooled.fresh,
        pooled.reused,
        pooled.released
    );
    const T: usize = 4;
    const M: u64 = 500_000;
    let (allocated, alloc_wall) = concurrent(false, M, T);
    let (pooled, pool_wall) = concurrent(true, M, T);
    assert_eq!(allocated, pooled);
    println!(
        "[P2] workers={T} per_worker={M} checksum={allocated} allocator_ms={:.3} pool_ms={:.3}",
        alloc_wall.as_secs_f64() * 1000.0,
        pool_wall.as_secs_f64() * 1000.0
    );
}
#[test]
fn oracle_has_literal_and_wrapping_vectors() {
    assert_eq!(expected(1, 9), 9);
    assert_eq!(expected(4, 7), 34);
    assert_eq!(expected(5, 0), 10);
    assert_eq!(expected(3, u64::MAX), 0);
    assert_eq!(expected(u64::MAX, u64::MAX), 9_223_372_036_854_775_810);
    assert_eq!(expected(1_000_000, 0), 499_999_500_000);
}
#[test]
fn both_paths_execute_exact_serial_work() {
    assert_eq!(run(false, 17, 3).0.checksum, 187);
    assert_eq!(run(true, 17, 3).0.checksum, 187);
}
#[test]
fn counters_are_local_deltas_not_prior_pool_traffic() {
    drop(message_pool::acquire::<PooledEcho>());
    let first = run(true, 8, 0).0;
    let second = run(true, 8, 0).0;
    assert_eq!(
        (first.fresh, first.reused, second.fresh, second.reused),
        (1, 7, 1, 7)
    );
}
#[test]
fn both_concurrent_paths_join_and_verify_each_worker() {
    assert_eq!(concurrent(false, 5, 4).0, 70);
    assert_eq!(concurrent(true, 5, 4).0, 70);
}
#[test]
fn wrong_positive_checksum_is_rejected() {
    let mut work = run(false, 8, 0).0;
    work.checksum = 1;
    assert!(std::panic::catch_unwind(|| validate(&work, 8, 0, false)).is_err());
}
#[test]
fn incomplete_work_and_empty_measurement_are_rejected() {
    let mut work = run(false, 8, 0).0;
    work.count = 7;
    assert!(std::panic::catch_unwind(|| validate(&work, 8, 0, false)).is_err());
    assert!(std::panic::catch_unwind(|| validate(&work, 0, 0, false)).is_err());
}
