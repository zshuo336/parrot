//! Local path-aware lookup into two existing actors. This example has no
//! distributed activation, ownership, migration or retry protocol.
use parrot::{
    ParrotActorSystem, PrefixResolver,
    thread::{context::ThreadContext, system::ThreadActorSystem},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorPath, ActorRef},
    system::ActorSystem,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

struct Increment;
struct Counter(u64);
impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<Increment>() {
                self.0 += 1;
                Ok(Box::new(self.0) as BoxedMessage)
            } else {
                Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "expected Increment".into(),
                ))
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

struct Entities(BTreeMap<String, Arc<dyn ActorRef>>);
impl PrefixResolver for Entities {
    fn resolve(&self, path: &str) -> Option<BoxedActorRef> {
        self.0.get(path).map(|actor| actor.clone_boxed())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let facade = ParrotActorSystem::new(Default::default()).await?;
    let engine = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("local".into(), engine.clone(), true)
        .await?;
    let result = async {
        let mut entities = BTreeMap::new();
        for key in ["alice", "bob"] {
            let actor = engine
                .spawn_at(
                    Counter(0),
                    &format!("/instances/{key}"),
                    None,
                    Default::default(),
                )
                .await?;
            entities.insert(
                format!("/user/entity-{key}"),
                Arc::new(actor) as Arc<dyn ActorRef>,
            );
        }
        facade.register_prefix_resolver("/user/entity-", Arc::new(Entities(entities)))?;
        for (key, expected) in [("alice", 1), ("bob", 1), ("alice", 2)] {
            let path = format!("/user/entity-{key}");
            let reference = facade
                .get_actor(&ActorPath::placeholder(&path))
                .await
                .ok_or("missing entity")?;
            let reply = reference
                .send_with_timeout(Box::new(Increment), Some(Duration::from_secs(3)))
                .await?;
            let count = reply.downcast_ref::<u64>().ok_or("unexpected reply")?;
            assert_eq!(*count, expected);
            println!("{path}: {count}");
        }
        assert!(
            facade
                .get_actor(&ActorPath::placeholder("/user/entity-unknown"))
                .await
                .is_none()
        );
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    let shutdown = facade.shutdown_runtime().await;
    result?;
    shutdown?;
    Ok(())
}
