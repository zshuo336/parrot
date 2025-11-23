use super::*;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(3);

fn observe(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "native fallback join did not progress"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn self_owned_join_returns_capacity_only_after_actual_worker_exit() {
    let budget = Arc::new(NativeWorkerBudget::with_capacity(1));
    let reservation = budget.try_reserve().unwrap();
    let (send, receive) = mpsc::sync_channel(1);
    let (entered, observe_entered) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        drop(NativeJoinJob::new(
            receive.recv().unwrap(),
            Some(reservation),
        ));
        entered.send(()).unwrap();
        wait.recv_timeout(WAIT).unwrap();
        Ok(())
    });
    send.send(worker).unwrap();
    observe_entered.recv_timeout(WAIT).unwrap();
    let retained = budget.try_reserve().is_err();
    release.send(()).unwrap();
    observe(|| budget.try_reserve().is_ok());
    assert!(
        retained,
        "self-owned worker returned its native lease before exit"
    );
}

#[test]
fn unfinished_self_owner_does_not_block_a_finished_sibling_join() {
    let budget = Arc::new(NativeWorkerBudget::with_capacity(2));
    let mut releases = Vec::new();
    let mut entered = Vec::new();
    for _ in 0..2 {
        let reservation = budget.try_reserve().unwrap();
        let (send, receive) = mpsc::sync_channel(1);
        let (started, observe_started) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            drop(NativeJoinJob::new(
                receive.recv().unwrap(),
                Some(reservation),
            ));
            started.send(()).unwrap();
            wait.recv_timeout(WAIT).unwrap();
            Ok(())
        });
        send.send(worker).unwrap();
        releases.push(release);
        entered.push(observe_started);
    }
    for started in entered {
        started.recv_timeout(WAIT).unwrap();
    }
    assert!(budget.try_reserve().is_err());
    releases.pop().unwrap().send(()).unwrap();
    observe(|| budget.try_reserve().is_ok());
    let sibling = budget.try_reserve().unwrap();
    let held = budget.try_reserve().is_err();
    releases.pop().unwrap().send(()).unwrap();
    observe(|| budget.try_reserve().is_ok());
    sibling.release_after_join();
    assert!(held, "unfinished sibling returned its native lease");
}

#[test]
fn panicking_completion_does_not_stop_later_joins() {
    prepare_join_fallback().unwrap();
    let (entered, observe_entered) = mpsc::sync_channel(1);
    defer_join(std::thread::spawn(|| ()), move |joined| {
        joined.unwrap();
        entered.send(()).unwrap();
        panic!("join completion failure");
    });
    observe_entered.recv_timeout(WAIT).unwrap();
    let (finished, observe_finished) = mpsc::sync_channel(1);
    defer_join(std::thread::spawn(|| 42), move |joined| {
        finished.send(joined.unwrap()).unwrap();
    });
    assert_eq!(observe_finished.recv_timeout(WAIT).unwrap(), 42);
}
