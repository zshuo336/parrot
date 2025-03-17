// Actor Ring Implementation
// Function: Create a ring of actors, implement message passing, accumulation, and sequential exit
// Process: A→B→C→D→E→F→A forms a ring, initial message starts from A, each actor adds 1 to the value and passes it
//          After completing the ring cycle, sequential exit process is initiated, eventually the whole system safely exits

use std::time::Duration;
use parrot::actix::{ActixActorSystem, ActixActor, ActixContext};
use parrot::system::ParrotActorSystem;
use parrot_api::match_message;
use parrot_api::{
    actor::{ActorState, EmptyConfig},
    message::Message,
    types::{BoxedMessage, ActorResult, BoxedActorRef},
    system::{ActorSystemConfig, ActorSystem},
    address::ActorRefExt,
    errors::ActorError,
};
use actix::{AsyncContext, Context as ActixBaseContext, Actor as ActixBaseTrait};
use parrot_api_derive::{Message, ParrotActor};
use std::any::Any;
use std::ptr::NonNull;
use tokio::signal;

//----------------------------------------------------------------------
// Message Type Definitions
//----------------------------------------------------------------------

// NextActorRef: Used to pass the reference to the next actor, establishing the ring structure
#[derive(Debug, Message)]
#[message(result = "()")]
struct NextActorRef {
    next_ref: BoxedActorRef,
}

// AddMessage: Accumulation message, contains the current accumulated value
// Each actor receives it, adds 1 to the value and sends it to the next actor
#[derive(Debug, Clone, Message)]
#[message(result = "u32")]
struct AddMessage {
    value: u32,
}

// ExitMessage: Exit message, used to trigger sequential actor exit
// Starts from A sending to B, then B sends to C, and so on
#[derive(Debug, Clone, Message)]
#[message(result = "()")]
struct ExitMessage;

//----------------------------------------------------------------------
// Actor Implementation
//----------------------------------------------------------------------

// RingActor: Node actor in the ring
// Handles three types of messages: NextActorRef, AddMessage and ExitMessage
#[derive(Debug, ParrotActor)]
#[ParrotActor(engine = "actix")]
struct RingActor {
    name: String,           // Actor name (A-F)
    next: Option<BoxedActorRef>,  // Reference to the next actor
    state: ActorState,      // Current state of the actor
}

impl RingActor {
    // Create a new RingActor instance
    pub fn new(name: String) -> Self {
        Self {
            name,
            next: None,
            state: ActorState::Starting,
        }
    }

    fn handle_message(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>) -> ActorResult<BoxedMessage> {
        Err(ActorError::Other(anyhow::anyhow!("Actor {} received message: {:?}", self.name, msg)))
    }

    // Message handling method: processes three types of messages
    // Uses match_message! macro for message dispatch, ensuring type safety
    fn handle_message_engine(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>, engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        let actix_ctx: Option<&ActixBaseContext<ActixActor<Self>>> = unsafe { engine_ctx.as_ref().downcast_ref::<ActixBaseContext<ActixActor<Self>>>() };
        assert!(actix_ctx.is_some());
        match_message!("option", self, msg,
            // Handle NextActorRef message: receive reference to the next actor, establish ring connection
            NextActorRef => |actor: &mut Self, next_ref: &NextActorRef| {
                println!("Actor {} received next actor reference", actor.name);
                actor.next = Some(next_ref.next_ref.clone_boxed());
                ()
            },
            
            // Handle AddMessage: increment value and pass to the next actor
            AddMessage => |actor: &mut Self, add_msg: &AddMessage| {
                let new_value = add_msg.value + 1;
                println!("Actor {} received AddMessage with value {}, sending {} to next", 
                         actor.name, add_msg.value, new_value);
                
                // If this is A (first actor), and received message from F (completed a cycle)
                if actor.name == "A" && add_msg.value > 1 {
                    println!("Ring completed! Final value: {}", new_value);
                    
                    // Send exit message to the next actor to start exit process
                    if let Some(next) = &actor.next {
                        let next_ref = next.clone_boxed();
                        tokio::spawn(async move {
                            next_ref.ask(ExitMessage).await.unwrap();
                        });
                    }
                } else if let Some(next) = &actor.next {
                    // Send incremented message to the next actor
                    let next_ref = next.clone_boxed();
                    let next_value = new_value;  // Clone the value, not the reference
                    tokio::spawn(async move {
                        next_ref.ask(AddMessage { value: next_value }).await.unwrap();
                    });
                }
                
                new_value
            },
            
            // Handle ExitMessage: trigger sequential exit process
            ExitMessage => |actor: &mut Self, _: &ExitMessage| {
                println!("Actor {} received exit message", actor.name);
                
                // If this is A, the entire ring has completed exit, shut down the system
                if actor.name == "A" {
                    println!("Actor A received exit message, full ring shutdown completed");
                    // Asynchronously shut down the system
                    tokio::spawn(async {
                        // Give some time for logs to be output
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        // Normal program exit
                        std::process::exit(0);
                    });
                } else if let Some(next) = &actor.next {
                    // Forward exit message to the next actor
                    let next_ref = next.clone_boxed();
                    tokio::spawn(async move {
                        next_ref.ask(ExitMessage).await.unwrap();
                    });
                    
                    // Current actor will enter Stopping state
                    actor.state = ActorState::Stopping;
                }
                
                ()
            }
        )
    }
    
    // Add state method for the actor system to use
    fn state(&self) -> ActorState {
        self.state
    }
}

// Manually implement Clone for RingActor
impl Clone for RingActor {
    fn clone(&self) -> Self {
        RingActor {
            name: self.name.clone(),
            next: self.next.as_ref().map(|n| n.clone_boxed()),
            state: self.state.clone(),
        }
    }
}

//----------------------------------------------------------------------
// Main Program Entry
//----------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use actix::System as the application runtime environment
    actix::System::new().block_on(async {
        run_ring_example().await
    })
}

// Async function to run the ring actor example
async fn run_ring_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting Actor Ring example");
    
    //----------------------------------------------------------------------
    // System Initialization
    //----------------------------------------------------------------------
    
    // Create global Actor system
    let config = ActorSystemConfig::default();
    let system = ParrotActorSystem::new(config).await?;

    // Create and register ActixActorSystem
    let actix_system = ActixActorSystem::new().await?;
    system.register_actix_system("actix".to_string(), actix_system, true).await?;

    //----------------------------------------------------------------------
    // Create Actors and Add to System
    //----------------------------------------------------------------------
    
    // Create 6 actors: A, B, C, D, E, F
    let actor_a = RingActor::new("A".to_string());
    let actor_b = RingActor::new("B".to_string());
    let actor_c = RingActor::new("C".to_string());
    let actor_d = RingActor::new("D".to_string());
    let actor_e = RingActor::new("E".to_string());
    let actor_f = RingActor::new("F".to_string());
    
    // Add actors to the system, get their references
    let a_ref = system.spawn_root_actix(actor_a, EmptyConfig::default()).await?;
    let b_ref = system.spawn_root_actix(actor_b, EmptyConfig::default()).await?;
    let c_ref = system.spawn_root_actix(actor_c, EmptyConfig::default()).await?;
    let d_ref = system.spawn_root_actix(actor_d, EmptyConfig::default()).await?;
    let e_ref = system.spawn_root_actix(actor_e, EmptyConfig::default()).await?;
    let f_ref = system.spawn_root_actix(actor_f, EmptyConfig::default()).await?;
    
    println!("All actors spawned");
    
    //----------------------------------------------------------------------
    // Establish Ring Connection
    //----------------------------------------------------------------------
    
    // Establish ring connection: A -> B -> C -> D -> E -> F -> A
    println!("Building the ring: A -> B -> C -> D -> E -> F -> A");
    
    // Send "next actor reference" messages to establish the ring structure
    a_ref.ask(NextActorRef { next_ref: b_ref.clone_boxed() }).await?;
    b_ref.ask(NextActorRef { next_ref: c_ref.clone_boxed() }).await?;
    c_ref.ask(NextActorRef { next_ref: d_ref.clone_boxed() }).await?;
    d_ref.ask(NextActorRef { next_ref: e_ref.clone_boxed() }).await?;
    e_ref.ask(NextActorRef { next_ref: f_ref.clone_boxed() }).await?;
    f_ref.ask(NextActorRef { next_ref: a_ref.clone_boxed() }).await?;
    
    println!("Ring setup completed");
    
    // Give the system some time to process messages
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    //----------------------------------------------------------------------
    // Start Message Passing
    //----------------------------------------------------------------------
    
    // Send initial accumulation message to A with value 1
    // This will start the entire message passing process:
    // 1. Each actor receives the message, adds 1 to the value and sends it to the next
    // 2. When A receives a message from F, it prints the final value and starts the exit process
    // 3. Exit process: A sends exit message to B, B sends to C, and so on
    // 4. When A receives exit message from F, the entire system safely exits
    println!("Starting with AddMessage(1) to Actor A");
    let result = a_ref.ask(AddMessage { value: 1 }).await?;
    println!("Initial response from Actor A: {}", result);
    
    // Wait for the system to naturally exit
    // Note: The program will exit after Actor A receives the exit message, no need to actively close the system here
    println!("Waiting for ring to process messages and exit");
    
    // Wait longer to give messages enough time to propagate, or wait for user to interrupt the program
    println!("Press Ctrl+C to exit manually if needed...");
    
    // Two ways to end the program:
    // 1. Wait for 30 seconds - should be enough for messages to complete
    // 2. Wait for user to press Ctrl+C to exit actively
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(30)) => {
            println!("Timeout reached, exiting...");
        }
        _ = signal::ctrl_c() => {
            println!("Received Ctrl+C, shutting down...");
        }
    }
    
    Ok(())
}
