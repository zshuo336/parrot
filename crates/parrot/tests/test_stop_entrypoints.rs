//! Public stop entry points must use the same lifecycle and generation boundary.
use parrot::thread::{
    config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::ActorContext,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
struct Observations {
    entered: Notify,
    release: Notify,
    hooks: AtomicUsize,
    handled_after_stop: AtomicUsize,
}
struct StopActor(Arc<Observations>);
impl Actor for StopActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match msg.downcast_ref::<u64>() {
                Some(0) => {
                    self.0.entered.notify_one();
                    self.0.release.notified().await;
                    ctx.stop().await?;
                }
                Some(1) => {
                    self.0.handled_after_stop.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            Ok(msg)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.hooks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
fn system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_millis(300),
        ..Default::default()
    })
}
async fn external_stop(mode: SchedulingMode) {
    let system = system();
    let obs = Arc::new(Observations::default());
    let actor = system
        .spawn_at(
            StopActor(obs.clone()),
            "/stop",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(mode),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    actor
        .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(1)))
        .await
        .unwrap();
    actor.stop().await.unwrap();
    let count = system.actor_count();
    let hooks = obs.hooks.load(Ordering::SeqCst);
    let alive = actor.is_alive().await;
    system.shutdown_internal().await.unwrap();
    assert_eq!(hooks, 1, "ActorRef::stop must await before_stop");
    assert_eq!(count, 0, "stop must unregister its generation");
    assert!(!alive);
    assert_eq!(
        obs.hooks.load(Ordering::SeqCst),
        1,
        "shutdown must not repeat the hook"
    );
}
async fn self_stop(mode: SchedulingMode) {
    let system = system();
    let obs = Arc::new(Observations::default());
    let actor = system
        .spawn_at(
            StopActor(obs.clone()),
            "/self-stop",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(mode),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let request_ref = actor.clone();
    let request = tokio::spawn(async move {
        request_ref
            .send_with_timeout(Box::new(0_u64), Some(Duration::from_secs(1)))
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), obs.entered.notified())
        .await
        .unwrap();
    actor.deliver(Box::new(1_u64)).await.unwrap();
    obs.release.notify_one();
    let response = request.await.unwrap();
    let stopped = tokio::time::timeout(Duration::from_millis(600), async {
        while system.actor_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let hooks = obs.hooks.load(Ordering::SeqCst);
    let _ = system.shutdown_internal().await;
    assert_eq!(
        *response.unwrap().downcast::<u64>().unwrap(),
        0,
        "self-stop must let the current handler return"
    );
    assert!(
        stopped.is_ok(),
        "self-stop must unregister without system shutdown"
    );
    assert_eq!(hooks, 1);
    assert_eq!(obs.handled_after_stop.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn shared_ref_stop() {
    external_stop(SchedulingMode::SharedPool {
        max_messages_per_run: 64,
    })
    .await;
}
#[tokio::test]
async fn shard_ref_stop() {
    external_stop(SchedulingMode::Sharded {
        affinity_key: "stop".into(),
    })
    .await;
}
#[tokio::test]
async fn dedicated_ref_stop() {
    external_stop(SchedulingMode::DedicatedThread).await;
}
#[tokio::test]
async fn shared_self_stop() {
    self_stop(SchedulingMode::SharedPool {
        max_messages_per_run: 64,
    })
    .await;
}
#[tokio::test]
async fn shard_self_stop() {
    self_stop(SchedulingMode::Sharded {
        affinity_key: "stop".into(),
    })
    .await;
}
#[tokio::test]
async fn dedicated_self_stop() {
    self_stop(SchedulingMode::DedicatedThread).await;
}

#[tokio::test]
async fn old_reference_cannot_stop_replacement_generation() {
    let system = system();
    let old = system
        .spawn_at(
            StopActor(Default::default()),
            "/same",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    old.send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(1)))
        .await
        .unwrap();
    // Keep the old mailbox allocated: checking only Weak::upgrade is insufficient.
    let retained_mailbox = system.get_mailbox("/same").unwrap();
    system.stop_actor("/same").await.unwrap();
    let obs = Arc::new(Observations::default());
    let replacement = system
        .spawn_at(StopActor(obs.clone()), "/same", None, Default::default())
        .await
        .unwrap();
    let _ = old.stop().await;
    let response = replacement
        .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(1)))
        .await;
    let hooks = obs.hooks.load(Ordering::SeqCst);
    system.shutdown_internal().await.unwrap();
    drop(retained_mailbox);
    assert_eq!(*response.unwrap().downcast::<u64>().unwrap(), 42);
    assert_eq!(hooks, 0, "old stop must not run the replacement hook");
}

struct BlockingActor {
    entered: Arc<Notify>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
impl Actor for BlockingActor {
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
            self.entered.notify_one();
            // A bounded synchronous wait models non-preemptible user code.
            // The bound also lets the test clean up if an assertion fails.
            let _ = self
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2));
            Ok(msg)
        })
    }
}
#[tokio::test]
async fn timed_out_dedicated_thread_keeps_its_capacity_until_exit() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        max_dedicated_threads: 1,
        shutdown_timeout: Duration::from_millis(40),
        ..Default::default()
    });
    let entered = Arc::new(Notify::new());
    let (release, released) = std::sync::mpsc::channel();
    let config = ThreadActorConfig {
        scheduling_mode: Some(SchedulingMode::DedicatedThread),
        ..Default::default()
    };
    let actor = system
        .spawn_at(
            BlockingActor {
                entered: entered.clone(),
                release: std::sync::Mutex::new(released),
            },
            "/blocked",
            None,
            config.clone(),
        )
        .await
        .unwrap();
    actor.deliver(Box::new(42_u64)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    let stopped = system.stop_actor("/blocked").await;
    let early = system
        .spawn_at(
            StopActor(Default::default()),
            "/other",
            None,
            config.clone(),
        )
        .await;
    let exceeded_capacity = early.is_ok();
    release.send(()).unwrap();
    if early.is_ok() {
        let _ = system.stop_actor("/other").await;
    }
    let replacement = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match system
                .spawn_at(
                    StopActor(Default::default()),
                    "/after-exit",
                    None,
                    config.clone(),
                )
                .await
            {
                Ok(actor) => break actor,
                Err(_) => tokio::time::sleep(Duration::from_millis(1)).await,
            }
        }
    })
    .await;
    // Wait for initialization before shutdown to avoid testing an unrelated init race.
    if let Ok(actor) = &replacement {
        actor
            .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(1)))
            .await
            .unwrap();
    }
    let _ = system.shutdown_internal().await;
    assert!(matches!(
        stopped,
        Err(parrot::thread::error::SystemError::Timeout(_))
    ));
    assert!(
        !exceeded_capacity,
        "a timed-out but live OS thread still consumes the configured limit"
    );
    assert!(
        replacement.is_ok(),
        "a completed retired thread must release its capacity"
    );
}
