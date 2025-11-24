use super::*;
use parrot_api::{
    errors::ActorError,
    types::{ActorResult, BoxedMessage},
};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

struct RecursivePayload(Arc<AtomicUsize>);
impl Drop for RecursivePayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("I/O panic payload disposal");
    }
}
struct Fault {
    poll_panic: bool,
    business_error: bool,
    drop_panic: bool,
    drops: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl Future for Fault {
    type Output = ActorResult<BoxedMessage>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        if self.poll_panic {
            std::panic::panic_any(RecursivePayload(self.payloads.clone()));
        }
        Poll::Ready(if self.business_error {
            Err(ActorError::MessageHandlingError(
                "I/O business failure".into(),
            ))
        } else {
            Ok(Box::new(42u8) as BoxedMessage)
        })
    }
}
impl Drop for Fault {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.drop_panic {
            std::panic::panic_any(RecursivePayload(self.payloads.clone()));
        }
    }
}
async fn faults(actix: bool) {
    for (poll_panic, business_error, drop_panic) in [
        (false, true, false),
        (false, false, true),
        (false, true, true),
        (true, false, true),
    ] {
        let system = if actix {
            ParrotActorSystem::with_actix(config(), Default::default())
                .await
                .unwrap()
        } else {
            ParrotActorSystem::start(config()).await.unwrap()
        };
        let drops = Arc::new(AtomicUsize::new(0));
        let payloads = Arc::new(AtomicUsize::new(0));
        let result = bounded(system.io_runtime().unwrap().run_io(Box::pin(Fault {
            poll_panic,
            business_error,
            drop_panic,
            drops: drops.clone(),
            payloads: payloads.clone(),
        })))
        .await;
        if business_error {
            assert!(
                matches!(result, Err(ActorError::MessageHandlingError(message)) if message == "I/O business failure")
            );
        } else if poll_panic {
            assert!(matches!(result, Err(ActorError::Panic(_))));
        } else {
            assert!(
                matches!(result, Err(ActorError::InternalError(message)) if message.contains("input disposal failed"))
            );
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            payloads.load(Ordering::SeqCst),
            usize::from(poll_panic) + usize::from(drop_panic)
        );
        bounded(system.io_runtime().unwrap().run_io_typed(tcp()))
            .await
            .unwrap();
        let closed = bounded(system.shutdown_runtime()).await;
        assert_eq!(
            closed.is_err(),
            poll_panic || drop_panic,
            "ordinary business errors must not poison system shutdown; cleanup panics must remain visible"
        );
        if drop_panic && !poll_panic {
            assert!(
                closed
                    .unwrap_err()
                    .to_string()
                    .contains("input disposal failed")
            );
        }
    }
}
#[tokio::test]
async fn thread_io_preserves_primary_error_and_recursive_cleanup_failure() {
    faults(false).await;
}

#[actix::test]
async fn rejected_io_returns_actual_input_disposal_failure() {
    for actix in [false, true] {
        let system = if actix {
            ParrotActorSystem::with_actix(config(), Default::default())
                .await
                .unwrap()
        } else {
            ParrotActorSystem::start(config()).await.unwrap()
        };
        let runtime = system.runtime("default").unwrap();
        bounded(system.shutdown_runtime()).await.unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let payloads = Arc::new(AtomicUsize::new(0));
        let result = bounded(runtime.io_runtime().unwrap().run_io(Box::pin(Fault {
            poll_panic: true,
            business_error: false,
            drop_panic: true,
            drops: drops.clone(),
            payloads: payloads.clone(),
        })))
        .await;
        assert!(
            matches!(result, Err(ActorError::InternalError(message)) if message.contains("input disposal failed")),
            "rejection must preserve input cleanup failure"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            payloads.load(Ordering::SeqCst),
            1,
            "rejected input must never be polled"
        );
        bounded(runtime.shutdown()).await.unwrap();
    }
}
#[actix::test]
async fn actix_io_preserves_primary_error_and_recursive_cleanup_failure() {
    faults(true).await;
}

#[tokio::test]
async fn independent_io_admission_is_bounded_and_all_accepted_tasks_retire() {
    let system = ParrotActorSystem::start(config()).await.unwrap();
    let io = system.io_runtime().unwrap();
    let mut tasks = Vec::new();
    let mut retired = Vec::new();
    for _ in 0..1024 {
        let (input, entered, cleanup) = pending();
        tasks.push(tokio::spawn(io.run_io(Box::pin(input))));
        bounded(entered).await.unwrap();
        retired.push(cleanup);
    }
    let (input, entered, cleanup) = pending();
    let rejected = bounded(io.run_io(Box::pin(input))).await;
    assert!(
        matches!(rejected, Err(ActorError::InternalError(message)) if message.contains("limit"))
    );
    assert!(entered.await.is_err());
    bounded(cleanup).await.unwrap();
    bounded(system.shutdown_runtime()).await.unwrap();
    for cleanup in retired {
        bounded(cleanup).await.unwrap();
    }
    for task in tasks {
        assert!(matches!(
            bounded(task).await.unwrap(),
            Err(ActorError::Stopped)
        ));
    }
}

#[actix::test]
async fn actix_io_rejects_native_and_facade_self_shutdown_without_closing_admission() {
    let facade = Arc::new(
        ParrotActorSystem::with_actix(config(), Default::default())
            .await
            .unwrap(),
    );
    let runtime = facade.runtime("default").unwrap();
    let native = runtime.clone();
    assert!(
        bounded(
            facade
                .io_runtime()
                .unwrap()
                .run_io_typed(async move { Ok(native.shutdown().await.is_err()) })
        )
        .await
        .unwrap()
    );
    let captured = facade.clone();
    assert!(
        bounded(
            facade
                .io_runtime()
                .unwrap()
                .run_io_typed(async move { Ok(captured.shutdown_runtime().await.is_err()) })
        )
        .await
        .unwrap()
    );
    bounded(runtime.io_runtime().unwrap().run_io_typed(tcp()))
        .await
        .unwrap();
    bounded(facade.shutdown_runtime()).await.unwrap();
}

#[test]
fn last_native_owner_retires_io_after_the_callers_host_has_exited() {
    for actix in [false, true] {
        let host = actix::System::new();
        let (facade, exits) = host.block_on(async {
            let facade = if actix {
                ParrotActorSystem::with_actix(config(), Default::default())
                    .await
                    .unwrap()
            } else {
                ParrotActorSystem::start(config()).await.unwrap()
            };
            let exits = execution_contract(&facade).await;
            (facade, exits)
        });
        drop(host);
        // Final native ownership is released without an ambient async executor.
        drop(facade);
        let deadline = std::time::Instant::now() + LIMIT;
        while exits.load(Ordering::SeqCst) != 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(exits.load(Ordering::SeqCst), 2);
    }
}
