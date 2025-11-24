#![cfg(test)]

use parrot_api::{
    errors::ActorError,
    runtime::{ActorRuntime, RuntimeConfig, SchedulerConfig, TokioActorRuntime},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

fn observe(runtime: &TokioActorRuntime, observed: &Mutex<Option<String>>) {
    let mut shutdown = Box::pin(runtime.clone().shutdown());
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    let result = match shutdown.as_mut().poll(&mut cx) {
        Poll::Ready(Err(ActorError::InternalError(reason)))
            if reason.contains("outside its submitted tasks") =>
        {
            "rejected".to_owned()
        }
        other => format!("{other:?}"),
    };
    *observed.lock().unwrap() = Some(result);
}

struct Input {
    runtime: TokioActorRuntime,
    ready: bool,
    observed: Arc<Mutex<Option<String>>>,
}

impl Future for Input {
    type Output = usize;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<usize> {
        if self.ready {
            Poll::Ready(41)
        } else {
            Poll::Pending
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        observe(&self.runtime, &self.observed);
    }
}

async fn assert_rejected(runtime: TokioActorRuntime, observed: Arc<Mutex<Option<String>>>) {
    let observation = observed.lock().unwrap().clone();
    let retry = runtime.spawn(async { 42 }).await;
    runtime.shutdown().await.unwrap();
    assert_eq!(
        observation.as_deref(),
        Some("rejected"),
        "cleanup waited for itself"
    );
    assert_eq!(retry.unwrap(), 42, "cleanup reentry closed admission");
}

#[tokio::test]
async fn cancelled_active_input_drop_rejects_its_own_shutdown() {
    let runtime = TokioActorRuntime::start(RuntimeConfig::default())
        .await
        .unwrap();
    let observed = Arc::new(Mutex::new(None));
    let mut submission = Box::pin(runtime.spawn(Input {
        runtime: runtime.clone(),
        ready: false,
        observed: observed.clone(),
    }));
    assert!(futures::poll!(&mut submission).is_pending());
    assert_eq!(runtime.metrics().active_actors, 1);
    drop(submission);
    assert_rejected(runtime, observed).await;
}

#[tokio::test]
async fn timed_out_input_drop_rejects_its_own_shutdown() {
    let runtime = TokioActorRuntime::start(RuntimeConfig {
        scheduler_config: SchedulerConfig {
            task_timeout: Duration::from_millis(1),
            ..Default::default()
        },
        ..Default::default()
    })
    .await
    .unwrap();
    let observed = Arc::new(Mutex::new(None));
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.spawn(Input {
            runtime: runtime.clone(),
            ready: false,
            observed: observed.clone(),
        }),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(ActorError::Timeout)));
    assert_rejected(runtime, observed).await;
}

struct PanicInput {
    _input: Input,
}
impl Future for PanicInput {
    type Output = usize;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<usize> {
        panic!("cleanup scope poll panic witness");
    }
}

#[tokio::test]
async fn panicking_input_drop_rejects_its_own_shutdown() {
    let runtime = TokioActorRuntime::start(RuntimeConfig::default())
        .await
        .unwrap();
    let observed = Arc::new(Mutex::new(None));
    let result = runtime
        .spawn(PanicInput {
            _input: Input {
                runtime: runtime.clone(),
                ready: false,
                observed: observed.clone(),
            },
        })
        .await;
    assert!(
        matches!(result, Err(ActorError::Panic(reason)) if reason == "cleanup scope poll panic witness")
    );
    assert_rejected(runtime, observed).await;
}

#[tokio::test]
async fn panic_payload_drop_rejects_its_own_shutdown() {
    struct Payload {
        runtime: TokioActorRuntime,
        observed: Arc<Mutex<Option<String>>>,
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            observe(&self.runtime, &self.observed);
        }
    }
    let runtime = TokioActorRuntime::start(RuntimeConfig::default())
        .await
        .unwrap();
    let observed = Arc::new(Mutex::new(None));
    let payload = Payload {
        runtime: runtime.clone(),
        observed: observed.clone(),
    };
    let result = runtime
        .spawn(async move { std::panic::panic_any(payload) })
        .await;
    assert!(
        matches!(result, Err(ActorError::Panic(reason)) if reason == "non-string panic payload")
    );
    assert_rejected(runtime, observed).await;
}

#[test]
fn completed_input_drop_rejects_its_own_shutdown_without_closing_admission() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let runtime = TokioActorRuntime::start(RuntimeConfig::default())
                .await
                .unwrap();
            let observed = Arc::new(Mutex::new(None));
            let value = runtime
                .spawn(Input {
                    runtime: runtime.clone(),
                    ready: true,
                    observed: observed.clone(),
                })
                .await
                .unwrap();
            assert_eq!(value, 41);
            let observation = observed.lock().unwrap().clone();
            let retry = runtime.spawn(async { 42 }).await;
            runtime.shutdown().await.unwrap();
            assert_eq!(
                observation.as_deref(),
                Some("rejected"),
                "input cleanup waited for itself"
            );
            assert_eq!(retry.unwrap(), 42, "cleanup reentry closed admission");
        });
}

#[test]
fn queued_input_drop_rejects_its_own_shutdown_without_closing_admission() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let runtime = TokioActorRuntime::start(RuntimeConfig {
                scheduler_config: SchedulerConfig {
                    task_queue_capacity: 1,
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
            let mut first = Box::pin(runtime.spawn(std::future::pending::<()>()));
            assert!(futures::poll!(&mut first).is_pending());
            let observed = Arc::new(Mutex::new(None));
            let mut queued = Box::pin(runtime.spawn(Input {
                runtime: runtime.clone(),
                ready: false,
                observed: observed.clone(),
            }));
            assert!(futures::poll!(&mut queued).is_pending());
            assert_eq!(runtime.metrics().pending_messages, 1);
            drop(queued);
            let observation = observed.lock().unwrap().clone();
            drop(first);
            let retry = runtime.spawn(async { 42 }).await;
            runtime.shutdown().await.unwrap();
            assert_eq!(
                observation.as_deref(),
                Some("rejected"),
                "queued cleanup waited for itself"
            );
            assert_eq!(retry.unwrap(), 42, "cleanup reentry closed admission");
        });
}
