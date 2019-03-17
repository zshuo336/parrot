use std::time::Duration;
use std::ptr::NonNull;
use std::any::Any;
use std::sync::{Arc, atomic::{AtomicU32, Ordering}};

use actix;
use anyhow::Result;
use parrot::actix::{ActixActorSystem, ActixActor, ActixContext};
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    message::Message,
    match_message,
    types::{BoxedMessage, ActorResult, BoxedActorRef, BoxedFuture},
    system::{ActorSystemConfig, ActorSystem},
    address::{ActorRefExt, ActorPath},
    errors::ActorError,
};
use parrot_api_derive::{Message, ParrotActor};
use actix::ActorContext as ActixActorContextTrait;

mod test_helpers;
use test_helpers::{setup_test_system, wait_for, with_test_system, DEFAULT_WAIT_TIME};

// Define messages for sync tests
#[derive(Clone, Debug, Message)]
#[message(result = "u32")]
struct AddMessage(u32, u32);

#[derive(Clone, Debug, Message)]
#[message(result = "u32")]
struct MultiplyMessage(u32, u32);

#[derive(Clone, Debug, Message)]
#[message(result = "u32")]
struct GetCounterMessage;

#[derive(Clone, Debug, Message)]
#[message(result = "()")]
struct ResetCounterMessage;

// Define a test actor for synchronous message handling
#[derive(Debug, Clone, ParrotActor)]
#[ParrotActor(engine = "actix")]
struct MathActor {
    counter: u32,
    state: ActorState,
}

impl MathActor {
    pub fn new() -> Self {
        Self {
            counter: 0,
            state: ActorState::Starting,
        }
    }

    // 添加异步handle_message方法以满足ParrotActor派生宏的要求
    async fn handle_message(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>) -> ActorResult<BoxedMessage> {
        match_message!(self, msg,
            AddMessage => |actor: &mut Self, add_msg: &AddMessage| {
                // Increment counter for operation tracking
                actor.counter += 1;
                
                // Calculate sum
                add_msg.0 + add_msg.1
            },
            
            MultiplyMessage => |actor: &mut Self, mult_msg: &MultiplyMessage| {
                // Increment counter for operation tracking
                actor.counter += 1;
                
                // Calculate product
                mult_msg.0 * mult_msg.1
            },
            
            GetCounterMessage => |actor: &mut Self, _: &GetCounterMessage| {
                // Return current operation counter
                actor.counter
            },
            
            ResetCounterMessage => |actor: &mut Self, _: &ResetCounterMessage| {
                // Reset counter
                actor.counter = 0;
                ()
            }
        )
    }

    fn handle_message_engine(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>, _engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        match_message!("option", self, msg,
            AddMessage => |actor: &mut Self, add_msg: &AddMessage| {
                // Increment counter for operation tracking
                actor.counter += 1;
                
                // Calculate sum
                add_msg.0 + add_msg.1
            },
            
            MultiplyMessage => |actor: &mut Self, mult_msg: &MultiplyMessage| {
                // Increment counter for operation tracking
                actor.counter += 1;
                
                // Calculate product
                mult_msg.0 * mult_msg.1
            },
            
            GetCounterMessage => |actor: &mut Self, _: &GetCounterMessage| {
                // Return current operation counter
                actor.counter
            },
            
            ResetCounterMessage => |actor: &mut Self, _: &ResetCounterMessage| {
                // Reset counter
                actor.counter = 0;
                ()
            }
        )
    }
}

// Run the tests
fn main() {
    actix::System::new().block_on(async {
        // Run all tests
        let results = run_all_tests().await;
        
        match results {
            Ok(_) => println!("All sync message tests passed!"),
            Err(e) => {
                eprintln!("Test failure: {}", e);
                std::process::exit(1);
            }
        }
    });
}

async fn run_all_tests() -> Result<()> {
    test_sync_add_operation().await?;
    test_sync_multiply_operation().await?;
    test_operation_counter().await?;
    test_message_ordering().await?;
    
    Ok(())
}

async fn test_sync_add_operation() -> Result<()> {
    with_test_system(|system| async move {
        // Create math actor
        let actor = MathActor::new();
        let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
        
        // Test addition with various values
        let sum1 = actor_ref.ask(AddMessage(5, 3)).await?;
        assert_eq!(sum1, 8, "5 + 3 should equal 8");
        
        let sum2 = actor_ref.ask(AddMessage(10, 20)).await?;
        assert_eq!(sum2, 30, "10 + 20 should equal 30");
        
        let sum3 = actor_ref.ask(AddMessage(0, 100)).await?;
        assert_eq!(sum3, 100, "0 + 100 should equal 100");
        
        Ok(((), system))
    }).await
}

async fn test_sync_multiply_operation() -> Result<()> {
    with_test_system(|system| async move {
        // Create math actor
        let actor = MathActor::new();
        let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
        
        // Test multiplication with various values
        let product1 = actor_ref.ask(MultiplyMessage(4, 3)).await?;
        assert_eq!(product1, 12, "4 * 3 should equal 12");
        
        let product2 = actor_ref.ask(MultiplyMessage(10, 0)).await?;
        assert_eq!(product2, 0, "10 * 0 should equal 0");
        
        let product3 = actor_ref.ask(MultiplyMessage(5, 5)).await?;
        assert_eq!(product3, 25, "5 * 5 should equal 25");
        
        Ok(((), system))
    }).await
}

async fn test_operation_counter() -> Result<()> {
    with_test_system(|system| async move {
        // Create math actor
        let actor = MathActor::new();
        let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
        
        // Initial counter should be 0
        let initial = actor_ref.ask(GetCounterMessage).await?;
        assert_eq!(initial, 0, "Initial counter should be 0");
        
        // Perform some operations and check counter updates
        actor_ref.ask(AddMessage(1, 2)).await?;
        actor_ref.ask(MultiplyMessage(3, 4)).await?;
        actor_ref.ask(AddMessage(5, 6)).await?;
        
        // Counter should now be 3
        let counter = actor_ref.ask(GetCounterMessage).await?;
        assert_eq!(counter, 3, "Counter should be 3 after 3 operations");
        
        // Reset counter
        actor_ref.ask(ResetCounterMessage).await?;
        
        // Counter should be back to 0
        let reset = actor_ref.ask(GetCounterMessage).await?;
        assert_eq!(reset, 0, "Counter should be 0 after reset");
        
        Ok(((), system))
    }).await
}

async fn test_message_ordering() -> Result<()> {
    with_test_system(|system| async move {
        // Create math actor
        let actor = MathActor::new();
        let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
        
        // Reset counter to ensure clean state
        actor_ref.ask(ResetCounterMessage).await?;
        
        // Send multiple messages in rapid succession
        for i in 1..=10 {
            actor_ref.ask(AddMessage(i, 0)).await?;
        }
        
        // Counter should be 10 after 10 operations
        let counter = actor_ref.ask(GetCounterMessage).await?;
        assert_eq!(counter, 10, "Counter should be 10 after 10 sequential operations");
        
        Ok(((), system))
    }).await
} 