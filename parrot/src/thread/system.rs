//! # Thread Actor System
//!
//! This module provides the core implementation of the thread-based actor system.
//! `ThreadActorSystem` is the central component that manages actor lifecycles,
//! maintains the actor registry, and coordinates the different scheduler implementations.
//!
//! ## Key Concepts
//! - Actor registry: Central storage of actor references and mailboxes
//! - Actor lifecycle management: Spawning, stopping, and monitoring actors
//! - Scheduler coordination: Managing shared and dedicated thread pools
//! - System shutdown: Graceful termination of all actors and threads
//!
//! ## Design Principles
//! - Thread safety: Uses Arc, RwLock, and atomic types for concurrent access
//! - Resource efficiency: Proper cleanup of actors and threads
//! - Flexibility: Support for different actor scheduling strategies
//! - Reliability: Robust error handling and supervision

use std::collections::HashMap;
use std::sync::{Arc, RwLock, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::fmt;
use std::error::Error;
use std::pin::Pin;
use std::any::Any;

use tokio::runtime::Handle;
use tokio::sync::Notify;
use anyhow::anyhow;
use tracing::{error, warn, info, debug};
use async_trait::async_trait;

use parrot_api::actor::{Actor, ActorState};
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::context::ActorContext;
use parrot_api::system::{ActorSystem, ActorSystemConfig, SystemStatus, SystemState, SystemResources};
use parrot_api::types::{BoxedActorRef, BoxedMessage, ActorResult, BoxedFuture};
use parrot_api::errors::ActorError;
use parrot_api::supervisor::SupervisorStrategyType;

use crate::thread::actor::ThreadActor;
use crate::thread::address::{ThreadActorRef, WeakMailboxRef};
use crate::thread::config::{ThreadActorSystemConfig, ThreadActorConfig, BackpressureStrategy, SupervisorStrategy};
use crate::thread::context::{ThreadContext, SystemRef as ContextSystemRef};
use crate::thread::error::{SystemError, SpawnError};
use crate::thread::mailbox::Mailbox;
use crate::thread::mailbox::mpsc::MpscMailbox;
use crate::thread::mailbox::spsc_ringbuf::SpscRingbufMailbox;
use crate::thread::scheduler::{ThreadScheduler, SystemRef as SchedulerSystemRef};
use crate::thread::scheduler::shared::SharedThreadPool;
use crate::thread::scheduler::dedicated::DedicatedThreadPool;

/// Entry in the actor registry
struct ActorRegistryEntry {
    /// Reference to the actor for sending messages
    actor_ref: Arc<dyn ActorRef>,
    
    /// Reference to the actor's mailbox
    mailbox: Arc<dyn Mailbox>,
    
    /// Actor configuration
    config: ThreadActorConfig,
    
    /// Optional reference to the actor's supervisor
    supervisor: Option<Arc<dyn ActorRef>>,
}

/// Thread-based implementation of the Parrot actor system
pub struct ThreadActorSystem {
    /// System configuration
    config: Arc<ThreadActorSystemConfig>,
    
    /// Registry of all active actors
    registry: Arc<RwLock<HashMap<String, ActorRegistryEntry>>>,
    
    /// Shared thread pool scheduler
    shared_scheduler: Arc<dyn ThreadScheduler>,
    
    /// Dedicated thread pool scheduler
    dedicated_scheduler: Arc<dyn ThreadScheduler>,
    
    /// Runtime handle for spawning async tasks
    runtime_handle: Handle,
    
    /// Signal for system shutdown
    shutdown_signal: Arc<Notify>,
    
    /// Flag indicating if the system is shutting down
    is_shutting_down: Arc<AtomicBool>,
}

impl fmt::Debug for ThreadActorSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let registry = self.registry.read().unwrap();
        f.debug_struct("ThreadActorSystem")
            .field("config", &self.config)
            .field("actor_count", &registry.len())
            .field("is_shutting_down", &self.is_shutting_down.load(Ordering::Relaxed))
            .finish()
    }
}

/// Implementation of the SystemRef trait for the ThreadActorSystem
/// This allows the system to be referenced by contexts and schedulers
#[async_trait]
impl ContextSystemRef for ThreadActorSystem {
    fn runtime_handle(&self) -> &Handle {
        &self.runtime_handle
    }
    
    fn default_ask_timeout(&self) -> Duration {
        self.config.default_ask_timeout
    }
    
    fn default_backpressure_strategy(&self) -> BackpressureStrategy {
        self.config.default_backpressure_strategy.clone()
    }
    
    async fn spawn_actor(&self, actor: BoxedMessage, config: BoxedMessage, strategy: Option<SupervisorStrategy>) -> ActorResult<BoxedActorRef> {
        // 这只是一个占位实现，实际应该将 actor 和 config 解析并调用 spawn_internal
        Err(ActorError::Other(anyhow!("Not implemented yet")))
    }
}

/// Implementation of the SchedulerSystemRef trait for ThreadActorSystem
impl SchedulerSystemRef for ThreadActorSystem {
    fn lookup(&self, path: &str) -> Option<Arc<dyn Mailbox>> {
        let registry = self.registry.read().unwrap();
        registry.get(path).map(|entry| entry.mailbox.clone())
    }
    
    fn handle_panic(&self, path: &str, error: String) {
        // TODO: Implement supervision strategy based on actor configuration
        error!("Actor at path '{}' panicked: {}", path, error);
        
        // For now, just log the error and potentially stop the actor
        // In the future, implement different supervision strategies
        if let Err(e) = self.stop_by_path(path) {
            error!("Failed to stop panicked actor '{}': {}", path, e);
        }
    }
}

impl ThreadActorSystem {
    /// Create a new ThreadActorSystem with the given configuration and runtime handle
    pub fn new(config: ThreadActorSystemConfig, runtime_handle: Handle) -> Self {
        let config = Arc::new(config);
        let registry = Arc::new(RwLock::new(HashMap::new()));
        let shutdown_signal = Arc::new(Notify::new());
        let is_shutting_down = Arc::new(AtomicBool::new(false));
        
        // Create system reference
        let system_ref: Weak<dyn SchedulerSystemRef + Send + Sync> = Weak::new();
        
        // 先创建系统实例
        let system = Self {
            config: config.clone(),
            registry: registry.clone(),
            shared_scheduler: Arc::new(SharedThreadPool::new(
                config.shared_pool_size,
                runtime_handle.clone(),
                config.shared_queue_capacity,
                Some(system_ref.clone())
            )),
            dedicated_scheduler: Arc::new(DedicatedThreadPool::new(
                runtime_handle.clone(),
                Some(system_ref),
                config.max_dedicated_threads
            )),
            runtime_handle,
            shutdown_signal,
            is_shutting_down,
        };
        
        // TODO: 创建系统之后，需要使用 Arc::downgrade 设置调度器的系统引用
        // 这需要处理循环引用问题，后续完善
        
        system
    }
    
    /// Get a reference to the system configuration
    pub fn config(&self) -> &Arc<ThreadActorSystemConfig> {
        &self.config
    }
    
    /// Get a reference to the runtime handle
    pub fn runtime_handle(&self) -> &Handle {
        &self.runtime_handle
    }
    
    /// Check if the system is currently shutting down
    pub fn is_shutting_down(&self) -> bool {
        self.is_shutting_down.load(Ordering::Relaxed)
    }
    
    /// Get the shutdown signal for the system
    pub fn shutdown_signal(&self) -> Arc<Notify> {
        self.shutdown_signal.clone()
    }
    
    /// Spawn a new actor within the system
    pub fn spawn_internal<A>(&self, 
                           actor: A,
                           config: A::Config,
                           parent_path: Option<&str>,
                           actor_name: &str) 
                           -> Result<Arc<dyn ActorRef>, SpawnError>
    where
        A: Actor + Send + Sync + 'static,
    {
        if self.is_shutting_down() {
            return Err(SpawnError::SystemShutdown);
        }
        
        // Generate the full actor path
        let path_str = if let Some(parent) = parent_path {
            format!("{}/{}", parent, actor_name)
        } else {
            format!("/{}", actor_name)
        };
        
        // Check if an actor already exists at this path
        {
            let registry = self.registry.read().unwrap();
            if registry.contains_key(&path_str) {
                return Err(SpawnError::ActorPathAlreadyExists(path_str));
            }
        }
        
        // Get the parent actor reference if a parent path was provided
        let parent_ref = if let Some(parent_path) = parent_path {
            let registry = self.registry.read().unwrap();
            registry.get(parent_path)
                .map(|entry| entry.actor_ref.clone())
        } else {
            None
        };
        
        // Extract actor configuration or use defaults
        let actor_config = ThreadActorConfig::default();
        
        // Merge with system defaults
        let config = self.config.merge_with_actor_config(&actor_config);
        
        // Create the appropriate mailbox based on scheduling mode
        let mailbox: Arc<dyn Mailbox> = match config.scheduling_mode.unwrap() {
            crate::thread::config::SchedulingMode::SharedPool { .. } => {
                Arc::new(MpscMailbox::new(
                    config.mailbox_capacity.unwrap_or(1024),
                    ActorPath {
                        path: path_str.clone(),
                        target: Arc::new(()),  // placeholder
                    }
                ))
            },
            crate::thread::config::SchedulingMode::DedicatedThread => {
                Arc::new(SpscRingbufMailbox::new(
                    config.mailbox_capacity.unwrap_or(1024),
                    ActorPath {
                        path: path_str.clone(),
                        target: Arc::new(()),  // placeholder
                    }
                ))
            },
        };
        
        // Create a weak reference to the mailbox
        let weak_mailbox = Arc::downgrade(&mailbox);
        
        // Create the actor path
        let path = ActorPath {
            path: path_str.clone(),
            target: Arc::new(()),  // placeholder, to be replaced
        };
        
        // Create a weak reference to self for the context
        let system_ref = Arc::downgrade(&(Arc::new(self.clone()) as Arc<dyn ContextSystemRef + Send + Sync>));
        
        // Create the context
        let mut context = ThreadContext::new(
            system_ref,
            self.runtime_handle.clone(),
            path.clone(),
            parent_ref.map(|r| Box::new(r) as Box<dyn ActorRef>),
            config.supervisor_strategy.clone().unwrap_or(SupervisorStrategy::Stop),
        );
        
        // Create the actor reference
        let actor_ref = Arc::new(ThreadActorRef::new(
            path.clone(),
            weak_mailbox,
            config.backpressure_strategy.clone().unwrap_or(BackpressureStrategy::Block),
            config.ask_timeout.unwrap_or(Duration::from_secs(5)),
        ));
        
        // 更新ActorPath中的target
        let path = ActorPath {
            path: path_str.clone(),
            target: actor_ref.clone(),
        };
        
        // Set the self reference in the context
        context.set_self_ref(Box::new(actor_ref.clone()));
        
        // Create the actor wrapper
        let mut thread_actor = ThreadActor::new(actor, path.clone());
        
        // 在单独的异步任务中初始化和启动actor
        let actor_ref_clone = actor_ref.clone();
        let runtime = self.runtime_handle.clone();
        
        runtime.spawn(async move {
            // 首先初始化actor
            match thread_actor.initialize(&mut context).await {
                Ok(_) => {
                    // 发送Start消息通知actor已启动
                    let start_msg = Box::new(crate::thread::envelope::ControlMessage::Start);
                    if let Err(e) = thread_actor.process_message(start_msg, &mut context).await {
                        error!("Failed to start actor at {}: {}", path_str, e);
                        return;
                    }
                    
                    // TODO: 启动消息处理循环
                    // 这里应该启动一个循环从mailbox获取消息并处理
                    // 因为这里有很多特定于实现的内容，我们暂时省略细节
                },
                Err(e) => {
                    error!("Failed to initialize actor at {}: {}", path_str, e);
                    // TODO: 通知父Actor初始化失败
                }
            }
        });
        
        // Register the actor in the registry
        {
            let mut registry = self.registry.write().unwrap();
            let entry = ActorRegistryEntry {
                actor_ref: actor_ref.clone(),
                mailbox: mailbox.clone(),
                config: config.clone(),
                supervisor: parent_ref,
            };
            registry.insert(path_str.clone(), entry);
        }
        
        // Schedule the actor based on its scheduling mode
        match config.scheduling_mode.unwrap() {
            crate::thread::config::SchedulingMode::SharedPool { .. } => {
                if let Err(e) = self.shared_scheduler.schedule(&path_str, mailbox, Some(config)) {
                    // 如果调度失败，从注册表中移除actor
                    let mut registry = self.registry.write().unwrap();
                    registry.remove(&path_str);
                    return Err(SpawnError::SchedulerError(e.to_string()));
                }
            },
            crate::thread::config::SchedulingMode::DedicatedThread => {
                if let Err(e) = self.dedicated_scheduler.schedule(&path_str, mailbox, Some(config)) {
                    // 如果调度失败，从注册表中移除actor
                    let mut registry = self.registry.write().unwrap();
                    registry.remove(&path_str);
                    return Err(SpawnError::SchedulerError(e.to_string()));
                }
            },
        }
        
        // Return the actor reference
        Ok(actor_ref)
    }
    
    /// Stop an actor by its path
    pub fn stop_by_path(&self, path: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        if !self.is_shutting_down() {
            // First check if the actor exists
            let (actor_config, actor_ref) = {
                let registry = self.registry.read().unwrap();
                match registry.get(path) {
                    Some(entry) => (entry.config.clone(), entry.actor_ref.clone()),
                    None => return Err(Box::new(SystemError::ActorNotFound(path.to_string()))),
                }
            };
            
            // 发送Stop消息
            let stop_msg = Box::new(crate::thread::envelope::ControlMessage::Stop);
            self.runtime_handle.block_on(async {
                let _ = actor_ref.send(stop_msg).await;
            });
            
            // Deschedule the actor from the appropriate scheduler
            match actor_config.scheduling_mode.unwrap() {
                crate::thread::config::SchedulingMode::SharedPool { .. } => {
                    self.shared_scheduler.deschedule(path)?;
                },
                crate::thread::config::SchedulingMode::DedicatedThread => {
                    self.dedicated_scheduler.deschedule(path)?;
                },
            }
            
            // Remove the actor from the registry
            {
                let mut registry = self.registry.write().unwrap();
                registry.remove(path);
            }
        }
        
        Ok(())
    }
    
    /// Stop an actor by its reference
    pub fn stop_actor(&self, actor_ref: &dyn ActorRef) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.stop_by_path(actor_ref.path().as_str())
    }
    
    /// Get a reference to an actor by its path
    pub fn get_actor(&self, path: &str) -> Option<Arc<dyn ActorRef>> {
        let registry = self.registry.read().unwrap();
        registry.get(path).map(|entry| entry.actor_ref.clone())
    }
    
    /// Watch an actor for termination
    pub async fn watch(&self, watcher: &dyn ActorRef, target: &dyn ActorRef) -> Result<(), SystemError> {
        // 查找actor处理器并添加watcher
        let target_path = target.path();
        let watcher_path = watcher.path();
        
        debug!("Actor at {} watching actor at {} for termination", 
               watcher_path, target_path);
        
        // 检查目标actor是否存在
        let target_ref = self.get_actor(&target_path)
            .ok_or_else(|| SystemError::ActorNotFound(target_path.clone()))?;
        
        // 检查watcher是否存在
        let _ = self.get_actor(&watcher_path)
            .ok_or_else(|| SystemError::ActorNotFound(watcher_path.clone()))?;
        
        // 查找目标actor的处理器并添加watcher
        // 在这个框架中，这需要通过将watcher注册到目标actor的watchers列表中来实现
        // TODO: 实现真正的watcher注册机制
        // 考虑到我们没有直接访问ThreadActor实例的方式，这可能需要通过消息传递来实现
        
        // 发送watch消息到目标actor
        let watch_request = Box::new(WatchRequest {
            watcher_path: watcher_path.clone(),
        });
        
        // 发送消息以注册watcher
        if let Err(e) = target_ref.send(watch_request).await {
            return Err(SystemError::Other(anyhow::anyhow!(
                "Failed to send watch request from {} to {}: {}", 
                watcher_path, target_path, e
            )));
        }
        
        Ok(())
    }
    
    /// Stop watching an actor
    pub async fn unwatch(&self, watcher: &dyn ActorRef, target: &dyn ActorRef) -> Result<(), SystemError> {
        // 查找actor处理器并移除watcher
        let target_path = target.path();
        let watcher_path = watcher.path();
        
        debug!("Actor at {} unwatching actor at {}", 
               watcher_path, target_path);
        
        // 检查目标actor是否存在
        let target_ref = self.get_actor(&target_path)
            .ok_or_else(|| SystemError::ActorNotFound(target_path.clone()))?;
        
        // 检查watcher是否存在
        let _ = self.get_actor(&watcher_path)
            .ok_or_else(|| SystemError::ActorNotFound(watcher_path.clone()))?;
        
        // 发送unwatch消息到目标actor
        let unwatch_request = Box::new(UnwatchRequest {
            watcher_path: watcher_path.clone(),
        });
        
        // 发送消息以取消注册watcher
        if let Err(e) = target_ref.send(unwatch_request).await {
            return Err(SystemError::Other(anyhow::anyhow!(
                "Failed to send unwatch request from {} to {}: {}", 
                watcher_path, target_path, e
            )));
        }
        
        Ok(())
    }
    
    /// 检查actor健康状态
    pub async fn check_health(&self, actor_path: &str) -> Result<ActorState, SystemError> {
        let actor_ref = self.get_actor(actor_path)
            .ok_or_else(|| SystemError::ActorNotFound(actor_path.to_string()))?;
        
        // 发送健康检查消息
        let health_check = Box::new(crate::thread::envelope::ControlMessage::HealthCheck);
        
        match actor_ref.send(health_check).await {
            Ok(response) => {
                // 尝试从响应中提取ActorState
                if let Some(state) = response.downcast_ref::<ActorState>() {
                    Ok(*state)
                } else {
                    Err(SystemError::Other(anyhow::anyhow!(
                        "Failed to extract actor state from health check response"
                    )))
                }
            },
            Err(e) => {
                Err(SystemError::Other(anyhow::anyhow!(
                    "Failed to send health check to actor at {}: {}", 
                    actor_path, e
                )))
            }
        }
    }
    
    /// 处理actor死亡
    pub fn handle_actor_termination(&self, path: &str, reason: Option<String>) {
        let parent_path = {
            // 从路径中提取父路径
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() > 1 {
                let parent_parts = &parts[0..parts.len()-1];
                Some(parent_parts.join("/"))
            } else {
                None
            }
        };
        
        // 如果有父actor，通知父actor子actor已终止
        if let Some(parent_path) = parent_path {
            if let Some(parent_ref) = self.get_actor(&parent_path) {
                let failure_msg = Box::new(crate::thread::envelope::ControlMessage::ChildFailure {
                    path: path.to_string(),
                    reason: reason.unwrap_or_else(|| "Unknown reason".to_string()),
                });
                
                self.runtime_handle.spawn(async move {
                    let _ = parent_ref.send(failure_msg).await;
                });
            }
        }
        
        // 从注册表中移除actor
        let mut registry = self.registry.write().unwrap();
        registry.remove(path);
    }
    
    /// Shutdown the actor system, stopping all actors
    pub async fn shutdown_internal(&self, timeout: Duration) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Set the shutdown flag
        self.is_shutting_down.store(true, Ordering::SeqCst);
        
        // Notify all waiters
        self.shutdown_signal.notify_waiters();
        
        // 通知所有actor系统正在关闭
        let shutdown_msg = Box::new(crate::thread::envelope::ControlMessage::SystemShutdown);
        {
            let registry = self.registry.read().unwrap();
            for entry in registry.values() {
                let actor_ref = entry.actor_ref.clone();
                let shutdown_clone = shutdown_msg.clone();
                self.runtime_handle.spawn(async move {
                    let _ = actor_ref.send(shutdown_clone).await;
                });
            }
        }
        
        // 给actor一些时间处理关闭消息
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Collect all actor paths
        let paths = {
            let registry = self.registry.read().unwrap();
            registry.keys().cloned().collect::<Vec<_>>()
        };
        
        // Stop all actors
        for path in paths {
            if let Err(e) = self.stop_by_path(&path) {
                error!("Error stopping actor '{}' during shutdown: {}", path, e);
            }
        }
        
        // Shutdown the schedulers
        let shared_scheduler = self.shared_scheduler.clone();
        let dedicated_scheduler = self.dedicated_scheduler.clone();
        
        let shared_future = tokio::task::spawn(async move {
            if let Err(e) = shared_scheduler.shutdown() {
                error!("Error shutting down shared scheduler: {}", e);
            }
        });
        
        let dedicated_future = tokio::task::spawn(async move {
            if let Err(e) = dedicated_scheduler.shutdown() {
                error!("Error shutting down dedicated scheduler: {}", e);
            }
        });
        
        // Wait for scheduler shutdown with timeout
        match tokio::time::timeout(timeout, async {
            let _ = shared_future.await;
            let _ = dedicated_future.await;
        }).await {
            Ok(_) => {
                info!("Actor system shutdown completed gracefully");
                Ok(())
            },
            Err(_) => {
                warn!("Actor system shutdown timed out after {:?}", timeout);
                Err(Box::new(SystemError::Timeout(format!("Actor system shutdown timed out after {:?}", timeout))))
            }
        }
    }
}

impl Clone for ThreadActorSystem {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            registry: self.registry.clone(),
            shared_scheduler: self.shared_scheduler.clone(),
            dedicated_scheduler: self.dedicated_scheduler.clone(),
            runtime_handle: self.runtime_handle.clone(),
            shutdown_signal: self.shutdown_signal.clone(),
            is_shutting_down: self.is_shutting_down.clone(),
        }
    }
}

// Implement the ActorSystem trait
#[async_trait]
impl ActorSystem for ThreadActorSystem {
    async fn start(config: ActorSystemConfig) -> Result<Self, parrot_api::system::SystemError> {
        // 将通用ActorSystemConfig转换为特定于线程实现的ThreadActorSystemConfig
        let thread_config = ThreadActorSystemConfig {
            shared_pool_size: config.runtime_config.thread_pool_size.unwrap_or_else(num_cpus::get),
            shared_queue_capacity: 10000, // 默认值
            max_dedicated_threads: 32,    // 默认值
            default_scheduling_mode: crate::thread::config::SchedulingMode::SharedPool { max_messages_per_run: 10 },
            default_mailbox_capacity: 1024,
            default_ask_timeout: config.timeouts.message_handling,
            default_supervisor_strategy: SupervisorStrategy::Restart { 
                max_retries: config.guardian_config.max_restarts as usize,
                within: config.guardian_config.restart_window,
            },
            default_backpressure_strategy: BackpressureStrategy::Block,
            shutdown_timeout: config.timeouts.system_shutdown,
        };
        
        // 获取或创建runtime句柄
        let runtime_handle = Handle::try_current()
            .map_err(|e| parrot_api::system::SystemError::InitializationError(
                format!("Failed to get tokio runtime handle: {}", e)
            ))?;
        
        // 创建线程actor系统
        let system = Self::new(thread_config, runtime_handle);
        
        Ok(system)
    }
    
    async fn spawn_root_typed<A>(&self, actor: A, config: A::Config) -> Result<Box<dyn ActorRef>, parrot_api::system::SystemError>
    where
        A: Actor + Send + 'static,
    {
        let actor_name = format!("actor-{}", uuid::Uuid::new_v4());
        
        match self.spawn_internal(actor, config, None, &actor_name) {
            Ok(actor_ref) => Ok(Box::new(actor_ref) as Box<dyn ActorRef>),
            Err(e) => Err(parrot_api::system::SystemError::ActorCreationError(e.to_string())),
        }
    }
    
    async fn spawn_root_boxed(
        &self,
        actor: Box<dyn Actor<Config = Box<dyn Any + Send>, Context = dyn ActorContext>>,
        config: Box<dyn Any + Send>,
    ) -> Result<Box<dyn ActorRef>, parrot_api::system::SystemError> {
        Err(parrot_api::system::SystemError::ActorCreationError(
            "Thread actor system does not support boxed actors yet".to_string()
        ))
    }
    
    async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        self.get_actor(&path.path).map(|actor_ref| Box::new(actor_ref) as Box<dyn ActorRef>)
    }
    
    async fn broadcast<M: parrot_api::message::Message + Clone>(&self, msg: M) -> Result<(), parrot_api::system::SystemError> {
        let registry = self.registry.read().unwrap();
        for entry in registry.values() {
            // Clone message for each actor
            let msg_clone = msg.clone();
            let actor_ref = entry.actor_ref.clone();
            
            // Send in background to avoid blocking
            self.runtime_handle.spawn(async move {
                if let Err(e) = actor_ref.send(Box::new(msg_clone) as BoxedMessage).await {
                    error!("Error broadcasting message: {}", e);
                }
            });
        }
        
        Ok(())
    }
    
    fn status(&self) -> SystemStatus {
        let registry = self.registry.read().unwrap();
        
        // 确定系统当前状态
        let state = if self.is_shutting_down() {
            SystemState::ShuttingDown
        } else {
            SystemState::Running
        };
        
        // TODO: 实现真实的资源统计
        SystemStatus {
            state,
            active_actors: registry.len(),
            uptime: Duration::from_secs(0), // 这里应该跟踪实际的运行时间
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: 0,
            },
        }
    }
    
    async fn shutdown(self) -> Result<(), parrot_api::system::SystemError> {
        let timeout = self.config.shutdown_timeout;
        self.shutdown_internal(timeout).await
            .map_err(|e| parrot_api::system::SystemError::Other(anyhow!("{}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::runtime::Builder;
    
    #[test]
    fn test_system_creation() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        
        let config = ThreadActorSystemConfig::default();
        let system = ThreadActorSystem::new(config, runtime.handle().clone());
        
        assert_eq!(system.is_shutting_down(), false);
        assert!(system.registry.read().unwrap().is_empty());
    }
    
    #[test]
    fn test_system_shutdown() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        
        let config = ThreadActorSystemConfig::default();
        let system = ThreadActorSystem::new(config, runtime.handle().clone());
        
        // 简单测试关闭系统
        runtime.block_on(async {
            let result = system.shutdown_internal(Duration::from_millis(100)).await;
            assert!(result.is_ok(), "System shutdown failed: {:?}", result);
            assert_eq!(system.is_shutting_down(), true);
        });
    }
}

/// 请求对一个actor进行监视
#[derive(Debug)]
pub struct WatchRequest {
    /// 观察者的路径
    pub watcher_path: ActorPath,
}

/// 请求停止监视一个actor
#[derive(Debug)]
pub struct UnwatchRequest {
    /// 观察者的路径
    pub watcher_path: ActorPath,
} 