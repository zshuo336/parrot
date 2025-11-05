//! OneForAll Restart reconstructs both children through stable references.
//! The intentional child panic is caught by the runtime; the example exits successfully.
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorSpawnerExt},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Crash;
struct ThreadFamily {
    generation: usize,
    root: bool,
    drops: Arc<AtomicUsize>,
}
struct ActixFamily {
    generation: usize,
    root: bool,
    drops: Arc<AtomicUsize>,
}
impl ActixEngineExt for ActixFamily {}
macro_rules! family {
    ($actor:ident, $context:ty) => {
        impl Actor for $actor {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn init<'a>(
                &'a mut self,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    if self.root {
                        ctx.set_supervisor_strategy(
                            parrot_api::supervisor::SupervisorStrategyType::OneForAll(
                                parrot_api::supervisor::OneForAllStrategy {
                                    max_restarts: 2,
                                    within: std::time::Duration::from_secs(30),
                                    decider: parrot_api::supervisor::BasicDecisionFn::new(|_| {
                                        parrot_api::supervisor::SupervisionDecision::Restart
                                    }),
                                },
                            ),
                        );
                        for _ in 0..2 {
                            let generation = Arc::new(AtomicUsize::new(0));
                            let drops = self.drops.clone();
                            ctx.spawner()
                                .spawn_factory(move || Self {
                                    root: false,
                                    generation: generation.fetch_add(1, Ordering::SeqCst),
                                    drops: drops.clone(),
                                })
                                .await?;
                        }
                    } else {
                        assert!(ctx.parent().is_some());
                    }
                    Ok(())
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    assert!(!message.is::<Crash>(), "intentional child failure");
                    if !self.root {
                        return Ok(Box::new(self.generation) as BoxedMessage);
                    }
                    let children: Vec<BoxedActorRef> = ctx
                        .children()
                        .unwrap()
                        .read_all()
                        .iter()
                        .map(|child| child.clone_boxed())
                        .collect();
                    Ok(Box::new(children) as BoxedMessage)
                })
            }
        }
        impl Drop for $actor {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }
    };
}
family!(ThreadFamily, parrot::thread::ThreadContext<Self>);
family!(
    ActixFamily,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
async fn verify(parent: BoxedActorRef, drops: Arc<AtomicUsize>, engine: &str) {
    let children = *parent
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<Vec<BoxedActorRef>>()
        .unwrap();
    assert_eq!(children.len(), 2);
    for expected in 0..=2 {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for child in &children {
                loop {
                    if let Ok(value) = child
                        .send_with_timeout(Box::new(()), Some(std::time::Duration::from_millis(50)))
                        .await
                        && *value.downcast::<usize>().unwrap() == expected
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 2 * expected);
        if expected < 2 {
            assert!(children[expected].send(Box::new(Crash)).await.is_err());
        }
    }
    parent.stop().await.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 7);
    println!(
        "{engine}: both original references reached generation 2; all seven instances retired"
    );
}
fn main() {
    actix::System::new().block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
        let parent = thread
            .spawn_at(
                ThreadFamily {
                    root: true,
                    generation: 0,
                    drops: drops.clone(),
                },
                "/family",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        verify(Box::new(parent), drops, "Thread").await;
        thread.shutdown_internal().await.unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let actix = parrot::actix::ActixActorSystem::with_arbiter_count(2)
            .await
            .unwrap();
        let parent = actix
            .spawn_root_typed(
                ActixFamily {
                    root: true,
                    generation: 0,
                    drops: drops.clone(),
                },
                EmptyConfig,
            )
            .await
            .unwrap();
        verify(parent, drops, "Actix").await;
        actix.shutdown().await.unwrap();
    });
}
