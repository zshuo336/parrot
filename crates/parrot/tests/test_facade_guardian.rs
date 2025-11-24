//! Configured facade startup executes real roots and preserves external policies.
use futures::FutureExt;
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    system::ParrotActorSystem,
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, FactoryOptions, SpawnOptions},
    errors::ActorError,
    supervisor::{
        BasicDecisionFn, DefaultStrategy, OneForAllStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    system::{ActorSystem, ActorSystemConfig, GuardianConfig},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
const WAIT: Duration = Duration::from_secs(8);

#[derive(Default)]
struct Plan {
    builds: AtomicUsize,
    inits: AtomicUsize,
    drops: AtomicUsize,
}
impl Plan {
    fn make(self: &Arc<Self>) -> Probe {
        Probe {
            plan: self.clone(),
            generation: self.builds.fetch_add(1, Ordering::SeqCst),
            failures: 0,
        }
    }
}
struct Probe {
    plan: Arc<Plan>,
    generation: usize,
    failures: usize,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.plan.drops.fetch_add(1, Ordering::SeqCst);
    }
}
enum Command {
    Read,
    Fail,
    Failures,
}
impl Actor for Probe {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, context: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        assert!(context.parent().is_none());
        self.plan.inits.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = match *message.downcast::<Command>().unwrap() {
                Command::Read => self.generation,
                Command::Failures => self.failures,
                Command::Fail => {
                    self.failures += 1;
                    return Err(ActorError::MessageHandlingError(
                        "facade guardian witness".into(),
                    ));
                }
            };
            Ok(Box::new(value) as BoxedMessage)
        })
    }
}
fn all() -> GuardianConfig {
    GuardianConfig {
        max_restarts: 1,
        restart_window: Duration::from_secs(60),
        supervision_strategy: SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 99,
            within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
        }),
    }
}
fn config(guardian_config: GuardianConfig) -> ActorSystemConfig {
    ActorSystemConfig {
        name: "configured-facade".into(),
        guardian_config,
        ..Default::default()
    }
}
async fn facade(actix: bool, config: ActorSystemConfig) -> ParrotActorSystem {
    if actix {
        ParrotActorSystem::with_actix(
            config,
            ActixSystemSettings {
                arbiter_count: 2,
                shutdown_timeout: WAIT,
                ..Default::default()
            },
        )
        .await
        .unwrap()
    } else {
        ParrotActorSystem::with_thread(
            config,
            ThreadActorSystemConfig {
                shared_pool_size: 2,
                shared_burst_workers_max: 0,
                shutdown_timeout: WAIT,
                ..Default::default()
            },
        )
        .await
        .unwrap()
    }
}
async fn ask(reference: &BoxedActorRef, command: Command) -> ActorResult<usize> {
    reference
        .send(Box::new(command))
        .await
        .map(|reply| *reply.downcast::<usize>().unwrap())
}
async fn recovered(reference: &BoxedActorRef) {
    tokio::time::timeout(WAIT, async {
        loop {
            match ask(reference, Command::Read).await {
                Ok(generation) => {
                    assert_eq!(generation, 1, "wrong replacement generation");
                    return;
                }
                Err(ActorError::Stopped) => tokio::task::yield_now().await,
                Err(error) => panic!("replacement failed: {error}"),
            }
        }
    })
    .await
    .expect("replacement was constructed but never published");
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn spawn(system: &ParrotActorSystem, plan: &Arc<Plan>, path: &str) -> BoxedActorRef {
    let source = plan.clone();
    system
        .factory_runtime()
        .unwrap()
        .spawn_factory(
            Box::new(move || Box::new(source.make())),
            FactoryOptions {
                spawn: SpawnOptions::at(path),
                max_restarts: 99,
                ..Default::default()
            },
        )
        .await
        .unwrap()
}
async fn shutdown(system: &ParrotActorSystem) {
    if let Err(error) = system.shutdown_runtime().await {
        assert!(
            error.to_string().contains("facade guardian witness"),
            "{error}"
        );
    }
}
macro_rules! both {
    ($name:ident, $body:ident) => {
        mod $name {
            use super::*;
            #[actix::test]
            async fn thread() {
                $body(false).await;
            }
            #[actix::test]
            async fn actix() {
                $body(true).await;
            }
        }
    };
}
async fn shared_budget(actix: bool) {
    let system = facade(actix, config(all())).await;
    let plans = [Arc::new(Plan::default()), Arc::new(Plan::default())];
    let a = spawn(&system, &plans[0], "/first").await;
    let b = spawn(&system, &plans[1], "/second").await;
    assert_eq!(ask(&a, Command::Read).await.unwrap(), 0);
    assert_eq!(ask(&b, Command::Read).await.unwrap(), 0);
    let identity = b.death_watch().unwrap().id();
    assert!(ask(&a, Command::Fail).await.is_err());
    until(|| {
        plans
            .iter()
            .all(|plan| plan.inits.load(Ordering::SeqCst) == 2)
    })
    .await;
    recovered(&a).await;
    recovered(&b).await;
    assert_eq!(
        AsyncEngineRuntime::get_actor(&system, "/second")
            .await
            .unwrap()
            .death_watch()
            .unwrap()
            .id(),
        identity
    );
    assert!(ask(&b, Command::Fail).await.is_err());
    until(|| {
        plans
            .iter()
            .all(|plan| plan.drops.load(Ordering::SeqCst) == 2)
    })
    .await;
    assert!(ask(&a, Command::Read).await.is_err());
    assert!(ask(&b, Command::Read).await.is_err());
    for plan in plans {
        assert_eq!(plan.builds.load(Ordering::SeqCst), 2);
    }
    shutdown(&system).await;
}
both!(
    facade_created_engines_use_guardian_and_one_shared_budget,
    shared_budget
);

#[tokio::test]
async fn trait_start_publishes_a_usable_guardian_controlled_default_engine() {
    let system = <ParrotActorSystem as ActorSystem>::start(config(all()))
        .await
        .unwrap();
    let a_plan = Arc::new(Plan::default());
    let b_plan = Arc::new(Plan::default());
    let a = spawn(&system, &a_plan, "/trait-first").await;
    let b = spawn(&system, &b_plan, "/trait-second").await;
    assert_eq!(ask(&a, Command::Read).await.unwrap(), 0);
    assert_eq!(ask(&b, Command::Read).await.unwrap(), 0);
    assert!(ask(&a, Command::Fail).await.is_err());
    until(|| b_plan.inits.load(Ordering::SeqCst) == 2).await;
    recovered(&b).await;
    shutdown(&system).await;
}

async fn resume(actix: bool) {
    let system = facade(
        actix,
        config(GuardianConfig {
            supervision_strategy: SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure),
            ..Default::default()
        }),
    )
    .await;
    let plan = Arc::new(Plan::default());
    let reference = system
        .spawn(
            Box::new(plan.make()),
            Box::new(()),
            SpawnOptions::at("/instance"),
        )
        .await
        .unwrap();
    for expected in 1..=2 {
        assert!(ask(&reference, Command::Fail).await.is_err());
        assert_eq!(ask(&reference, Command::Failures).await.unwrap(), expected);
    }
    assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
    shutdown(&system).await;
    assert_eq!(plan.drops.load(Ordering::SeqCst), 1);
}
both!(configured_resume_applies_to_facade_instances, resume);

async fn external_policy(actix: bool) {
    for external_actix in [false, true] {
        let guardian = GuardianConfig {
            supervision_strategy: SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure),
            ..Default::default()
        };
        let external: Arc<dyn AsyncEngineRuntime> = if external_actix {
            Arc::new(
                ActixActorSystem::with_guardian(
                    ActixSystemSettings {
                        arbiter_count: 2,
                        ..Default::default()
                    },
                    guardian,
                )
                .await
                .unwrap(),
            )
        } else {
            ThreadActorSystem::with_guardian(
                ThreadActorSystemConfig {
                    shared_pool_size: 2,
                    shared_burst_workers_max: 0,
                    ..Default::default()
                },
                guardian,
            )
            .await
            .unwrap()
        };
        let plan = Arc::new(Plan::default());
        let reference = external
            .spawn(
                Box::new(plan.make()),
                Box::new(()),
                SpawnOptions::at("/external"),
            )
            .await
            .unwrap();
        assert_eq!(ask(&reference, Command::Failures).await.unwrap(), 0);
        let system = facade(actix, config(all())).await;
        let erased: Arc<dyn Any + Send + Sync> = external.clone();
        if external_actix {
            let native = erased.downcast::<ActixActorSystem>().ok().unwrap();
            system
                .register_actix_system("external".into(), (*native).clone(), false)
                .await
                .unwrap();
        } else {
            let native = erased.downcast::<ThreadActorSystem>().ok().unwrap();
            system
                .register_thread_system("external".into(), native, false)
                .await
                .unwrap();
        }
        system
            .register_runtime("alias".into(), external.clone(), false)
            .unwrap();
        assert!(ask(&reference, Command::Fail).await.is_err());
        assert_eq!(ask(&reference, Command::Failures).await.unwrap(), 1);
        assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
        let removed = system.unregister_system("external").unwrap().unwrap();
        assert!(ask(&reference, Command::Fail).await.is_err());
        assert_eq!(ask(&reference, Command::Failures).await.unwrap(), 2);
        drop(removed);
        shutdown(&system).await;
        assert_eq!(plan.drops.load(Ordering::SeqCst), 1);
    }
}
both!(
    registering_running_external_aliases_preserves_their_policy,
    external_policy
);

struct Capture {
    drops: Arc<AtomicUsize>,
    hostile: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.hostile, "facade config capture destructor");
    }
}
fn captured(drops: &Arc<AtomicUsize>, hostile: bool) -> ActorSystemConfig {
    let capture = Capture {
        drops: drops.clone(),
        hostile,
    };
    let mut guardian = all();
    if let SupervisorStrategyType::OneForAll(policy) = &mut guardian.supervision_strategy {
        policy.decider = BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Restart
        });
    }
    config(guardian)
}
#[tokio::test]
async fn unpolled_facade_constructors_protect_configuration_captures() {
    let drops = Arc::new(AtomicUsize::new(0));
    for kind in 0..6 {
        let config = captured(&drops, true);
        let future = match kind {
            0 => ParrotActorSystem::new(config),
            1 => ParrotActorSystem::start(config),
            2 => ParrotActorSystem::with_thread(config, Default::default()),
            3 => ParrotActorSystem::with_actix(config, Default::default()),
            4 => <ParrotActorSystem as ActorSystem>::start(config),
            _ => {
                let future = <ThreadActorSystem as ActorSystem>::start(config);
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok()
                );
                until(|| drops.load(Ordering::SeqCst) == kind + 1).await;
                continue;
            }
        };
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok());
        until(|| drops.load(Ordering::SeqCst) == kind + 1).await;
    }
}
async fn policy_retirement(actix: bool) {
    let drops = Arc::new(AtomicUsize::new(0));
    let system = facade(actix, captured(&drops, false)).await;
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    system.shutdown_runtime().await.unwrap();
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "facade must transfer rather than retain a second policy owner"
    );
    drop(system);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
both!(
    shutdown_retires_configured_policy_while_facade_is_still_alive,
    policy_retirement
);

#[tokio::test]
async fn empty_registry_final_drop_protects_stored_configuration() {
    let drops = Arc::new(AtomicUsize::new(0));
    let system = ParrotActorSystem::new(captured(&drops, true))
        .await
        .unwrap();
    system.shutdown_runtime().await.unwrap();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(system))).is_ok());
    until(|| drops.load(Ordering::SeqCst) == 1).await;
}

async fn invalid_configuration(actix: bool) {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut config = captured(&drops, false);
    config.guardian_config.restart_window = Duration::ZERO;
    let error = if actix {
        ParrotActorSystem::with_actix(config, Default::default())
            .await
            .err()
            .unwrap()
    } else {
        ParrotActorSystem::with_thread(config, Default::default())
            .await
            .err()
            .unwrap()
    };
    assert!(
        error.to_string().contains("guardian restart_window"),
        "{error}"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
both!(
    invalid_guardian_rejects_startup_and_joins_capture_disposal,
    invalid_configuration
);

async fn failed_initialization(actix: bool) {
    let drops = Arc::new(AtomicUsize::new(0));
    let config = captured(&drops, false);
    let error = if actix {
        ParrotActorSystem::with_actix(
            config,
            ActixSystemSettings {
                mailbox_capacity: 0,
                ..Default::default()
            },
        )
        .await
        .err()
        .unwrap()
    } else {
        ParrotActorSystem::with_thread(
            config,
            ThreadActorSystemConfig {
                shared_queue_capacity: 0,
                ..Default::default()
            },
        )
        .await
        .err()
        .unwrap()
    };
    assert!(
        matches!(
            error,
            parrot_api::system::SystemError::InitializationError(_)
        ),
        "{error}"
    );
    assert!(
        error.to_string().contains(if actix {
            "actix.mailbox_capacity"
        } else {
            "queue"
        }),
        "{error}"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
both!(
    failed_native_initialization_joins_unpublished_policy_disposal,
    failed_initialization
);

async fn rejected_runtime_configuration(hostile: bool) {
    use parrot_api::system::SystemError;

    for entry in 0..3 {
        for invalid in 0..3 {
            let drops = Arc::new(AtomicUsize::new(0));
            let mut config = captured(&drops, hostile);
            match invalid {
                0 => config.runtime_config.worker_threads = Some(0),
                1 => config.runtime_config.io_threads = Some(0),
                _ => {
                    config.timeouts.message_handling = Duration::from_secs(2);
                    config.runtime_config.scheduler_config.task_timeout = Duration::from_secs(1);
                }
            }
            let future = match entry {
                0 => ParrotActorSystem::start(config),
                1 => ParrotActorSystem::with_thread(config, Default::default()),
                _ => <ParrotActorSystem as ActorSystem>::start(config),
            };
            let result = std::panic::AssertUnwindSafe(future)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| panic!("startup unwound: entry={entry}, invalid={invalid}"));
            let error = result
                .err()
                .expect("invalid runtime configuration accepted");
            assert_eq!(
                drops.load(Ordering::SeqCst),
                1,
                "policy disposal not joined"
            );
            if hostile {
                assert!(matches!(error, SystemError::ActorError(_)), "{error}");
                assert!(error.to_string().contains("disposal failed"), "{error}");
            } else {
                assert!(
                    matches!(error, SystemError::InitializationError(_)),
                    "{error}"
                );
            }
        }
    }
}

#[tokio::test]
async fn accepted_thread_placement_configurations_start_and_shutdown() {
    use parrot_api::runtime::LoadBalancingStrategy;

    for strategy in [
        LoadBalancingStrategy::Random,
        LoadBalancingStrategy::LeastLoaded,
    ] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut config = captured(&drops, false);
        config.runtime_config.scheduler_config.load_balancing = strategy;
        let system = ParrotActorSystem::with_thread(config, Default::default())
            .await
            .unwrap();
        system.shutdown_runtime().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn rejected_runtime_configuration_joins_policy_disposal() {
    rejected_runtime_configuration(false).await;
}

#[tokio::test]
async fn rejected_runtime_configuration_preserves_policy_disposal_failure() {
    rejected_runtime_configuration(true).await;
}

#[actix::test]
async fn explicit_native_settings_reach_the_created_engines() {
    let thread = facade(false, config(all())).await;
    let runtime = thread.runtime("default").unwrap();
    let native = (runtime.as_ref() as &dyn Any)
        .downcast_ref::<ThreadActorSystem>()
        .unwrap();
    assert_eq!(native.config().name, "configured-facade");
    assert_eq!(native.config().shared_pool_size, 2);
    assert_eq!(native.config().shared_burst_workers_max, 0);
    shutdown(&thread).await;
    let actix = facade(true, config(all())).await;
    let runtime = actix.runtime("default").unwrap();
    let native = (runtime.as_ref() as &dyn Any)
        .downcast_ref::<ActixActorSystem>()
        .unwrap();
    assert_eq!(native.arbiter_size(), 2);
    shutdown(&actix).await;
}
