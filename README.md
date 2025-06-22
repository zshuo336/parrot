# Parrot Actor Framework

Parrot is a flexible, modular actor framework for Rust that provides a unified API over different actor system implementations.

## Features

- **Unified API**: Use the same core API regardless of the underlying actor system
- **Pluggable Backends**: Currently supports Actix with ability to add more backends
- **Type-Safe**: Strong typing for actors and messages
- **Flexible**: Configure with different supervision strategies and message handling approaches

## Usage

### Define Messages

```rust
use parrot_api::message::Message;
use parrot_api_derive::Message;

#[derive(Message, Debug, Clone)]
#[message(result = "u32")]
struct Ping(u32);

#[derive(Message, Debug, Clone)]
#[message(result = "String")]
struct Pong(u32);
```

### Define Actors

```rust
use parrot_api::actor::Actor;
use parrot_api::types::{BoxedMessage, ActorResult};
use parrot_api_derive::ParrotActor;

#[derive(ParrotActor)]
#[engine = "actix"]
struct MyActor {
    count: u32,
}

impl MyActor {
    // Message handler method called by receive_message
    async fn handle_message<M: Message>(&mut self, msg: M, ctx: &mut ActixContext) 
        -> ActorResult<M::Result> {
        if let Some(ping) = msg.downcast_ref::<Ping>() {
            println!("Received Ping({})", ping.0);
            self.count += 1;
            return Ok(self.count);
        }
        
        if let Some(pong) = msg.downcast_ref::<Pong>() {
            return Ok(format!("Pong: {}", pong.0));
        }
        
        Err(ActorError::UnknownMessage)
    }
}
```

### Start the Actor System and Create Actors

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create ParrotActorSystem
    let system = ParrotActorSystem::start(ActorSystemConfig::default()).await?;
    
    // Create and register ActixActorSystem
    let actix_system = ActixActorSystem::new().await?;
    system.register_actix_system("actix", actix_system, true).await?;
    
    // Create actor
    let ping_actor = MyActor { count: 0 };
    let actor_ref = system.spawn_root_typed(ping_actor, ()).await?;
    
    // Send messages
    for i in 0..5 {
        let result = actor_ref.send(Ping(i)).await?;
        println!("Response: {}", result);
    }
    
    // Shutdown system
    system.shutdown().await?;
    
    Ok(())
}
```

## Architecture

The framework is organized into these primary components:

1. `parrot-api`: Core traits and interfaces
2. `parrot-api-derive`: Procedural macros for code generation
3. `parrot`: Implementation of different actor system backends

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request. 