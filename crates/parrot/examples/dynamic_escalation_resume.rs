//! Run with cargo run --locked -p parrot --example dynamic_escalation_resume.
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorSpawnerExt},
    errors::ActorError,
    supervisor::{DefaultStrategy, SupervisorStrategyType},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
macro_rules! node {
    ($name:ident,$context:ty) => {
        struct $name {
            leaf: bool,
            inits: Arc<AtomicUsize>,
            value: usize,
            child: Option<BoxedActorRef>,
        }
        impl $name {
            fn new(inits: Arc<AtomicUsize>) -> Self {
                Self {
                    leaf: false,
                    inits,
                    value: 0,
                    child: None,
                }
            }
        }
        impl ActixEngineExt for $name {}
        impl Actor for $name {
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
                    self.inits.fetch_add(1, Ordering::SeqCst);
                    self.value = 42;
                    if !self.leaf {
                        let mut leaf = Self::new(Arc::new(AtomicUsize::new(0)));
                        leaf.leaf = true;
                        self.child = Some(
                            ctx.spawner()
                                .spawn_supervised(
                                    leaf,
                                    EmptyConfig,
                                    SupervisorStrategyType::Default(
                                        DefaultStrategy::EscalateFailure,
                                    ),
                                )
                                .await?,
                        );
                    }
                    Ok(())
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                _: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    if self.leaf {
                        return Err(ActorError::MessageHandlingError(
                            "deliberate child failure".into(),
                        ));
                    }
                    Ok(
                        Box::new((self.value, self.child.as_ref().unwrap().clone_boxed()))
                            as BoxedMessage,
                    )
                })
            }
        }
    };
}
node!(ThreadParent, parrot::thread::ThreadContext<Self>);
node!(
    ActixParent,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
async fn fail_child(parent: &BoxedActorRef) -> ActorResult<()> {
    let (value, child) = *parent
        .send(Box::new(()))
        .await?
        .downcast::<(usize, BoxedActorRef)>()
        .unwrap();
    assert_eq!(value, 42);
    assert!(
        matches!(child.send(Box::new(())).await,Err(ActorError::MessageHandlingError(ref s)) if s=="deliberate child failure")
    );
    Ok(())
}
async fn verify(parent: &BoxedActorRef, inits: &AtomicUsize) -> ActorResult<()> {
    let (value, _) = *parent
        .send(Box::new(()))
        .await?
        .downcast::<(usize, BoxedActorRef)>()
        .unwrap();
    assert_eq!(value, 42);
    assert_eq!(inits.load(Ordering::SeqCst), 1);
    parent.stop().await
}
#[actix::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
    let inits = Arc::new(AtomicUsize::new(0));
    let parent: BoxedActorRef = Box::new(
        thread
            .spawn_at(
                ThreadParent::new(inits.clone()),
                "/parent",
                None,
                parrot::thread::ThreadActorConfig {
                    supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Resume),
                    ..Default::default()
                },
            )
            .await?,
    );
    fail_child(&parent).await?;
    // Actual child retirement follows publication of its escalation signal.
    // The following parent request waits for that pending decision to Resume.
    tokio::time::timeout(Duration::from_secs(3), async {
        while thread.actor_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    verify(&parent, &inits).await?;
    assert_eq!(thread.actor_count(), 0);
    thread.shutdown_internal().await?;
    let actix = parrot::actix::ActixActorSystem::with_arbiter_count(1).await?;
    let inits = Arc::new(AtomicUsize::new(0));
    let parent = actix
        .spawn_root_typed_with_strategy(
            ActixParent::new(inits.clone()),
            EmptyConfig,
            SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure),
        )
        .await?;
    fail_child(&parent).await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while actix.status().active_actors != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    verify(&parent, &inits).await?;
    assert_eq!(actix.status().active_actors, 0);
    actix.shutdown().await?;
    println!(
        "Thread and Actix: dynamic parent resumed child escalation, original state 42, init once, empty registry"
    );
    Ok(())
}
