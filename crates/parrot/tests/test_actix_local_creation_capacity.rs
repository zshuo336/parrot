//! Local construction must share the production system's initial-creation limit.
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext, ActixSystemSettings};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    engine::{AsyncLocalRuntime, FactoryOptions, LocalActor, SpawnOptions},
    local_typed::{LocalBoxedFuture, LocalMsgVariant, LocalTypedDispatch},
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{rc::Rc, sync::Arc, task::Poll, time::Duration};
use tokio::sync::Semaphore;

struct Pending(Arc<Semaphore>);
impl ActixEngineExt for Pending {}
impl Actor for Pending {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.acquire().await.unwrap().forget();
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}

struct Read;
impl parrot_api::Message for Read {
    type Result = ();
}
struct Local(Rc<()>);
impl LocalMsgVariant<Read> for Local {
    fn inject(_: Read) {}
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
impl LocalTypedDispatch for Local {
    type Msg = ();
    type Reply = ();
    fn dispatch<'a>(
        &'a mut self,
        _: (),
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        assert_eq!(Rc::strong_count(&self.0), 1);
        Box::pin(async { Ok(()) })
    }
}
impl LocalActor for Local {
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert_eq!(Rc::strong_count(&self.0), 1);
        Box::pin(async move { Ok(message) })
    }
}

async fn local_capacity(mode: u8) {
    let system = Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 2,
            shutdown_timeout: Duration::from_secs(10),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let gate = Arc::new(Semaphore::new(0));
    let mut roots = Vec::new();
    for _ in 0..1024 {
        roots.push(
            system
                .spawn_root_typed(Pending(gate.clone()), EmptyConfig)
                .await
                .unwrap(),
        );
    }
    let creation = async {
        match mode {
            0 => system
                .spawn_local_typed::<Local, Read, _>(|| Local(Rc::new(())), "/local-capacity")
                .await
                .map(|reference| reference.into_dyn()),
            1 => system
                .spawn_local_typed_factory::<Local, Read, _>(
                    || Local(Rc::new(())),
                    "/local-capacity",
                )
                .await
                .map(|reference| reference.into_dyn()),
            _ => system
                .spawn_local_factory(
                    Box::new(|| Box::new(Local(Rc::new(())))),
                    FactoryOptions {
                        spawn: SpawnOptions::at("/local-capacity"),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| parrot_api::errors::ActorError::InternalError(error.to_string())),
        }
    };
    let mut pending = Box::pin(creation);
    let first = futures::poll!(&mut pending);
    let registered = system.status().active_actors;
    gate.add_permits(1024);
    let local: BoxedActorRef = match first {
        Poll::Ready(result) => result.unwrap(),
        Poll::Pending => pending.await.unwrap(),
    };
    let business = if mode == 2 {
        local.send(Box::new(())).await
    } else {
        local.send(Box::new(Read)).await
    };
    local.stop().await.unwrap();
    for actor in roots {
        actor.stop().await.unwrap();
    }
    system.as_ref().clone().shutdown().await.unwrap();
    business.unwrap();
    assert_eq!(
        registered, 1024,
        "Local root bypassed shared creation capacity"
    );
}

#[actix::test]
async fn one_shot_local_root_shares_dynamic_initialization_capacity() {
    local_capacity(0).await;
}
#[actix::test]
async fn repeatable_local_root_shares_dynamic_initialization_capacity() {
    local_capacity(1).await;
}
#[actix::test]
async fn neutral_local_root_shares_dynamic_initialization_capacity() {
    local_capacity(2).await;
}
