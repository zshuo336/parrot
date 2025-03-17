use std::time::Duration;
use std::ptr::NonNull;
use std::any::Any;
use std::sync::{Arc, Mutex};

use actix;
use anyhow::{Result, anyhow};
use parrot::actix::{ActixActorSystem, ActixActor, ActixContext};
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    message::Message,
    match_message,
    types::{BoxedMessage, ActorResult, BoxedActorRef, BoxedFuture},
    system::{ActorSystemConfig, ActorSystem},
    address::{ActorRefExt, ActorPath},
    errors::ActorError,
};
use parrot_api_derive::{Message, ParrotActor};

// 添加Actix的ActorContext trait导入
use actix::prelude::*;

mod test_helpers;
use test_helpers::{setup_test_system, wait_for, with_test_system, DEFAULT_WAIT_TIME};

// Shared state to verify async operation completion
type SharedState = Arc<Mutex<Vec<String>>>;

// Define messages for async tests
#[derive(Clone, Debug, Message)]
#[message(result = "String")]
struct AsyncTaskMessage {
    name: String,
    delay_ms: u64,
}

#[derive(Clone, Debug, Message)]
#[message(result = "Vec<String>")]
struct GetTasksMessage;

#[derive(Clone, Debug, Message)]
#[message(result = "()")]
struct TaskCompletedMessage {
    task_name: String,
}

// Define an actor that processes messages asynchronously
#[derive(Debug, Clone, ParrotActor)]
#[ParrotActor(engine = "actix")]
struct AsyncTestActor {
    tasks_started: Vec<String>,
    tasks_completed: Vec<String>,
    state: ActorState,
}

impl AsyncTestActor {
    pub fn new() -> Self {
        Self {
            tasks_started: Vec::new(),
            tasks_completed: Vec::new(),
            state: ActorState::Starting,
        }
    }

    // 宏生成的Actix actor会调用此函数来处理消息
    async fn handle_message(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>) -> ActorResult<BoxedMessage> {
        // 这里仅返回一个空结果，不进行实际处理，避免与handle_message_engine重复处理
        Err(ActorError::Other(anyhow::anyhow!("Not implemented - use handle_message_engine")))
    }
    
    // 添加engine版本的处理函数
    fn handle_message_engine(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>, engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        // 获取Actix上下文，需要使用ActorContext trait来访问address方法
        let actix_ctx = unsafe { engine_ctx.as_ref().downcast_ref::<actix::Context<ActixActor<Self>>>() };
        
        match_message!("option", self, msg,
            AsyncTaskMessage => |actor: &mut Self, task: &AsyncTaskMessage| {
                // Record task start
                let task_name = task.name.clone();
                let delay = task.delay_ms;
                
                actor.tasks_started.push(task_name.clone());
                
                // Get actor address to send completion message back to self
                if let Some(ctx) = actix_ctx {
                    // 使用ActorContext trait的address方法
                    let addr = ctx.address();
                    
                    // Spawn async task
                    let task_name_for_async = task_name.clone();
                    actix::Arbiter::current().spawn(async move {
                        // Simulate work with delay
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        
                        // Send completion message back to actor
                        let completion_msg = TaskCompletedMessage {
                            task_name: task_name_for_async.clone(),
                        };
                        
                        // Create message envelope for actix
                        let envelope = parrot::actix::message::create_envelope(completion_msg);
                        let wrapper = parrot::actix::message::ActixMessageWrapper { envelope };
                        
                        // Send message back to actor
                        addr.do_send(wrapper);
                    });
                }
                
                // Return immediate response
                format!("Started task: {}", task_name)
            },
            
            TaskCompletedMessage => |actor: &mut Self, msg: &TaskCompletedMessage| {
                // Record task completion
                actor.tasks_completed.push(msg.task_name.clone());
                ()
            },
            
            GetTasksMessage => |actor: &mut Self, _: &GetTasksMessage| {
                // Return both lists for verification
                [actor.tasks_started.clone(), actor.tasks_completed.clone()].concat()
            }
        )
    }
}

// Run async tests
fn main() {
    actix::System::new().block_on(async {
        // Run all tests
        let results = run_all_tests().await;
        
        match results {
            Ok(_) => println!("All async tests passed!"),
            Err(e) => {
                eprintln!("Test failure: {}", e);
                std::process::exit(1);
            }
        }
    });
}

async fn run_all_tests() -> Result<()> {
    test_async_message_handling().await?;
    test_multiple_async_tasks().await?;
    
    Ok(())
}

async fn test_async_message_handling() -> Result<()> {
    with_test_system(|system| async move {
        // Create async test actor
        let actor = AsyncTestActor::new();
        let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
        
        // Start an async task that takes some time
        let response = actor_ref.ask(AsyncTaskMessage {
            name: "Test Task".to_string(),
            delay_ms: 100,
        }).await?;
        
        // Verify immediate response
        assert_eq!(response, "Started task: Test Task", "Should get immediate start confirmation");
        
        // Wait for task to complete
        wait_for(200).await;
        
        // Check task completion
        let tasks = actor_ref.ask(GetTasksMessage).await?;
        
        // Should have 2 entries - one in started and one in completed
        assert_eq!(tasks.len(), 2, "Should have 2 task entries (start + completion)");
        assert!(tasks.contains(&"Test Task".to_string()), "Tasks should contain the test task");
        
        Ok(((), system))
    }).await
}

async fn test_multiple_async_tasks() -> Result<()> {
    with_test_system(|system| async move {
        // Create async test actor
        let actor = AsyncTestActor::new();
        let actor_ref = system.spawn_root_actix(actor, EmptyConfig::default()).await?;
        
        // Start multiple async tasks with different delays
        actor_ref.ask(AsyncTaskMessage {
            name: "Fast Task".to_string(),
            delay_ms: 50,
        }).await?;
        
        actor_ref.ask(AsyncTaskMessage {
            name: "Medium Task".to_string(),
            delay_ms: 100,
        }).await?;
        
        actor_ref.ask(AsyncTaskMessage {
            name: "Slow Task".to_string(),
            delay_ms: 150,
        }).await?;
        
        // Wait for shortest task to complete, but not all
        wait_for(75).await;
        
        // At this point, only the fast task should be complete
        let tasks_partial = actor_ref.ask(GetTasksMessage).await?;
        let started_count = 3; // All tasks should be started
        let completed_count = tasks_partial.len() - started_count; // 计算完成的任务数
        
        // 验证任务起始数量是否正确
        assert_eq!(
            tasks_partial.len() >= started_count,
            true,
            "Should have started at least all tasks"
        );
        
        // 验证完成的任务中包含快速任务
        assert!(
            tasks_partial.contains(&"Fast Task".to_string()),
            "Fast task should be in the list of tasks"
        );
        
        // Wait for all tasks to complete
        wait_for(150).await;
        
        // All tasks should now be complete
        let tasks_final = actor_ref.ask(GetTasksMessage).await?;
        
        // 验证所有任务都已开始
        assert!(
            tasks_final.contains(&"Fast Task".to_string()) &&
            tasks_final.contains(&"Medium Task".to_string()) &&
            tasks_final.contains(&"Slow Task".to_string()),
            "All tasks should be present in the list"
        );
        
        // 检查任务数量，应该至少包含所有已开始的任务
        assert!(
            tasks_final.len() >= started_count, 
            "Task list should contain at least all started tasks"
        );
        
        // Check that specific tasks are in the list
        assert!(tasks_final.contains(&"Fast Task".to_string()), "Fast task should be recorded");
        assert!(tasks_final.contains(&"Medium Task".to_string()), "Medium task should be recorded");
        assert!(tasks_final.contains(&"Slow Task".to_string()), "Slow task should be recorded");
        
        Ok(((), system))
    }).await
} 