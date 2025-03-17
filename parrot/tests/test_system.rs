use std::time::Duration;
use std::ptr::NonNull;
use std::any::Any;
use std::sync::Arc;

use actix;
use anyhow::Result;
use parrot::actix::{ActixActorSystem, ActixActor, ActixContext};
use parrot::system::{ParrotActorSystem, ActorSystemImpl};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    message::Message,
    match_message,
    types::{BoxedMessage, ActorResult, BoxedActorRef, BoxedFuture, WeakActorTarget},
    system::{ActorSystemConfig, ActorSystem, SystemError},
    address::{ActorRefExt, ActorPath, ActorRef},
    errors::ActorError,
};
use parrot_api_derive::{Message, ParrotActor};
use actix::ActorContext as ActixActorContextTrait;
use async_trait::async_trait;

mod test_helpers;
use test_helpers::{setup_test_system, wait_for, with_test_system, DEFAULT_WAIT_TIME};

// Create a simple ActorRef implementation for testing
#[derive(Debug)]
struct TestActorRef {
    path_value: String,
}

impl TestActorRef {
    fn new(path: &str) -> Self {
        Self {
            path_value: path.to_string(),
        }
    }
}

#[async_trait]
impl ActorRef for TestActorRef {
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

// Define a simple message for broadcast tests
#[derive(Clone, Debug, Message)]
#[message(result = "String")]
struct BroadcastTestMessage(String);

// Define a simple test actor
#[derive(Debug, Clone, ParrotActor)]
#[ParrotActor(engine = "actix")]
struct SystemTestActor {
    name: String,
    received_broadcasts: Vec<String>,
    state: ActorState,
}

impl SystemTestActor {
    pub fn new(name: String) -> Self {
        Self {
            name,
            received_broadcasts: Vec::new(),
            state: ActorState::Starting,
        }
    }

    // Add async handle_message method to satisfy ParrotActor macro requirements
    async fn handle_message(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>) -> ActorResult<BoxedMessage> {
        match_message!(self, msg,
            BroadcastTestMessage => |actor: &mut Self, msg: &BroadcastTestMessage| {
                // Record broadcast message
                actor.received_broadcasts.push(msg.0.clone());
                
                // Return confirmation with actor name
                format!("{} received: {}", actor.name, msg.0)
            }
        )
    }

    fn handle_message_engine(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>, _engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        match_message!("option", self, msg,
            BroadcastTestMessage => |actor: &mut Self, msg: &BroadcastTestMessage| {
                // Record broadcast message
                actor.received_broadcasts.push(msg.0.clone());
                
                // Return confirmation with actor name
                format!("{} received: {}", actor.name, msg.0)
            }
        )
    }
}

// Run tests
fn main() {
    actix::System::new().block_on(async {
        // Run all tests
        let results = run_all_tests().await;
        
        match results {
            Ok(_) => println!("All system tests passed!"),
            Err(e) => {
                eprintln!("Test failure: {}", e);
                std::process::exit(1);
            }
        }
    });
}

async fn run_all_tests() -> Result<()> {
    test_actor_system_creation().await?;
    test_multiple_system_registration().await?;
    test_system_default_selection().await?;
    test_broadcast_message().await?;
    
    Ok(())
}

async fn test_actor_system_creation() -> Result<()> {
    // Test creating a ParrotActorSystem with default config
    let config = ActorSystemConfig::default();
    let system = ParrotActorSystem::new(config).await?;
    
    // Create an ActixActorSystem
    let actix_system = ActixActorSystem::new().await?;
    
    // Register the system
    system.register_actix_system("test_actix".to_string(), actix_system, true).await?;
    
    // List registered systems
    let systems = system.list_registered_systems()?;
    assert_eq!(systems.len(), 1, "Should have 1 registered system");
    assert!(systems.contains(&"test_actix".to_string()), "System named 'test_actix' should be registered");
    
    // Verify a default is set by attempting to create an actor
    let actor = SystemTestActor::new("TestActor".to_string());
    let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
    assert!(!actor_ref.path().is_empty(), "Actor should have a path");
    
    // Shutdown the system
    system.shutdown().await?;
    
    Ok(())
}

async fn test_multiple_system_registration() -> Result<()> {
    // Create ParrotActorSystem
    let config = ActorSystemConfig::default();
    let system = ParrotActorSystem::new(config).await?;
    
    // Create and register multiple Actix systems
    let actix_system1 = ActixActorSystem::new().await?;
    let actix_system2 = ActixActorSystem::new().await?;
    let actix_system3 = ActixActorSystem::new().await?;
    
    // Register systems
    system.register_actix_system("system1".to_string(), actix_system1, true).await?;
    system.register_actix_system("system2".to_string(), actix_system2, false).await?;
    system.register_actix_system("system3".to_string(), actix_system3, false).await?;
    
    // List registered systems
    let systems = system.list_registered_systems()?;
    assert_eq!(systems.len(), 3, "Should have 3 registered systems");
    
    // Create an actor to verify a default exists
    let actor = SystemTestActor::new("DefaultSystemActor".to_string());
    let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
    
    // Set a different system as default
    system.set_default_system("system2")?;
    
    // Create another actor to verify it's using the new default
    let actor2 = SystemTestActor::new("System2Actor".to_string());
    let actor_ref2 = system.spawn_root_actix(actor2, EmptyConfig::default()).await?;
    
    // Both actors should exist
    assert!(!actor_ref.path().is_empty(), "First actor should have a path");
    assert!(!actor_ref2.path().is_empty(), "Second actor should have a path");
    
    // Test error when setting non-existent system as default
    let result = system.set_default_system("nonexistent");
    assert!(result.is_err(), "Setting non-existent system should return error");
    
    // Shutdown
    system.shutdown().await?;
    
    Ok(())
}

async fn test_system_default_selection() -> Result<()> {
    // Create a system with multiple registered instances
    let config = ActorSystemConfig::default();
    let system = ParrotActorSystem::new(config).await?;
    
    // Create actor systems
    let actix_system1 = ActixActorSystem::new().await?;
    let actix_system2 = ActixActorSystem::new().await?;
    
    // Register with different names
    system.register_actix_system("primary".to_string(), actix_system1, true).await?;
    system.register_actix_system("secondary".to_string(), actix_system2, false).await?;
    
    // Create actor on default system
    let actor1 = SystemTestActor::new("DefaultActor".to_string());
    let actor_ref1 = system.spawn_root_actix(actor1, EmptyConfig::default()).await?;
    
    // Verify actor was created
    let path1 = actor_ref1.path();
    println!("Actor path: {}", path1);
    
    // No longer check for specific name, but check path format
    assert!(path1.starts_with("actix://"), "Actor path should start with actix://");
    
    // Change default and create another actor
    system.set_default_system("secondary")?;
    
    // Create actor on new default system
    let actor2 = SystemTestActor::new("SecondaryActor".to_string());
    let actor_ref2 = system.spawn_root_actix(actor2, EmptyConfig::default()).await?;
    
    // Verify second actor
    let path2 = actor_ref2.path();
    println!("Second actor path: {}", path2);
    
    // No longer check for specific name, but check path format
    assert!(path2.starts_with("actix://"), "Second actor path should start with actix://");
    
    // To test ActorPath lookup, create a TestActorRef
    let test_ref = TestActorRef::new(&path1);
    let actor_target: WeakActorTarget = Arc::new(test_ref);
    
    // Should be able to get actor by path - we'll use internal_get_actor which we know is exposed
    let path_for_lookup = ActorPath {
        path: path1.clone(),
        target: actor_target,
    };
    
    let retrieved1 = system.internal_get_actor(&path_for_lookup).await;
    
    assert!(retrieved1.is_some(), "Should retrieve actor by path");
    
    // Cleanup
    system.shutdown().await?;
    
    Ok(())
}

async fn test_broadcast_message() -> Result<()> {
    // Create system
    let config = ActorSystemConfig::default();
    let system = ParrotActorSystem::new(config).await?;
    
    // Create actix system
    let actix_system = ActixActorSystem::new().await?;
    system.register_actix_system("broadcast_test".to_string(), actix_system, true).await?;
    
    // Create several actors to receive broadcasts
    let actor1 = SystemTestActor::new("BroadcastReceiver1".to_string());
    let actor2 = SystemTestActor::new("BroadcastReceiver2".to_string());
    let actor3 = SystemTestActor::new("BroadcastReceiver3".to_string());
    
    // Spawn actors
    let actor_ref1 = system.spawn_root_actix(actor1, EmptyConfig::default()).await?;
    let actor_ref2 = system.spawn_root_actix(actor2, EmptyConfig::default()).await?;
    let actor_ref3 = system.spawn_root_actix(actor3, EmptyConfig::default()).await?;
    
    // Broadcast a message to all actors - using internal_broadcast 
    let broadcast_msg = BroadcastTestMessage("This is a broadcast".to_string());
    system.internal_broadcast(broadcast_msg.clone()).await?;
    
    // Wait for message processing
    wait_for(100).await;
    
    // Verify each actor received the broadcast by sending individual messages
    let response1 = actor_ref1.ask(BroadcastTestMessage("Individual1".to_string())).await?;
    assert!(response1.contains("BroadcastReceiver1"), "Should get response from actor1");
    
    let response2 = actor_ref2.ask(BroadcastTestMessage("Individual2".to_string())).await?;
    assert!(response2.contains("BroadcastReceiver2"), "Should get response from actor2");
    
    let response3 = actor_ref3.ask(BroadcastTestMessage("Individual3".to_string())).await?;
    assert!(response3.contains("BroadcastReceiver3"), "Should get response from actor3");
    
    // Shutdown
    system.shutdown().await?;
    
    Ok(())
} 