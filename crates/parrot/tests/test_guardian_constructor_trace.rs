//! Constructor ownership must survive different call, poll and disposal traces.
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    system::ParrotActorSystem,
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncCohortRuntime, AsyncEngineRuntime, AsyncRootCohort},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType},
    system::{ActorSystem, ActorSystemConfig, GuardianConfig, SystemError, SystemState},
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

struct Capture(mpsc::Sender<Option<TraceContext>>);
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.0.send(TraceContext::current());
    }
}

fn guardian(
    sender: mpsc::Sender<Option<TraceContext>>,
    decisions: Arc<Mutex<Vec<Option<TraceContext>>>>,
    invalid: bool,
) -> GuardianConfig {
    let capture = Capture(sender);
    GuardianConfig {
        max_restarts: u32::from(invalid),
        restart_window: if invalid { Duration::ZERO } else { WAIT },
        supervision_strategy: SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 0,
            within: WAIT,
            decider: BasicDecisionFn::new(move |_| {
                let _ = &capture;
                decisions.lock().unwrap().push(TraceContext::current());
                SupervisionDecision::Resume
            }),
        }),
    }
}

#[derive(Clone, Copy)]
enum Entry {
    FacadeStart,
    FacadeThread,
    FacadeActix,
    ThreadGuardian,
    ActixGuardian,
    ThreadStart,
}
enum System {
    Facade(ParrotActorSystem),
    Thread(Arc<ThreadActorSystem>),
    Actix(ActixActorSystem),
}
struct Probe(Arc<AtomicUsize>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
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
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<()>() {
                Err(ActorError::MessageHandlingError(
                    "constructor trace witness".into(),
                ))
            } else {
                Ok(message)
            }
        })
    }
}
impl System {
    async fn exercise(&self) -> (BoxedActorRef, Arc<AtomicUsize>, TraceContext) {
        let runtime: &dyn AsyncEngineRuntime = match self {
            Self::Facade(system) => system,
            Self::Thread(system) => system.as_ref(),
            Self::Actix(system) => system,
        };
        let drops = Arc::new(AtomicUsize::new(0));
        let actor = runtime
            .spawn(
                Box::new(Probe(drops.clone())),
                Box::new(()),
                Default::default(),
            )
            .await
            .unwrap();
        let cause = TraceContext::root();
        let request = cause.in_scope(|| actor.send(Box::new(())));
        let error = TraceContext::root().scope(request).await.unwrap_err();
        assert!(
            error.to_string().contains("constructor trace witness"),
            "{error}"
        );
        assert_eq!(
            *actor
                .send(Box::new(47u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            47,
            "guardian Resume must leave the actual actor serving"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        (actor, drops, cause)
    }
    async fn close(self) {
        match self {
            Self::Facade(system) => {
                system.shutdown_runtime().await.unwrap();
                system.shutdown_runtime().await.unwrap();
                assert_eq!(
                    AsyncEngineRuntime::status(&system).state,
                    SystemState::Stopped
                );
            }
            Self::Thread(system) => {
                system.shutdown_internal().await.unwrap();
                system.shutdown_internal().await.unwrap();
                assert_eq!(
                    AsyncEngineRuntime::status(system.as_ref()).state,
                    SystemState::Stopped
                );
            }
            Self::Actix(system) => {
                system.clone().shutdown().await.unwrap();
                system.clone().shutdown().await.unwrap();
                assert_eq!(system.status().state, SystemState::Stopped);
                assert_eq!(system.status().resources.thread_count, 0);
            }
        }
    }
}

fn construct(
    entry: Entry,
    guardian: GuardianConfig,
    invalid_settings: bool,
) -> BoxedFuture<'static, Result<System, SystemError>> {
    let mut config = ActorSystemConfig {
        guardian_config: guardian,
        ..Default::default()
    };
    config.runtime_config.worker_threads = Some(1);
    let mut thread = ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    };
    let mut actix = ActixSystemSettings {
        arbiter_count: 1,
        ..Default::default()
    };
    if invalid_settings {
        // Native validation occurs after guardian extraction for Thread.
        thread.shared_queue_capacity = 0;
        actix.shutdown_timeout = Duration::MAX;
        if matches!(entry, Entry::FacadeStart | Entry::ThreadStart) {
            config.runtime_config.worker_threads = Some(0);
        }
    }
    match entry {
        Entry::FacadeStart => {
            let future = ParrotActorSystem::start(config);
            Box::pin(async { future.await.map(System::Facade) })
        }
        Entry::FacadeThread => {
            let future = ParrotActorSystem::with_thread(config, thread);
            Box::pin(async { future.await.map(System::Facade) })
        }
        Entry::FacadeActix => {
            let future = ParrotActorSystem::with_actix(config, actix);
            Box::pin(async { future.await.map(System::Facade) })
        }
        Entry::ThreadGuardian => {
            let future = ThreadActorSystem::with_guardian(thread, config.guardian_config);
            Box::pin(async { future.await.map(System::Thread) })
        }
        Entry::ActixGuardian => {
            let future = ActixActorSystem::with_guardian(actix, config.guardian_config);
            Box::pin(async { future.await.map(System::Actix) })
        }
        Entry::ThreadStart => {
            let future = <ThreadActorSystem as ActorSystem>::start(config);
            Box::pin(async {
                future.await.map(|system| {
                    let system = Arc::new(system);
                    system.set_self_weak(Arc::downgrade(&system));
                    System::Thread(system)
                })
            })
        }
    }
}

#[derive(Clone, Copy)]
enum Outcome {
    Accepted,
    InvalidGuardian,
    InvalidSettings,
    Unpolled,
}

fn contract(entry: Entry, outcome: Outcome) {
    for (capture, poll) in [
        (Some(TraceContext::root()), Some(TraceContext::root())),
        (None, Some(TraceContext::root())),
        (Some(TraceContext::root()), None),
    ] {
        let host = actix::System::new();
        let (sender, receiver) = mpsc::channel();
        let decisions = Arc::new(Mutex::new(Vec::new()));
        let construction = trace::in_scope(capture.clone(), || {
            construct(
                entry,
                guardian(
                    sender,
                    decisions.clone(),
                    matches!(outcome, Outcome::InvalidGuardian),
                ),
                matches!(outcome, Outcome::InvalidSettings),
            )
        });
        let disposal = Some(TraceContext::root());
        if matches!(outcome, Outcome::Unpolled) {
            trace::in_scope(disposal, || drop(construction));
            drop(host);
        } else {
            let result = host.block_on(trace::scope(poll, async {
                tokio::time::timeout(WAIT, construction)
                    .await
                    .expect("constructor stranded")
            }));
            match (outcome, result) {
                (Outcome::Accepted, Ok(system)) => {
                    let cause = host.block_on(trace::scope(disposal, async {
                        let (actor, drops, cause) = tokio::time::timeout(WAIT, system.exercise())
                            .await
                            .expect("actor failed to resume");
                        tokio::time::timeout(WAIT, system.close())
                            .await
                            .expect("retirement stranded");
                        assert_eq!(drops.load(Ordering::SeqCst), 1);
                        assert!(actor.send(Box::new(53u64)).await.is_err());
                        cause
                    }));
                    let observed = decisions.lock().unwrap();
                    assert_eq!(observed.len(), 1);
                    let actual = observed[0].as_ref().expect("failure context lost");
                    assert_eq!(
                        actual.trace_id(),
                        cause.trace_id(),
                        "decider lost failure causality"
                    );
                }
                (Outcome::InvalidGuardian, Err(error)) => {
                    assert!(error.to_string().contains("restart_window"), "{error}");
                }
                (Outcome::InvalidSettings, Err(error)) => {
                    assert!(
                        matches!(error, SystemError::InitializationError(_)),
                        "{error}"
                    );
                }
                (_, Ok(system)) => {
                    host.block_on(system.close());
                    panic!("invalid constructor accepted");
                }
                (_, Err(error)) => panic!("valid constructor rejected: {error}"),
            }
            drop(host);
        }
        assert_eq!(
            receiver.recv_timeout(WAIT).expect("policy capture leaked"),
            capture,
            "guardian disposal used poll/Drop context instead of constructor ownership"
        );
        assert!(
            matches!(
                receiver.recv_timeout(WAIT),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ),
            "policy still retained or disposed twice"
        );
        assert!(
            TraceContext::current().is_none(),
            "constructor scope escaped"
        );
    }
}

macro_rules! cases {
    ($entry:ident, $accepted:ident, $guardian:ident, $settings:ident, $unpolled:ident) => {
        #[test]
        fn $accepted() {
            contract(Entry::$entry, Outcome::Accepted);
        }
        #[test]
        fn $guardian() {
            contract(Entry::$entry, Outcome::InvalidGuardian);
        }
        #[test]
        fn $settings() {
            contract(Entry::$entry, Outcome::InvalidSettings);
        }
        #[test]
        fn $unpolled() {
            contract(Entry::$entry, Outcome::Unpolled);
        }
    };
}
cases!(
    FacadeStart,
    facade_start_accepted,
    facade_start_invalid_guardian,
    facade_start_invalid_settings,
    facade_start_unpolled
);
cases!(
    FacadeThread,
    facade_thread_accepted,
    facade_thread_invalid_guardian,
    facade_thread_invalid_settings,
    facade_thread_unpolled
);
cases!(
    FacadeActix,
    facade_actix_accepted,
    facade_actix_invalid_guardian,
    facade_actix_invalid_settings,
    facade_actix_unpolled
);
cases!(
    ThreadGuardian,
    thread_guardian_accepted,
    thread_guardian_invalid_guardian,
    thread_guardian_invalid_settings,
    thread_guardian_unpolled
);
cases!(
    ActixGuardian,
    actix_guardian_accepted,
    actix_guardian_invalid_guardian,
    actix_guardian_invalid_settings,
    actix_guardian_unpolled
);
cases!(
    ThreadStart,
    thread_start_accepted,
    thread_start_invalid_guardian,
    thread_start_invalid_settings,
    thread_start_unpolled
);

enum Group {
    Native(parrot::root_cohort::RootCohort),
    Runtime(Arc<dyn AsyncRootCohort>),
}
impl Group {
    async fn stop(&self) {
        match self {
            Self::Native(group) => group.stop().await.unwrap(),
            Self::Runtime(group) => group.stop().await.unwrap(),
        }
    }
}
fn group<'a>(
    system: &'a System,
    erased: bool,
    strategy: SupervisorStrategyType,
) -> BoxedFuture<'a, Result<Group, SystemError>> {
    match (system, erased) {
        (System::Thread(system), false) => {
            let future = system.root_cohort(strategy);
            Box::pin(async {
                future
                    .await
                    .map(Group::Native)
                    .map_err(SystemError::ActorError)
            })
        }
        (System::Actix(system), false) => {
            let future = system.root_cohort(strategy);
            Box::pin(async {
                future
                    .await
                    .map(Group::Native)
                    .map_err(SystemError::ActorError)
            })
        }
        (System::Thread(system), true) => {
            let future = AsyncCohortRuntime::root_cohort(system.as_ref(), strategy);
            Box::pin(async { future.await.map(Group::Runtime) })
        }
        (System::Actix(system), true) => {
            let future = AsyncCohortRuntime::root_cohort(system, strategy);
            Box::pin(async { future.await.map(Group::Runtime) })
        }
        _ => unreachable!(),
    }
}
fn cohort_contract(actix: bool, erased: bool, outcome: Outcome) {
    for (capture, poll) in [
        (Some(TraceContext::root()), Some(TraceContext::root())),
        (None, Some(TraceContext::root())),
        (Some(TraceContext::root()), None),
    ] {
        let host = actix::System::new();
        let system = host.block_on(async {
            if actix {
                System::Actix(ActixActorSystem::with_arbiter_count(1).await.unwrap())
            } else {
                System::Thread(ThreadActorSystem::shared(ThreadActorSystemConfig {
                    shared_pool_size: 1,
                    shared_burst_workers_max: 0,
                    ..Default::default()
                }))
            }
        });
        if matches!(outcome, Outcome::InvalidSettings) {
            host.block_on(async {
                match &system {
                    System::Thread(system) => system.shutdown_internal().await.unwrap(),
                    System::Actix(system) => system.clone().shutdown().await.unwrap(),
                    _ => unreachable!(),
                }
            });
        }
        let (sender, receiver) = mpsc::channel();
        let construction = trace::in_scope(capture.clone(), || {
            group(
                &system,
                erased,
                guardian(sender, Arc::default(), false).supervision_strategy,
            )
        });
        if matches!(outcome, Outcome::Unpolled) {
            TraceContext::root().in_scope(|| drop(construction));
        } else {
            let result = host.block_on(trace::scope(poll, async {
                tokio::time::timeout(WAIT, construction)
                    .await
                    .expect("cohort constructor stranded")
            }));
            match (outcome, result) {
                (Outcome::Accepted, Ok(group)) => {
                    host.block_on(TraceContext::root().scope(async {
                        tokio::time::timeout(WAIT, async {
                            group.stop().await;
                            group.stop().await;
                        })
                        .await
                        .expect("cohort retirement stranded");
                    }));
                    drop(group);
                }
                (Outcome::InvalidSettings, Err(SystemError::ActorError(ActorError::Stopped))) => {}
                _ => panic!("cohort admission result did not match the live system state"),
            }
        }
        host.block_on(system.close());
        drop(host);
        assert_eq!(
            receiver.recv_timeout(WAIT).expect("cohort policy leaked"),
            capture,
            "cohort disposal used poll/Drop context instead of constructor ownership"
        );
        assert!(matches!(
            receiver.recv_timeout(WAIT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(TraceContext::current().is_none());
    }
}
macro_rules! cohort_cases {
    ($actix:expr, $erased:expr, $accepted:ident, $closed:ident, $unpolled:ident) => {
        #[test]
        fn $accepted() {
            cohort_contract($actix, $erased, Outcome::Accepted);
        }
        #[test]
        fn $closed() {
            cohort_contract($actix, $erased, Outcome::InvalidSettings);
        }
        #[test]
        fn $unpolled() {
            cohort_contract($actix, $erased, Outcome::Unpolled);
        }
    };
}
cohort_cases!(
    false,
    false,
    thread_cohort_accepted,
    thread_cohort_closed,
    thread_cohort_unpolled
);
cohort_cases!(
    true,
    false,
    actix_cohort_accepted,
    actix_cohort_closed,
    actix_cohort_unpolled
);
cohort_cases!(
    false,
    true,
    runtime_thread_cohort_accepted,
    runtime_thread_cohort_closed,
    runtime_thread_cohort_unpolled
);
cohort_cases!(
    true,
    true,
    runtime_actix_cohort_accepted,
    runtime_actix_cohort_closed,
    runtime_actix_cohort_unpolled
);
