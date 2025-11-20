use super::*;
use crate::thread::{
    config::{SchedulingMode, ThreadActorSystemConfig},
    system::{ErasedSpawnable, TypedSpawnPayload},
};
use parrot_api::system::{GuardianConfig, SystemTimeouts};
use std::{sync::atomic::AtomicUsize, time::Duration};

const WAIT: Duration = Duration::from_secs(5);
type Gate = (oneshot::Sender<()>, std::sync::mpsc::Receiver<()>);

#[derive(Default)]
struct State {
    validation: AtomicUsize,
    publication: AtomicUsize,
    configs: AtomicUsize,
    actors: AtomicUsize,
    initialized: AtomicUsize,
    handled: AtomicUsize,
    init_delay: Duration,
}
struct Echo(Option<Arc<State>>);
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if let Some(state) = &self.0 {
                tokio::time::sleep(state.init_delay).await;
                state.initialized.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        if let Some(state) = &self.0 {
            state.handled.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async { Ok(message) })
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        if let Some(state) = &self.0 {
            state.actors.fetch_add(1, Ordering::SeqCst);
        }
    }
}
fn hold(gate: Gate) {
    let _ = gate.0.send(());
    gate.1
        .recv_timeout(WAIT)
        .expect("test gate was not released");
}
struct Config {
    state: Arc<State>,
    gate: Option<Gate>,
}
impl Drop for Config {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.take() {
            hold(gate);
        }
        self.state.configs.fetch_add(1, Ordering::SeqCst);
    }
}
struct Bridge {
    state: Arc<State>,
    gate: Mutex<Option<Gate>>,
    payload: Option<TypedSpawnPayload<Echo>>,
}
#[async_trait::async_trait]
impl ErasedSpawnable for Bridge {
    fn accepts_child_config(&self, config: &BoxedMessage) -> bool {
        self.state.validation.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = self.gate.lock().unwrap().take() {
            hold(gate);
        }
        config.is::<Config>()
    }
    fn spawn_child_on(
        mut self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        engine: ThreadActorConfig,
        parent: BoxedActorRef,
        receipt: ChildSpawnReceipt,
    ) -> ActorResult<BoxedActorRef> {
        self.state.publication.fetch_add(1, Ordering::SeqCst);
        Box::new(self.payload.take().unwrap()).spawn_child_on(system, engine, parent, receipt)
    }
    async fn spawn_on(
        self: Box<Self>,
        _: Arc<ThreadActorSystem>,
        _: BoxedMessage,
        _: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef> {
        panic!("child test must use the native child publication path");
    }
}
async fn setup(
    mode: SchedulingMode,
    creation: Duration,
    shutdown: Duration,
) -> (
    Arc<ThreadActorSystem>,
    ThreadActorRef<Echo>,
    Arc<crate::children::ChildScope>,
) {
    setup_with_owner(mode, creation, shutdown, None).await
}
async fn setup_with_owner(
    mode: SchedulingMode,
    creation: Duration,
    shutdown: Duration,
    owner: Option<tokio::runtime::Handle>,
) -> (
    Arc<ThreadActorSystem>,
    ThreadActorRef<Echo>,
    Arc<crate::children::ChildScope>,
) {
    let config = ThreadActorSystemConfig {
        default_scheduling_mode: mode,
        shared_pool_size: 2,
        shared_burst_workers_max: 0,
        shutdown_timeout: shutdown,
        ..Default::default()
    };
    let timeouts = SystemTimeouts {
        actor_creation: creation,
        system_shutdown: shutdown,
        ..Default::default()
    };
    let system = if let Some(owner) = owner {
        let mut system = ThreadActorSystem::new(config);
        system.operation_timeouts = timeouts;
        // Queue-state tests must block the actual lifecycle executor. The
        // public host-exit contracts retain the production constructor wiring.
        Arc::get_mut(&mut system.root_cohorts)
            .unwrap()
            .set_test_retirement_runtime(owner);
        let system = Arc::new(system);
        system.set_self_weak(Arc::downgrade(&system));
        let strategy = crate::root_cohort::guardian_strategy(
            crate::owned_payload::OwnedPayload::unbound(GuardianConfig::default()),
        )
        .await
        .unwrap();
        system
            .root_cohorts
            .install_guardian(strategy, shutdown, system.cleanup_slots.clone())
            .await
            .unwrap();
        system
    } else {
        ThreadActorSystem::with_guardian_and_timeouts(config, GuardianConfig::default(), timeouts)
            .await
            .unwrap()
    };
    let parent = system
        .spawn_root_typed_thread(Echo(None), EmptyConfig)
        .await
        .unwrap();
    let scope = match system.registry.read().unwrap().get(&parent.path()).unwrap() {
        ActorRegistryEntry::Dynamic(entry) => entry.children.clone(),
        _ => panic!("real parent was not published"),
    };
    (system, parent, scope)
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child lifecycle did not retire");
}
fn inputs(
    state: &Arc<State>,
    validation: Option<Gate>,
    config: Option<Gate>,
) -> (BoxedMessage, BoxedMessage) {
    (
        Box::new(ErasedSpawnBox(Box::new(Bridge {
            state: state.clone(),
            gate: Mutex::new(validation),
            payload: Some(TypedSpawnPayload::new(Echo(Some(state.clone())))),
        }))),
        Box::new(Config {
            state: state.clone(),
            gate: config,
        }),
    )
}
async fn retired(
    system: &Arc<ThreadActorSystem>,
    parent: &ThreadActorRef<Echo>,
    scope: &Arc<crate::children::ChildScope>,
    state: &Arc<State>,
) {
    until(|| {
        system.actor_count() == 1
            && scope.view().is_empty()
            && state.actors.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(state.configs.load(Ordering::SeqCst), 1);
    assert_eq!(
        *parent
            .send(Box::new(42_u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    system.shutdown_internal().await.unwrap();
}
// Keep a synchronous stage blocked after the caller times out. The retained
// lifecycle must retire its inputs when released, without late publication.
async fn expires(mode: SchedulingMode, stage: u8) {
    let (system, parent, scope) =
        setup(mode, Duration::from_millis(100), Duration::from_secs(3)).await;
    let state = Arc::new(State::default());
    let (entered, observed) = oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let gate = (entered, held);
    let (actor, config) = match stage {
        1 => inputs(&state, None, Some(gate)),
        2 => inputs(&state, None, None),
        _ => inputs(&state, Some(gate), None),
    };
    let permit = if stage == 2 {
        Some(
            system
                .factory_slots
                .clone()
                .acquire_many_owned(system.factory_slots.available_permits() as u32)
                .await
                .unwrap(),
        )
    } else {
        None
    };
    let owner = system.clone();
    let ancestor = parent.clone_boxed();
    let children = scope.clone();
    let mut caller = tokio::spawn(async move {
        owner
            .spawn_child_erased(actor, config, None, ancestor, &children)
            .await
    });
    if stage != 2 {
        tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
    }
    let outcome = tokio::time::timeout(Duration::from_secs(1), &mut caller).await;
    if stage != 2 {
        release.send(()).unwrap();
    }
    drop(permit);
    // Release every gate before asserting, including when testing old code.
    if outcome.is_err() {
        if let Ok(Ok(child)) = caller.await {
            child.stop().await.unwrap();
        }
    } else if let Ok(Ok(Ok(child))) = &outcome {
        child.stop().await.unwrap();
    }
    retired(&system, &parent, &scope, &state).await;
    assert!(
        matches!(outcome, Ok(Ok(Err(ActorError::TimeoutDetail(_))))),
        "configured child creation deadline did not reach the caller"
    );
    assert_eq!(state.publication.load(Ordering::SeqCst), 0);
    assert_eq!(state.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(state.handled.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.validation.load(Ordering::SeqCst),
        usize::from(stage != 2)
    );
}
async fn longer(mode: SchedulingMode) {
    let (system, parent, scope) = setup(
        mode,
        Duration::from_millis(1500),
        Duration::from_millis(100),
    )
    .await;
    let state = Arc::new(State::default());
    let (entered, observed) = oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let (actor, config) = inputs(&state, Some((entered, held)), None);
    let owner = system.clone();
    let ancestor = parent.clone_boxed();
    let children = scope.clone();
    let caller = tokio::spawn(async move {
        owner
            .spawn_child_erased(actor, config, None, ancestor, &children)
            .await
    });
    tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    release.send(()).unwrap();
    let result = tokio::time::timeout(WAIT, caller).await.unwrap().unwrap();
    if let Ok(child) = &result {
        assert_eq!(
            *child
                .send(Box::new(9_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            9
        );
        child.stop().await.unwrap();
    }
    retired(&system, &parent, &scope, &state).await;
    assert!(
        result.is_ok(),
        "shutdown timeout shortened a longer child creation budget"
    );
    assert_eq!(state.publication.load(Ordering::SeqCst), 1);
    assert_eq!(state.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(state.handled.load(Ordering::SeqCst), 1);
}
async fn shared_init(mode: SchedulingMode) {
    let (system, parent, scope) =
        setup(mode, Duration::from_millis(600), Duration::from_secs(3)).await;
    let state = Arc::new(State {
        init_delay: Duration::from_millis(450),
        ..Default::default()
    });
    let (entered, observed) = oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let (actor, config) = inputs(&state, None, Some((entered, held)));
    let owner = system.clone();
    let ancestor = parent.clone_boxed();
    let children = scope.clone();
    let caller = tokio::spawn(async move {
        owner
            .spawn_child_erased(actor, config, None, ancestor, &children)
            .await
    });
    tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    release.send(()).unwrap();
    let child = tokio::time::timeout(WAIT, caller)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let reply = tokio::time::timeout(WAIT, child.send(Box::new(7_u64)))
        .await
        .unwrap();
    // With a reset budget this request succeeds. Stop it so the negative
    // control also releases every worker before checking the regression.
    if reply.is_ok() {
        child.stop().await.unwrap();
    }
    retired(&system, &parent, &scope, &state).await;
    assert!(
        reply.is_err(),
        "child initialization restarted the creation budget after config disposal"
    );
    assert_eq!(state.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(state.handled.load(Ordering::SeqCst), 0);
    assert_eq!(state.publication.load(Ordering::SeqCst), 1);
}

fn queued_callback(mode: SchedulingMode, publication: bool) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let (system, parent, scope) = setup_with_owner(
                mode,
                Duration::from_millis(300),
                Duration::from_secs(3),
                Some(tokio::runtime::Handle::current()),
            )
            .await;
            parent.send(Box::new(())).await.unwrap();
            let state = Arc::new(State::default());
            let (config_entered, config_observed) = oneshot::channel();
            let (config_release, config_held) = std::sync::mpsc::channel();
            let config_gate = publication.then_some((config_entered, config_held));
            let (actor, config) = inputs(&state, None, config_gate);
            let slots = system.factory_slots.available_permits();
            let owner = system.clone();
            let ancestor = parent.clone_boxed();
            let children = scope.clone();
            let start = async move {
                owner
                    .spawn_child_erased(actor, config, None, ancestor, &children)
                    .await
            };
            let (entered, observed) = oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            let (mut caller, blocker) = if publication {
                let caller = tokio::spawn(start);
                tokio::time::timeout(WAIT, config_observed)
                    .await
                    .unwrap()
                    .unwrap();
                // Queue this job while the sole worker is still in config Drop.
                let blocker = tokio::task::spawn_blocking(move || hold((entered, held)));
                config_release.send(()).unwrap();
                (caller, blocker)
            } else {
                let blocker = tokio::task::spawn_blocking(move || hold((entered, held)));
                (tokio::spawn(start), blocker)
            };
            tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
            until(|| system.factory_slots.available_permits() < slots).await;
            let result = tokio::time::timeout(Duration::from_secs(1), &mut caller).await;
            release.send(()).unwrap();
            blocker.await.unwrap();
            if result.is_err() {
                if let Ok(Ok(child)) = caller.await {
                    child.stop().await.unwrap();
                }
            } else if let Ok(Ok(Ok(child))) = &result {
                child.stop().await.unwrap();
            }
            retired(&system, &parent, &scope, &state).await;
            assert!(matches!(result, Ok(Ok(Err(ActorError::TimeoutDetail(_))))));
            assert_eq!(
                state.validation.load(Ordering::SeqCst),
                usize::from(publication),
                "expired queued validation entered user code"
            );
            assert_eq!(
                state.publication.load(Ordering::SeqCst),
                0,
                "expired queued publication entered user code"
            );
            assert_eq!(state.initialized.load(Ordering::SeqCst), 0);
            assert_eq!(state.handled.load(Ordering::SeqCst), 0);
        });
}

macro_rules! queued_contracts {
    ($($name:ident, $mode:expr, $publication:expr;)*) => {$ (
        #[test]
        fn $name() { queued_callback($mode, $publication); }
    )*};
}
queued_contracts! {
    shared_queued_validation_callback, SchedulingMode::SharedPool { max_messages_per_run: 10 }, false;
    dedicated_queued_validation_callback, SchedulingMode::DedicatedThread, false;
    sharded_queued_validation_callback, SchedulingMode::Sharded { affinity_key: "child-timeout".into() }, false;
    shared_queued_publication_callback, SchedulingMode::SharedPool { max_messages_per_run: 10 }, true;
    dedicated_queued_publication_callback, SchedulingMode::DedicatedThread, true;
    sharded_queued_publication_callback, SchedulingMode::Sharded { affinity_key: "child-timeout".into() }, true;
}

macro_rules! contracts {
    ($($name:ident, $mode:expr, $check:ident $(, $stage:expr)?;)*) => {$ (
        #[tokio::test]
        async fn $name() { $check($mode $(, $stage)?).await; }
    )*};
}
contracts! {
    shared_validation, SchedulingMode::SharedPool { max_messages_per_run: 10 }, expires, 0;
    dedicated_validation, SchedulingMode::DedicatedThread, expires, 0;
    sharded_validation, SchedulingMode::Sharded { affinity_key: "child-timeout".into() }, expires, 0;
    shared_config, SchedulingMode::SharedPool { max_messages_per_run: 10 }, expires, 1;
    dedicated_config, SchedulingMode::DedicatedThread, expires, 1;
    sharded_config, SchedulingMode::Sharded { affinity_key: "child-timeout".into() }, expires, 1;
    shared_queued, SchedulingMode::SharedPool { max_messages_per_run: 10 }, expires, 2;
    dedicated_queued, SchedulingMode::DedicatedThread, expires, 2;
    sharded_queued, SchedulingMode::Sharded { affinity_key: "child-timeout".into() }, expires, 2;
    shared_longer_creation, SchedulingMode::SharedPool { max_messages_per_run: 10 }, longer;
    dedicated_longer_creation, SchedulingMode::DedicatedThread, longer;
    sharded_longer_creation, SchedulingMode::Sharded { affinity_key: "child-timeout".into() }, longer;
    shared_initialization_budget, SchedulingMode::SharedPool { max_messages_per_run: 10 }, shared_init;
    dedicated_initialization_budget, SchedulingMode::DedicatedThread, shared_init;
    sharded_initialization_budget, SchedulingMode::Sharded { affinity_key: "child-timeout".into() }, shared_init;
}
