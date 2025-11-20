// Parrot Actor Asynchronous Message Handling Example
// This example demonstrates two ways to handle asynchronous messages in the Parrot Actor framework:
// 1. Spawning asynchronous tasks within message handlers

use parrot::actix as __parrot_engine;
use std::time::Duration;

use actix::{AsyncContext, Context as ActixBaseContext};
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{ActorState, EmptyConfig},
    address::ActorRefExt,
    context::ActorContext,
    errors::ActorError,
    match_message,
    system::{ActorSystem, ActorSystemConfig},
    types::{ActorResult, BoxedMessage},
};
use parrot_api_derive::{Message, ParrotActor};
use tokio::time::sleep;

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
    completed: Option<tokio::sync::oneshot::Sender<()>>,
    #[allow(dead_code)]
    state: ActorState,
}

impl AsyncActor {
    async fn handle_message(
        &mut self,
        msg: BoxedMessage,
        _ctx: &mut ActixContext<ActixActor<Self>>,
    ) -> ActorResult<BoxedMessage> {
        // Fallback for message types not handled by the engine fast path.
        Err(ActorError::MessageHandlingError(format!(
            "AsyncActor received unhandled message: {:?}",
            msg
        )))
    }

    pub fn new(completed: tokio::sync::oneshot::Sender<()>) -> Self {
        Self {
            processed_tasks: Vec::new(),
            completed: Some(completed),
            state: ActorState::Starting,
        }
    }

    // Simulate an asynchronous operation, wait for specified time and return result
    async fn do_async_work(name: String, duration_ms: u64) -> String {
        println!("Starting async task: {}", name);
        sleep(Duration::from_millis(duration_ms)).await;
        println!("Async task completed: {}", name);
        format!("{} completed", name)
    }

    // Engine-specific message handling method
    fn handle_message_engine(
        &mut self,
        msg: BoxedMessage,
        ctx: &mut ActixContext<ActixActor<Self>>,
        mut engine_ctx: parrot_api::actor::EngineContextHandle,
    ) -> Option<ActorResult<BoxedMessage>> {
        let self_ref = ctx.get_self_ref();
        let actix_ctx = engine_ctx.downcast_mut::<ActixBaseContext<ActixActor<Self>>>();
        assert!(actix_ctx.is_some());
        let actix_ctx = actix_ctx.unwrap();

        match_message!("option", self, msg,
            // Method 1: Spawn async task, don't wait for result
            SpawnAsyncTask => |_actor: &mut Self, task: &SpawnAsyncTask| {
                println!("Received SpawnAsyncTask message: {}", task.name);
                let task_name = task.name.clone();
                let duration = task.duration_ms;
                // The native context owns this future and cancels it on stop.
                actix_ctx.spawn(actix::fut::wrap_future::<_, ActixActor<Self>>(async move {
                    let result = Self::do_async_work(task_name.clone(), duration).await;
                    println!("Async task [{task_name}] executed successfully: {result}");
                    if let Err(error) = self_ref.deliver(Box::new(SaveTaskResult(task_name))).await {
                        eprintln!("saving async result failed: {error}");
                    }
                }));

                // Since the task is async, we return immediately without waiting for the result

            },

            // Get list of processed tasks
            GetProcessedTasks => |actor: &mut Self, _: &GetProcessedTasks| {
                actor.processed_tasks.clone()
            },

            // Handle save task result message
            SaveTaskResult => |actor: &mut Self, msg: &SaveTaskResult| {
                actor.processed_tasks.push(msg.0.clone());
                if actor.processed_tasks.len() == 2
                    && let Some(completed) = actor.completed.take() {
                    let _ = completed.send(());
                }
            }
        )
    }

    #[allow(dead_code)]
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
    actix::System::new().block_on(async { run_async_example().await })
}

async fn run_async_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting async message handling example");

    // Create Actor system
    let config = ActorSystemConfig::default();
    let system = ParrotActorSystem::new(config).await?;

    // Register ActixActorSystem
    let actix_system = ActixActorSystem::new().await?;
    system
        .register_actix_system("actix".to_string(), actix_system, true)
        .await?;

    let result = async {
        // Completion is emitted by the actor after both saved results are applied.
        let (completed, received) = tokio::sync::oneshot::channel();
        let actor = AsyncActor::new(completed);
        let actor_ref = system.spawn_root_actix(actor, EmptyConfig).await?;

        println!("1. Testing async task spawning (without waiting for results)");
        // Send multiple async task messages
        actor_ref
            .ask(SpawnAsyncTask {
                name: "Task A".to_string(),
                duration_ms: 20,
            })
            .await?;

        actor_ref
            .ask(SpawnAsyncTask {
                name: "Task B".to_string(),
                duration_ms: 10,
            })
            .await?;

        tokio::time::timeout(Duration::from_secs(5), received).await??;

        // Get list of processed tasks
        let mut processed_tasks = actor_ref.ask(GetProcessedTasks).await?;
        processed_tasks.sort();
        assert_eq!(processed_tasks, ["Task A", "Task B"]);
        println!("\nList of processed tasks:");
        for task in processed_tasks {
            println!("- {}", task);
        }

        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    ActorSystem::shutdown(system).await?;
    result?;
    println!("async tasks=2; saved=2; system retired");
    Ok(())
}
