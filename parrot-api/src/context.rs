use std::time::Duration;
use std::any::Any;
use std::sync::Arc;
use std::hash::Hash;
use async_trait::async_trait;
use crate::actor::Actor;
use crate::errors::ActorError;
use crate::address::{ActorPath, ActorRef};
use crate::types::{BoxedActorRef, BoxedMessage, ActorResult, BoxedFuture};
use crate::message::{Message, MessageEnvelope};
use crate::supervisor::SupervisorStrategyType;
use crate::stream::StreamRegistry;
use crate::supervisor::SupervisorStrategy;

/// Actor spawning functionality
pub trait ActorSpawner: Send + Sync {
    /// Spawn a new actor
    fn spawn<'a>(&'a self, actor: BoxedMessage, config: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedActorRef>>;
    
    /// Spawn a new supervised actor
    fn spawn_with_strategy<'a>(&'a self, actor: BoxedMessage, config: BoxedMessage, strategy: SupervisorStrategyType) -> BoxedFuture<'a, ActorResult<BoxedActorRef>>;
}

/// Extension trait for type-safe actor spawning
pub trait ActorSpawnerExt: ActorSpawner {
    /// Spawn a new actor with type information
    fn spawn_typed<'a, A: Actor>(&'a self, actor: A, config: A::Config) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        self.spawn(Box::new(actor), Box::new(config))
    }
    
    /// Spawn a new supervised actor with type information
    fn spawn_supervised<'a, A: Actor>(&'a self, actor: A, config: A::Config, strategy: SupervisorStrategyType) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        self.spawn_with_strategy(Box::new(actor), Box::new(config), strategy)
    }
}

impl<T: ActorSpawner + ?Sized> ActorSpawnerExt for T {}

/// Actor factory trait for creating new actor instances
#[async_trait]
pub trait ActorFactory: Send + 'static {
    /// Actor type
    type ActorType: Actor;
    
    /// Create an actor instance
    async fn create_actor(&self) -> ActorResult<(Self::ActorType, <Self::ActorType as Actor>::Config)>;
}

/// Actor context functionality
pub trait ActorContext: Send + Sync {
    /// Get actor self reference
    fn get_self_ref(&self) -> BoxedActorRef;

    /// Stop the actor
    fn stop<'a>(&'a mut self) -> BoxedFuture<'a, ActorResult<()>>;

    /// Send message to another actor
    fn send<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>>;

    /// Send message to another actor and wait for response
    fn ask<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>>;

    /// Schedule a message to be sent after delay
    fn schedule_once<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage, delay: Duration) -> BoxedFuture<'a, ActorResult<()>>;

    /// Schedule a message to be sent periodically
    fn schedule_periodic<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage, initial_delay: Duration, interval: Duration) -> BoxedFuture<'a, ActorResult<()>>;

    /// Watch another actor for termination
    fn watch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>>;

    /// Unwatch another actor
    fn unwatch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>>;

    /// Get parent Actor reference
    fn parent(&self) -> Option<BoxedActorRef>;
    
    /// Get child Actor references
    fn children(&self) -> Vec<BoxedActorRef>;
    
    /// Set receive timeout
    fn set_receive_timeout(&mut self, timeout: Option<Duration>);
    
    /// Get receive timeout
    fn receive_timeout(&self) -> Option<Duration>;
    
    /// Set supervisor strategy
    fn set_supervisor_strategy(&mut self, strategy: SupervisorStrategyType);
    
    /// Get Actor path
    fn path(&self) -> &ActorPath;

    /// Get stream registry
    fn stream_registry(&mut self) -> &mut dyn StreamRegistry;

    /// Get spawner interface
    fn spawner(&mut self) -> &mut dyn ActorSpawner;
}

/// Actor context extension trait for message handling
pub trait ActorContextMessage: ActorContext {
    /// Send message to self
    fn send_self<M: Message>(&self, msg: M) -> ActorResult<()> {
        let envelope = MessageEnvelope::new(msg, None, None);
        let self_ref = self.get_self_ref();
        tokio::spawn(async move {
            let _ = self_ref.send(envelope).await;
        });
        Ok(())
    }
    
    /// Send message to self and wait for response
    async fn ask_self<M: Message>(&self, msg: M) -> ActorResult<M::Result> {
        let envelope = MessageEnvelope::new(msg, None, None);
        let result = self.get_self_ref().send(envelope).await?;
        M::extract_result(result)
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
    ) -> ActorResult<ScheduledTask>;
    
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
        self.target.path() == other.target.path() && 
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
    ChildTerminated(BoxedActorRef),
    /// Watched Actor terminated
    Terminated(BoxedActorRef),
    /// Receive timeout
    ReceiveTimeout,
} 