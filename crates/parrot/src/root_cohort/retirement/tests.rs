use super::*;
use std::sync::{
    Condvar,
    atomic::{AtomicUsize, Ordering},
};

const WAIT: Duration = Duration::from_secs(5);

fn build() -> std::io::Result<Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
}

fn spawn_failure(panic: bool) {
    let builds = Arc::new(AtomicUsize::new(0));
    let observed = builds.clone();
    let result = Driver::start(
        1,
        move || {
            observed.fetch_add(1, Ordering::SeqCst);
            build()
        },
        move |_job| {
            if panic {
                panic!("retirement OS spawn panic");
            }
            Err(std::io::Error::other("retirement OS spawn error"))
        },
    );
    let message = result.err().expect("spawn failure was accepted");
    assert!(message.contains(if panic {
        "retirement OS spawn panic"
    } else {
        "retirement OS spawn error"
    }));
    assert_eq!(
        builds.load(Ordering::SeqCst),
        0,
        "failed OS spawn constructed a runtime"
    );
}
#[test]
fn os_spawn_error_does_not_construct_runtime() {
    spawn_failure(false);
}
#[test]
fn os_spawn_panic_does_not_construct_runtime() {
    spawn_failure(true);
}

struct Exited(Arc<AtomicUsize>);
impl Drop for Exited {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn build_failure(panic: bool) {
    let exits = Arc::new(AtomicUsize::new(0));
    let exited = exits.clone();
    let result = Driver::start(
        1,
        move || {
            if panic {
                panic!("retirement runtime build panic");
            }
            Err(std::io::Error::other("retirement runtime build error"))
        },
        move |job| {
            std::thread::Builder::new().spawn(move || {
                let _exited = Exited(exited);
                job();
            })
        },
    );
    let message = result.err().expect("failed runtime was accepted");
    assert!(message.contains(if panic {
        "retirement runtime build panic"
    } else {
        "retirement runtime build error"
    }));
    assert_eq!(
        exits.load(Ordering::SeqCst),
        1,
        "startup returned before joining the failed driver"
    );
}
#[test]
fn runtime_build_error_joins_started_driver() {
    build_failure(false);
}
#[test]
fn runtime_build_panic_joins_started_driver() {
    build_failure(true);
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    entered: tokio::sync::Notify,
    disposed: AtomicUsize,
}
impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Policy(Arc<Gate>);
impl Policy {
    fn decide(&self) -> parrot_api::supervisor::SupervisionDecision {
        parrot_api::supervisor::SupervisionDecision::Stop
    }
}
impl Drop for Policy {
    fn drop(&mut self) {
        self.0.entered.notify_one();
        let (open, timeout) = self
            .0
            .changed
            .wait_timeout_while(self.0.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(
            *open || !timeout.timed_out(),
            "capacity test did not release policy"
        );
        self.0.disposed.fetch_add(1, Ordering::SeqCst);
    }
}

fn capacity(retain_handle: bool) {
    // Keep an external driver owner until every task and reservation is retired.
    let driver = Driver::start(1, build, |job| std::thread::Builder::new().spawn(job)).unwrap();
    let retirement = Arc::new(Retirement {
        reservation: driver.reserve().unwrap(),
        completion: Mutex::new(None),
    });
    assert!(driver.reserve().is_err());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let policy = Policy(gate.clone());
    let scope = ChildScope::new(WAIT, Arc::new(Semaphore::new(1)));
    scope.set_strategy(SupervisorStrategyType::OneForOne(
        parrot_api::supervisor::OneForOneStrategy {
            max_restarts: 0,
            within: WAIT,
            decider: parrot_api::supervisor::BasicDecisionFn::new(move |_| policy.decide()),
        },
    ));
    let waiting = retirement.start(scope, Weak::new());
    let mut retained = Some(retirement);
    if !retain_handle {
        drop(retained.take());
    }
    let observer = build().unwrap();
    observer.block_on(async {
        tokio::time::timeout(WAIT, gate.entered.notified())
            .await
            .unwrap();
        assert!(
            driver.reserve().is_err(),
            "pending cleanup released its reservation"
        );
        assert_eq!(gate.disposed.load(Ordering::SeqCst), 0);
        gate.release();
        tokio::time::timeout(WAIT, waiting).await.unwrap().unwrap();
        assert_eq!(gate.disposed.load(Ordering::SeqCst), 1);
        if retain_handle {
            assert!(
                driver.reserve().is_err(),
                "retained terminal handle released its reservation"
            );
            drop(retained.take());
        }
        tokio::time::timeout(WAIT, async {
            loop {
                if let Ok(reservation) = driver.reserve() {
                    drop(reservation);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual disposal did not restore capacity");
    });
    drop(observer);
    drop(driver);
}
#[test]
fn pending_cleanup_retains_capacity_after_last_public_handle() {
    capacity(false);
}
#[test]
fn completed_handle_retains_capacity_until_last_clone_is_released() {
    capacity(true);
}
