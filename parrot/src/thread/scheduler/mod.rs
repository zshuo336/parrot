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

pub mod shared;
pub mod queue;
pub mod dedicated;

use std::fmt;
use std::error::Error;
use std::sync::{Arc, Weak};

use tokio::runtime::Handle;

use crate::thread::mailbox::Mailbox;
use crate::thread::config::ThreadActorConfig;

/// Common interface for all thread scheduler implementations
pub trait ThreadScheduler: fmt::Debug + Send + Sync {
    /// Schedule an actor on the thread pool
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

/// System reference interface for thread schedulers
pub trait SystemRef: fmt::Debug {
    /// Look up an actor by path
    fn lookup(&self, path: &str) -> Option<Arc<dyn Mailbox>>;
    
    /// Handle worker panic
    fn handle_panic(&self, path: &str, error: String);
}

/// Factory for creating thread schedulers
pub struct ThreadSchedulerFactory {
    /// Runtime handle for spawning tasks
    runtime_handle: Handle,
    
    /// Reference to the actor system
    system_ref: Option<Weak<dyn SystemRef + Send + Sync>>,
}

impl ThreadSchedulerFactory {
    /// Create a new thread scheduler factory
    pub fn new(
        runtime_handle: Handle,
        system_ref: Option<Weak<dyn SystemRef + Send + Sync>>,
    ) -> Self {
        Self {
            runtime_handle,
            system_ref,
        }
    }
    
    /// Create a shared thread pool
    pub fn create_shared_pool(
        &self,
        config: Option<shared::SharedThreadPoolConfig>,
    ) -> Arc<dyn ThreadScheduler> {
        let pool = shared::SharedThreadPool::new(
            config,
            self.runtime_handle.clone(),
            self.system_ref.clone(),
        );
        
        Arc::new(pool)
    }
    
    /// Create a dedicated thread pool
    pub fn create_dedicated_pool(
        &self,
        config: Option<dedicated::DedicatedThreadPoolConfig>,
    ) -> Arc<dyn ThreadScheduler> {
        let pool = dedicated::DedicatedThreadPool::new(
            config,
            self.runtime_handle.clone(),
            self.system_ref.clone(),
        );
        
        Arc::new(DedicatedThreadScheduler::new(pool))
    }
}

/// Adapter to implement ThreadScheduler for DedicatedThreadPool
pub struct DedicatedThreadScheduler {
    /// Inner dedicated thread pool
    pool: dedicated::DedicatedThreadPool,
}

impl DedicatedThreadScheduler {
    /// Create a new dedicated thread scheduler
    pub fn new(pool: dedicated::DedicatedThreadPool) -> Self {
        Self { pool }
    }
}

impl ThreadScheduler for DedicatedThreadScheduler {
    fn schedule(
        &self,
        path: &str,
        mailbox: Arc<dyn Mailbox>,
        config: Option<ThreadActorConfig>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.pool.schedule(path, mailbox, config).await
            })
        })
    }
    
    fn deschedule(
        &self,
        path: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.pool.deschedule(path).await
            })
        })
    }
    
    fn is_scheduled(&self, path: &str) -> bool {
        self.pool.is_scheduled(path)
    }
    
    fn shutdown(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.pool.shutdown().await
            })
        })
    }
}

impl fmt::Debug for DedicatedThreadScheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DedicatedThreadScheduler")
            .field("pool", &self.pool)
            .finish()
    }
} 