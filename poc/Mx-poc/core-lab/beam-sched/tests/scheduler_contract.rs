use beam_sched::{Priority, ReductionScheduler};
use std::sync::mpsc;
use std::time::Duration;
const DEADLINE: Duration = Duration::from_secs(2);

#[test]
fn a_blocked_actor_does_not_lock_all_other_actors() {
    let scheduler = ReductionScheduler::new(2).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let blocked = scheduler
        .spawn(
            Box::new(move |_, _| {
                entered.send(()).unwrap();
                gate.recv().unwrap();
                true
            }),
            0,
        )
        .unwrap();
    let (done, received) = mpsc::channel();
    let independent = scheduler
        .spawn(
            Box::new(move |_, _| {
                done.send(()).unwrap();
                true
            }),
            0,
        )
        .unwrap();
    scheduler
        .send(blocked, Box::new(()), Priority::Normal)
        .unwrap();
    ready.recv_timeout(DEADLINE).unwrap();
    scheduler
        .send(independent, Box::new(()), Priority::Normal)
        .unwrap();
    let observed = received.recv_timeout(DEADLINE);
    release.send(()).unwrap();
    scheduler.shutdown().unwrap();
    assert!(
        observed.is_ok(),
        "other worker cannot progress while the global process lock is held"
    );
}

#[test]
fn configured_budget_rotates_actors_instead_of_only_incrementing_a_counter() {
    let scheduler = ReductionScheduler::with_budget(1, 1).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let blocker = scheduler
        .spawn(
            Box::new(move |_, _| {
                entered.send(()).unwrap();
                gate.recv().unwrap();
                true
            }),
            0,
        )
        .unwrap();
    let (seen, events) = mpsc::channel();
    let slow_seen = seen.clone();
    let slow = scheduler
        .spawn(
            Box::new(move |_, _| {
                slow_seen.send("slow").unwrap();
                true
            }),
            0,
        )
        .unwrap();
    let fast = scheduler
        .spawn(
            Box::new(move |_, _| {
                seen.send("fast").unwrap();
                true
            }),
            0,
        )
        .unwrap();
    scheduler
        .send(blocker, Box::new(()), Priority::Normal)
        .unwrap();
    ready.recv_timeout(DEADLINE).unwrap();
    for _ in 0..5 {
        scheduler
            .send(slow, Box::new(()), Priority::Normal)
            .unwrap();
    }
    scheduler
        .send(fast, Box::new(()), Priority::Normal)
        .unwrap();
    release.send(()).unwrap();
    let observed: Vec<_> = (0..6)
        .map(|_| events.recv_timeout(DEADLINE).unwrap())
        .collect();
    scheduler.shutdown().unwrap();
    assert_eq!(&observed[..2], &["slow", "fast"]);
}

struct DropNotice(mpsc::Sender<()>);
impl Drop for DropNotice {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
#[test]
fn stopping_an_actor_releases_its_heap_before_scheduler_destruction() {
    let scheduler = ReductionScheduler::new(1).unwrap();
    let (dropped, observed) = mpsc::channel();
    let mut notice = Some(DropNotice(dropped));
    let actor = scheduler
        .spawn(
            Box::new(move |heap, _| {
                heap.put(0, Box::new(notice.take().unwrap()));
                false
            }),
            1,
        )
        .unwrap();
    scheduler
        .send(actor, Box::new(()), Priority::Normal)
        .unwrap();
    let result = observed.recv_timeout(DEADLINE);
    scheduler.shutdown().unwrap();
    assert!(result.is_ok(), "dead actor heap was retained");
}

#[test]
fn shutdown_releases_idle_actors_and_their_captured_resources() {
    let scheduler = ReductionScheduler::new(2).unwrap();
    let (dropped, observed) = mpsc::channel();
    let notice = DropNotice(dropped);
    scheduler
        .spawn(
            Box::new(move |_, _| {
                let _ = &notice;
                true
            }),
            0,
        )
        .unwrap();
    scheduler.shutdown().unwrap();
    assert!(
        observed.recv_timeout(DEADLINE).is_ok(),
        "shutdown retained actors/workers"
    );
}

use beam_sched::{SchedulerError, Step};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[test]
fn a_long_resumable_message_yields_to_other_actors_and_high_priority_in_its_own_mailbox() {
    let scheduler = ReductionScheduler::with_budget(1, 2).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (record, events) = mpsc::channel();
    let long_record = record.clone();
    // The remaining work is stored in the message itself across turns.
    let mut first = true;
    let long = scheduler
        .spawn_resumable(
            Box::new(move |_, message, budget| {
                let left = message.downcast_mut::<u64>().unwrap();
                if *left == 0 {
                    long_record.send("high").unwrap();
                    return Step::Complete;
                }
                if first {
                    first = false;
                    entered.send(()).unwrap();
                    gate.recv().unwrap();
                }
                while *left > 0 && budget.consume(1) {
                    *left -= 1;
                    long_record.send("step").unwrap();
                }
                if *left == 0 {
                    Step::Complete
                } else {
                    Step::Yield
                }
            }),
            0,
        )
        .unwrap();
    let fast = scheduler
        .spawn(
            Box::new(move |_, _| {
                record.send("fast").unwrap();
                true
            }),
            0,
        )
        .unwrap();
    scheduler
        .send(long, Box::new(8u64), Priority::Normal)
        .unwrap();
    ready.recv_timeout(DEADLINE).unwrap();
    scheduler
        .send(fast, Box::new(()), Priority::Normal)
        .unwrap();
    scheduler
        .send(long, Box::new(0u64), Priority::High)
        .unwrap();
    release.send(()).unwrap();
    let log: Vec<_> = (0..10)
        .map(|_| events.recv_timeout(DEADLINE).unwrap())
        .collect();
    scheduler.shutdown().unwrap();
    assert_eq!(&log[..3], &["step", "step", "high"]);
    let fast_position = log.iter().position(|&e| e == "fast").unwrap();
    assert!(
        fast_position < log.iter().rposition(|&e| e == "step").unwrap(),
        "fast actor ran only after long work completed: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|&&e| e == "step").count(),
        8,
        "continuation lost or replayed work"
    );
    assert_eq!(scheduler.worker_counts(), (1, 1));
}

#[test]
fn low_priority_eventually_runs_under_a_high_priority_backlog() {
    let scheduler = ReductionScheduler::with_budget(1, 1).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let blocker = scheduler
        .spawn(
            Box::new(move |_, _| {
                entered.send(()).unwrap();
                gate.recv().unwrap();
                true
            }),
            0,
        )
        .unwrap();
    let (seen, events) = mpsc::channel();
    let actor = scheduler
        .spawn(
            Box::new(move |_, m| {
                seen.send(*m.downcast::<u8>().unwrap()).unwrap();
                true
            }),
            0,
        )
        .unwrap();
    scheduler
        .send(blocker, Box::new(()), Priority::Normal)
        .unwrap();
    ready.recv_timeout(DEADLINE).unwrap();
    scheduler.send(actor, Box::new(1u8), Priority::Low).unwrap();
    for _ in 0..32 {
        scheduler
            .send(actor, Box::new(9u8), Priority::High)
            .unwrap();
    }
    release.send(()).unwrap();
    let log: Vec<_> = (0..33)
        .map(|_| events.recv_timeout(DEADLINE).unwrap())
        .collect();
    scheduler.shutdown().unwrap();
    assert_eq!(log[0], 9, "high should overtake queued low work");
    assert!(
        log.iter().position(|&n| n == 1).unwrap() <= 8,
        "low starved: {log:?}"
    );
}

#[test]
fn same_actor_never_runs_concurrently_and_keeps_fifo_within_priority() {
    let scheduler = ReductionScheduler::with_budget(4, 3).unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let inside = active.clone();
    let maximum = Arc::new(AtomicUsize::new(0));
    let max = maximum.clone();
    let (seen, events) = mpsc::channel();
    let actor = scheduler
        .spawn(
            Box::new(move |_, m| {
                let n = inside.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(n, Ordering::SeqCst);
                std::thread::yield_now();
                seen.send(*m.downcast::<usize>().unwrap()).unwrap();
                inside.fetch_sub(1, Ordering::SeqCst);
                true
            }),
            0,
        )
        .unwrap();
    for i in 0usize..500 {
        scheduler
            .send(actor, Box::new(i), Priority::Normal)
            .unwrap();
    }
    for i in 0usize..500 {
        assert_eq!(events.recv_timeout(DEADLINE).unwrap(), i);
    }
    scheduler.shutdown().unwrap();
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
    assert_eq!(scheduler.worker_counts(), (4, 4));
}

#[test]
fn panic_retires_the_actor_its_heap_and_queued_messages_without_killing_worker() {
    let scheduler = ReductionScheduler::new(1).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (drops, notices) = mpsc::channel();
    let heap_drop = drops.clone();
    let doomed = scheduler
        .spawn(
            Box::new(move |heap, _| {
                heap.put(0, Box::new(DropNotice(heap_drop.clone())));
                entered.send(()).unwrap();
                gate.recv().unwrap();
                panic!("handler panic")
            }),
            1,
        )
        .unwrap();
    let (done, received) = mpsc::channel();
    let survivor = scheduler
        .spawn(
            Box::new(move |_, _| {
                done.send(()).unwrap();
                true
            }),
            0,
        )
        .unwrap();
    scheduler
        .send(doomed, Box::new(()), Priority::Normal)
        .unwrap();
    ready.recv_timeout(DEADLINE).unwrap();
    for _ in 0..3 {
        scheduler
            .send(
                doomed,
                Box::new(DropNotice(drops.clone())),
                Priority::Normal,
            )
            .unwrap();
    }
    scheduler
        .send(survivor, Box::new(()), Priority::Normal)
        .unwrap();
    release.send(()).unwrap();
    received.recv_timeout(DEADLINE).unwrap();
    for _ in 0..4 {
        notices.recv_timeout(DEADLINE).unwrap();
    }
    assert_eq!(scheduler.handler_panics(), 1);
    assert_eq!(scheduler.alive_count(), 1);
    assert_eq!(
        scheduler
            .send(doomed, Box::new(()), Priority::Normal)
            .unwrap_err()
            .0,
        SchedulerError::UnknownActor
    );
    scheduler.shutdown().unwrap();
    assert_eq!(scheduler.worker_counts(), (1, 1));
}

#[test]
fn bounded_admission_returns_unsent_message_and_shutdown_rejects_new_work() {
    let scheduler = ReductionScheduler::new(1).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let blocker = scheduler
        .spawn(
            Box::new(move |_, _| {
                entered.send(()).unwrap();
                gate.recv().unwrap();
                true
            }),
            0,
        )
        .unwrap();
    let actor = scheduler.spawn(Box::new(|_, _| true), 0).unwrap();
    scheduler
        .send(blocker, Box::new(()), Priority::Normal)
        .unwrap();
    ready.recv_timeout(DEADLINE).unwrap();
    let mut admitted = 0usize;
    loop {
        match scheduler.send(actor, Box::new(admitted), Priority::Normal) {
            Ok(()) => {
                admitted += 1;
                assert!(admitted <= 1024);
            }
            Err((kind, message)) => {
                assert_eq!(kind, SchedulerError::MailboxFull);
                assert_eq!(*message.downcast::<usize>().unwrap(), admitted);
                break;
            }
        }
    }
    scheduler.request_shutdown();
    release.send(()).unwrap();
    scheduler.shutdown().unwrap();
    assert_eq!(
        scheduler
            .send(actor, Box::new(()), Priority::Normal)
            .unwrap_err()
            .0,
        SchedulerError::Stopped
    );
    assert_eq!(
        scheduler.spawn(Box::new(|_, _| true), 0).unwrap_err(),
        SchedulerError::Stopped
    );
    assert_eq!(scheduler.worker_counts(), (1, 1));
    assert_eq!(scheduler.alive_count(), 0);
}

#[test]
fn callbacks_and_destructors_can_reenter_without_self_joining() {
    let scheduler = ReductionScheduler::new(1).unwrap();
    let weak = Arc::downgrade(&scheduler);
    let (sent, received) = mpsc::channel();
    let actor = scheduler
        .spawn(
            Box::new(move |_, _| {
                let scheduler = weak.upgrade().unwrap();
                assert_eq!(scheduler.alive_count(), 1);
                sent.send(scheduler.shutdown()).unwrap();
                false
            }),
            0,
        )
        .unwrap();
    scheduler
        .send(actor, Box::new(()), Priority::Normal)
        .unwrap();
    assert_eq!(
        received.recv_timeout(DEADLINE).unwrap(),
        Err(SchedulerError::CalledFromWorker)
    );
    scheduler.shutdown().unwrap();
    assert_eq!(scheduler.worker_counts(), (1, 1));
}

#[test]
fn concurrent_shutdown_waits_for_all_actor_destructors_and_joins_workers() {
    let scheduler = ReductionScheduler::new(2).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    struct BlockingDrop {
        entered: mpsc::Sender<()>,
        gate: Mutex<mpsc::Receiver<()>>,
    }
    impl Drop for BlockingDrop {
        fn drop(&mut self) {
            self.entered.send(()).unwrap();
            self.gate.get_mut().unwrap().recv().unwrap();
        }
    }
    let value = BlockingDrop {
        entered,
        gate: Mutex::new(gate),
    };
    scheduler
        .spawn(
            Box::new(move |_, _| {
                let _ = &value;
                true
            }),
            0,
        )
        .unwrap();
    let (finished, observed) = mpsc::channel();
    let first = scheduler.clone();
    let tx = finished.clone();
    let a = std::thread::spawn(move || {
        first.shutdown().unwrap();
        tx.send(()).unwrap();
    });
    ready.recv_timeout(DEADLINE).unwrap();
    let second = scheduler.clone();
    let b = std::thread::spawn(move || {
        second.shutdown().unwrap();
        finished.send(()).unwrap();
    });
    let premature = observed.recv_timeout(Duration::from_millis(50));
    release.send(()).unwrap();
    a.join().unwrap();
    b.join().unwrap();
    assert!(
        premature.is_err(),
        "shutdown returned before destructor completed"
    );
    observed.recv_timeout(DEADLINE).unwrap();
    observed.recv_timeout(DEADLINE).unwrap();
    assert_eq!(scheduler.worker_counts(), (2, 2));
}

#[test]
fn dropping_last_scheduler_owner_releases_idle_actor_resources() {
    let scheduler = ReductionScheduler::new(2).unwrap();
    let (sent, received) = mpsc::channel();
    let resource = DropNotice(sent);
    scheduler
        .spawn(
            Box::new(move |_, _| {
                let _ = &resource;
                true
            }),
            0,
        )
        .unwrap();
    drop(scheduler);
    received.recv_timeout(DEADLINE).unwrap();
}

#[test]
fn zero_configuration_is_rejected_and_yield_without_charging_still_rotates() {
    assert!(matches!(
        ReductionScheduler::new(0),
        Err(SchedulerError::InvalidWorkers)
    ));
    assert!(matches!(
        ReductionScheduler::with_budget(1, 0),
        Err(SchedulerError::InvalidBudget)
    ));
    let scheduler = ReductionScheduler::with_budget(1, 1).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (seen, events) = mpsc::channel();
    let slow_seen = seen.clone();
    let mut first = true;
    let slow = scheduler
        .spawn_resumable(
            Box::new(move |_, m, _budget| {
                if first {
                    first = false;
                    entered.send(()).unwrap();
                    gate.recv().unwrap();
                }
                slow_seen.send("yield").unwrap();
                let n = m.downcast_mut::<usize>().unwrap();
                *n -= 1;
                if *n == 0 {
                    Step::Complete
                } else {
                    Step::Yield
                }
            }),
            0,
        )
        .unwrap();
    let fast = scheduler
        .spawn(
            Box::new(move |_, _| {
                seen.send("fast").unwrap();
                true
            }),
            0,
        )
        .unwrap();
    scheduler
        .send(slow, Box::new(3usize), Priority::Normal)
        .unwrap();
    ready.recv_timeout(DEADLINE).unwrap();
    scheduler
        .send(fast, Box::new(()), Priority::Normal)
        .unwrap();
    release.send(()).unwrap();
    let log: Vec<_> = (0..4)
        .map(|_| events.recv_timeout(DEADLINE).unwrap())
        .collect();
    scheduler.shutdown().unwrap();
    assert_eq!(log, vec!["yield", "fast", "yield", "yield"]);
}

#[test]
fn message_and_actor_destructors_reenter_scheduler_without_locked_state() {
    let scheduler = ReductionScheduler::new(1).unwrap();
    let (events, observed) = mpsc::channel();
    struct Reenter {
        scheduler: std::sync::Weak<ReductionScheduler>,
        events: mpsc::Sender<usize>,
    }
    impl Drop for Reenter {
        fn drop(&mut self) {
            if let Some(s) = self.scheduler.upgrade() {
                self.events.send(s.alive_count()).unwrap();
            }
        }
    }
    let captured = Reenter {
        scheduler: Arc::downgrade(&scheduler),
        events: events.clone(),
    };
    let actor = scheduler
        .spawn(
            Box::new(move |_, _| {
                let _ = &captured;
                false
            }),
            0,
        )
        .unwrap();
    scheduler
        .send(
            actor,
            Box::new(Reenter {
                scheduler: Arc::downgrade(&scheduler),
                events,
            }),
            Priority::Normal,
        )
        .unwrap();
    // The message is destroyed inside its callback; the actor after removal.
    assert_eq!(observed.recv_timeout(DEADLINE).unwrap(), 1);
    assert_eq!(observed.recv_timeout(DEADLINE).unwrap(), 0);
    scheduler.shutdown().unwrap();
}

#[test]
fn panicking_destructor_does_not_stop_retirement_of_other_resources() {
    let scheduler = ReductionScheduler::new(1).unwrap();
    let (seen, events) = mpsc::channel();
    struct BadDrop;
    impl Drop for BadDrop {
        fn drop(&mut self) {
            panic!("payload destructor");
        }
    }
    let actor = scheduler
        .spawn(
            Box::new(move |heap, _| {
                heap.put(0, Box::new(BadDrop));
                heap.put(1, Box::new(DropNotice(seen.clone())));
                false
            }),
            2,
        )
        .unwrap();
    scheduler
        .send(actor, Box::new(()), Priority::Normal)
        .unwrap();
    events.recv_timeout(DEADLINE).unwrap();
    scheduler.shutdown().unwrap();
    assert_eq!(scheduler.worker_counts(), (1, 1));
}
