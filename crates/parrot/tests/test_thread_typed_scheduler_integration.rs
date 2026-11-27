//! Static and dynamic actors share native scheduling ownership and limits.
use parrot::thread::{
    ThreadActorSystem,
    config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
};
use parrot_api::typed::{ParrotMsgVariant, ParrotTypedDispatch};
use parrot_api::{
    Message,
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{Arc, Condvar, Mutex},
    thread::ThreadId,
    time::Duration,
};

enum Op {
    Owner,
    Hold(Arc<tokio::sync::Notify>),
    Block(Arc<Drops>),
}
impl Message for Op {
    type Result = ThreadId;
}
#[derive(Default)]
struct Drops {
    owners: Mutex<Vec<ThreadId>>,
    entered: tokio::sync::Notify,
    gate: Option<(Mutex<bool>, Condvar)>,
}
impl Drops {
    fn wait(&self) {
        self.entered.notify_one();
        if let Some((open, changed)) = &self.gate {
            let mut open = open.lock().unwrap();
            while !*open {
                open = changed.wait(open).unwrap();
            }
        }
    }
    fn release(&self) {
        if let Some((open, changed)) = &self.gate {
            *open.lock().unwrap() = true;
            changed.notify_all();
        }
    }
}
struct Release(Arc<Drops>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Static(Arc<Drops>);
impl Drop for Static {
    fn drop(&mut self) {
        self.0.wait();
        self.0
            .owners
            .lock()
            .unwrap()
            .push(std::thread::current().id());
    }
}
impl ParrotTypedDispatch for Static {
    type Msg = Op;
    type Reply = ThreadId;
    fn dispatch(&mut self, message: Op) -> BoxedFuture<'_, ActorResult<ThreadId>> {
        Box::pin(async move {
            match message {
                Op::Owner => Ok(std::thread::current().id()),
                Op::Hold(entered) => {
                    entered.notify_one();
                    std::future::pending().await
                }
                Op::Block(gate) => {
                    gate.wait();
                    Ok(std::thread::current().id())
                }
            }
        })
    }
}
impl ParrotMsgVariant<Static> for Op {
    fn inject(value: Self) -> Op {
        value
    }
    fn extract(value: ThreadId) -> ActorResult<ThreadId> {
        Ok(value)
    }
}
struct Dynamic;
impl Actor for Dynamic {
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
}
fn sharded() -> SchedulingMode {
    SchedulingMode::Sharded {
        affinity_key: "shared-native-domain".into(),
    }
}
fn system(mode: SchedulingMode) -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_scheduling_mode: mode,
        default_ask_timeout: Duration::from_secs(3),
        shutdown_timeout: Duration::from_secs(2),
        ..Default::default()
    })
}
async fn empty(system: &ThreadActorSystem) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while system.actor_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual registration retirement stalled");
}

#[tokio::test]
async fn explicit_shared_placement_enforces_one_poll_budget_across_both_protocols() {
    use parrot_api::runtime::{LoadBalancingStrategy, RuntimeConfig, SchedulerConfig};
    for strategy in [
        LoadBalancingStrategy::Random,
        LoadBalancingStrategy::LeastLoaded,
    ] {
        let system = ThreadActorSystem::with_runtime_config(
            ThreadActorSystemConfig {
                shared_burst_workers_max: 0,
                ..Default::default()
            },
            RuntimeConfig {
                worker_threads: Some(1),
                scheduler_config: SchedulerConfig {
                    load_balancing: strategy,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let dynamic = system
            .spawn_at(Dynamic, "/other", None, ThreadActorConfig::default())
            .await
            .unwrap();
        dynamic.send(Box::new(())).await.unwrap();
        let actor = system
            .spawn_typed::<_, Op>(Static(Arc::default()), "/holder")
            .await
            .unwrap();
        let gate = Arc::new(Drops {
            gate: Some((Mutex::new(false), Condvar::new())),
            ..Default::default()
        });
        let _release = Release(gate.clone());
        actor.tell(Op::Block(gate.clone())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
            .await
            .unwrap();
        let reply = dynamic.send(Box::new(()));
        tokio::pin!(reply);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut reply)
                .await
                .is_err(),
            "dynamic handler ran while the static handler occupied the only worker"
        );
        gate.release();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), reply)
                .await
                .unwrap()
                .unwrap()
                .is::<ThreadId>()
        );
        let entered = Arc::new(tokio::sync::Notify::new());
        actor.tell(Op::Hold(entered.clone())).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        assert!(
            dynamic
                .send_with_timeout(Box::new(()), Some(Duration::from_secs(3)))
                .await
                .unwrap()
                .is::<ThreadId>()
        );
        actor.stop().await.unwrap();
        dynamic.stop().await.unwrap();
        system.shutdown_internal().await.unwrap();
    }
}

#[tokio::test]
async fn static_and_dynamic_share_the_same_native_shard_and_reusable_path() {
    let system = system(sharded());
    let dynamic = system
        .spawn_at(Dynamic, "/dynamic", None, ThreadActorConfig::default())
        .await
        .unwrap();
    let owner = *dynamic
        .send_with_timeout(Box::new(()), Some(Duration::from_secs(3)))
        .await
        .unwrap()
        .downcast::<ThreadId>()
        .unwrap();
    for _ in 0..2 {
        let drops = Arc::new(Drops::default());
        let actor = system
            .spawn_typed::<_, Op>(Static(drops.clone()), "/static")
            .await
            .unwrap();
        assert_eq!(actor.ask(Op::Owner).await.unwrap(), owner);
        actor.stop().await.unwrap();
        assert_eq!(*drops.owners.lock().unwrap(), [owner]);
        assert_eq!(system.actor_count(), 1);
    }
    dynamic.stop().await.unwrap();
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn suspended_static_actor_does_not_block_another_on_the_same_shard() {
    let system = system(sharded());
    let first = system
        .spawn_typed::<_, Op>(Static(Arc::default()), "/first")
        .await
        .unwrap();
    let first_owner = first.ask(Op::Owner).await.unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    first.tell(Op::Hold(entered.clone())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let second = system
        .spawn_typed::<_, Op>(Static(Arc::default()), "/second")
        .await
        .unwrap();
    assert_eq!(second.ask(Op::Owner).await.unwrap(), first_owner);
    second.stop().await.unwrap();
    first.stop().await.unwrap();
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn dedicated_static_and_dynamic_share_the_worker_limit_and_release_it() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        max_dedicated_threads: 1,
        default_scheduling_mode: SchedulingMode::DedicatedThread,
        default_ask_timeout: Duration::from_secs(3),
        ..Default::default()
    });
    let dynamic = system
        .spawn_at(Dynamic, "/dynamic", None, ThreadActorConfig::default())
        .await
        .unwrap();
    dynamic
        .send_with_timeout(Box::new(()), Some(Duration::from_secs(3)))
        .await
        .unwrap();
    let drops = Arc::new(Drops::default());
    let rejected = system
        .spawn_typed::<_, Op>(Static(drops.clone()), "/static")
        .await
        .unwrap();
    assert!(rejected.ask(Op::Owner).await.is_err());
    let failure = rejected.stop().await.unwrap_err();
    assert!(
        failure
            .to_string()
            .contains("Dedicated thread limit reached"),
        "{failure}"
    );
    assert_eq!(drops.owners.lock().unwrap().len(), 1);
    assert_eq!(
        system.scheduler_group().dedicated_scheduler.worker_count(),
        1
    );
    dynamic.stop().await.unwrap();
    let actor = system
        .spawn_typed::<_, Op>(Static(Arc::default()), "/static")
        .await
        .unwrap();
    actor.ask(Op::Owner).await.unwrap();
    actor.stop().await.unwrap();
    assert_eq!(
        system.scheduler_group().dedicated_scheduler.worker_count(),
        0
    );
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn dedicated_timeout_keeps_path_and_worker_until_real_disposal_and_join() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        max_dedicated_threads: 1,
        default_scheduling_mode: SchedulingMode::DedicatedThread,
        shutdown_timeout: Duration::from_millis(50),
        ..Default::default()
    });
    let drops = Arc::new(Drops {
        gate: Some((Mutex::new(false), Condvar::new())),
        ..Default::default()
    });
    let _release = Release(drops.clone());
    let actor = system
        .spawn_typed::<_, Op>(Static(drops.clone()), "/blocked")
        .await
        .unwrap();
    let owner = actor.ask(Op::Owner).await.unwrap();
    assert!(actor.stop().await.is_err());
    tokio::time::timeout(Duration::from_secs(2), drops.entered.notified())
        .await
        .unwrap();
    assert!(drops.owners.lock().unwrap().is_empty());
    assert_eq!(system.actor_count(), 1);
    assert_eq!(
        system.scheduler_group().dedicated_scheduler.worker_count(),
        1
    );
    assert!(
        system
            .spawn_typed::<_, Op>(Static(Arc::default()), "/blocked")
            .await
            .is_err()
    );
    drops.release();
    empty(&system).await;
    assert_eq!(*drops.owners.lock().unwrap(), [owner]);
    assert_eq!(
        system.scheduler_group().dedicated_scheduler.worker_count(),
        0
    );
    let replacement = system
        .spawn_typed::<_, Op>(Static(Arc::default()), "/blocked")
        .await
        .unwrap();
    replacement.ask(Op::Owner).await.unwrap();
    replacement.stop().await.unwrap();
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn shard_exit_cancels_static_work_and_preserves_its_actual_retirement() {
    let system = system(sharded());
    let drops = Arc::new(Drops::default());
    let actor = system
        .spawn_typed::<_, Op>(Static(drops.clone()), "/exit")
        .await
        .unwrap();
    let owner = actor.ask(Op::Owner).await.unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    actor.tell(Op::Hold(entered.clone())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    system
        .scheduler_group()
        .sharded()
        .shutdown_result()
        .unwrap();
    empty(&system).await;
    assert!(actor.stop().await.is_err());
    assert_eq!(*drops.owners.lock().unwrap(), [owner]);
    assert!(!actor.is_alive());
    system.shutdown_internal().await.unwrap();
}
