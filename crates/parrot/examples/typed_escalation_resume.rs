//! Run with cargo run --locked -p parrot --example typed_escalation_resume.
use parrot::{
    actix::ActixActorSystem,
    thread::ThreadActorSystem,
    typed_context::{TypedContext, TypedContextExt},
};
use parrot_api::{
    Message, ParrotTypedActor,
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    typed::{TypedLifecycle, TypedReceive},
    typed_channel::TypedActorRef,
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
pub struct Fail;
impl Message for Fail {
    type Result = ();
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Fail))]
pub struct Child;
impl TypedReceive<Fail> for Child {
    fn receive_typed(&mut self, _: Fail) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Err(ActorError::MessageHandlingError("child failure".into())) })
    }
}
pub struct Inspect;
impl Message for Inspect {
    type Result = (usize, TypedActorRef<Child, Fail>);
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Inspect), lifecycle)]
struct Parent {
    value: usize,
    child: Option<TypedActorRef<Child, Fail>>,
    inits: Arc<AtomicUsize>,
}
impl TypedLifecycle for Parent {
    fn init_with_context<'a>(
        &'a mut self,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.inits.fetch_add(1, Ordering::SeqCst);
            self.value = 42;
            self.child = Some(
                ctx.spawn_typed_child_with_strategy(
                    Child,
                    SupervisorStrategyType::OneForOne(OneForOneStrategy {
                        max_restarts: 0,
                        within: Duration::from_secs(30),
                        decider: BasicDecisionFn::new(|_| SupervisionDecision::Escalate),
                    }),
                )
                .await?,
            );
            Ok(())
        })
    }
}
impl TypedReceive<Inspect> for Parent {
    fn receive_typed(
        &mut self,
        _: Inspect,
    ) -> BoxedFuture<'_, ActorResult<(usize, TypedActorRef<Child, Fail>)>> {
        Box::pin(async { Ok((self.value, self.child.as_ref().unwrap().clone())) })
    }
}
async fn verify(
    parent: TypedActorRef<Parent, Inspect>,
    decisions: Arc<AtomicUsize>,
    inits: Arc<AtomicUsize>,
) -> ActorResult<()> {
    let (value, child) = parent.ask(Inspect).await?;
    assert_eq!(value, 42);
    assert!(
        matches!(child.ask(Fail).await, Err(ActorError::MessageHandlingError(ref s)) if s == "child failure")
    );
    // A child reply alone does not mean its parent's policy has run.
    tokio::time::timeout(Duration::from_secs(3), async {
        while decisions.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("parent decision");
    assert_eq!(parent.ask(Inspect).await?.0, 42);
    assert_eq!(inits.load(Ordering::SeqCst), 1);
    assert_eq!(decisions.load(Ordering::SeqCst), 1);
    parent.stop().await
}
fn inputs() -> (
    Parent,
    SupervisorStrategyType,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let decisions = Arc::new(AtomicUsize::new(0));
    let inits = Arc::new(AtomicUsize::new(0));
    let calls = decisions.clone();
    (
        Parent {
            value: 0,
            child: None,
            inits: inits.clone(),
        },
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 0,
            within: Duration::from_secs(30),
            decider: BasicDecisionFn::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                SupervisionDecision::Resume
            }),
        }),
        decisions,
        inits,
    )
}
#[actix::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let thread = ThreadActorSystem::shared(Default::default());
    let (actor, policy, decisions, inits) = inputs();
    let parent = thread
        .spawn_typed_with_strategy(actor, "/parent", policy)
        .await?;
    verify(parent, decisions, inits).await?;
    assert_eq!(thread.actor_count(), 0);
    thread.shutdown_internal().await?;
    let actix = Arc::new(ActixActorSystem::with_arbiter_count(1).await?);
    let (actor, policy, decisions, inits) = inputs();
    let parent = actix
        .spawn_typed_with_strategy(actor, "/parent", policy)
        .await?;
    verify(parent, decisions, inits).await?;
    assert_eq!(actix.status().active_actors, 0);
    actix.as_ref().clone().shutdown().await?;
    println!(
        "Thread and Actix: child escalation resumed the original parent, state 42, init once, empty registry"
    );
    Ok(())
}
