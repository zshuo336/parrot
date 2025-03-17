// Parrot Actor Asynchronous Message Handling Example
// This example demonstrates two ways to handle asynchronous messages in the Parrot Actor framework:
// 1. Spawning asynchronous tasks within message handlers

use std::time::Duration;
use std::ptr::NonNull;
use std::any::Any;

use parrot::actix::{ActixActorSystem, ActixActor, ActixContext};
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    message::{Message, MessageEnvelope},
    types::{BoxedMessage, ActorResult, BoxedActorRef, BoxedFuture},
    system::{ActorSystemConfig, ActorSystem},
    address::ActorRefExt,
    errors::ActorError,
    match_message,
    message_response_ok,
};
use actix::{AsyncContext, Context as ActixBaseContext, Actor as ActixBaseTrait, fut};
use parrot_api_derive::{Message, ParrotActor};
use anyhow::Error;
use tokio::time::sleep;
use uuid;
use parrot::actix::message::ActixMessageWrapper;

//----------------------------------------------------------------------
// Message Type Definitions
//----------------------------------------------------------------------

// 1. Spawn async task message - doesn't wait for result
#[derive(Debug, Message)]
#[message(result = "()")]
struct SpawnAsyncTask {
    name: String,
    duration_ms: u64,
}

// 2. Get state message
#[derive(Debug, Message)]
#[message(result = "Vec<String>")]
struct GetProcessedTasks;

// 3. Save task result message
#[derive(Debug, Message)]
#[message(result = "()")]
struct SaveTaskResult(String);

//----------------------------------------------------------------------
// Actor Implementation
//----------------------------------------------------------------------

#[derive(Debug, ParrotActor)]
#[ParrotActor(engine = "actix")]
struct AsyncActor {
    processed_tasks: Vec<String>,
    state: ActorState,
}

impl AsyncActor {
    pub fn new() -> Self {
        Self {
            processed_tasks: Vec::new(),
            state: ActorState::Starting,
        }
    }

    // Simulate an asynchronous operation, wait for specified time and return result
    async fn do_async_work(name: String, duration_ms: u64, processed_tasks: &mut Vec<String>) -> Result<String, Error> {
        println!("Starting async task: {}", name);
        processed_tasks.push(name.clone());
        sleep(Duration::from_millis(duration_ms)).await;
        println!("Async task completed: {}", name);
        Ok(format!("{} completed", name))
    }

    // Engine-specific message handling method
    fn handle_message_engine(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>, engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        let actix_ctx = unsafe { engine_ctx.as_ref().downcast_ref::<ActixBaseContext<ActixActor<Self>>>() };
        assert!(actix_ctx.is_some());
        let actix_ctx = actix_ctx.unwrap();

        match_message!("option", self, msg,
            // Method 1: Spawn async task, don't wait for result
            SpawnAsyncTask => |_actor: &mut Self, task: &SpawnAsyncTask| {
                println!("Received SpawnAsyncTask message: {}", task.name);
                let task_name = task.name.clone();
                let duration = task.duration_ms;
                
                // Clone address before async closure, instead of directly using context
                let addr = actix_ctx.address().clone();
                
                // Use actix::Arbiter to execute async task
                actix::Arbiter::current().spawn(async move {
                    println!("address: {:?}", addr);
                    // Execute async operation - don't use self reference
                    let mut tasks = Vec::new();
                    let result = Self::do_async_work(task_name.clone(), duration, &mut tasks).await;
                    
                    // If we need to save results, we should send a message back to the actor
                    match result {
                        Ok(result) => {
                            println!("Async task [{}] executed successfully: {}", task_name, result);
                            // Properly wrap message when sending back to actor
                            let msg = SaveTaskResult(task_name.clone());
                            // Use create_envelope helper function
                            let envelope = parrot::actix::message::create_envelope(msg);
                            let wrapper = ActixMessageWrapper { envelope };
                            addr.do_send(wrapper);
                        },
                        Err(e) => println!("Async task [{}] failed: {}", task_name, e),
                    }
                });
                
                // Since the task is async, we return immediately without waiting for the result
                ()
            },
            
            // Get list of processed tasks
            GetProcessedTasks => |actor: &mut Self, _: &GetProcessedTasks| {
                actor.processed_tasks.clone()
            },
            
            // Handle save task result message
            SaveTaskResult => |actor: &mut Self, msg: &SaveTaskResult| {
                actor.processed_tasks.push(msg.0.clone());
                ()
            }
        )
    }
    
    // Return Actor state
    fn state(&self) -> ActorState {
        self.state
    }
}

//----------------------------------------------------------------------
// Main Program
//----------------------------------------------------------------------
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Use actix::System as the application runtime environment
    actix::System::new().block_on(async {
        run_async_example().await
    })
}

async fn run_async_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting async message handling example");
    
    // Create Actor system
    let config = ActorSystemConfig::default();
    let system = ParrotActorSystem::new(config).await?;

    // Register ActixActorSystem
    let actix_system = ActixActorSystem::new().await?;
    system.register_actix_system("actix".to_string(), actix_system, true).await?;

    // Create async processing Actor
    let actor = AsyncActor::new();
    let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
    
    println!("1. Testing async task spawning (without waiting for results)");
    // Send multiple async task messages
    actor_ref.ask(SpawnAsyncTask {
        name: "Task A".to_string(),
        duration_ms: 2000,
    }).await?;
    
    actor_ref.ask(SpawnAsyncTask {
        name: "Task B".to_string(),
        duration_ms: 1000,
    }).await?;
    
    // Wait some time for async tasks to execute
    println!("Waiting for async tasks to execute...");
    tokio::time::sleep(Duration::from_millis(5000)).await;
    
    // Get list of processed tasks
    let processed_tasks = actor_ref.ask(GetProcessedTasks).await?;
    println!("\nList of processed tasks:");
    for task in processed_tasks {
        println!("- {}", task);
    }
    
    // Wait enough time for all async tasks to complete
    println!("\nWaiting for all async tasks to complete...");
    tokio::time::sleep(Duration::from_secs(3)).await;
    
    println!("Example completed, exiting program");
    Ok(())
} 