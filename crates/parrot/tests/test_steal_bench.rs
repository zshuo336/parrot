//! Instrumented queue experiments, not production scheduler performance gates.
//!
//! Compare an unbounded shared FIFO with bounded local FIFOs and a rotating
//! victim scan. Producers pause after every third attempt; this is not a Poisson
//! arrival model. Producers, local consumers and thieves can all contend.
//! Report admission/rejection and delivery separately, and drain accepted work
//! after producers stop. Identity/result tracking is included in elapsed time.
//! One fixed-order sample cannot establish a speedup or isolation guarantee.

use crossbeam_queue::SegQueue;
use parrot::thread::scheduler::steal::StealDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

enum Queues {
    Central(Box<SegQueue<u64>>),
    Local(Vec<StealDeque<u64>>),
}

impl Queues {
    fn push(&self, id: u64) -> bool {
        match self {
            Self::Central(q) => {
                q.push(id);
                true
            }
            Self::Local(q) => q[id as usize % q.len()].push(id),
        }
    }

    fn pop(&self, worker: usize, cursor: &mut usize) -> Option<u64> {
        match self {
            Self::Central(q) => q.pop(),
            Self::Local(q) => q[worker].pop().or_else(|| {
                for _ in 0..q.len() {
                    let victim = *cursor % q.len();
                    *cursor = cursor.wrapping_add(1);
                    if victim != worker
                        && let Some(id) = q[victim].steal()
                    {
                        return Some(id);
                    }
                }
                None
            }),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Self::Central(q) => q.is_empty(),
            Self::Local(q) => q.iter().all(StealDeque::is_empty),
        }
    }
}

#[derive(Default)]
struct Admission {
    attempts: usize,
    rejected: usize,
    accepted: Vec<u64>,
}

struct Delivery {
    items: Vec<(u64, u64)>,
    drained: bool,
}

// Join every participant before propagating a failure; no detached workers.
fn join_all<T>(handles: Vec<JoinHandle<T>>) -> Result<Vec<T>, String> {
    let results: Vec<_> = handles.into_iter().map(JoinHandle::join).collect();
    results
        .into_iter()
        .map(|result| result.map_err(|_| "participant panicked".to_owned()))
        .collect()
}

fn verify_run(
    admissions: &[Admission],
    deliveries: &[Delivery],
    empty: bool,
) -> Result<usize, String> {
    let mut expected = Vec::new();
    for producer in admissions {
        if producer.attempts != producer.accepted.len() + producer.rejected {
            return Err("attempts must equal accepted plus rejected".into());
        }
        expected.extend_from_slice(&producer.accepted);
    }
    expected.sort_unstable();
    if expected.is_empty() || expected.windows(2).any(|ids| ids[0] == ids[1]) {
        return Err("accepted identities must be nonempty and unique".into());
    }
    let mut actual = Vec::new();
    for worker in deliveries {
        if !worker.drained {
            return Err("worker exceeded drain deadline".into());
        }
        for &(id, result) in &worker.items {
            // Independent arithmetic oracle, including u64 wraparound.
            if result != ((u128::from(id) * 3) & u128::from(u64::MAX)) as u64 {
                return Err("wrong work result".into());
            }
            actual.push(id);
        }
    }
    actual.sort_unstable();
    if actual != expected {
        return Err("accepted and delivered identities differ".into());
    }
    if !empty {
        return Err("queue retained work after all participants joined".into());
    }
    Ok(expected.len())
}

fn drain(
    queues: &Queues,
    worker: usize,
    production_done: &AtomicBool,
    deadline: Instant,
) -> Delivery {
    let mut cursor = worker + 1;
    let mut delivered = Vec::new();
    while Instant::now() < deadline {
        if let Some(id) = queues.pop(worker, &mut cursor) {
            delivered.push((id, id.wrapping_mul(3)));
        } else if production_done.load(Ordering::Acquire) && queues.is_empty() {
            return Delivery {
                items: delivered,
                drained: true,
            };
        } else {
            std::thread::yield_now();
        }
    }
    Delivery {
        items: delivered,
        drained: false,
    }
}

fn experiment(local: bool, workers: usize, producers: usize, limit: usize, run: Duration) {
    assert!(workers > 0 && producers > 0 && producers <= u16::MAX as usize);
    assert!(limit > 0 && (limit as u64) < (1 << 48));
    let queues = Arc::new(if local {
        Queues::Local((0..workers).map(|_| StealDeque::new(4096)).collect())
    } else {
        Queues::Central(Box::default())
    });
    let barrier = Arc::new(Barrier::new(workers + producers + 1));
    let production_done = Arc::new(AtomicBool::new(false));
    let mut consumers = Vec::new();
    for worker in 0..workers {
        let (queues, barrier, production_done) =
            (queues.clone(), barrier.clone(), production_done.clone());
        consumers.push(std::thread::spawn(move || {
            barrier.wait();
            let deadline = Instant::now() + run + Duration::from_secs(10);
            drain(&queues, worker, &production_done, deadline)
        }));
    }
    let mut writers = Vec::new();
    for producer in 0..producers {
        let (queues, barrier) = (queues.clone(), barrier.clone());
        writers.push(std::thread::spawn(move || {
            barrier.wait();
            let deadline = Instant::now() + run;
            let mut admission = Admission::default();
            for seq in 0..limit {
                if Instant::now() >= deadline {
                    break;
                }
                let id = ((producer as u64) << 48) | seq as u64;
                admission.attempts += 1;
                if queues.push(std::hint::black_box(id)) {
                    admission.accepted.push(id);
                } else {
                    admission.rejected += 1;
                }
                if seq.is_multiple_of(3) {
                    std::thread::sleep(Duration::from_micros(1));
                }
            }
            admission
        }));
    }
    let started = Instant::now();
    barrier.wait();
    let admissions = join_all(writers);
    production_done.store(true, Ordering::Release);
    let deliveries = join_all(consumers);
    let wall = started.elapsed();
    let admissions = admissions.expect("producer failed");
    let deliveries = deliveries.expect("consumer failed");
    let total = verify_run(&admissions, &deliveries, queues.is_empty()).unwrap();
    let rejected: usize = admissions.iter().map(|p| p.rejected).sum();
    let counts: Vec<_> = deliveries.iter().map(|w| w.items.len()).collect();
    println!(
        "queue={} accepted={total} rejected={rejected} delivered={total} elapsed={:.3}s delivered/s={:.0} per-worker={counts:?}",
        if local {
            "bounded-local-fifo"
        } else {
            "unbounded-central-fifo"
        },
        wall.as_secs_f64(),
        total as f64 / wall.as_secs_f64()
    );
}

#[test]
#[ignore = "instrumented 8-producer/8-consumer experiment; no performance certification"]
fn q_scheduler_queue_showdown() {
    for local in [false, true] {
        experiment(local, 8, 8, 10_000_000, Duration::from_secs(3));
    }
}

#[test]
fn concurrent_experiments_account_for_all_accepted_work() {
    for local in [false, true] {
        experiment(local, 4, 4, 1000, Duration::from_secs(5));
    }
}

fn fixture() -> (Vec<Admission>, Vec<Delivery>) {
    (
        vec![Admission {
            attempts: 4,
            rejected: 1,
            accepted: vec![0, 2, u64::MAX],
        }],
        vec![Delivery {
            items: vec![(u64::MAX, u64::MAX - 2), (0, 0), (2, 6)],
            drained: true,
        }],
    )
}

#[test]
fn oracle_accepts_reordering_rejection_and_wrapping_results() {
    let (admissions, deliveries) = fixture();
    assert_eq!(verify_run(&admissions, &deliveries, true), Ok(3));
}

#[test]
fn oracle_rejects_loss_duplicate_wrong_result_and_undrained_work() {
    for mutation in 0..6 {
        let (mut a, mut d) = fixture();
        match mutation {
            0 => {
                d[0].items.pop();
            }
            1 => {
                d[0].items[1] = d[0].items[2];
            }
            2 => {
                d[0].items[2].1 = 0;
            }
            3 => {
                d[0].drained = false;
            }
            4 => {
                a[0].rejected = 0;
            }
            _ => {}
        }
        assert!(
            verify_run(&a, &d, mutation != 5).is_err(),
            "mutation {mutation}"
        );
    }
    let (mut a, mut d) = fixture();
    a[0].accepted.clear();
    a[0].rejected = a[0].attempts;
    d[0].items.clear();
    assert!(
        verify_run(&a, &d, true).is_err(),
        "all rejected is not a workload"
    );
}

#[test]
fn queue_admission_reports_full_and_closed_rejection() {
    let queues = Queues::Local(vec![StealDeque::new(4)]);
    for id in 0..4 {
        assert!(queues.push(id));
    }
    assert!(!queues.push(4));
    let Queues::Local(q) = &queues else {
        unreachable!()
    };
    q[0].close();
    assert!(!queues.push(5));
    let mut cursor = 0;
    for id in 0..4 {
        assert_eq!(queues.pop(0, &mut cursor), Some(id));
    }
    assert!(queues.is_empty());
}

#[test]
fn joining_reports_panic_after_joining_other_participants() {
    let completed = Arc::new(AtomicBool::new(false));
    let marker = completed.clone();
    let handles = vec![
        std::thread::spawn(|| panic!("intentional participant failure")),
        std::thread::spawn(move || marker.store(true, Ordering::Release)),
    ];
    assert!(join_all(handles).is_err());
    assert!(completed.load(Ordering::Acquire));
}

#[test]
fn accepted_work_is_drained_after_producers_have_stopped() {
    for queues in [
        Queues::Central(Box::default()),
        Queues::Local(vec![StealDeque::new(4)]),
    ] {
        for id in [0, 2, u64::MAX] {
            assert!(queues.push(id));
        }
        let delivery = drain(
            &queues,
            0,
            &AtomicBool::new(true),
            Instant::now() + Duration::from_secs(1),
        );
        let (admissions, _) = fixture();
        assert_eq!(
            verify_run(&admissions, &[delivery], queues.is_empty()),
            Ok(3)
        );
    }
}
