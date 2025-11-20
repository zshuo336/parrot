//! A -> B -> C -> D -> E -> F -> A, followed by confirmed retirement.
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRefExt,
    context::ActorContext,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_api_derive::Message;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

#[derive(Message)]
struct Next(BoxedActorRef);

#[derive(Message)]
struct Token {
    value: u32,
    remaining: usize,
    visited: Vec<&'static str>,
    completed: oneshot::Sender<(u32, Vec<&'static str>)>,
}

struct RingActor {
    name: &'static str,
    next: Option<BoxedActorRef>,
    retired: Arc<Mutex<Vec<&'static str>>>,
}

impl Drop for RingActor {
    fn drop(&mut self) {
        self.retired.lock().unwrap().push(self.name);
    }
}

impl ActixEngineExt for RingActor {}

impl Actor for RingActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

    fn state(&self) -> ActorState {
        ActorState::Running
    }

    fn use_async_handler(&self) -> bool {
        true
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(next) = msg.downcast_ref::<Next>() {
                self.next = Some(next.0.clone_boxed());
            } else {
                let mut token = *msg
                    .downcast::<Token>()
                    .map_err(|_| ActorError::MessageHandlingError("unknown ring message".into()))?;
                token.value += 1;
                token.visited.push(self.name);
                if token.remaining == 0 {
                    token
                        .completed
                        .send((token.value, token.visited))
                        .map_err(|_| {
                            ActorError::MessageHandlingError("ring observer closed".into())
                        })?;
                } else {
                    token.remaining -= 1;
                    let next = self.next.as_ref().ok_or_else(|| {
                        ActorError::MessageHandlingError("ring has no next actor".into())
                    })?;
                    // Waiting for a business reply around the cycle would deadlock.
                    ctx.send(next.clone_boxed(), Box::new(token)).await?;
                }
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}

#[actix::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let system = ActixActorSystem::new().await?;
    let retired = Arc::new(Mutex::new(Vec::new()));
    let result = async {
        let mut actors = Vec::new();
        for name in ["A", "B", "C", "D", "E", "F"] {
            actors.push(
                system
                    .spawn_root_typed(
                        RingActor {
                            name,
                            next: None,
                            retired: retired.clone(),
                        },
                        EmptyConfig,
                    )
                    .await?,
            );
        }
        for index in 0..actors.len() {
            actors[index]
                .ask(Next(actors[(index + 1) % actors.len()].clone_boxed()))
                .await?;
        }
        let (completed, received) = oneshot::channel();
        actors[0]
            .tell(Token {
                value: 1,
                remaining: 6,
                visited: Vec::new(),
                completed,
            })
            .await?;
        let (value, visited) = tokio::time::timeout(Duration::from_secs(5), received).await??;
        assert_eq!(value, 8);
        assert_eq!(visited, ["A", "B", "C", "D", "E", "F", "A"]);
        for actor in actors[1..].iter().chain(&actors[..1]) {
            actor.stop().await?;
            assert!(!actor.is_alive().await);
        }
        assert_eq!(*retired.lock().unwrap(), ["B", "C", "D", "E", "F", "A"]);
        assert_eq!(system.status().active_actors, 0);
        println!("ring={} value={value} retired=6", visited.join("->"));
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    system.shutdown().await?;
    result
}
