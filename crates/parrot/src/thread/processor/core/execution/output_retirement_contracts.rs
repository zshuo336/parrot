use super::*;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

#[derive(Debug, Default)]
struct Counts {
    futures: AtomicUsize,
    outputs: AtomicUsize,
    payloads: AtomicUsize,
}

#[derive(Clone, Copy, Debug)]
enum Disposal {
    Clean,
    Panic,
    Recursive,
}

struct RecursivePayload(Arc<Counts>);
impl Drop for RecursivePayload {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("recursive result panic payload");
    }
}

#[derive(Debug)]
struct Output {
    counts: Arc<Counts>,
    disposal: Disposal,
    block: Option<(mpsc::SyncSender<()>, std::sync::Mutex<mpsc::Receiver<()>>)>,
}
impl std::fmt::Display for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("original execution error")
    }
}
impl std::error::Error for Output {}
impl Drop for Output {
    fn drop(&mut self) {
        self.counts.outputs.fetch_add(1, Ordering::SeqCst);
        if let Some((entered, release)) = self.block.take() {
            entered.send(()).unwrap();
            release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
        match self.disposal {
            Disposal::Clean => {}
            Disposal::Panic => panic!("discarded execution output destructor"),
            Disposal::Recursive => std::panic::panic_any(RecursivePayload(self.counts.clone())),
        }
    }
}

type Produce = Box<dyn FnOnce() -> Result<Output, SystemError> + Send>;
struct Delegate {
    counts: Arc<Counts>,
    produce: Option<Produce>,
    disposal: Disposal,
}
impl std::future::Future for Delegate {
    type Output = Result<Output, SystemError>;
    fn poll(mut self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(self.produce.take().expect("delegate polled twice")())
    }
}
impl Drop for Delegate {
    fn drop(&mut self) {
        self.counts.futures.fetch_add(1, Ordering::SeqCst);
        match self.disposal {
            Disposal::Clean => {}
            Disposal::Panic => panic!("delegate future destructor"),
            Disposal::Recursive => std::panic::panic_any(RecursivePayload(self.counts.clone())),
        }
    }
}

fn delegate(
    counts: &Arc<Counts>,
    error: bool,
    output_disposal: Disposal,
    future_disposal: Disposal,
    block: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
) -> Delegate {
    let observed = counts.clone();
    Delegate {
        counts: counts.clone(),
        disposal: future_disposal,
        produce: Some(Box::new(move || {
            let output = Output {
                counts: observed,
                disposal: output_disposal,
                block: block.map(|(entered, release)| (entered, std::sync::Mutex::new(release))),
            };
            if error {
                Err(SystemError::Other(anyhow::Error::new(output)))
            } else {
                Ok(output)
            }
        })),
    }
}

fn poll_catching<T>(
    future: BoxedProcessorFuture<Result<T, SystemError>>,
) -> Result<T, SystemError> {
    match panic::catch_unwind(AssertUnwindSafe(|| futures::executor::block_on(future))) {
        Ok(result) => result,
        Err(payload) => {
            // An old implementation can leak a recursive throwing payload.
            // Report the escaped unwind without causing a test-harness abort.
            std::mem::forget(payload);
            panic!("discarded execution outcome escaped its owner");
        }
    }
}

async fn check_discarded(error: bool, disposal: Disposal) {
    let owner = Arc::new(Executions::default());
    let counts = Arc::new(Counts::default());
    let future = owner.own(
        Box::pin(delegate(&counts, error, disposal, Disposal::Panic, None)),
        "output contract",
    );
    let failure = poll_catching(future).unwrap_err();
    let SystemError::Panic(message) = failure else {
        panic!("future disposal must retain its panic classification");
    };
    assert!(message.contains("delegate future destructor"), "{message}");
    assert!(message.contains("discarded execution outcome"), "{message}");
    match disposal {
        Disposal::Panic => assert!(
            message.contains("discarded execution output destructor"),
            "{message}"
        ),
        Disposal::Recursive => {
            assert!(message.contains("non-string panic payload"), "{message}");
            assert!(
                message.contains("panic payload destructor panicked"),
                "{message}"
            );
            assert_eq!(counts.payloads.load(Ordering::SeqCst), 1);
        }
        Disposal::Clean => unreachable!(),
    }
    for _ in 0..2 {
        let error = tokio::time::timeout(Duration::from_secs(2), owner.join())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            SystemError::Panic(message.clone()).to_string()
        );
    }
    assert_eq!(counts.futures.load(Ordering::SeqCst), 1);
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn returned_error_disposal_is_isolated_after_delegate_disposal_failure() {
    check_discarded(true, Disposal::Panic).await;
}

#[tokio::test]
async fn successful_output_disposal_is_isolated_after_delegate_disposal_failure() {
    check_discarded(false, Disposal::Panic).await;
}

#[tokio::test]
async fn recursive_returned_error_payload_is_retired_independently() {
    check_discarded(true, Disposal::Recursive).await;
}

#[tokio::test]
async fn recursive_successful_output_payload_is_retired_independently() {
    check_discarded(false, Disposal::Recursive).await;
}

async fn check_blocked(error: bool) {
    let owner = Arc::new(Executions::default());
    let counts = Arc::new(Counts::default());
    let (entered, wait_entered) = mpsc::sync_channel(1);
    let (release, wait_release) = mpsc::sync_channel(1);
    let future = owner.own(
        Box::pin(delegate(
            &counts,
            error,
            Disposal::Clean,
            Disposal::Panic,
            Some((entered, wait_release)),
        )),
        "blocked output contract",
    );
    let worker = std::thread::spawn(move || poll_catching(future));
    let entered = wait_entered.recv_timeout(Duration::from_secs(2));
    let mut receipt = Box::pin(owner.join());
    let while_blocked = receipt
        .as_mut()
        .poll(&mut Context::from_waker(std::task::Waker::noop()));
    release.send(()).unwrap();
    let result = worker.join().expect("execution poll escaped protection");
    assert!(entered.is_ok(), "output destructor never actually entered");
    assert!(
        while_blocked.is_pending(),
        "execution retired before output disposal: {while_blocked:?}"
    );
    let failure = result.unwrap_err().to_string();
    assert!(failure.contains("delegate future destructor"), "{failure}");
    let observed = tokio::time::timeout(Duration::from_secs(2), receipt)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(observed.to_string(), failure);
    assert_eq!(counts.futures.load(Ordering::SeqCst), 1);
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retirement_waits_for_discarded_error_to_finish_disposal() {
    check_blocked(true).await;
}

#[tokio::test]
async fn retirement_waits_for_discarded_success_to_finish_disposal() {
    check_blocked(false).await;
}

#[tokio::test]
async fn clean_completion_transfers_output_without_running_its_destructor() {
    let owner = Arc::new(Executions::default());
    let counts = Arc::new(Counts::default());
    let output = poll_catching(owner.own(
        Box::pin(delegate(
            &counts,
            false,
            Disposal::Clean,
            Disposal::Clean,
            None,
        )),
        "clean output",
    ))
    .unwrap();
    owner.join().await.unwrap();
    assert_eq!(counts.futures.load(Ordering::SeqCst), 1);
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 0);
    drop(output);
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn clean_future_disposal_preserves_original_error_type_and_ownership() {
    let owner = Arc::new(Executions::default());
    let counts = Arc::new(Counts::default());
    let error = poll_catching(owner.own(
        Box::pin(delegate(
            &counts,
            true,
            Disposal::Clean,
            Disposal::Clean,
            None,
        )),
        "clean error",
    ))
    .unwrap_err();
    owner.join().await.unwrap();
    let SystemError::Other(original) = error else {
        panic!("original error class lost")
    };
    assert!(original.downcast_ref::<Output>().is_some());
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 0);
    drop(original);
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn recursive_delegate_payload_is_recorded_before_clean_output_disposal() {
    let owner = Arc::new(Executions::default());
    let counts = Arc::new(Counts::default());
    let error = poll_catching(owner.own(
        Box::pin(delegate(
            &counts,
            false,
            Disposal::Clean,
            Disposal::Recursive,
            None,
        )),
        "recursive delegate",
    ))
    .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("panic payload destructor panicked"),
        "{message}"
    );
    assert_eq!(counts.payloads.load(Ordering::SeqCst), 1);
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 1);
    assert_eq!(owner.join().await.unwrap_err().to_string(), message);
}
