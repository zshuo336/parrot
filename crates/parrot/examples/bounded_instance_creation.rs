//! Return original inputs on overload, then retry after cancelled work retires.
use parrot::thread::{
    InstanceAdmissionError, InstanceAdmissionFailure, ThreadActorSystem, ThreadActorSystemConfig,
    ThreadContext,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::time::Duration;

struct Echo;
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let system = ThreadActorSystem::try_shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    })?;
    let mut staged = Vec::new();
    for _ in 0..ThreadActorSystem::MAX_PENDING_INSTANCE_CREATIONS {
        staged.push(system.try_spawn_typed_with_config(Echo, EmptyConfig, Default::default())?);
    }
    let mut rejected = system
        .try_spawn_typed_with_config(Echo, EmptyConfig, Default::default())
        .err()
        .expect("staged inputs must exhaust creation admission");
    assert_eq!(rejected.reason, InstanceAdmissionFailure::Full);
    drop(staged);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let reference = loop {
        let InstanceAdmissionError {
            actor,
            config,
            engine,
            ..
        } = rejected;
        match system.try_spawn_typed_with_config(actor, config, engine) {
            Ok(creation) => break creation.await?,
            Err(error) if error.reason == InstanceAdmissionFailure::Full => {
                rejected = error;
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "cleanup did not restore admission"
                );
                tokio::task::yield_now().await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let reply = reference.ask(Box::new(73u64)).await?;
    assert_eq!(
        *reply.downcast::<u64>().expect("echo returned another type"),
        73
    );
    system.shutdown_internal().await?;
    println!("Rejected inputs retained their ownership; retry returned the actual echo reply.");
    Ok(())
}
