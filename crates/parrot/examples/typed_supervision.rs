//! Run with cargo run --locked -p parrot --example typed_supervision.
use parrot::{actix::ActixActorSystem, thread::ThreadActorSystem};
use parrot_api::{
    errors::ActorError,
    message::Message,
    supervisor::{DefaultStrategy, SupervisorStrategyType},
    typed::TypedReceive,
    types::{ActorResult, BoxedFuture},
};
use std::sync::Arc;
#[derive(Clone, Copy)]
struct Request {
    fail: bool,
}
impl Message for Request {
    type Result = usize;
}
struct Counter(usize);
impl TypedReceive<Request> for Counter {
    fn receive_typed(&mut self, request: Request) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async move {
            if request.fail {
                self.0 += 1;
                Err(ActorError::MessageHandlingError(
                    "deliberate failure after mutation".into(),
                ))
            } else {
                Ok(self.0)
            }
        })
    }
}
fn resume() -> SupervisorStrategyType {
    SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure)
}
#[actix::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use parrot::thread::typed::SingleVariant;
    let thread = ThreadActorSystem::shared(Default::default());
    let actor = thread
        .spawn_typed_single_with_strategy::<_, Request>(Counter(0), "/counter", resume())
        .await?;
    assert!(matches!(
        actor.ask(SingleVariant::wrap(Request { fail: true })).await,
        Err(ActorError::MessageHandlingError(_))
    ));
    assert_eq!(
        actor
            .ask(SingleVariant::wrap(Request { fail: false }))
            .await?,
        1
    );
    actor.stop().await?;
    thread.shutdown_internal().await?;
    let actix = Arc::new(ActixActorSystem::with_arbiter_count(1).await?);
    let actor = actix
        .spawn_typed_single_with_strategy::<_, Request>(Counter(0), "/counter", resume())
        .await?;
    assert!(matches!(
        actor.ask(SingleVariant::wrap(Request { fail: true })).await,
        Err(ActorError::MessageHandlingError(_))
    ));
    assert_eq!(
        actor
            .ask(SingleVariant::wrap(Request { fail: false }))
            .await?,
        1
    );
    actor.stop().await?;
    actix.as_ref().clone().shutdown().await?;
    println!(
        "Thread and Actix: typed Resume preserved mutation, original reference, and successful stop"
    );
    Ok(())
}
