//! Minimal Thread actor: two typed requests and explicit shutdown.
use parrot::thread::{ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRefExt,
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use parrot_api_derive::Message;

#[derive(Message)]
#[message(result = "u64")]
struct Add(u64);

struct Counter(u64);
impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn state(&self) -> ActorState {
        ActorState::Running
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let Add(amount) = *msg
                .downcast::<Add>()
                .map_err(|_| ActorError::MessageHandlingError("unknown counter message".into()))?;
            self.0 += amount;
            Ok(Box::new(self.0) as BoxedMessage)
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let system = ThreadActorSystem::shared(Default::default());
    let result = async {
        let actor = system
            .spawn_root_typed_thread(Counter(0), EmptyConfig)
            .await?;
        assert_eq!(ActorRefExt::ask(&actor, Add(2)).await?, 2);
        assert_eq!(ActorRefExt::ask(&actor, Add(3)).await?, 5);
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    system.shutdown_internal().await?;
    result?;
    assert_eq!(system.actor_count(), 0);
    println!("simple counter=5; system retired");
    Ok(())
}
