//! Run with cargo run --locked -p parrot --example typed_factory.
use parrot::thread::{ThreadActorSystem, typed::SingleVariant};
use parrot_api::{
    errors::ActorError,
    message::Message,
    typed::TypedReceive,
    types::{ActorResult, BoxedFuture},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Request(bool);
impl Message for Request {
    type Result = usize;
}
struct Counter {
    generation: usize,
    drops: Arc<AtomicUsize>,
}
impl Drop for Counter {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl TypedReceive<Request> for Counter {
    fn receive_typed(&mut self, request: Request) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async move {
            if request.0 {
                Err(ActorError::MessageHandlingError(
                    "deliberate failure".into(),
                ))
            } else {
                Ok(self.generation)
            }
        })
    }
}
#[actix::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let system = ThreadActorSystem::shared(Default::default());
    let built = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let made = built.clone();
    let retired = drops.clone();
    let actor = system
        .spawn_typed_single_factory::<_, Request, _>(
            move || Counter {
                generation: made.fetch_add(1, Ordering::SeqCst),
                drops: retired.clone(),
            },
            "/counter",
        )
        .await?;
    for generation in 0..=2 {
        assert_eq!(
            actor.ask(SingleVariant::wrap(Request(false))).await?,
            generation
        );
        if generation < 2 {
            assert!(matches!(
                actor.ask(SingleVariant::wrap(Request(true))).await,
                Err(ActorError::MessageHandlingError(_))
            ));
        }
    }
    actor.stop().await?;
    assert_eq!(built.load(Ordering::SeqCst), 3);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    system.shutdown_internal().await?;
    let system = Arc::new(parrot::actix::ActixActorSystem::with_arbiter_count(1).await?);
    let built = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let made = built.clone();
    let retired = drops.clone();
    let actor = system
        .spawn_typed_single_factory_with_strategy::<_, Request, _>(
            move || Counter {
                generation: made.fetch_add(1, Ordering::SeqCst),
                drops: retired.clone(),
            },
            "/counter",
            parrot_api::supervisor::OneForOneStrategy {
                max_restarts: 2,
                within: std::time::Duration::from_secs(60),
                decider: parrot_api::supervisor::BasicDecisionFn::new(|_| {
                    parrot_api::supervisor::SupervisionDecision::Restart
                }),
            },
        )
        .await?;
    for generation in 0..=2 {
        assert_eq!(
            actor.ask(SingleVariant::wrap(Request(false))).await?,
            generation
        );
        if generation < 2 {
            assert!(matches!(
                actor.ask(SingleVariant::wrap(Request(true))).await,
                Err(ActorError::MessageHandlingError(_))
            ));
        }
    }
    actor.stop().await?;
    assert_eq!(built.load(Ordering::SeqCst), 3);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    system.as_ref().clone().shutdown().await?;
    println!(
        "Thread and Actix typed factories: three generations each, original references, all instances retired"
    );
    Ok(())
}
