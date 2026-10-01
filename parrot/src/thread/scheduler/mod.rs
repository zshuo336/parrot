//! # Thread Scheduler Module
//!
//! This module provides thread scheduling implementations for the actor system.
//! It includes various thread pool implementations for different actor workloads.
//!
//! ## Key Concepts
//! - Thread pools: Shared and dedicated thread pools for different workloads
//! - Scheduling: Distributing actors across available threads
//! - Load balancing: Optimizing resource utilization
//!
//! ## Design Principles
//! - Flexibility: Multiple scheduling strategies for different needs
//! - Efficiency: Minimizing overhead in the scheduling process
//! - Adaptability: Runtime selection of appropriate scheduler

// Re-exported modules with unified organization
pub mod shared;
pub mod dedicated_thread;
pub mod queue;

use std::fmt;
use std::error::Error;
use std::sync::Arc;

use tokio::runtime::Handle;

use crate::thread::mailbox::Mailbox;
use crate::thread::config::ThreadActorConfig;
use crate::thread::scheduler::dedicated_thread::DedicatedThreadScheduler;
use crate::thread::scheduler::shared::SharedThreadPool;

/// Common interface for all thread scheduler implementations
pub trait ThreadScheduler: fmt::Debug + Send + Sync {
    /// Schedule an actor on the thread pool using type erasure
    fn schedule(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), Box<dyn Error + Send + Sync>>;

    /// Deschedule an actor from the thread pool
    fn deschedule(
        &self,
        path: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>>;

    /// Check if an actor is currently scheduled
    fn is_scheduled(&self, path: &str) -> bool;

    /// Shut down the thread pool
    fn shutdown(&self) -> Result<(), Box<dyn Error + Send + Sync>>;

    /// Number of worker threads (for status reporting).
    fn metrics_thread_count(&self) -> usize {
        0
    }
}

/// Group of schedulers
///
/// This struct contains a shared thread scheduler and a dedicated thread scheduler.
/// It provides a way to manage and access both schedulers.
pub struct SchedulerGroup {
    pub shared_scheduler: Arc<SharedThreadPool>,
    pub dedicated_scheduler: Arc<DedicatedThreadScheduler>,
}

impl fmt::Debug for SchedulerGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulerGroup")
            .field("shared_scheduler", &self.shared_scheduler)
            .field("dedicated_scheduler", &self.dedicated_scheduler)
            .finish()
    }
}

/// Factory for creating thread schedulers
pub struct ThreadSchedulerFactory {
    /// Runtime handle for spawning tasks
    runtime_handle: Handle,
}

impl ThreadSchedulerFactory {
    /// Create a new thread scheduler factory
    pub fn new(
        runtime_handle: Handle,
    ) -> Self {
        Self {
            runtime_handle,
        }
    }

    /// Create a new scheduler group
    pub fn create_scheduler_group(
        &self,
        config: Option<shared::SharedThreadPoolConfig>,
        dedicated_config: Option<dedicated_thread::DedicatedThreadConfig>,
    ) -> SchedulerGroup {
        SchedulerGroup {
            shared_scheduler: self.create_shared_pool(config),
            dedicated_scheduler: self.create_dedicated_pool(dedicated_config),
        }
    }

    /// Create a shared thread pool
    fn create_shared_pool(
        &self,
        config: Option<shared::SharedThreadPoolConfig>,
    ) -> Arc<SharedThreadPool> {
        let pool = SharedThreadPool::new(
            config,
            self.runtime_handle.clone(),
        );

        Arc::new(pool)
    }

    /// Create a dedicated thread pool
    fn create_dedicated_pool(
        &self,
        config: Option<dedicated_thread::DedicatedThreadConfig>,
    ) -> Arc<DedicatedThreadScheduler> {
        let scheduler = DedicatedThreadScheduler::new(config)
            .with_runtime_handle(self.runtime_handle.clone());
        Arc::new(scheduler)
    }
}
