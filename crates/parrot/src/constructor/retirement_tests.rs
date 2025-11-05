use super::*;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

struct BadPayload;
impl Drop for BadPayload {
    fn drop(&mut self) {
        panic!("secondary factory payload drop");
    }
}
struct Capture {
    drops: Arc<AtomicUsize>,
    recursive: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.recursive {
            std::panic::panic_any(BadPayload);
        }
        panic!("factory capture retirement probe");
    }
}
fn factory(drops: &Arc<AtomicUsize>, recursive: bool, cleanup: Arc<Semaphore>) -> Arc<Factory<()>> {
    let capture = Capture {
        drops: drops.clone(),
        recursive,
    };
    Factory::new(
        move || {
            let _ = &capture;
        },
        Arc::new(Semaphore::new(1)),
        cleanup,
    )
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn isolated(name: &str, body: impl FnOnce()) {
    const FLAG: &str = "PARROT_FACTORY_RETIREMENT_CASE";
    let full = format!("constructor::retirement_tests::{name}");
    if std::env::var(FLAG).as_deref() == Ok(name) {
        body();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &full, "--nocapture"])
        .env(FLAG, name)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: {status}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{name}: deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn unpolled_retirement_survives_executor_shutdown() {
    isolated("unpolled_retirement_survives_executor_shutdown", || {
        let r = runtime();
        let drops = Arc::new(AtomicUsize::new(0));
        let f = factory(&drops, false, Arc::new(Semaphore::new(1)));
        let receipt = {
            let _entered = r.enter();
            f.retire()
        };
        drop(r);
        let error = futures::executor::block_on(receipt).unwrap_err();
        assert!(
            error.contains("factory capture retirement probe"),
            "{error}"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    });
}
#[test]
fn abandoned_retirement_waiter_keeps_capture_disposal() {
    isolated("abandoned_retirement_waiter_keeps_capture_disposal", || {
        let r = runtime();
        let drops = Arc::new(AtomicUsize::new(0));
        let f = factory(&drops, false, Arc::new(Semaphore::new(1)));
        {
            let _entered = r.enter();
            drop(f.retire());
        }
        r.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), async {
                while drops.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        });
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(f);
        drop(r);
    });
}
#[test]
fn cleanup_permit_waiter_reports_capture_failure_after_shutdown() {
    isolated(
        "cleanup_permit_waiter_reports_capture_failure_after_shutdown",
        || {
            let r = runtime();
            let drops = Arc::new(AtomicUsize::new(0));
            let cleanup = Arc::new(Semaphore::new(1));
            let permit = cleanup.clone().try_acquire_owned().unwrap();
            let f = factory(&drops, false, cleanup);
            let mut receipt = {
                let _entered = r.enter();
                f.retire()
            };
            r.block_on(async {
                assert!(futures::poll!(&mut receipt).is_pending());
                tokio::task::yield_now().await;
            });
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(r);
            drop(permit);
            let error = futures::executor::block_on(receipt).unwrap_err();
            assert!(
                error.contains("factory capture retirement probe"),
                "{error}"
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        },
    );
}
struct Output(Arc<AtomicUsize>);
impl Drop for Output {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("unpublished output retirement probe");
    }
}
#[test]
fn shutdown_retirement_waits_for_last_prepared_output() {
    isolated("shutdown_retirement_waits_for_last_prepared_output", || {
        let r = runtime();
        let drops = Arc::new(AtomicUsize::new(0));
        let outputs = Arc::new(AtomicUsize::new(0));
        let capture = Capture {
            drops: drops.clone(),
            recursive: false,
        };
        let output = outputs.clone();
        let f = Factory::new(
            move || {
                let _ = &capture;
                Output(output.clone())
            },
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(1)),
        );
        let prepared = r.block_on(f.prepare::<()>(Arc::default())).unwrap();
        let mut receipt = {
            let _entered = r.enter();
            f.retire()
        };
        r.block_on(async {
            assert!(futures::poll!(&mut receipt).is_pending());
            tokio::task::yield_now().await;
        });
        drop(r);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(outputs.load(Ordering::SeqCst), 0);
        assert!(futures::executor::block_on(async { futures::poll!(&mut receipt) }).is_pending());
        drop(prepared);
        let error = futures::executor::block_on(receipt).unwrap_err();
        assert!(
            error.contains("unpublished output retirement probe"),
            "{error}"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(outputs.load(Ordering::SeqCst), 1);
    });
}
#[test]
fn owned_factory_retirement_starts_before_waiter_poll() {
    isolated("owned_factory_retirement_starts_before_waiter_poll", || {
        let r = runtime();
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Capture {
            drops: drops.clone(),
            recursive: true,
        };
        let f = owned::OwnedFactory::new(
            move || {
                let _ = &capture;
            },
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(1)),
            r.handle().clone(),
        );
        let receipt = f.retire();
        drop(r);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let error = futures::executor::block_on(receipt)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("panic payload destructor panicked"),
            "{error}"
        );
    });
}
#[test]
fn recursive_constructor_panic_keeps_construction_error_class() {
    isolated(
        "recursive_constructor_panic_keeps_construction_error_class",
        || {
            runtime().block_on(async {
                let f = Factory::<()>::new(
                    || std::panic::panic_any(BadPayload),
                    Arc::new(Semaphore::new(1)),
                    Arc::new(Semaphore::new(1)),
                );
                let error = f
                    .construct(Arc::default(), |_| Ok::<_, ()>(()))
                    .await
                    .unwrap_err();
                match error {
                    ConstructionError::ConstructorPanicked(message) => {
                        assert!(message.contains("panic payload destructor panicked"))
                    }
                    other => panic!("wrong failure class: {other:?}"),
                };
                f.retire().await.unwrap();
            })
        },
    );
}
#[test]
fn recursive_capture_panic_keeps_stable_retirement_receipt() {
    isolated(
        "recursive_capture_panic_keeps_stable_retirement_receipt",
        || {
            runtime().block_on(async {
                let drops = Arc::new(AtomicUsize::new(0));
                let f = factory(&drops, true, Arc::new(Semaphore::new(1)));
                let (a, b) = tokio::join!(f.retire(), f.retire());
                let a = a.unwrap_err();
                assert_eq!(b.unwrap_err(), a);
                assert!(a.contains("panic payload destructor panicked"), "{a}");
                assert_eq!(f.retire().await.unwrap_err(), a);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            })
        },
    );
}
#[test]
fn final_unretired_factory_drop_isolates_capture_panic() {
    isolated(
        "final_unretired_factory_drop_isolates_capture_panic",
        || {
            let drops = Arc::new(AtomicUsize::new(0));
            drop(factory(&drops, true, Arc::new(Semaphore::new(1))));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        },
    );
}
#[test]
fn capture_drop_can_reenter_retirement_without_lock() {
    isolated("capture_drop_can_reenter_retirement_without_lock", || {
        runtime().block_on(async {
            struct Reenter(Arc<Mutex<std::sync::Weak<Factory<()>>>>, Arc<AtomicUsize>);
            impl Drop for Reenter {
                fn drop(&mut self) {
                    let f = self.0.lock().unwrap().upgrade().unwrap();
                    drop(f.retire());
                    self.1.fetch_add(1, Ordering::SeqCst);
                }
            }
            let weak = Arc::new(Mutex::new(std::sync::Weak::new()));
            let drops = Arc::new(AtomicUsize::new(0));
            let capture = Reenter(weak.clone(), drops.clone());
            let f = Factory::new(
                move || {
                    let _ = &capture;
                },
                Arc::new(Semaphore::new(1)),
                Arc::new(Semaphore::new(1)),
            );
            *weak.lock().unwrap() = Arc::downgrade(&f);
            f.retire().await.unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        })
    });
}

#[test]
fn last_prepared_capture_failure_reaches_discard_receipt() {
    isolated(
        "last_prepared_capture_failure_reaches_discard_receipt",
        || {
            runtime().block_on(async {
                let drops = Arc::new(AtomicUsize::new(0));
                let f = factory(&drops, false, Arc::new(Semaphore::new(1)));
                let prepared = f.prepare::<()>(Arc::default()).await.unwrap();
                drop(f);
                let error = prepared.discard().await.unwrap_err();
                assert!(
                    error.contains("factory capture retirement probe"),
                    "{error}"
                );
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            })
        },
    );
}

#[test]
fn recursive_prepare_panic_keeps_construction_error_class() {
    isolated(
        "recursive_prepare_panic_keeps_construction_error_class",
        || {
            runtime().block_on(async {
                let f = Factory::<()>::new(
                    || std::panic::panic_any(BadPayload),
                    Arc::new(Semaphore::new(1)),
                    Arc::new(Semaphore::new(1)),
                );
                match f.prepare::<()>(Arc::default()).await {
                    Err(ConstructionError::ConstructorPanicked(message)) => {
                        assert!(message.contains("panic payload destructor panicked"))
                    }
                    _ => panic!("prepared constructor lost original error class"),
                }
                f.retire().await.unwrap();
            })
        },
    );
}
#[test]
fn recursive_publication_panic_keeps_disposal_error_class() {
    isolated(
        "recursive_publication_panic_keeps_disposal_error_class",
        || {
            runtime().block_on(async {
                let f = Factory::new(
                    || (),
                    Arc::new(Semaphore::new(1)),
                    Arc::new(Semaphore::new(1)),
                );
                let p = f.prepare::<()>(Arc::default()).await.unwrap();
                let error = p
                    .publish(|_| -> Result<(), ()> { std::panic::panic_any(BadPayload) })
                    .await
                    .unwrap_err();
                match error {
                    ConstructionError::DisposalPanicked(message) => {
                        assert!(message.contains("panic payload destructor panicked"))
                    }
                    _ => panic!("publication lost original error class"),
                };
                assert!(
                    f.retire()
                        .await
                        .unwrap_err()
                        .contains("panic payload destructor panicked")
                );
            })
        },
    );
}
#[test]
fn shutdown_retirement_waits_for_blocking_constructor() {
    isolated("shutdown_retirement_waits_for_blocking_constructor", || {
        let r = runtime();
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Capture {
            drops: drops.clone(),
            recursive: false,
        };
        let (entered, observed) = tokio::sync::oneshot::channel();
        let entered = Mutex::new(Some(entered));
        let (release, held) = std::sync::mpsc::channel();
        let held = Mutex::new(held);
        let f = Factory::new(
            move || {
                let _ = &capture;
                entered.lock().unwrap().take().unwrap().send(()).unwrap();
                held.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            },
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(1)),
        );
        let control = Arc::new(ConstructionControl::default());
        let worker_control = control.clone();
        let worker_factory = f.clone();
        let worker = r.spawn(async move {
            worker_factory
                .construct(worker_control, |_| Ok::<_, ()>(()))
                .await
        });
        r.block_on(observed).unwrap();
        let mut receipt = {
            let _entered = r.enter();
            f.retire()
        };
        r.block_on(async {
            assert!(futures::poll!(&mut receipt).is_pending());
            tokio::task::yield_now().await;
        });
        control.cancel();
        r.shutdown_timeout(Duration::from_millis(20));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(futures::executor::block_on(async { futures::poll!(&mut receipt) }).is_pending());
        release.send(()).unwrap();
        let error = futures::executor::block_on(receipt).unwrap_err();
        assert!(
            error.contains("factory capture retirement probe"),
            "{error}"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(worker);
    });
}

#[tokio::test]
async fn admitted_thread_constructor_does_not_wait_for_unrelated_cleanup_capacity() {
    use crate::thread::ThreadActorSystem;
    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::types::{ActorResult, BoxedMessage};
    struct Probe;
    impl Actor for Probe {
        type Config = EmptyConfig;
        type Context = crate::thread::ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn receive_message<'a>(
            &'a mut self,
            m: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(m) })
        }
    }
    let system = ThreadActorSystem::shared(Default::default());
    let slots = system.cleanup_slots.clone();
    let permits = slots
        .clone()
        .acquire_many_owned(slots.available_permits() as u32)
        .await
        .unwrap();
    let (entered, observed) = tokio::sync::oneshot::channel();
    let entered = Mutex::new(Some(entered));
    let spawn = system.spawn_supervised(
        move || {
            entered.lock().unwrap().take().unwrap().send(()).ok();
            Probe
        },
        "/independent-constructor",
        crate::thread::config::SupervisorStrategy::Stop,
    );
    tokio::pin!(spawn);
    assert!(futures::poll!(&mut spawn).is_pending());
    let progress = tokio::time::timeout(Duration::from_secs(3), observed).await;
    drop(permits);
    let reference = spawn.await.unwrap();
    use parrot_api::address::ActorRef;
    reference.stop().await.unwrap();
    system.shutdown_internal().await.unwrap();
    assert!(
        matches!(progress, Ok(Ok(()))),
        "admitted constructor was blocked by unused configuration cleanup"
    );
}
