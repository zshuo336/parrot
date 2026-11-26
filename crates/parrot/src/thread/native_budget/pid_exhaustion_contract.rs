// SPDX-License-Identifier: AGPL-3.0-only
use super::*;
use std::time::{Duration, Instant};

#[path = "../../../tests/support/thread_budget.rs"]
mod thread_budget;

fn kernel_denials() -> u64 {
    std::fs::read_to_string("/sys/fs/cgroup/pids.events")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("max "))
        .expect("missing cgroup v2 max event counter")
        .parse()
        .unwrap()
}

#[test]
#[ignore = "requires a private Linux cgroup v2 container with --pids-limit=64"]
fn thread_exhaustion_retries_original_native_join() {
    let timeout = Duration::from_secs(5);
    let budget = Arc::new(NativeWorkerBudget::with_capacity(2));
    let first = budget.try_reserve().unwrap();
    let second = budget.try_reserve().unwrap();
    let original = Arc::new(73);
    let value = original.clone();
    let worker = std::thread::spawn(move || value);
    let peer = std::thread::spawn(|| 31);
    let (occupied, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel::<()>();
    let (first_done, first_receipt) = mpsc::channel();
    let (second_done, second_receipt) = mpsc::channel();

    let full = thread_budget::ThreadBudget::exhaust();
    defer_join(peer, move |joined| {
        occupied
            .send(std::thread::current().name().unwrap().to_owned())
            .unwrap();
        let _ = wait.recv_timeout(timeout * 3);
        first.release_after_join();
        first_done.send(joined.unwrap()).unwrap();
    });
    assert_eq!(
        ready.recv_timeout(timeout).unwrap(),
        "parrot-native-join-reserved"
    );

    // No other code in this isolated process starts threads between these reads.
    let before = kernel_denials();
    defer_join(worker, move |joined| {
        second.release_after_join();
        second_done.send(joined.unwrap()).unwrap();
    });
    let deadline = Instant::now() + timeout;
    let after = loop {
        let now = kernel_denials();
        if now > before {
            break now;
        }
        assert!(
            Instant::now() < deadline,
            "dispatcher never encountered the PID limit"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(matches!(
        second_receipt.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert!(budget.try_reserve().is_err());

    drop(full);
    assert!(Arc::ptr_eq(
        &second_receipt.recv_timeout(timeout).unwrap(),
        &original
    ));
    let returned = budget.try_reserve().unwrap();
    assert!(budget.try_reserve().is_err());
    assert!(matches!(
        first_receipt.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    release.send(()).unwrap();
    assert_eq!(first_receipt.recv_timeout(timeout).unwrap(), 31);
    let last = budget.try_reserve().unwrap();
    assert!(budget.try_reserve().is_err());
    returned.release_after_join();
    last.release_after_join();
    eprintln!(
        "native join PID denials {before} -> {after}; both original results and leases recovered"
    );
}
