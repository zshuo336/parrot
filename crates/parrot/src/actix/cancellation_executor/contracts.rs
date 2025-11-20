use super::*;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

fn executor(capacity: usize) -> Arc<Executor> {
    Executor::with_spawner(capacity, |_, job| std::thread::Builder::new().spawn(job)).unwrap()
}

fn finish(executor: Arc<Executor>, capacity: u32) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let permit = runtime
        .block_on(async {
            tokio::time::timeout(WAIT, executor.slots.clone().acquire_many_owned(capacity)).await
        })
        .unwrap()
        .unwrap();
    drop(executor);
    drop(permit);
}

fn startup_failure(panics: bool) {
    let exited = Arc::new(AtomicUsize::new(0));
    let counted = exited.clone();
    let result = Executor::with_spawner(3, move |index, job| {
        if index == 1 {
            if panics {
                panic!("cancellation executor factory witness");
            }
            return Err(std::io::Error::other(
                "cancellation executor factory witness",
            ));
        }
        let counted = counted.clone();
        std::thread::Builder::new().spawn(move || {
            job();
            counted.fetch_add(1, Ordering::SeqCst);
        })
    });
    let error = match result {
        Ok(_) => panic!("failed startup returned an executor"),
        Err(error) => error,
    };
    assert!(
        error.contains("cancellation executor factory witness"),
        "{error}"
    );
    assert_eq!(
        exited.load(Ordering::SeqCst),
        1,
        "partial executor escaped without joining its started thread"
    );
}

#[test]
fn partial_executor_creation_error_joins_started_thread() {
    startup_failure(false);
}

#[test]
fn partial_executor_factory_panic_joins_started_thread() {
    startup_failure(true);
}

#[test]
fn startup_reservations_bound_live_and_queued_cancellation_work() {
    let executor = executor(3);
    let mut reservations = Vec::new();
    for _ in 0..3 {
        reservations.push(executor.reserve().unwrap());
    }
    assert!(executor.reserve().is_err());
    let (entered, receive_entered) = mpsc::channel();
    let (done, receive_done) = mpsc::channel();
    let mut releases = Vec::new();
    for reservation in reservations {
        let (release, receive) = mpsc::channel();
        releases.push(release);
        let entered = entered.clone();
        let done = done.clone();
        reservation.submit(move || {
            entered.send(()).unwrap();
            receive.recv_timeout(WAIT).unwrap();
            done.send(()).unwrap();
        });
    }
    receive_entered.recv_timeout(WAIT).unwrap();
    receive_entered.recv_timeout(WAIT).unwrap();
    let third_entered_early = receive_entered.try_recv().is_ok();
    let admitted_early = executor.reserve().is_ok();
    for release in releases {
        release.send(()).unwrap();
    }
    for _ in 0..3 {
        receive_done.recv_timeout(WAIT).unwrap();
    }
    let slots = executor.slots.clone();
    finish(executor, 3);
    assert!(
        !third_entered_early,
        "bounded executor started a third worker"
    );
    assert!(
        !admitted_early,
        "queued work released its startup reservation"
    );
    assert_eq!(slots.available_permits(), 3);
}

#[test]
fn cancellation_job_and_payload_panics_do_not_destroy_executor_workers() {
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("cancellation panic payload disposal witness");
        }
    }
    let executor = executor(4);
    let payloads = Arc::new(AtomicUsize::new(0));
    let (entered, receive_entered) = mpsc::channel();
    let (release, receive_release) = mpsc::channel();
    // Occupy one worker, so recovery and the healthy job must use the other.
    executor.reserve().unwrap().submit(move || {
        entered.send(()).unwrap();
        receive_release.recv_timeout(WAIT * 3).unwrap();
    });
    receive_entered.recv_timeout(WAIT).unwrap();
    let counted = payloads.clone();
    executor
        .reserve()
        .unwrap()
        .submit(move || std::panic::panic_any(Payload(counted)));
    let (done, receive_done) = mpsc::channel();
    executor.reserve().unwrap().submit(move || {
        done.send(()).unwrap();
    });
    let recovered = receive_done.recv_timeout(WAIT);
    release.send(()).unwrap();
    finish(executor, 4);
    assert!(
        recovered.is_ok(),
        "panic destroyed the only available cancellation worker"
    );
    assert_eq!(payloads.load(Ordering::SeqCst), 1);
}
