//! Static protocols must use the configured native scheduler for their lifetime.
use parrot::{
    thread::{
        ThreadActorSystem,
        config::{SchedulingMode, ThreadActorSystemConfig},
    },
    typed_context::{TypedContext, TypedContextExt},
};
use parrot_api::{
    Message,
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{Arc, Mutex},
    thread::ThreadId,
    time::Duration,
};

#[derive(Clone)]
struct Event {
    generation: usize,
    stage: &'static str,
    thread: ThreadId,
    name: String,
}
type Events = Arc<Mutex<Vec<Event>>>;

enum Op {
    Ping,
    Fail,
    Child,
}
impl Message for Op {
    type Result = ThreadId;
}
struct Probe {
    events: Events,
    generation: usize,
}
impl Probe {
    fn record(&self, stage: &'static str) -> ThreadId {
        let thread = std::thread::current();
        self.events.lock().unwrap().push(Event {
            generation: self.generation,
            stage,
            thread: thread.id(),
            name: thread.name().unwrap_or("").to_owned(),
        });
        thread.id()
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.record("drop");
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Op;
    type Reply = ThreadId;

    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.record("init construct");
        Box::pin(async move {
            self.record("init poll");
            tokio::time::sleep(Duration::from_millis(1)).await;
            self.record("init resumed");
            Ok(())
        })
    }
    fn dispatch(&mut self, _: Op) -> BoxedFuture<'_, ActorResult<ThreadId>> {
        unreachable!("managed context required")
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        message: Op,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<ThreadId>> {
        self.record("handler construct");
        Box::pin(async move {
            self.record("handler poll");
            tokio::time::sleep(Duration::from_millis(1)).await;
            let owner = self.record("handler resumed");
            match message {
                Op::Ping => Ok(owner),
                Op::Fail => Err(ActorError::InternalError("replace generation".into())),
                Op::Child => {
                    let child = context
                        .spawn_typed_child::<_, Op>(Probe {
                            events: self.events.clone(),
                            generation: 100,
                        })
                        .await?;
                    let owner = child.ask(Op::Ping).await?;
                    child.stop().await?;
                    Ok(owner)
                }
            }
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.record("stop construct");
        Box::pin(async move {
            self.record("stop poll");
            tokio::time::sleep(Duration::from_millis(1)).await;
            self.record("stop resumed");
            Ok(())
        })
    }
}
impl ParrotMsgVariant<Probe> for Op {
    fn inject(value: Self) -> Op {
        value
    }
    fn extract(value: ThreadId) -> ActorResult<ThreadId> {
        Ok(value)
    }
}

fn sharded() -> SchedulingMode {
    SchedulingMode::Sharded {
        affinity_key: "static-lifecycle".into(),
    }
}
fn assert_owner(events: &Events, generation: usize, prefix: &str) -> ThreadId {
    let events = events.lock().unwrap();
    let selected: Vec<_> = events
        .iter()
        .filter(|event| event.generation == generation)
        .collect();
    assert!(!selected.is_empty());
    let owner = selected[0].thread;
    for event in selected {
        assert!(
            event.name.starts_with(prefix),
            "{} ran on {:?}, expected {prefix}",
            event.stage,
            event.name
        );
        assert_eq!(event.thread, owner, "{} migrated", event.stage);
    }
    owner
}
async fn lifecycle(mode: SchedulingMode, prefix: &str, factory: bool) {
    let events = Events::default();
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_scheduling_mode: mode,
        default_ask_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_secs(3),
        ..Default::default()
    });
    let reference = if factory {
        let generation = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let events = events.clone();
        system
            .spawn_typed_factory_with_strategy::<_, Op, _>(
                move || Probe {
                    events: events.clone(),
                    generation: generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                },
                "/static-affinity",
                OneForOneStrategy {
                    max_restarts: 1,
                    within: Duration::from_secs(30),
                    decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
                },
            )
            .await
            .unwrap()
    } else {
        system
            .spawn_typed::<_, Op>(
                Probe {
                    events: events.clone(),
                    generation: 0,
                },
                "/static-affinity",
            )
            .await
            .unwrap()
    };
    let first = reference.ask(Op::Ping).await.unwrap();
    let child = reference.ask(Op::Child).await.unwrap();
    let replacement = if factory {
        assert!(reference.ask(Op::Fail).await.is_err());
        Some(reference.ask(Op::Ping).await.unwrap())
    } else {
        None
    };
    reference.stop().await.unwrap();
    system.shutdown_internal().await.unwrap();
    assert_eq!(system.actor_count(), 0);
    assert_eq!(assert_owner(&events, 0, prefix), first);
    assert_eq!(assert_owner(&events, 100, prefix), child);
    if factory {
        assert_eq!(assert_owner(&events, 1, prefix), first);
        assert_eq!(replacement, Some(first));
    }
    if prefix == "parrot-shard-" {
        assert_eq!(child, first, "same affinity must use the same native shard");
    } else {
        assert_ne!(child, first, "each dedicated actor needs its own OS worker");
    }
    let events = events.lock().unwrap();
    for generation in [0, 100] {
        assert_eq!(
            events
                .iter()
                .filter(|event| event.generation == generation && event.stage == "drop")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn dedicated_instance_and_child_keep_native_lifecycle_affinity() {
    lifecycle(SchedulingMode::DedicatedThread, "parrot-dedicated-", false).await;
}
#[tokio::test]
async fn sharded_instance_and_child_share_native_affinity() {
    lifecycle(sharded(), "parrot-shard-", false).await;
}
#[tokio::test]
async fn dedicated_factory_replacement_keeps_its_native_owner() {
    lifecycle(SchedulingMode::DedicatedThread, "parrot-dedicated-", true).await;
}
#[tokio::test]
async fn sharded_factory_replacement_keeps_its_native_owner() {
    lifecycle(sharded(), "parrot-shard-", true).await;
}
