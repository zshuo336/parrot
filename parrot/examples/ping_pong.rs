use std::time::Duration;
use parrot::actix::{ActixActorSystem, ActorBase, IntoActorBase};
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::Actor,
    message::Message,
    errors::ActorError,
    types::{BoxedMessage, ActorResult, BoxedFuture},
    system::ActorSystemConfig,
};
use parrot_api_derive::{Message, ParrotActor};

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
}

#[derive(Debug, Clone, ParrotActor)]
#[ParrotActor(engine = ACTIX)]
struct PongActor {
    count: u32,
}

impl PingActor {
    async fn handle_message<M: Message>(&mut self, msg: M, ctx: &mut parrot::actix::ActixContext<Self>) 
        -> ActorResult<M::Result> {
        
        if let Some(ping) = msg.downcast_ref::<Ping>() {
                println!("PingActor received Ping({})", ping.0);
            self.count += 1;
            return Ok(Box::new(self.count) as Box<dyn std::any::Any + Send>);
        }
        
        if let Some(pong) = msg.downcast_ref::<Pong>() {
            return Ok(Box::new(format!("PingActor got Pong({})", pong.0)) as Box<dyn std::any::Any + Send>);
        }
        
        Err(ActorError::MessageHandlingError("Unknown message type".to_string()))
    }
}

impl PongActor {
    async fn handle_message<M: Message>(&mut self, msg: M, ctx: &mut parrot::actix::ActixContext<Self>) 
        -> ActorResult<M::Result> {
        
        if let Some(ping) = msg.downcast_ref::<Ping>() {
            println!("PongActor received Ping({})", ping.0);
            self.count += 1;
            return Ok(Box::new(self.count) as Box<dyn std::any::Any + Send>);
        }
        
        if let Some(pong) = msg.downcast_ref::<Pong>() {
            return Ok(Box::new(format!("PongActor got Pong({})", pong.0)) as Box<dyn std::any::Any + Send>);
        }
        
        Err(ActorError::MessageHandlingError("Unknown message type".to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create global actor system
    let system = ParrotActorSystem::start(ActorSystemConfig::default()).await?;

    // Create and register ActixActorSystem
    let actix_system = ActixActorSystem::new().await?;
    system.register_actix_system("actix".to_string(), actix_system, true).await?;

    // Create actors
    let ping_actor = PingActor { count: 0 };
    let pong_actor = PongActor { count: 0 };

    // Spawn actors using global system
    let ping_ref = system.spawn_root_typed(ping_actor, ()).await?;
    let pong_ref = system.spawn_root_typed(pong_actor, ()).await?;

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

    // Shutdown system
    system.shutdown().await?;

    Ok(())
}