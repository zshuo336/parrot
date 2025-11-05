use std::sync::{mpsc::TryRecvError, Arc};
use std::time::Duration;
use supervision::{
    BoxedMessage, BoxedResult, ChildFactory, DeathReason, SupervisionStrategy, Supervisor,
    WatchEvent,
};
fn factory() -> ChildFactory {
    Box::new(|| {
        Box::new(|msg: BoxedMessage| -> BoxedResult {
            if *msg.downcast_ref::<u64>().unwrap() == 999 {
                panic!("crash");
            }
            Ok(msg)
        })
    })
}
#[test]
fn watch_observes_only_its_target() {
    let sup = Supervisor::new();
    sup.spawn_supervised(factory(), "a", SupervisionStrategy::Stop)
        .unwrap();
    sup.spawn_supervised(factory(), "b", SupervisionStrategy::Stop)
        .unwrap();
    let a = sup.watch("wa", "a").unwrap();
    let b = sup.watch("wb", "b").unwrap();
    assert!(sup.ask("a", Box::new(999u64)).is_err());
    a.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        matches!(b.try_recv(), Err(TryRecvError::Empty)),
        "unrelated actor death leaked to target b watcher"
    );
}
#[test]
fn duplicate_path_is_rejected() {
    let sup = Supervisor::new();
    sup.spawn_supervised(factory(), "a", SupervisionStrategy::Stop)
        .unwrap();
    assert!(sup
        .spawn_supervised(factory(), "a", SupervisionStrategy::Stop)
        .is_err());
}
#[test]
fn escalation_stops_siblings_and_closes_admission() {
    let sup = Supervisor::new();
    sup.spawn_supervised(factory(), "a", SupervisionStrategy::Escalate)
        .unwrap();
    sup.spawn_supervised(factory(), "b", SupervisionStrategy::Stop)
        .unwrap();
    let a = sup.watch("wa", "a").unwrap();
    assert!(sup.ask("a", Box::new(999u64)).is_err());
    loop {
        let WatchEvent::Terminated { reason, .. } = a.recv_timeout(Duration::from_secs(2)).unwrap();
        if matches!(reason, DeathReason::Escalated(_)) {
            break;
        }
    }
    assert!(
        sup.ask("b", Box::new(42u64)).is_err(),
        "escalation did not stop siblings"
    );
    assert!(sup
        .spawn_supervised(factory(), "c", SupervisionStrategy::Stop)
        .is_err());
}
#[test]
fn idle_worker_does_not_keep_the_supervisor_owner_alive() {
    let sup = Supervisor::new();
    sup.spawn_supervised(factory(), "a", SupervisionStrategy::Stop)
        .unwrap();
    sup.ask("a", Box::new(42u64)).unwrap();
    let weak = Arc::downgrade(&sup);
    drop(sup);
    assert!(
        weak.upgrade().is_none(),
        "worker/supervisor/sender ownership cycle"
    );
}

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Mutex,
};
fn receive(rx: &mpsc::Receiver<WatchEvent>) -> (Option<u64>, DeathReason) {
    let WatchEvent::Terminated {
        generation, reason, ..
    } = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    (generation, reason)
}
fn policy(max: usize) -> SupervisionStrategy {
    SupervisionStrategy::OneForOne {
        max_restarts: max,
        window: Duration::from_secs(10),
    }
}
#[test]
fn restart_rebuilds_state_on_the_same_worker_without_terminal_notification() {
    let sup = Supervisor::new();
    let builds = Arc::new(AtomicUsize::new(0));
    let built = builds.clone();
    sup.spawn_supervised(
        Box::new(move || {
            built.fetch_add(1, Ordering::SeqCst);
            let mut value = 0u64;
            Box::new(move |m| {
                let n = *m.downcast::<u64>().unwrap();
                if n == 999 {
                    panic!("restart");
                }
                value += n;
                Ok(Box::new(value))
            })
        }),
        "state",
        policy(2),
    )
    .unwrap();
    let watch = sup.watch("w", "state").unwrap();
    assert_eq!(
        *sup.ask("state", Box::new(5u64))
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        5
    );
    assert!(sup.ask("state", Box::new(999u64)).is_err());
    assert_eq!(
        *sup.ask("state", Box::new(1u64))
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        1
    );
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    assert_eq!(sup.worker_counts(), (1, 0));
    assert!(matches!(watch.try_recv(), Err(TryRecvError::Empty)));
    sup.stop_child("state").unwrap();
    assert_eq!(receive(&watch).1, DeathReason::Normal);
    sup.shutdown().unwrap();
    assert_eq!(sup.worker_counts(), (1, 1));
}
#[test]
fn exhausted_restart_budget_retires_the_real_sibling_tree() {
    let sup = Supervisor::new();
    let builds = Arc::new(AtomicUsize::new(0));
    let built = builds.clone();
    sup.spawn_supervised(
        Box::new(move || {
            built.fetch_add(1, Ordering::SeqCst);
            factory()()
        }),
        "a",
        policy(2),
    )
    .unwrap();
    sup.spawn_supervised(factory(), "b", SupervisionStrategy::Stop)
        .unwrap();
    let a = sup.watch("w", "a").unwrap();
    let b = sup.watch("w", "b").unwrap();
    for _ in 0..3 {
        assert!(sup.ask("a", Box::new(999u64)).is_err());
    }
    assert!(matches!(receive(&a).1, DeathReason::Escalated(_)));
    assert_eq!(receive(&b).1, DeathReason::Killed);
    sup.shutdown().unwrap();
    assert_eq!(builds.load(Ordering::SeqCst), 3);
    assert_eq!(sup.alive_count(), 0);
    assert_eq!(sup.worker_counts(), (2, 2));
}
#[test]
fn factory_panic_uses_restart_policy_and_does_not_poison_future_construction() {
    let sup = Supervisor::new();
    let builds = Arc::new(AtomicUsize::new(0));
    let built = builds.clone();
    sup.spawn_supervised(
        Box::new(move || {
            if built.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("factory failed");
            }
            factory()()
        }),
        "a",
        policy(1),
    )
    .unwrap();
    assert_eq!(
        *sup.ask("a", Box::new(42u64))
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    sup.shutdown().unwrap();
    assert_eq!(sup.worker_counts(), (1, 1));
}
struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[test]
fn terminal_factory_panic_notifies_after_queued_resources_are_released() {
    let sup = Supervisor::new();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    sup.spawn_supervised(
        Box::new(move || {
            entered.send(()).unwrap();
            gate.recv().unwrap();
            panic!("initial factory failure");
        }),
        "a",
        SupervisionStrategy::Stop,
    )
    .unwrap();
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    let watch = sup.watch("w", "a").unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    sup.tell("a", Box::new(Dropped(drops.clone()))).unwrap();
    release.send(()).unwrap();
    assert!(matches!(receive(&watch).1, DeathReason::Panic(_)));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(sup.alive_count(), 0);
    sup.spawn_supervised(factory(), "a", SupervisionStrategy::Stop)
        .unwrap();
    sup.ask("a", Box::new(42u64)).unwrap();
    sup.shutdown().unwrap();
}
#[test]
fn watches_are_targeted_one_shot_and_generation_bound() {
    let sup = Supervisor::new();
    let missing = sup.watch("w", "missing").unwrap();
    assert_eq!(receive(&missing), (None, DeathReason::NotFound));
    sup.spawn_supervised(factory(), "a", SupervisionStrategy::Stop)
        .unwrap();
    sup.spawn_supervised(factory(), "b", SupervisionStrategy::Stop)
        .unwrap();
    let cancelled = sup.watch("w", "a").unwrap();
    let b = sup.watch("w", "b").unwrap();
    sup.unwatch_target("w", "a");
    assert!(matches!(
        cancelled.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
    let replaced = sup.watch("w", "a").unwrap();
    let a = sup.watch("w", "a").unwrap();
    assert!(matches!(
        replaced.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
    sup.stop_child("a").unwrap();
    let old_generation = receive(&a).0.unwrap();
    assert!(matches!(b.try_recv(), Err(TryRecvError::Empty)));
    sup.spawn_supervised(factory(), "a", SupervisionStrategy::Stop)
        .unwrap();
    let newer = sup.watch("w", "a").unwrap();
    sup.stop_child("a").unwrap();
    assert!(receive(&newer).0.unwrap() > old_generation);
    assert!(matches!(a.try_recv(), Err(TryRecvError::Disconnected)));
    sup.stop_child("b").unwrap();
    assert_eq!(receive(&b).1, DeathReason::Normal);
    sup.shutdown().unwrap();
}
struct BlockingDrop {
    entered: mpsc::Sender<()>,
    gate: mpsc::Receiver<()>,
}
impl Drop for BlockingDrop {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.gate.recv().unwrap();
    }
}
fn blocked_destructor_factory(entered: mpsc::Sender<()>, gate: mpsc::Receiver<()>) -> ChildFactory {
    let mut probe = Some(BlockingDrop { entered, gate });
    Box::new(move || {
        let probe = probe.take().unwrap();
        Box::new(move |m| {
            let _ = &probe;
            Ok(m)
        })
    })
}
#[test]
fn stopping_path_stays_reserved_until_the_old_destructor_finishes() {
    let sup = Supervisor::new();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    sup.spawn_supervised(
        blocked_destructor_factory(entered, gate),
        "a",
        SupervisionStrategy::Stop,
    )
    .unwrap();
    sup.ask("a", Box::new(1u64)).unwrap();
    let watch = sup.watch("w", "a").unwrap();
    sup.stop_child("a").unwrap();
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(sup
        .spawn_supervised(factory(), "a", SupervisionStrategy::Stop)
        .is_err());
    assert!(sup.ask("a", Box::new(2u64)).is_err());
    assert!(matches!(watch.try_recv(), Err(TryRecvError::Empty)));
    release.send(()).unwrap();
    assert_eq!(receive(&watch).1, DeathReason::Normal);
    sup.spawn_supervised(factory(), "a", SupervisionStrategy::Stop)
        .unwrap();
    sup.ask("a", Box::new(42u64)).unwrap();
    sup.shutdown().unwrap();
}
#[test]
fn concurrent_shutdown_waits_for_destructors_and_worker_join() {
    let sup = Supervisor::new();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    sup.spawn_supervised(
        blocked_destructor_factory(entered, gate),
        "a",
        SupervisionStrategy::Stop,
    )
    .unwrap();
    sup.ask("a", Box::new(1u64)).unwrap();
    let (started, starts) = mpsc::channel();
    let (finished, finishes) = mpsc::channel();
    let mut callers = vec![];
    for _ in 0..2 {
        let s = sup.clone();
        let start = started.clone();
        let finish = finished.clone();
        callers.push(std::thread::spawn(move || {
            start.send(()).unwrap();
            s.shutdown().unwrap();
            finish.send(()).unwrap();
        }));
    }
    starts.recv_timeout(Duration::from_secs(3)).unwrap();
    starts.recv_timeout(Duration::from_secs(3)).unwrap();
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(matches!(
        finishes.recv_timeout(Duration::from_millis(30)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    release.send(()).unwrap();
    for caller in callers {
        caller.join().unwrap();
    }
    assert_eq!(sup.alive_count(), 0);
    assert_eq!(sup.worker_counts(), (1, 1));
}
#[test]
fn expired_queued_ask_is_not_executed_after_blocked_work_returns() {
    let sup = Supervisor::new();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let seen = Arc::new(Mutex::new(vec![]));
    let observed = seen.clone();
    let mut gate = Some(gate);
    sup.spawn_supervised(
        Box::new(move || {
            let gate = gate.take().unwrap();
            let entered = entered.clone();
            let observed = observed.clone();
            Box::new(move |msg| {
                let n = *msg.downcast::<u64>().unwrap();
                observed.lock().unwrap().push(n);
                if n == 0 {
                    entered.send(()).unwrap();
                    gate.recv().unwrap();
                }
                Ok(Box::new(n))
            })
        }),
        "a",
        SupervisionStrategy::Stop,
    )
    .unwrap();
    sup.tell("a", Box::new(0u64)).unwrap();
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(sup
        .ask_with_timeout("a", Box::new(1u64), Duration::from_millis(10))
        .is_err());
    release.send(()).unwrap();
    sup.ask("a", Box::new(2u64)).unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![0, 2]);
    sup.shutdown().unwrap();
}
#[test]
fn bounded_admission_returns_the_original_message_and_counts_running_work() {
    let sup = Supervisor::new();
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let mut gate = Some(gate);
    sup.spawn_supervised(
        Box::new(move || {
            let gate = gate.take().unwrap();
            let entered = entered.clone();
            Box::new(move |msg| {
                entered.send(()).unwrap();
                gate.recv().unwrap();
                Ok(msg)
            })
        }),
        "a",
        SupervisionStrategy::Stop,
    )
    .unwrap();
    sup.tell("a", Box::new(0u64)).unwrap();
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    for n in 1..1024u64 {
        sup.tell("a", Box::new(n)).unwrap();
    }
    let (error, message) = sup.tell("a", Box::new(54321u64)).unwrap_err();
    assert_eq!(error, "mailbox full");
    assert_eq!(*message.downcast::<u64>().unwrap(), 54321);
    sup.request_shutdown();
    release.send(()).unwrap();
    sup.shutdown().unwrap();
    assert_eq!(sup.worker_counts(), (1, 1));
}
#[test]
fn factory_can_reenter_supervisor_and_worker_shutdown_never_self_joins() {
    let sup = Supervisor::new();
    let weak = Arc::downgrade(&sup);
    sup.spawn_supervised(
        Box::new(move || {
            let s = weak.upgrade().unwrap();
            assert_eq!(s.alive_count(), 1);
            s.spawn_supervised(factory(), "helper", SupervisionStrategy::Stop)
                .unwrap();
            let weak = weak.clone();
            Box::new(move |_| {
                let result = weak.upgrade().unwrap().shutdown();
                Ok(Box::new(result.is_err()))
            })
        }),
        "a",
        SupervisionStrategy::Stop,
    )
    .unwrap();
    assert!(*sup
        .ask("a", Box::new(()))
        .unwrap()
        .downcast::<bool>()
        .unwrap());
    sup.shutdown().unwrap();
    assert_eq!(sup.worker_counts(), (2, 2));
}
#[test]
fn reentrant_and_panicking_drops_do_not_skip_other_resources() {
    struct Reenter(std::sync::Weak<Supervisor>, Arc<AtomicUsize>);
    impl Drop for Reenter {
        fn drop(&mut self) {
            if let Some(s) = self.0.upgrade() {
                assert!(s.alive_count() > 0);
                let _ = s.shutdown();
            }
            self.1.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Panics;
    impl Drop for Panics {
        fn drop(&mut self) {
            panic!("payload drop");
        }
    }
    let sup = Supervisor::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut probe = Some(Reenter(Arc::downgrade(&sup), drops.clone()));
    let (entered, ready) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let mut gate = Some(gate);
    sup.spawn_supervised(
        Box::new(move || {
            let probe = probe.take().unwrap();
            let gate = gate.take().unwrap();
            let entered = entered.clone();
            Box::new(move |msg| {
                let _ = &probe;
                entered.send(()).unwrap();
                gate.recv().unwrap();
                Ok(msg)
            })
        }),
        "a",
        SupervisionStrategy::Stop,
    )
    .unwrap();
    sup.tell("a", Box::new(0u64)).unwrap();
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    sup.tell("a", Box::new(Panics)).unwrap();
    sup.tell("a", Box::new(Dropped(drops.clone()))).unwrap();
    sup.request_shutdown();
    release.send(()).unwrap();
    sup.shutdown().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(sup.worker_counts(), (1, 1));
}
#[test]
fn last_owner_drop_retires_actor_captures_before_returning() {
    let sup = Supervisor::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut probe = Some(Dropped(drops.clone()));
    sup.spawn_supervised(
        Box::new(move || {
            let probe = probe.take().unwrap();
            Box::new(move |msg| {
                let _ = &probe;
                Ok(msg)
            })
        }),
        "a",
        SupervisionStrategy::Stop,
    )
    .unwrap();
    sup.ask("a", Box::new(0u64)).unwrap();
    drop(sup);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
#[test]
fn invalid_policy_and_zero_timeout_reject_without_running_callbacks() {
    let sup = Supervisor::new();
    assert!(sup
        .spawn_supervised(
            factory(),
            "a",
            SupervisionStrategy::OneForOne {
                max_restarts: 1,
                window: Duration::ZERO
            }
        )
        .is_err());
    assert!(sup
        .spawn_supervised(factory(), "", SupervisionStrategy::Stop)
        .is_err());
    assert_eq!(sup.worker_counts(), (0, 0));
    sup.spawn_supervised(factory(), "a", SupervisionStrategy::Stop)
        .unwrap();
    assert!(sup
        .ask_with_timeout("a", Box::new(999u64), Duration::ZERO)
        .is_err());
    sup.ask("a", Box::new(42u64)).unwrap();
    sup.shutdown().unwrap();
}

#[test]
fn old_restarts_expire_from_the_window_before_the_next_failure() {
    let sup = Supervisor::new();
    let builds = Arc::new(AtomicUsize::new(0));
    let built = builds.clone();
    sup.spawn_supervised(
        Box::new(move || {
            built.fetch_add(1, Ordering::SeqCst);
            factory()()
        }),
        "a",
        SupervisionStrategy::OneForOne {
            max_restarts: 1,
            window: Duration::from_millis(2),
        },
    )
    .unwrap();
    let watch = sup.watch("w", "a").unwrap();
    // The actual interruptible backoff is at least 10 ms, longer than this
    // window. The next successful ask is the completion barrier, not a sleep.
    for _ in 0..3 {
        assert!(sup.ask("a", Box::new(999u64)).is_err());
        sup.ask("a", Box::new(42u64)).unwrap();
    }
    assert_eq!(builds.load(Ordering::SeqCst), 4);
    assert!(matches!(watch.try_recv(), Err(TryRecvError::Empty)));
    sup.shutdown().unwrap();
    assert_eq!(receive(&watch).1, DeathReason::Killed);
}
#[test]
fn panicking_unobserved_reply_does_not_kill_the_worker() {
    struct Panics(Arc<AtomicUsize>);
    impl Drop for Panics {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("discarded reply Drop");
        }
    }
    let sup = Supervisor::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let dropped = drops.clone();
    sup.spawn_supervised(
        Box::new(move || {
            let dropped = dropped.clone();
            Box::new(move |msg| {
                let n = *msg.downcast::<u64>().unwrap();
                if n == 1 {
                    Ok(Box::new(Panics(dropped.clone())))
                } else {
                    Ok(Box::new(n))
                }
            })
        }),
        "a",
        SupervisionStrategy::Stop,
    )
    .unwrap();
    sup.tell("a", Box::new(1u64)).unwrap();
    assert_eq!(
        *sup.ask("a", Box::new(42u64))
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    sup.shutdown().unwrap();
    assert_eq!(sup.worker_counts(), (1, 1));
}
