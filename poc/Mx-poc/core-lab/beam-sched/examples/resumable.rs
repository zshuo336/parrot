//! Actual resumable work, interrupted by a high-priority query on the same actor.
use beam_sched::{Priority, ReductionScheduler, Step};
use std::sync::mpsc;
use std::time::Duration;
enum Work {
    Sum { next: u64, sum: u64 },
    Probe,
}
#[derive(Debug)]
enum Event {
    Progress(u64),
    Sum(u64),
}
fn main() {
    let scheduler = ReductionScheduler::with_budget(1, 32).unwrap();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (result, events) = mpsc::channel();
    let mut progress = 0;
    let actor = scheduler
        .spawn_resumable(
            Box::new(
                move |_, message, budget| match message.downcast_mut::<Work>().unwrap() {
                    Work::Probe => {
                        result.send(Event::Progress(progress)).unwrap();
                        Step::Complete
                    }
                    Work::Sum { next, sum } => {
                        if *next == 0 {
                            entered.send(()).unwrap();
                            gate.recv().unwrap();
                        }
                        while *next < 10_000 && budget.consume(1) {
                            *sum += *next;
                            *next += 1;
                            progress = *next;
                        }
                        if *next == 10_000 {
                            result.send(Event::Sum(*sum)).unwrap();
                            Step::Complete
                        } else {
                            Step::Yield
                        }
                    }
                },
            ),
            0,
        )
        .unwrap();
    scheduler
        .send(
            actor,
            Box::new(Work::Sum { next: 0, sum: 0 }),
            Priority::Normal,
        )
        .unwrap();
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    scheduler
        .send(actor, Box::new(Work::Probe), Priority::High)
        .unwrap();
    release.send(()).unwrap();
    match events.recv_timeout(Duration::from_secs(3)).unwrap() {
        Event::Progress(n) => {
            assert!(n > 0 && n < 10_000);
            println!("high-priority query observed partial progress: {n}/10000");
        }
        other => panic!("query must run before the sum finishes: {other:?}"),
    }
    match events.recv_timeout(Duration::from_secs(3)).unwrap() {
        Event::Sum(value) => {
            assert_eq!(value, 49_995_000);
            println!("resumed sum: {value}");
        }
        other => panic!("expected completed sum: {other:?}"),
    }
    scheduler.shutdown().unwrap();
    assert_eq!(scheduler.worker_counts(), (1, 1));
    println!("actor resources retired and worker joined");
}
