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
pub mod dedicated_thread;
pub mod queue;
pub mod sharded;
pub mod shared;
pub mod steal;

use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex};

use tokio::runtime::Handle;

use crate::thread::config::{SchedulingMode, ThreadActorConfig};
use crate::thread::error::SystemError;
use crate::thread::mailbox::Mailbox;
use crate::thread::processor::ProcessorInterface;
use crate::thread::scheduler::dedicated_thread::DedicatedThreadScheduler;
use crate::thread::scheduler::sharded::ShardedScheduler;
use crate::thread::scheduler::shared::SharedThreadPool;

/// Owned control work polled by the actor's assigned scheduler: an affinity
/// runtime or an isolated shared-pool poll. Dropping its completion receiver
/// cancels a queued or suspended hook without moving its user future.
pub(crate) struct StopHookRequest {
    path: String,
    processor: Arc<dyn ProcessorInterface>,
    deadline: tokio::time::Instant,
    completion: tokio::sync::oneshot::Sender<Result<(), SystemError>>,
}

impl StopHookRequest {
    async fn run(self) {
        use futures::FutureExt;
        let Self {
            path,
            processor,
            deadline,
            mut completion,
        } = self;
        // Check before constructing any actor lifecycle future. A busy shard
        // may only dequeue this request after the caller's deadline expired.
        if completion.is_closed() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = completion.send(Err(SystemError::Timeout(format!(
                "actor {path} stop hook deadline expired"
            ))));
            return;
        }
        let result = tokio::select! {
            biased;
            _ = completion.closed() => return,
            _ = processor.processing_cancelled() => {
                Err(SystemError::ShutdownError(format!("actor {path} execution cancelled")))
            }
            _ = tokio::time::sleep_until(deadline) => {
                Err(SystemError::Timeout(format!("actor {path} stop hook exceeded deadline")))
            }
            result = std::panic::AssertUnwindSafe(async {
                processor.clone().stop_erased().await
            }).catch_unwind() => {
                result.unwrap_or_else(|_| Err(SystemError::ShutdownError(format!(
                    "actor {path} stop hook panicked"
                ))))
            }
        };
        let _ = completion.send(result);
    }
}

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
    fn deschedule(&self, path: &str) -> Result<(), Box<dyn Error + Send + Sync>>;

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
    /// Lazily-started shard pool (ADR-14). Shard count defaults to CPU count;
    /// started on first `Sharded` actor spawn.
    pub sharded_scheduler: Mutex<Option<Arc<ShardedScheduler>>>,
    sharded_default_size: usize,
}

impl SchedulerGroup {
    /// Keep lifecycle execution on the same scheduler as initialization and
    /// message handling. The system owns the outer operation and its deadline.
    pub(crate) async fn stop_hook(
        &self,
        path: &str,
        mode: &SchedulingMode,
        processor: Arc<dyn ProcessorInterface>,
        slots: Arc<tokio::sync::Semaphore>,
        deadline: tokio::time::Instant,
    ) -> Result<(), SystemError> {
        let (completion, result) = tokio::sync::oneshot::channel();
        let request = StopHookRequest {
            path: path.into(),
            processor,
            deadline,
            completion,
        };
        match mode {
            SchedulingMode::DedicatedThread => {
                self.dedicated_scheduler.enqueue_stop_hook(request)?
            }
            SchedulingMode::Sharded { affinity_key } => {
                let scheduler = self
                    .sharded_scheduler
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| SystemError::ActorNotFound(path.into()))?;
                scheduler.enqueue_stop_hook(affinity_key, request)?;
            }
            SchedulingMode::SharedPool { .. } => {
                let permit = slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|error| SystemError::ShutdownError(error.to_string()))?;
                shared::poll_isolated(
                    async move {
                        request.run().await;
                        Ok(())
                    },
                    permit,
                    slots,
                    Arc::new(tokio::sync::Notify::new()),
                )
                .await
                .map_err(|error| SystemError::ShutdownError(error.to_string()))?;
            }
        }
        result.await.map_err(|_| {
            SystemError::ShutdownError(format!(
                "actor {path} scheduler exited before completing the stop hook"
            ))
        })?
    }

    /// Get (or start) the sharded scheduler with the default shard count.
    pub fn sharded(&self) -> Arc<ShardedScheduler> {
        let mut guard = self.sharded_scheduler.lock().unwrap();
        if let Some(s) = guard.as_ref() {
            return s.clone();
        }
        let s = ShardedScheduler::start(self.sharded_default_size, 32);
        *guard = Some(s.clone());
        s
    }
}

impl fmt::Debug for SchedulerGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulerGroup")
            .field("shared_scheduler", &self.shared_scheduler)
            .field("dedicated_scheduler", &self.dedicated_scheduler)
            .field(
                "sharded_scheduler",
                &self.sharded_scheduler.lock().unwrap().is_some(),
            )
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
    pub fn new(runtime_handle: Handle) -> Self {
        Self { runtime_handle }
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
            sharded_scheduler: Mutex::new(None),
            sharded_default_size: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8),
        }
    }

    /// Create a shared thread pool
    fn create_shared_pool(
        &self,
        config: Option<shared::SharedThreadPoolConfig>,
    ) -> Arc<SharedThreadPool> {
        let pool = SharedThreadPool::new(config, self.runtime_handle.clone());

        Arc::new(pool)
    }

    /// Create a dedicated thread pool
    fn create_dedicated_pool(
        &self,
        config: Option<dedicated_thread::DedicatedThreadConfig>,
    ) -> Arc<DedicatedThreadScheduler> {
        let scheduler =
            DedicatedThreadScheduler::new(config).with_runtime_handle(self.runtime_handle.clone());
        Arc::new(scheduler)
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::thread::{context::ThreadContext, system::ThreadActorSystem};
    use parrot_api::{
        actor::{Actor, ActorState, EmptyConfig},
        address::ActorRef,
        types::{ActorResult, BoxedFuture, BoxedMessage},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct CountStop(Arc<AtomicUsize>);
    impl Actor for CountStop {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Ok(msg) })
        }
        fn before_stop<'a>(
            &'a mut self,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    async fn rejected_request_does_not_enter_actor(expired: bool) {
        let system = ThreadActorSystem::shared(Default::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let actor = system
            .spawn_at(CountStop(calls.clone()), "/guard", None, Default::default())
            .await
            .unwrap();
        actor
            .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(1)))
            .await
            .unwrap();
        let processor = system
            .get_mailbox("/guard")
            .unwrap()
            .get_processor()
            .unwrap();
        let (completion, result) = tokio::sync::oneshot::channel();
        let request = StopHookRequest {
            path: "/guard".into(),
            processor,
            completion,
            deadline: if expired {
                tokio::time::Instant::now()
            } else {
                tokio::time::Instant::now() + Duration::from_secs(10)
            },
        };
        if expired {
            request.run().await;
            assert!(matches!(
                result.await.unwrap(),
                Err(SystemError::Timeout(_))
            ));
        } else {
            drop(result);
            request.run().await;
        }
        let observed = calls.load(Ordering::SeqCst);
        // A rejected control request must not even set the processor Stopping.
        let reply = actor
            .send_with_timeout(Box::new(43_u64), Some(Duration::from_secs(1)))
            .await;
        system.shutdown_internal().await.unwrap();
        assert_eq!(observed, 0, "rejected request constructed the actor hook");
        assert_eq!(*reply.unwrap().downcast::<u64>().unwrap(), 43);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only the real shutdown may invoke the hook"
        );
    }

    #[tokio::test]
    async fn expired_control_request_never_enters_actor() {
        rejected_request_does_not_enter_actor(true).await;
    }
    #[tokio::test]
    async fn cancelled_control_request_never_enters_actor() {
        rejected_request_does_not_enter_actor(false).await;
    }
}
