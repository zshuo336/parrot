//! OneForAll Stop retires a failed child and its healthy sibling on both engines.
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
    root: bool,
    drops: Arc<AtomicUsize>,
}
struct ActixFamily {
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
                                    max_restarts: 3,
                                    within: std::time::Duration::from_secs(30),
                                    decider: parrot_api::supervisor::BasicDecisionFn::new(|_| {
                                        parrot_api::supervisor::SupervisionDecision::Stop
                                    }),
                                },
                            ),
                        );
                        for _ in 0..2 {
                            ctx.spawner()
                                .spawn_typed(
                                    Self {
                                        root: false,
                                        drops: self.drops.clone(),
                                    },
                                    EmptyConfig,
                                )
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
    for child in &children {
        child.send(Box::new(())).await.unwrap();
    }
    let _ = children[0].send(Box::new(Crash)).await;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let children = *parent
                .send(Box::new(()))
                .await
                .unwrap()
                .downcast::<Vec<BoxedActorRef>>()
                .unwrap();
            if children.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    parent.stop().await.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    println!("{engine}: OneForAll Stop retired both children; parent remained responsive");
}
fn main() {
    actix::System::new().block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
        let parent = thread
            .spawn_at(
                ThreadFamily {
                    root: true,
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
