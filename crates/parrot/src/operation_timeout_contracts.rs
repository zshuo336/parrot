//! Real handler cancellation, state retention and initialization retirement.
use crate::{
    actix::{ActixActorSystem, ActixSystemSettings},
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    Message,
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, FactoryOptions, SpawnOptions},
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalMsgVariant, LocalTypedDispatch},
    supervisor::{DefaultStrategy, SupervisorStrategyType},
    system::{GuardianConfig, SystemTimeouts},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_context::TypedContext,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
const WAIT: Duration = Duration::from_secs(5);
const CREATION_WAIT: Duration = Duration::from_millis(800);

mod dynamic_stream;
mod reconstruction;

#[derive(Default)]
struct Plan {
    builds: AtomicUsize,
    entered: AtomicUsize,
    cancelled: AtomicUsize,
    actors: AtomicUsize,
    init_hangs: bool,
    init_delay: Duration,
    init_completed: AtomicUsize,
    bad_drop: bool,
}
struct Work(Arc<Plan>);
impl Drop for Work {
    fn drop(&mut self) {
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
        assert!(!self.0.bad_drop, "timed handler disposal witness");
    }
}
struct Probe(Arc<Plan>);
impl Probe {
    fn new(plan: Arc<Plan>) -> Self {
        plan.builds.fetch_add(1, Ordering::SeqCst);
        Self(plan)
    }
    async fn initialize(&self) -> ActorResult<()> {
        if !self.0.init_delay.is_zero() {
            tokio::time::sleep(self.0.init_delay).await;
            self.0.init_completed.fetch_add(1, Ordering::SeqCst);
        }
        if self.0.init_hangs {
            self.hang().await?;
        }
        Ok(())
    }
    async fn hang(&self) -> ActorResult<usize> {
        let _work = Work(self.0.clone());
        self.0.entered.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
    async fn execute(&self, op: Op) -> ActorResult<usize> {
        match op {
            Op::Hang => self.hang().await,
            Op::Read => Ok(self.0.entered.load(Ordering::SeqCst)),
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.actors.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Clone, Copy)]
enum Op {
    Hang,
    Read,
}
impl Message for Op {
    type Result = usize;
}
impl ParrotMsgVariant<Probe> for Op {
    fn inject(value: Self) -> Op {
        value
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Op;
    type Reply = usize;
    fn init_with_context<'a>(
        &'a mut self,
        _: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(self.initialize())
    }
    fn dispatch(&mut self, op: Op) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(self.execute(op))
    }
}
struct Dynamic(Probe);
impl Actor for Dynamic {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(self.0.initialize())
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.0
                .execute(*message.downcast::<Op>().unwrap())
                .await
                .map(|value| Box::new(value) as BoxedMessage)
        })
    }
}
struct Local {
    probe: Probe,
    _owner: Rc<()>,
}
impl LocalMsgVariant<Op> for Local {
    fn inject(value: Op) -> Op {
        value
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
impl LocalTypedDispatch for Local {
    type Msg = Op;
    type Reply = usize;
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(self.probe.initialize())
    }
    fn dispatch<'a>(
        &'a mut self,
        op: Op,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<usize>> {
        Box::pin(self.probe.execute(op))
    }
}
fn guardian() -> GuardianConfig {
    GuardianConfig {
        supervision_strategy: SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure),
        ..Default::default()
    }
}
fn timeouts() -> SystemTimeouts {
    SystemTimeouts {
        actor_creation: Duration::from_millis(100),
        message_handling: Duration::from_millis(40),
        system_shutdown: Duration::from_secs(2),
    }
}
enum System {
    Thread(Arc<ThreadActorSystem>),
    Actix(Arc<ActixActorSystem>),
}
impl System {
    async fn new(actix: bool) -> Self {
        if actix {
            Self::Actix(Arc::new(
                ActixActorSystem::with_guardian_and_timeouts(
                    ActixSystemSettings {
                        arbiter_count: 1,
                        ..Default::default()
                    },
                    guardian(),
                    timeouts(),
                )
                .await
                .unwrap(),
            ))
        } else {
            Self::Thread(
                ThreadActorSystem::with_guardian_and_timeouts(
                    ThreadActorSystemConfig {
                        shared_pool_size: 2,
                        shared_burst_workers_max: 0,
                        ..Default::default()
                    },
                    guardian(),
                    timeouts(),
                )
                .await
                .unwrap(),
            )
        }
    }
    fn runtime(&self) -> &dyn AsyncEngineRuntime {
        match self {
            Self::Thread(system) => &**system,
            Self::Actix(system) => &**system,
        }
    }
    async fn shutdown(&self, failed: bool) {
        let result = tokio::time::timeout(WAIT, self.runtime().shutdown())
            .await
            .unwrap();
        if !failed {
            result.unwrap();
        }
    }
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
fn timeout<T>(result: ActorResult<T>) {
    assert!(
        matches!(
            result,
            Err(ActorError::Timeout) | Err(ActorError::TimeoutDetail(_))
        ),
        "handler must report its execution timeout"
    );
}
async fn dynamic(actix: bool, factory: bool, initializing: bool) {
    let system = System::new(actix).await;
    let plan = Arc::new(Plan {
        init_hangs: initializing,
        ..Default::default()
    });
    let result = tokio::time::timeout(CREATION_WAIT, async {
        if factory {
            let source = plan.clone();
            system
                .runtime()
                .factory_runtime()
                .unwrap()
                .spawn_factory(
                    Box::new(move || Box::new(Dynamic(Probe::new(source.clone())))),
                    FactoryOptions {
                        spawn: SpawnOptions::at("/timed"),
                        ..Default::default()
                    },
                )
                .await
        } else {
            system
                .runtime()
                .spawn(
                    Box::new(Dynamic(Probe::new(plan.clone()))),
                    Box::new(()),
                    SpawnOptions::at("/timed"),
                )
                .await
        }
    })
    .await
    .expect("creation ignored the configured budget");
    if initializing {
        // Some entries return the logical reference before init; neither may
        // activate a timed-out generation or retain its unfinished future.
        if let Ok(reference) = result {
            let reply = tokio::time::timeout(CREATION_WAIT, reference.send(Box::new(Op::Read)))
                .await
                .unwrap();
            assert!(reply.is_err(), "failed init became a live actor");
        }
        until(|| plan.actors.load(Ordering::SeqCst) == 1).await;
    } else {
        let reference = result.unwrap();
        timeout(
            tokio::time::timeout(WAIT, reference.send(Box::new(Op::Hang)))
                .await
                .unwrap(),
        );
        assert_eq!(
            plan.cancelled.load(Ordering::SeqCst),
            1,
            "timeout returned before actual future disposal"
        );
        let value = reference.send(Box::new(Op::Read)).await.unwrap();
        assert_eq!(*value.downcast::<usize>().unwrap(), 1, "Resume lost state");
    }
    system.shutdown(initializing).await;
    assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
    assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(plan.actors.load(Ordering::SeqCst), 1);
}
async fn typed(actix: bool, factory: bool, initializing: bool) {
    let system = System::new(actix).await;
    let plan = Arc::new(Plan {
        init_hangs: initializing,
        ..Default::default()
    });
    let source = plan.clone();
    let result = tokio::time::timeout(CREATION_WAIT, async {
        match &system {
            System::Thread(system) if factory => {
                system
                    .spawn_typed_factory::<Probe, Op, _>(
                        move || Probe::new(source.clone()),
                        "/timed",
                    )
                    .await
            }
            System::Thread(system) => {
                system
                    .spawn_typed::<Probe, Op>(Probe::new(source), "/timed")
                    .await
            }
            System::Actix(system) if factory => {
                system
                    .spawn_typed_factory::<Probe, Op, _>(
                        move || Probe::new(source.clone()),
                        "/timed",
                    )
                    .await
            }
            System::Actix(system) => {
                system
                    .spawn_typed::<Probe, Op>(Probe::new(source), "/timed")
                    .await
            }
        }
    })
    .await
    .expect("creation ignored the configured budget");
    if initializing {
        if let Ok(reference) = result {
            assert!(
                tokio::time::timeout(CREATION_WAIT, reference.ask(Op::Read))
                    .await
                    .unwrap()
                    .is_err()
            );
        }
        until(|| plan.actors.load(Ordering::SeqCst) == 1).await;
    } else {
        let reference = result.unwrap();
        timeout(
            tokio::time::timeout(WAIT, reference.ask(Op::Hang))
                .await
                .unwrap(),
        );
        assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(reference.ask(Op::Read).await.unwrap(), 1);
    }
    system.shutdown(initializing).await;
    assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
    assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(plan.actors.load(Ordering::SeqCst), 1);
}
async fn local(factory: bool, initializing: bool) {
    let system = System::new(true).await;
    let System::Actix(engine) = &system else {
        unreachable!()
    };
    let plan = Arc::new(Plan {
        init_hangs: initializing,
        ..Default::default()
    });
    let source = plan.clone();
    let make = move || Local {
        probe: Probe::new(source.clone()),
        _owner: Rc::new(()),
    };
    let result = if factory {
        engine
            .spawn_local_typed_factory::<Local, Op, _>(make, "/timed")
            .await
    } else {
        engine
            .spawn_local_typed::<Local, Op, _>(make, "/timed")
            .await
    };
    if initializing {
        if let Ok(reference) = result {
            assert!(
                tokio::time::timeout(CREATION_WAIT, reference.ask(Op::Read))
                    .await
                    .unwrap()
                    .is_err()
            );
        }
        until(|| plan.actors.load(Ordering::SeqCst) == 1).await;
    } else {
        let reference = result.unwrap();
        timeout(
            tokio::time::timeout(WAIT, reference.ask(Op::Hang))
                .await
                .unwrap(),
        );
        assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(reference.ask(Op::Read).await.unwrap(), 1);
    }
    system.shutdown(initializing).await;
    assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
    assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(plan.actors.load(Ordering::SeqCst), 1);
}
macro_rules! engine_cases {
    ($module:ident, $body:ident) => {
        mod $module {
            use super::*;
            #[actix::test]
            async fn thread_instance_handler() {
                $body(false, false, false).await;
            }
            #[actix::test]
            async fn thread_factory_handler() {
                $body(false, true, false).await;
            }
            #[actix::test]
            async fn actix_instance_handler() {
                $body(true, false, false).await;
            }
            #[actix::test]
            async fn actix_factory_handler() {
                $body(true, true, false).await;
            }
            #[actix::test]
            async fn thread_instance_init() {
                $body(false, false, true).await;
            }
            #[actix::test]
            async fn thread_factory_init() {
                $body(false, true, true).await;
            }
            #[actix::test]
            async fn actix_instance_init() {
                $body(true, false, true).await;
            }
            #[actix::test]
            async fn actix_factory_init() {
                $body(true, true, true).await;
            }
        }
    };
}
engine_cases!(dynamic_entries, dynamic);
engine_cases!(static_entries, typed);
#[actix::test]
async fn local_instance_handler() {
    local(false, false).await;
}
#[actix::test]
async fn local_factory_handler() {
    local(true, false).await;
}
#[actix::test]
async fn local_instance_init() {
    local(false, true).await;
}
#[actix::test]
async fn local_factory_init() {
    local(true, true).await;
}

async fn public_start(actix: bool, trait_start: bool) {
    use parrot_api::system::{ActorSystem, ActorSystemConfig};
    let config = ActorSystemConfig {
        guardian_config: guardian(),
        timeouts: timeouts(),
        ..Default::default()
    };
    let facade = if trait_start {
        <crate::ParrotActorSystem as ActorSystem>::start(config)
            .await
            .unwrap()
    } else if actix {
        crate::ParrotActorSystem::with_actix(
            config,
            ActixSystemSettings {
                arbiter_count: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap()
    } else {
        crate::ParrotActorSystem::with_thread(
            config,
            ThreadActorSystemConfig {
                shared_pool_size: 2,
                shared_burst_workers_max: 0,
                ..Default::default()
            },
        )
        .await
        .unwrap()
    };
    let plan = Arc::new(Plan::default());
    let actor = AsyncEngineRuntime::spawn(
        &facade,
        Box::new(Dynamic(Probe::new(plan.clone()))),
        Box::new(()),
        SpawnOptions::at("/public-timed"),
    )
    .await
    .unwrap();
    timeout(
        tokio::time::timeout(WAIT, actor.send(Box::new(Op::Hang)))
            .await
            .unwrap(),
    );
    assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(
        *actor
            .send(Box::new(Op::Read))
            .await
            .unwrap()
            .downcast::<usize>()
            .unwrap(),
        1
    );
    facade.shutdown_runtime().await.unwrap();
    assert_eq!(plan.actors.load(Ordering::SeqCst), 1);
}
#[actix::test]
async fn public_facade_thread_maps_handler_timeout() {
    public_start(false, false).await;
}
#[actix::test]
async fn public_facade_actix_maps_handler_timeout() {
    public_start(true, false).await;
}
#[actix::test]
async fn public_facade_trait_start_maps_handler_timeout() {
    public_start(false, true).await;
}
#[tokio::test]
async fn thread_trait_start_maps_handler_timeout() {
    use parrot_api::system::{ActorSystem, ActorSystemConfig};
    let system = Arc::new(
        <ThreadActorSystem as ActorSystem>::start(ActorSystemConfig {
            guardian_config: guardian(),
            timeouts: timeouts(),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    system.set_self_weak(Arc::downgrade(&system));
    let plan = Arc::new(Plan::default());
    let actor = AsyncEngineRuntime::spawn(
        &*system,
        Box::new(Dynamic(Probe::new(plan.clone()))),
        Box::new(()),
        SpawnOptions::at("/native-timed"),
    )
    .await
    .unwrap();
    timeout(
        tokio::time::timeout(WAIT, actor.send(Box::new(Op::Hang)))
            .await
            .unwrap(),
    );
    assert_eq!(
        *actor
            .send(Box::new(Op::Read))
            .await
            .unwrap()
            .downcast::<usize>()
            .unwrap(),
        1
    );
    system.shutdown_internal().await.unwrap();
    assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(plan.actors.load(Ordering::SeqCst), 1);
}

async fn failed_disposal(actix: bool) {
    let system = System::new(actix).await;
    let plan = Arc::new(Plan {
        bad_drop: true,
        ..Default::default()
    });
    let source = plan.clone();
    let actor = match &system {
        System::Thread(system) => system
            .spawn_typed_factory::<Probe, Op, _>(move || Probe::new(source.clone()), "/bad-drop")
            .await
            .unwrap(),
        System::Actix(system) => system
            .spawn_typed_factory::<Probe, Op, _>(move || Probe::new(source.clone()), "/bad-drop")
            .await
            .unwrap(),
    };
    assert!(
        tokio::time::timeout(WAIT, actor.ask(Op::Hang))
            .await
            .unwrap()
            .is_err()
    );
    until(|| plan.actors.load(Ordering::SeqCst) == 1).await;
    assert!(
        actor.ask(Op::Read).await.is_err(),
        "failed disposal reused actor storage"
    );
    system.shutdown(true).await;
    assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(
        plan.builds.load(Ordering::SeqCst),
        1,
        "failed disposal reconstructed actor"
    );
}
#[actix::test]
async fn thread_timeout_disposal_failure_vetoes_resume_and_restart() {
    failed_disposal(false).await;
}
#[actix::test]
async fn actix_timeout_disposal_failure_vetoes_resume_and_restart() {
    failed_disposal(true).await;
}

#[tokio::test(start_paused = true)]
async fn execution_deadline_cancels_the_future_before_reporting_timeout() {
    use crate::typed_supervision::{DispatchCleanup, guarded_stage};
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };
    struct PendingWork(Arc<AtomicUsize>, Arc<AtomicUsize>);
    impl Future for PendingWork {
        type Output = ActorResult<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Poll::Pending
        }
    }
    impl Drop for PendingWork {
        fn drop(&mut self) {
            self.1.fetch_add(1, Ordering::SeqCst);
        }
    }
    let polls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let (mut first, mut cancelled) = (None, false);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut future = Box::pin(guarded_stage(
        || PendingWork(polls.clone(), drops.clone()),
        DispatchCleanup::new(&mut first, &mut cancelled).with_deadline(Some(deadline)),
    ));
    assert!(futures::poll!(future.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    timeout(future.await.unwrap());
    assert_eq!(
        polls.load(Ordering::SeqCst),
        1,
        "expired handler was polled again"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(first.is_none());
    assert!(!cancelled);
}

#[tokio::test(start_paused = true)]
async fn zero_handler_timeout_keeps_pending_work_until_owner_cancels() {
    use crate::typed_supervision::{DispatchCleanup, guarded_stage};
    let plan = Arc::new(Plan::default());
    let (mut first, mut cancelled) = (None, false);
    let probe = Probe::new(plan.clone());
    let mut work = Box::pin(guarded_stage(
        || Box::pin(probe.hang()),
        DispatchCleanup::new(&mut first, &mut cancelled).with_timeout(Duration::ZERO),
    ));
    assert!(futures::poll!(work.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert!(futures::poll!(work.as_mut()).is_pending());
    assert_eq!(plan.cancelled.load(Ordering::SeqCst), 0);
    drop(work);
    assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
    assert!(first.is_none());
    assert!(!cancelled);
}

async fn combined_creation_budget(actix: bool, typed: bool) {
    let system = System::new(actix).await;
    let plan = Arc::new(Plan {
        init_delay: Duration::from_millis(80),
        ..Default::default()
    });
    let source = plan.clone();
    let make = move || {
        std::thread::sleep(Duration::from_millis(60));
        Probe::new(source.clone())
    };
    tokio::time::timeout(CREATION_WAIT, async {
        if typed {
            let result = match &system {
                System::Thread(engine) => {
                    engine
                        .spawn_typed_factory::<Probe, Op, _>(make, "/shared-deadline")
                        .await
                }
                System::Actix(engine) => {
                    engine
                        .spawn_typed_factory::<Probe, Op, _>(make, "/shared-deadline")
                        .await
                }
            };
            if let Ok(reference) = result {
                assert!(
                    reference.ask(Op::Read).await.is_err(),
                    "init reset the construction budget"
                );
            }
        } else {
            let result = system
                .runtime()
                .factory_runtime()
                .unwrap()
                .spawn_factory(
                    Box::new(move || Box::new(Dynamic(make()))),
                    FactoryOptions {
                        spawn: SpawnOptions::at("/shared-deadline"),
                        ..Default::default()
                    },
                )
                .await;
            if let Ok(reference) = result {
                assert!(
                    reference.send(Box::new(Op::Read)).await.is_err(),
                    "init reset the construction budget"
                );
            }
        }
    })
    .await
    .expect("creation ignored its configured deadline");
    until(|| plan.actors.load(Ordering::SeqCst) == 1).await;
    assert_eq!(plan.init_completed.load(Ordering::SeqCst), 0);
    system.shutdown(true).await;
    assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
}
#[actix::test]
async fn thread_dynamic_constructor_and_init_share_one_budget() {
    combined_creation_budget(false, false).await;
}
#[actix::test]
async fn actix_dynamic_constructor_and_init_share_one_budget() {
    combined_creation_budget(true, false).await;
}
#[actix::test]
async fn thread_static_constructor_and_init_share_one_budget() {
    combined_creation_budget(false, true).await;
}
#[actix::test]
async fn actix_static_constructor_and_init_share_one_budget() {
    combined_creation_budget(true, true).await;
}

#[tokio::test]
async fn late_constructor_output_is_retired_even_when_its_destructor_panics() {
    use crate::typed_supervision::{DispatchCleanup, guarded_stage};
    struct Output(Arc<AtomicUsize>);
    impl Drop for Output {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("late output disposal witness");
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let (mut first, mut cancelled) = (None, false);
    let result = guarded_stage(
        || {
            std::thread::sleep(Duration::from_millis(20));
            let output = Output(drops.clone());
            let polls = polls.clone();
            Box::pin(async move {
                polls.fetch_add(1, Ordering::SeqCst);
                Ok(output)
            })
        },
        DispatchCleanup::new(&mut first, &mut cancelled).with_timeout(Duration::from_millis(5)),
    )
    .await
    .unwrap();
    timeout(result);
    assert_eq!(polls.load(Ordering::SeqCst), 0, "polled expired user work");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(first.is_some(), "deadline swallowed a real cleanup failure");
    assert!(cancelled, "failed cancellation could permit actor reuse");
}

#[actix::test]
async fn synchronous_actix_handler_reports_late_completion_without_replaying_work() {
    use crate::actix::{actor::ActixActor, context::ActixContext};
    use parrot_api::actor::{ActixEngineExt, EmptyConfig, EngineContextHandle};
    struct SyncProbe(Probe);
    impl Actor for SyncProbe {
        type Config = EmptyConfig;
        type Context = ActixContext<ActixActor<Self>>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn receive_message<'a>(
            &'a mut self,
            _: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { panic!("sync actor entered the async fallback") })
        }
    }
    impl ActixEngineExt for SyncProbe {
        fn receive_message_with_engine<'a>(
            &'a mut self,
            message: BoxedMessage,
            _: &'a mut Self::Context,
            _: EngineContextHandle<'_>,
        ) -> Option<ActorResult<BoxedMessage>> {
            if let Op::Hang = *message.downcast::<Op>().unwrap() {
                self.0.0.entered.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(80));
            }
            Some(Ok(Box::new(self.0.0.entered.load(Ordering::SeqCst))))
        }
    }
    let system = System::new(true).await;
    let System::Actix(engine) = &system else {
        unreachable!()
    };
    let plan = Arc::new(Plan::default());
    let actor = engine
        .spawn_root_typed(SyncProbe(Probe::new(plan.clone())), EmptyConfig)
        .await
        .unwrap();
    timeout(
        tokio::time::timeout(WAIT, actor.send(Box::new(Op::Hang)))
            .await
            .unwrap(),
    );
    let reply = actor.send(Box::new(Op::Read)).await.unwrap();
    assert_eq!(*reply.downcast::<usize>().unwrap(), 1);
    system.shutdown(false).await;
    assert_eq!(plan.builds.load(Ordering::SeqCst), 1);
    assert_eq!(plan.actors.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn public_start_rejects_unrepresentable_operation_deadlines() {
    use parrot_api::system::{ActorSystem, ActorSystemConfig, SystemError};
    for field in 0..3 {
        for entry in 0..4 {
            let mut limits = timeouts();
            match field {
                0 => limits.actor_creation = Duration::MAX,
                1 => limits.message_handling = Duration::MAX,
                _ => limits.system_shutdown = Duration::MAX,
            }
            let config = ActorSystemConfig {
                timeouts: limits,
                ..Default::default()
            };
            let error = match entry {
                0 => crate::ParrotActorSystem::start(config).await.err(),
                1 => crate::ParrotActorSystem::with_actix(config, ActixSystemSettings::default())
                    .await
                    .err(),
                2 => <crate::ParrotActorSystem as ActorSystem>::start(config)
                    .await
                    .err(),
                _ => <ThreadActorSystem as ActorSystem>::start(config)
                    .await
                    .err(),
            };
            let Some(SystemError::InitializationError(message)) = error else {
                panic!("entry {entry} did not reject invalid timeout field {field}");
            };
            assert!(
                message.contains(["actor_creation", "message_handling", "system_shutdown"][field])
            );
        }
    }
}

#[actix::test]
async fn shutdown_budget_maps_to_native_engines_and_zero_preserves_settings() {
    for budget in [Duration::ZERO, Duration::from_millis(73)] {
        let native = Duration::from_millis(211);
        let limits = SystemTimeouts {
            system_shutdown: budget,
            ..Default::default()
        };
        let thread = ThreadActorSystem::with_guardian_and_timeouts(
            ThreadActorSystemConfig {
                shutdown_timeout: native,
                shared_pool_size: 1,
                shared_burst_workers_max: 0,
                ..Default::default()
            },
            guardian(),
            limits.clone(),
        )
        .await
        .unwrap();
        let actix = ActixActorSystem::with_guardian_and_timeouts(
            ActixSystemSettings {
                shutdown_timeout: native,
                arbiter_count: 1,
                ..Default::default()
            },
            guardian(),
            limits,
        )
        .await
        .unwrap();
        let expected = if budget.is_zero() { native } else { budget };
        assert_eq!(thread.config().shutdown_timeout, expected);
        assert_eq!(actix.settings.shutdown_timeout, expected);
        thread.shutdown_internal().await.unwrap();
        AsyncEngineRuntime::shutdown(&actix).await.unwrap();
    }
}

#[tokio::test]
async fn thread_batch_execution_timeout_cancels_work_and_preserves_cardinality() {
    use crate::thread::{actor::ThreadActor, context::ThreadContext};
    use parrot_api::actor::EmptyConfig;
    struct BatchProbe(Probe);
    impl Actor for BatchProbe {
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
            Box::pin(async { panic!("batch contract used the serial handler") })
        }
        fn receive_batch<'a>(
            &'a mut self,
            _: Vec<BoxedMessage>,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
            Box::pin(async move {
                self.0.hang().await.unwrap();
                unreachable!()
            })
        }
    }
    let system = System::new(false).await;
    let System::Thread(engine) = &system else {
        unreachable!()
    };
    let plan = Arc::new(Plan::default());
    let mut actor = ThreadActor::new_for_test(BatchProbe(Probe::new(plan.clone())));
    let mut context = ThreadContext::new_for_test_with("/batch-timeout", Arc::downgrade(engine));
    actor.initialize(&mut context).await.unwrap();
    actor
        .process_message(
            Box::new(crate::thread::envelope::ControlMessage::Start),
            &mut context,
        )
        .await
        .unwrap();
    let results = tokio::time::timeout(
        WAIT,
        actor.process_business_batch(
            vec![Box::new(Op::Hang), Box::new(Op::Hang), Box::new(Op::Hang)],
            &mut context,
        ),
    )
    .await
    .unwrap();
    assert_eq!(results.len(), 3);
    for result in results {
        timeout(result);
    }
    assert_eq!(plan.entered.load(Ordering::SeqCst), 1);
    assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
    drop(actor);
    drop(context);
    system.shutdown(false).await;
    assert_eq!(plan.actors.load(Ordering::SeqCst), 1);
}

struct StreamLocal {
    inner: Local,
    stage: u8,
    errors: Arc<AtomicUsize>,
}
impl LocalMsgVariant<Op> for StreamLocal {
    fn inject(value: Op) -> Op {
        value
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
impl LocalTypedDispatch for StreamLocal {
    type Msg = Op;
    type Reply = usize;
    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        streams: &'a mut parrot_api::local_typed::LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.stage == 4 {
                self.inner.probe.hang().await?;
            }
            if self.stage == 3 {
                streams
                    .add_stream(futures::stream::poll_fn(
                        |_| -> std::task::Poll<Option<u32>> {
                            panic!("stream source failure witness")
                        },
                    ))
                    .unwrap();
            } else {
                streams.add_stream(futures::stream::iter([1_u32])).unwrap();
            }
            Ok(())
        })
    }
    fn local_stream_started<'a>(
        &'a mut self,
        _: u64,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.stage == 0 {
                self.inner.probe.hang().await?;
            }
            Ok(())
        })
    }
    fn local_stream_item<'a>(
        &'a mut self,
        _: u64,
        _: Box<dyn Any>,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.stage == 1 {
                self.inner.probe.hang().await?;
            }
            Ok(())
        })
    }
    fn local_stream_finished<'a>(
        &'a mut self,
        _: u64,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.stage == 2 {
                self.inner.probe.hang().await?;
            }
            Ok(())
        })
    }
    fn local_stream_error<'a>(
        &'a mut self,
        _: u64,
        _: ActorError,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.errors.fetch_add(1, Ordering::SeqCst);
            if self.stage == 3 {
                self.inner.probe.hang().await?;
            }
            Ok(())
        })
    }
    fn dispatch<'a>(
        &'a mut self,
        op: Op,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<usize>> {
        Box::pin(self.inner.probe.execute(op))
    }
}
async fn local_stream_timeout(factory: bool, stage: u8, bad_drop: bool) {
    let system = System::new(true).await;
    let System::Actix(engine) = &system else {
        unreachable!()
    };
    let plan = Arc::new(Plan {
        bad_drop,
        ..Default::default()
    });
    let errors = Arc::new(AtomicUsize::new(0));
    let source = plan.clone();
    let seen_errors = errors.clone();
    let make = move || StreamLocal {
        inner: Local {
            probe: Probe::new(source.clone()),
            _owner: Rc::new(()),
        },
        stage,
        errors: seen_errors.clone(),
    };
    let actor = if factory {
        engine
            .spawn_local_typed_factory::<StreamLocal, Op, _>(make, "/stream-timeout")
            .await
    } else {
        engine
            .spawn_local_typed::<StreamLocal, Op, _>(make, "/stream-timeout")
            .await
    };
    until(|| plan.actors.load(Ordering::SeqCst) == 1).await;
    if let Ok(actor) = actor {
        assert!(
            actor.ask(Op::Read).await.is_err(),
            "timed-out local stream remained active"
        );
    }
    system.shutdown(true).await;
    assert_eq!(
        plan.builds.load(Ordering::SeqCst),
        1,
        "failed stream was reconstructed"
    );
    assert_eq!(plan.entered.load(Ordering::SeqCst), 1);
    assert_eq!(plan.cancelled.load(Ordering::SeqCst), 1);
    let expected_errors = usize::from(stage != 4 && !bad_drop);
    assert_eq!(
        errors.load(Ordering::SeqCst),
        expected_errors,
        "callback ran after failed cancellation"
    );
}
macro_rules! local_stream_cases {
    ($module:ident, $factory:expr) => {
        mod $module {
            use super::*;
            #[actix::test]
            async fn started() {
                local_stream_timeout($factory, 0, false).await;
            }
            #[actix::test]
            async fn item() {
                local_stream_timeout($factory, 1, false).await;
            }
            #[actix::test]
            async fn finished() {
                local_stream_timeout($factory, 2, false).await;
            }
            #[actix::test]
            async fn error() {
                local_stream_timeout($factory, 3, false).await;
            }
            #[actix::test]
            async fn initialize_streams() {
                local_stream_timeout($factory, 4, false).await;
            }
            #[actix::test]
            async fn failed_cancellation_skips_error_callback() {
                local_stream_timeout($factory, 0, true).await;
            }
        }
    };
}
local_stream_cases!(local_stream_instance, false);
local_stream_cases!(local_stream_factory, true);
