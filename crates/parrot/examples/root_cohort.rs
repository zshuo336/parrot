//! Two parentless roots recover together through their original typed references.
use parrot::{actix::ActixActorSystem, thread::ThreadActorSystem};
use parrot_api::{
    Message,
    errors::ActorError,
    supervisor::{DefaultSupervisorStrategyFactory, SupervisorStrategyType},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Probe(usize);
enum Request {
    Generation,
    Fail,
}
impl Message for Request {
    type Result = usize;
}
impl ParrotMsgVariant<Probe> for Request {
    fn inject(value: Self) -> Self {
        value
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Request;
    type Reply = usize;
    fn dispatch(&mut self, request: Request) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async move {
            match request {
                Request::Generation => Ok(self.0),
                Request::Fail => Err(ActorError::MessageHandlingError("example failure".into())),
            }
        })
    }
}
fn policy() -> SupervisorStrategyType {
    SupervisorStrategyType::OneForAll(DefaultSupervisorStrategyFactory::one_for_all(
        1,
        Duration::from_secs(60),
    ))
}

#[actix::main]
async fn main() -> anyhow::Result<()> {
    let thread = ThreadActorSystem::shared(Default::default());
    let actix = Arc::new(ActixActorSystem::new().await?);
    for use_actix in [false, true] {
        let group = if use_actix {
            actix.root_cohort(policy()).await?
        } else {
            thread.root_cohort(policy()).await?
        };
        let first_generation = Arc::new(AtomicUsize::new(0));
        let second_generation = Arc::new(AtomicUsize::new(0));
        let first = move || Probe(first_generation.fetch_add(1, Ordering::SeqCst));
        let second = move || Probe(second_generation.fetch_add(1, Ordering::SeqCst));
        let (a, b) = if use_actix {
            (
                actix
                    .spawn_typed_factory_in_cohort::<_, Request, _>(first, "/first", &group)
                    .await?,
                actix
                    .spawn_typed_factory_in_cohort::<_, Request, _>(second, "/second", &group)
                    .await?,
            )
        } else {
            (
                thread
                    .spawn_typed_factory_in_cohort::<_, Request, _>(first, "/first", &group)
                    .await?,
                thread
                    .spawn_typed_factory_in_cohort::<_, Request, _>(second, "/second", &group)
                    .await?,
            )
        };
        assert_eq!(a.ask(Request::Generation).await?, 0);
        assert_eq!(b.ask(Request::Generation).await?, 0);
        assert!(a.ask(Request::Fail).await.is_err());
        assert_eq!(a.ask(Request::Generation).await?, 1);
        assert_eq!(b.ask(Request::Generation).await?, 1);
        assert_eq!(group.members().len(), 2);
        group.stop().await?;
        assert!(group.members().is_empty());
        println!(
            "{}: both original root references reached generation 1 and retired",
            if use_actix { "Actix" } else { "Thread" }
        );
    }
    thread.shutdown_internal().await?;
    actix.as_ref().clone().shutdown().await?;
    Ok(())
}
