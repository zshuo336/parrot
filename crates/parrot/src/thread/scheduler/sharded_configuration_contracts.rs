use super::*;
use std::{cell::RefCell, sync::atomic::AtomicUsize, time::Duration};

struct Exit(Arc<AtomicUsize>);
impl Drop for Exit {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
thread_local! {static EXIT: RefCell<Option<Exit>> = const {RefCell::new(None)};}

#[test]
fn native_budget_exhaustion_preserves_an_earlier_worker_failure() {
    let result = ShardedScheduler::try_start_with_budget_and_startup(
        2,
        1,
        || panic!("early budget startup fault"),
        Arc::new(NativeWorkerBudget::with_capacity(1)),
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("native worker budget exhausted"),
        "{message}"
    );
    assert!(message.contains("early budget startup fault"), "{message}");
}

#[test]
fn native_budget_is_restored_after_spawn_readiness_and_unwind_failures() {
    for failure in 0..5 {
        let budget = Arc::new(NativeWorkerBudget::with_capacity(3));
        let retired = Arc::new(AtomicUsize::new(0));
        let counter = retired.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ShardedScheduler::try_start_with_hooks_and_budget(
                3,
                1,
                || {},
                move |index| {
                    if (failure == 0 && index == 0) || (failure == 1 && index == 1) {
                        return Err(std::io::Error::other("spawn admission failure"));
                    }
                    if failure == 4 && index == 1 {
                        panic!("creator unwind");
                    }
                    Ok(())
                },
                move |index| {
                    EXIT.with(|slot| *slot.borrow_mut() = Some(Exit(counter.clone())));
                    if index == 1 {
                        if failure == 2 {
                            return Err(std::io::Error::other("runtime readiness failure"));
                        }
                        if failure == 3 {
                            panic!("runtime readiness panic");
                        }
                    }
                    Builder::new_current_thread().enable_all().build()
                },
                budget.clone(),
            )
        }));
        if failure == 4 {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_err());
        }
        assert_eq!(retired.load(Ordering::SeqCst), [0, 1, 2, 2, 1][failure]);
        let reservations: Vec<_> = (0..3).map(|_| budget.try_reserve().unwrap()).collect();
        assert!(budget.try_reserve().is_err());
        drop(reservations);
        let replacement =
            ShardedScheduler::try_start_with_budget_and_startup(3, 1, || {}, budget).unwrap();
        replacement.shutdown_result().unwrap();
    }
}

#[test]
fn partial_spawn_failure_joins_every_started_thread_before_returning_the_cause() {
    for fail_at in [0, 1, 2] {
        let retired = Arc::new(AtomicUsize::new(0));
        let counter = retired.clone();
        let result = ShardedScheduler::try_start_with_hooks(
            3,
            1,
            || {},
            move |index| {
                if index == fail_at {
                    Err(std::io::Error::other("injected pthread creation failure"))
                } else {
                    Ok(())
                }
            },
            move |_| {
                EXIT.with(|slot| *slot.borrow_mut() = Some(Exit(counter.clone())));
                Builder::new_current_thread().enable_all().build()
            },
        );
        assert!(
            matches!(result, Err(SystemError::ThreadSetupError(ref message))
            if message.contains(&format!("shard {fail_at}")) && message.contains("injected pthread creation failure"))
        );
        assert_eq!(retired.load(Ordering::SeqCst), fail_at);
    }
}

#[test]
fn runtime_failure_or_panic_is_reported_and_all_created_threads_are_joined() {
    for panicking in [false, true] {
        let retired = Arc::new(AtomicUsize::new(0));
        let counter = retired.clone();
        let result = ShardedScheduler::try_start_with_hooks(
            3,
            1,
            || {},
            |_| Ok(()),
            move |index| {
                EXIT.with(|slot| *slot.borrow_mut() = Some(Exit(counter.clone())));
                if index == 1 {
                    if panicking {
                        panic!("injected runtime construction panic");
                    }
                    return Err(std::io::Error::other(
                        "injected runtime construction failure",
                    ));
                }
                Builder::new_current_thread().enable_all().build()
            },
        );
        let Err(SystemError::ThreadSetupError(message)) = result else {
            panic!("runtime construction failure was accepted")
        };
        assert!(message.contains("shard 1"), "{message}");
        assert!(
            message.contains(if panicking {
                "injected runtime construction panic"
            } else {
                "injected runtime construction failure"
            }),
            "{message}"
        );
        assert_eq!(retired.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn startup_unwind_does_not_detach_earlier_threads() {
    let retired = Arc::new(AtomicUsize::new(0));
    let counter = retired.clone();
    let result = std::panic::catch_unwind(|| {
        ShardedScheduler::try_start_with_hooks(
            2,
            1,
            || {},
            |index| {
                if index == 1 {
                    panic!("injected creator unwind");
                }
                Ok(())
            },
            move |_| {
                EXIT.with(|slot| *slot.borrow_mut() = Some(Exit(counter.clone())));
                Builder::new_current_thread().enable_all().build()
            },
        )
    });
    assert!(result.is_err());
    assert_eq!(retired.load(Ordering::SeqCst), 1);
}

#[test]
fn successful_construction_waits_for_runtime_readiness_and_shutdown_retires_all_threads() {
    let started = Arc::new(AtomicUsize::new(0));
    let retired = Arc::new(AtomicUsize::new(0));
    let count = started.clone();
    let exits = retired.clone();
    let scheduler = ShardedScheduler::try_start_with_hooks(
        2,
        1,
        || {},
        |_| Ok(()),
        move |_| {
            EXIT.with(|slot| *slot.borrow_mut() = Some(Exit(exits.clone())));
            let runtime = Builder::new_current_thread().enable_all().build()?;
            count.fetch_add(1, Ordering::SeqCst);
            Ok(runtime)
        },
    )
    .unwrap();
    assert_eq!(started.load(Ordering::SeqCst), 2);
    assert_eq!(retired.load(Ordering::SeqCst), 0);
    scheduler.shutdown_result().unwrap();
    scheduler.shutdown_result().unwrap();
    assert_eq!(retired.load(Ordering::SeqCst), 2);
}

#[test]
fn creation_error_retains_an_earlier_worker_failure() {
    let (began, observed) = std::sync::mpsc::sync_channel(1);
    let result = ShardedScheduler::try_start_with_hooks(
        2,
        1,
        move || {
            began.send(()).unwrap();
            panic!("earlier startup callback failure");
        },
        move |index| {
            if index == 1 {
                observed.recv_timeout(Duration::from_secs(5)).unwrap();
                Err(std::io::Error::other("later thread spawn failure"))
            } else {
                Ok(())
            }
        },
        |_| Builder::new_current_thread().enable_all().build(),
    );
    let Err(SystemError::ThreadSetupError(message)) = result else {
        panic!("partial creation failure lost")
    };
    assert!(message.contains("later thread spawn failure"), "{message}");
    assert!(
        message.contains("earlier startup callback failure"),
        "{message}"
    );
}

struct BlockedExit {
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
    retired: Arc<AtomicUsize>,
}
impl Drop for BlockedExit {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.release.recv_timeout(Duration::from_secs(30)).unwrap();
        self.retired.fetch_add(1, Ordering::SeqCst);
    }
}
thread_local! {static BLOCKED_EXIT: RefCell<Option<BlockedExit>> = const {RefCell::new(None)};}
struct Release(Option<std::sync::mpsc::SyncSender<()>>);
impl Drop for Release {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[test]
fn failed_construction_cannot_return_while_an_os_thread_tls_destructor_is_blocked() {
    let (entered, observed) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::sync_channel(1);
    let retired = Arc::new(AtomicUsize::new(0));
    let counter = retired.clone();
    let wait = Arc::new(Mutex::new(Some(wait)));
    let (done, completion) = std::sync::mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            let result = ShardedScheduler::try_start_with_hooks(
                2,
                1,
                || {},
                |index| {
                    if index == 1 {
                        Err(std::io::Error::other("gated spawn failure"))
                    } else {
                        Ok(())
                    }
                },
                move |_| {
                    BLOCKED_EXIT.with(|slot| {
                        *slot.borrow_mut() = Some(BlockedExit {
                            entered: entered.clone(),
                            release: wait.lock().unwrap().take().unwrap(),
                            retired: counter.clone(),
                        })
                    });
                    Builder::new_current_thread().enable_all().build()
                },
            );
            done.send(result.map(|scheduler| scheduler.shutdown_result().unwrap()))
                .unwrap();
        });
        let release = Release(Some(release));
        observed.recv_timeout(Duration::from_secs(5)).unwrap();
        let early = completion.recv_timeout(Duration::from_secs(1));
        let returned_early = early.is_ok();
        let before = retired.load(Ordering::SeqCst);
        drop(release);
        let result = match early {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                completion.recv_timeout(Duration::from_secs(5)).unwrap()
            }
            Err(error) => panic!("construction result lost: {error}"),
        };
        worker.join().unwrap();
        assert!(!returned_early, "creation returned before OS retirement");
        assert_eq!(before, 0);
        assert!(matches!(result, Err(SystemError::ThreadSetupError(_))));
        assert_eq!(retired.load(Ordering::SeqCst), 1);
    });
}
