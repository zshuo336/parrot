//! Static protocols must use the configured native scheduler for their lifetime.
use parrot::{
    system::ParrotActorSystem,
    thread::{
        ThreadActorSystem,
        config::{SchedulingMode, ThreadActorSystemConfig},
    },
    typed_context::{TypedContext, TypedContextExt},
};
use parrot_api::{
    ActorSystem, Message,
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
fn assert_lifecycle(
    events: &[Event],
    generation: usize,
    owner: ThreadId,
    prefix: &str,
    messages: usize,
    normal_stop: bool,
) {
    let selected: Vec<_> = events
        .iter()
        .filter(|event| event.generation == generation)
        .collect();
    let mut expected = vec!["init construct", "init poll", "init resumed"];
    for _ in 0..messages {
        expected.extend(["handler construct", "handler poll", "handler resumed"]);
    }
    if normal_stop {
        expected.extend(["stop construct", "stop poll", "stop resumed"]);
    }
    expected.push("drop");
    assert_eq!(
        selected.iter().map(|event| event.stage).collect::<Vec<_>>(),
        expected,
        "generation {generation} lifecycle"
    );
    for event in selected {
        assert!(
            event.name.starts_with(prefix),
            "{} ran on {:?}, expected {prefix}",
            event.stage,
            event.name
        );
        assert_eq!(event.thread, owner, "{} migrated", event.stage);
    }
}
#[derive(Clone, Copy)]
enum Entry {
    Native,
    Default,
    Named,
}

async fn lifecycle(mode: SchedulingMode, prefix: &str, factory: bool, entry: Entry) {
    let events = Events::default();
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_scheduling_mode: mode,
        default_ask_timeout: Duration::from_secs(5),
        shutdown_timeout: Duration::from_secs(3),
        ..Default::default()
    });
    let facade = match entry {
        Entry::Native => None,
        Entry::Default | Entry::Named => {
            let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
            facade
                .register_thread_system(
                    "fallback".into(),
                    ThreadActorSystem::shared(Default::default()),
                    true,
                )
                .await
                .unwrap();
            facade
                .register_thread_system(
                    "selected".into(),
                    system.clone(),
                    matches!(entry, Entry::Default),
                )
                .await
                .unwrap();
            Some(facade)
        }
    };
    const PATH: &str = "/static-affinity";
    let reference = if factory {
        let generation = std::sync::atomic::AtomicUsize::new(0);
        let events = events.clone();
        let make = move || Probe {
            events: events.clone(),
            generation: generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        };
        let strategy = OneForOneStrategy {
            max_restarts: 1,
            within: Duration::from_secs(30),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
        };
        match &facade {
            None => {
                system
                    .spawn_typed_factory_with_strategy::<_, Op, _>(make, PATH, strategy)
                    .await
            }
            Some(f) if matches!(entry, Entry::Named) => {
                f.spawn_typed_factory_with_strategy_on::<_, Op, _>("selected", make, PATH, strategy)
                    .await
            }
            Some(f) => {
                f.spawn_typed_factory_with_strategy::<_, Op, _>(make, PATH, strategy)
                    .await
            }
        }
    } else {
        let actor = Probe {
            events: events.clone(),
            generation: 0,
        };
        match &facade {
            None => system.spawn_typed::<_, Op>(actor, PATH).await,
            Some(f) if matches!(entry, Entry::Named) => {
                f.spawn_typed_on::<_, Op>("selected", actor, PATH).await
            }
            Some(f) => f.spawn_typed::<_, Op>(actor, PATH).await,
        }
    }
    .unwrap();
    assert_eq!(
        system.actor_count(),
        1,
        "root was registered on the wrong engine"
    );
    let first = reference.ask(Op::Ping).await.unwrap();
    let child = reference.ask(Op::Child).await.unwrap();
    let replacement = if factory {
        assert!(reference.ask(Op::Fail).await.is_err());
        Some(reference.ask(Op::Ping).await.unwrap())
    } else {
        None
    };
    reference.stop().await.unwrap();
    if let Some(facade) = facade {
        facade.shutdown().await.unwrap();
    } else {
        system.shutdown_internal().await.unwrap();
    }
    assert_eq!(system.actor_count(), 0);
    let events = events.lock().unwrap();
    assert_lifecycle(&events, 100, child, prefix, 1, true);
    if let Some(replacement) = replacement {
        assert_lifecycle(&events, 0, first, prefix, 3, false);
        assert_lifecycle(&events, 1, replacement, prefix, 1, true);
        assert_eq!(replacement, first);
        let retired = events
            .iter()
            .rposition(|event| event.generation == 0)
            .unwrap();
        let restarted = events
            .iter()
            .position(|event| event.generation == 1)
            .unwrap();
        assert!(retired < restarted, "old generation survived replacement");
    } else {
        assert_lifecycle(&events, 0, first, prefix, 2, true);
    }
    if prefix == "parrot-shard-" {
        assert_eq!(child, first, "same affinity must use the same native shard");
    } else {
        assert_ne!(child, first, "each dedicated actor needs its own OS worker");
    }
}

#[tokio::test]
async fn dedicated_instance_and_child_keep_native_lifecycle_affinity() {
    lifecycle(
        SchedulingMode::DedicatedThread,
        "parrot-dedicated-",
        false,
        Entry::Native,
    )
    .await;
}
#[tokio::test]
async fn sharded_instance_and_child_share_native_affinity() {
    lifecycle(sharded(), "parrot-shard-", false, Entry::Native).await;
}
#[tokio::test]
async fn dedicated_factory_replacement_keeps_its_native_owner() {
    lifecycle(
        SchedulingMode::DedicatedThread,
        "parrot-dedicated-",
        true,
        Entry::Native,
    )
    .await;
}
#[tokio::test]
async fn sharded_factory_replacement_keeps_its_native_owner() {
    lifecycle(sharded(), "parrot-shard-", true, Entry::Native).await;
}

async fn facade_lifecycle(entry: Entry, factory: bool) {
    lifecycle(
        SchedulingMode::DedicatedThread,
        "parrot-dedicated-",
        factory,
        entry,
    )
    .await;
    lifecycle(sharded(), "parrot-shard-", factory, entry).await;
}

#[tokio::test]
async fn facade_default_instance_and_child_use_selected_workers() {
    facade_lifecycle(Entry::Default, false).await;
}
#[tokio::test]
async fn facade_named_instance_and_child_use_selected_workers() {
    facade_lifecycle(Entry::Named, false).await;
}
#[tokio::test]
async fn facade_default_factory_restarts_on_selected_worker() {
    facade_lifecycle(Entry::Default, true).await;
}
#[tokio::test]
async fn facade_named_factory_restarts_on_selected_worker() {
    facade_lifecycle(Entry::Named, true).await;
}
