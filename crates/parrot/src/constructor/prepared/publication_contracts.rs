use super::*;
use std::{
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct Counts {
    captures: AtomicUsize,
    outcomes: AtomicUsize,
    payloads: AtomicUsize,
}

struct Capture(Arc<Counts>, bool);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.captures.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "original prepared factory capture failure");
    }
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
        panic!("prepared outcome panic payload destructor");
    }
}

#[derive(Debug)]
struct Outcome {
    counts: Arc<Counts>,
    disposal: Disposal,
    marker: u32,
    blocked: Option<(mpsc::SyncSender<()>, Mutex<mpsc::Receiver<()>>)>,
}
impl Drop for Outcome {
    fn drop(&mut self) {
        self.counts.outcomes.fetch_add(1, Ordering::SeqCst);
        if let Some((entered, release)) = self.blocked.take() {
            entered.send(()).unwrap();
            release.lock().unwrap().recv_timeout(WAIT).unwrap();
        }
        match self.disposal {
            Disposal::Clean => {}
            Disposal::Panic => panic!("discarded prepared publication outcome destructor"),
            Disposal::Recursive => std::panic::panic_any(RecursivePayload(self.counts.clone())),
        }
    }
}

async fn prepared(counts: &Arc<Counts>, capture_panics: bool) -> Prepared<u32> {
    let capture = Capture(counts.clone(), capture_panics);
    let factory = Factory::new(
        move || {
            std::hint::black_box(&capture);
            71
        },
        Arc::new(Semaphore::new(1)),
        Arc::new(Semaphore::new(1)),
    );
    let value = factory.prepare::<()>(Arc::default()).await.unwrap();
    drop(factory);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
    value
}

async fn discarded(error: bool, disposal: Disposal) {
    let counts = Arc::new(Counts::default());
    let value = prepared(&counts, true).await;
    let workers = value.workers.clone();
    let admission = value
        .parts
        .as_ref()
        .unwrap()
        .1
        .cleanup
        .as_ref()
        .unwrap()
        .clone();
    let outcome = Outcome {
        counts: counts.clone(),
        disposal,
        marker: 89,
        blocked: None,
    };
    let failure = value
        .publish(move |actor| -> Result<Outcome, Outcome> {
            assert_eq!(actor, 71);
            if error { Err(outcome) } else { Ok(outcome) }
        })
        .await;
    let message = match failure {
        Err(ConstructionError::DisposalPanicked(message)) => message,
        other => panic!("expected actual capture disposal failure, got {other:?}"),
    };
    assert!(
        message.contains("original prepared factory capture failure"),
        "{message}"
    );
    assert!(
        message.contains("discarded prepared publication outcome disposal failed"),
        "{message}"
    );
    match disposal {
        Disposal::Panic => assert!(
            message.contains("discarded prepared publication outcome destructor"),
            "{message}"
        ),
        Disposal::Recursive => assert!(
            message.contains("panic payload destructor panicked"),
            "{message}"
        ),
        Disposal::Clean => unreachable!(),
    }
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    assert_eq!(counts.outcomes.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        usize::from(matches!(disposal, Disposal::Recursive))
    );
    assert_eq!(workers.available_permits(), 1);
    assert_eq!(Arc::strong_count(&admission), 1);
}

#[tokio::test]
async fn discarded_success_panic_preserves_final_capture_failure() {
    discarded(false, Disposal::Panic).await;
}
#[tokio::test]
async fn discarded_error_panic_preserves_final_capture_failure() {
    discarded(true, Disposal::Panic).await;
}
#[tokio::test]
async fn recursive_success_disposal_preserves_final_capture_failure() {
    discarded(false, Disposal::Recursive).await;
}
#[tokio::test]
async fn recursive_error_disposal_preserves_final_capture_failure() {
    discarded(true, Disposal::Recursive).await;
}

async fn blocked(error: bool) {
    let counts = Arc::new(Counts::default());
    let value = prepared(&counts, true).await;
    let workers = value.workers.clone();
    let (entered, observe_entered) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let outcome = Outcome {
        counts: counts.clone(),
        disposal: Disposal::Clean,
        marker: 89,
        blocked: Some((entered, Mutex::new(wait))),
    };
    let owner = tokio::spawn(value.publish(move |_| -> Result<Outcome, Outcome> {
        if error { Err(outcome) } else { Ok(outcome) }
    }));
    let started = tokio::task::spawn_blocking(move || observe_entered.recv_timeout(WAIT))
        .await
        .unwrap();
    let retained = workers.available_permits() == 0 && !owner.is_finished();
    release.send(()).unwrap();
    let result = tokio::time::timeout(WAIT, owner).await.unwrap().unwrap();
    assert!(
        started.is_ok(),
        "discarded publication outcome was not disposed"
    );
    assert!(
        retained,
        "publication finished before actual outcome disposal"
    );
    assert!(
        matches!(result, Err(ConstructionError::DisposalPanicked(ref message))
        if message.contains("original prepared factory capture failure")),
        "{result:?}"
    );
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    assert_eq!(counts.outcomes.load(Ordering::SeqCst), 1);
    assert_eq!(workers.available_permits(), 1);
}

#[tokio::test]
async fn blocked_discarded_success_retains_publication_worker_until_disposal() {
    blocked(false).await;
}
#[tokio::test]
async fn blocked_discarded_error_retains_publication_worker_until_disposal() {
    blocked(true).await;
}

async fn clean(error: bool) {
    let counts = Arc::new(Counts::default());
    let value = prepared(&counts, false).await;
    let outcome = Outcome {
        counts: counts.clone(),
        disposal: Disposal::Clean,
        marker: 89,
        blocked: None,
    };
    let result = value
        .publish(move |_| -> Result<Outcome, Outcome> {
            if error { Err(outcome) } else { Ok(outcome) }
        })
        .await;
    let outcome = if error {
        match result {
            Err(ConstructionError::Publication(error)) => error,
            other => panic!("original publication error lost: {other:?}"),
        }
    } else {
        result.unwrap()
    };
    assert_eq!(outcome.marker, 89);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    assert_eq!(counts.outcomes.load(Ordering::SeqCst), 0);
    drop(outcome);
    assert_eq!(counts.outcomes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn clean_success_preserves_original_output_ownership() {
    clean(false).await;
}
#[tokio::test]
async fn clean_error_preserves_original_publication_error_type() {
    clean(true).await;
}
