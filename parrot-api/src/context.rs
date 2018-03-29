use std::time::Duration;
use std::any::Any;
use std::sync::Arc;
use std::hash::Hash;
use async_trait::async_trait;
use crate::actor::Actor;
use crate::errors::ActorError;
use crate::address::{ActorPath, ActorRef};
use crate::message::{Message, MessageEnvelope};
use crate::supervisor::SupervisorStrategyType;
use crate::stream::StreamRegistry;

/// Actor spawning trait for specific actor types
#[async_trait]
pub trait TypedActorSpawner<A: Actor>: Send {
    /// Create a child Actor of specific type
    async fn spawn_typed_actor(&mut self, actor: A, config: A::Config) -> Result<Box<dyn ActorRef>, ActorError>;
}

/// Actor spawning trait
#[async_trait]
pub trait ActorSpawner: Send {
    /// Create a child Actor from a boxed actor and config
    async fn spawn_boxed_actor(
        &mut self,
        actor: Box<dyn Actor<Config = Box<dyn Any + Send>>>,
        config: Box<dyn Any + Send>,
    ) -> Result<Box<dyn ActorRef>, ActorError>;
}

/// Actor factory trait
#[async_trait]
pub trait ActorFactory: Send + 'static {
    /// Actor type
    type ActorType: Actor;
    
    /// Create an actor instance
    async fn create_actor(&self) -> Result<(Self::ActorType, <Self::ActorType as Actor>::Config), ActorError>;
}

/// Actor context trait
#[async_trait]
pub trait ActorContext: Send {
    /// Get Actor's self reference
    fn self_ref(&self) -> Box<dyn ActorRef>;
    
    /// Get parent Actor reference
    fn parent(&self) -> Option<Box<dyn ActorRef>>;
    
    /// Get child Actor references
    fn children(&self) -> Vec<Box<dyn ActorRef>>;
    
    /// Get spawner interface
    fn spawner(&mut self) -> &mut dyn ActorSpawner;
    
    /// Set receive timeout
    fn set_receive_timeout(&mut self, timeout: Option<Duration>);
    
    /// Get receive timeout
    fn receive_timeout(&self) -> Option<Duration>;
    
    /// Stop self
    fn stop_self(&mut self);
    
    /// Watch another Actor
    fn watch(&mut self, other: Box<dyn ActorRef>);
    
    /// Unwatch an Actor
    fn unwatch(&mut self, other: Box<dyn ActorRef>);
    
    /// Set supervisor strategy
    fn set_supervisor_strategy(&mut self, strategy: SupervisorStrategyType);
    
    /// Get Actor path
    fn path(&self) -> &ActorPath;

    /// Get stream registry
    fn stream_registry(&mut self) -> &mut dyn StreamRegistry;
}

/// Actor context extension trait for message handling
pub trait ActorContextMessage: ActorContext {
    /// Send message to self
    fn send_self<M: Message>(&self, msg: M) -> Result<(), ActorError> {
        let envelope = MessageEnvelope::new(msg, None, None);
        let self_ref = self.self_ref();
        tokio::spawn(async move {
            let _ = self_ref.send(envelope).await;
        });
        Ok(())
    }
    
    /// Send message to self and wait for response
    async fn ask_self<M: Message>(&self, msg: M) -> Result<M::Result, ActorError> {
        let envelope = MessageEnvelope::new(msg, None, None);
        let result = self.self_ref().send(envelope).await?;
        result.downcast::<M::Result>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("Failed to downcast message result".to_string()))
    }
}

// Implement ActorContextMessage for all types that implement ActorContext
impl<T: ActorContext + ?Sized> ActorContextMessage for T {}

/// Actor context extension trait for scheduling
#[async_trait]
pub trait ActorContextScheduler: ActorContext {
    /// Create scheduled message
    async fn schedule<M: Message>(
        &self,
        msg: M,
        delay: Duration,
        interval: Option<Duration>,
    ) -> Result<ScheduledTask, ActorError>;
    
    /// Cancel scheduled task
    fn cancel_schedule(&self, task: ScheduledTask);
}

/// Scheduled task
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    /// Task ID
    pub id: uuid::Uuid,
    /// Target Actor
    pub target: Arc<dyn ActorRef>,
    /// Schedule time
    pub schedule_time: std::time::SystemTime,
}

impl PartialEq for ScheduledTask {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && 
        self.target.eq_ref(&*other.target) && 
        self.schedule_time == other.schedule_time
    }
}

impl Eq for ScheduledTask {}

impl Hash for ScheduledTask {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.schedule_time.hash(state);
    }
}

/// Actor lifecycle events
#[derive(Debug)]
pub enum LifecycleEvent {
    /// Actor started
    Started,
    /// Actor stopped
    Stopped,
    /// Child Actor terminated
    ChildTerminated(Box<dyn ActorRef>),
    /// Watched Actor terminated
    Terminated(Box<dyn ActorRef>),
    /// Receive timeout
    ReceiveTimeout,
} 