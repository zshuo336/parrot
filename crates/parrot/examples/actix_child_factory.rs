//! A managed Actix parent owns one logical child through multiple generations.
//! The intentional child panics are contained by supervision.
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorSpawnerExt},
    supervisor::{DefaultSupervisorStrategyFactory, SupervisorStrategyType},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Crash;
struct Child {
    generation: usize,
    drops: Arc<AtomicUsize>,
    parent_path: String,
}
impl Drop for Child {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActixEngineExt for Child {}
impl Actor for Child {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            assert!(!msg.is::<Crash>(), "intentional child failure");
            assert_eq!(ctx.parent().unwrap().path(), self.parent_path);
            Ok(Box::new(self.generation) as BoxedMessage)
        })
    }
}
struct Parent {
    made: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}
impl ActixEngineExt for Parent {}
impl Actor for Parent {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let made = self.made.clone();
            let dropped = self.dropped.clone();
            let parent_path = ctx.get_self_ref().path();
            let child = ctx
                .spawner()
                .spawn_factory_with_strategy(
                    move || Child {
                        generation: made.fetch_add(1, Ordering::SeqCst),
                        drops: dropped.clone(),
                        parent_path: parent_path.clone(),
                    },
                    SupervisorStrategyType::OneForOne(
                        DefaultSupervisorStrategyFactory::one_for_one(2, Duration::from_secs(30)),
                    ),
                )
                .await?;
            Ok(Box::new(child) as BoxedMessage)
        })
    }
}
#[actix::main]
async fn main() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let made = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let parent = system
        .spawn_root_typed(
            Parent {
                made: made.clone(),
                dropped: dropped.clone(),
            },
            EmptyConfig,
        )
        .await
        .unwrap();
    let child = *parent
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    for expected in 0..3 {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(reply) = child
                    .send_with_timeout(Box::new(()), Some(Duration::from_millis(100)))
                    .await
                    && *reply.downcast::<usize>().unwrap() == expected
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if expected < 2 {
            let _ = child.send(Box::new(Crash)).await;
        }
    }
    parent.stop().await.unwrap();
    assert_eq!(made.load(Ordering::SeqCst), 3);
    assert_eq!(dropped.load(Ordering::SeqCst), 3);
    assert_eq!(system.status().active_actors, 0);
    system.shutdown().await.unwrap();
    println!("Actix logical child followed three generations; parent stop retired all instances");
}
