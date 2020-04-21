use std::sync::{Arc, Weak};
use std::time::Duration;
use std::error::Error;

use async_trait::async_trait;
use tokio::runtime::Runtime;

use parrot_api::types::BoxedMessage;
use parrot_api::address::ActorPath;
use parrot::thread::mailbox::{SimpleMailbox, Mailbox};
use parrot::thread::config::{ThreadActorConfig, SchedulingMode, BackpressureStrategy};
use parrot::thread::scheduler::dedicated::{DedicatedThreadPool, SystemRef};
use parrot::system::actor::ThreadActor;

// 简单的系统引用实现
struct ExampleSystemRef;

impl SystemRef for ExampleSystemRef {
    fn find_actor(&self, _path: &str) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        None
    }
    
    fn handle_worker_panic(&self, error: String, mailbox_path: String) {
        eprintln!("Worker panic in {}: {}", mailbox_path, error);
    }
}

// 计算密集型actor示例
struct ComputeActor {
    counter: u64,
}

impl ComputeActor {
    fn new() -> Self {
        Self { counter: 0 }
    }
    
    // 模拟计算密集型工作
    fn do_heavy_computation(&mut self, iterations: u64) -> u64 {
        let mut result = 0;
        for i in 0..iterations {
            result = result.wrapping_add(i.wrapping_mul(self.counter));
            if i % 1000 == 0 {
                // 模拟复杂计算
                std::thread::sleep(Duration::from_micros(1));
            }
        }
        self.counter += 1;
        result
    }
}

// Actor实现
#[async_trait]
impl ThreadActor for ComputeActor {
    async fn receive(&mut self, message: BoxedMessage) -> Result<(), Box<dyn Error + Send + Sync>> {
        if let Some(iterations) = message.downcast_ref::<u64>() {
            let result = self.do_heavy_computation(*iterations);
            println!("Computed result: {} after {} iterations", result, iterations);
        } else {
            println!("Received unknown message");
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // 创建tokio运行时
    let runtime = Runtime::new()?;
    let handle = runtime.handle().clone();
    
    // 创建系统引用
    let system_ref = Arc::new(ExampleSystemRef);
    let system_weak = Arc::downgrade(&system_ref) as Weak<dyn SystemRef + Send + Sync>;
    
    // 创建专用线程池，最多允许3个线程
    let pool = DedicatedThreadPool::new(
        handle.clone(),
        Some(system_weak),
        3,
    );
    
    // 创建几个计算密集型actor
    let actor1 = Arc::new(ComputeActor::new());
    let actor2 = Arc::new(ComputeActor::new());
    
    // 创建对应的mailbox
    let mailbox1 = SimpleMailbox::new("compute/actor1", 100);
    let mailbox2 = SimpleMailbox::new("compute/actor2", 100);
    
    // 配置为专用线程
    let config = ThreadActorConfig {
        scheduling_mode: Some(SchedulingMode::DedicatedThread),
        ..Default::default()
    };
    
    // 在各自的专用线程上调度这些actor
    println!("Scheduling actor1 on dedicated thread");
    pool.schedule(mailbox1.clone(), Some(&config))?;
    
    println!("Scheduling actor2 on dedicated thread");
    pool.schedule(mailbox2.clone(), Some(&config))?;
    
    // 向这些actor发送计算任务
    println!("Sending computation tasks to actors");
    runtime.block_on(async {
        let msg1: BoxedMessage = Box::new(1_000_000u64);
        let msg2: BoxedMessage = Box::new(2_000_000u64);
        
        mailbox1.push(msg1, BackpressureStrategy::Block).await?;
        mailbox2.push(msg2, BackpressureStrategy::Block).await?;
        
        // 给一些时间让任务完成
        tokio::time::sleep(Duration::from_millis(500)).await;
        
        // 发送更多任务
        let msg3: BoxedMessage = Box::new(500_000u64);
        let msg4: BoxedMessage = Box::new(1_500_000u64);
        
        mailbox1.push(msg3, BackpressureStrategy::Block).await?;
        mailbox2.push(msg4, BackpressureStrategy::Block).await?;
        
        // 等待所有任务完成
        tokio::time::sleep(Duration::from_secs(1)).await;
        
        // 取消调度actor1
        println!("Descheduling actor1");
        pool.deschedule(&mailbox1.path(), true, Some(1000))?;
        
        // 再等待一段时间
        tokio::time::sleep(Duration::from_millis(500)).await;
        
        // 关闭线程池
        println!("Shutting down the pool");
        pool.shutdown(1000).await?;
        
        Result::<(), Box<dyn Error + Send + Sync>>::Ok(())
    })?;
    
    println!("All tasks completed successfully");
    Ok(())
} 