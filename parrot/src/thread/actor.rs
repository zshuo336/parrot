use async_trait::async_trait;
use parrot_api::actor::{Actor, ActorState};
use parrot_api::address::ActorPath;
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture, BoxedActorRef};
use parrot_api::errors::ActorError;
use std::fmt::Debug;
use std::collections::HashSet;
use anyhow::anyhow;
use tracing::{error, debug, info, warn};

use crate::thread::mailbox::Mailbox;
use crate::thread::context::ThreadContext;
use crate::thread::envelope::{AskEnvelope, ControlMessage};

/// Thread-based implementation of an Actor.
/// 
/// This is a wrapper that adapts the generic Actor trait to the
/// thread-based execution model, handling message dispatching and lifecycle.
#[derive(Debug)]
pub struct ThreadActor<A>
where
    A: Actor + Send + Sync + 'static,
    // 移除对A::Context的Deref约束，改为强制约束A::Context必须是ThreadContext<A>
    A::Context: Send + 'static,
{
    /// The wrapped actor implementation
    inner: A,
    /// Current actor state
    state: ActorState,
    /// Actor path for addressing
    path: ActorPath,
    /// Set of actor paths that are watching this actor
    watchers: HashSet<String>,
}

impl<A> ThreadActor<A> 
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    /// Creates a new ThreadActor wrapping the provided actor implementation.
    pub fn new(actor: A, path: ActorPath) -> Self {
        Self {
            inner: actor,
            state: ActorState::Starting,
            path,
            watchers: HashSet::new(),
        }
    }
    
    /// Gets the current state of the actor.
    pub fn state(&self) -> ActorState {
        self.state
    }
    
    /// Gets the actor's path.
    pub fn path(&self) -> &ActorPath {
        &self.path
    }
    
    /// Initialize the actor.
    pub async fn initialize<'a>(&'a mut self, ctx: &'a mut ThreadContext<A>) -> ActorResult<()> {
        if self.state != ActorState::Starting {
            return Err(ActorError::Other(anyhow!("Actor is already initialized")));
        }
        
        debug!("Initializing actor at path: {}", self.path.path);
        
        // 现在ctx就是A::Context，不需要类型转换
        match self.inner.init(ctx).await {
            Ok(_) => {
                // Transition to Running state is handled by process_message for Start message
                Ok(())
            },
            Err(e) => {
                error!("Failed to initialize actor at {}: {}", self.path.path, e);
                self.state = ActorState::Stopped;
                Err(e)
            }
        }
    }
    
    /// Add an actor to the watchers list
    pub fn add_watcher(&mut self, watcher_path: String) {
        self.watchers.insert(watcher_path);
    }
    
    /// Remove an actor from the watchers list
    pub fn remove_watcher(&mut self, watcher_path: &str) {
        self.watchers.remove(watcher_path);
    }
    
    /// Get the watchers list
    pub fn watchers(&self) -> &HashSet<String> {
        &self.watchers
    }
    
    /// Process a message.
    pub async fn process_message<'a>(&'a mut self, msg: BoxedMessage, ctx: &'a mut ThreadContext<A>) -> ActorResult<BoxedMessage> {
        // Handle control messages specially
        if let Some(control_msg) = msg.downcast_ref::<ControlMessage>() {
            return self.handle_control_message(control_msg, ctx).await;
        }
        
        // Handle Ask messages specially
        if let Some(ask_envelope) = msg.downcast_ref::<AskEnvelope>() {
            return self.handle_ask_envelope(ask_envelope, ctx).await;
        }
        
        // Handle watch request
        if let Some(watch_req) = msg.downcast_ref::<crate::thread::system::WatchRequest>() {
            debug!("Actor at {} received watch request from {}", 
                   self.path.path, watch_req.watcher_path);
            self.add_watcher(watch_req.watcher_path.clone());
            return Ok(Box::new(()));
        }
        
        // Handle unwatch request
        if let Some(unwatch_req) = msg.downcast_ref::<crate::thread::system::UnwatchRequest>() {
            debug!("Actor at {} received unwatch request from {}", 
                   self.path.path, unwatch_req.watcher_path);
            self.remove_watcher(&unwatch_req.watcher_path);
            return Ok(Box::new(()));
        }
        
        // Only process regular messages if the actor is running
        if self.state != ActorState::Running {
            return Err(ActorError::Other(anyhow!("Actor is not running")));
        }
        
        // 不需要类型转换，ctx就是A::Context
        self.inner.receive_message(msg, ctx).await
    }
    
    /// Handle internal control messages
    async fn handle_control_message<'a>(&'a mut self, control_msg: &ControlMessage, ctx: &'a mut ThreadContext<A>) -> ActorResult<BoxedMessage> {
        match control_msg {
            ControlMessage::Start => {
                debug!("Handling Start message for actor at {}", self.path.path);
                if self.state == ActorState::Starting {
                    // Transition to Running state
                    self.state = ActorState::Running;
                    info!("Actor at {} is now running", self.path.path);
                    Ok(Box::new(()))
                } else {
                    warn!("Ignoring Start message for actor at {} in state {:?}", 
                        self.path.path, self.state);
                    Ok(Box::new(()))
                }
            },
            
            ControlMessage::Stop => {
                debug!("Handling Stop message for actor at {}", self.path.path);
                // Shut down the actor
                self.shutdown(ctx).await?;
                Ok(Box::new(()))
            },
            
            ControlMessage::ChildFailure { path, reason } => {
                debug!("Handling ChildFailure message for child {} at parent {}: {}", 
                    path, self.path.path, reason);
                // 获取child_ref
                let child_ref = if let Some(child) = ctx.children().into_iter().find(|c| c.path() == path) {
                    child
                } else {
                    // 如果找不到子Actor的引用，则创建一个错误
                    return Err(ActorError::Other(anyhow!("Child actor reference not found")));
                };
                
                self.inner.handle_child_terminated(child_ref, ctx).await?;
                Ok(Box::new(()))
            },
            
            ControlMessage::SystemShutdown => {
                debug!("Handling SystemShutdown message for actor at {}", self.path.path);
                // The system is shutting down, stop the actor
                self.shutdown(ctx).await?;
                Ok(Box::new(()))
            },
            
            ControlMessage::HealthCheck => {
                debug!("Handling HealthCheck message for actor at {}", self.path.path);
                // Just return the current state
                Ok(Box::new(self.state))
            }
        }
    }
    
    /// Handle ask envelopes
    async fn handle_ask_envelope<'a>(&'a mut self, envelope: &AskEnvelope, ctx: &'a mut ThreadContext<A>) -> ActorResult<BoxedMessage> {
        // Only process messages if the actor is running
        if self.state != ActorState::Running {
            return Err(ActorError::Other(anyhow!("Actor is not running")));
        }
        
        // Process the payload with the inner actor
        // 使用clone_arc()方法来克隆BoxedMessage
        let payload = envelope.payload.clone_arc();
        match self.inner.receive_message(payload, ctx).await {
            Ok(response) => {
                // 使用类型安全的方式发送回复
                if let Err(e) = envelope.reply.send(Ok(response)) {
                    error!("Failed to send reply for ask operation: {}", e);
                }
                Ok(Box::new(()))
            },
            Err(e) => {
                // 发送错误回复
                if let Err(send_err) = envelope.reply.send(Err(e.clone())) {
                    error!("Failed to send error reply for ask operation: {}", send_err);
                }
                // 原样返回错误
                Err(e)
            }
        }
    }
    
    /// Shut down the actor.
    pub async fn shutdown<'a>(&'a mut self, ctx: &'a mut ThreadContext<A>) -> ActorResult<()> {
        // Only attempt shutdown if not already stopped
        if self.state == ActorState::Stopped {
            return Ok(());
        }
        
        debug!("Shutting down actor at {}", self.path.path);
        
        // Transition to Stopping state
        self.state = ActorState::Stopping;
        
        // Call the inner actor's before_stop method
        let result = self.inner.before_stop(ctx).await;
        
        // Transition to Stopped state regardless of result
        self.state = ActorState::Stopped;
        
        // Notify all watchers
        self.notify_watchers_of_termination(ctx);
        
        info!("Actor at {} has stopped", self.path.path);
        
        result
    }
    
    /// Notify all watchers that this actor has terminated
    fn notify_watchers_of_termination(&self, _ctx: &ThreadContext<A>) {
        if !self.watchers.is_empty() {
            debug!("Notifying {} watchers of termination for actor at {}", 
                self.watchers.len(), self.path.path);
            
            // TODO: Implement actual notification logic
            // This would involve sending death notification messages to all watchers
            // For now this is just a placeholder
        }
    }
}

/// 为BoxedMessage添加克隆功能的扩展特性
trait BoxedMessageClone {
    /// 尝试克隆BoxedMessage，如果内部类型支持克隆
    fn clone_arc(&self) -> BoxedMessage;
}

impl BoxedMessageClone for BoxedMessage {
    fn clone_arc(&self) -> BoxedMessage {
        // 这里实现一个简单的克隆策略
        // 对于常见的类型，我们尝试克隆
        // 对于其他类型，返回一个空消息
        
        if let Some(s) = self.downcast_ref::<String>() {
            Box::new(s.clone())
        } else if let Some(b) = self.downcast_ref::<bool>() {
            Box::new(*b)
        } else if let Some(i) = self.downcast_ref::<i32>() {
            Box::new(*i)
        } else if let Some(i) = self.downcast_ref::<i64>() {
            Box::new(*i)
        } else if let Some(u) = self.downcast_ref::<u32>() {
            Box::new(*u)
        } else if let Some(u) = self.downcast_ref::<u64>() {
            Box::new(*u)
        } else if let Some(f) = self.downcast_ref::<f32>() {
            Box::new(*f)
        } else if let Some(f) = self.downcast_ref::<f64>() {
            Box::new(*f)
        } else if let Some(v) = self.downcast_ref::<Vec<u8>>() {
            Box::new(v.clone())
        } else if let Some(_) = self.downcast_ref::<()>() {
            Box::new(())
        } else {
            // 对于不支持克隆的类型，返回一个空消息
            Box::new(())
        }
    }
}

// 为ActorError添加Clone特性实现
impl Clone for ActorError {
    fn clone(&self) -> Self {
        match self {
            ActorError::UnhandledMessage => ActorError::UnhandledMessage,
            ActorError::SystemError(msg) => ActorError::SystemError(msg.clone()),
            ActorError::InitializationError(msg) => ActorError::InitializationError(msg.clone()),
            ActorError::ActorNotFound(path) => ActorError::ActorNotFound(path.clone()),
            ActorError::Timeout => ActorError::Timeout,
            ActorError::MessageSendError(msg) => ActorError::MessageSendError(msg.clone()),
            ActorError::SerializationError(msg) => ActorError::SerializationError(msg.clone()),
            ActorError::DeserializationError(msg) => ActorError::DeserializationError(msg.clone()),
            ActorError::StreamError(msg) => ActorError::StreamError(msg.clone()),
            ActorError::ConfigurationError(msg) => ActorError::ConfigurationError(msg.clone()),
            ActorError::ActorTerminated(msg) => ActorError::ActorTerminated(msg.clone()),
            ActorError::Other(err) => ActorError::Other(anyhow!("{}", err)),
        }
    }
}

// Note: This is a stub implementation that will be expanded in future PRs
// to include more actor lifecycle management, supervision, etc. 