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
    retire_only: bool,
}

// The user future must remain outside an async state machine's unwind. A poll
// panic and a destructor panic are two separate failures, never a double unwind.
struct ControlWork {
    processor: Arc<dyn ProcessorInterface>,
    retire_only: bool,
    future: Option<parrot_api::types::BoxedFuture<'static, Result<(), SystemError>>>,
}
impl ControlWork {
    fn new(processor: Arc<dyn ProcessorInterface>, retire_only: bool) -> Self {
        Self {
            processor,
            retire_only,
            future: None,
        }
    }
    fn phase(&self) -> &'static str {
        if self.retire_only {
            "actor retirement"
        } else {
            "actor stop hook"
        }
    }
    fn dispose(&mut self) -> Option<String> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.future.take())))
            .err()
            .map(|payload| {
                format!(
                    "{} future disposal panicked: {}",
                    self.phase(),
                    crate::owned_payload::panic_message(payload)
                )
            })
    }
}
impl std::future::Future for ControlWork {
    type Output = Result<(), SystemError>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        let owner = self.get_mut();
        if owner.future.is_none() {
            // Lazy creation preserves the biased cancellation/deadline checks.
            let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if owner.retire_only {
                    owner.processor.clone().retire_erased()
                } else {
                    owner.processor.clone().stop_erased()
                }
            }));
            match made {
                Ok(future) => owner.future = Some(future),
                Err(payload) => {
                    return Poll::Ready(Err(SystemError::Panic(format!(
                        "{} construction panicked: {}",
                        owner.phase(),
                        crate::owned_payload::panic_message(payload)
                    ))));
                }
            }
        }
        let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner
                .future
                .as_mut()
                .expect("live control work")
                .as_mut()
                .poll(cx)
        }));
        let result = match polled {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => result,
            Err(payload) => Err(SystemError::Panic(format!(
                "{} panicked: {}",
                owner.phase(),
                crate::owned_payload::panic_message(payload)
            ))),
        }
        .map_err(crate::error_receipt::SystemReceipt::new);
        if let Some(disposal) = owner.dispose() {
            let message = match result {
                Ok(()) => disposal,
                Err(error) => format!(
                    "{}; {disposal}",
                    crate::error_receipt::system_summary(error.error())
                ),
            };
            return Poll::Ready(Err(SystemError::Panic(message)));
        }
        Poll::Ready(result.map_err(crate::error_receipt::SystemReceipt::into_error))
    }
}
impl Drop for ControlWork {
    fn drop(&mut self) {
        if let Some(message) = self.dispose() {
            // Cancelled work has no poll result. Preserve the actual disposal
            // failure through the processor's failure notification instead.
            if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.processor.notify_system_panic(message)
            })) {
                let message = crate::owned_payload::panic_message(payload);
                tracing::error!(%message, "control disposal failure notification panicked");
            }
        }
    }
}

// A borrowed cancellation observer is still arbitrary user code. Keep it out
// of select!'s owned future tuple so poll unwind cannot also unwind its Drop.
struct CancellationObserver<'a> {
    processor: &'a dyn ProcessorInterface,
    phase: &'static str,
    future: Option<parrot_api::types::BoxedFuture<'a, ()>>,
}
impl<'a> CancellationObserver<'a> {
    fn new(processor: &'a dyn ProcessorInterface) -> Result<Self, SystemError> {
        Self::for_stage(processor, "stop")
    }
    fn for_stage(
        processor: &'a dyn ProcessorInterface,
        phase: &'static str,
    ) -> Result<Self, SystemError> {
        let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            processor.processing_cancelled()
        }))
        .map_err(|payload| {
            SystemError::Panic(format!(
                "{phase} cancellation observer construction panicked: {}",
                crate::owned_payload::panic_message(payload)
            ))
        })?;
        Ok(Self {
            processor,
            phase,
            future: Some(future),
        })
    }
    fn retire(&mut self) -> Option<String> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.future.take())))
            .err()
            .map(crate::owned_payload::panic_message)
    }
    fn finish(&mut self, result: Result<(), SystemError>) -> Result<(), SystemError> {
        let phase = self.phase;
        let Some(disposal) = self.retire() else {
            return result;
        };
        let message = format!("{phase} cancellation observer disposal panicked: {disposal}");
        let message = match result {
            Ok(()) => message,
            Err(error) => {
                let receipt = crate::error_receipt::SystemReceipt::new(error);
                format!(
                    "{}; {message}",
                    crate::error_receipt::system_summary(receipt.error())
                )
            }
        };
        Err(SystemError::Panic(message))
    }
}
impl std::future::Future for CancellationObserver<'_> {
    type Output = Result<(), SystemError>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        let owner = self.get_mut();
        let phase = owner.phase;
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner
                .future
                .as_mut()
                .expect("live cancellation observer")
                .as_mut()
                .poll(cx)
        })) {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(())) => Ok(()),
            Err(payload) => Err(SystemError::Panic(format!(
                "{phase} cancellation observer panicked: {}",
                crate::owned_payload::panic_message(payload)
            ))),
        };
        Poll::Ready(owner.finish(result))
    }
}
impl Drop for CancellationObserver<'_> {
    fn drop(&mut self) {
        let phase = self.phase;
        if let Some(disposal) = self.retire() {
            let message = format!("{phase} cancellation observer disposal panicked: {disposal}");
            if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.processor.notify_system_panic(message)
            })) {
                let message = crate::owned_payload::panic_message(payload);
                tracing::error!(%message,"cancellation disposal notification panicked");
            }
        }
    }
}

impl StopHookRequest {
    async fn run(self) {
        let Self {
            path,
            processor,
            deadline,
            mut completion,
            retire_only,
        } = self;
        if retire_only {
            let result = ControlWork::new(processor, true).await;
            let _ = completion.send(result);
            return;
        }
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
        let mut cancellation = match CancellationObserver::new(&*processor) {
            Ok(cancellation) => cancellation,
            Err(error) => {
                let _ = completion.send(Err(error));
                return;
            }
        };
        let result = tokio::select! {
            biased;
            _ = completion.closed() => return,
            cancelled = &mut cancellation => {
                match cancelled {
                    Err(error) => Err(error),
                    Ok(()) if processor.has_failed() => Ok(()),
                    Ok(()) => Err(SystemError::ShutdownError(format!("actor {path} execution cancelled"))),
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                Err(SystemError::Timeout(format!("actor {path} stop hook exceeded deadline")))
            }
            result = ControlWork::new(processor.clone(), false) => result,
        };
        // Dispose the losing observer before publishing any stop result.
        // Closed/dropped requests use the owner's protected Drop notification.
        let result = cancellation.finish(result);
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

    /// Owned OS worker threads (including retirement until joined). Tasks on
    /// an external executor do not constitute owned OS threads.
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
        if processor.has_failed() {
            return Ok(());
        }
        if !processor.is_initialized() {
            // A suspended initializer owns the actor lock. Cancel it before
            // waiting for retirement; it has not admitted a Stop hook yet.
            processor.cancel_processing();
            return Ok(());
        }
        let (completion, result) = tokio::sync::oneshot::channel();
        let request = StopHookRequest {
            path: path.into(),
            processor: processor.clone(),
            deadline,
            completion,
            retire_only: false,
        };
        if let Err(error) = self.run_control(mode, request, slots).await {
            // A dedicated member can fail and exit after the cohort reserved a
            // healthy stop. Missing hook execution is then expected; callers
            // must still obtain the separate real retirement receipt.
            return if processor.has_failed() {
                Ok(())
            } else {
                Err(error)
            };
        }
        match result.await {
            Ok(result) => result, // Preserve an actual hook error or panic.
            Err(_) if processor.has_failed() => Ok(()),
            Err(_) => Err(SystemError::ShutdownError(format!(
                "actor {path} scheduler exited before completing the stop hook"
            ))),
        }
    }

    /// Actual storage retirement; unlike a hook deadline this receipt is not
    /// cancellable merely because the caller has already received a timeout.
    pub(crate) async fn retire_actor(
        &self,
        path: &str,
        mode: &SchedulingMode,
        processor: Arc<dyn ProcessorInterface>,
        slots: Arc<tokio::sync::Semaphore>,
        deadline: tokio::time::Instant,
    ) -> Result<(), SystemError> {
        processor.cancel_processing();
        if matches!(mode, SchedulingMode::DedicatedThread) {
            // The dedicated loop disposes storage on its own thread before
            // exiting, including cancellation and panic paths.
            let mut joined = self
                .dedicated_scheduler
                .deschedule_processor_until(path, &processor, deadline)
                .await;
            let retired = processor.retirement().await;
            // Retain generation identity across the storage receipt wait: a
            // metrics/admission call may already have reaped this worker and
            // admitted another actor using the same textual path.
            while matches!(joined, Err(SystemError::Timeout(_))) {
                joined = self
                    .dedicated_scheduler
                    .deschedule_processor_until(
                        path,
                        &processor,
                        tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                    )
                    .await;
            }
            // Preserve the actual business disposal source when both fail.
            // The scheduler separately retains the OS join failure receipt.
            return retired.and(joined);
        }
        let (completion, result) = tokio::sync::oneshot::channel();
        let submitted = self
            .run_control(
                mode,
                StopHookRequest {
                    path: path.into(),
                    processor: processor.clone(),
                    deadline,
                    completion,
                    retire_only: true,
                },
                slots,
            )
            .await;
        if let Err(error) = submitted {
            tracing::error!(%path, %error, "actor retirement submission failed; retaining registration until disposal receipt");
        } else {
            let _ = result.await;
        }
        // A lost scheduler/control sender is not proof of actor destruction.
        processor.retirement().await
    }

    async fn run_control(
        &self,
        mode: &SchedulingMode,
        request: StopHookRequest,
        slots: Arc<tokio::sync::Semaphore>,
    ) -> Result<(), SystemError> {
        let path = &request.path;
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
        Ok(())
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
            retire_only: false,
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

#[cfg(test)]
mod control_panic_contracts {
    use super::*;
    use crate::thread::processor::{ProcessorStats, ProcessorStatsTrait};
    use futures::FutureExt;
    use parrot_api::types::BoxedFuture;
    use std::{
        any::Any,
        future::Future,
        panic::{AssertUnwindSafe, catch_unwind, panic_any},
        pin::Pin,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::{Context, Poll},
        time::Duration,
    };

    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("secondary control payload");
        }
    }
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic_any(Secondary);
        }
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Creation,
        Poll,
        Plain,
        PendingDrop,
        ReadyDrop,
        PollDrop,
        Business,
        Healthy,
    }
    #[derive(Debug)]
    struct BusinessError {
        drops: Arc<AtomicUsize>,
        disposed: Arc<tokio::sync::Notify>,
        identity: Arc<()>,
    }
    impl fmt::Display for BusinessError {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            panic!("framework must not format business errors");
        }
    }
    impl std::error::Error for BusinessError {}
    impl Drop for BusinessError {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            self.disposed.notify_one();
            panic_any(Secondary);
        }
    }
    struct Processor {
        mode: Mode,
        constructed: AtomicUsize,
        polled: AtomicUsize,
        future_drops: AtomicUsize,
        payload_drops: Arc<AtomicUsize>,
        error_drops: Arc<AtomicUsize>,
        error_disposed: Arc<tokio::sync::Notify>,
        identity: Arc<()>,
        cancelled: AtomicBool,
        cancel: tokio::sync::Notify,
        notices: Mutex<Vec<String>>,
        poll_thread: Mutex<Option<std::thread::ThreadId>>,
        drop_thread: Mutex<Option<std::thread::ThreadId>>,
        panic_notice: AtomicBool,
    }
    impl Processor {
        fn new(mode: Mode) -> Arc<Self> {
            Arc::new(Self {
                mode,
                constructed: AtomicUsize::new(0),
                polled: AtomicUsize::new(0),
                future_drops: AtomicUsize::new(0),
                payload_drops: Arc::new(AtomicUsize::new(0)),
                error_drops: Arc::new(AtomicUsize::new(0)),
                error_disposed: Arc::new(tokio::sync::Notify::new()),
                identity: Arc::new(()),
                cancelled: AtomicBool::new(false),
                cancel: tokio::sync::Notify::new(),
                notices: Mutex::new(Vec::new()),
                poll_thread: Mutex::new(None),
                drop_thread: Mutex::new(None),
                panic_notice: AtomicBool::new(false),
            })
        }
        fn work(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
            self.constructed.fetch_add(1, Ordering::SeqCst);
            if self.mode == Mode::Creation {
                panic_any(Payload(self.payload_drops.clone()));
            }
            Box::pin(Work(self))
        }
    }
    struct Work(Arc<Processor>);
    impl Future for Work {
        type Output = Result<(), SystemError>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            let p = &self.0;
            p.polled.fetch_add(1, Ordering::SeqCst);
            *p.poll_thread.lock().unwrap() = Some(std::thread::current().id());
            match p.mode {
                Mode::Poll | Mode::PollDrop => panic_any(Payload(p.payload_drops.clone())),
                Mode::Plain => panic!("original control failure"),
                Mode::PendingDrop => Poll::Pending,
                Mode::Business => {
                    Poll::Ready(Err(SystemError::Other(anyhow::Error::new(BusinessError {
                        drops: p.error_drops.clone(),
                        disposed: p.error_disposed.clone(),
                        identity: p.identity.clone(),
                    }))))
                }
                _ => Poll::Ready(Ok(())),
            }
        }
    }
    impl Drop for Work {
        fn drop(&mut self) {
            self.0.future_drops.fetch_add(1, Ordering::SeqCst);
            *self.0.drop_thread.lock().unwrap() = Some(std::thread::current().id());
            if matches!(
                self.0.mode,
                Mode::PendingDrop | Mode::ReadyDrop | Mode::PollDrop
            ) {
                panic_any(Payload(self.0.payload_drops.clone()));
            }
        }
    }
    impl ProcessorStatsTrait for Processor {
        fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
            None
        }
    }
    impl ProcessorInterface for Processor {
        fn is_initialized(&self) -> bool {
            true
        }
        fn initialize_and_start_erased(
            self: Arc<Self>,
        ) -> BoxedFuture<'static, Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
        }
        fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
            self.work()
        }
        fn retire_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
            self.work()
        }
        fn process_batch_erased(
            self: Arc<Self>,
            _: Arc<dyn Mailbox>,
            _: usize,
            _: bool,
        ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
            Box::pin(async { Ok((0, 0)) })
        }
        fn cancel_processing(&self) {
            self.cancelled.store(true, Ordering::Release);
            self.cancel.notify_waiters();
        }
        fn processing_cancelled(&self) -> BoxedFuture<'_, ()> {
            Box::pin(async move {
                let notified = self.cancel.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !self.cancelled.load(Ordering::Acquire) {
                    notified.await;
                }
            })
        }
        fn notify_system_panic(&self, message: String) {
            self.notices.lock().unwrap().push(message);
            if self.panic_notice.load(Ordering::Relaxed) {
                panic_any(Payload(self.payload_drops.clone()));
            }
        }
        fn as_any(self: Arc<Self>) -> Arc<dyn Any> {
            self
        }
        fn as_any_ref(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }
    fn request(
        p: &Arc<Processor>,
        retire: bool,
    ) -> (
        StopHookRequest,
        tokio::sync::oneshot::Receiver<Result<(), SystemError>>,
    ) {
        let (completion, result) = tokio::sync::oneshot::channel();
        (
            StopHookRequest {
                path: "/control-contract".into(),
                processor: p.clone(),
                retire_only: retire,
                completion,
                deadline: tokio::time::Instant::now() + Duration::from_secs(2),
            },
            result,
        )
    }
    fn no_unwind<T>(result: std::thread::Result<T>) -> T {
        match result {
            Ok(v) => v,
            Err(p) => {
                std::mem::forget(p);
                panic!("user failure escaped control boundary");
            }
        }
    }
    async fn terminal(mode: Mode, retire: bool) {
        let p = Processor::new(mode);
        let (request, result) = request(&p, retire);
        no_unwind(AssertUnwindSafe(request.run()).catch_unwind().await);
        let error = result
            .await
            .expect("lost control receipt")
            .expect_err("failure became success");
        let SystemError::Panic(message) = error else {
            panic!("wrong control error class");
        };
        assert!(message.contains(if retire { "retirement" } else { "stop hook" }));
        assert_eq!(p.constructed.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.future_drops.load(Ordering::SeqCst),
            usize::from(mode != Mode::Creation)
        );
        if mode == Mode::Plain {
            assert!(message.contains("original control failure"));
        } else {
            assert!(message.contains("panic payload destructor panicked"));
            assert_eq!(
                p.payload_drops.load(Ordering::SeqCst),
                if mode == Mode::PollDrop { 2 } else { 1 }
            );
        }
    }
    #[tokio::test]
    async fn stop_creation_recursive_panic_has_receipt() {
        terminal(Mode::Creation, false).await;
    }
    #[tokio::test]
    async fn stop_poll_recursive_panic_has_receipt() {
        terminal(Mode::Poll, false).await;
    }
    #[tokio::test]
    async fn retirement_creation_recursive_panic_has_receipt() {
        terminal(Mode::Creation, true).await;
    }
    #[tokio::test]
    async fn retirement_poll_recursive_panic_has_receipt() {
        terminal(Mode::Poll, true).await;
    }
    #[tokio::test]
    async fn stop_panic_retains_original_diagnostic() {
        terminal(Mode::Plain, false).await;
    }
    #[tokio::test]
    async fn completed_stop_future_disposal_is_part_of_receipt() {
        terminal(Mode::ReadyDrop, false).await;
    }
    #[tokio::test]
    async fn completed_retirement_future_disposal_is_part_of_receipt() {
        terminal(Mode::ReadyDrop, true).await;
    }
    async fn cancel(kind: u8) {
        let p = Processor::new(Mode::PendingDrop);
        let (request, result) = request(&p, kind == 4);
        let mut task = Box::pin(request.run());
        assert!(futures::poll!(&mut task).is_pending());
        match kind {
            0 => {
                drop(result);
                no_unwind(AssertUnwindSafe(task).catch_unwind().await);
            }
            1 => {
                p.cancel_processing();
                no_unwind(AssertUnwindSafe(task).catch_unwind().await);
                assert!(matches!(
                    result.await.unwrap(),
                    Err(SystemError::ShutdownError(_))
                ));
            }
            2 => {
                tokio::time::advance(Duration::from_secs(3)).await;
                no_unwind(AssertUnwindSafe(task).catch_unwind().await);
                assert!(matches!(
                    result.await.unwrap(),
                    Err(SystemError::Timeout(_))
                ));
            }
            _ => {
                no_unwind(catch_unwind(AssertUnwindSafe(|| drop(task))));
                assert!(result.await.is_err());
            }
        }
        assert_eq!(p.future_drops.load(Ordering::SeqCst), 1);
        assert_eq!(p.payload_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            *p.drop_thread.lock().unwrap(),
            *p.poll_thread.lock().unwrap()
        );
        let notices = p.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("disposal"));
    }
    #[tokio::test]
    async fn receiver_cancellation_retires_on_original_thread() {
        cancel(0).await;
    }
    #[tokio::test]
    async fn processor_cancellation_retires_on_original_thread() {
        cancel(1).await;
    }
    #[tokio::test(start_paused = true)]
    async fn deadline_retires_future_before_timeout_receipt() {
        cancel(2).await;
    }
    #[tokio::test]
    async fn dropped_control_owner_retires_pending_stop() {
        cancel(3).await;
    }
    #[tokio::test]
    async fn dropped_control_owner_retires_pending_retirement() {
        cancel(4).await;
    }
    #[tokio::test]
    async fn cancellation_failure_notification_cannot_unwind() {
        let p = Processor::new(Mode::PendingDrop);
        p.panic_notice.store(true, Ordering::Relaxed);
        let (request, result) = request(&p, false);
        let mut task = Box::pin(request.run());
        assert!(futures::poll!(&mut task).is_pending());
        drop(result);
        no_unwind(AssertUnwindSafe(task).catch_unwind().await);
        assert_eq!(p.future_drops.load(Ordering::SeqCst), 1);
        assert_eq!(p.payload_drops.load(Ordering::SeqCst), 2);
        assert_eq!(p.notices.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn poll_and_drop_panics_cannot_abort_control_executor() {
        const CHILD: &str = "PARROT_CONTROL_DOUBLE_PANIC_CHILD";
        if std::env::var_os(CHILD).is_some() {
            terminal(Mode::PollDrop, false).await;
            terminal(Mode::PollDrop, true).await;
            return;
        }
        let child=tokio::time::timeout(Duration::from_secs(10),tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","thread::scheduler::control_panic_contracts::poll_and_drop_panics_cannot_abort_control_executor","--nocapture"])
            .env(CHILD,"1").env("RUST_BACKTRACE","0").kill_on_drop(true).output()).await.unwrap().unwrap();
        assert!(
            child.status.success(),
            "control child failed: {}\n{}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
    }
    async fn business(orphan: bool) {
        let p = Processor::new(Mode::Business);
        let (request, result) = request(&p, true);
        if orphan {
            drop(result);
            no_unwind(AssertUnwindSafe(request.run()).catch_unwind().await);
        } else {
            no_unwind(AssertUnwindSafe(request.run()).catch_unwind().await);
            let error = result.await.unwrap().unwrap_err();
            let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&error);
            let mut found = false;
            for _ in 0..8 {
                let Some(current) = cause else { break };
                if let Some(original) = current.downcast_ref::<BusinessError>() {
                    assert!(Arc::ptr_eq(&original.identity, &p.identity));
                    found = true;
                    break;
                }
                cause = current.source();
            }
            assert!(
                found,
                "original business error absent from receipt source chain"
            );
            no_unwind(catch_unwind(AssertUnwindSafe(|| drop(error))));
        }
        tokio::time::timeout(Duration::from_secs(2), p.error_disposed.notified())
            .await
            .unwrap();
        assert_eq!(p.error_drops.load(Ordering::SeqCst), 1);
        assert_eq!(p.future_drops.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn business_error_source_and_safe_disposal_survive_receipt() {
        business(false).await;
    }
    #[tokio::test]
    async fn abandoned_retirement_receipt_safely_disposes_business_error() {
        business(true).await;
    }
    #[tokio::test]
    async fn healthy_control_preserves_success() {
        for retire in [false, true] {
            let p = Processor::new(Mode::Healthy);
            let (request, result) = request(&p, retire);
            request.run().await;
            result.await.unwrap().unwrap();
            assert_eq!(p.constructed.load(Ordering::SeqCst), 1);
            assert_eq!(p.future_drops.load(Ordering::SeqCst), 1);
        }
    }
    #[tokio::test]
    async fn cancelled_stop_never_constructs_user_future() {
        let p = Processor::new(Mode::Creation);
        let (request, result) = request(&p, false);
        drop(result);
        request.run().await;
        assert_eq!(p.constructed.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn expired_stop_never_constructs_user_future() {
        let p = Processor::new(Mode::Creation);
        let (mut request, result) = request(&p, false);
        request.deadline = tokio::time::Instant::now();
        request.run().await;
        assert!(matches!(
            result.await.unwrap(),
            Err(SystemError::Timeout(_))
        ));
        assert_eq!(p.constructed.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn retirement_outlives_receiver_and_hook_deadline() {
        let p = Processor::new(Mode::Healthy);
        let (mut request, result) = request(&p, true);
        request.deadline = tokio::time::Instant::now();
        drop(result);
        request.run().await;
        assert_eq!(p.constructed.load(Ordering::SeqCst), 1);
        assert_eq!(p.future_drops.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn real_shard_survives_control_retirement_panic() {
        let scheduler = ShardedScheduler::start(1, 1);
        let bad = Processor::new(Mode::Plain);
        let (request, result) = request(&bad, true);
        scheduler.enqueue_stop_hook("one", request).unwrap();
        let bad_result = tokio::time::timeout(Duration::from_secs(2), result).await;
        let good = Processor::new(Mode::Healthy);
        let (request, result) = self::request(&good, false);
        let queued = scheduler.enqueue_stop_hook("one", request);
        let good_result = tokio::time::timeout(Duration::from_secs(2), result).await;
        no_unwind(catch_unwind(AssertUnwindSafe(|| scheduler.shutdown())));
        assert!(matches!(bad_result, Ok(Ok(Err(SystemError::Panic(_))))));
        assert!(queued.is_ok());
        assert!(matches!(good_result, Ok(Ok(Ok(())))));
        assert_eq!(scheduler.owned_thread_count(), 0);
    }
}

#[cfg(test)]
mod cancellation_contracts {
    use super::*;
    use crate::thread::processor::{ProcessorStats, ProcessorStatsTrait};
    use futures::FutureExt;
    use parrot_api::types::BoxedFuture;
    use std::{
        any::Any,
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll},
        time::Duration,
    };
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Construct,
        Plain,
        Poll,
        PollDrop,
        ReadyDrop,
        PendingDrop,
        Ready,
        Pending,
    }
    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("secondary cancellation payload");
        }
    }
    struct Payload(Arc<AtomicUsize>);
    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::panic::panic_any(Secondary);
        }
    }
    struct Processor {
        mode: Mode,
        stop_ready: bool,
        business: bool,
        panic_notice: bool,
        created: AtomicUsize,
        drops: AtomicUsize,
        stop_calls: AtomicUsize,
        payload_drops: Arc<AtomicUsize>,
        notices: Mutex<Vec<String>>,
        display: Arc<AtomicUsize>,
        error_drops: Arc<AtomicUsize>,
        error_disposed: Arc<tokio::sync::Notify>,
        poll_thread: Mutex<Option<std::thread::ThreadId>>,
        drop_thread: Mutex<Option<std::thread::ThreadId>>,
    }
    impl Processor {
        fn new(mode: Mode, stop_ready: bool, business: bool, panic_notice: bool) -> Arc<Self> {
            Arc::new(Self {
                mode,
                stop_ready,
                business,
                panic_notice,
                created: AtomicUsize::new(0),
                drops: AtomicUsize::new(0),
                stop_calls: AtomicUsize::new(0),
                payload_drops: Arc::new(AtomicUsize::new(0)),
                notices: Mutex::new(Vec::new()),
                display: Arc::new(AtomicUsize::new(0)),
                error_drops: Arc::new(AtomicUsize::new(0)),
                error_disposed: Arc::new(tokio::sync::Notify::new()),
                poll_thread: Mutex::new(None),
                drop_thread: Mutex::new(None),
            })
        }
    }
    #[derive(Debug)]
    struct Business {
        display: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        disposed: Arc<tokio::sync::Notify>,
    }
    impl std::fmt::Display for Business {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.display.fetch_add(1, Ordering::SeqCst);
            panic!("business formatting must not run");
        }
    }
    impl std::error::Error for Business {}
    impl Drop for Business {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            self.disposed.notify_one();
        }
    }
    struct Observer<'a>(&'a Processor);
    impl Future for Observer<'_> {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            *self.0.poll_thread.lock().unwrap() = Some(std::thread::current().id());
            match self.0.mode {
                Mode::Plain => panic!("original cancellation observer failure"),
                Mode::Poll | Mode::PollDrop => {
                    std::panic::panic_any(Payload(self.0.payload_drops.clone()))
                }
                Mode::Ready | Mode::ReadyDrop => Poll::Ready(()),
                _ => Poll::Pending,
            }
        }
    }
    impl Drop for Observer<'_> {
        fn drop(&mut self) {
            self.0.drops.fetch_add(1, Ordering::SeqCst);
            *self.0.drop_thread.lock().unwrap() = Some(std::thread::current().id());
            if matches!(
                self.0.mode,
                Mode::PollDrop | Mode::ReadyDrop | Mode::PendingDrop
            ) {
                std::panic::panic_any(Payload(self.0.payload_drops.clone()));
            }
        }
    }
    impl ProcessorStatsTrait for Processor {
        fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
            None
        }
    }
    impl ProcessorInterface for Processor {
        fn is_initialized(&self) -> bool {
            true
        }
        fn initialize_and_start_erased(
            self: Arc<Self>,
        ) -> BoxedFuture<'static, Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
        }
        fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
            self.stop_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if !self.stop_ready {
                    std::future::pending::<()>().await;
                }
                if self.business {
                    Err(SystemError::Other(anyhow::Error::new(Business {
                        display: self.display.clone(),
                        drops: self.error_drops.clone(),
                        disposed: self.error_disposed.clone(),
                    })))
                } else {
                    Ok(())
                }
            })
        }
        fn retire_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
            Box::pin(async { Ok(()) })
        }
        fn process_batch_erased(
            self: Arc<Self>,
            _: Arc<dyn crate::thread::mailbox::Mailbox>,
            _: usize,
            _: bool,
        ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
            Box::pin(async { Ok((0, 0)) })
        }
        fn processing_cancelled(&self) -> BoxedFuture<'_, ()> {
            self.created.fetch_add(1, Ordering::SeqCst);
            if self.mode == Mode::Construct {
                std::panic::panic_any(Payload(self.payload_drops.clone()));
            }
            Box::pin(Observer(self))
        }
        fn notify_system_panic(&self, message: String) {
            self.notices.lock().unwrap().push(message);
            if self.panic_notice {
                std::panic::panic_any(Payload(self.payload_drops.clone()));
            }
        }
        fn as_any(self: Arc<Self>) -> Arc<dyn Any> {
            self
        }
        fn as_any_ref(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }
    fn request(
        p: &Arc<Processor>,
    ) -> (
        StopHookRequest,
        tokio::sync::oneshot::Receiver<Result<(), SystemError>>,
    ) {
        let (completion, result) = tokio::sync::oneshot::channel();
        (
            StopHookRequest {
                path: "/cancel-observer".into(),
                processor: p.clone(),
                deadline: tokio::time::Instant::now() + Duration::from_secs(2),
                completion,
                retire_only: false,
            },
            result,
        )
    }
    fn no_unwind<T>(value: std::thread::Result<T>) -> T {
        match value {
            Ok(v) => v,
            Err(payload) => {
                std::mem::forget(payload);
                panic!("cancellation observer escaped request ownership");
            }
        }
    }
    async fn terminal(mode: Mode, stop_ready: bool, business: bool) {
        let p = Processor::new(mode, stop_ready, business, false);
        let (mut req, result) = request(&p);
        if !stop_ready && mode == Mode::PendingDrop {
            req.deadline = tokio::time::Instant::now() + Duration::from_millis(10);
        }
        no_unwind(std::panic::AssertUnwindSafe(req.run()).catch_unwind().await);
        let result = result.await.expect("lost stop receipt");
        if mode == Mode::Pending && !business {
            result.unwrap();
        } else if mode == Mode::Ready {
            assert!(matches!(result, Err(SystemError::ShutdownError(_))));
        } else if mode == Mode::Pending && business {
            let Err(SystemError::Other(error)) = result else {
                panic!("business source erased");
            };
            let mut current: &(dyn std::error::Error + 'static) = error.as_ref();
            let mut found = false;
            for _ in 0..8 {
                if let Some(error) = current.downcast_ref::<Business>() {
                    assert!(Arc::ptr_eq(&error.display, &p.display));
                    found = true;
                    break;
                }
                if let Some(source) = current.source() {
                    current = source;
                } else {
                    break;
                }
            }
            assert!(found, "original business source identity lost");
            drop(error);
        } else {
            let Err(SystemError::Panic(message)) = result else {
                panic!("observer failure was lost or misclassified");
            };
            assert!(message.contains("cancellation observer"));
            if mode == Mode::Plain {
                assert!(message.contains("original cancellation observer failure"));
            } else {
                assert!(message.contains("panic payload destructor panicked"));
            }
            if business {
                assert!(message.contains("original error retained"));
            }
        }
        assert_eq!(p.created.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.drops.load(Ordering::SeqCst),
            usize::from(mode != Mode::Construct)
        );
        assert_eq!(
            p.payload_drops.load(Ordering::SeqCst),
            match mode {
                Mode::PollDrop => 2,
                Mode::Construct | Mode::Poll | Mode::PendingDrop | Mode::ReadyDrop => 1,
                _ => 0,
            }
        );
        assert_eq!(
            p.stop_calls.load(Ordering::SeqCst),
            usize::from(matches!(mode, Mode::Pending | Mode::PendingDrop))
        );
        assert!(
            p.notices.lock().unwrap().is_empty(),
            "terminal failure must be in its receipt"
        );
        if mode != Mode::Construct {
            assert_eq!(
                *p.poll_thread.lock().unwrap(),
                *p.drop_thread.lock().unwrap()
            );
        }
        if business {
            tokio::time::timeout(Duration::from_secs(2), p.error_disposed.notified())
                .await
                .unwrap();
            assert_eq!(p.error_drops.load(Ordering::SeqCst), 1);
            assert_eq!(p.display.load(Ordering::SeqCst), 0);
        }
    }
    async fn isolated(mode: Mode, stop_ready: bool, business: bool, name: &str) {
        const CHILD: &str = "PARROT_CONTROL_CANCELLATION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            terminal(mode, stop_ready, business).await;
            return;
        }
        let full = format!("thread::scheduler::cancellation_contracts::{name}");
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &full, "--nocapture"])
                .env(CHILD, "1")
                .env("RUST_BACKTRACE", "0")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"),
            "observer child failed: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[tokio::test]
    async fn observer_construction_panic_has_stop_receipt() {
        terminal(Mode::Construct, false, false).await;
    }
    #[tokio::test]
    async fn observer_poll_panic_preserves_cause() {
        terminal(Mode::Plain, false, false).await;
    }
    #[tokio::test]
    async fn observer_recursive_poll_payload_is_retired() {
        terminal(Mode::Poll, false, false).await;
    }
    #[tokio::test]
    async fn observer_poll_and_drop_cannot_abort() {
        isolated(
            Mode::PollDrop,
            false,
            false,
            "observer_poll_and_drop_cannot_abort",
        )
        .await;
    }
    #[tokio::test]
    async fn completed_observer_disposal_has_stop_receipt() {
        terminal(Mode::ReadyDrop, false, false).await;
    }
    #[tokio::test]
    async fn losing_observer_disposal_changes_success_to_failure() {
        terminal(Mode::PendingDrop, true, false).await;
    }
    #[tokio::test]
    async fn deadline_disposes_observer_before_result() {
        terminal(Mode::PendingDrop, false, false).await;
    }
    #[tokio::test]
    async fn observer_disposal_uses_safe_business_summary() {
        isolated(
            Mode::PendingDrop,
            true,
            true,
            "observer_disposal_uses_safe_business_summary",
        )
        .await;
    }
    #[tokio::test]
    async fn healthy_observer_preserves_business_source() {
        terminal(Mode::Pending, true, true).await;
    }
    #[tokio::test]
    async fn healthy_stop_with_pending_observer_still_completes() {
        terminal(Mode::Pending, true, false).await;
    }
    #[tokio::test]
    async fn ordinary_cancellation_keeps_existing_error_class() {
        terminal(Mode::Ready, false, false).await;
    }
    async fn cancel(receiver: bool, panic_notice: bool) {
        let p = Processor::new(Mode::PendingDrop, false, false, panic_notice);
        let (req, result) = request(&p);
        let mut work = Box::pin(req.run());
        assert!(futures::poll!(&mut work).is_pending());
        if receiver {
            drop(result);
            no_unwind(std::panic::AssertUnwindSafe(work).catch_unwind().await);
        } else {
            no_unwind(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || drop(work),
            )));
            assert!(result.await.is_err());
        }
        assert_eq!(p.drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            p.payload_drops.load(Ordering::SeqCst),
            if panic_notice { 2 } else { 1 }
        );
        let notices = p.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("cancellation observer disposal"));
        assert_eq!(
            *p.poll_thread.lock().unwrap(),
            *p.drop_thread.lock().unwrap()
        );
    }
    #[tokio::test]
    async fn closed_receiver_reports_observer_disposal_failure() {
        cancel(true, false).await;
    }
    #[tokio::test]
    async fn dropped_request_reports_observer_disposal_failure() {
        cancel(false, false).await;
    }
    #[tokio::test]
    async fn observer_disposal_notification_is_also_protected() {
        cancel(false, true).await;
    }
    #[tokio::test]
    async fn expired_or_closed_requests_do_not_construct_observers() {
        for expired in [true, false] {
            let p = Processor::new(Mode::Construct, false, false, false);
            let (mut req, result) = request(&p);
            if expired {
                req.deadline = tokio::time::Instant::now();
                no_unwind(std::panic::AssertUnwindSafe(req.run()).catch_unwind().await);
                assert!(matches!(
                    result.await.unwrap(),
                    Err(SystemError::Timeout(_))
                ));
            } else {
                drop(result);
                no_unwind(std::panic::AssertUnwindSafe(req.run()).catch_unwind().await);
            }
            assert_eq!(p.created.load(Ordering::SeqCst), 0);
            assert_eq!(p.stop_calls.load(Ordering::SeqCst), 0);
        }
    }
    #[tokio::test]
    async fn retirement_does_not_consult_stop_cancellation_observer() {
        let p = Processor::new(Mode::Construct, false, false, false);
        let (mut req, result) = request(&p);
        req.retire_only = true;
        no_unwind(std::panic::AssertUnwindSafe(req.run()).catch_unwind().await);
        result.await.unwrap().unwrap();
        assert_eq!(p.created.load(Ordering::SeqCst), 0);
    }
}
