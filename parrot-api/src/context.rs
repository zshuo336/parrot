//! # Actor Context Module
//! 
//! ## Key Concepts
//! - ActorContext: Primary interface for system interaction
//! - ActorSpawner: Actor creation and supervision
//! - ActorFactory: Actor instantiation patterns
//! - LifecycleEvent: Actor lifecycle management
//!
//! ## Design Principles
//! - Encapsulation: Actors interact with system only through context
//! - Type Safety: Generic interfaces for type-safe operations
//! - Resource Control: Managed access to system resources
//! - Lifecycle Management: Comprehensive actor lifecycle control
//!
//! ## Thread Safety
//! All components are designed to be thread-safe and support concurrent access
//!
//! ## Performance Considerations
//! - Uses async/await for non-blocking operations
//! - Minimizes allocations where possible
//! - Employs type erasure for interface flexibility

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

/// # Actor Spawner
///
/// ## Overview
/// Interface for creating and managing new actors in the system
///
/// ## Key Responsibilities
/// - Actor creation
/// - Supervision setup
/// - Configuration management
///
/// ## Thread Safety
/// All methods are thread-safe and can be called concurrently
#[async_trait]
pub trait ActorSpawner: Send + Sync {
    /// Creates new actor instance
    ///
    /// ## Parameters
    /// - `actor`: Type-erased actor instance
    /// - `config`: Actor configuration
    ///
    /// ## Returns
    /// - `Ok(ActorRef)`: Successfully created actor
    /// - `Err(Error)`: Creation failed
    ///
    /// ## Performance
    /// - Allocates actor on heap
    /// - Performs async initialization
    fn spawn<'a>(&'a self, actor: BoxedMessage, config: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedActorRef>>;
    
    /// Creates supervised actor
    ///
    /// ## Parameters
    /// - `actor`: Type-erased actor instance
    /// - `config`: Actor configuration
    /// - `strategy`: Supervision strategy
    ///
    /// ## Implementation Details
    /// 1. Creates actor instance
    /// 2. Sets up supervision
    /// 3. Initializes actor
    fn spawn_with_strategy<'a>(&'a self, 
        actor: BoxedMessage, 
        config: BoxedMessage, 
        strategy: SupervisorStrategyType
    ) -> BoxedFuture<'a, ActorResult<BoxedActorRef>>;
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

/// # Actor Context
///
/// ## Overview
/// Core interface for actor system interaction
///
/// ## Key Responsibilities
/// - Message passing
/// - Lifecycle management
/// - Child actor supervision
/// - System service access
///
/// ## Thread Safety
/// - All methods are thread-safe
/// - Internal state is protected
///
/// ## Performance
/// - Async operations
/// - Minimized blocking
pub trait ActorContext: Send + Sync {

    /// Returns self reference
    ///
    /// ## Thread Safety
    /// Safe to call from any thread
    fn get_self_ref(&self) -> BoxedActorRef;

    /// Initiates actor shutdown
    ///
    /// ## Implementation Details
    /// 1. Stops child actors
    /// 2. Cancels pending messages
    /// 3. Runs cleanup
    fn stop<'a>(&'a mut self) -> BoxedFuture<'a, ActorResult<()>>;

    /// Sends message without response
    ///
    /// ## Parameters
    /// - `target`: Recipient actor
    /// - `msg`: Message to send
    ///
    /// ## Performance
    /// - Non-blocking operation
    /// - May allocate on heap
    fn send<'a>(&'a self, target: BoxedActorRef, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>>;

    /// Sends message and awaits response
    ///
    /// ## Parameters
    /// - `target`: Recipient actor
    /// - `msg`: Message to send
    ///
    /// ## Returns
    /// - `Ok(response)`: Successful response
    /// - `Err(error)`: Communication failed
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

    /// Sets the parent actor reference.
    fn set_parent(&mut self, parent: BoxedActorRef);

    /// Returns reference to parent actor if it exists.
    fn parent(&self) -> Option<BoxedActorRef>;
    
    /// Returns references to all child actors.
    fn children(&self) -> Option<Arc<Vec<BoxedActorRef>>>;
    
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
        let self_ref = self.get_self_ref();
        tokio::spawn(async move {
            let _ = self_ref.send(Box::new(msg) as BoxedMessage).await;
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
        let result = self.get_self_ref().send(Box::new(msg) as BoxedMessage).await?;
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

/// # Scheduled Task
///
/// ## Overview
/// Represents a scheduled message delivery
///
/// ## Key Responsibilities
/// - Unique task identification
/// - Target actor reference
/// - Execution timing
///
/// ## Thread Safety
/// - Implements Send + Sync
/// - Safe for concurrent access
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    /// Unique task identifier
    /// - **Uniqueness**: Guaranteed unique within system
    /// - **Persistence**: Stable across restarts
    pub id: uuid::Uuid,
    
    /// Target actor reference
    /// - **Thread Safety**: Arc ensures safe sharing
    /// - **Lifecycle**: Strong reference to prevent cleanup
    pub target: Arc<dyn ActorRef>,
    
    /// Scheduled execution time
    /// - **Precision**: System time precision
    /// - **Timezone**: UTC based
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

/// # Lifecycle Events
///
/// ## Overview
/// Events representing actor lifecycle transitions
///
/// ## Key Events
/// - Started: Initialization complete
/// - Stopped: Termination complete
/// - ChildTerminated: Child actor stopped
/// - Terminated: Watched actor stopped
/// - ReceiveTimeout: Message timeout
///
/// ## Usage
/// Used for monitoring and reacting to actor state changes
#[derive(Debug)]
pub enum LifecycleEvent {
    /// Actor started successfully
    Started,
    
    /// Actor stopped completely
    Stopped,
    
    /// Child actor terminated
    /// - **Parameter**: Reference to terminated child
    ChildTerminated(BoxedActorRef),
    
    /// Watched actor terminated
    /// - **Parameter**: Reference to terminated actor
    Terminated(BoxedActorRef),
    
    /// Message receive timeout occurred
    ReceiveTimeout,
} 