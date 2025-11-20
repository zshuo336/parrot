use super::*;
use std::{
    sync::{atomic::AtomicUsize, mpsc},
    time::Duration,
};

struct Capture {
    drops: Arc<AtomicUsize>,
    failure: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.failure, "later capture disposal failure");
    }
}

struct Output {
    drops: Arc<AtomicUsize>,
    failure: bool,
}
impl Drop for Output {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.failure, "first direct output disposal failure");
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn disposal_error(result: Result<(), ConstructionError<()>>) -> String {
    match result {
        Err(ConstructionError::DisposalPanicked(error)) => error,
        other => panic!("output cleanup lost its error class: {other:?}"),
    }
}

async fn assert_retirement(factory: &Factory<Output>, expected: &str) {
    let (first, second) = tokio::join!(factory.retire(), factory.retire());
    assert_eq!(first.unwrap_err(), expected);
    assert_eq!(second.unwrap_err(), expected);
    assert_eq!(factory.retire().await.unwrap_err(), expected);
}

#[tokio::test]
async fn direct_publication_disposal_failure_survives_later_success() {
    let outputs = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let builds = AtomicUsize::new(0);
    let observed = outputs.clone();
    let capture = Capture {
        drops: captures.clone(),
        failure: false,
    };
    let factory = Factory::new(
        move || {
            let _ = &capture;
            Output {
                drops: observed.clone(),
                failure: builds.fetch_add(1, Ordering::SeqCst) == 0,
            }
        },
        Arc::new(Semaphore::new(1)),
        Arc::new(Semaphore::new(1)),
    );
    let failure = disposal_error(
        factory
            .construct(Arc::default(), |output| {
                drop(output);
                Ok(())
            })
            .await,
    );
    assert!(failure.contains("first direct output disposal failure"));
    factory
        .construct(Arc::default(), |output| {
            drop(output);
            Ok::<_, ()>(())
        })
        .await
        .unwrap();
    assert_eq!(outputs.load(Ordering::SeqCst), 2);
    assert_retirement(&factory, &failure).await;
    assert_eq!(captures.load(Ordering::SeqCst), 1);
}

struct Release(Option<mpsc::Sender<()>>);
impl Release {
    fn open(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        self.open();
    }
}

struct Blocked {
    factory: Arc<Factory<Output>>,
    entered: tokio::sync::oneshot::Receiver<()>,
    release: Release,
    outputs: Arc<AtomicUsize>,
    captures: Arc<AtomicUsize>,
}
fn blocked(capture_failure: bool) -> Blocked {
    let outputs = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let observed = outputs.clone();
    let capture = Capture {
        drops: captures.clone(),
        failure: capture_failure,
    };
    let (entered, observed_entry) = tokio::sync::oneshot::channel();
    let entered = Mutex::new(Some(entered));
    let (release, held) = mpsc::channel();
    let held = Mutex::new(held);
    Blocked {
        factory: Factory::new(
            move || {
                let _ = &capture;
                entered.lock().unwrap().take().unwrap().send(()).unwrap();
                held.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .expect("constructor gate was never released");
                Output {
                    drops: observed.clone(),
                    failure: true,
                }
            },
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(1)),
        ),
        entered: observed_entry,
        release: Release(Some(release)),
        outputs,
        captures,
    }
}

async fn entered(receipt: tokio::sync::oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(3), receipt)
        .await
        .expect("constructor did not enter")
        .unwrap();
}

#[tokio::test]
async fn cancelled_direct_output_failure_precedes_later_capture_failure() {
    let mut fixture = blocked(true);
    let control = Arc::new(ConstructionControl::default());
    let worker_factory = fixture.factory.clone();
    let worker_control = control.clone();
    let publications = Arc::new(AtomicUsize::new(0));
    let published = publications.clone();
    let worker = tokio::spawn(async move {
        worker_factory
            .construct(worker_control, move |_| {
                published.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(())
            })
            .await
    });
    entered(fixture.entered).await;
    let mut retiring = fixture.factory.retire();
    assert!(futures::poll!(&mut retiring).is_pending());
    assert_eq!(fixture.outputs.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.captures.load(Ordering::SeqCst), 0);
    control.cancel();
    fixture.release.open();
    let failure = disposal_error(worker.await.unwrap());
    assert!(failure.contains("first direct output disposal failure"));
    assert_eq!(retiring.await.unwrap_err(), failure);
    assert_retirement(&fixture.factory, &failure).await;
    assert_eq!(publications.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.outputs.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.captures.load(Ordering::SeqCst), 1);
}

#[test]
fn abandoned_direct_waiter_keeps_failure_after_original_host_exit() {
    crate::test_support::isolated(
        "constructor::direct_retirement_contracts::abandoned_direct_waiter_keeps_failure_after_original_host_exit",
        || {
            let runtime = runtime();
            let mut fixture = blocked(false);
            let control = Arc::new(ConstructionControl::default());
            let worker_control = control.clone();
            let worker_factory = fixture.factory.clone();
            let worker = runtime.spawn(async move {
                worker_factory
                    .construct(worker_control, |_| Ok::<_, ()>(()))
                    .await
            });
            runtime.block_on(entered(fixture.entered));
            let mut retiring = {
                let _entered = runtime.enter();
                fixture.factory.retire()
            };
            runtime.block_on(async {
                assert!(futures::poll!(&mut retiring).is_pending());
            });
            control.cancel();
            worker.abort();
            runtime.shutdown_timeout(Duration::from_millis(20));
            assert_eq!(fixture.outputs.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.captures.load(Ordering::SeqCst), 0);
            assert!(
                futures::executor::block_on(async { futures::poll!(&mut retiring) }).is_pending()
            );
            fixture.release.open();
            let result = futures::executor::block_on(retiring);
            let error = result.expect_err("abandoned constructor lost its output disposal failure");
            assert!(
                error.contains("first direct output disposal failure"),
                "{error}"
            );
            futures::executor::block_on(assert_retirement(&fixture.factory, &error));
            assert_eq!(fixture.outputs.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.captures.load(Ordering::SeqCst), 1);
        },
    );
}

#[tokio::test]
async fn rejected_publication_without_cleanup_failure_keeps_successful_retirement() {
    let outputs = Arc::new(AtomicUsize::new(0));
    let observed = outputs.clone();
    let factory = Factory::new(
        move || Output {
            drops: observed.clone(),
            failure: false,
        },
        Arc::new(Semaphore::new(1)),
        Arc::new(Semaphore::new(1)),
    );
    let result = factory
        .construct(Arc::default(), |output| {
            drop(output);
            Err::<(), _>(())
        })
        .await;
    assert!(matches!(result, Err(ConstructionError::Publication(()))));
    factory.retire().await.unwrap();
    factory.retire().await.unwrap();
    assert_eq!(outputs.load(Ordering::SeqCst), 1);
}
