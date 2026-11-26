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

#[test]
fn a_worker_in_tls_drop_does_not_delay_an_exited_sibling_receipt() {
    struct TlsGate(mpsc::Sender<()>, mpsc::Receiver<()>);
    impl Drop for TlsGate {
        fn drop(&mut self) {
            self.0.send(()).unwrap();
            self.1.recv_timeout(WAIT * 4).unwrap();
        }
    }
    thread_local! {
        static EXIT: std::cell::RefCell<Option<TlsGate>> = const { std::cell::RefCell::new(None) };
    }
    prepare_join_fallback().unwrap();
    let budget = Arc::new(NativeWorkerBudget::with_capacity(2));
    let first = budget.try_reserve().unwrap();
    let second = budget.try_reserve().unwrap();
    let (entered, in_tls) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let (first_done, first_receipt) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        EXIT.with(|slot| slot.replace(Some(TlsGate(entered, wait))));
    });
    in_tls.recv_timeout(WAIT).unwrap();
    defer_join(worker, move |joined| {
        joined.unwrap();
        first.release_after_join();
        first_done.send(()).unwrap();
    });
    let (second_done, second_receipt) = mpsc::channel();
    let sibling = std::thread::spawn(|| 73);
    defer_join(sibling, move |joined| {
        second.release_after_join();
        second_done.send(joined.unwrap()).unwrap();
    });

    let early = second_receipt.recv_timeout(WAIT);
    let early_slot = budget.try_reserve().ok();
    let recovered_one = early_slot.is_some() && budget.try_reserve().is_err();
    assert!(matches!(
        first_receipt.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    release.send(()).unwrap();
    first_receipt.recv_timeout(WAIT).unwrap();
    let value = early
        .as_ref()
        .ok()
        .copied()
        .unwrap_or_else(|| second_receipt.recv_timeout(WAIT).unwrap());
    assert_eq!(value, 73);
    drop(early_slot);
    let one = budget.try_reserve().unwrap();
    let two = budget.try_reserve().unwrap();
    one.release_after_join();
    two.release_after_join();
    assert_eq!(
        early,
        Ok(73),
        "sibling joined only after unrelated TLS was released"
    );
    assert!(
        recovered_one,
        "only the exited sibling's slot should have returned"
    );
}
