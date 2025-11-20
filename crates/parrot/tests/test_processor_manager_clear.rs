#[path = "support/manager_ownership.rs"]
mod support;

use parrot::thread::{ActorProcessorManager, SystemError};
use std::sync::{Arc, atomic::Ordering};
use support::{Counts, Probe, isolated, panics};

#[test]
fn try_clear_reports_each_panicking_path_after_disposing_every_snapshot_handle() {
    isolated(
        "try_clear_reports_each_panicking_path_after_disposing_every_snapshot_handle",
        || {
            let manager = ActorProcessorManager::new();
            let counts = Arc::new(Counts::default());
            for path in ["/panic-a", "/panic-b"] {
                manager
                    .register(
                        path,
                        Arc::new(Probe {
                            counts: counts.clone(),
                            on_drop: Some(Box::new(move || panic!("disposal failed at {path}"))),
                        }),
                    )
                    .unwrap();
            }
            manager.register("/healthy", Probe::plain(&counts)).unwrap();
            let Err(SystemError::Panic(message)) = manager.try_clear() else {
                panic!("each destructor panic must be returned as a panic receipt");
            };
            for path in ["/panic-a", "/panic-b"] {
                assert!(message.contains(&format!("{path}:")), "{message}");
                assert!(
                    message.contains(&format!("disposal failed at {path}")),
                    "{message}"
                );
            }
            assert!(!message.contains("/healthy:"), "{message}");
            assert_eq!(counts.drops.load(Ordering::SeqCst), 3);
            assert_eq!(counts.stops.load(Ordering::SeqCst), 0);
            assert!(manager.is_empty());
            manager.register("/new", Probe::plain(&counts)).unwrap();
            assert!(!panics(|| manager.try_clear().unwrap()));
            assert_eq!(counts.drops.load(Ordering::SeqCst), 4);
        },
    );
}

struct Payload;
impl Drop for Payload {
    fn drop(&mut self) {
        panic!("recursive manager panic payload");
    }
}

#[test]
fn recursive_disposal_payload_cannot_abort_cleanup_or_poison_the_registry() {
    isolated(
        "recursive_disposal_payload_cannot_abort_cleanup_or_poison_the_registry",
        || {
            let manager = ActorProcessorManager::new();
            let counts = Arc::new(Counts::default());
            for path in ["/payload-a", "/payload-b"] {
                manager
                    .register(
                        path,
                        Arc::new(Probe {
                            counts: counts.clone(),
                            on_drop: Some(Box::new(|| std::panic::panic_any(Payload))),
                        }),
                    )
                    .unwrap();
            }
            manager.register("/healthy", Probe::plain(&counts)).unwrap();
            let Err(SystemError::Panic(message)) = manager.try_clear() else {
                panic!("recursive payload must remain a disposal failure");
            };
            assert!(message.contains("/payload-a:"), "{message}");
            assert!(message.contains("/payload-b:"), "{message}");
            assert!(message.contains("non-string panic payload"), "{message}");
            assert!(
                message.contains("panic payload destructor panicked"),
                "{message}"
            );
            assert_eq!(counts.drops.load(Ordering::SeqCst), 3);
            assert_eq!(counts.stops.load(Ordering::SeqCst), 0);
            assert!(manager.is_empty());
            assert!(!panics(|| manager.try_clear().unwrap()));
        },
    );
}

#[test]
fn empty_and_externally_owned_snapshots_report_success_without_stopping() {
    let manager = ActorProcessorManager::new();
    let counts = Arc::new(Counts::default());
    manager.try_clear().unwrap();
    let processor = Probe::plain(&counts);
    manager.register("/retained", processor.clone()).unwrap();
    manager.try_clear().unwrap();
    assert!(manager.is_empty());
    assert_eq!(Arc::strong_count(&processor), 1);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 0);
    assert_eq!(counts.stops.load(Ordering::SeqCst), 0);
    assert!(!panics(|| manager.try_clear().unwrap()));
    drop(processor);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
}
