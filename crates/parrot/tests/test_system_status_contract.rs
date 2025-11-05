use parrot::{
    system::ParrotActorSystem,
    thread::{context::ThreadContext, system::ThreadActorSystem},
};
use parrot_api::address::ActorRef;
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    system::ActorSystem,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
struct Echo;
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(msg) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
#[tokio::test]
async fn facade_status_counts_actual_live_actors() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("thread".into(), engine.clone(), true)
        .await
        .unwrap();
    let a = facade.spawn_root_thread(Echo, EmptyConfig).await.unwrap();
    let b = facade.spawn_root_thread(Echo, EmptyConfig).await.unwrap();
    for actor in [&a, &b] {
        assert_eq!(
            *actor
                .send(Box::new(42_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
    }
    let direct = engine.status().active_actors;
    let reported = facade.status().active_actors;
    facade.internal_shutdown().await.unwrap();
    assert_eq!(direct, 2, "actual engine should own two responsive actors");
    assert_eq!(
        reported, direct,
        "facade status must not report zero for a live engine"
    );
}

#[tokio::test]
async fn thread_status_is_stopped_after_successful_owned_shutdown() {
    let engine = ThreadActorSystem::shared(Default::default());
    let actor = engine
        .spawn_root_typed_thread(Echo, EmptyConfig)
        .await
        .unwrap();
    assert_eq!(
        *actor
            .send(Box::new(42_u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    engine.shutdown_internal().await.unwrap();
    let status = engine.status();
    assert_eq!(status.active_actors, 0);
    assert_eq!(
        status.state,
        parrot_api::system::SystemState::Stopped,
        "completed owned shutdown must not remain ShuttingDown forever"
    );
}

#[tokio::test]
async fn facade_aliases_and_replacements_count_each_real_thread_engine_once() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let first = ThreadActorSystem::shared(Default::default());
    let second = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("".into(), first.clone(), true)
        .await
        .unwrap();
    facade
        .register_thread_system("alias".into(), first.clone(), false)
        .await
        .unwrap();
    let a = facade.spawn_root_thread(Echo, EmptyConfig).await.unwrap();
    assert!(a.send(Box::new(42_u64)).await.is_ok());
    let aliased = facade.status().active_actors;
    facade
        .register_thread_system("alias".into(), second.clone(), true)
        .await
        .unwrap();
    let b = facade.spawn_root_thread(Echo, EmptyConfig).await.unwrap();
    assert!(b.send(Box::new(42_u64)).await.is_ok());
    let replaced = facade.status().active_actors;
    facade.set_default_system("").unwrap();
    let switched = facade.status().active_actors;
    // Replacing the last registration excludes the old engine, but must not
    // stop externally owned actors or count the new engine twice.
    facade
        .register_thread_system("".into(), second, true)
        .await
        .unwrap();
    let removed = facade.status().active_actors;
    assert!(a.send(Box::new(42_u64)).await.is_ok());
    facade.internal_shutdown().await.unwrap();
    first.shutdown_internal().await.unwrap();
    assert_eq!((aliased, replaced, switched, removed), (1, 2, 2, 1));
}

#[actix::test]
async fn facade_counts_actix_clones_and_mixed_engines_without_alias_duplication() {
    use parrot::actix::ActixActorSystem;
    use parrot_api::context::ActorContext;
    struct ErasedEcho;
    impl Actor for ErasedEcho {
        type Config = Box<dyn std::any::Any + Send>;
        type Context = dyn ActorContext;
        fn receive_message<'a>(
            &'a mut self,
            m: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(m) })
        }
        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let actix = ActixActorSystem::with_arbiter_count(2).await.unwrap();
    facade
        .register_actix_system("actix".into(), actix.clone(), true)
        .await
        .unwrap();
    facade
        .register_actix_system("actix-alias".into(), actix.clone(), false)
        .await
        .unwrap();
    let a = facade
        .spawn_root_boxed(Box::new(ErasedEcho), Box::new(()))
        .await
        .unwrap();
    assert!(a.send(Box::new(42_u64)).await.is_ok());
    let thread = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("thread".into(), thread.clone(), true)
        .await
        .unwrap();
    let b = facade.spawn_root_thread(Echo, EmptyConfig).await.unwrap();
    assert!(b.send(Box::new(42_u64)).await.is_ok());
    let running = facade.status();
    actix.shutdown().await.unwrap();
    let mixed = facade.status();
    thread.shutdown_internal().await.unwrap();
    let stopped = facade.status();
    facade.internal_shutdown().await.unwrap();
    use parrot_api::system::SystemState;
    assert_eq!(
        (running.active_actors, running.resources.thread_count),
        (2, 2)
    );
    assert_eq!(
        (
            mixed.state,
            mixed.active_actors,
            mixed.resources.thread_count
        ),
        (SystemState::Running, 1, 0)
    );
    assert_eq!(
        (
            stopped.state,
            stopped.active_actors,
            stopped.resources.thread_count
        ),
        (SystemState::Stopped, 0, 0)
    );
}

#[tokio::test]
async fn both_facade_constructors_report_their_own_elapsed_uptime() {
    for facade in [
        ParrotActorSystem::new(Default::default()).await.unwrap(),
        <ParrotActorSystem as ActorSystem>::start(Default::default())
            .await
            .unwrap(),
    ] {
        let after_creation = std::time::Instant::now();
        tokio::task::yield_now().await;
        let floor = after_creation.elapsed();
        let status = facade.status();
        facade.internal_shutdown().await.unwrap();
        assert_eq!(status.state, parrot_api::system::SystemState::Running);
        assert_eq!(
            (status.active_actors, status.resources.thread_count),
            (0, 0)
        );
        assert!(
            status.uptime >= floor,
            "uptime must include elapsed time after construction"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_resources_count_owned_dedicated_and_shard_threads_not_shared_tasks() {
    use parrot::thread::config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig};
    let engine = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 3,
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    let idle = engine.status().resources.thread_count;
    let dedicated = engine
        .spawn_at(
            Echo,
            "/dedicated",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(SchedulingMode::DedicatedThread),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(dedicated.send(Box::new(42_u64)).await.is_ok());
    let dedicated_count = engine.status().resources.thread_count;
    let sharded = engine
        .spawn_at(
            Echo,
            "/sharded",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(SchedulingMode::Sharded {
                    affinity_key: "status".into(),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(sharded.send(Box::new(42_u64)).await.is_ok());
    let together = engine.status();
    engine.shutdown_internal().await.unwrap();
    let retired = engine.status();
    let shards = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    assert_eq!(
        (idle, dedicated_count, together.resources.thread_count),
        (0, 1, 1 + shards)
    );
    assert_eq!(together.active_actors, 2);
    assert_eq!(
        (retired.state, retired.resources.thread_count),
        (parrot_api::system::SystemState::Stopped, 0)
    );
}

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(std::sync::Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct HeldDrop(std::sync::Arc<Gate>);
impl Drop for HeldDrop {
    fn drop(&mut self) {
        self.0.block();
    }
}
impl Actor for HeldDrop {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(m) })
    }
}
async fn wait_stopped(engine: &ThreadActorSystem) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while engine.status().state != parrot_api::system::SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned cleanup did not reach Stopped after release");
}
async fn held_retirement(mode: parrot::thread::config::SchedulingMode, cancel_waiter: bool) {
    use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
    use parrot_api::system::SystemState;
    let engine = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: std::time::Duration::from_millis(100),
        ..Default::default()
    });
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    facade
        .register_thread_system("held".into(), engine.clone(), true)
        .await
        .unwrap();
    let gate = std::sync::Arc::new(Gate::default());
    let release = Release(gate.clone());
    let actor = engine
        .spawn_at(
            HeldDrop(gate.clone()),
            "/held",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(mode),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(actor.send(Box::new(42_u64)).await.is_ok());
    let owner = engine.clone();
    let waiter = tokio::spawn(async move { owner.shutdown_internal().await });
    tokio::time::timeout(std::time::Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    if cancel_waiter {
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
    } else {
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result,
            Err(parrot::thread::error::SystemError::Timeout(_))
        ));
    }
    let blocked = engine.status().state;
    let facade_blocked = facade.status().state;
    drop(release);
    wait_stopped(&engine).await;
    let finished = facade.status();
    let repeated = engine.shutdown_internal().await;
    let _ = facade.internal_shutdown().await;
    assert_eq!(
        (blocked, facade_blocked),
        (SystemState::ShuttingDown, SystemState::ShuttingDown)
    );
    assert_eq!(
        (
            finished.state,
            finished.active_actors,
            finished.resources.thread_count
        ),
        (SystemState::Stopped, 0, 0)
    );
    if !cancel_waiter {
        assert!(
            matches!(
                repeated,
                Err(parrot::thread::error::SystemError::Timeout(_))
            ),
            "completion must retain the caller's original deadline result"
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_deadline_is_not_retirement_but_late_cleanup_reaches_stopped() {
    held_retirement(
        parrot::thread::config::SchedulingMode::SharedPool {
            max_messages_per_run: 10,
        },
        false,
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_deadline_is_not_retirement_but_late_cleanup_reaches_stopped() {
    held_retirement(
        parrot::thread::config::SchedulingMode::DedicatedThread,
        false,
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shard_deadline_is_not_retirement_but_late_cleanup_reaches_stopped() {
    held_retirement(
        parrot::thread::config::SchedulingMode::Sharded {
            affinity_key: "held".into(),
        },
        false,
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_shutdown_waiter_does_not_lose_final_state() {
    held_retirement(
        parrot::thread::config::SchedulingMode::SharedPool {
            max_messages_per_run: 10,
        },
        true,
    )
    .await;
}

#[tokio::test]
async fn equal_actor_paths_in_distinct_registries_are_not_deduplicated() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    for name in ["first", "second"] {
        let engine = ThreadActorSystem::shared(Default::default());
        let actor = engine
            .spawn_at(Echo, "/same", None, Default::default())
            .await
            .unwrap();
        assert!(actor.send(Box::new(42_u64)).await.is_ok());
        facade
            .register_thread_system(name.into(), engine, false)
            .await
            .unwrap();
    }
    let count = facade.status().active_actors;
    facade.internal_shutdown().await.unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn failed_stop_hook_preserves_error_but_reports_finished_resources() {
    struct RejectStop;
    impl Actor for RejectStop {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn receive_message<'a>(
            &'a mut self,
            m: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(m) })
        }
        fn before_stop<'a>(
            &'a mut self,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async {
                Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "stop failed".into(),
                ))
            })
        }
    }
    let engine = ThreadActorSystem::shared(Default::default());
    let actor = engine
        .spawn_root_typed_thread(RejectStop, EmptyConfig)
        .await
        .unwrap();
    assert!(actor.send(Box::new(42_u64)).await.is_ok());
    let result = engine.shutdown_internal().await;
    wait_stopped(&engine).await;
    let state = engine.status();
    assert!(result.is_err());
    assert_eq!(
        (
            state.state,
            state.active_actors,
            state.resources.thread_count
        ),
        (parrot_api::system::SystemState::Stopped, 0, 0)
    );
}
