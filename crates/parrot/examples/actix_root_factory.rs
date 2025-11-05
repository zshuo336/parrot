//! A root factory keeps one logical reference through multiple generations.
//! The intentional actor panics are contained by supervision.
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::ActorContext,
    supervisor::DefaultSupervisorStrategyFactory,
    types::{ActorResult, BoxedFuture, BoxedMessage},
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
            assert!(ctx.parent().is_none());
            Ok(Box::new(self.generation) as BoxedMessage)
        })
    }
}
#[actix::main]
async fn main() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let made = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let construct_count = made.clone();
    let drop_count = dropped.clone();
    let child = system
        .spawn_root_factory_with_strategy(
            move || Child {
                generation: construct_count.fetch_add(1, Ordering::SeqCst),
                drops: drop_count.clone(),
            },
            DefaultSupervisorStrategyFactory::one_for_one(2, Duration::from_secs(30)),
        )
        .await
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
    child.stop().await.unwrap();
    assert_eq!(made.load(Ordering::SeqCst), 3);
    assert_eq!(dropped.load(Ordering::SeqCst), 3);
    assert_eq!(system.status().active_actors, 0);
    system.shutdown().await.unwrap();
    println!("Actix root reference followed three generations; stop retired all instances");
}
