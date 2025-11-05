//! Two static factory children recover as one cohort on each engine.
use parrot::{
    actix::ActixActorSystem,
    thread::ThreadActorSystem,
    typed_context::{TypedContext, TypedContextExt},
};
use parrot_api::{
    Message, ParrotTypedActor,
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType},
    typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive},
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
pub struct Context;
impl Message for Context {
    type Result = TypedContext;
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Context))]
struct Parent;
impl TypedReceive<Context> for Parent {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Context,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<TypedContext>> {
        Box::pin(async move { Ok(ctx.clone()) })
    }
}
#[derive(Default)]
struct Counts {
    built: AtomicUsize,
    initialized: AtomicUsize,
    dropped: AtomicUsize,
}
struct Counter {
    counts: Arc<Counts>,
    generation: usize,
}
enum Op {
    Read,
    Fail,
}
impl Message for Op {
    type Result = usize;
}
impl ParrotMsgVariant<Counter> for Op {
    fn inject(op: Self) -> Self {
        op
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
impl ParrotTypedDispatch for Counter {
    type Msg = Op;
    type Reply = usize;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.counts.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&mut self, op: Op) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async move {
            match op {
                Op::Read => Ok(self.generation),
                Op::Fail => Err(ActorError::MessageHandlingError(
                    "requested cohort restart".into(),
                )),
            }
        })
    }
}
impl Drop for Counter {
    fn drop(&mut self) {
        self.counts.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
async fn run(context: TypedContext) -> ActorResult<[Arc<Counts>; 2]> {
    context.set_child_strategy(SupervisorStrategyType::OneForAll(OneForAllStrategy {
        max_restarts: 1,
        within: Duration::from_secs(30),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    }));
    let counts = [Arc::new(Counts::default()), Arc::new(Counts::default())];
    let mut children = Vec::new();
    for count in &counts {
        let state = count.clone();
        children.push(
            context
                .spawn_typed_child_factory::<_, Op, _>(move || Counter {
                    generation: state.built.fetch_add(1, Ordering::SeqCst),
                    counts: state.clone(),
                })
                .await?,
        );
    }
    for child in &children {
        assert_eq!(child.ask(Op::Read).await?, 0);
    }
    assert!(children[0].ask(Op::Fail).await.is_err());
    tokio::time::timeout(Duration::from_secs(3), async {
        while counts
            .iter()
            .any(|count| count.initialized.load(Ordering::SeqCst) != 2)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| ActorError::Timeout)?;
    for child in &children {
        assert_eq!(child.ask(Op::Read).await?, 1);
    }
    assert_eq!(context.children().len(), 2);
    for count in &counts {
        assert_eq!(count.built.load(Ordering::SeqCst), 2);
        assert_eq!(count.dropped.load(Ordering::SeqCst), 1);
    }
    Ok(counts)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    actix::System::new().block_on(async {
        let system=ThreadActorSystem::shared(Default::default());
        let parent=system.spawn_typed::<_,Context>(Parent,"/factory-parent").await?;
        let counts=run(parent.ask(Context).await?).await?;parent.stop().await?;
        for count in counts {assert_eq!(count.dropped.load(Ordering::SeqCst),2);}
        assert_eq!(system.actor_count(),0);system.shutdown_internal().await?;
        let system=Arc::new(ActixActorSystem::with_arbiter_count(1).await?);
        let parent=system.spawn_typed::<_,Context>(Parent,"/factory-parent").await?;
        let counts=run(parent.ask(Context).await?).await?;parent.stop().await?;
        for count in counts {assert_eq!(count.dropped.load(Ordering::SeqCst),2);}
        assert_eq!(system.status().active_actors,0);system.as_ref().clone().shutdown().await?;
        println!("Thread and Actix: two typed children rebuilt together, original references, all generations retired");
        Ok(())
    })
}
