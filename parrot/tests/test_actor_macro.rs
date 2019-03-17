use std::time::Duration;
use std::ptr::NonNull;
use std::any::Any;
use std::sync::Arc;

use actix;
use anyhow::Result;
use parrot::actix::{ActixActorSystem, ActixActor, ActixContext};
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    message::Message,
    types::{BoxedMessage, ActorResult, BoxedActorRef, BoxedFuture, WeakActorTarget},
    system::{ActorSystemConfig, ActorSystem},
    address::{ActorRefExt, ActorPath},
    errors::ActorError,
    match_message,
};
use parrot_api_derive::{Message, ParrotActor};

mod test_helpers;
use test_helpers::{setup_test_system, wait_for, with_test_system, DEFAULT_WAIT_TIME};

// Define messages using the macro
#[derive(Clone, Debug, Message)]
#[message(result = "String")]
struct GreetMessage {
    name: String,
}

#[derive(Clone, Debug, Message)]
#[message(result = "u32")]
struct CounterMessage {
    increment_by: u32,
}

#[derive(Clone, Debug, Message)]
#[message(result = "Vec<String>")]
struct GetStateMessage;

// Define an actor using the macro
#[derive(Debug, Clone, ParrotActor)]
#[ParrotActor(engine = "actix")]
struct MacroTestActor {
    name: String,
    counter: u32,
    messages_received: Vec<String>,
    state: ActorState,
}

impl MacroTestActor {
    pub fn new(name: String) -> Self {
        Self {
            name,
            counter: 0,
            messages_received: Vec::new(),
            state: ActorState::Starting,
        }
    }

    // Define the engine-specific message handler required by the macro
    fn handle_message_engine(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>, _engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        match_message!("option", self, msg,
            GreetMessage => |actor: &mut Self, greet: &GreetMessage| {
                // Record the greeting and return a response
                let message = format!("Hello, {}!", greet.name);
                actor.messages_received.push(message.clone());
                message
            },
            CounterMessage => |actor: &mut Self, count: &CounterMessage| {
                // Update counter and return new value
                actor.counter += count.increment_by;
                actor.counter
            },
            GetStateMessage => |actor: &mut Self, _: &GetStateMessage| {
                // Return a copy of all received messages
                actor.messages_received.clone()
            }
        )
    }

    // 添加必要的handle_message方法
    async fn handle_message(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>) -> ActorResult<BoxedMessage> {
        match_message!(self, msg,
            CounterMessage => |actor: &mut Self, msg: &CounterMessage| {
                // Increment counter by the message value
                actor.counter += msg.increment_by;
                actor.counter
            },

            GreetMessage => |actor: &mut Self, msg: &GreetMessage| {
                // Record the greeting and return a response
                let message = format!("Hello, {}!", msg.name);
                actor.messages_received.push(message.clone());
                message
            },

            GetStateMessage => |actor: &mut Self, _: &GetStateMessage| {
                // Return a copy of all received messages
                actor.messages_received.clone()
            }
        )
    }
}

// Run all tests
fn main() {
    actix::System::new().block_on(async {
        // Run all tests and report results
        let results = run_all_tests().await;
        
        match results {
            Ok(_) => println!("All macro tests passed!"),
            Err(e) => {
                eprintln!("Test failure: {}", e);
                std::process::exit(1);
            }
        }
    });
}

async fn run_all_tests() -> Result<()> {
    test_macro_actor_creation().await?;
    test_macro_message_handling().await?;
    test_macro_state_tracking().await?;
    
    Ok(())
}

async fn test_macro_actor_creation() -> Result<()> {
    with_test_system(|system| async move {
        // Create actor using the macro-generated implementation
        let actor = MacroTestActor::new("MacroTest".to_string());
        let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
        
        // Verify that we have a valid actor reference
        let path = actor_ref.path();
        assert!(!path.is_empty(), "Should have a valid actor path");
        
        Ok(((), system))
    }).await
}

async fn test_macro_message_handling() -> Result<()> {
    with_test_system(|system| async move {
        // Create and spawn the actor
        let actor = MacroTestActor::new("MacroTest".to_string());
        let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
        
        // Test greeting message
        let response = actor_ref.ask(GreetMessage {
            name: "MacroTest".to_string(),
        }).await?;
        
        assert_eq!(response, "Hello, MacroTest!", "Should get correct greeting response");
        
        // Test counter message
        let counter_value = actor_ref.ask(CounterMessage {
            increment_by: 10,
        }).await?;
        
        assert_eq!(counter_value, 10, "Counter should be incremented to 10");
        
        // Increment again
        let counter_value = actor_ref.ask(CounterMessage {
            increment_by: 5,
        }).await?;
        
        assert_eq!(counter_value, 15, "Counter should be incremented to 15");
        
        Ok(((), system))
    }).await
}

async fn test_macro_state_tracking() -> Result<()> {
    with_test_system(|system| async move {
        // Create and spawn the actor
        let actor = MacroTestActor::new("MacroTest".to_string());
        let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
        
        // Send multiple messages to accumulate state
        actor_ref.ask(GreetMessage {
            name: "User1".to_string(),
        }).await?;
        
        actor_ref.ask(GreetMessage {
            name: "User2".to_string(),
        }).await?;
        
        actor_ref.ask(GreetMessage {
            name: "User3".to_string(),
        }).await?;
        
        // Get the accumulated state
        let messages = actor_ref.ask(GetStateMessage).await?;
        
        // Verify we have the expected messages
        assert_eq!(messages.len(), 3, "Should have 3 messages in state");
        assert!(messages.contains(&"Hello, User1!".to_string()), "Should contain greeting for User1");
        assert!(messages.contains(&"Hello, User2!".to_string()), "Should contain greeting for User2");
        assert!(messages.contains(&"Hello, User3!".to_string()), "Should contain greeting for User3");
        
        Ok(((), system))
    }).await
} 