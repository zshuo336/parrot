//! Distribution must reach real owners and retain reservations through retirement.
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    system::ParrotActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{
        AsyncEngineRuntime, AsyncFactoryRuntime, AsyncLocalRuntime, FactoryOptions, LocalActor,
        SpawnOptions,
    },
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalTypedDispatch},
    message::Message,
    runtime::{LoadBalancingStrategy, RuntimeConfig, SchedulerConfig},
    supervisor::{DefaultStrategy, SupervisorStrategyType},
    system::ActorSystemConfig,
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{any::Any, collections::HashMap, rc::Rc, sync::Arc, thread::ThreadId, time::Duration};

const WAIT: Duration = Duration::from_secs(8);
struct Identity;
impl Message for Identity {
    type Result = ThreadId;
}
struct Dynamic;
impl Actor for Dynamic {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert!(message.is::<Identity>());
        Box::pin(async { Ok(Box::new(std::thread::current().id()) as BoxedMessage) })
    }
}
struct Static;
impl ParrotTypedDispatch for Static {
    type Msg = Identity;
    type Reply = ThreadId;
    fn dispatch(&mut self, _: Identity) -> BoxedFuture<'_, ActorResult<ThreadId>> {
        Box::pin(async { Ok(std::thread::current().id()) })
    }
}
impl ParrotMsgVariant<Static> for Identity {
    fn inject(value: Self) -> Self {
        value
    }
    fn extract(reply: ThreadId) -> ActorResult<ThreadId> {
        Ok(reply)
    }
}
struct Local(Rc<()>);
impl LocalTypedDispatch for Local {
    type Msg = Identity;
    type Reply = ThreadId;
    fn dispatch<'a>(
        &'a mut self,
        _: Identity,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<ThreadId>> {
        Box::pin(async move {
            assert_eq!(Rc::strong_count(&self.0), 1);
            let owner = std::thread::current().id();
            tokio::task::yield_now().await;
            assert_eq!(owner, std::thread::current().id());
            Ok(owner)
        })
    }
}
impl ParrotMsgVariant<LocalProtocol<Local>> for Identity {
    fn inject(value: Self) -> Self {
        value
    }
    fn extract(reply: ThreadId) -> ActorResult<ThreadId> {
        Ok(reply)
    }
}
impl LocalActor for Local {
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert!(message.is::<Identity>());
        Box::pin(
            async move { Ok(Box::new(self.dispatch(Identity, context).await?) as BoxedMessage) },
        )
    }
}

async fn configured(strategy: LoadBalancingStrategy) -> (ParrotActorSystem, Arc<ActixActorSystem>) {
    let facade = ParrotActorSystem::with_actix(
        ActorSystemConfig {
            runtime_config: RuntimeConfig {
                worker_threads: Some(3),
                scheduler_config: SchedulerConfig {
                    load_balancing: strategy,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
        ActixSystemSettings {
            arbiter_count: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let runtime = facade.runtime("default").unwrap();
    let native = Arc::new(
        (runtime.as_ref() as &dyn Any)
            .downcast_ref::<ActixActorSystem>()
            .unwrap()
            .clone(),
    );
    (facade, native)
}
async fn actor(system: &Arc<ActixActorSystem>, kind: usize, index: usize) -> BoxedActorRef {
    let path = format!("/distribution-{kind}-{index}");
    match kind {
        0 => system
            .spawn(Box::new(Dynamic), Box::new(()), SpawnOptions::at(path))
            .await
            .unwrap(),
        1 => system
            .spawn_typed::<Static, Identity>(Static, &path)
            .await
            .unwrap()
            .into_dyn(),
        2 => system
            .spawn_local_typed::<Local, Identity, _>(|| Local(Rc::new(())), &path)
            .await
            .unwrap()
            .into_dyn(),
        3 => system
            .spawn_factory(
                Box::new(|| Box::new(Dynamic)),
                FactoryOptions {
                    spawn: SpawnOptions::at(path),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        4 => {
            let group = system
                .root_cohort(SupervisorStrategyType::Default(
                    DefaultStrategy::StopOnFailure,
                ))
                .await
                .unwrap();
            system
                .spawn_typed_factory_in_cohort::<Static, Identity, _>(|| Static, &path, &group)
                .await
                .unwrap()
                .into_dyn()
        }
        _ => AsyncLocalRuntime::spawn_local_factory(
            system.as_ref(),
            Box::new(|| Box::new(Local(Rc::new(())))),
            FactoryOptions {
                spawn: SpawnOptions::at(path),
                ..Default::default()
            },
        )
        .await
        .unwrap(),
    }
}
async fn owner(actor: &BoxedActorRef) -> ThreadId {
    *tokio::time::timeout(WAIT, actor.send(Box::new(Identity)))
        .await
        .unwrap()
        .unwrap()
        .downcast::<ThreadId>()
        .unwrap()
}

#[actix::test]
async fn round_robin_and_least_loaded_balance_all_six_execution_entries() {
    for strategy in [
        LoadBalancingStrategy::RoundRobin,
        LoadBalancingStrategy::LeastLoaded,
    ] {
        let (facade, system) = configured(strategy).await;
        let mut actors = Vec::new();
        let mut cycle = Vec::new();
        for index in 0..18 {
            let actor = actor(&system, index % 6, index).await;
            let thread = owner(&actor).await;
            assert_ne!(thread, std::thread::current().id());
            assert_eq!(thread, owner(&actor).await);
            if index < 3 {
                cycle.push(thread);
            }
            assert_eq!(
                thread,
                cycle[index % 3],
                "entry {} bypassed placement",
                index % 6
            );
            actors.push(actor);
        }
        assert_eq!(
            cycle.iter().collect::<std::collections::HashSet<_>>().len(),
            3
        );
        tokio::time::timeout(WAIT, facade.shutdown_runtime())
            .await
            .unwrap()
            .unwrap();
        for actor in &actors {
            assert!(!actor.is_alive().await);
        }
    }
}

#[actix::test]
async fn least_loaded_reuses_the_owner_freed_by_each_execution_entry() {
    for kind in 0..6 {
        let (facade, system) = configured(LoadBalancingStrategy::LeastLoaded).await;
        let mut actors = Vec::new();
        let mut owners = Vec::new();
        for index in 0..3 {
            let actor = actor(&system, kind, index).await;
            owners.push(owner(&actor).await);
            actors.push(actor);
        }
        // The next round-robin slot is 0. Retiring slot 1 must select 1 instead.
        tokio::time::timeout(WAIT, actors[1].stop())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(WAIT, async {
            while system.arbiter_loads() != [1, 0, 1] {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let replacement = actor(&system, kind, 3).await;
        assert_eq!(
            owner(&replacement).await,
            owners[1],
            "entry {kind} retained or omitted its load"
        );
        tokio::time::timeout(WAIT, facade.shutdown_runtime())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(system.arbiter_loads(), [0, 0, 0]);
    }
}

#[actix::test]
async fn random_policy_preserves_real_actor_owners_across_all_entries() {
    let (facade, system) = configured(LoadBalancingStrategy::Random).await;
    let mut counts = HashMap::new();
    let mut actors = Vec::new();
    for index in 0..96 {
        let actor = actor(&system, index % 6, index).await;
        let thread = owner(&actor).await;
        *counts.entry(thread).or_insert(0) += 1;
        assert_eq!(thread, owner(&actor).await);
        actors.push(actor);
    }
    // RNG correctness is checked with a fixed seed in the private selector test.
    // A valid random sample need not visit every worker or repeat a worker.
    assert!(counts.len() <= 3);
    tokio::time::timeout(WAIT, facade.shutdown_runtime())
        .await
        .unwrap()
        .unwrap();
}
