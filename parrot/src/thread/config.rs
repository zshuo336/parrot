use std::time::Duration;

// --- Configuration Enums (Copied from docs/threadactor/configuration.md) ---

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
    pub shared_pool_size: usize,
    pub default_scheduling_mode: SchedulingMode,
    pub default_mailbox_capacity: usize,
    pub default_ask_timeout: Duration,
    pub default_supervisor_strategy: SupervisorStrategy,
    pub default_backpressure_strategy: BackpressureStrategy,
    // pub dedicated_thread_affinity_strategy: Option<Box<dyn Fn(ActorPath) -> Option<usize> + Send + Sync>>,
    // pub thread_name_prefix: String,
}

impl Default for ThreadActorSystemConfig {
    fn default() -> Self {
        Self {
            shared_pool_size: num_cpus::get(),
            default_scheduling_mode: SchedulingMode::SharedPool { max_messages_per_run: 10 },
            default_mailbox_capacity: 1024,
            default_ask_timeout: Duration::from_secs(5),
            default_supervisor_strategy: SupervisorStrategy::Restart { max_retries: 3, within: Duration::from_secs(10) },
            default_backpressure_strategy: BackpressureStrategy::Block,
            // dedicated_thread_affinity_strategy: None,
            // thread_name_prefix: "parrot-thread-worker-".to_string(),
        }
    }
}

// --- Actor Configuration ---

/// Configuration for individual actors, potentially overriding system defaults.
#[derive(Clone, Debug, Default)]
pub struct ThreadActorConfig {
    pub scheduling_mode: Option<SchedulingMode>,
    pub mailbox_capacity: Option<usize>,
    pub supervisor_strategy: Option<SupervisorStrategy>,
    pub ask_timeout: Option<Duration>,
    pub backpressure_strategy: Option<BackpressureStrategy>,
    // pub core_affinity: Option<usize>,
    // pub dispatcher: Option<String>,
} 