use super::*;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    task::{Context, Poll},
};
use tokio::sync::oneshot;

const WAIT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
enum Completion {
    Pending,
    Ready,
    Panic,
}

struct Probe {
    completion: Completion,
    entered: Option<oneshot::Sender<()>>,
    release: mpsc::Receiver<()>,
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl Future for Probe {
    type Output = usize;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        match self.completion {
            Completion::Pending => Poll::Pending,
            Completion::Ready => Poll::Ready(41),
            Completion::Panic => panic!("runtime lifecycle poll witness"),
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        self.release
            .recv_timeout(WAIT)
            .expect("probe disposal was not released");
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Release(Option<mpsc::Sender<()>>);
impl Release {
    fn open(&mut self) {
        if let Some(release) = self.0.take() {
            let _ = release.send(());
        }
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        self.open();
    }
}
fn probe(completion: Completion) -> (Probe, oneshot::Receiver<()>, Release) {
    let (entered, observed) = oneshot::channel();
    let (release, held) = mpsc::channel();
    (
        Probe {
            completion,
            entered: Some(entered),
            release: held,
            polls: Arc::default(),
            drops: Arc::default(),
        },
        observed,
        Release(Some(release)),
    )
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime lifecycle did not reach the expected state");
}

async fn queued_cleanup(cancel: bool) {
    let runtime = TokioActorRuntime::start(RuntimeConfig {
        scheduler_config: SchedulerConfig {
            task_queue_capacity: 1,
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap();
    let (active_entered, active_observed) = oneshot::channel();
    let (active_release, active_held) = oneshot::channel();
    let mut active_release = Some(active_release);
    let active_runtime = runtime.clone();
    let active = tokio::spawn(async move {
        active_runtime
            .spawn(async move {
                active_entered.send(()).unwrap();
                active_held.await.unwrap();
                7
            })
            .await
    });
    tokio::time::timeout(WAIT, active_observed)
        .await
        .unwrap()
        .unwrap();
    let (input, observed, mut release) = probe(Completion::Pending);
    let (polls, drops) = (input.polls.clone(), input.drops.clone());
    let queued_runtime = runtime.clone();
    let queued = tokio::spawn(async move { queued_runtime.spawn(input).await });
    until(|| runtime.metrics().pending_messages == 1).await;
    let mut shutdown = Box::pin(runtime.clone().shutdown());
    if cancel {
        queued.abort();
    } else {
        assert!(futures::poll!(&mut shutdown).is_pending());
        active_release.take().unwrap().send(()).unwrap();
    }
    tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
    let retained_pending = runtime.metrics().pending_messages;
    if cancel {
        active_release.take().unwrap().send(()).unwrap();
    }
    assert_eq!(
        tokio::time::timeout(WAIT, active)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        7
    );
    let premature_shutdown = futures::poll!(&mut shutdown).is_ready();
    let drops_before_release = drops.load(Ordering::SeqCst);
    release.open();
    let result = tokio::time::timeout(WAIT, queued).await.unwrap();
    if cancel {
        assert!(result.unwrap_err().is_cancelled());
    } else {
        assert!(matches!(result.unwrap(), Err(ActorError::Stopped)));
    }
    if !premature_shutdown {
        tokio::time::timeout(WAIT, shutdown).await.unwrap().unwrap();
    }
    assert_eq!(
        polls.load(Ordering::SeqCst),
        0,
        "a queued input was executed after stop"
    );
    assert_eq!(drops_before_release, 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.metrics().pending_messages, 0);
    assert_eq!(runtime.metrics().active_actors, 0);
    assert_eq!(
        (retained_pending, premature_shutdown),
        (1, false),
        "accepted input disposal must retain admission and block shutdown"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_cancellation_retains_admission_until_future_is_disposed() {
    queued_cleanup(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_shutdown_rejection_retains_admission_until_future_is_disposed() {
    queued_cleanup(false).await;
}

async fn active_cleanup(completion: Completion, cancel: bool, timeout: bool, io: bool) {
    let runtime = TokioActorRuntime::start(RuntimeConfig {
        io_threads: io.then_some(1),
        scheduler_config: SchedulerConfig {
            task_timeout: if timeout {
                Duration::from_millis(30)
            } else {
                Duration::ZERO
            },
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap();
    let (input, observed, mut release) = probe(completion);
    let (polls, drops) = (input.polls.clone(), input.drops.clone());
    let task_runtime = runtime.clone();
    let task = tokio::spawn(async move {
        if io {
            task_runtime.spawn_io(input).await
        } else {
            task_runtime.spawn(input).await
        }
    });
    if cancel {
        until(|| polls.load(Ordering::SeqCst) > 0).await;
        task.abort();
    }
    tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
    let retained_active = runtime.metrics().active_actors;
    let mut shutdown = Box::pin(runtime.clone().shutdown());
    let premature_shutdown = futures::poll!(&mut shutdown).is_ready();
    let drops_before_release = drops.load(Ordering::SeqCst);
    release.open();
    let result = tokio::time::timeout(WAIT, task).await.unwrap();
    if cancel {
        assert!(result.unwrap_err().is_cancelled());
    } else if timeout {
        assert!(matches!(result.unwrap(), Err(ActorError::Timeout)));
    } else if matches!(completion, Completion::Panic) {
        assert!(matches!(result.unwrap(), Err(ActorError::Panic(reason))
            if reason == "runtime lifecycle poll witness"));
    } else {
        assert_eq!(result.unwrap().unwrap(), 41);
    }
    if !premature_shutdown {
        tokio::time::timeout(WAIT, shutdown).await.unwrap().unwrap();
    }
    assert!(polls.load(Ordering::SeqCst) > 0);
    assert_eq!(drops_before_release, 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.metrics().active_actors, 0);
    assert_eq!(
        retained_active, 1,
        "running cleanup was reported as retired"
    );
    assert!(
        !premature_shutdown,
        "shutdown returned while task disposal was blocked"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_cancellation_retains_admission_until_future_is_disposed() {
    active_cleanup(Completion::Pending, true, false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_timeout_retains_admission_until_future_is_disposed() {
    active_cleanup(Completion::Pending, false, true, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_future_is_disposed_before_admission_is_released() {
    active_cleanup(Completion::Ready, false, false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panicking_future_is_disposed_before_admission_is_released() {
    active_cleanup(Completion::Panic, false, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn io_cancellation_waits_for_actual_input_disposal() {
    active_cleanup(Completion::Pending, true, false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn io_timeout_waits_for_actual_input_disposal() {
    active_cleanup(Completion::Pending, false, true, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn io_ready_future_is_disposed_before_admission_is_released() {
    active_cleanup(Completion::Ready, false, false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn io_panicking_future_is_disposed_before_admission_is_released() {
    active_cleanup(Completion::Panic, false, false, true).await;
}

#[tokio::test]
async fn excessive_queue_capacity_returns_initialization_error() {
    let result = std::panic::AssertUnwindSafe(TokioActorRuntime::start(RuntimeConfig {
        scheduler_config: SchedulerConfig {
            task_queue_capacity: usize::MAX,
            ..Default::default()
        },
        ..Default::default()
    }))
    .catch_unwind()
    .await;
    assert!(
        matches!(result, Ok(Err(ActorError::InitializationError(_)))),
        "invalid capacity must return a configuration error instead of panicking: {result:?}"
    );
}
#[tokio::test]
async fn overflowing_task_timeout_returns_initialization_error() {
    let result = TokioActorRuntime::start(RuntimeConfig {
        scheduler_config: SchedulerConfig {
            task_timeout: Duration::MAX,
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    assert!(
        matches!(result, Err(ActorError::InitializationError(_))),
        "an unrepresentable task deadline was accepted: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capacity_is_reused_only_after_actual_future_disposal() {
    let runtime = TokioActorRuntime::start(RuntimeConfig {
        scheduler_config: SchedulerConfig {
            task_queue_capacity: 1,
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap();
    let (first, observed, mut release) = probe(Completion::Ready);
    let first_runtime = runtime.clone();
    let first_task = tokio::spawn(async move { first_runtime.spawn(first).await });
    tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
    let executed = Arc::new(AtomicUsize::new(0));
    let next_executed = executed.clone();
    let next_runtime = runtime.clone();
    let second = tokio::spawn(async move {
        next_runtime
            .spawn(async move {
                next_executed.fetch_add(1, Ordering::SeqCst);
                42
            })
            .await
    });
    until(|| runtime.metrics().pending_messages == 1).await;
    let executed_during_disposal = executed.load(Ordering::SeqCst);
    let active_during_disposal = runtime.metrics().active_actors;
    release.open();
    assert_eq!(
        tokio::time::timeout(WAIT, first_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        41
    );
    assert_eq!(
        tokio::time::timeout(WAIT, second)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        42
    );
    assert_eq!(executed_during_disposal, 0);
    assert_eq!(active_during_disposal, 1);
    assert_eq!(executed.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.metrics().pending_messages, 0);
    assert_eq!(runtime.metrics().active_actors, 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_shutdown_waiter_keeps_admission_closed_and_cleanup_joinable() {
    let runtime = TokioActorRuntime::start(RuntimeConfig::default())
        .await
        .unwrap();
    let mut work = Box::pin(runtime.spawn(futures::future::pending::<()>()));
    assert!(futures::poll!(&mut work).is_pending());
    let mut first_shutdown = Box::pin(runtime.clone().shutdown());
    assert!(futures::poll!(&mut first_shutdown).is_pending());
    drop(first_shutdown);
    let polled = Arc::new(AtomicUsize::new(0));
    let rejected_polled = polled.clone();
    let rejected = runtime
        .spawn(async move {
            rejected_polled.fetch_add(1, Ordering::SeqCst);
        })
        .await;
    assert!(matches!(rejected, Err(ActorError::Stopped)));
    assert_eq!(polled.load(Ordering::SeqCst), 0);
    let mut second_shutdown = Box::pin(runtime.clone().shutdown());
    let mut third_shutdown = Box::pin(runtime.clone().shutdown());
    assert!(futures::poll!(&mut second_shutdown).is_pending());
    assert!(futures::poll!(&mut third_shutdown).is_pending());
    assert_eq!(runtime.metrics().active_actors, 1);
    drop(work);
    tokio::time::timeout(WAIT, async {
        second_shutdown.await.unwrap();
        third_shutdown.await.unwrap();
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn maximum_representable_capacity_accepts_and_executes_work() {
    let runtime = TokioActorRuntime::start(RuntimeConfig {
        scheduler_config: SchedulerConfig {
            task_queue_capacity: Semaphore::MAX_PERMITS,
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(runtime.spawn(async { 43 }).await.unwrap(), 43);
    assert_eq!(runtime.metrics().active_actors, 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_activation_and_shutdown_never_publish_work_after_retirement() {
    for _ in 0..32 {
        let runtime = TokioActorRuntime::start(RuntimeConfig::default())
            .await
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(17));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = Arc::new(AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let (runtime, barrier, closed, completed) = (
                runtime.clone(),
                barrier.clone(),
                closed.clone(),
                completed.clone(),
            );
            tasks.spawn(async move {
                barrier.wait().await;
                runtime
                    .spawn(async move {
                        assert!(!closed.load(Ordering::SeqCst));
                        tokio::task::yield_now().await;
                        assert!(!closed.load(Ordering::SeqCst));
                        completed.fetch_add(1, Ordering::SeqCst);
                    })
                    .await
            });
        }
        barrier.wait().await;
        tokio::time::timeout(WAIT, runtime.clone().shutdown())
            .await
            .unwrap()
            .unwrap();
        closed.store(true, Ordering::SeqCst);
        let mut successful = 0;
        tokio::time::timeout(WAIT, async {
            while let Some(result) = tasks.join_next().await {
                match result.unwrap() {
                    Ok(()) => successful += 1,
                    Err(ActorError::Stopped) => {}
                    result => panic!("unexpected admission result: {result:?}"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), successful);
        assert_eq!(runtime.metrics().active_actors, 0);
        assert_eq!(runtime.metrics().pending_messages, 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn panic_payload_is_disposed_before_active_admission_is_released() {
    struct Payload {
        _input: Probe,
    }
    let runtime = TokioActorRuntime::start(RuntimeConfig::default())
        .await
        .unwrap();
    let (input, observed, mut release) = probe(Completion::Pending);
    let (polls, drops) = (input.polls.clone(), input.drops.clone());
    let task_runtime = runtime.clone();
    let task = tokio::spawn(async move {
        task_runtime
            .spawn(async move {
                std::panic::panic_any(Payload { _input: input });
            })
            .await
    });
    tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
    let active_during_disposal = runtime.metrics().active_actors;
    let mut shutdown = Box::pin(runtime.clone().shutdown());
    let premature_shutdown = futures::poll!(&mut shutdown).is_ready();
    release.open();
    assert!(
        matches!(tokio::time::timeout(WAIT, task).await.unwrap().unwrap(),
        Err(ActorError::Panic(reason)) if reason == "non-string panic payload")
    );
    if !premature_shutdown {
        tokio::time::timeout(WAIT, shutdown).await.unwrap().unwrap();
    }
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        (active_during_disposal, premature_shutdown),
        (1, false),
        "caught panic payload disposal must remain part of the accepted task"
    );
}

#[tokio::test]
async fn panic_payload_destructor_failure_returns_the_original_panic_receipt() {
    struct BadPayload;
    impl Drop for BadPayload {
        fn drop(&mut self) {
            panic!("panic payload disposal witness");
        }
    }
    let runtime = TokioActorRuntime::start(RuntimeConfig::default())
        .await
        .unwrap();
    let result = std::panic::AssertUnwindSafe(runtime.spawn(async {
        std::panic::panic_any(BadPayload);
    }))
    .catch_unwind()
    .await;
    runtime.clone().shutdown().await.unwrap();
    assert!(
        matches!(result, Ok(Err(ActorError::Panic(reason)))
        if reason == "non-string panic payload; panic payload destructor panicked"),
        "caught task panic escaped while disposing its panic payload"
    );
    assert_eq!(runtime.metrics().active_actors, 0);
}

#[tokio::test]
async fn runtime_panic_diagnostics_are_bounded_at_utf8_character_boundaries() {
    let runtime = TokioActorRuntime::start(RuntimeConfig::default())
        .await
        .unwrap();
    let result = runtime
        .spawn(async {
            std::panic::panic_any("\u{754c}".repeat(2048));
        })
        .await;
    runtime.clone().shutdown().await.unwrap();
    match result {
        Err(ActorError::Panic(reason)) => {
            assert_eq!(reason, format!("{}\u{2026}", "\u{754c}".repeat(1024)));
        }
        result => panic!("unexpected runtime panic receipt: {result:?}"),
    }
    assert_eq!(runtime.metrics().active_actors, 0);
}
