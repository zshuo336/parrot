//! Static protocol failures must retain typed replies and execute supervision.
use parrot::{actix::ActixActorSystem, thread::ThreadActorSystem};
use parrot_api::{
    errors::ActorError,
    message::Message,
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::TypedActorRef,
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy)]
enum Op {
    Read,
    Error,
    Panic,
    ConstructorPanic,
    DomainRejection,
}
impl Message for Op {
    type Result = Result<usize, &'static str>;
}
struct Probe {
    value: usize,
    hooks: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    drop_gate: Option<Arc<Gate>>,
    panic_drop: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(gate) = &self.drop_gate {
            gate.block();
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "typed destructor failure");
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Op;
    type Reply = Result<usize, &'static str>;
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.hooks.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&mut self, op: Op) -> BoxedFuture<'_, ActorResult<Self::Reply>> {
        if matches!(op, Op::ConstructorPanic) {
            self.value += 1;
            panic!("typed constructor panic");
        }
        Box::pin(async move {
            match op {
                Op::ConstructorPanic => unreachable!(),
                Op::Read => Ok(Ok(self.value)),
                Op::DomainRejection => Ok(Err("domain rejection")),
                Op::Error => {
                    self.value += 1;
                    Err(ActorError::MessageHandlingError(
                        "typed business failure".into(),
                    ))
                }
                Op::Panic => {
                    self.value += 1;
                    panic!("typed handler panic")
                }
            }
        })
    }
}
impl ParrotMsgVariant<Probe> for Op {
    fn inject(msg: Self) -> Op {
        msg
    }
    fn extract(reply: Result<usize, &'static str>) -> ActorResult<Self::Result> {
        Ok(reply)
    }
}
enum Engine {
    Thread(Arc<ThreadActorSystem>),
    Actix(Arc<ActixActorSystem>),
}
impl Engine {
    async fn new(actix: bool) -> Self {
        if actix {
            Self::Actix(Arc::new(
                ActixActorSystem::with_arbiter_count(1).await.unwrap(),
            ))
        } else {
            Self::Thread(ThreadActorSystem::shared(Default::default()))
        }
    }
    async fn spawn(&self, actor: Probe, path: &str) -> TypedActorRef<Probe, Op> {
        match self {
            Self::Thread(s) => s.spawn_typed(actor, path).await.unwrap(),
            Self::Actix(s) => s.spawn_typed(actor, path).await.unwrap(),
        }
    }
    async fn timed(actix: bool) -> Self {
        let timeout = Duration::from_millis(80);
        if actix {
            Self::Actix(Arc::new(
                ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
                    arbiter_count: 1,
                    shutdown_timeout: timeout,
                    ..Default::default()
                })
                .await
                .unwrap(),
            ))
        } else {
            Self::Thread(ThreadActorSystem::shared(
                parrot::thread::ThreadActorSystemConfig {
                    shared_pool_size: 1,
                    shutdown_timeout: timeout,
                    ..Default::default()
                },
            ))
        }
    }
    async fn exists(&self, path: &str) -> bool {
        match self {
            Self::Thread(s) => s.get_actor_ref(path).is_some(),
            Self::Actix(s) => s.get_actor(&path.to_owned()).await.is_some(),
        }
    }
    async fn try_spawn_strategy(
        &self,
        actor: Probe,
        path: &str,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> ActorResult<TypedActorRef<Probe, Op>> {
        match self {
            Self::Thread(s) => s.spawn_typed_with_strategy(actor, path, strategy).await,
            Self::Actix(s) => s.spawn_typed_with_strategy(actor, path, strategy).await,
        }
    }
    async fn absent(&self, path: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let exists = match self {
                    Self::Thread(s) => s.get_actor_ref(path).is_some(),
                    Self::Actix(s) => s.get_actor(&path.to_owned()).await.is_some(),
                };
                if !exists {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    async fn spawn_strategy(
        &self,
        actor: Probe,
        path: &str,
        strategy: parrot_api::supervisor::SupervisorStrategyType,
    ) -> TypedActorRef<Probe, Op> {
        match self {
            Self::Thread(s) => s
                .spawn_typed_with_strategy(actor, path, strategy)
                .await
                .unwrap(),
            Self::Actix(s) => s
                .spawn_typed_with_strategy(actor, path, strategy)
                .await
                .unwrap(),
        }
    }
    async fn shutdown(self) {
        match self {
            Self::Thread(s) => {
                s.shutdown_internal().await.unwrap();
            }
            Self::Actix(s) => {
                s.as_ref().clone().shutdown().await.unwrap();
            }
        }
    }
}
fn probe() -> (Probe, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let hooks = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    (
        Probe {
            value: 0,
            hooks: hooks.clone(),
            drops: drops.clone(),
            drop_gate: None,
            panic_drop: false,
        },
        hooks,
        drops,
    )
}
async fn retired(drops: &AtomicUsize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while drops.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed static instance must actually retire");
}
async fn default_stop(actix: bool) {
    for tell in [false, true] {
        let system = Engine::new(actix).await;
        let (state, hooks, drops) = probe();
        let actor = system.spawn(state, "/failed").await;
        let (state, _, _) = probe();
        let sibling = system.spawn(state, "/healthy").await;
        assert_eq!(
            actor.ask(Op::DomainRejection).await.unwrap(),
            Err("domain rejection")
        );
        if tell {
            actor.tell(Op::Error).await.unwrap();
        } else {
            assert!(
                matches!(actor.ask(Op::Error).await, Err(ActorError::MessageHandlingError(s)) if s == "typed business failure")
            );
        }
        retired(&drops).await;
        system.absent("/failed").await;
        assert!(!actor.is_alive());
        assert_eq!(hooks.load(Ordering::SeqCst), 0);
        assert_eq!(sibling.ask(Op::Read).await.unwrap(), Ok(0));
        system.shutdown().await;
    }
}
async fn panic_reply(actix: bool) {
    let system = Engine::new(actix).await;
    let (state, hooks, drops) = probe();
    let actor = system.spawn(state, "/panic").await;
    let reply = actor.ask(Op::Panic).await;
    assert!(
        matches!(reply, Err(ActorError::Panic(s)) if s.contains("typed handler panic")),
        "original handler panic must reach the caller"
    );
    retired(&drops).await;
    system.absent("/panic").await;
    assert_eq!(hooks.load(Ordering::SeqCst), 0);
    system.shutdown().await;
}
#[actix::test]
async fn thread_typed_handler_errors_execute_default_stop() {
    default_stop(false).await;
}
#[actix::test]
async fn actix_typed_handler_errors_execute_default_stop() {
    default_stop(true).await;
}
#[actix::test]
async fn thread_typed_panic_reaches_its_requester() {
    panic_reply(false).await;
}
#[actix::test]
async fn actix_typed_panic_reaches_its_requester() {
    panic_reply(true).await;
}

fn strategy(
    decision: parrot_api::supervisor::SupervisionDecision,
    calls: Arc<AtomicUsize>,
    all: bool,
) -> parrot_api::supervisor::SupervisorStrategyType {
    use parrot_api::supervisor::*;
    let decider = BasicDecisionFn::new(move |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        decision
    });
    if all {
        SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 0,
            within: Duration::from_secs(60),
            decider,
        })
    } else {
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 0,
            within: Duration::from_secs(60),
            decider,
        })
    }
}
#[actix::test]
async fn resume_preserves_static_state_and_identity_without_charging_restart_budget() {
    for actix in [false, true] {
        for all in [false, true] {
            let system = Engine::new(actix).await;
            let (state, hooks, drops) = probe();
            let calls = Arc::new(AtomicUsize::new(0));
            let actor = system
                .spawn_strategy(
                    state,
                    "/resume",
                    strategy(
                        parrot_api::supervisor::SupervisionDecision::Resume,
                        calls.clone(),
                        all,
                    ),
                )
                .await;
            let (state, _, _) = probe();
            let sibling = system.spawn(state, "/unrelated").await;
            let original = actor.clone().into_dyn();
            let identity = original.death_watch().unwrap().id();
            for (index, op) in [Op::Error, Op::Panic, Op::ConstructorPanic]
                .into_iter()
                .enumerate()
            {
                let reply = actor.ask(op).await;
                if matches!(op, Op::Error) {
                    assert!(matches!(reply, Err(ActorError::MessageHandlingError(_))));
                } else {
                    assert!(matches!(reply, Err(ActorError::Panic(_))));
                }
                assert_eq!(actor.ask(Op::Read).await.unwrap(), Ok(index + 1));
            }
            actor.tell(Op::Error).await.unwrap();
            assert_eq!(actor.ask(Op::Read).await.unwrap(), Ok(4));
            assert_eq!(calls.load(Ordering::SeqCst), 4);
            assert_eq!(hooks.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert_eq!(original.death_watch().unwrap().id(), identity);
            assert_eq!(sibling.ask(Op::Read).await.unwrap(), Ok(0));
            actor.stop().await.unwrap();
            assert_eq!(hooks.load(Ordering::SeqCst), 1);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            system.shutdown().await;
        }
    }
}
#[actix::test]
async fn non_resume_decision_runs_once_and_cannot_reconstruct_an_instance() {
    use parrot_api::supervisor::SupervisionDecision::*;
    for actix in [false, true] {
        for decision in [Stop, Restart, Escalate] {
            let system = Engine::new(actix).await;
            let (state, hooks, drops) = probe();
            let calls = Arc::new(AtomicUsize::new(0));
            let actor = system
                .spawn_strategy(state, "/terminal", strategy(decision, calls.clone(), false))
                .await;
            assert!(matches!(
                actor.ask(Op::Error).await,
                Err(ActorError::MessageHandlingError(_))
            ));
            retired(&drops).await;
            system.absent("/terminal").await;
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(hooks.load(Ordering::SeqCst), 0);
            assert!(!actor.is_alive());
            system.shutdown().await;
        }
    }
}

#[derive(Default)]
struct Gate {
    open: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
    entered: tokio::sync::Notify,
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
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Capture(Arc<AtomicUsize>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn gated_strategy(
    gate: Arc<Gate>,
    calls: Arc<AtomicUsize>,
    captures: Arc<AtomicUsize>,
) -> parrot_api::supervisor::SupervisorStrategyType {
    use parrot_api::supervisor::*;
    let capture = Capture(captures);
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |_| {
            let _keep_alive = &capture;
            calls.fetch_add(1, Ordering::SeqCst);
            gate.block();
            SupervisionDecision::Resume
        }),
    })
}
#[actix::test]
async fn stop_during_blocked_decision_retains_capture_and_path_until_real_completion() {
    for actix in [false, true] {
        let system = Engine::timed(actix).await;
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let (state, hooks, drops) = probe();
        let actor = system
            .spawn_strategy(
                state,
                "/blocked",
                gated_strategy(gate.clone(), calls.clone(), captures.clone()),
            )
            .await;
        let (state, _, _) = probe();
        let neighbour = system.spawn(state, "/neighbour").await;
        actor.tell(Op::Error).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        assert_eq!(
            neighbour.ask(Op::Read).await.unwrap(),
            Ok(0),
            "decider cannot occupy the execution lane"
        );
        let stop = tokio::time::timeout(Duration::from_secs(2), actor.stop())
            .await
            .unwrap();
        assert!(
            matches!(
                stop,
                Err(ActorError::TimeoutDetail(_)) | Err(ActorError::Timeout)
            ),
            "{stop:?}"
        );
        assert!(
            system.exists("/blocked").await,
            "timeout cannot release a retained decision owner"
        );
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!actor.is_alive());
        gate.release();
        system.absent("/blocked").await;
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            hooks.load(Ordering::SeqCst),
            0,
            "cancelled failed dispatch cannot run a healthy stop hook"
        );
        assert!(actor.ask(Op::Read).await.is_err());
        system.shutdown().await;
    }
}
#[actix::test]
async fn rejected_static_actor_and_policy_disposal_survive_caller_cancellation() {
    for actix in [false, true] {
        let system = Engine::new(actix).await;
        let (state, _, _) = probe();
        let existing = system.spawn(state, "/occupied").await;
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let (mut rejected, _, drops) = probe();
        rejected.drop_gate = Some(gate.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let policy = gated_strategy(Arc::new(Gate::default()), calls.clone(), captures.clone());
        let mut pending = Box::pin(system.try_spawn_strategy(rejected, "/occupied", policy));
        assert!(futures::poll!(&mut pending).is_pending());
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        drop(pending);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(
            existing.ask(Op::Read).await.unwrap(),
            Ok(0),
            "rejection drop cannot hold registry/executor"
        );
        gate.release();
        retired(&drops).await;
        retired(&captures).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        existing.stop().await.unwrap();
        system.shutdown().await;
    }
}
async fn cleanup_failure_is_not_replaced_by_supervision_diagnostics(actix: bool) {
    use parrot_api::supervisor::*;
    for choice in 0..3 {
        let system = Engine::new(actix).await;
        let (mut state, _, drops) = probe();
        state.panic_drop = true;
        let policy = match choice {
            0 => SupervisorStrategyType::Default(DefaultStrategy::StopOnFailure),
            1 => SupervisorStrategyType::Default(DefaultStrategy::RestartOnFailure),
            _ => SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(|_| panic!("decider failure")),
            }),
        };
        let actor = system.spawn_strategy(state, "/drop-panic", policy).await;
        assert!(matches!(
            actor.ask(Op::Error).await,
            Err(ActorError::MessageHandlingError(_))
        ));
        let error = actor.stop().await.unwrap_err();
        assert!(
            error.to_string().contains("typed destructor failure"),
            "actix={actix} choice={choice}: {error}"
        );
        retired(&drops).await;
        system.absent("/drop-panic").await;
        system.shutdown().await;
    }
}
#[actix::test]
async fn thread_cleanup_failure_is_not_replaced_by_supervision_diagnostics() {
    cleanup_failure_is_not_replaced_by_supervision_diagnostics(false).await;
}
#[actix::test]
async fn actix_cleanup_failure_is_not_replaced_by_supervision_diagnostics() {
    cleanup_failure_is_not_replaced_by_supervision_diagnostics(true).await;
}

struct PanickingCapture(Arc<AtomicUsize>);
impl Drop for PanickingCapture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("typed policy destructor failure");
    }
}
async fn rejected_inputs_double_panic_is_isolated(actix: bool) {
    if std::env::var_os("PARROT_TYPED_DOUBLE_DROP_CHILD").is_some() {
        use parrot_api::supervisor::*;
        let system = Engine::new(actix).await;
        let (state, _, _) = probe();
        let existing = system.spawn(state, "/occupied").await;
        let (mut rejected, _, drops) = probe();
        rejected.panic_drop = true;
        let captures = Arc::new(AtomicUsize::new(0));
        let capture = PanickingCapture(captures.clone());
        let policy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 1,
            within: Duration::from_secs(1),
            decider: BasicDecisionFn::new(move |_| {
                let _keep_alive = &capture;
                SupervisionDecision::Stop
            }),
        });
        assert!(
            system
                .try_spawn_strategy(rejected, "/occupied", policy)
                .await
                .is_err()
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(existing.ask(Op::Read).await.unwrap(), Ok(0));
        system.shutdown().await;
        return;
    }
    let test = if actix {
        "actix_rejected_actor_and_policy_panics_do_not_abort_the_process"
    } else {
        "thread_rejected_actor_and_policy_panics_do_not_abort_the_process"
    };
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("PARROT_TYPED_DOUBLE_DROP_CHILD", "1")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "rejection must isolate each destructor, child status={:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
#[actix::test]
async fn thread_rejected_actor_and_policy_panics_do_not_abort_the_process() {
    rejected_inputs_double_panic_is_isolated(false).await;
}
#[actix::test]
async fn actix_rejected_actor_and_policy_panics_do_not_abort_the_process() {
    rejected_inputs_double_panic_is_isolated(true).await;
}

mod operation_sources {
    use super::*;
    use parrot_api::supervisor::{
        BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType,
    };
    use std::error::Error;

    #[derive(Clone, Copy)]
    enum Stage {
        Healthy,
        Init,
        Handler,
    }
    struct State {
        stage: Stage,
        hostile: bool,
        formats: AtomicUsize,
        sources: AtomicUsize,
        drops: AtomicUsize,
        hooks: AtomicUsize,
        decisions: AtomicUsize,
    }
    struct Business(Arc<State>);
    impl std::fmt::Debug for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("OperationBusiness")
        }
    }
    impl std::fmt::Display for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.formats.fetch_add(1, Ordering::SeqCst);
            assert!(!self.0.hostile, "framework invoked business Display");
            f.write_str("original operation business cause")
        }
    }
    impl Error for Business {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.sources.fetch_add(1, Ordering::SeqCst);
            panic!("framework invoked unknown business source");
        }
    }
    impl State {
        fn fail(self: &Arc<Self>) -> ActorError {
            ActorError::Other(anyhow::Error::new(Business(self.clone())))
        }
        fn policy(self: &Arc<Self>) -> OneForOneStrategy {
            let state = self.clone();
            OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(move |_| {
                    state.decisions.fetch_add(1, Ordering::SeqCst);
                    SupervisionDecision::Stop
                }),
            }
        }
    }
    struct Actor(Arc<State>);
    impl Drop for Actor {
        fn drop(&mut self) {
            self.0.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[derive(Clone, Copy)]
    enum Message {
        Ready,
        Fail,
    }
    impl parrot_api::message::Message for Message {
        type Result = ();
    }
    impl ParrotTypedDispatch for Actor {
        type Msg = Message;
        type Reply = ();
        fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async move {
                if matches!(self.0.stage, Stage::Init) {
                    Err(self.0.fail())
                } else {
                    Ok(())
                }
            })
        }
        fn dispatch(&mut self, message: Message) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async move {
                if matches!(message, Message::Fail) {
                    Err(self.0.fail())
                } else {
                    Ok(())
                }
            })
        }
        fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            self.0.hooks.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }
    impl ParrotMsgVariant<Actor> for Message {
        fn inject(message: Self) -> Self {
            message
        }
        fn extract(_: ()) -> ActorResult<()> {
            Ok(())
        }
    }
    fn original(error: &ActorError, state: &Arc<State>) -> usize {
        let mut source = Some(error as &(dyn Error + 'static));
        for _ in 0..128 {
            let Some(current) = source else { break };
            if let Some(business) = current.downcast_ref::<Business>() {
                assert!(Arc::ptr_eq(&business.0, state));
                return business as *const Business as usize;
            }
            source = current.source();
        }
        panic!("operation's concrete business source was lost");
    }
    async fn run(actix: bool, factory: bool, stage: Stage, hostile: bool) {
        let state = Arc::new(State {
            stage,
            hostile,
            formats: AtomicUsize::new(0),
            sources: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            hooks: AtomicUsize::new(0),
            decisions: AtomicUsize::new(0),
        });
        let engine = Engine::new(actix).await;
        let path = "/operation-source";
        let reference = match &engine {
            Engine::Thread(system) => {
                if factory {
                    let make = state.clone();
                    system
                        .spawn_typed_factory_with_strategy::<_, Message, _>(
                            move || Actor(make.clone()),
                            path,
                            state.policy(),
                        )
                        .await
                        .unwrap()
                } else {
                    system
                        .spawn_typed_with_strategy::<_, Message>(
                            Actor(state.clone()),
                            path,
                            SupervisorStrategyType::OneForOne(state.policy()),
                        )
                        .await
                        .unwrap()
                }
            }
            Engine::Actix(system) => {
                if factory {
                    let make = state.clone();
                    system
                        .spawn_typed_factory_with_strategy::<_, Message, _>(
                            move || Actor(make.clone()),
                            path,
                            state.policy(),
                        )
                        .await
                        .unwrap()
                } else {
                    system
                        .spawn_typed_with_strategy::<_, Message>(
                            Actor(state.clone()),
                            path,
                            SupervisorStrategyType::OneForOne(state.policy()),
                        )
                        .await
                        .unwrap()
                }
            }
        };
        let cloned = reference.clone();
        let reply = match stage {
            Stage::Init => None,
            Stage::Healthy => {
                reference.ask(Message::Ready).await.unwrap();
                reference.stop().await.unwrap();
                None
            }
            Stage::Handler => {
                reference.ask(Message::Ready).await.unwrap();
                Some(reference.ask(Message::Fail).await.unwrap_err())
            }
        };
        // Registry absence proves the original execution finished independently
        // of these late stop callers; stop cannot mask it by cancelling early.
        engine.absent(path).await;
        assert_eq!(state.drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            state.formats.load(Ordering::SeqCst),
            0,
            "framework formatted the operation error"
        );
        assert_eq!(state.sources.load(Ordering::SeqCst), 0);
        if matches!(stage, Stage::Healthy) {
            reference.stop().await.unwrap();
            cloned.stop().await.unwrap();
            assert_eq!(state.hooks.load(Ordering::SeqCst), 1);
            assert_eq!(state.decisions.load(Ordering::SeqCst), 0);
        } else {
            let first = reference.stop().await.unwrap_err();
            let second = cloned.stop().await.unwrap_err();
            let pointer = original(&first, &state);
            assert_eq!(
                original(&second, &state),
                pointer,
                "repeated stop replaced the error object"
            );
            if let Some(reply) = reply {
                assert_eq!(
                    original(&reply, &state),
                    pointer,
                    "reply and stop must share the original cause"
                );
            }
            assert_eq!(
                state.hooks.load(Ordering::SeqCst),
                0,
                "failed instance must not run a normal stop hook"
            );
            assert_eq!(state.decisions.load(Ordering::SeqCst), 1);
        }
        assert!(reference.ask(Message::Ready).await.is_err());
        engine.shutdown().await;
        assert_eq!(state.sources.load(Ordering::SeqCst), 0);
        assert_eq!(state.formats.load(Ordering::SeqCst), 0);
    }
    macro_rules! cases {
        ($($name:ident:$actix:expr,$factory:expr,$stage:ident,$hostile:expr;)*)=>{$(
            #[actix::test]
            async fn $name(){run($actix,$factory,Stage::$stage,$hostile).await;}
        )*}
    }
    cases! {
        thread_instance_healthy:false,false,Healthy,false;
        thread_factory_healthy:false,true,Healthy,false;
        actix_instance_healthy:true,false,Healthy,false;
        actix_factory_healthy:true,true,Healthy,false;
        thread_instance_init:false,false,Init,false;
        thread_instance_init_hostile:false,false,Init,true;
        thread_factory_init:false,true,Init,false;
        thread_factory_init_hostile:false,true,Init,true;
        actix_instance_init:true,false,Init,false;
        actix_instance_init_hostile:true,false,Init,true;
        actix_factory_init:true,true,Init,false;
        actix_factory_init_hostile:true,true,Init,true;
        thread_instance_handler:false,false,Handler,false;
        thread_instance_handler_hostile:false,false,Handler,true;
        thread_factory_handler:false,true,Handler,false;
        thread_factory_handler_hostile:false,true,Handler,true;
        actix_instance_handler:true,false,Handler,false;
        actix_instance_handler_hostile:true,false,Handler,true;
        actix_factory_handler:true,true,Handler,false;
        actix_factory_handler_hostile:true,true,Handler,true;
    }
}
