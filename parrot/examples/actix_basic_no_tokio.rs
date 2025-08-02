/// # Actix-based Actor System Example
/// 
/// This example demonstrates how to:
/// - Create an ActixActorSystem without creating a nested Tokio runtime
/// - Define a custom actor and message
/// - Spawn the actor using spawn_root_typed
/// - Send messages and receive responses
/// - Properly shutdown the system
/// 
/// The key difference in this example is the use of `actix::System::new().block_on()`
/// instead of `#[tokio::main]`, which avoids the "Cannot start a runtime from within 
/// a runtime" error that can occur when a Tokio runtime already exists.

use parrot::actix::{ActixActorSystem, ActixContext, ActixActor};
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::message::Message;
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture};
use parrot_api::errors::ActorError;
use parrot_api::address::{ActorRef, ActorRefExt};
use actix;
use std::fmt::Debug;
use std::time::Duration;
use std::any::Any;
use std::ptr::NonNull;

/// A simple message that carries a string payload
#[derive(Debug, Clone)]
struct TestMessage(String);

impl Message for TestMessage {
    type Result = String;
    
    /// Extract the result from a BoxedMessage
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

/// A simple actor that replies to messages
struct TestActor {
    /// Actor's name for identification
    name: String,
    /// Current actor state
    state: ActorState,
}

impl TestActor {
    /// Create a new TestActor
    fn new(name: String) -> Self {
        Self {
            name,
            state: ActorState::Starting,
        }
    }
}

// Implement the Actor trait for our TestActor
impl Actor for TestActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    
    /// Basic message handling interface (required by trait)
    /// This is used for the general async message processing path
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let name = self.name.clone();
        Box::pin(async move {
            println!("Actor {} received message via async interface", name);
            // Respond with a simple message
            let response = format!("Response from {}", name);
            Ok(Box::new(response) as BoxedMessage)
        })
    }
    
    /// Engine-specific message handling (for Actix)
    /// This is the primary message handler for Actix-based actors
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine_ctx: NonNull<dyn Any>
    ) -> Option<ActorResult<BoxedMessage>> {
        // Try to downcast the message to our TestMessage type
        if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
            println!("Actor {} received TestMessage: {}", self.name, test_msg.0);
            
            // Create and return response
            let response = format!("Hello from {}: received {}", self.name, test_msg.0);
            Some(Ok(Box::new(response) as BoxedMessage))
        } else {
            // Unknown message type
            Some(Err(ActorError::MessageHandlingError(
                format!("Actor {} received unknown message type", self.name)
            )))
        }
    }
    
    /// Return the current actor state
    fn state(&self) -> ActorState {
        self.state
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting Actix-based actor system example");
    
    // Create an actix system and run within it
    // This approach avoids nested runtime errors
    actix::System::new().block_on(async {
        println!("Initializing ActixActorSystem...");
        
        // Create the actor system
        let system = ActixActorSystem::new().await.expect("Failed to create actor system");
        
        println!("Creating actor...");
        
        // Create actor instances
        let actor = TestActor::new("TestActor1".to_string());
        
        // Spawn actor using spawn_root_typed method
        let actor_ref = system.spawn_root_typed(actor, EmptyConfig::default())
            .await
            .expect("Failed to spawn actor");
        
        println!("Created actor: {}", actor_ref.path());
        
        // Send a message and await response
        println!("Sending message to actor...");
        let message = TestMessage("Hello, Actor World!".to_string());
        let response = actor_ref.ask(message)
            .await
            .expect("Failed to get response");
        
        println!("Got response: {}", response);
        
        // Wait a bit to ensure messages are processed
        println!("Waiting for message processing to complete...");
        tokio::time::sleep(Duration::from_millis(500)).await;
        
        // Shutdown the system
        println!("Shutting down actor system...");
        system.shutdown().await.expect("Failed to shutdown system");
        
        // Stop the actix system
        println!("Stopping Actix system...");
        actix::System::current().stop();
    });
    
    println!("Example completed successfully");
    Ok(())
} 