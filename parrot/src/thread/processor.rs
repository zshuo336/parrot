use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};
use tokio::sync::mpsc::{self, Sender, Receiver};
use tokio::time::{sleep, timeout};

use parrot_api::actor::{Actor, ActorState};
use parrot_api::types::{BoxedMessage, ActorResult};

use crate::thread::actor::ThreadActor;
use crate::thread::context::ThreadContext;
use crate::thread::mailbox::Mailbox;
use crate::thread::envelope::ControlMessage;
use crate::thread::error::SystemError;
use crate::thread::config::ThreadActorConfig;

/// 消息处理命令
enum ProcessorCommand {
    /// 处理一个消息批次
    ProcessBatch {
        /// 最大消息数
        max_messages: usize,
    },
    
    /// 停止处理
    Stop,
    
    /// 暂停处理
    Pause,
    
    /// 恢复处理
    Resume,
}

/// Actor处理器，负责管理Actor的消息循环和生命周期
pub struct ActorProcessor<A: Actor + Send + Sync + 'static>
where
    A::Context: std::ops::Deref<Target = ThreadContext<A>>
{
    /// Actor实例
    actor: ThreadActor<A>,
    
    /// Actor上下文
    context: ThreadContext<A>,
    
    /// Actor邮箱
    mailbox: Arc<dyn Mailbox>,
    
    /// Actor路径
    path: String,
    
    /// Actor配置
    config: ThreadActorConfig,
    
    /// 命令发送器
    cmd_tx: Option<Sender<ProcessorCommand>>,
    
    /// 命令接收器
    cmd_rx: Option<Receiver<ProcessorCommand>>,
    
    /// 是否在运行
    running: bool,
}

impl<A: Actor + Send + Sync + 'static> ActorProcessor<A>
where
    A::Context: std::ops::Deref<Target = ThreadContext<A>>
{
    /// 创建一个新的Actor处理器
    pub fn new(
        actor: ThreadActor<A>,
        context: ThreadContext<A>,
        mailbox: Arc<dyn Mailbox>,
        path: String,
        config: ThreadActorConfig,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        
        Self {
            actor,
            context,
            mailbox,
            path,
            config,
            cmd_tx: Some(cmd_tx),
            cmd_rx: Some(cmd_rx),
            running: false,
        }
    }
    
    /// 获取处理器的命令发送器
    pub fn command_sender(&self) -> Option<Sender<ProcessorCommand>> {
        self.cmd_tx.clone()
    }
    
    /// 启动处理器
    pub async fn start(&mut self) -> Result<(), SystemError> {
        if self.running {
            return Ok(());
        }
        
        self.running = true;
        
        // 初始化actor
        debug!("Initializing actor at {}", self.path);
        if let Err(e) = self.actor.initialize(&mut self.context).await {
            error!("Failed to initialize actor at {}: {}", self.path, e);
            return Err(SystemError::ActorCreationError(format!(
                "Failed to initialize actor at {}: {}", self.path, e
            )));
        }
        
        // 发送Start消息
        debug!("Sending Start message to actor at {}", self.path);
        let start_msg = Box::new(ControlMessage::Start);
        if let Err(e) = self.actor.process_message(start_msg, &mut self.context).await {
            error!("Failed to start actor at {}: {}", self.path, e);
            return Err(SystemError::ActorCreationError(format!(
                "Failed to start actor at {}: {}", self.path, e
            )));
        }
        
        // 获取批处理大小
        let batch_size = match self.config.scheduling_mode.as_ref() {
            Some(crate::thread::config::SchedulingMode::SharedPool { max_messages_per_run }) => {
                *max_messages_per_run
            },
            _ => 10, // 默认批处理大小
        };
        
        // 启动命令循环
        let mut cmd_rx = self.cmd_rx.take().expect("Command receiver already taken");
        let mailbox = self.mailbox.clone();
        let path = self.path.clone();
        
        debug!("Starting processor command loop for actor at {}", path);
        
        // 创建消息处理器的弱引用，以防止循环引用
        let mut processor = self.clone_without_channels();
        
        // 启动命令循环
        tokio::spawn(async move {
            // 先处理一个批次
            let _ = processor.process_batch(batch_size).await;
            
            // 然后进入命令循环
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    ProcessorCommand::ProcessBatch { max_messages } => {
                        if let Err(e) = processor.process_batch(max_messages).await {
                            error!("Error processing batch for actor at {}: {}", 
                                   path, e);
                            
                            // 尝试关闭actor
                            if let Err(e) = processor.shutdown().await {
                                error!("Error shutting down actor at {} after batch processing failure: {}", 
                                      path, e);
                            }
                            
                            break;
                        }
                    },
                    ProcessorCommand::Stop => {
                        info!("Stopping actor processor for {}", path);
                        if let Err(e) = processor.shutdown().await {
                            error!("Error shutting down actor at {}: {}", path, e);
                        }
                        break;
                    },
                    ProcessorCommand::Pause => {
                        debug!("Pausing actor processor for {}", path);
                        // 暂停处理，但保持命令循环运行
                    },
                    ProcessorCommand::Resume => {
                        debug!("Resuming actor processor for {}", path);
                        // 立即处理一个批次
                        let _ = processor.process_batch(batch_size).await;
                    }
                }
            }
            
            info!("Actor processor for {} has stopped", path);
        });
        
        Ok(())
    }
    
    /// 处理一批消息
    async fn process_batch(&mut self, max_messages: usize) -> ActorResult<()> {
        debug!("Processing batch of up to {} messages for actor at {}", 
               max_messages, self.path);
        
        // 检查actor状态
        if self.actor.state() != ActorState::Running {
            warn!("Cannot process batch: actor at {} is not running (state: {:?})", 
                 self.path, self.actor.state());
            return Ok(());
        }
        
        // 处理消息批次
        let mut processed = 0;
        for _ in 0..max_messages {
            // 尝试获取一条消息
            match self.mailbox.pop().await {
                Ok(Some(msg)) => {
                    // 处理消息
                    if let Err(e) = self.actor.process_message(msg, &mut self.context).await {
                        error!("Error processing message for actor at {}: {}", 
                               self.path, e);
                               
                        // 根据错误类型决定是否继续
                        // TODO: 实现更复杂的错误处理和恢复策略
                        if self.actor.state() != ActorState::Running {
                            // actor已停止，中断批处理
                            break;
                        }
                    }
                    
                    processed += 1;
                },
                Ok(None) => {
                    // 邮箱为空，批处理结束
                    break;
                },
                Err(e) => {
                    error!("Error popping message from mailbox for actor at {}: {}", 
                           self.path, e);
                    // 邮箱出错，中断批处理
                    break;
                }
            }
        }
        
        debug!("Processed {} messages for actor at {}", processed, self.path);
        
        // 如果邮箱不为空，则调度下一个批次
        if !self.mailbox.is_empty() {
            if let Some(cmd_tx) = &self.cmd_tx {
                if let Err(e) = cmd_tx.send(ProcessorCommand::ProcessBatch { 
                    max_messages 
                }).await {
                    error!("Failed to schedule next batch for actor at {}: {}", 
                           self.path, e);
                }
            }
        }
        
        Ok(())
    }
    
    /// 关闭处理器
    async fn shutdown(&mut self) -> ActorResult<()> {
        debug!("Shutting down actor processor for {}", self.path);
        
        // 发送Stop消息
        let stop_msg = Box::new(ControlMessage::Stop);
        if let Err(e) = self.actor.process_message(stop_msg, &mut self.context).await {
            error!("Error sending Stop message to actor at {}: {}", 
                   self.path, e);
        }
        
        // 调用actor的shutdown方法
        if let Err(e) = self.actor.shutdown(&mut self.context).await {
            error!("Error shutting down actor at {}: {}", self.path, e);
            return Err(e);
        }
        
        self.running = false;
        Ok(())
    }
    
    /// 创建不包含通道的克隆
    fn clone_without_channels(&self) -> Self {
        // 这里会导致编译错误，因为很多字段不支持Clone
        // 在实际实现中，应该有更好的方式来处理这个问题
        // 这只是一个占位实现
        unimplemented!("Cannot clone actor processor");
    }
} 