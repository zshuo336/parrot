/// # Basic Actix Actor Example
///
/// This example demonstrates the core actor patterns using Actix backend
/// without focusing on the runtime complexity. For production use where you
/// might have nested runtime concerns, see the actix_basic_no_tokio example.
///
/// - Simple actor definition
/// - Basic message handling
/// - Request-response pattern with ask
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRefExt;
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::fmt::Debug;

/// Simple message with string payload
#[derive(Debug, Clone)]
struct Greeting(String);

impl Message for Greeting {
    type Result = String;

    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        if let Ok(string) = result.downcast::<String>() {
            Ok(*string)
        } else {
            Err(ActorError::MessageHandlingError(
                "Failed to extract result".to_string(),
            ))
        }
    }
}

/// A minimal actor implementation
struct GreeterActor {
    state: ActorState,
}

impl Default for GreeterActor {
    fn default() -> Self {
        Self {
            state: ActorState::Starting,
        }
    }
}

impl Actor for GreeterActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

    // The core message handler
    fn receive_message<'a>(
        &'a mut self,
        _msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            // Default implementation for async message handling
            Ok(Box::new("Hello from async handler".to_string()) as BoxedMessage)
        })
    }

    // Actix-specific optimized message handling
    fn state(&self) -> ActorState {
        self.state
    }
}

// M6: actix 同步快路径移至引擎侧扩展 trait（ActixEngineExt）。
impl parrot_api::actor::ActixEngineExt for GreeterActor {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine_ctx: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        if let Some(greeting) = msg.downcast_ref::<Greeting>() {
            // Create personalized response
            let response = format!("Hello, {}!", greeting.0);
            Some(Ok(Box::new(response) as BoxedMessage))
        } else {
            // Unknown message type
            Some(Err(ActorError::MessageHandlingError(
                "Unknown message type".to_string(),
            )))
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting basic Actix actor example");

    // Create a controlled environment for the actor system
    actix::System::new().block_on(async {
        // Initialize actor system
        let system = ActixActorSystem::new().await?;

        // Create and spawn our actor
        let actor = GreeterActor::default();
        let actor_ref = system.spawn_root_typed(actor, EmptyConfig).await?;

        // Send a message and get response
        let response = actor_ref.ask(Greeting("Actix World".to_string())).await?;

        println!("Response: {}", response);

        // Cleanup
        system.shutdown().await?;
        actix::System::current().stop();

        Ok(())
    })
}
