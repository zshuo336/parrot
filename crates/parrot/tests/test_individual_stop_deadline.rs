//! A system stop must cancel one actor without shutting down its neighbours.
use parrot::thread::{
    config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    error::SystemError,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

struct Dropped(Arc<Notify>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
struct ActorUnderTest {
    entered: Arc<Notify>,
    dropped: Arc<Notify>,
    wait_in_stop: bool,
}
impl ActorUnderTest {
    async fn wait(&self) {
        let _guard = Dropped(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending::<()>().await;
    }
}
impl Actor for ActorUnderTest {
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
        Box::pin(async move {
            if !self.wait_in_stop && msg.downcast_ref::<u64>() == Some(&0) {
                self.wait().await;
            }
            Ok(msg)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.wait_in_stop {
                self.wait().await;
            }
            Ok(())
        })
    }
}

async fn stop_one(mode: SchedulingMode, wait_in_stop: bool) {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_millis(50),
        ..Default::default()
    });
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let actor = system
        .spawn_at(
            ActorUnderTest {
                entered: entered.clone(),
                dropped: dropped.clone(),
                wait_in_stop,
            },
            "/waiting",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(mode.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let neighbour = system
        .spawn_at(
            ActorUnderTest {
                entered: Default::default(),
                dropped: Default::default(),
                wait_in_stop: false,
            },
            "/neighbour",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(mode),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    actor.deliver(Box::new(0_u64)).await.unwrap();
    if !wait_in_stop {
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
    } else {
        actor
            .send_with_timeout(Box::new(1_u64), Some(Duration::from_secs(1)))
            .await
            .unwrap();
    }
    let result =
        tokio::time::timeout(Duration::from_millis(600), system.stop_actor("/waiting")).await;
    let cancelled = tokio::time::timeout(Duration::from_millis(200), dropped.notified()).await;
    let alive = actor.is_alive().await;
    let neighbour_result = neighbour
        .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(1)))
        .await;
    let _ = system.shutdown_internal().await;
    assert!(
        matches!(result, Ok(Err(SystemError::Timeout(_)))),
        "stop must use the configured deadline: {result:?}"
    );
    assert!(
        cancelled.is_ok(),
        "stop must drop the suspended future before system shutdown"
    );
    assert!(!alive, "old reference must reject delivery after stop");
    assert_eq!(*neighbour_result.unwrap().downcast::<u64>().unwrap(), 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_stop_cancels_pending_handler() {
    stop_one(
        SchedulingMode::SharedPool {
            max_messages_per_run: 10,
        },
        false,
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shard_stop_cancels_pending_handler() {
    stop_one(
        SchedulingMode::Sharded {
            affinity_key: "same".into(),
        },
        false,
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_stop_cancels_pending_handler() {
    stop_one(SchedulingMode::DedicatedThread, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_stop_cancels_pending_hook() {
    stop_one(
        SchedulingMode::SharedPool {
            max_messages_per_run: 10,
        },
        true,
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shard_stop_cancels_pending_hook() {
    stop_one(
        SchedulingMode::Sharded {
            affinity_key: "same".into(),
        },
        true,
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_stop_cancels_pending_hook() {
    stop_one(SchedulingMode::DedicatedThread, true).await;
}

struct ControlledStop {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl Actor for ControlledStop {
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
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            Ok(())
        })
    }
}

async fn controlled_stop_system() -> (
    Arc<ThreadActorSystem>,
    Arc<Notify>,
    Arc<Notify>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_secs(2),
        ..Default::default()
    });
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    system
        .spawn_at(
            ControlledStop {
                entered: entered.clone(),
                release: release.clone(),
                calls: calls.clone(),
            },
            "/controlled",
            None,
            Default::default(),
        )
        .await
        .unwrap()
        .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(1)))
        .await
        .unwrap();
    (system, entered, release, calls)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_stop_callers_share_cleanup_and_reserve_the_path() {
    let (system, entered, release, calls) = controlled_stop_system().await;
    let first_system = system.clone();
    let first = tokio::spawn(async move { first_system.stop_actor("/controlled").await });
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    let replacement = system
        .spawn_at(
            ControlledStop {
                entered: Default::default(),
                release: Default::default(),
                calls: Default::default(),
            },
            "/controlled",
            None,
            Default::default(),
        )
        .await;
    let second = system.stop_actor("/controlled");
    tokio::pin!(second);
    let early = futures::poll!(&mut second);
    let waited = early.is_pending();
    release.notify_one();
    let second_result = match early {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => second.await,
    };
    let first_result = first.await.unwrap();
    system.shutdown_internal().await.unwrap();
    assert!(matches!(
        replacement,
        Err(parrot::thread::error::SpawnError::ActorPathAlreadyExists(_))
    ));
    assert!(waited, "concurrent stop must await actual completion");
    first_result.unwrap();
    second_result.unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_stop_caller_does_not_abandon_cleanup() {
    let (system, entered, release, calls) = controlled_stop_system().await;
    let stopping_system = system.clone();
    let stopping = tokio::spawn(async move { stopping_system.stop_actor("/controlled").await });
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    stopping.abort();
    assert!(stopping.await.unwrap_err().is_cancelled());
    release.notify_one();
    let cleaned = tokio::time::timeout(Duration::from_secs(1), async {
        while system.actor_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let _ = system.shutdown_internal().await;
    assert!(cleaned.is_ok(), "cleanup must outlive the cancelled caller");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_shutdown_joins_an_existing_individual_stop() {
    let (system, entered, release, calls) = controlled_stop_system().await;
    let stopping_system = system.clone();
    let stopping = tokio::spawn(async move { stopping_system.stop_actor("/controlled").await });
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    let shutdown = system.shutdown_internal();
    tokio::pin!(shutdown);
    let early = futures::poll!(&mut shutdown);
    let waited = early.is_pending();
    release.notify_one();
    let result = match early {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => shutdown.await,
    };
    stopping.await.unwrap().unwrap();
    result.unwrap();
    assert!(waited, "shutdown must join the in-progress stop");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_system_shutdown_callers_wait_for_the_same_hook() {
    let (system, entered, release, calls) = controlled_stop_system().await;
    let first_system = system.clone();
    let first = tokio::spawn(async move { first_system.shutdown_internal().await });
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    let second = system.shutdown_internal();
    tokio::pin!(second);
    let early = futures::poll!(&mut second);
    let waited = early.is_pending();
    release.notify_one();
    let result = match early {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => second.await,
    };
    first.await.unwrap().unwrap();
    result.unwrap();
    assert!(
        waited,
        "a second caller must not report success before cleanup completes"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_shutdown_caller_does_not_abandon_cleanup() {
    let (system, entered, release, calls) = controlled_stop_system().await;
    let first_system = system.clone();
    let first = tokio::spawn(async move { first_system.shutdown_internal().await });
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), system.shutdown_internal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(system.actor_count(), 0);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

struct BlockingStop;
impl Actor for BlockingStop {
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
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        // Deliberately blocks even before returning the future.
        std::thread::sleep(Duration::from_millis(250));
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_stop_hook_does_not_block_individual_stop_deadline() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_millis(40),
        ..Default::default()
    });
    system
        .spawn_at(BlockingStop, "/blocking", None, Default::default())
        .await
        .unwrap()
        .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(1)))
        .await
        .unwrap();
    let start = tokio::time::Instant::now();
    let stopped = system.stop_actor("/blocking").await;
    let elapsed = start.elapsed();
    let _ = system.shutdown_internal().await;
    assert!(
        matches!(stopped, Err(SystemError::Timeout(_))),
        "{stopped:?}"
    );
    assert!(
        elapsed < Duration::from_millis(200),
        "synchronous hook blocked the timer: {elapsed:?}"
    );
}

struct BlockingMessageDrop;
impl Drop for BlockingMessageDrop {
    fn drop(&mut self) {
        std::thread::sleep(Duration::from_millis(250));
    }
}

async fn close_with_blocking_message_drop(whole_system: bool) {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_millis(40),
        ..Default::default()
    });
    let entered = Arc::new(Notify::new());
    let actor = system
        .spawn_at(
            ActorUnderTest {
                entered: entered.clone(),
                dropped: Default::default(),
                wait_in_stop: false,
            },
            "/blocked-drop",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    actor.deliver(Box::new(0_u64)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    actor.deliver(Box::new(BlockingMessageDrop)).await.unwrap();
    let start = tokio::time::Instant::now();
    let result = if whole_system {
        system.shutdown_internal().await
    } else {
        system.stop_actor("/blocked-drop").await
    };
    let elapsed = start.elapsed();
    let alive = actor.is_alive().await;
    let _ = system.shutdown_internal().await;
    assert!(matches!(result, Err(SystemError::Timeout(_))), "{result:?}");
    assert!(
        elapsed < Duration::from_millis(200),
        "message destructor blocked the deadline: {elapsed:?}"
    );
    assert!(!alive);
}

#[tokio::test(flavor = "current_thread")]
async fn message_destructor_cannot_block_individual_stop_deadline() {
    close_with_blocking_message_drop(false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn message_destructor_cannot_block_system_shutdown_deadline() {
    close_with_blocking_message_drop(true).await;
}
