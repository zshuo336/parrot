#[path = "support/manager_ownership.rs"]
mod support;

use parrot::thread::ActorProcessorManager;
use std::sync::{Arc, atomic::Ordering, mpsc};
use std::time::Duration;
use support::{Counts, Probe, isolated, panics};

#[test]
fn clear_allows_destructor_reentry_and_keeps_the_next_snapshot() {
    isolated(
        "clear_allows_destructor_reentry_and_keeps_the_next_snapshot",
        || {
            let manager = Arc::new(ActorProcessorManager::new());
            let counts = Arc::new(Counts::default());
            let next_counts = Arc::new(Counts::default());
            let owner = Arc::downgrade(&manager);
            let next = Probe::plain(&next_counts);
            manager
                .register(
                    "/old",
                    Arc::new(Probe {
                        counts: counts.clone(),
                        on_drop: Some(Box::new(move || {
                            let manager = owner.upgrade().unwrap();
                            assert!(manager.is_empty());
                            manager.register("/next", next).unwrap();
                            assert!(manager.get("/next").is_some());
                        })),
                    }),
                )
                .unwrap();
            manager.clear();
            assert!(manager.get("/old").is_none());
            assert!(manager.get("/next").is_some());
            assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
            assert_eq!(next_counts.drops.load(Ordering::SeqCst), 0);
            manager.clear();
            assert_eq!(next_counts.drops.load(Ordering::SeqCst), 1);
            assert_eq!(counts.stops.load(Ordering::SeqCst), 0);
        },
    );
}

#[test]
fn clear_panic_does_not_poison_registry_or_skip_other_handles() {
    isolated(
        "clear_panic_does_not_poison_registry_or_skip_other_handles",
        || {
            let manager = ActorProcessorManager::new();
            let counts = Arc::new(Counts::default());
            manager
                .register(
                    "/panic",
                    Arc::new(Probe {
                        counts: counts.clone(),
                        on_drop: Some(Box::new(|| panic!("manager disposal panic"))),
                    }),
                )
                .unwrap();
            manager.register("/healthy", Probe::plain(&counts)).unwrap();
            assert!(panics(|| manager.clear()));
            let usable = std::panic::catch_unwind(|| manager.is_empty());
            let drops = counts.drops.load(Ordering::SeqCst);
            assert!(
                matches!(usable, Ok(true)) && drops == 2,
                "registry must remain usable and all handles released: usable={usable:?}, drops={drops}"
            );
            manager.register("/new", Probe::plain(&counts)).unwrap();
            assert!(manager.remove("/new").is_some());
            assert_eq!(counts.drops.load(Ordering::SeqCst), 3);
            assert_eq!(counts.stops.load(Ordering::SeqCst), 0);
        },
    );
}

#[test]
fn blocked_destructor_does_not_block_registry_or_clear_new_registrations() {
    let manager = Arc::new(ActorProcessorManager::new());
    let counts = Arc::new(Counts::default());
    let (entered, wait_entered) = mpsc::sync_channel(1);
    let (release, wait_release) = mpsc::sync_channel(1);
    let wait_release = std::sync::Mutex::new(wait_release);
    manager
        .register(
            "/old",
            Arc::new(Probe {
                counts: counts.clone(),
                on_drop: Some(Box::new(move || {
                    entered.send(()).unwrap();
                    wait_release
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                })),
            }),
        )
        .unwrap();
    let clearing = manager.clone();
    let clear = std::thread::spawn(move || clearing.clear());
    let entered = wait_entered.recv_timeout(Duration::from_secs(2));
    let (observed, wait_observed) = mpsc::sync_channel(1);
    let observing = manager.clone();
    let new_counts = counts.clone();
    let observer = std::thread::spawn(move || {
        let empty = observing.is_empty();
        let old_absent = observing.get("/old").is_none();
        observing
            .register("/new", Probe::plain(&new_counts))
            .unwrap();
        observed.send((empty, old_absent)).unwrap();
    });
    let observed = wait_observed.recv_timeout(Duration::from_millis(500));
    release.send(()).unwrap();
    clear.join().unwrap();
    observer.join().unwrap();
    assert!(entered.is_ok(), "destructor must actually be held");
    assert_eq!(
        observed.unwrap(),
        (true, true),
        "registry stayed locked during disposal"
    );
    assert!(manager.get("/new").is_some());
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    manager.clear();
    assert_eq!(counts.drops.load(Ordering::SeqCst), 2);
}

#[test]
fn clear_releases_only_manager_handles_and_never_stops_processors() {
    let manager = ActorProcessorManager::new();
    let counts = Arc::new(Counts::default());
    let processor = Probe::plain(&counts);
    manager.register("/retained", processor.clone()).unwrap();
    let retained = manager.get("/retained").unwrap();
    manager.clear();
    manager.clear();
    assert!(manager.is_empty());
    assert_eq!(Arc::strong_count(&processor), 2);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 0);
    assert_eq!(counts.stops.load(Ordering::SeqCst), 0);
    drop(retained);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 0);
    drop(processor);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
}
