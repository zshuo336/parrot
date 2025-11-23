use super::*;
use std::sync::mpsc;

const WAIT: Duration = Duration::from_secs(3);

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
        panic!("isolated outcome panic payload destructor");
    }
}

#[derive(Debug)]
struct Output {
    counts: Arc<Counts>,
    disposal: Disposal,
    marker: u32,
    block: Option<(mpsc::SyncSender<()>, Mutex<mpsc::Receiver<()>>)>,
}
impl fmt::Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("original isolated business error")
    }
}
impl std::error::Error for Output {}
impl Drop for Output {
    fn drop(&mut self) {
        self.counts.outputs.fetch_add(1, Ordering::SeqCst);
        if let Some((entered, release)) = self.block.take() {
            entered.send(()).unwrap();
            release.lock().unwrap().recv_timeout(WAIT).unwrap();
        }
        match self.disposal {
            Disposal::Clean => {}
            Disposal::Panic => panic!("discarded isolated outcome destructor"),
            Disposal::Recursive => std::panic::panic_any(RecursivePayload(self.counts.clone())),
        }
    }
}

struct Delegate {
    counts: Arc<Counts>,
    output: Option<anyhow::Result<Output>>,
    panic_drop: bool,
}
impl Future for Delegate {
    type Output = anyhow::Result<Output>;
    fn poll(mut self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(self.output.take().expect("delegate polled twice"))
    }
}
impl Drop for Delegate {
    fn drop(&mut self) {
        self.counts.futures.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "isolated delegate destructor");
    }
}

fn delegate(
    counts: &Arc<Counts>,
    error: bool,
    disposal: Disposal,
    panic_drop: bool,
    block: Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>,
) -> Delegate {
    let output = Output {
        counts: counts.clone(),
        disposal,
        marker: 73,
        block: block.map(|(entered, release)| (entered, Mutex::new(release))),
    };
    Delegate {
        counts: counts.clone(),
        output: Some(if error {
            Err(anyhow::Error::new(output))
        } else {
            Ok(output)
        }),
        panic_drop,
    }
}

async fn discarded(error: bool, disposal: Disposal) {
    let counts = Arc::new(Counts::default());
    let slots = Arc::new(Semaphore::new(1));
    let finished = Arc::new(Notify::new());
    let permit = slots.clone().acquire_owned().await.unwrap();
    let failure = poll_isolated(
        delegate(&counts, error, disposal, true, None),
        permit,
        slots.clone(),
        finished.clone(),
    )
    .await
    .unwrap_err();
    let message = failure.to_string();
    assert!(
        message.contains("isolated delegate destructor"),
        "{message}"
    );
    assert!(
        message.contains("discarded isolated outcome disposal failed"),
        "{message}"
    );
    match disposal {
        Disposal::Panic => assert!(
            message.contains("discarded isolated outcome destructor"),
            "{message}"
        ),
        Disposal::Recursive => assert!(
            message.contains("panic payload destructor panicked"),
            "{message}"
        ),
        Disposal::Clean => unreachable!(),
    }
    assert_eq!(counts.futures.load(Ordering::SeqCst), 1);
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        usize::from(matches!(disposal, Disposal::Recursive))
    );
    assert_eq!(slots.available_permits(), 1);
    tokio::time::timeout(WAIT, finished.notified())
        .await
        .expect("failed disposal skipped poll completion notification");
    let permit = slots.clone().acquire_owned().await.unwrap();
    assert_eq!(
        poll_isolated(async { Ok(91) }, permit, slots, finished)
            .await
            .unwrap(),
        91
    );
}

#[tokio::test]
async fn discarded_success_panic_preserves_both_failures_and_completion() {
    discarded(false, Disposal::Panic).await;
}
#[tokio::test]
async fn discarded_error_panic_preserves_both_failures_and_completion() {
    discarded(true, Disposal::Panic).await;
}
#[tokio::test]
async fn recursive_success_disposal_preserves_the_original_future_failure() {
    discarded(false, Disposal::Recursive).await;
}
#[tokio::test]
async fn recursive_error_disposal_preserves_the_original_future_failure() {
    discarded(true, Disposal::Recursive).await;
}

async fn blocked(error: bool) {
    let counts = Arc::new(Counts::default());
    let slots = Arc::new(Semaphore::new(1));
    let finished = Arc::new(Notify::new());
    let permit = slots.clone().acquire_owned().await.unwrap();
    let (entered, observe_entered) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let owner = tokio::spawn(poll_isolated(
        delegate(&counts, error, Disposal::Clean, true, Some((entered, wait))),
        permit,
        slots.clone(),
        finished.clone(),
    ));
    let entered = tokio::task::spawn_blocking(move || observe_entered.recv_timeout(WAIT))
        .await
        .unwrap();
    let retained = slots.available_permits() == 0 && !owner.is_finished();
    let mut completion = Box::pin(finished.notified());
    let premature = futures::poll!(completion.as_mut()).is_ready();
    release.send(()).unwrap();
    let result = tokio::time::timeout(WAIT, owner).await.unwrap().unwrap();
    assert!(
        entered.is_ok(),
        "discarded outcome destructor never entered"
    );
    assert!(
        retained && !premature,
        "poll completed before actual outcome disposal"
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("isolated delegate destructor")
    );
    tokio::time::timeout(WAIT, completion).await.unwrap();
    assert_eq!(slots.available_permits(), 1);
    assert_eq!(counts.futures.load(Ordering::SeqCst), 1);
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn blocked_discarded_success_retains_permit_and_completion_until_disposal() {
    blocked(false).await;
}
#[tokio::test]
async fn blocked_discarded_error_retains_permit_and_completion_until_disposal() {
    blocked(true).await;
}

async fn clean(error: bool) {
    let counts = Arc::new(Counts::default());
    let slots = Arc::new(Semaphore::new(1));
    let permit = slots.clone().acquire_owned().await.unwrap();
    let result = poll_isolated(
        delegate(&counts, error, Disposal::Clean, false, None),
        permit,
        slots.clone(),
        Arc::new(Notify::new()),
    )
    .await;
    let output = if error {
        result.unwrap_err().downcast::<Output>().unwrap()
    } else {
        result.unwrap()
    };
    assert_eq!(output.marker, 73);
    assert_eq!(counts.futures.load(Ordering::SeqCst), 1);
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 0);
    assert_eq!(slots.available_permits(), 1);
    drop(output);
    assert_eq!(counts.outputs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn clean_completion_transfers_success_without_disposal() {
    clean(false).await;
}
#[tokio::test]
async fn clean_completion_preserves_original_error_type_and_ownership() {
    clean(true).await;
}
