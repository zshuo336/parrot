//! # Shared Thread Pool Module
//!
//! Provides the implementation of a shared thread pool for actor execution.
//! This module integrates the best aspects of both the original shared pool
//! and the multi-thread scheduler implementations.
//!
//! ## Key Concepts
//! - Work stealing: Workers pull tasks from a shared queue
//! - Thread efficiency: Batch processing and optimal thread utilization
//! - Health monitoring: Comprehensive metrics and worker status tracking
//!
//! ## Design Principles
//! - Lock-free operations where possible for high throughput
//! - Fair work distribution across worker threads
//! - Resilience through isolation of actor failures
//!
//! ## Thread Safety
//! - Thread-safe through Arc, AtomicBool/AtomicUsize and message passing
//! - Uses weak references to avoid reference cycles
//! - Coordinator pattern for centralized management

mod pool;
mod worker;
mod worker_manager;

// Re-exports
pub use pool::SharedThreadPool;

// Export types
pub use pool::SharedThreadPoolConfig;

// Import for SharedSystemRef trait
use std::sync::Arc;
use std::fmt;
use anyhow::anyhow;

use crate::thread::mailbox::Mailbox;
use crate::thread::scheduler::BoxedActorFuture;
use parrot_api::types::BoxedMessage;
