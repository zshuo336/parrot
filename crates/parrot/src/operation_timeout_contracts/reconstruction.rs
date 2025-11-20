use super::*;
use crate::thread::{SchedulingMode, ThreadContext};
use parrot_api::{
    actor::{ActixEngineExt, EmptyConfig},
    local_typed::LocalTypedActorRef,
    supervisor::{BasicDecisionFn, OneForAllStrategy, OneForOneStrategy},
    typed_channel::TypedActorRef,
    types::BoxedActorRef,
};
use std::sync::{Condvar, Mutex};

#[derive(Clone, Copy)]
enum Kind {
    Shared,
    Dedicated,
    Sharded,
    DynamicActix,
    TypedThread,
    TypedActix,
    Local,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Late,
    SharedBudget,
    OldRetirement,
    FailedInit,
    Stop,
    RecoveryBound,
    BadLate,
    BadStop,
    BadShutdown,
}
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    entered: AtomicUsize,
}
impl Gate {
    fn wait(&self) {
        self.entered.store(1, Ordering::SeqCst);
        let open = self.open.lock().unwrap();
        let (open, wait) = self
            .changed
            .wait_timeout_while(open, WAIT, |open| !*open)
            .unwrap();
        assert!(
            *open && !wait.timed_out(),
            "constructor gate was not released"
        );
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Plan {
    case: Case,
    builds: AtomicUsize,
    inits: [AtomicUsize; 2],
    drops: AtomicUsize,
    handled: [AtomicUsize; 2],
    gate: Arc<Gate>,
    sibling: bool,
}
impl Plan {
    fn new(case: Case, sibling: bool) -> Arc<Self> {
        Arc::new(Self {
            case,
            builds: AtomicUsize::new(0),
            inits: Default::default(),
            drops: AtomicUsize::new(0),
            handled: Default::default(),
            gate: Arc::default(),
            sibling,
        })
    }
    fn make(self: &Arc<Self>) -> Probe {
        let generation = self.builds.fetch_add(1, Ordering::SeqCst);
        assert!(
            generation < 2,
            "a failed construction retried without a new failure decision"
        );
        let actor = Probe {
            plan: self.clone(),
            generation,
        };
        if generation == 1 && !self.sibling {
            match self.case {
                Case::Late
                | Case::Stop
                | Case::RecoveryBound
                | Case::BadLate
                | Case::BadStop
                | Case::BadShutdown => self.gate.wait(),
                Case::SharedBudget | Case::FailedInit => {
                    std::thread::sleep(Duration::from_millis(120))
                }
                Case::OldRetirement => {}
            }
        }
        actor
    }
}
struct Probe {
    plan: Arc<Plan>,
    generation: usize,
}
impl Probe {
    async fn initialize(&self) -> ActorResult<()> {
        if !self.plan.sibling {
            if self.generation == 1 && self.plan.case == Case::SharedBudget {
                tokio::time::sleep(Duration::from_millis(120)).await;
            }
            if self.generation == 0 && self.plan.case == Case::FailedInit {
                tokio::time::sleep(Duration::from_millis(120)).await;
                return Err(ActorError::InitializationError("first init failed".into()));
            }
        }
        self.plan.inits[self.generation].fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn execute(&self, op: Request) -> ActorResult<usize> {
        match op {
            Request::Read => {
                self.plan.handled[self.generation].fetch_add(1, Ordering::SeqCst);
                Ok(self.generation)
            }
            Request::Fail => Err(ActorError::MessageHandlingError(
                "reconstruction witness".into(),
            )),
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        if self.generation == 0 && self.plan.case == Case::OldRetirement && !self.plan.sibling {
            std::thread::sleep(Duration::from_millis(350));
        }
        self.plan.drops.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.generation != 1
                || !matches!(
                    self.plan.case,
                    Case::BadLate | Case::BadStop | Case::BadShutdown
                )
                || self.plan.sibling,
            "late constructor output disposal witness"
        );
    }
}
enum Request {
    Read,
    Fail,
}
impl Message for Request {
    type Result = usize;
}
impl ParrotMsgVariant<Probe> for Request {
    fn inject(op: Self) -> Request {
        op
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Request;
    type Reply = usize;
    fn init_with_context<'a>(
        &'a mut self,
        _: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(self.initialize())
    }
    fn dispatch(&mut self, op: Request) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async move { self.execute(op) })
    }
}
macro_rules! dynamic {
    ($name:ident, $context:ty) => {
        struct $name(Probe);
        impl Actor for $name {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn init<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(self.0.initialize())
            }
            fn receive_message<'a>(
                &'a mut self,
                op: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    self.0
                        .execute(*op.downcast::<Request>().unwrap())
                        .map(|value| Box::new(value) as BoxedMessage)
                })
            }
        }
    };
}
dynamic!(ThreadProbe, ThreadContext<Self>);
dynamic!(
    ActixProbe,
    crate::actix::ActixContext<crate::actix::ActixActor<Self>>
);
impl ActixEngineExt for ActixProbe {}
struct LocalProbe {
    inner: Probe,
    thread: std::thread::ThreadId,
    _local: Rc<()>,
}
impl Drop for LocalProbe {
    fn drop(&mut self) {
        assert_eq!(self.thread, std::thread::current().id());
    }
}
impl LocalMsgVariant<Request> for LocalProbe {
    fn inject(op: Request) -> Request {
        op
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
impl LocalTypedDispatch for LocalProbe {
    type Msg = Request;
    type Reply = usize;
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(self.inner.initialize())
    }
    fn dispatch<'a>(
        &'a mut self,
        op: Request,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<usize>> {
        Box::pin(async move { self.inner.execute(op) })
    }
}
enum Reference {
    Dynamic(BoxedActorRef),
    Typed(TypedActorRef<Probe, Request>),
    Local(LocalTypedActorRef<LocalProbe, Request>),
}
impl Clone for Reference {
    fn clone(&self) -> Self {
        match self {
            Self::Dynamic(r) => Self::Dynamic(r.clone_boxed()),
            Self::Typed(r) => Self::Typed(r.clone()),
            Self::Local(r) => Self::Local(r.clone()),
        }
    }
}
impl Reference {
    fn retirement_status(&self) -> parrot_api::address::RetirementStatus {
        match self {
            Self::Dynamic(r) => r.retirement_status(),
            Self::Typed(r) => r.retirement_status(),
            Self::Local(r) => r.retirement_status(),
        }
    }
    async fn ask(&self, op: Request) -> ActorResult<usize> {
        match self {
            Self::Dynamic(r) => r
                .send(Box::new(op))
                .await
                .map(|reply| *reply.downcast::<usize>().unwrap()),
            Self::Typed(r) => r.ask(op).await,
            Self::Local(r) => r.ask(op).await,
        }
    }
    async fn stop(&self) -> ActorResult<()> {
        match self {
            Self::Dynamic(r) => r.stop().await,
            Self::Typed(r) => r.stop().await,
            Self::Local(r) => r.stop().await,
        }
    }
}
fn policy(group: bool) -> SupervisorStrategyType {
    if group {
        SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 1,
            within: WAIT,
            decider: BasicDecisionFn::new(|_| parrot_api::supervisor::SupervisionDecision::Restart),
        })
    } else {
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 1,
            within: WAIT,
            decider: BasicDecisionFn::new(|_| parrot_api::supervisor::SupervisionDecision::Restart),
        })
    }
}
async fn system(kind: Kind, group: bool, case: Case) -> System {
    let limits = SystemTimeouts {
        actor_creation: Duration::from_millis(if case == Case::RecoveryBound {
            1000
        } else {
            200
        }),
        message_handling: Duration::ZERO,
        system_shutdown: Duration::from_millis(if case == Case::RecoveryBound {
            150
        } else {
            2500
        }),
    };
    let guardian = GuardianConfig {
        supervision_strategy: policy(group),
        max_restarts: 1,
        restart_window: WAIT,
    };
    match kind {
        Kind::Shared | Kind::Dedicated | Kind::Sharded | Kind::TypedThread => System::Thread(
            ThreadActorSystem::with_guardian_and_timeouts(
                ThreadActorSystemConfig {
                    shared_pool_size: 4,
                    shared_burst_workers_max: 0,
                    default_scheduling_mode: match kind {
                        Kind::Dedicated => SchedulingMode::DedicatedThread,
                        Kind::Sharded => SchedulingMode::Sharded {
                            affinity_key: "reconstruction".into(),
                        },
                        _ => SchedulingMode::SharedPool {
                            max_messages_per_run: 10,
                        },
                    },
                    ..Default::default()
                },
                guardian,
                limits,
            )
            .await
            .unwrap(),
        ),
        _ => System::Actix(Arc::new(
            ActixActorSystem::with_guardian_and_timeouts(
                ActixSystemSettings {
                    arbiter_count: 2,
                    ..Default::default()
                },
                guardian,
                limits,
            )
            .await
            .unwrap(),
        )),
    }
}
async fn spawn(system: &System, kind: Kind, plan: &Arc<Plan>, path: &str) -> Reference {
    let plan = plan.clone();
    match (system, kind) {
        (System::Thread(s), Kind::TypedThread) => Reference::Typed(
            s.spawn_typed_factory(move || plan.make(), path)
                .await
                .unwrap(),
        ),
        (System::Thread(s), _) => Reference::Dynamic(Box::new(
            s.spawn_supervised(
                move || ThreadProbe(plan.make()),
                path,
                crate::thread::SupervisorStrategy::Stop,
            )
            .await
            .unwrap(),
        )),
        (System::Actix(s), Kind::TypedActix) => Reference::Typed(
            s.spawn_typed_factory(move || plan.make(), path)
                .await
                .unwrap(),
        ),
        (System::Actix(s), Kind::Local) => Reference::Local(
            s.spawn_local_typed_factory(
                move || LocalProbe {
                    inner: plan.make(),
                    thread: std::thread::current().id(),
                    _local: Rc::new(()),
                },
                path,
            )
            .await
            .unwrap(),
        ),
        (System::Actix(s), _) => Reference::Dynamic(
            s.spawn_root_factory(move || ActixProbe(plan.make()))
                .await
                .unwrap(),
        ),
    }
}
async fn check(kind: Kind, group: bool, case: Case) {
    let system = system(kind, group, case).await;
    let plan = Plan::new(case, false);
    let _release = Release(plan.gate.clone());
    let reference = spawn(&system, kind, &plan, "/reconstruction").await;
    let sibling_plan = Plan::new(case, true);
    let sibling = if group {
        Some(spawn(&system, kind, &sibling_plan, "/sibling").await)
    } else {
        None
    };
    if let Some(sibling) = &sibling {
        assert_eq!(sibling.ask(Request::Read).await.unwrap(), 0);
    }
    if case != Case::FailedInit {
        assert_eq!(reference.ask(Request::Read).await.unwrap(), 0);
        assert!(reference.ask(Request::Fail).await.is_err());
    }
    let mut stop = None;
    let shutdown = system.runtime().shutdown();
    tokio::pin!(shutdown);
    if matches!(
        case,
        Case::Late
            | Case::Stop
            | Case::RecoveryBound
            | Case::BadLate
            | Case::BadStop
            | Case::BadShutdown
    ) {
        until(|| plan.gate.entered.load(Ordering::SeqCst) == 1).await;
        if matches!(case, Case::Stop | Case::BadStop) {
            let stopping = reference.clone();
            stop = Some(tokio::spawn(async move { stopping.stop().await }));
            tokio::time::sleep(Duration::from_millis(30)).await;
        } else {
            tokio::time::sleep(Duration::from_millis(350)).await;
        }
        if case == Case::BadShutdown {
            assert!(futures::poll!(&mut shutdown).is_pending());
        }
        plan.gate.release();
    }
    let expect_live = matches!(case, Case::OldRetirement | Case::FailedInit);
    if expect_live {
        // Dynamic generation references reject while the logical constructor
        // is absent. Retry only admission errors; never replay the failed op.
        until(|| plan.builds.load(Ordering::SeqCst) == 2).await;
        let response = tokio::time::timeout(WAIT, async {
            loop {
                match reference.ask(Request::Read).await {
                    Ok(value) => break value,
                    Err(_) if plan.drops.load(Ordering::SeqCst) < 2 => {
                        tokio::task::yield_now().await
                    }
                    Err(_) => break usize::MAX,
                }
            }
        })
        .await
        .unwrap();
        let shutdown = tokio::time::timeout(WAIT, &mut shutdown).await.unwrap();
        assert_eq!(
            response, 1,
            "old retirement or failed init consumed the next generation's creation budget"
        );
        shutdown.unwrap();
        assert_eq!(plan.inits[1].load(Ordering::SeqCst), 1);
    } else {
        until(|| {
            plan.drops.load(Ordering::SeqCst) == 2 || plan.inits[1].load(Ordering::SeqCst) != 0
        })
        .await;
        if let Some(stop) = stop {
            let result = stop.await.unwrap();
            if case == Case::BadStop {
                let error = result.expect_err("explicit cancellation lost output disposal failure");
                assert!(
                    error
                        .to_string()
                        .contains("late constructor output disposal witness"),
                    "{error:?}"
                );
            } else {
                result.expect("healthy explicit cancellation must retire successfully");
            }
        }
        if matches!(case, Case::BadLate | Case::BadStop) {
            // A retired member does not poison a later independent shutdown.
            // Public stop may cache an earlier deadline; actual retirement
            // must retain the output failure on the original identity.
            assert!(reference.stop().await.is_err());
            until(|| {
                matches!(
                    reference.retirement_status(),
                    parrot_api::address::RetirementStatus::Retired(_)
                )
            })
            .await;
            for _ in 0..2 {
                let parrot_api::address::RetirementStatus::Retired(result) =
                    reference.retirement_status()
                else {
                    panic!("actual retirement receipt disappeared");
                };
                let error = result.expect_err("output disposal failure was lost");
                assert!(
                    error
                        .to_string()
                        .contains("late constructor output disposal witness"),
                    "{error:?}"
                );
            }
        }
        let shutdown = tokio::time::timeout(WAIT, &mut shutdown).await.unwrap();
        if case == Case::BadShutdown {
            let error = shutdown.expect_err("late output disposal failure was lost");
            assert!(
                error
                    .to_string()
                    .contains("late constructor output disposal witness"),
                "{error:?}"
            );
        } else if matches!(case, Case::Stop | Case::BadStop | Case::BadLate) {
            shutdown.unwrap();
        }
        assert_eq!(
            plan.inits[1].load(Ordering::SeqCst),
            0,
            "expired reconstruction initialized its late output"
        );
        assert_eq!(plan.handled[1].load(Ordering::SeqCst), 0);
        if group && case != Case::SharedBudget {
            assert_eq!(
                sibling_plan.inits[1].load(Ordering::SeqCst),
                0,
                "failed preparation activated a sibling"
            );
        }
    }
    assert_eq!(plan.builds.load(Ordering::SeqCst), 2);
    assert_eq!(plan.drops.load(Ordering::SeqCst), 2);
    if group {
        assert_eq!(
            sibling_plan.drops.load(Ordering::SeqCst),
            sibling_plan.builds.load(Ordering::SeqCst)
        );
    }
}
macro_rules! cases {
    ($($module:ident, $kind:ident;)*) => {$ (
        mod $module {
            use super::*;
            #[actix::test] async fn independent_late() { check(Kind::$kind, false, Case::Late).await; }
            #[actix::test] async fn cohort_late() { check(Kind::$kind, true, Case::Late).await; }
            #[actix::test] async fn independent_shared_budget() { check(Kind::$kind, false, Case::SharedBudget).await; }
            #[actix::test] async fn cohort_shared_budget() { check(Kind::$kind, true, Case::SharedBudget).await; }
            #[actix::test] async fn independent_old_retirement() { check(Kind::$kind, false, Case::OldRetirement).await; }
            #[actix::test] async fn cohort_old_retirement() { check(Kind::$kind, true, Case::OldRetirement).await; }
            #[actix::test] async fn independent_failed_init() { check(Kind::$kind, false, Case::FailedInit).await; }
            #[actix::test] async fn cohort_failed_init() { check(Kind::$kind, true, Case::FailedInit).await; }
            #[actix::test] async fn independent_stop() { check(Kind::$kind, false, Case::Stop).await; }
            #[actix::test] async fn cohort_stop() { check(Kind::$kind, true, Case::Stop).await; }
            #[actix::test] async fn independent_recovery_bound() { check(Kind::$kind, false, Case::RecoveryBound).await; }
            #[actix::test] async fn cohort_recovery_bound() { check(Kind::$kind, true, Case::RecoveryBound).await; }
            #[actix::test] async fn independent_bad_late() { check(Kind::$kind, false, Case::BadLate).await; }
            #[actix::test] async fn cohort_bad_late() { check(Kind::$kind, true, Case::BadLate).await; }
            #[actix::test] async fn independent_bad_stop() { check(Kind::$kind, false, Case::BadStop).await; }
            #[actix::test] async fn cohort_bad_stop() { check(Kind::$kind, true, Case::BadStop).await; }
            #[actix::test] async fn independent_bad_shutdown() { check(Kind::$kind, false, Case::BadShutdown).await; }
            #[actix::test] async fn cohort_bad_shutdown() { check(Kind::$kind, true, Case::BadShutdown).await; }
        }
    )*};
}
cases! { shared, Shared; dedicated, Dedicated; sharded, Sharded; dynamic_actix, DynamicActix; typed_thread, TypedThread; typed_actix, TypedActix; local, Local; }
