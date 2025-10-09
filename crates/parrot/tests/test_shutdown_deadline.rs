use std::sync::Arc;
use std::time::Duration;

use parrot::thread::config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::error::SystemError;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use tokio::sync::Notify;

struct Cancelled(Arc<Notify>);
impl Drop for Cancelled {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

struct WaitingActor {
    entered: Arc<Notify>,
    cancelled: Arc<Notify>,
    release: Arc<Notify>,
    wait_in_stop: bool,
}

impl WaitingActor {
    async fn wait(&self) {
        let _cancelled = Cancelled(self.cancelled.clone());
        self.entered.notify_one();
        self.release.notified().await;
    }
}

impl Actor for WaitingActor {
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
            if !self.wait_in_stop {
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

async fn shutdown_cancels(mode: SchedulingMode, wait_in_stop: bool) {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_millis(40),
        ..Default::default()
    });
    let entered = Arc::new(Notify::new());
    let cancelled = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let actor = system
        .spawn_at(
            WaitingActor {
                entered: entered.clone(),
                cancelled: cancelled.clone(),
                release: release.clone(),
                wait_in_stop,
            },
            "/waiting",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(mode),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let pending_ref = actor.clone();
    let pending = tokio::spawn(async move { pending_ref.send(Box::new(42_u64)).await });
    let pending = if wait_in_stop {
        assert!(pending.await.unwrap().is_ok());
        None
    } else {
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        Some(pending)
    };
    let result = tokio::time::timeout(Duration::from_millis(500), system.shutdown_internal()).await;
    // A regression can unblock its handler for cleanup instead of hanging the test runtime.
    if result.is_err() {
        release.notify_one();
    }
    assert!(
        matches!(result, Ok(Err(SystemError::Timeout(_)))),
        "shutdown must report its deadline: {result:?}"
    );
    assert_eq!(system.actor_count(), 0);
    assert!(!actor.is_alive().await);
    assert!(actor.deliver(Box::new(7_u64)).await.is_err());
    tokio::time::timeout(Duration::from_secs(1), cancelled.notified())
        .await
        .unwrap();
    if let Some(pending) = pending {
        assert!(
            tokio::time::timeout(Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
    assert!(
        matches!(
            system.shutdown_internal().await,
            Err(SystemError::Timeout(_))
        ),
        "repeated shutdown must retain the deadline failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_wait_is_cancelled_at_system_deadline() {
    shutdown_cancels(
        SchedulingMode::SharedPool {
            max_messages_per_run: 10,
        },
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shard_wait_is_cancelled_at_system_deadline() {
    shutdown_cancels(
        SchedulingMode::Sharded {
            affinity_key: "shutdown".into(),
        },
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_wait_is_cancelled_at_system_deadline() {
    shutdown_cancels(SchedulingMode::DedicatedThread, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_stop_hook_cannot_block_system_shutdown() {
    shutdown_cancels(
        SchedulingMode::SharedPool {
            max_messages_per_run: 10,
        },
        true,
    )
    .await;
}

struct BlockingStop(std::sync::Mutex<std::sync::mpsc::Receiver<()>>);
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
        Box::pin(async move {
            // A bounded fallback keeps the test recoverable on a broken runtime.
            let _ = self.0.lock().unwrap().recv_timeout(Duration::from_secs(2));
            Ok(())
        })
    }
}

#[tokio::test]
async fn synchronous_stop_hook_does_not_block_shutdown_timer() {
    let (release, wait) = std::sync::mpsc::channel();
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    });
    let actor = system
        .spawn_at(
            BlockingStop(std::sync::Mutex::new(wait)),
            "/blocking-stop",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    actor.send(Box::new(())).await.unwrap();
    let start = tokio::time::Instant::now();
    let result = system.shutdown_internal().await;
    let elapsed = start.elapsed();
    let _ = release.send(());
    assert!(matches!(result, Err(SystemError::Timeout(_))), "{result:?}");
    assert!(
        elapsed < Duration::from_secs(1),
        "shutdown timer was blocked for {elapsed:?}"
    );
}
