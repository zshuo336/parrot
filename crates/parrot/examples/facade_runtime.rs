//! Open facade registration, actual engine replies, factories and shared shutdown.
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, InstanceConfig, RuntimeActor, SpawnOptions},
    system::{SystemError, SystemState, SystemStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{any::Any, sync::Arc};

struct Echo;
impl Actor for Echo {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            Ok(message)
        })
    }
}

// An application-defined runtime can delegate to an existing backend. It need
// not expose capabilities (such as factories) it does not wish to support.
struct Extension(Arc<parrot::thread::ThreadActorSystem>);
impl AsyncEngineRuntime for Extension {
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        AsyncEngineRuntime::spawn(self.0.as_ref(), actor, config, options)
    }
    fn get_actor<'a>(&'a self, path: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        AsyncEngineRuntime::get_actor(self.0.as_ref(), path)
    }
    fn status(&self) -> SystemStatus {
        AsyncEngineRuntime::status(self.0.as_ref())
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        AsyncEngineRuntime::shutdown(self.0.as_ref())
    }
}

#[actix::main]
async fn main() -> anyhow::Result<()> {
    let facade = ParrotActorSystem::new(Default::default()).await?;
    facade.register_runtime(
        "thread".into(),
        parrot::thread::ThreadActorSystem::shared(Default::default()),
        true,
    )?;
    facade.register_runtime(
        "actix".into(),
        Arc::new(parrot::actix::ActixActorSystem::with_arbiter_count(1).await?),
        false,
    )?;
    facade.register_runtime(
        "extension".into(),
        Arc::new(Extension(parrot::thread::ThreadActorSystem::shared(
            Default::default(),
        ))),
        false,
    )?;
    let work = async {
        for name in ["thread", "actix", "extension"] {
            let path = format!("/user/{name}");
            let actor = facade
                .spawn_on(name, Box::new(Echo), Box::new(()), SpawnOptions::at(&path))
                .await?;
            let result = actor.send(Box::new(42_u64)).await?;
            anyhow::ensure!(result.downcast_ref::<u64>() == Some(&42), "wrong reply");
            let found = facade
                .get_actor(&path)
                .await
                .ok_or_else(|| anyhow::anyhow!("missing actor"))?;
            anyhow::ensure!(
                found.death_watch().unwrap().id() == actor.death_watch().unwrap().id(),
                "wrong registry identity"
            );
            println!("{name}: facade reply42 and real lookup verified");
        }
        let parent = facade
            .factory_runtime()
            .unwrap()
            .spawn_factory(Box::new(|| Box::new(Echo)), Default::default())
            .await?;
        anyhow::ensure!(
            parent.send(Box::new(7_u64)).await?.downcast_ref::<u64>() == Some(&7),
            "wrong factory reply"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    // Cleanup also runs if application work fails. Concurrent waiters share it.
    let (first, second) = tokio::join!(facade.shutdown_runtime(), facade.shutdown_runtime());
    work?;
    first?;
    second?;
    anyhow::ensure!(
        facade.status().state == SystemState::Stopped,
        "resources still running"
    );
    println!("facade: factory and shared shutdown verified");
    Ok(())
}
