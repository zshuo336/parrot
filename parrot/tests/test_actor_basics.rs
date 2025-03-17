use std::time::Duration;
use std::ptr::NonNull;
use std::any::Any;
use std::sync::Arc;

use actix;
use anyhow::Result;
use async_trait::async_trait;
use parrot::actix::{ActixActorSystem, ActixActor, ActixContext};
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    message::Message,
    types::{BoxedMessage, ActorResult, BoxedActorRef, BoxedFuture, WeakActorTarget},
    system::{ActorSystemConfig, ActorSystem},
    address::{ActorRefExt, ActorPath, ActorRef},
    errors::ActorError,
};

mod test_helpers;
use test_helpers::{setup_test_system, wait_for, with_test_system, DEFAULT_WAIT_TIME};

// Define a mock actor reference for testing
#[derive(Debug)]
struct MockActorRef {
    path_value: String,
}

impl MockActorRef {
    fn new(path: &str) -> Self {
        Self {
            path_value: path.to_string(),
        }
    }
}

#[async_trait]
impl ActorRef for MockActorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            Ok(msg)
        })
    }
    
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            Ok(())
        })
    }
    
    fn path(&self) -> String {
        self.path_value.clone()
    }
    
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move {
            true
        })
    }
    
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            path_value: self.path_value.clone(),
        })
    }
}

// Define a basic message for testing
#[derive(Clone, Debug)]
struct TestMessage(String);

impl Message for TestMessage {
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

// Define a test counter message
#[derive(Clone, Debug)]
struct IncrementCounter(u32);

impl Message for IncrementCounter {
    type Result = u32;
    
    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        if let Ok(value) = result.downcast::<u32>() {
            Ok(*value)
        } else {
            Err(ActorError::MessageHandlingError(
                "Failed to extract counter result".to_string(),
            ))
        }
    }
}

// Define a basic test actor
struct BasicTestActor {
    state: ActorState,
    counter: u32,
    name: String,
}

impl BasicTestActor {
    pub fn new(name: String) -> Self {
        Self {
            state: ActorState::Starting,
            counter: 0,
            name,
        }
    }
}

impl Actor for BasicTestActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    
    // Implement the required receive_message method
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            // Default implementation for async message handling
            if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
                // Echo back with actor name
                let response = format!("{} received: {}", self.name, test_msg.0);
                Ok(Box::new(response) as BoxedMessage)
            } else if let Some(counter_msg) = msg.downcast_ref::<IncrementCounter>() {
                // Increment counter and return new value
                self.counter += counter_msg.0;
                Ok(Box::new(self.counter) as BoxedMessage)
            } else {
                // Unknown message type
                Err(ActorError::MessageHandlingError(
                    "Unknown message type".to_string()
                ))
            }
        })
    }
    
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine_ctx: NonNull<dyn Any>
    ) -> Option<ActorResult<BoxedMessage>> {
        if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
            // Echo back with actor name
            let response = format!("{} received: {}", self.name, test_msg.0);
            Some(Ok(Box::new(response) as BoxedMessage))
        } else if let Some(counter_msg) = msg.downcast_ref::<IncrementCounter>() {
            // Increment counter and return new value
            self.counter += counter_msg.0;
            Some(Ok(Box::new(self.counter) as BoxedMessage))
        } else {
            // Unknown message type
            Some(Err(ActorError::MessageHandlingError(
                "Unknown message type".to_string()
            )))
        }
    }
    
    fn state(&self) -> ActorState {
        self.state
    }
}

// Run basic actor tests
fn main() {
    actix::System::new().block_on(async {
        // Run all tests and report results
        let results = run_all_tests().await;
        
        match results {
            Ok(_) => println!("All tests passed!"),
            Err(e) => {
                eprintln!("Test failure: {}", e);
                std::process::exit(1);
            }
        }
    });
}

async fn run_all_tests() -> Result<()> {
    test_basic_actor_creation().await?;
    test_actor_message_handling().await?;
    test_actor_counter().await?;
    test_actor_get_by_path().await?;
    
    Ok(())
}

// Using fn instead of #[test] because async functions can't be used directly as tests
async fn test_basic_actor_creation() -> Result<()> {
    with_test_system(|system| async move {
        // Create a basic test actor
        let test_actor = BasicTestActor::new("TestActor1".to_string());
        let actor_ref = system.spawn_root_actix(test_actor, EmptyConfig::default()).await?;
        
        // Check that we can get a valid reference - test that the actor has a path
        let actor_path = actor_ref.path();
        assert!(!actor_path.is_empty(), "Actor reference should have a valid path");
        
        Ok(((), system))
    }).await
}

async fn test_actor_message_handling() -> Result<()> {
    with_test_system(|system| async move {
        // Create a basic test actor
        let test_actor = BasicTestActor::new("MessageHandler".to_string());
        let actor_ref = system.spawn_root_actix(test_actor, EmptyConfig::default()).await?;
        
        // Send a test message
        let msg = TestMessage("Hello Actor!".to_string());
        let response = actor_ref.ask(msg).await?;
        
        // Verify response
        assert_eq!(
            response, 
            "MessageHandler received: Hello Actor!",
            "Actor should process messages and return correct responses"
        );
        
        Ok(((), system))
    }).await
}

async fn test_actor_counter() -> Result<()> {
    with_test_system(|system| async move {
        // Create a counter test actor
        let test_actor = BasicTestActor::new("Counter".to_string());
        let actor_ref = system.spawn_root_actix(test_actor, EmptyConfig::default()).await?;
        
        // Test initial counter value
        let initial = actor_ref.ask(IncrementCounter(0)).await?;
        assert_eq!(initial, 0, "Initial counter should be 0");
        
        // Increment counter multiple times
        let val1 = actor_ref.ask(IncrementCounter(5)).await?;
        assert_eq!(val1, 5, "Counter should be 5 after incrementing by 5");
        
        let val2 = actor_ref.ask(IncrementCounter(10)).await?;
        assert_eq!(val2, 15, "Counter should be 15 after incrementing by 10");
        
        Ok(((), system))
    }).await
}

async fn test_actor_get_by_path() -> Result<()> {
    with_test_system(|system| async move {
        // Create a test actor
        let test_actor = BasicTestActor::new("PathTest".to_string());
        let actor_ref = system.spawn_root_actix(test_actor, EmptyConfig::default()).await?;
        
        // Get actor path
        let actor_path_str = actor_ref.path();
        
        // Create a mock actor reference for the target
        let mock_ref = MockActorRef::new(&actor_path_str);
        
        // Try to get actor by path - use ActorPath struct
        if let Some(retrieved_ref) = system.internal_get_actor(&ActorPath {
            path: actor_path_str.clone(),
            target: Arc::new(mock_ref) as WeakActorTarget,
        }).await {
            // Send message to verify it's the same actor
            let response = retrieved_ref.ask(TestMessage("Via Path".to_string())).await?;
            assert_eq!(
                response,
                "PathTest received: Via Path", 
                "Should get the same actor through path reference"
            );
        } else {
            anyhow::bail!("Failed to retrieve actor by path");
        }
        
        Ok(((), system))
    }).await
} 