use super::*;
use crate::owned_payload::permit_wake_contracts::watch;
use std::{
    sync::{atomic::AtomicUsize, mpsc},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct Output(Arc<AtomicUsize>);
impl Drop for Output {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn host() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn constructed(prepared: bool, panics: bool, publication_error: bool) {
    let host = host();
    let workers = Arc::new(Semaphore::new(1));
    let (entered, observe) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let wait = Mutex::new(wait);
    let drops = Arc::new(AtomicUsize::new(0));
    let moved = drops.clone();
    let factory = Factory::new(
        move || {
            entered.send(()).unwrap();
            wait.lock().unwrap().recv_timeout(WAIT).unwrap();
            assert!(
                !panics,
                "original constructor failure before worker release"
            );
            Output(moved.clone())
        },
        workers.clone(),
        Arc::new(Semaphore::new(1)),
    );
    let owner = factory.clone();
    host.block_on(async {
        let operation = tokio::spawn(async move {
            if prepared {
                owner
                    .prepare::<Output>(Arc::default())
                    .await
                    .map(|value| value.adopt())
            } else {
                owner
                    .construct(Arc::default(), move |output| {
                        if publication_error {
                            Err(output)
                        } else {
                            Ok(output)
                        }
                    })
                    .await
            }
        });
        tokio::task::spawn_blocking(move || observe.recv_timeout(WAIT))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(workers.available_permits(), 0);
        let waiter = watch(workers.clone().acquire_owned());
        release.send(()).unwrap();
        let result = tokio::time::timeout(WAIT, operation)
            .await
            .unwrap()
            .unwrap();
        let capacity = waiter.ready().unwrap();
        assert_eq!(workers.available_permits(), 0);
        drop(capacity);
        assert_eq!(workers.available_permits(), 1);
        if panics {
            assert!(
                matches!(result, Err(ConstructionError::ConstructorPanicked(ref message))
                if message.contains("original constructor failure before worker release")),
                "worker quota wake replaced the actual constructor error"
            );
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        } else {
            let output = if publication_error {
                match result {
                    Err(ConstructionError::Publication(output)) => output,
                    _ => panic!("worker quota wake replaced the original business error"),
                }
            } else {
                match result {
                    Ok(output) => output,
                    _ => panic!("worker quota wake lost the constructed output"),
                }
            };
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(output);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
        factory.retire().await.unwrap();
    });
}

fn published(publication_error: bool, panics: bool) {
    let host = host();
    let workers = Arc::new(Semaphore::new(1));
    let factory = Factory::new(|| 37u32, workers.clone(), Arc::new(Semaphore::new(1)));
    host.block_on(async {
        let prepared = factory.prepare::<Output>(Arc::default()).await.unwrap();
        let (entered, observe) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let drops = Arc::new(AtomicUsize::new(0));
        let moved = drops.clone();
        let operation = tokio::spawn(prepared.publish(move |value| {
            assert_eq!(value, 37);
            entered.send(()).unwrap();
            wait.recv_timeout(WAIT).unwrap();
            assert!(
                !panics,
                "original publication failure before worker release"
            );
            let output = Output(moved);
            if publication_error {
                Err(output)
            } else {
                Ok(output)
            }
        }));
        tokio::task::spawn_blocking(move || observe.recv_timeout(WAIT))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(workers.available_permits(), 0);
        let waiter = watch(workers.clone().acquire_owned());
        release.send(()).unwrap();
        let result = tokio::time::timeout(WAIT, operation)
            .await
            .unwrap()
            .unwrap();
        let capacity = waiter.ready().unwrap();
        drop(capacity);
        assert_eq!(workers.available_permits(), 1);
        if panics {
            assert!(
                matches!(result, Err(ConstructionError::DisposalPanicked(ref message))
                if message.contains("original publication failure before worker release")),
                "worker quota wake replaced the actual publication failure"
            );
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        } else {
            let output = if publication_error {
                match result {
                    Err(ConstructionError::Publication(output)) => output,
                    _ => panic!("worker quota wake replaced the original publication error"),
                }
            } else {
                match result {
                    Ok(output) => output,
                    _ => panic!("worker quota wake lost the published output"),
                }
            };
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(output);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
        if panics {
            assert!(
                factory
                    .retire()
                    .await
                    .unwrap_err()
                    .contains("original publication failure before worker release")
            );
        } else {
            factory.retire().await.unwrap();
        }
    });
}

#[test]
fn construct_success_keeps_output_when_worker_quota_wake_panics() {
    constructed(false, false, false);
}
#[test]
fn construct_business_error_keeps_type_when_worker_quota_wake_panics() {
    constructed(false, false, true);
}
#[test]
fn construct_panic_keeps_original_error_when_worker_quota_wake_panics() {
    constructed(false, true, false);
}
#[test]
fn prepare_success_keeps_output_when_worker_quota_wake_panics() {
    constructed(true, false, false);
}
#[test]
fn prepare_panic_keeps_original_error_when_worker_quota_wake_panics() {
    constructed(true, true, false);
}
#[test]
fn publish_success_keeps_output_when_worker_quota_wake_panics() {
    published(false, false);
}
#[test]
fn publish_business_error_keeps_type_when_worker_quota_wake_panics() {
    published(true, false);
}
#[test]
fn publish_panic_keeps_original_error_when_worker_quota_wake_panics() {
    published(false, true);
}
