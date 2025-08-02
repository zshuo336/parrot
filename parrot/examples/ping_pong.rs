use std::time::Duration;
use parrot::actix::{ActixActorSystem, ActixActor, ActixContext};
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{ActorState, EmptyConfig},
    message::Message,
    errors::ActorError,
    types::{BoxedMessage, ActorResult},
    system::{ActorSystemConfig, ActorSystem},
    address::ActorRefExt,
};
use parrot_api_derive::{Message, ParrotActor};
use std::any::Any;
use std::ptr::NonNull;

// Engine selection constants
pub const ACTIX: &str = "actix";

// Define messages
#[derive(Debug, Clone, Message)]
#[message(result = "u32")]
struct Ping(u32);

#[derive(Debug, Clone, Message)]
#[message(result = "String")]
struct Pong(u32);

// Define actors - showing different ways to set engine
#[derive(Debug, Clone, ParrotActor)]
#[ParrotActor(engine = "actix")]
struct PingActor {
    count: u32,
    state: ActorState,
}

#[derive(Debug, Clone, ParrotActor)]
#[ParrotActor(engine = ACTIX)]
struct PongActor {
    count: u32,
    state: ActorState,
}

impl PingActor {
    pub fn new() -> Self {
        Self {
            count: 0,
            state: ActorState::Starting,
        }
    }

    fn handle_message_engine(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>, _engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        if let Some(ping) = msg.downcast_ref::<Ping>() {
            println!("PingActor received Ping({})", ping.0);
            self.count += 1;
            return Some(Ok(Box::new(self.count) as BoxedMessage));
        }
        
        if let Some(pong) = msg.downcast_ref::<Pong>() {
            let response = format!("PingActor got Pong({})", pong.0);
            return Some(Ok(Box::new(response) as BoxedMessage));
        }
        
        Some(Err(ActorError::MessageHandlingError("Unknown message type".to_string())))
    }
}

impl PongActor {
    pub fn new() -> Self {
        Self {
            count: 0,
            state: ActorState::Starting,
        }
    }

    fn handle_message_engine(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>, _engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        if let Some(ping) = msg.downcast_ref::<Ping>() {
            println!("PongActor received Ping({})", ping.0);
            self.count += 1;
            return Some(Ok(Box::new(self.count) as BoxedMessage));
        }
        
        if let Some(pong) = msg.downcast_ref::<Pong>() {
            let response = format!("PongActor got Pong({})", pong.0);
            return Some(Ok(Box::new(response) as BoxedMessage));
        }
        
        Some(Err(ActorError::MessageHandlingError("Unknown message type".to_string())))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // use actix::System to wrap our application
    actix::System::new().block_on(async {
        run_example().await
    })
}

async fn run_example() -> Result<(), Box<dyn std::error::Error>> {
    // Create global actor system
    let config = ActorSystemConfig::default();
    let system = ParrotActorSystem::new(config).await?;

    // Create and register ActixActorSystem
    let actix_system = ActixActorSystem::new().await?;
    system.register_actix_system("actix".to_string(), actix_system, true).await?;

    // Create actors
    let ping_actor = PingActor::new();
    let pong_actor = PongActor::new();

    // Spawn actors using global system
    let ping_ref = system.spawn_root_actix(ping_actor, EmptyConfig::default()).await?;
    let pong_ref = system.spawn_root_actix(pong_actor, EmptyConfig::default()).await?;

    // Send messages
    for i in 0..5 {
        let ping_result = ping_ref.ask(Ping(i)).await?;
        println!("PingActor response: {}", ping_result);
        
        let pong_result = pong_ref.ask(Ping(i)).await?;
        println!("PongActor response: {}", pong_result);
        
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Wait for messages to be processed
    tokio::time::sleep(Duration::from_secs(1)).await;
    
    println!("Before system shutdown");
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    // Shutdown system (use ActorSystem trait method)
    println!("Calling system shutdown");
    ActorSystem::shutdown(system).await?;
    
    println!("After system shutdown");
    tokio::time::sleep(Duration::from_secs(1)).await;
    println!("Exiting");

    Ok(())
}