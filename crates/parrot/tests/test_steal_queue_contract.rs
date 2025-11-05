use parrot::thread::scheduler::steal::StealDeque;
use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[test]
fn concurrent_length_never_exceeds_capacity() {
    const CAP: usize = 4;
    const ITEMS: usize = 200_000;
    let queue = Arc::new(StealDeque::new(CAP));
    let start = Arc::new(Barrier::new(4));
    let stop = Arc::new(AtomicBool::new(false));
    let invalid = Arc::new(AtomicUsize::new(0));
    let writer = {
        let (queue, start, stop) = (queue.clone(), start.clone(), stop.clone());
        std::thread::spawn(move || {
            start.wait();
            let mut accepted = 0;
            while accepted < ITEMS && !stop.load(Ordering::Relaxed) {
                if queue.push(accepted) {
                    accepted += 1;
                } else {
                    std::thread::yield_now();
                }
            }
            accepted
        })
    };
    let reader = {
        let (queue, start, stop) = (queue.clone(), start.clone(), stop.clone());
        std::thread::spawn(move || {
            start.wait();
            let mut received = Vec::new();
            while received.len() < ITEMS && !stop.load(Ordering::Relaxed) {
                if let Some(item) = queue.steal() {
                    received.push(item);
                } else {
                    std::thread::yield_now();
                }
            }
            stop.store(true, Ordering::Relaxed);
            received
        })
    };
    let observer = {
        let (queue, start, stop, invalid) =
            (queue.clone(), start.clone(), stop.clone(), invalid.clone());
        std::thread::spawn(move || {
            start.wait();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                let len = queue.len();
                if len > CAP {
                    invalid.store(len, Ordering::Relaxed);
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
            }
            stop.store(true, Ordering::Relaxed);
        })
    };
    start.wait();
    let accepted = writer.join();
    let received = reader.join();
    let observed = observer.join();
    observed.unwrap();
    assert_eq!(
        invalid.load(Ordering::Relaxed),
        0,
        "public queue length exceeded capacity"
    );
    assert_eq!(
        accepted.unwrap(),
        ITEMS,
        "producer did not finish within external observation bound"
    );
    assert_eq!(
        received.unwrap(),
        (0..ITEMS).collect::<Vec<_>>(),
        "FIFO ownership changed"
    );
    assert_eq!(queue.len(), 0);
    assert!(queue.is_empty());
}

#[test]
fn snapshots_track_accepted_and_retired_items() {
    let queue = StealDeque::new(4);
    assert!(queue.is_empty());
    for i in 0..4 {
        assert!(queue.push(i));
        assert_eq!(queue.len(), i + 1);
    }
    assert!(!queue.push(99));
    assert_eq!(queue.len(), 4);
    assert_eq!(queue.pop(), Some(0));
    assert_eq!(queue.len(), 3);
    queue.close();
    assert!(!queue.push(100));
    for i in 1..4 {
        assert_eq!(queue.steal(), Some(i));
        assert_eq!(queue.len(), 3 - i);
    }
    assert!(queue.is_empty());
}
