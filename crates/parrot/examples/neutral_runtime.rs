//! Run the same asynchronous application on any registered production engine.
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineCatalog, FactoryOptions, LocalActor, SpawnOptions},
    local_typed::LocalBoxedFuture,
    typed_context::TypedContext,
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
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            if message.is::<CreateLocal>() {
                let child = ctx
                    .local_spawner()
                    .ok_or_else(|| {
                        parrot_api::errors::ActorError::InitializationError(
                            "Local child capability unavailable".into(),
                        )
                    })?
                    .spawn_local_factory(
                        Box::new(|| Box::new(LocalEcho(std::rc::Rc::new(42)))),
                        None,
                    )
                    .await?;
                return Ok(Box::new(child) as BoxedMessage);
            }
            if let Some(CreateChild(factory)) = message.downcast_ref::<CreateChild>() {
                let spawner = ctx.async_spawner().ok_or_else(|| {
                    parrot_api::errors::ActorError::InitializationError(
                        "child capability unavailable".into(),
                    )
                })?;
                let child = if *factory {
                    spawner
                        .spawn_factory(Box::new(|| Box::new(Echo)), None)
                        .await?
                } else {
                    spawner.spawn(Box::new(Echo), Box::new(()), None).await?
                };
                return Ok(Box::new(child) as BoxedMessage);
            }
            Ok(message)
        })
    }
}
async fn application(catalog: &AsyncEngineCatalog) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    for name in catalog.list() {
        let runtime = catalog.lookup(&name).expect("owned registration");
        let work = async {
            let reference = runtime
                .spawn(Box::new(Echo), Box::new(()), SpawnOptions::at("/user/echo"))
                .await?;
            let answer = reference.send(Box::new(42_u64)).await?;
            anyhow::ensure!(answer.downcast_ref::<u64>() == Some(&42), "incorrect reply");
            anyhow::ensure!(
                runtime.get_actor("/user/echo").await.is_some(),
                "missing registry identity"
            );
            reference.stop().await?;
            let factories = runtime
                .factory_runtime()
                .ok_or_else(|| anyhow::anyhow!("factory capability unavailable"))?;
            let parent = factories
                .spawn_factory(
                    Box::new(|| Box::new(Echo)),
                    FactoryOptions {
                        spawn: SpawnOptions::at("/user/family"),
                        ..Default::default()
                    },
                )
                .await?;
            let mut children = Vec::new();
            for factory in [false, true] {
                let child = *parent
                    .send(Box::new(CreateChild(factory)))
                    .await?
                    .downcast::<BoxedActorRef>()
                    .map_err(|_| anyhow::anyhow!("incorrect child reply"))?;
                anyhow::ensure!(
                    child.path().starts_with("/user/family/"),
                    "incorrect child path"
                );
                let answer = child.send(Box::new(42_u64)).await?;
                anyhow::ensure!(
                    answer.downcast_ref::<u64>() == Some(&42),
                    "incorrect child reply"
                );
                children.push(child);
            }
            // Capability lookup is engine-neutral. Thread explicitly has no
            // owner-thread Local executor; application state is built only when
            // the selected runtime exposes that capability.
            if let Some(local) = runtime.local_runtime() {
                let actor = local
                    .spawn_local_factory(
                        Box::new(|| Box::new(LocalEcho(std::rc::Rc::new(42)))),
                        FactoryOptions {
                            spawn: SpawnOptions::at("/user/local"),
                            ..Default::default()
                        },
                    )
                    .await?;
                let answer = actor.send(Box::new(())).await?;
                anyhow::ensure!(
                    answer.downcast_ref::<u64>() == Some(&42),
                    "incorrect Local reply"
                );
                let found = runtime
                    .get_actor("/user/local")
                    .await
                    .ok_or_else(|| anyhow::anyhow!("Local registry entry missing"))?;
                anyhow::ensure!(
                    found.death_watch().unwrap().id() == actor.death_watch().unwrap().id(),
                    "Local identity mismatch"
                );
                let child = *actor.send(Box::new(CreateLocal)).await?.downcast::<BoxedActorRef>()
                    .map_err(|_| anyhow::anyhow!("Local child reference type mismatch"))?;
                anyhow::ensure!(child.path().starts_with("/user/local/"), "incorrect Local parent path");
                let native_child = *parent.send(Box::new(CreateLocal)).await?.downcast::<BoxedActorRef>()
                    .map_err(|_| anyhow::anyhow!("native parent's Local child reference type mismatch"))?;
                anyhow::ensure!(native_child.path().starts_with("/user/family/"), "incorrect native parent path");
                for reference in [&child, &native_child] {
                    anyhow::ensure!(reference.send(Box::new(())).await?.downcast_ref::<u64>() == Some(&42), "incorrect Local child reply");
                }
                children.push(native_child);
                found.stop().await?;
                anyhow::ensure!(!child.is_alive().await, "Local child outlived its parent");
                println!("{name}: Local Rc across await, native/Local child scopes and actual registry identity verified");
            }
            parent.stop().await?;
            for child in children {
                anyhow::ensure!(!child.is_alive().await, "child outlived parent");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        // Join the selected engine even when application work returns an error.
        let stopped = runtime.shutdown().await;
        if work.is_ok() && stopped.is_ok() {
            println!(
                "{name}: async reply42, named factory, real children and owned shutdown verified"
            );
        }
        if let Err(error) = work {
            failures.push(format!("{name}: {error}"));
        }
        if let Err(error) = stopped {
            failures.push(format!("{name} shutdown: {error}"));
        }
    }
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}
#[actix::main]
async fn main() -> anyhow::Result<()> {
    let catalog = AsyncEngineCatalog::new();
    let actix = catalog.register(
        "actix",
        Arc::new(parrot::actix::ActixActorSystem::with_arbiter_count(1).await?),
    )?;
    let thread = catalog.register(
        "thread",
        parrot::thread::ThreadActorSystem::shared(Default::default()),
    )?;
    let result = application(&catalog).await;
    drop((thread, actix));
    result
}

// Both root factories and child creation use the same engine-neutral Actor types.
struct CreateChild(bool);

// This actor and its futures are deliberately non-Send. No concrete engine type
// occurs in its implementation or in the application capability selection above.
struct LocalEcho(std::rc::Rc<u64>);
impl LocalActor for LocalEcho {
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<CreateLocal>() {
                let child = context
                    .local_spawner()
                    .ok_or_else(|| {
                        parrot_api::errors::ActorError::InitializationError(
                            "Local child capability unavailable".into(),
                        )
                    })?
                    .spawn_local_factory(
                        Box::new(|| Box::new(LocalEcho(std::rc::Rc::new(42)))),
                        None,
                    )
                    .await?;
                return Ok(Box::new(child) as BoxedMessage);
            }
            let state = self.0.clone();
            tokio::task::yield_now().await;
            Ok(Box::new(*state) as BoxedMessage)
        })
    }
}

struct CreateLocal;
