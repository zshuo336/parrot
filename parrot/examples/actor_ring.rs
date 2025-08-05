// Actor环形队列实现
// 功能：创建环形Actor队列，实现消息传递、累加和顺序退出
// 流程：A→B→C→D→E→F→A形成环形，初始消息从A开始传递，每个Actor将值加1并传递
//       完成环形传递后，启动顺序退出流程，最终整个系统安全退出

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
};
use actix::{AsyncContext, Context as ActixBaseContext, Actor as ActixBaseTrait};
use parrot_api_derive::{Message, ParrotActor};
use std::any::Any;
use std::ptr::NonNull;
use tokio::signal;

//----------------------------------------------------------------------
// 消息类型定义
//----------------------------------------------------------------------

// NextActorRef：用于传递下一个Actor的引用，建立环形结构
#[derive(Debug, Message)]
#[message(result = "()")]
struct NextActorRef {
    next_ref: BoxedActorRef,
}

// AddMessage：累加消息，包含当前累加值
// 每个Actor收到后将值加1并发送给下一个
#[derive(Debug, Clone, Message)]
#[message(result = "u32")]
struct AddMessage {
    value: u32,
}

// ExitMessage：退出消息，用于触发Actor的顺序退出
// 从A开始发送给B，然后B发送给C，依此类推
#[derive(Debug, Clone, Message)]
#[message(result = "()")]
struct ExitMessage;

//----------------------------------------------------------------------
// Actor实现
//----------------------------------------------------------------------

// RingActor：环形队列中的节点Actor
// 处理三种消息类型：NextActorRef、AddMessage和ExitMessage
#[derive(Debug, ParrotActor)]
#[ParrotActor(engine = "actix")]
struct RingActor {
    name: String,           // Actor名称（A-F）
    next: Option<BoxedActorRef>,  // 下一个Actor的引用
    state: ActorState,      // Actor的当前状态
}

impl RingActor {
    // 创建新的RingActor实例
    pub fn new(name: String) -> Self {
        Self {
            name,
            next: None,
            state: ActorState::Starting,
        }
    }

    // 消息处理方法：处理三种消息类型
    // 使用match_message!宏进行消息分发，保证类型安全
    fn handle_message_engine(&mut self, msg: BoxedMessage, _ctx: &mut ActixContext<ActixActor<Self>>, engine_ctx: NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
        let actix_ctx: Option<&ActixBaseContext<ActixActor<Self>>> = unsafe { engine_ctx.as_ref().downcast_ref::<ActixBaseContext<ActixActor<Self>>>() };
        assert!(actix_ctx.is_some());
        let actix_ctx = actix_ctx.unwrap();
        match_message!("option", self, msg,
            // 处理NextActorRef消息：接收下一个Actor的引用，建立环形连接
            NextActorRef => |actor: &mut Self, next_ref: &NextActorRef| {
                println!("Actor {} received next actor reference", actor.name);
                actor.next = Some(next_ref.next_ref.clone_boxed());
                ()
            },
            
            // 处理AddMessage消息：累加值并传递给下一个Actor
            AddMessage => |actor: &mut Self, add_msg: &AddMessage| {
                let new_value = add_msg.value + 1;
                println!("Actor {} received AddMessage with value {}, sending {} to next", 
                         actor.name, add_msg.value, new_value);
                
                // 如果是A(第一个Actor)，且接收到来自F的消息(完成一圈)
                if actor.name == "A" && add_msg.value > 1 {
                    println!("Ring completed! Final value: {}", new_value);
                    
                    // 向下一个Actor发送退出消息启动退出流程
                    if let Some(next) = &actor.next {
                        let next_ref = next.clone_boxed();
                        actix_ctx.spawn(async move {
                            next_ref.ask(ExitMessage).await.unwrap();
                        });
                    }
                } else if let Some(next) = &actor.next {
                    // 向下一个Actor发送累加后的消息
                    let next_ref = next.clone_boxed();
                    let next_value = new_value;  // 克隆值而不是引用
                    actix_ctx.spawn(async move {
                        next_ref.ask(AddMessage { value: next_value }).await.unwrap();
                    });
                }
                
                new_value
            },
            
            // 处理ExitMessage消息：触发顺序退出流程
            ExitMessage => |actor: &mut Self, _: &ExitMessage| {
                println!("Actor {} received exit message", actor.name);
                
                // 如果是A，整个环已经退出完毕，关闭系统
                if actor.name == "A" {
                    println!("Actor A received exit message, full ring shutdown completed");
                    // 异步关闭系统
                    actix_ctx.spawn(async {
                        // 给一点时间输出日志
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        // 正常退出程序
                        std::process::exit(0);
                    });
                } else if let Some(next) = &actor.next {
                    // 向下一个Actor转发退出消息
                    let next_ref = next.clone_boxed();
                    actix_ctx.spawn(async move {
                        next_ref.ask(ExitMessage).await.unwrap();
                    });
                    
                    // 当前Actor将进入Stopping状态
                    actor.state = ActorState::Stopping;
                }
                
                ()
            }
        )
    }
    
    // 添加状态方法以便Actor系统使用
    fn state(&self) -> ActorState {
        self.state
    }
}

// 手动为RingActor实现Clone
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
// 主程序入口
//----------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 使用actix::System作为应用程序的运行环境
    actix::System::new().block_on(async {
        run_ring_example().await
    })
}

// 运行环形Actor示例的异步函数
async fn run_ring_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting Actor Ring example");
    
    //----------------------------------------------------------------------
    // 系统初始化
    //----------------------------------------------------------------------
    
    // 创建全局Actor系统
    let config = ActorSystemConfig::default();
    let system = ParrotActorSystem::new(config).await?;

    // 创建并注册ActixActorSystem
    let actix_system = ActixActorSystem::new().await?;
    system.register_actix_system("actix".to_string(), actix_system, true).await?;

    //----------------------------------------------------------------------
    // 创建Actor并添加到系统
    //----------------------------------------------------------------------
    
    // 创建6个Actor: A, B, C, D, E, F
    let actor_a = RingActor::new("A".to_string());
    let actor_b = RingActor::new("B".to_string());
    let actor_c = RingActor::new("C".to_string());
    let actor_d = RingActor::new("D".to_string());
    let actor_e = RingActor::new("E".to_string());
    let actor_f = RingActor::new("F".to_string());
    
    // 将Actor添加到系统中，获取它们的引用
    let a_ref = system.spawn_root_actix(actor_a, EmptyConfig::default()).await?;
    let b_ref = system.spawn_root_actix(actor_b, EmptyConfig::default()).await?;
    let c_ref = system.spawn_root_actix(actor_c, EmptyConfig::default()).await?;
    let d_ref = system.spawn_root_actix(actor_d, EmptyConfig::default()).await?;
    let e_ref = system.spawn_root_actix(actor_e, EmptyConfig::default()).await?;
    let f_ref = system.spawn_root_actix(actor_f, EmptyConfig::default()).await?;
    
    println!("All actors spawned");
    
    //----------------------------------------------------------------------
    // 建立环形连接
    //----------------------------------------------------------------------
    
    // 建立环形连接：A -> B -> C -> D -> E -> F -> A
    println!("Building the ring: A -> B -> C -> D -> E -> F -> A");
    
    // 发送"下一个Actor引用"消息，建立环形结构
    a_ref.ask(NextActorRef { next_ref: b_ref.clone_boxed() }).await?;
    b_ref.ask(NextActorRef { next_ref: c_ref.clone_boxed() }).await?;
    c_ref.ask(NextActorRef { next_ref: d_ref.clone_boxed() }).await?;
    d_ref.ask(NextActorRef { next_ref: e_ref.clone_boxed() }).await?;
    e_ref.ask(NextActorRef { next_ref: f_ref.clone_boxed() }).await?;
    f_ref.ask(NextActorRef { next_ref: a_ref.clone_boxed() }).await?;
    
    println!("Ring setup completed");
    
    // 给系统一点时间处理消息
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    //----------------------------------------------------------------------
    // 启动消息传递
    //----------------------------------------------------------------------
    
    // 向A发送初始累加消息，值为1
    // 这将启动整个消息传递流程：
    // 1. 每个Actor收到消息后将值加1并发送给下一个
    // 2. 当A收到来自F的消息后，会打印最终累加值并启动退出流程
    // 3. 退出流程：A发送退出消息给B，B发送给C，依此类推
    // 4. 当A收到F的退出消息后，整个系统安全退出
    println!("Starting with AddMessage(1) to Actor A");
    let result = a_ref.ask(AddMessage { value: 1 }).await?;
    println!("Initial response from Actor A: {}", result);
    
    // 等待系统自然退出
    // 注意：程序会在Actor A收到退出消息后退出，不需要这里主动关闭系统
    println!("Waiting for ring to process messages and exit");
    
    // 等待更长时间让消息有足够时间传递，或者等待用户中断程序
    println!("Press Ctrl+C to exit manually if needed...");
    
    // 两种方式结束程序：
    // 1. 等待30秒 - 应该足够让消息完成传递
    // 2. 等待用户按Ctrl+C主动退出
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
