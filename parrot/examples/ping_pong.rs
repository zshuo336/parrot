use std::time::Duration;
use actix::Actor as ActixActor;
use parrot::actix::{
    ActixActorSystem, ActixActorWrapper, ActixActorConfig,
    MessageConverter, ActixMessageHandler,
};
use parrot_api::{
    actor::Actor,
    message::Message,
    errors::ActorError,
    system::ActorSystemConfig,
};

// Define messages
#[derive(Debug, Clone)]
struct Ping(u32);

impl Message for Ping {
    type Result = Pong;
}

#[derive(Debug, Clone)]
struct Pong(u32);

impl Message for Pong {
    type Result = ();
}

// Implement message conversion
impl MessageConverter for Ping {
    type ActixMessage = Ping;
    type ParrotResult = Pong;

    fn to_actix(&self) -> Self::ActixMessage {
        self.clone()
    }

    fn from_actix_result(result: <Self::ActixMessage as actix::Message>::Result) -> Self::ParrotResult {
        result
    }
}

impl MessageConverter for Pong {
    type ActixMessage = Pong;
    type ParrotResult = ();

    fn to_actix(&self) -> Self::ActixMessage {
        self.clone()
    }

    fn from_actix_result(result: <Self::ActixMessage as actix::Message>::Result) -> Self::ParrotResult {
        result
    }
}

// Define actors
struct PingActor {
    count: u32,
}

impl ActixActor for PingActor {
    type Context = actix::Context<Self>;
}

impl ActixMessageHandler<Ping> for PingActor {
    fn handle_parrot_message(&mut self, msg: Ping) -> Result<Pong, ActorError> {
        println!("PingActor received Ping({})", msg.0);
        self.count += 1;
        Ok(Pong(msg.0 + 1))
    }
}

struct PongActor {
    count: u32,
}

impl ActixActor for PongActor {
    type Context = actix::Context<Self>;
}

impl ActixMessageHandler<Pong> for PongActor {
    fn handle_parrot_message(&mut self, msg: Pong) -> Result<(), ActorError> {
        println!("PongActor received Pong({})", msg.0);
        self.count += 1;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create actor system
    let system = ActixActorSystem::start(ActorSystemConfig::default()).await?;

    // Create actors
    let ping_actor = PingActor { count: 0 };
    let pong_actor = PongActor { count: 0 };

    // Wrap actors
    let wrapped_ping = ActixActorWrapper::new(ping_actor);
    let wrapped_pong = ActixActorWrapper::new(pong_actor);

    // Start actors
    let ping_ref = system.spawn_root_typed(wrapped_ping, ActixActorConfig::default()).await?;
    let pong_ref = system.spawn_root_typed(wrapped_pong, ActixActorConfig::default()).await?;

    // Send messages
    for i in 0..5 {
        ping_ref.send(Ping(i).into()).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Wait for messages to be processed
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Shutdown system
    system.shutdown().await?;

    Ok(())
}