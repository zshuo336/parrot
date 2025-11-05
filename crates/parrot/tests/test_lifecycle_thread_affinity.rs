//! Lifecycle hooks must execute on the actor's assigned OS thread.
use parrot::thread::{
    config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{Arc, Mutex},
    thread::ThreadId,
    time::Duration,
};

type Events = Arc<Mutex<Vec<(&'static str, ThreadId)>>>;
struct AffinityActor(Events);
impl AffinityActor {
    fn record(&self, event: &'static str) {
        self.0
            .lock()
            .unwrap()
            .push((event, std::thread::current().id()));
    }
}
impl Drop for AffinityActor {
    fn drop(&mut self) {
        self.record("actor dropped");
    }
}
impl Actor for AffinityActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.record("init construction");
        Box::pin(async move {
            self.record("init poll");
            tokio::time::sleep(Duration::from_millis(1)).await;
            self.record("init resumed");
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.record("handler construction");
        Box::pin(async move {
            self.record("handler poll");
            tokio::time::sleep(Duration::from_millis(1)).await;
            self.record("handler resumed");
            Ok(msg)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.record("stop construction");
        Box::pin(async move {
            self.record("stop poll");
            tokio::time::sleep(Duration::from_millis(1)).await;
            self.record("stop resumed");
            Ok(())
        })
    }
}
async fn check(mode: SchedulingMode, whole_system: bool, inherited_mode: bool) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_scheduling_mode: mode.clone(),
        shutdown_timeout: Duration::from_secs(1),
        ..Default::default()
    });
    let actor = system
        .spawn_at(
            AffinityActor(events.clone()),
            "/affinity",
            None,
            ThreadActorConfig {
                scheduling_mode: if inherited_mode { None } else { Some(mode) },
                ..Default::default()
            },
        )
        .await
        .unwrap();
    actor
        .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(2)))
        .await
        .unwrap();
    if whole_system {
        system.shutdown_internal().await.unwrap();
    } else {
        actor.stop().await.unwrap();
        system.shutdown_internal().await.unwrap();
    }
    let events = events.lock().unwrap();
    assert_eq!(
        events.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        [
            "init construction",
            "init poll",
            "init resumed",
            "handler construction",
            "handler poll",
            "handler resumed",
            "stop construction",
            "stop poll",
            "stop resumed",
            "actor dropped",
        ]
    );
    let owner = events[0].1;
    for (name, thread) in events.iter() {
        assert_eq!(
            *thread, owner,
            "{name} migrated away from the assigned actor thread"
        );
    }
}
fn shard() -> SchedulingMode {
    SchedulingMode::Sharded {
        affinity_key: "lifecycle".into(),
    }
}
#[tokio::test]
async fn dedicated_stop_hook_keeps_thread_affinity() {
    check(SchedulingMode::DedicatedThread, false, false).await;
}
#[tokio::test]
async fn shard_stop_hook_keeps_thread_affinity() {
    check(shard(), false, false).await;
}
#[tokio::test]
async fn dedicated_shutdown_hook_keeps_thread_affinity() {
    check(SchedulingMode::DedicatedThread, true, false).await;
}
#[tokio::test]
async fn shard_shutdown_hook_keeps_thread_affinity() {
    check(shard(), true, false).await;
}
#[tokio::test]
async fn inherited_dedicated_mode_keeps_lifecycle_affinity() {
    check(SchedulingMode::DedicatedThread, false, true).await;
}
#[tokio::test]
async fn inherited_shard_mode_keeps_lifecycle_affinity() {
    check(shard(), true, true).await;
}

struct RecordDrop(Events);
impl Drop for RecordDrop {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap()
            .push(("stop dropped", std::thread::current().id()));
    }
}
struct StopTimingActor {
    events: Events,
    synchronous: bool,
}
impl Actor for StopTimingActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(Box::new(std::thread::current().id()) as BoxedMessage) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.events
            .lock()
            .unwrap()
            .push(("stop construction", std::thread::current().id()));
        if self.synchronous {
            std::thread::sleep(Duration::from_millis(250));
        }
        Box::pin(async move {
            let _drop = RecordDrop(self.events.clone());
            if !self.synchronous {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }
}
async fn check_hook_deadline(mode: SchedulingMode, synchronous: bool) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shutdown_timeout: Duration::from_millis(40),
        ..Default::default()
    });
    let actor = system
        .spawn_at(
            StopTimingActor {
                events: events.clone(),
                synchronous,
            },
            "/timing",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(mode),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let owner = *actor
        .send_with_timeout(Box::new(()), Some(Duration::from_secs(1)))
        .await
        .unwrap()
        .downcast::<ThreadId>()
        .unwrap();
    let started = tokio::time::Instant::now();
    let result = actor.stop().await;
    let elapsed = started.elapsed();
    // Observe cancellation on its owning thread before shutting down the system.
    let dropped = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if events
                .lock()
                .unwrap()
                .iter()
                .any(|(name, _)| *name == "stop dropped")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    let _ = system.shutdown_internal().await;
    assert!(
        matches!(
            result,
            Err(parrot_api::errors::ActorError::TimeoutDetail(_))
        ),
        "{result:?}"
    );
    assert!(
        elapsed < Duration::from_millis(200),
        "owner thread blocked system timer: {elapsed:?}"
    );
    assert!(dropped.is_ok(), "cancelled hook resources must be released");
    let events = events.lock().unwrap();
    assert_eq!(
        events.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        ["stop construction", "stop dropped"]
    );
    assert!(
        events.iter().all(|(_, thread)| *thread == owner),
        "{events:?}"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn dedicated_hook_cancellation_keeps_thread_affinity() {
    check_hook_deadline(SchedulingMode::DedicatedThread, false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn shard_hook_cancellation_keeps_thread_affinity() {
    check_hook_deadline(shard(), false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn dedicated_synchronous_hook_does_not_block_system_timer() {
    check_hook_deadline(SchedulingMode::DedicatedThread, true).await;
}
#[tokio::test(flavor = "current_thread")]
async fn shard_synchronous_hook_does_not_block_system_timer() {
    check_hook_deadline(shard(), true).await;
}

struct PanicStop(Events);
impl Actor for PanicStop {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(Box::new(std::thread::current().id()) as BoxedMessage) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0
            .lock()
            .unwrap()
            .push(("panic hook", std::thread::current().id()));
        panic!("deliberate stop hook panic");
    }
}
async fn check_panic_isolation(mode: SchedulingMode) {
    let system = ThreadActorSystem::shared(Default::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let config = ThreadActorConfig {
        scheduling_mode: Some(mode),
        ..Default::default()
    };
    let actor = system
        .spawn_at(PanicStop(events.clone()), "/panic", None, config.clone())
        .await
        .unwrap();
    let neighbour = system
        .spawn_at(
            AffinityActor(Default::default()),
            "/neighbour",
            None,
            config,
        )
        .await
        .unwrap();
    let owner = *actor
        .send_with_timeout(Box::new(()), Some(Duration::from_secs(1)))
        .await
        .unwrap()
        .downcast::<ThreadId>()
        .unwrap();
    let result = system.stop_actor("/panic").await;
    let response = neighbour
        .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(1)))
        .await;
    let remains_registered = system.get_actor_ref("/panic").is_some();
    system.shutdown_internal().await.unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("stop hook panicked")
    );
    assert!(!remains_registered);
    assert_eq!(*response.unwrap().downcast::<u64>().unwrap(), 42);
    assert_eq!(*events.lock().unwrap(), [("panic hook", owner)]);
}
#[tokio::test]
async fn shard_stop_hook_panic_preserves_neighbour_on_same_shard() {
    check_panic_isolation(shard()).await;
}
#[tokio::test]
async fn dedicated_stop_hook_panic_preserves_system() {
    check_panic_isolation(SchedulingMode::DedicatedThread).await;
}
