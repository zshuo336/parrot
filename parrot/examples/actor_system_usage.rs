use parrot::{
    system::ParrotActorSystem,
    actix::{ActixActorSystem, ActixContext, ActixActor},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    system::{ActorSystem, ActorSystemConfig},
    message::Message,
    types::{BoxedMessage, ActorResult, BoxedFuture},
    address::{ActorRef, ActorRefExt},
};
use std::time::Duration;
use std::ptr::NonNull;
use std::any::Any;
use actix::{AsyncContext, Context as ActixBaseContext, Actor as ActixBaseTrait};
use anyhow::Result;

// Simple actor implementation using Actix context (not default)
struct SimpleActor {
    state: ActorState,
    name: String,
}

impl SimpleActor {
    fn new(name: String) -> Self {
        Self {
            state: ActorState::Starting,
            name,
        }
    }
}

impl Actor for SimpleActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        println!("SimpleActor '{}' received message", self.name);
        Box::pin(async move {
            // Return the same message as a response
            Ok(msg)
        })
    }
    
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine_ctx: NonNull<dyn Any>
    ) -> Option<ActorResult<BoxedMessage>> {
        // Add processing logic to avoid returning None
        println!("SimpleActor '{}' processing message via engine", self.name);
        
        if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
            println!("Got TestMessage: {}", test_msg.0);
            let response = Box::new(TestMessage(format!("SimpleActor {} received: {}", 
                self.name, test_msg.0))) as BoxedMessage;
            Some(Ok(response))
        } else {
            // Return generic response
            Some(Ok(Box::new(TestMessage("Unknown message processed".to_string())) as BoxedMessage))
        }
    }
    
    fn state(&self) -> ActorState {
        self.state
    }
}

// Actix-compatible actor implementation
struct ActixCompatibleActor {
    state: ActorState,
    name: String,
}

impl ActixCompatibleActor {
    fn new(name: String) -> Self {
        Self {
            state: ActorState::Starting,
            name,
        }
    }
}

impl Actor for ActixCompatibleActor {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        println!("ActixCompatibleActor '{}' received message", self.name);
        Box::pin(async move {
            // Return the same message as a response
            Ok(msg)
        })
    }
    
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
        _engine_ctx: NonNull<dyn Any>
    ) -> Option<ActorResult<BoxedMessage>> {
        // Properly handle messages and return results
        println!("ActixCompatibleActor '{}' received message via engine", self.name);
        
        // Check if it's a TestMessage type
        if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
            println!("Processing TestMessage: {}", test_msg.0);
            
            // Return response
            let response = Box::new(TestMessage(format!("Response from {}: {}", self.name, test_msg.0))) as BoxedMessage;
            Some(Ok(response))
        } else {
            // For unknown messages, still return the original message
            Some(Ok(msg))
        }
    }
    
    fn state(&self) -> ActorState {
        self.state
    }
}

// Test message
#[derive(Clone)]
struct TestMessage(String);

impl Message for TestMessage {
    type Result = String;
    
    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        // Directly try to cast to TestMessage or String
        match result.downcast::<TestMessage>() {
            Ok(msg) => return Ok(msg.0.clone()),
            Err(other_result) => {
                match other_result.downcast::<String>() {
                    Ok(string) => return Ok(*string),
                    Err(_) => Err(parrot_api::errors::ActorError::MessageHandlingError(
                        "Failed to extract result - unknown response type".to_string(),
                    )),
                }
            }
        }
    }
}

// Actor capable of spawning child actors
struct ActixContextSpawner {
    state: ActorState,
    name: String,
    child_count: usize,
}

impl ActixContextSpawner {
    fn new(name: String) -> Self {
        Self {
            state: ActorState::Starting,
            name,
            child_count: 0,
        }
    }
}

impl Actor for ActixContextSpawner {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        println!("ActixContextSpawner '{}' received message", self.name);
        Box::pin(async move {
            // Cannot spawn here
            Ok(Box::new("Cannot spawn in async handler".to_string()) as BoxedMessage)
        })
    }
    
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
        engine_ctx: NonNull<dyn Any>
    ) -> Option<ActorResult<BoxedMessage>> {
        println!("ActixContextSpawner '{}' received message via engine", self.name);
        
        // Check if received SpawnChildMessage
        if let Some(spawn_msg) = msg.downcast_ref::<SpawnChildMessage>() {
            println!("Received request to spawn child actor: {}", spawn_msg.0);
            
            // Try to convert engine_ctx to actix::Context<ActixActor<Self>>
            let actix_ctx: Option<&ActixBaseContext<ActixActor<ActixContextSpawner>>> = unsafe { engine_ctx.as_ref().downcast_ref::<actix::Context<ActixActor<Self>>>() };
            
            if let Some(actix_context) = actix_ctx {
                // Create child actor
                self.child_count += 1;
                let child_name = format!("{}_child_{}", self.name, self.child_count);
                let child_actor = SimpleActor::new(child_name.clone());
                
                // Get current actor address - verify context conversion success
                let addr = actix_context.address();
                println!("Current actor address from context: {:?}", addr);
                
                // Create and start child actor
                let child_actor_base = ActixActor::new(child_actor);
                let child_addr = actix::Actor::start(child_actor_base);
                
                // Generate actor path
                let path = format!("actix://{:?}", child_addr.clone());
                println!("Successfully spawned child actor at path: {}", path);
                
                Some(Ok(Box::new(format!("Spawned child '{}' after validating engine_ctx", child_name)) as BoxedMessage))
            } else {
                println!("Failed to downcast engine_ctx to actix::Context");
                Some(Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "Failed to downcast engine_ctx to actix::Context".to_string()
                )))
            }
        } else if let Some(another_child_msg) = msg.downcast_ref::<ActixAnotherChildMessage>() {
            println!("Received request to create another type of child: {}", another_child_msg.0);
            
            // Try again to convert to actix::Context
            let actix_ctx = unsafe { engine_ctx.as_ref().downcast_ref::<actix::Context<ActixActor<Self>>>() };
            
            if let Some(actix_context) = actix_ctx {
                // Get current actor address - verify context is usable
                let addr = actix_context.address();
                println!("Current actor address from context: {:?}", addr);
                
                // Create child actor
                self.child_count += 1;
                let child_name = format!("{}_another_child_{}", self.name, self.child_count);
                let child_actor = SimpleActor::new(child_name.clone());
                
                // Use ActixActor wrapper and start directly
                let child_actor_base = ActixActor::new(child_actor);
                let child_addr = actix::Actor::start(child_actor_base);
                
                // Generate actor path
                let path = format!("actix://{:?}", child_addr.clone());
                println!("Successfully spawned another child actor at path: {}", path);
                
                Some(Ok(Box::new(format!("Created another child '{}' using engine_ctx", 
                    child_name)) as BoxedMessage))
            } else {
                println!("Failed to downcast engine_ctx to actix::Context");
                Some(Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "Failed to downcast engine_ctx to actix::Context".to_string()
                )))
            }
        } else if let Some(test_msg) = msg.downcast_ref::<TestMessage>() {
            // Normal message processing
            println!("Processing TestMessage: {}", test_msg.0);
            Some(Ok(Box::new(format!("ActixContextSpawner received: {}", test_msg.0)) as BoxedMessage))
        } else {
            // Unknown message
            Some(Err(parrot_api::errors::ActorError::MessageHandlingError(
                "Unknown message type".to_string()
            )))
        }
    }
    
    fn state(&self) -> ActorState {
        self.state
    }
}

// Specific message to trigger spawn
#[derive(Debug, Clone)]
struct SpawnChildMessage(String);

impl Message for SpawnChildMessage {
    type Result = String;
    
    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        // Directly try to cast to String
        if let Ok(string) = result.downcast::<String>() {
            return Ok(*string);
        }
        
        Err(parrot_api::errors::ActorError::MessageHandlingError(
            "Failed to extract result - expected String".to_string()
        ))
    }
}

// ActixAnotherChildMessage for creating another type of child actor
#[derive(Debug, Clone)]
struct ActixAnotherChildMessage(String);

impl Message for ActixAnotherChildMessage {
    type Result = String;
    
    fn extract_result(result: BoxedMessage) -> ActorResult<Self::Result> {
        // Directly try to cast to String
        if let Ok(string) = result.downcast::<String>() {
            return Ok(*string);
        }
        
        Err(parrot_api::errors::ActorError::MessageHandlingError(
            "Failed to extract result - expected String".to_string()
        ))
    }
}

fn main() -> Result<()> {
    // Use actix::System to wrap our application
    actix::System::new().block_on(async {
        run_example().await
    })
}

async fn run_example() -> Result<()> {
    // Create system configuration
    let config = ActorSystemConfig::default();
    
    // Create Parrot actor system
    let system = ParrotActorSystem::new(config).await?;
    
    // Create and register the Actix system
    let actix_system = ActixActorSystem::new().await?;
    system.register_actix_system("actix".to_string(), actix_system, true).await?;
    
    // Create SimpleActor using spawn_root_actix method
    let simple_actor = SimpleActor::new("simple".to_string());
    match system.spawn_root_actix(simple_actor, EmptyConfig::default()).await {
        Ok(actor_ref) => {
            println!("Successfully created simple actor: {}", actor_ref.path());
            
            // Use ask to get response
            println!("Sending message with ask...");
            match actor_ref.ask(TestMessage("Hello Simple with ask".to_string())).await {
                Ok(response) => println!("Received response from simple actor: {}", response),
                Err(e) => println!("Failed to get response from simple actor: {}", e),
            }
            
            // Use send to send message (no response wait)
            println!("Sending message with send...");
            let msg = Box::new(TestMessage("Hello Simple with send".to_string())) as BoxedMessage;
            match actor_ref.send(msg).await {
                Ok(_) => println!("Message sent successfully to simple actor"),
                Err(e) => println!("Failed to send message to simple actor: {}", e),
            }
        },
        Err(e) => println!("Failed to create simple actor: {}", e),
    }
    
    // Create Actix-compatible actor using the specific spawn_root_actix method
    let actix_actor = ActixCompatibleActor::new("actix-compatible".to_string());
    match system.spawn_root_actix(actix_actor, EmptyConfig::default()).await {
        Ok(actor_ref) => {
            println!("Successfully created Actix-compatible actor: {}", actor_ref.path());
            
            // Use ask to get response
            println!("Sending message with ask to Actix actor...");
            match actor_ref.ask(TestMessage("Hello Actix with ask".to_string())).await {
                Ok(response) => println!("Received response from Actix actor: {}", response),
                Err(e) => println!("Failed to get response from Actix actor: {}", e),
            }
            
            // Use send to send message
            println!("Sending message with send to Actix actor...");
            let msg = Box::new(TestMessage("Hello Actix with send".to_string())) as BoxedMessage;
            match actor_ref.send(msg).await {
                Ok(_) => println!("Message sent successfully to Actix actor"),
                Err(e) => println!("Failed to send message to Actix actor: {}", e),
            }
        },
        Err(e) => println!("Failed to create Actix-compatible actor: {}", e),
    }
    
    // Create ActixContextSpawner capable of spawning child actors
    println!("\n=== Testing ActixContextSpawner (spawning child actors) ===");
    let spawner_actor = ActixContextSpawner::new("main-spawner".to_string());
    match system.spawn_root_actix(spawner_actor, EmptyConfig::default()).await {
        Ok(actor_ref) => {
            println!("Successfully created spawner actor: {}", actor_ref.path());
            
            // Send SpawnChildMessage to trigger child actor creation
            println!("Requesting to spawn a child actor...");
            match actor_ref.ask(SpawnChildMessage("test-child".to_string())).await {
                Ok(response) => println!("Spawn response: {}", response),
                Err(e) => println!("Failed to spawn child: {}", e),
            }
            
            // Send ActixAnotherChildMessage to trigger another type of child actor creation
            println!("Requesting to create another child actor type...");
            match actor_ref.ask(ActixAnotherChildMessage("another-test-child".to_string())).await {
                Ok(response) => println!("Another child spawn response: {}", response),
                Err(e) => println!("Failed to spawn another child: {}", e),
            }
            
            // Send a second SpawnChildMessage to trigger another child actor creation
            println!("Requesting to spawn another child actor...");
            match actor_ref.ask(SpawnChildMessage("another-child".to_string())).await {
                Ok(response) => println!("Spawn response: {}", response),
                Err(e) => println!("Failed to spawn child: {}", e),
            }
            
            // Send regular message for testing
            match actor_ref.ask(TestMessage("Test message to spawner".to_string())).await {
                Ok(response) => println!("Regular message response: {}", response),
                Err(e) => println!("Failed to get response: {}", e),
            }
        },
        Err(e) => println!("Failed to create spawner actor: {}", e),
    }
    
    // Wait for a moment to observe the output
    tokio::time::sleep(Duration::from_secs(1)).await;
    
    // Shutdown the system
    system.shutdown().await?;
    
    Ok(())
}