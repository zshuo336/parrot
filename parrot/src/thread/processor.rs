use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};
use tokio::sync::mpsc::{self, Sender, Receiver};
use tokio::time::{sleep, timeout};

use parrot_api::actor::{Actor, ActorState};
use parrot_api::types::{BoxedMessage, ActorResult};
use parrot_api::errors::ActorError;
use anyhow::anyhow;

use crate::thread::actor::ThreadActor;
use crate::thread::context::ThreadContext;
use crate::thread::mailbox::Mailbox;
use crate::thread::envelope::ControlMessage;
use crate::thread::error::SystemError;
use crate::thread::config::ThreadActorConfig;

/// 消息处理命令
#[derive(Debug)]
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
pub struct ActorProcessor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
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

impl<A> ActorProcessor<A>
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
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
        
        // 获取处理周期
        let mailbox = self.mailbox.clone();
        let path = self.path.clone();
        
        // 创建单独的命令处理通道
        let (worker_tx, mut worker_rx) = mpsc::channel::<ProcessorCommand>(32);
        
        // 从当前processor移出命令接收器
        let mut cmd_rx = self.cmd_rx.take().expect("Command receiver already taken");
        
        // 转发所有命令
        let cmd_forward_path = path.clone();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if worker_tx.send(cmd).await.is_err() {
                    error!("Worker channel closed for {}, stopping command forwarding", cmd_forward_path);
                    break;
                }
            }
        });
        
        // 创建新的命令发送器，给外部调用者使用
        let cmd_tx_clone = worker_tx.clone();
        self.cmd_tx = Some(cmd_tx_clone);
        
        debug!("Starting processor command loop for actor at {}", path);
        
        // 启动工作线程
        tokio::spawn(async move {
            // 创建工作器状态
            let mut processor_state = ProcessorState {
                mailbox,
                path: path.clone(),
            };
            
            // 首先处理一批消息
            if let Err(e) = processor_state.process_mailbox_batch(batch_size).await {
                error!("Initial batch processing failed for actor at {}: {}", path, e);
                return;
            }
            
            // 然后进入命令循环
            while let Some(cmd) = worker_rx.recv().await {
                match cmd {
                    ProcessorCommand::ProcessBatch { max_messages } => {
                        if let Err(e) = processor_state.process_mailbox_batch(max_messages).await {
                            error!("Error processing batch for actor at {}: {}", 
                                   path, e);
                            break;
                        }
                    },
                    ProcessorCommand::Stop => {
                        info!("Stopping actor processor for {}", path);
                        // 仅退出循环，ActorProcessor.shutdown会处理实际的停止逻辑
                        break;
                    },
                    ProcessorCommand::Pause => {
                        debug!("Pausing actor processor for {}", path);
                        // 暂停处理，但保持命令循环运行
                    },
                    ProcessorCommand::Resume => {
                        debug!("Resuming actor processor for {}", path);
                        if let Err(e) = processor_state.process_mailbox_batch(batch_size).await {
                            error!("Error processing batch during resume for actor at {}: {}", 
                                   path, e);
                            break;
                        }
                    }
                }
            }
            
            info!("Actor processor for {} has stopped", path);
        });
        
        Ok(())
    }
    
    /// 关闭Actor处理器
    pub async fn shutdown(&mut self) -> ActorResult<()> {
        debug!("Shutting down actor processor for {}", self.path);
        
        // 发送Stop消息
        let stop_msg = Box::new(ControlMessage::Stop);
        self.actor.process_message(stop_msg, &mut self.context).await?;
        
        // 关闭命令发送器
        self.cmd_tx = None;
        
        // 标记为未运行
        self.running = false;
        
        Ok(())
    }
}

/// 处理器工作线程的状态
struct ProcessorState {
    /// Actor邮箱
    mailbox: Arc<dyn Mailbox>,
    
    /// Actor路径
    path: String,
}

impl ProcessorState {
    /// 处理一批邮箱消息
    async fn process_mailbox_batch(&mut self, max_messages: usize) -> ActorResult<()> {
        debug!("Processing batch of up to {} messages for actor at {}", 
               max_messages, self.path);
        
        // 处理消息批次
        let mut processed = 0;
        for _ in 0..max_messages {
            // 尝试获取一条消息
            if let Some(msg) = self.mailbox.pop().await {
                // 将消息放回邮箱，由主处理器处理
                // 这不是最有效的方式，但避免了与ActorProcessor的同步问题
                if let Err(e) = self.mailbox.push(msg, crate::thread::config::BackpressureStrategy::Block).await {
                    error!("Failed to push message back to mailbox for actor at {}: {}", 
                           self.path, e);
                    return Err(ActorError::MessageSendError(format!(
                        "Failed to push message back to mailbox: {}", e
                    )));
                }
                
                processed += 1;
            } else {
                // 邮箱为空，批处理结束
                break;
            }
        }
        
        debug!("Processed {} messages for actor at {}", processed, self.path);
        
        // 如果邮箱不为空，调度下一个批次
        if !self.mailbox.is_empty() {
            // 邮箱不为空，将会在下一个循环中继续处理
        }
        
        Ok(())
    }
} 