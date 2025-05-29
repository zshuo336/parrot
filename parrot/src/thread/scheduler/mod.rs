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
use std::sync::{Arc, Weak};

use tokio::runtime::Handle;
use anyhow::anyhow;

use crate::thread::mailbox::Mailbox;
use crate::thread::config::ThreadActorConfig;
use parrot_api::types::{BoxedMessage, ActorResult};
use parrot_api::actor::Actor;
use crate::thread::context::ThreadContext;
use crate::thread::scheduler::dedicated_thread::DedicatedThreadScheduler;
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
}

/// Helper trait containing methods that require generic type parameters
pub trait TypedThreadScheduler: ThreadScheduler {
    /// Schedule a typed actor
    fn schedule_typed<A>(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), Box<dyn Error + Send + Sync>>
    where
        A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static;
}

/// Boxed future type for async actor operations
pub type BoxedActorFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Group of schedulers
/// 
/// This struct contains a shared thread scheduler and a dedicated thread scheduler.
/// It provides a way to manage and access both schedulers.
pub struct SchedulerGroup {
    pub shared_scheduler: Arc<dyn ThreadScheduler>,
    pub dedicated_scheduler: Arc<DedicatedThreadScheduler>,
}

/// Weak reference to a thread scheduler
pub type WeakSchedulerRef = Weak<SchedulerGroup>;

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
        let scheduler_group = SchedulerGroup {
            shared_scheduler: self.create_shared_pool(config),
            dedicated_scheduler: self.create_dedicated_pool(dedicated_config),
        };
        scheduler_group
    }

    /// Create a shared thread pool
    fn create_shared_pool(
        &self,
        config: Option<shared::SharedThreadPoolConfig>,
    ) -> Arc<dyn ThreadScheduler> {
        let pool = shared::SharedThreadPool::new(
            config,
            self.runtime_handle.clone(),
        );
        
        Arc::new(pool)
    }
    
    /// Create a dedicated thread pool
    fn create_dedicated_pool(
        &self,
        config: Option<dedicated_thread::DedicatedThreadConfig>,
    ) -> Arc<dedicated_thread::DedicatedThreadScheduler> {
        let scheduler = dedicated_thread::DedicatedThreadScheduler::new(config);
        Arc::new(scheduler)
    }
} 
