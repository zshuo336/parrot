use std::time::Duration;

pub const DEFAULT_DEDICATED_THREAD_MAILBOX_CAPACITY: usize = 102400;

// --- Configuration Enums ---

/// Determines how an actor's message processing is scheduled onto threads.
#[derive(Clone, Debug)]
pub enum SchedulingMode {
    /// Actor runs in the shared thread pool.
    SharedPool {
        /// Max messages to process in one scheduling run before yielding.
        max_messages_per_run: usize,
    },
    /// Actor runs exclusively on its own dedicated system thread.
    DedicatedThread,
}

/// Defines the behavior when `mailbox.push()` is called on a full mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackpressureStrategy {
    /// The `push` operation will asynchronously wait until space becomes available.
    Block,
    /// The `push` operation will immediately return a `MailboxError::Full` error.
    Error,
    /// The oldest message currently in the mailbox will be dropped to make space.
    DropOldest,
    /// The new message being pushed will be dropped.
    DropNewest,
}

/// Defines how a supervisor actor should react when a direct child actor fails (panics).
#[derive(Clone, Debug)]
pub enum SupervisorStrategy {
    /// Restart the child actor immediately.
    Restart {
        max_retries: usize,
        within: Duration,
    },
    /// Stop the child actor permanently.
    Stop,
    /// Stop the child actor and escalate the failure by stopping the supervisor itself.
    Escalate,
}

// --- System Configuration ---

/// Configuration for the `ThreadActorSystem`.
#[derive(Clone, Debug)]
pub struct ThreadActorSystemConfig {
    /// The number of worker threads in the shared thread pool.
    pub shared_pool_size: usize,
    
    /// The capacity of the shared scheduling queue.
    pub shared_queue_capacity: usize,
    
    /// The maximum number of dedicated threads allowed in the system.
    pub max_dedicated_threads: usize,
    
    /// The default scheduling mode assigned to new actors if not specified.
    pub default_scheduling_mode: SchedulingMode,
    
    /// The default capacity for actor mailboxes if not specified.
    pub default_mailbox_capacity: usize,
    
    /// The default timeout duration for `ask` operations.
    pub default_ask_timeout: Duration,
    
    /// The default supervision strategy applied to actors.
    pub default_supervisor_strategy: SupervisorStrategy,
    
    /// The default backpressure strategy used by mailboxes.
    pub default_backpressure_strategy: BackpressureStrategy,
    
    /// The timeout for system shutdown.
    pub shutdown_timeout: Duration,
    
    // pub dedicated_thread_affinity_strategy: Option<Box<dyn Fn(ActorPath) -> Option<usize> + Send + Sync>>,
    // pub thread_name_prefix: String,
}

impl Default for ThreadActorSystemConfig {
    fn default() -> Self {
        Self {
            shared_pool_size: num_cpus::get(),
            shared_queue_capacity: 10000,
            max_dedicated_threads: 32,
            default_scheduling_mode: SchedulingMode::SharedPool { max_messages_per_run: 10 },
            default_mailbox_capacity: 1024,
            default_ask_timeout: Duration::from_secs(5),
            default_supervisor_strategy: SupervisorStrategy::Restart { max_retries: 3, within: Duration::from_secs(10) },
            default_backpressure_strategy: BackpressureStrategy::Block,
            shutdown_timeout: Duration::from_secs(10),
            // dedicated_thread_affinity_strategy: None,
            // thread_name_prefix: "parrot-thread-worker-".to_string(),
        }
    }
}

impl ThreadActorSystemConfig {
    /// Merge system configuration with actor-specific configuration.
    /// This applies defaults from the system config where the actor config doesn't specify values.
    pub fn merge_with_actor_config(&self, actor_config: &ThreadActorConfig) -> ThreadActorConfig {
        ThreadActorConfig {
            scheduling_mode: actor_config.scheduling_mode.clone().or_else(|| Some(self.default_scheduling_mode.clone())),
            mailbox_capacity: actor_config.mailbox_capacity.or(Some(self.default_mailbox_capacity)),
            supervisor_strategy: actor_config.supervisor_strategy.clone().or_else(|| Some(self.default_supervisor_strategy.clone())),
            ask_timeout: actor_config.ask_timeout.or(Some(self.default_ask_timeout)),
            backpressure_strategy: actor_config.backpressure_strategy.clone().or_else(|| Some(self.default_backpressure_strategy.clone())),
        }
    }
}

// --- Actor Configuration ---

/// Configuration for individual actors, potentially overriding system defaults.
#[derive(Clone, Debug, Default)]
pub struct ThreadActorConfig {
    /// The scheduling mode for this actor.
    pub scheduling_mode: Option<SchedulingMode>,
    
    /// The capacity of this actor's mailbox.
    pub mailbox_capacity: Option<usize>,
    
    /// The supervision strategy for this actor.
    pub supervisor_strategy: Option<SupervisorStrategy>,
    
    /// The timeout for ask operations originating from this actor.
    pub ask_timeout: Option<Duration>,
    
    /// The backpressure strategy for this actor's mailbox.
    pub backpressure_strategy: Option<BackpressureStrategy>,
    
    // pub core_affinity: Option<usize>,
    // pub dispatcher: Option<String>,
} 