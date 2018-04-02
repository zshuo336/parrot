//! # Actor Context and Lifecycle Management
//! 
//! This module defines the execution context and lifecycle management facilities
//! for actors in the Parrot framework. The context provides actors with access
//! to system services and their runtime environment.
//!
//! ## Design Philosophy
//!
//! The context system is designed with these principles:
//! - Encapsulation: Actors interact with the system only through their context
//! - Type Safety: Generic interfaces for type-safe message passing
//! - Lifecycle Management: Comprehensive control over actor lifecycle
//! - Resource Control: Managed access to system resources
//!
//! ## Core Components
//!
//! - `ActorContext`: Primary interface for actor system interaction
//! - `ActorSpawner`: Actor creation and supervision
//! - `ActorFactory`: Actor instantiation patterns
//! - `LifecycleEvent`: Actor lifecycle notifications
//!
//! ## Usage Example
//!
//! ```rust
//! use parrot_api::context::{ActorContext, ActorContextMessage};
//! use parrot_api::message::Message;
//! use std::time::Duration;
//!
//! struct MyActor {
//!     context: Box<dyn ActorContext>,
//! }
//!
//! impl MyActor {
//!     async fn handle_message(&mut self, msg: MyMessage) {
//!         // Send message to another actor
//!         let other_actor = self.context.get_actor("other").await?;
//!         self.context.ask(other_actor, msg).await?;
//!
//!         // Schedule periodic task
//!         self.context.schedule_periodic(
//!             self.context.get_self_ref(),
//!             TickMessage,
//!             Duration::from_secs(0),
//!             Duration::from_secs(60)
//!         ).await?;
//!     }
//! }
//! ```

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

/// Interface for creating new actors in the system.
///
/// This trait provides the core functionality for spawning actors
/// and managing their supervision relationships.
pub trait ActorSpawner: Send + Sync {
    /// Creates a new actor instance in the system.
    ///
    /// # Parameters
    /// * `actor` - Type-erased actor instance
    /// * `config` - Type-erased actor configuration
    ///
    /// # Returns
    /// Future resolving to the actor reference or error
    fn spawn<'a>(&'a self, actor: BoxedMessage, config: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedActorRef>>;
    
    /// Creates a new actor with specified supervision strategy.
    ///
    /// # Parameters
    /// * `actor` - Type-erased actor instance
    /// * `config` - Type-erased actor configuration
    /// * `strategy` - Supervision strategy for the actor
    ///
    /// # Returns
    /// Future resolving to the actor reference or error
    fn spawn_with_strategy<'a>(&'a self, actor: BoxedMessage, config: BoxedMessage, strategy: SupervisorStrategyType) -> BoxedFuture<'a, ActorResult<BoxedActorRef>>;
}

/// Type-safe extension methods for actor spawning.
///
/// This trait provides convenience methods for spawning actors
/// while preserving their type information.
pub trait ActorSpawnerExt: ActorSpawner {
    /// Spawns a new actor with preserved type information.
    ///
    /// # Type Parameters
    /// * `A` - Concrete actor type
    ///
    /// # Parameters
    /// * `actor` - Typed actor instance
    /// * `config` - Typed actor configuration
    ///
    /// # Returns
    /// Future resolving to the actor reference or error
    fn spawn_typed<'a, A: Actor>(&'a self, actor: A, config: A::Config) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        self.spawn(Box::new(actor), Box::new(config))
    }
    
    /// Spawns a supervised actor with preserved type information.
    ///
    /// # Type Parameters
    /// * `A` - Concrete actor type
    ///
    /// # Parameters
    /// * `actor` - Typed actor instance
    /// * `config` - Typed actor configuration
    /// * `strategy` - Supervision strategy
    ///
    /// # Returns
    /// Future resolving to the actor reference or error
    fn spawn_supervised<'a, A: Actor>(&'a self, actor: A, config: A::Config, strategy: SupervisorStrategyType) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
        self.spawn_with_strategy(Box::new(actor), Box::new(config), strategy)
    }
}

impl<T: ActorSpawner + ?Sized> ActorSpawnerExt for T {}

/// Factory for creating actor instances.
///
/// This trait defines how new instances of a specific actor type
/// should be created, including their initial configuration.
#[async_trait]
pub trait ActorFactory: Send + 'static {
    /// The type of actor this factory creates
    type ActorType: Actor;
    
    /// Creates a new instance of the actor.
    ///
    /// # Returns
    /// Future resolving to the actor instance and its configuration
    async fn create_actor(&self) -> ActorResult<(Self::ActorType, <Self::ActorType as Actor>::Config)>;
}

/// Core interface for actor system interaction.
///
/// This trait provides actors with access to:
/// - Message passing facilities
/// - Lifecycle management
/// - Child actor supervision
/// - System services
pub trait ActorContext: Send + Sync {
    /// Returns a reference to this actor.
    fn get_self_ref(&self) -> BoxedActorRef;

    /// Initiates actor shutdown.
    ///
    /// # Returns
    /// Future completing when the actor has stopped
    fn stop<'a>(&'a mut self) -> BoxedFuture<'a, ActorResult<()>>;

    /// Sends a message to another actor without waiting for response.
    ///
    /// # Parameters
    /// * `target` - Recipient actor reference
    /// * `msg` - Message to send
    ///
    /// # Returns
    /// Future completing when message is sent
    fn send<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>>;

    /// Sends a message and waits for response.
    ///
    /// # Parameters
    /// * `target` - Recipient actor reference
    /// * `msg` - Message to send
    ///
    /// # Returns
    /// Future resolving to the response message
    fn ask<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>>;

    /// Schedules a one-time delayed message.
    ///
    /// # Parameters
    /// * `target` - Recipient actor reference
    /// * `msg` - Message to send
    /// * `delay` - Time to wait before sending
    ///
    /// # Returns
    /// Future completing when message is scheduled
    fn schedule_once<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage, delay: Duration) -> BoxedFuture<'a, ActorResult<()>>;

    /// Schedules a recurring message.
    ///
    /// # Parameters
    /// * `target` - Recipient actor reference
    /// * `msg` - Message to send
    /// * `initial_delay` - Delay before first message
    /// * `interval` - Time between messages
    ///
    /// # Returns
    /// Future completing when schedule is set
    fn schedule_periodic<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage, initial_delay: Duration, interval: Duration) -> BoxedFuture<'a, ActorResult<()>>;

    /// Registers for termination notification of another actor.
    ///
    /// # Parameters
    /// * `target` - Actor to watch
    ///
    /// # Returns
    /// Future completing when watch is registered
    fn watch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>>;

    /// Removes termination notification registration.
    ///
    /// # Parameters
    /// * `target` - Actor to unwatch
    ///
    /// # Returns
    /// Future completing when watch is removed
    fn unwatch<'a>(&'a mut self, target: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>>;

    /// Returns reference to parent actor if it exists.
    fn parent(&self) -> Option<BoxedActorRef>;
    
    /// Returns references to all child actors.
    fn children(&self) -> Vec<BoxedActorRef>;
    
    /// Sets timeout for receiving messages.
    ///
    /// # Parameters
    /// * `timeout` - Duration after which timeout occurs, or None to disable
    fn set_receive_timeout(&mut self, timeout: Option<Duration>);
    
    /// Returns current receive timeout setting.
    fn receive_timeout(&self) -> Option<Duration>;
    
    /// Sets supervision strategy for child actors.
    ///
    /// # Parameters
    /// * `strategy` - Strategy to use for handling child failures
    fn set_supervisor_strategy(&mut self, strategy: SupervisorStrategyType);
    
    /// Returns the actor's unique path in the system.
    fn path(&self) -> &ActorPath;

    /// Returns mutable access to stream processing registry.
    fn stream_registry(&mut self) -> &mut dyn StreamRegistry;

    /// Returns mutable access to actor spawning interface.
    fn spawner(&mut self) -> &mut dyn ActorSpawner;
}

/// Extension methods for message handling.
///
/// Provides convenience methods for sending messages to self
/// and handling responses.
pub trait ActorContextMessage: ActorContext {
    /// Sends a message to self without waiting for response.
    ///
    /// # Type Parameters
    /// * `M` - Message type
    ///
    /// # Parameters
    /// * `msg` - Message to send
    ///
    /// # Returns
    /// Result indicating success or failure
    fn send_self<M: Message>(&self, msg: M) -> ActorResult<()> {
        let envelope = MessageEnvelope::new(msg, None, None);
        let self_ref = self.get_self_ref();
        tokio::spawn(async move {
            let _ = self_ref.send(envelope).await;
        });
        Ok(())
    }
    
    /// Sends a message to self and waits for response.
    ///
    /// # Type Parameters
    /// * `M` - Message type
    ///
    /// # Parameters
    /// * `msg` - Message to send
    ///
    /// # Returns
    /// Future resolving to the response
    async fn ask_self<M: Message>(&self, msg: M) -> ActorResult<M::Result> {
        let envelope = MessageEnvelope::new(msg, None, None);
        let result = self.get_self_ref().send(envelope).await?;
        M::extract_result(result)
    }
}

impl<T: ActorContext + ?Sized> ActorContextMessage for T {}

/// Extension methods for message scheduling.
///
/// Provides functionality for scheduling delayed and periodic messages.
#[async_trait]
pub trait ActorContextScheduler: ActorContext {
    /// Creates a new scheduled message task.
    ///
    /// # Type Parameters
    /// * `M` - Message type
    ///
    /// # Parameters
    /// * `msg` - Message to schedule
    /// * `delay` - Initial delay
    /// * `interval` - Optional interval for recurring messages
    ///
    /// # Returns
    /// Future resolving to the scheduled task
    async fn schedule<M: Message>(
        &self,
        msg: M,
        delay: Duration,
        interval: Option<Duration>,
    ) -> ActorResult<ScheduledTask>;
    
    /// Cancels a scheduled task.
    ///
    /// # Parameters
    /// * `task` - Task to cancel
    fn cancel_schedule(&self, task: ScheduledTask);
}

/// Represents a scheduled message task.
///
/// Contains information about a scheduled message including
/// its target and timing.
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    /// Unique identifier for the task
    pub id: uuid::Uuid,
    
    /// Actor that will receive the message
    pub target: Arc<dyn ActorRef>,
    
    /// When the task is scheduled to execute
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

/// Events representing changes in actor lifecycle.
///
/// These events are used to notify actors of important
/// lifecycle transitions and system events.
#[derive(Debug)]
pub enum LifecycleEvent {
    /// Actor has completed initialization
    Started,
    
    /// Actor has terminated
    Stopped,
    
    /// A child actor has terminated
    ChildTerminated(BoxedActorRef),
    
    /// A watched actor has terminated
    Terminated(BoxedActorRef),
    
    /// No message received within timeout period
    ReceiveTimeout,
} 