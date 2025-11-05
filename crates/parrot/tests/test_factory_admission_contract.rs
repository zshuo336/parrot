//! Independent probes: factory execution must follow admission and be isolated.
use futures::FutureExt;
use parrot::thread::{SpawnError, SupervisorStrategy, ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Idle;
impl Actor for Idle {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(msg) })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_duplicate_path_never_invokes_the_initial_factory() {
    let sys = ThreadActorSystem::shared(Default::default());
    sys.spawn_at(Idle, "/occupied", None, Default::default())
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let result = sys
        .spawn_supervised(
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
                Idle
            },
            "/occupied",
            SupervisorStrategy::Stop,
        )
        .await;
    sys.shutdown_internal().await.unwrap();
    assert!(matches!(result, Err(SpawnError::ActorPathAlreadyExists(path)) if path == "/occupied"));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "creation must follow successful admission"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_factory_panic_is_a_spawn_error_not_a_caller_panic() {
    let sys = ThreadActorSystem::shared(Default::default());
    let result = std::panic::AssertUnwindSafe(sys.spawn_supervised(
        || -> Idle { panic!("factory panic") },
        "/initial",
        SupervisorStrategy::Stop,
    ))
    .catch_unwind()
    .await;
    let absent = sys.get_actor_ref("/initial").is_none();
    sys.shutdown_internal().await.unwrap();
    assert!(
        matches!(result, Ok(Err(SpawnError::InitializationFailed(message))) if message.contains("factory panic")),
        "factory panic escaped its owned spawn operation"
    );
    assert!(absent);
}

#[tokio::test]
async fn invalid_configuration_and_shutdown_reject_before_calling_the_factory() {
    let sys = ThreadActorSystem::shared(Default::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let result = sys
        .spawn_supervised_with_config(
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
                Idle
            },
            "/invalid",
            parrot::thread::ThreadActorConfig {
                message_batch_size: Some(0),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(SpawnError::InvalidConfig(_))));
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
    let observed = calls.clone();
    assert!(matches!(
        sys.spawn_supervised(
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
                Idle
            },
            "/late",
            SupervisorStrategy::Stop
        )
        .await,
        Err(SpawnError::SystemShutdown)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
