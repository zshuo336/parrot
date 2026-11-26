//! Root retirement must outlive the host executor and public waiters.
use parrot::{
    actix::ActixActorSystem,
    root_cohort::RootCohort,
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::supervisor::{
    BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType,
};
use parrot_api::{
    Message,
    actor::{Actor, ActorState, EmptyConfig},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

#[path = "support/isolated.rs"]
mod child;

fn isolated(name: &str, body: impl FnOnce()) {
    child::run(name, "PARROT_COHORT_HOST_CHILD", WAIT * 4, body);
}

macro_rules! host_contract {
    ($name:ident, $body:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || $body);
        }
    };
}

#[derive(Clone)]
enum Engine {
    Thread(Arc<ThreadActorSystem>),
    Actix(Arc<ActixActorSystem>),
}

#[test]
#[should_panic(expected = "child did not execute its body exactly once")]
fn cohort_isolation_rejects_missing_case() {
    isolated("__missing_root_cohort_host_exit__", || unreachable!());
}
impl Engine {
    async fn new(actix: bool) -> Self {
        if actix {
            Self::Actix(Arc::new(
                ActixActorSystem::with_arbiter_count(1).await.unwrap(),
            ))
        } else {
            Self::Thread(ThreadActorSystem::shared(ThreadActorSystemConfig {
                shared_pool_size: 1,
                shared_burst_workers_max: 0,
                ..Default::default()
            }))
        }
    }
    async fn cohort(&self, strategy: SupervisorStrategyType) -> RootCohort {
        self.try_cohort(strategy).await.unwrap()
    }
    async fn try_cohort(&self, strategy: SupervisorStrategyType) -> ActorResult<RootCohort> {
        match self {
            Self::Thread(system) => system.root_cohort(strategy).await,
            Self::Actix(system) => system.root_cohort(strategy).await,
        }
    }
    async fn shutdown(&self) {
        let _ = self.shutdown_result().await;
    }
    async fn shutdown_result(&self) -> Result<(), String> {
        match self {
            Self::Thread(system) => system
                .shutdown_internal()
                .await
                .map_err(|error| error.to_string()),
            Self::Actix(system) => system
                .as_ref()
                .clone()
                .shutdown()
                .await
                .map_err(|error| error.to_string()),
        }
    }
    async fn spawn(
        &self,
        cohort: &RootCohort,
        drops: &Arc<AtomicUsize>,
        factory: bool,
    ) -> BoxedActorRef {
        let drops = drops.clone();
        match (self, factory) {
            (Self::Thread(system), false) => Box::new(
                system
                    .spawn_in_cohort(
                        ThreadProbe(Capture(drops)),
                        EmptyConfig,
                        Default::default(),
                        "/member".into(),
                        cohort,
                    )
                    .await
                    .unwrap(),
            ),
            (Self::Thread(system), true) => Box::new(
                system
                    .spawn_supervised_in_cohort(
                        move || ThreadProbe(Capture(drops.clone())),
                        "/member",
                        Default::default(),
                        cohort,
                    )
                    .await
                    .unwrap(),
            ),
            (Self::Actix(system), false) => system
                .spawn_root_in_cohort(ActixProbe(Capture(drops)), EmptyConfig, "/member", cohort)
                .await
                .unwrap(),
            (Self::Actix(system), true) => system
                .spawn_root_factory_in_cohort(
                    move || ActixProbe(Capture(drops.clone())),
                    "/member",
                    cohort,
                )
                .await
                .unwrap(),
        }
    }
}

macro_rules! probe {
    ($name:ident, $context:ty) => {
        struct $name(Capture);
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
                Box::pin(async { Ok(()) })
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                let _ = &self.0;
                Box::pin(async { Ok(message) })
            }
        }
    };
}
probe!(ThreadProbe, parrot::thread::ThreadContext<Self>);
probe!(
    ActixProbe,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
impl parrot_api::actor::ActixEngineExt for ActixProbe {}

struct Capture(Arc<AtomicUsize>);
impl Capture {
    fn decide(&self) -> SupervisionDecision {
        SupervisionDecision::Stop
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn strategy(drops: &Arc<AtomicUsize>) -> SupervisorStrategyType {
    let capture = Capture(drops.clone());
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| capture.decide()),
    })
}

fn empty_cohort_after_host_exit(actix: bool) {
    let host = actix::System::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let (system, cohort) = host.block_on(async {
        let system = Engine::new(actix).await;
        let cohort = system.cohort(strategy(&drops)).await;
        (system, cohort)
    });
    drop(host);
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let stopped = observer.block_on(async { tokio::time::timeout(WAIT, cohort.stop()).await });
    let drops_at_receipt = drops.load(Ordering::SeqCst);
    let repeated = observer.block_on(async { tokio::time::timeout(WAIT, cohort.stop()).await });
    // Observe before releasing surviving handles, then retire the old candidate.
    observer.block_on(system.shutdown());
    drop(cohort);
    drop(system);
    assert!(
        stopped.is_ok_and(|result| result.is_ok()),
        "empty cohort lost retirement after host exit"
    );
    assert_eq!(
        drops_at_receipt, 1,
        "cohort receipt preceded actual policy capture disposal"
    );
    assert!(
        repeated.is_ok_and(|result| result.is_ok()),
        "repeated stop lost the terminal result"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn thread_empty_cohort_retires_policy_after_host_exit() {
    empty_cohort_after_host_exit(false);
}
#[test]
fn actix_empty_cohort_retires_policy_after_host_exit() {
    empty_cohort_after_host_exit(true);
}

fn member_after_host_exit(actix: bool, factory: bool) {
    let host = actix::System::new();
    let actors = Arc::new(AtomicUsize::new(0));
    let policies = Arc::new(AtomicUsize::new(0));
    let (system, cohort, reference) = host.block_on(async {
        let system = Engine::new(actix).await;
        let cohort = system.cohort(strategy(&policies)).await;
        let reference = system.spawn(&cohort, &actors, factory).await;
        let reply = reference.send(Box::new(37usize)).await.unwrap();
        assert_eq!(*reply.downcast::<usize>().unwrap(), 37);
        (system, cohort, reference)
    });
    assert_eq!(actors.load(Ordering::SeqCst), 0);
    drop(host);
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let stopped = observer.block_on(async { tokio::time::timeout(WAIT, cohort.stop()).await });
    let actors_at_receipt = actors.load(Ordering::SeqCst);
    let policies_at_receipt = policies.load(Ordering::SeqCst);
    let members_at_receipt = cohort.members().len();
    let repeated = observer.block_on(async { tokio::time::timeout(WAIT, cohort.stop()).await });
    observer.block_on(async {
        let _ = tokio::time::timeout(WAIT, system.shutdown()).await;
    });
    drop(reference);
    drop(cohort);
    drop(system);
    assert!(
        stopped.is_ok_and(|result| result.is_ok()),
        "live cohort member lost retirement after host exit"
    );
    assert_eq!(
        actors_at_receipt, 1,
        "receipt preceded actual actor disposal"
    );
    assert_eq!(
        policies_at_receipt, 1,
        "receipt preceded actual policy disposal"
    );
    assert_eq!(members_at_receipt, 0);
    assert!(
        repeated.is_ok_and(|result| result.is_ok()),
        "repeated member stop lost terminal receipt"
    );
}

host_contract!(
    thread_instance_retires_after_host_exit,
    member_after_host_exit(false, false)
);
host_contract!(
    thread_factory_retires_after_host_exit,
    member_after_host_exit(false, true)
);
host_contract!(
    actix_instance_retires_after_host_exit,
    member_after_host_exit(true, false)
);
host_contract!(
    actix_factory_retires_after_host_exit,
    member_after_host_exit(true, true)
);

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
    drops: AtomicUsize,
}
impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    async fn entered(&self) {
        tokio::time::timeout(WAIT, self.entered.notified())
            .await
            .unwrap();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct BlockedPolicy {
    gate: Arc<Gate>,
    panic: bool,
}
impl BlockedPolicy {
    fn decide(&self) -> SupervisionDecision {
        SupervisionDecision::Stop
    }
}
impl Drop for BlockedPolicy {
    fn drop(&mut self) {
        self.gate.entered.notify_one();
        let (open, timeout) = self
            .gate
            .changed
            .wait_timeout_while(self.gate.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(
            *open || !timeout.timed_out(),
            "blocked policy was not released"
        );
        drop(open);
        self.gate.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "deliberate host-exit policy disposal failure");
    }
}
fn blocked_strategy(gate: &Arc<Gate>, panic: bool) -> SupervisorStrategyType {
    let policy = BlockedPolicy {
        gate: gate.clone(),
        panic,
    };
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| policy.decide()),
    })
}
fn observer() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn cancelled_waiter(actix: bool, panic: bool, host_exit: bool) {
    let host = actix::System::new();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (system, cohort) = host.block_on(async {
        let system = Engine::new(actix).await;
        let cohort = system.cohort(blocked_strategy(&gate, panic)).await;
        (system, cohort)
    });
    let mut host = Some(host);
    if host_exit {
        drop(host.take());
    }
    // Start and discard the public waiter without polling it or the host again.
    drop(cohort.stop());
    let observer = observer();
    observer.block_on(gate.entered());
    let (early, result, repeated) = observer.block_on(async {
        let mut waiting = cohort.stop();
        let early = futures::poll!(&mut waiting).is_ready();
        assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
        gate.release();
        let result = tokio::time::timeout(WAIT, waiting).await.unwrap();
        let repeated = tokio::time::timeout(WAIT, cohort.stop()).await.unwrap();
        (early, result, repeated)
    });
    if let Some(host) = host {
        host.block_on(system.shutdown());
        drop(host);
    } else {
        observer.block_on(system.shutdown());
    }
    assert!(!early, "root receipt preceded blocked policy disposal");
    assert_eq!(gate.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        result.as_ref().err().map(ToString::to_string),
        repeated.as_ref().err().map(ToString::to_string)
    );
    if panic {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("deliberate host-exit policy disposal failure")
        );
    } else {
        result.unwrap();
    }
}

macro_rules! waiter_tests {
    ($success:ident, $panic:ident, $idle:ident, $actix:expr) => {
        #[test]
        fn $success() {
            cancelled_waiter($actix, false, true);
        }
        #[test]
        fn $panic() {
            cancelled_waiter($actix, true, true);
        }
        #[test]
        fn $idle() {
            cancelled_waiter($actix, false, false);
        }
    };
}
waiter_tests!(
    thread_cancelled_waiter_preserves_disposal,
    thread_policy_panic_is_stable,
    thread_idle_host_does_not_block_disposal,
    false
);
waiter_tests!(
    actix_cancelled_waiter_preserves_disposal,
    actix_policy_panic_is_stable,
    actix_idle_host_does_not_block_disposal,
    true
);

fn last_system_drop(actix: bool) {
    let host = actix::System::new();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (system, cohort) = host.block_on(async {
        let system = Engine::new(actix).await;
        let cohort = system.cohort(blocked_strategy(&gate, false)).await;
        (system, cohort)
    });
    drop(host);
    drop(system);
    let observer = observer();
    observer.block_on(gate.entered());
    assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
    gate.release();
    observer.block_on(async {
        tokio::time::timeout(WAIT, cohort.stop())
            .await
            .unwrap()
            .unwrap();
    });
    assert_eq!(gate.drops.load(Ordering::SeqCst), 1);
}
#[test]
fn thread_last_system_drop_retires_externally_retained_cohort() {
    last_system_drop(false);
}
#[test]
fn actix_last_system_drop_retires_externally_retained_cohort() {
    last_system_drop(true);
}

fn rejected_create(actix: bool, panic: bool) {
    let host = actix::System::new();
    let system = host.block_on(async {
        let system = Engine::new(actix).await;
        system.shutdown().await;
        system
    });
    drop(host);
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let observer = observer();
    let result = observer.block_on(async {
        let rejected = system.try_cohort(blocked_strategy(&gate, panic));
        tokio::pin!(rejected);
        assert!(futures::poll!(&mut rejected).is_pending());
        gate.entered().await;
        assert!(futures::poll!(&mut rejected).is_pending());
        assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
        gate.release();
        tokio::time::timeout(WAIT, rejected).await.unwrap()
    });
    assert_eq!(gate.drops.load(Ordering::SeqCst), 1);
    let error = result.err().expect("closed system admitted root cohort");
    if panic {
        assert!(
            error
                .to_string()
                .contains("deliberate host-exit policy disposal failure")
        );
    } else {
        assert!(matches!(error, parrot_api::errors::ActorError::Stopped));
    }
}
#[test]
fn thread_rejected_create_retires_after_host_exit() {
    rejected_create(false, false);
}
#[test]
fn actix_rejected_create_retires_after_host_exit() {
    rejected_create(true, false);
}
#[test]
fn thread_rejected_create_preserves_disposal_failure() {
    rejected_create(false, true);
}
#[test]
fn actix_rejected_create_preserves_disposal_failure() {
    rejected_create(true, true);
}

struct TypedProbe(Capture);
struct Ping(usize);
impl Message for Ping {
    type Result = usize;
}
impl ParrotMsgVariant<TypedProbe> for Ping {
    fn inject(value: Self) -> Self {
        value
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
impl ParrotTypedDispatch for TypedProbe {
    type Msg = Ping;
    type Reply = usize;
    fn dispatch(&mut self, ping: Ping) -> BoxedFuture<'_, ActorResult<usize>> {
        let _ = &self.0;
        Box::pin(async move { Ok(ping.0) })
    }
}
fn typed_after_host_exit(actix: bool, factory: bool) {
    let host = actix::System::new();
    let actors = Arc::new(AtomicUsize::new(0));
    let policies = Arc::new(AtomicUsize::new(0));
    let (system, cohort, reference) = host.block_on(async {
        let system = Engine::new(actix).await;
        let cohort = system.cohort(strategy(&policies)).await;
        let moved = actors.clone();
        let reference = match (&system, factory) {
            (Engine::Thread(system), false) => system
                .spawn_typed_in_cohort(TypedProbe(Capture(moved)), "/typed", &cohort)
                .await
                .unwrap(),
            (Engine::Thread(system), true) => system
                .spawn_typed_factory_in_cohort(
                    move || TypedProbe(Capture(moved.clone())),
                    "/typed",
                    &cohort,
                )
                .await
                .unwrap(),
            (Engine::Actix(system), false) => system
                .spawn_typed_in_cohort(TypedProbe(Capture(moved)), "/typed", &cohort)
                .await
                .unwrap(),
            (Engine::Actix(system), true) => system
                .spawn_typed_factory_in_cohort(
                    move || TypedProbe(Capture(moved.clone())),
                    "/typed",
                    &cohort,
                )
                .await
                .unwrap(),
        };
        assert_eq!(reference.ask(Ping(53)).await.unwrap(), 53);
        (system, cohort, reference)
    });
    assert_eq!(actors.load(Ordering::SeqCst), 0);
    drop(host);
    let observer = observer();
    let stopped = observer.block_on(async { tokio::time::timeout(WAIT, cohort.stop()).await });
    let actors_at_receipt = actors.load(Ordering::SeqCst);
    let policies_at_receipt = policies.load(Ordering::SeqCst);
    let members_at_receipt = cohort.members().len();
    observer.block_on(async {
        let _ = tokio::time::timeout(WAIT, system.shutdown()).await;
    });
    drop(reference);
    drop(cohort);
    drop(system);
    assert!(
        stopped.is_ok_and(|result| result.is_ok()),
        "typed member lost retirement after host exit"
    );
    assert_eq!(actors_at_receipt, 1);
    assert_eq!(policies_at_receipt, 1);
    assert_eq!(members_at_receipt, 0);
}
host_contract!(
    thread_typed_instance_retires_after_host_exit,
    typed_after_host_exit(false, false)
);
host_contract!(
    thread_typed_factory_retires_after_host_exit,
    typed_after_host_exit(false, true)
);
host_contract!(
    actix_typed_instance_retires_after_host_exit,
    typed_after_host_exit(true, false)
);
host_contract!(
    actix_typed_factory_retires_after_host_exit,
    typed_after_host_exit(true, true)
);

fn pending_factory_after_host_exit(actix: bool) {
    let host = actix::System::new();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let actors = Arc::new(AtomicUsize::new(0));
    let policies = Arc::new(AtomicUsize::new(0));
    let (system, cohort) = host.block_on(async {
        let system = Engine::new(actix).await;
        let cohort = system.cohort(strategy(&policies)).await;
        let moved = gate.clone();
        let drops = actors.clone();
        let construct = move || {
            moved.entered.notify_one();
            let (open, timeout) = moved
                .changed
                .wait_timeout_while(moved.open.lock().unwrap(), WAIT, |open| !*open)
                .unwrap();
            assert!(
                *open || !timeout.timed_out(),
                "pending constructor was not released"
            );
            Capture(drops.clone())
        };
        match &system {
            Engine::Thread(system) => {
                let mut creation = Box::pin(system.spawn_supervised_in_cohort(
                    move || ThreadProbe(construct()),
                    "/pending",
                    Default::default(),
                    &cohort,
                ));
                assert!(futures::poll!(&mut creation).is_pending());
                tokio::select! {
                    _ = &mut creation => panic!("pending constructor unexpectedly completed"),
                    _ = gate.entered() => {},
                }
                drop(creation);
            }
            Engine::Actix(system) => {
                let mut creation = Box::pin(system.spawn_root_factory_in_cohort(
                    move || ActixProbe(construct()),
                    "/pending",
                    &cohort,
                ));
                assert!(futures::poll!(&mut creation).is_pending());
                tokio::select! {
                    _ = &mut creation => panic!("pending constructor unexpectedly completed"),
                    _ = gate.entered() => {},
                }
                drop(creation);
            }
        }
        (system, cohort)
    });
    drop(host);
    let observer = observer();
    observer.block_on(async {
        let mut stop = cohort.stop();
        assert!(
            futures::poll!(&mut stop).is_pending(),
            "pending constructor was not retained"
        );
        assert_eq!(actors.load(Ordering::SeqCst), 0);
        assert_eq!(policies.load(Ordering::SeqCst), 0);
        gate.release();
        tokio::time::timeout(WAIT, stop).await.unwrap().unwrap();
    });
    assert_eq!(actors.load(Ordering::SeqCst), 1);
    assert_eq!(policies.load(Ordering::SeqCst), 1);
    assert_eq!(cohort.members().len(), 0);
    observer.block_on(system.shutdown());
}
host_contract!(
    thread_pending_factory_retires_after_host_exit,
    pending_factory_after_host_exit(false)
);
host_contract!(
    actix_pending_factory_retires_after_host_exit,
    pending_factory_after_host_exit(true)
);

fn guardian_shutdown(actix: bool, panic: bool) {
    let host = actix::System::new();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let actors = Arc::new(AtomicUsize::new(0));
    let (system, reference) = host.block_on(async {
        let guardian = parrot_api::system::GuardianConfig {
            supervision_strategy: blocked_strategy(&gate, panic),
            ..Default::default()
        };
        let system = if actix {
            Engine::Actix(Arc::new(
                ActixActorSystem::with_guardian(
                    parrot::actix::ActixSystemSettings {
                        arbiter_count: 1,
                        shutdown_timeout: WAIT,
                        ..Default::default()
                    },
                    guardian,
                )
                .await
                .unwrap(),
            ))
        } else {
            Engine::Thread(
                ThreadActorSystem::with_guardian(
                    ThreadActorSystemConfig {
                        shared_pool_size: 1,
                        shared_burst_workers_max: 0,
                        shutdown_timeout: WAIT,
                        ..Default::default()
                    },
                    guardian,
                )
                .await
                .unwrap(),
            )
        };
        let reference: BoxedActorRef = match &system {
            Engine::Thread(system) => Box::new(
                system
                    .spawn_root_typed_thread(ThreadProbe(Capture(actors.clone())), EmptyConfig)
                    .await
                    .unwrap(),
            ),
            Engine::Actix(system) => system
                .spawn_root_typed(ActixProbe(Capture(actors.clone())), EmptyConfig)
                .await
                .unwrap(),
        };
        reference.send(Box::new(71usize)).await.unwrap();
        (system, reference)
    });
    drop(host);
    let (finished, receive_finished) = std::sync::mpsc::channel();
    let moved = system.clone();
    let shutdown = std::thread::spawn(move || {
        let observer = observer();
        let first = observer.block_on(moved.shutdown_result());
        let repeated = observer.block_on(moved.shutdown_result());
        finished.send((first, repeated)).unwrap();
    });
    observer().block_on(gate.entered());
    let early = receive_finished.try_recv();
    assert!(
        matches!(early, Err(std::sync::mpsc::TryRecvError::Empty)),
        "system shutdown preceded blocked Guardian disposal"
    );
    assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
    gate.release();
    let (first, repeated) = receive_finished
        .recv_timeout(WAIT)
        .expect("system did not join Guardian retirement");
    shutdown.join().unwrap();
    assert_eq!(first, repeated);
    assert_eq!(gate.drops.load(Ordering::SeqCst), 1);
    assert_eq!(actors.load(Ordering::SeqCst), 1);
    match &system {
        Engine::Thread(system) => {
            assert_eq!(system.actor_count(), 0);
            assert_eq!(
                system.scheduler_metrics().unwrap().status,
                parrot::thread::scheduler::shared::SchedulerStatus::Shutdown
            );
        }
        Engine::Actix(system) => {
            assert_eq!(
                system.status().state,
                parrot_api::system::SystemState::Stopped
            );
            assert_eq!(system.status().active_actors, 0);
            assert_eq!(system.status().resources.thread_count, 0);
        }
    }
    if panic {
        assert!(
            first
                .unwrap_err()
                .contains("deliberate host-exit policy disposal failure")
        );
    } else if actix {
        first.expect("independent Actix shutdown survives original host exit");
    } else {
        assert!(
            first
                .unwrap_err()
                .contains("shared task unexpectedly cancelled")
        );
    }
    drop(reference);
}
host_contract!(
    thread_guardian_shutdown_joins_after_host_exit,
    guardian_shutdown(false, false)
);
host_contract!(
    thread_guardian_shutdown_preserves_policy_failure,
    guardian_shutdown(false, true)
);
host_contract!(
    actix_guardian_shutdown_joins_after_host_exit,
    guardian_shutdown(true, false)
);
host_contract!(
    actix_guardian_shutdown_preserves_policy_failure,
    guardian_shutdown(true, true)
);

struct LocalProbe {
    _owner: std::rc::Rc<()>,
    capture: Capture,
}
impl parrot_api::local_typed::LocalTypedDispatch for LocalProbe {
    type Msg = Ping;
    type Reply = usize;
    fn dispatch<'a>(
        &'a mut self,
        ping: Ping,
        _: &'a parrot_api::typed_context::TypedContext,
    ) -> parrot_api::local_typed::LocalBoxedFuture<'a, ActorResult<usize>> {
        let _ = &self.capture;
        Box::pin(async move { Ok(ping.0) })
    }
}
impl ParrotMsgVariant<parrot_api::local_typed::LocalProtocol<LocalProbe>> for Ping {
    fn inject(value: Self) -> Self {
        value
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}

fn ungrouped_after_host_exit(actix: bool, kind: u8, late_group: bool) {
    let host = actix::System::new();
    let actors = Arc::new(AtomicUsize::new(0));
    let policies = Arc::new(AtomicUsize::new(0));
    let (system, reference, stop, cohort) = host.block_on(async {
        let system = Engine::new(actix).await;
        let (reference, stop): (
            Box<dyn std::any::Any>,
            BoxedFuture<'static, ActorResult<()>>,
        ) = if kind == 1 {
            let actor = TypedProbe(Capture(actors.clone()));
            let reference = match &system {
                Engine::Thread(system) => system.spawn_typed::<_, Ping>(actor, "/ungrouped").await,
                Engine::Actix(system) => system.spawn_typed::<_, Ping>(actor, "/ungrouped").await,
            }
            .unwrap();
            assert_eq!(reference.ask(Ping(89)).await.unwrap(), 89);
            let moved = reference.clone();
            (
                Box::new(reference),
                Box::pin(async move { moved.stop().await }),
            )
        } else if kind == 2 {
            let Engine::Actix(system) = &system else {
                unreachable!()
            };
            let moved = actors.clone();
            let reference = system
                .spawn_local_typed::<LocalProbe, Ping, _>(
                    move || LocalProbe {
                        _owner: std::rc::Rc::new(()),
                        capture: Capture(moved.clone()),
                    },
                    "/ungrouped",
                )
                .await
                .unwrap();
            assert_eq!(reference.ask(Ping(89)).await.unwrap(), 89);
            let moved = reference.clone();
            (
                Box::new(reference),
                Box::pin(async move { moved.stop().await }),
            )
        } else {
            let reference: BoxedActorRef = match &system {
                Engine::Thread(system) => Box::new(
                    system
                        .spawn_root_typed_thread(ThreadProbe(Capture(actors.clone())), EmptyConfig)
                        .await
                        .unwrap(),
                ),
                Engine::Actix(system) => system
                    .spawn_root_typed(ActixProbe(Capture(actors.clone())), EmptyConfig)
                    .await
                    .unwrap(),
            };
            let reply = reference.send(Box::new(89usize)).await.unwrap();
            assert_eq!(*reply.downcast::<usize>().unwrap(), 89);
            let moved = reference.clone_boxed();
            (
                Box::new(reference),
                Box::pin(async move { moved.stop().await }),
            )
        };
        // This group is created only after the earlier monitor is live.
        let cohort = if late_group {
            Some(system.cohort(strategy(&policies)).await)
        } else {
            None
        };
        (system, reference, stop, cohort)
    });
    assert_eq!(actors.load(Ordering::SeqCst), 0);
    drop(host);
    let observer = observer();
    // Per-actor cleanup must finish while its system and unrelated group live.
    // A later whole-system cancellation cannot substitute for its monitor.
    let stop_result = observer.block_on(async {
        tokio::time::timeout(WAIT, stop)
            .await
            .expect("ungrouped actor stop lost its lifecycle monitor")
    });
    if !actix && kind == 1 {
        assert!(
            stop_result
                .unwrap_err()
                .to_string()
                .contains("execution failed")
        );
    } else {
        stop_result.unwrap();
    }
    assert_eq!(
        actors.load(Ordering::SeqCst),
        1,
        "actor retained by dead host"
    );
    match &system {
        Engine::Thread(system) => assert_eq!(system.actor_count(), 0),
        Engine::Actix(system) => assert_eq!(system.status().active_actors, 0),
    }
    let first = observer.block_on(async {
        tokio::time::timeout(WAIT, system.shutdown_result())
            .await
            .expect("ungrouped shutdown lost its retirement owner")
    });
    let repeated = observer.block_on(async {
        tokio::time::timeout(WAIT, system.shutdown_result())
            .await
            .expect("repeated ungrouped shutdown lost its receipt")
    });
    assert_eq!(first, repeated);
    assert_eq!(
        actors.load(Ordering::SeqCst),
        1,
        "actor retained by dead host"
    );
    assert_eq!(policies.load(Ordering::SeqCst), usize::from(late_group));
    match &system {
        Engine::Thread(system) => {
            assert_eq!(system.actor_count(), 0);
            assert_eq!(
                system.scheduler_metrics().unwrap().status,
                parrot::thread::scheduler::shared::SchedulerStatus::Shutdown
            );
        }
        Engine::Actix(system) => {
            assert_eq!(
                system.status().state,
                parrot_api::system::SystemState::Stopped
            );
            assert_eq!(system.status().active_actors, 0);
            assert_eq!(system.status().resources.thread_count, 0);
        }
    }
    drop(cohort);
    drop(reference);
}

macro_rules! ungrouped_contract {
    ($case:ident, $actix:expr, $kind:expr, $late:expr) => {
        host_contract!($case, ungrouped_after_host_exit($actix, $kind, $late));
    };
}
ungrouped_contract!(thread_ungrouped_instance_host_exit, false, 0, false);
ungrouped_contract!(thread_ungrouped_typed_host_exit, false, 1, false);
ungrouped_contract!(actix_ungrouped_instance_host_exit, true, 0, false);
ungrouped_contract!(actix_ungrouped_typed_host_exit, true, 1, false);
ungrouped_contract!(actix_ungrouped_local_host_exit, true, 2, false);
ungrouped_contract!(thread_late_group_instance_host_exit, false, 0, true);
ungrouped_contract!(thread_late_group_typed_host_exit, false, 1, true);
ungrouped_contract!(actix_late_group_instance_host_exit, true, 0, true);
ungrouped_contract!(actix_late_group_typed_host_exit, true, 1, true);
ungrouped_contract!(actix_late_group_local_host_exit, true, 2, true);
