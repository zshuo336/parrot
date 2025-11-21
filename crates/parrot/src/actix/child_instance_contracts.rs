//! Real root/child admission, recoverable inputs and actual failed retirement.
use super::*;
use crate::actix::{
    ActixActor, ActixActorSystem, ActixSystemSettings, ChildInstanceAdmissionError,
    ChildInstanceAdmissionFailure,
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorConfig, ActorState, EmptyConfig},
    address::ActorRef,
    context::ActorSpawnContext,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    types::BoxedMessage,
};
use std::{
    any::Any,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore, oneshot};

const WAIT: Duration = Duration::from_secs(5);
#[derive(Default)]
struct Gate {
    entered: Notify,
    open: Mutex<bool>,
    changed: Condvar,
    disposed: AtomicUsize,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(10), |open| {
                !*open
            })
            .unwrap();
        assert!(*open, "test must release destructor");
        self.disposed.fetch_add(1, Ordering::SeqCst);
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Vec<Arc<Gate>>);
impl Drop for Release {
    fn drop(&mut self) {
        for gate in &self.0 {
            gate.release();
        }
    }
}
#[derive(Default)]
struct Counts {
    initialized: AtomicUsize,
    actors: AtomicUsize,
    configs: AtomicUsize,
    validated: AtomicUsize,
}
struct Config {
    counts: Arc<Counts>,
    gate: Option<Arc<Gate>>,
}
impl ActorConfig for Config {}
impl Drop for Config {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.counts.configs.fetch_add(1, Ordering::SeqCst);
    }
}
struct Probe {
    counts: Arc<Counts>,
    init: Option<Arc<Semaphore>>,
    fail: bool,
    gate: Option<Arc<Gate>>,
    panic_drop: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "child actor destructor receipt");
    }
}
impl ActixEngineExt for Probe {}
impl Actor for Probe {
    type Config = Config;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.initialized.fetch_add(1, Ordering::SeqCst);
            if let Some(init) = &self.init {
                init.acquire().await.unwrap().forget();
            }
            if self.fail {
                Err(ActorError::InitializationError("child init receipt".into()))
            } else {
                Ok(())
            }
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}
struct Validating(Probe);
impl SpawnPayload for Validating {
    fn accepts_config(&self, config: &BoxedMessage) -> bool {
        self.0.counts.validated.fetch_add(1, Ordering::SeqCst);
        config.is::<Config>()
    }
    fn spawn(
        self: Box<Self>,
        system: &ActixActorSystem,
        parent: BoxedActorRef,
        lifetime: Arc<crate::children::ChildLifetime>,
        config: BoxedMessage,
        path: String,
        creation: crate::actix::system::Creation,
    ) -> Result<super::super::system::InstanceTicket, super::super::disposal::RejectedSpawn> {
        Box::new(TypedSpawn(self.0)).spawn(system, parent, lifetime, config, path, creation)
    }
}
fn probe(counts: &Arc<Counts>) -> Probe {
    Probe {
        counts: counts.clone(),
        init: None,
        fail: false,
        gate: None,
        panic_drop: false,
    }
}
fn config(counts: &Arc<Counts>) -> Config {
    Config {
        counts: counts.clone(),
        gate: None,
    }
}
fn inputs(counts: &Arc<Counts>) -> (BoxedMessage, BoxedMessage) {
    (
        Box::new(ErasedSpawn(Box::new(Validating(probe(counts))))),
        Box::new(config(counts)),
    )
}
struct Request {
    actor: BoxedMessage,
    config: BoxedMessage,
    strategy: Option<SupervisorStrategyType>,
    reply: oneshot::Sender<ActorResult<BoxedActorRef>>,
}
struct Parent;
impl ActixEngineExt for Parent {}
impl Actor for Parent {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match message.downcast::<Request>() {
                Ok(request) => {
                    let result = match request.strategy {
                        Some(strategy) => {
                            ctx.spawner()
                                .spawn_with_strategy(request.actor, request.config, strategy)
                                .await
                        }
                        None => ctx.spawner().spawn(request.actor, request.config).await,
                    };
                    let _ = request.reply.send(result);
                    Ok(Box::new(()) as BoxedMessage)
                }
                Err(message) => Ok(message),
            }
        })
    }
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("creation lifecycle did not finish");
}
async fn entered(gate: &Arc<Gate>) {
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .unwrap();
}
async fn setup() -> (ActixActorSystem, BoxedActorRef, ManagedSpawner) {
    let mut system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 2,
        shutdown_timeout: WAIT,
        ..Default::default()
    })
    .await
    .unwrap();
    system.instance_admission = Arc::new(Semaphore::new(1));
    system.cleanup_slots = Arc::new(Semaphore::new(4));
    let parent = system.spawn_root_typed(Parent, EmptyConfig).await.unwrap();
    parent.send(Box::new(())).await.unwrap();
    until(|| system.instance_admission.available_permits() == 1).await;
    let scope = system
        .registry
        .read()
        .unwrap()
        .actors
        .get(&parent.path())
        .unwrap()
        .control
        .children
        .clone();
    let spawner = ManagedSpawner {
        service: Some(system.downgrade()),
        scope: Some(scope),
        parent: Some(Arc::new(parent.clone_boxed())),
    };
    (system, parent, spawner)
}
async fn through(
    parent: &BoxedActorRef,
    actor: BoxedMessage,
    config: BoxedMessage,
    strategy: Option<SupervisorStrategyType>,
) -> ActorResult<BoxedActorRef> {
    let (reply, receive) = oneshot::channel();
    parent
        .deliver(Box::new(Request {
            actor,
            config,
            strategy,
            reply,
        }))
        .await
        .unwrap();
    tokio::time::timeout(WAIT, receive).await.unwrap().unwrap()
}
fn rejection(result: ActorResult<BoxedActorRef>) -> ChildInstanceAdmissionError {
    match result {
        Err(ActorError::Other(error)) => error.downcast::<ChildInstanceAdmissionError>().unwrap(),
        _ => panic!("expected recoverable child admission rejection"),
    }
}
struct PolicyCapture(Arc<Gate>);
impl Drop for PolicyCapture {
    fn drop(&mut self) {
        self.0.block();
    }
}
fn strategy(gate: &Arc<Gate>) -> SupervisorStrategyType {
    let capture = PolicyCapture(gate.clone());
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    })
}

#[actix::test]
async fn root_saturation_returns_exact_child_inputs_before_validation_and_allows_retry() {
    for explicit in [false, true] {
        let (system, parent, _) = setup().await;
        let roots = Arc::new(Counts::default());
        let init = Arc::new(Semaphore::new(0));
        let mut root = probe(&roots);
        root.init = Some(init.clone());
        let root = system.spawn_root_typed(root, config(&roots)).await.unwrap();
        until(|| roots.initialized.load(Ordering::SeqCst) == 1).await;
        let counts = Arc::new(Counts::default());
        let (actor, config) = inputs(&counts);
        let actor_identity = actor.as_ref() as *const dyn Any as *const ();
        let config_identity = config.as_ref() as *const dyn Any as *const ();
        let policy = Arc::new(Gate::default());
        policy.release();
        let rejected =
            rejection(through(&parent, actor, config, explicit.then(|| strategy(&policy))).await);
        assert_eq!(rejected.reason, ChildInstanceAdmissionFailure::Full);
        assert_eq!(counts.validated.load(Ordering::SeqCst), 0);
        assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
        assert_eq!(counts.configs.load(Ordering::SeqCst), 0);
        assert_eq!(policy.disposed.load(Ordering::SeqCst), 0);
        let recovered = rejected.into_inputs();
        assert_eq!(
            recovered.actor.as_ref() as *const dyn Any as *const (),
            actor_identity
        );
        assert_eq!(
            recovered.config.as_ref() as *const dyn Any as *const (),
            config_identity
        );
        assert_eq!(recovered.strategy.is_some(), explicit);
        init.add_permits(1);
        root.send(Box::new(())).await.unwrap();
        let child = through(
            &parent,
            recovered.actor,
            recovered.config,
            recovered.strategy,
        )
        .await
        .unwrap();
        child.send(Box::new(17u32)).await.unwrap();
        assert_eq!(counts.validated.load(Ordering::SeqCst), 1);
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
        assert_eq!(system.instance_admission.available_permits(), 1);
        child.stop().await.unwrap();
        root.stop().await.unwrap();
        parent.stop().await.unwrap();
        system.clone().shutdown().await.unwrap();
        assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
        assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
        assert_eq!(
            policy.disposed.load(Ordering::SeqCst),
            usize::from(explicit)
        );
    }
}

#[actix::test]
async fn closed_system_returns_unmodified_inputs_without_validation_or_cleanup() {
    let (system, parent, spawner) = setup().await;
    system.instance_admission.close();
    let counts = Arc::new(Counts::default());
    let (actor, config) = inputs(&counts);
    let policy = Arc::new(Gate::default());
    policy.release();
    let rejected = rejection(
        spawner
            .spawn_with_strategy(actor, config, strategy(&policy))
            .await,
    );
    assert_eq!(rejected.reason, ChildInstanceAdmissionFailure::ShuttingDown);
    assert_eq!(counts.validated.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 0);
    assert_eq!(policy.disposed.load(Ordering::SeqCst), 0);
    drop(rejected.into_inputs());
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    assert_eq!(policy.disposed.load(Ordering::SeqCst), 1);
    parent.stop().await.unwrap();
    system.clone().shutdown().await.unwrap();
}

struct InvalidInput {
    gate: Arc<Gate>,
    panic: bool,
}
impl Drop for InvalidInput {
    fn drop(&mut self) {
        self.gate.block();
        assert!(!self.panic, "child rejected input receipt");
    }
}
#[actix::test]
async fn invalid_child_inputs_keep_capacity_until_every_destructor_finishes() {
    for cancel in [false, true] {
        let (system, parent, spawner) = setup().await;
        let gates: Vec<_> = (0..3).map(|_| Arc::new(Gate::default())).collect();
        let release = Release(gates.clone());
        let mut creation = spawner.spawn_with_strategy(
            Box::new(InvalidInput {
                gate: gates[0].clone(),
                panic: !cancel,
            }),
            Box::new(InvalidInput {
                gate: gates[1].clone(),
                panic: false,
            }),
            strategy(&gates[2]),
        );
        assert!(futures::poll!(&mut creation).is_pending());
        for gate in &gates {
            entered(gate).await;
        }
        assert_eq!(system.instance_admission.available_permits(), 0);
        if cancel {
            drop(creation);
            creation = Box::pin(async { Err(ActorError::Stopped) });
        }
        for gate in &gates[..2] {
            gate.release();
            until(|| gate.disposed.load(Ordering::SeqCst) == 1).await;
            assert_eq!(system.instance_admission.available_permits(), 0);
        }
        drop(release);
        if !cancel {
            let error = creation.await.unwrap_err();
            assert!(error.to_string().contains("child rejected input receipt"));
        }
        until(|| system.instance_admission.available_permits() == 1).await;
        assert_eq!(gates[2].disposed.load(Ordering::SeqCst), 1);
        assert_eq!(system.registry.read().unwrap().actors.len(), 1);
        parent.stop().await.unwrap();
        system.clone().shutdown().await.unwrap();
    }
}

#[actix::test]
async fn failed_child_init_retains_slot_through_actor_and_policy_disposal() {
    for panic_drop in [false, true] {
        let (system, parent, spawner) = setup().await;
        let counts = Arc::new(Counts::default());
        let actor_gate = Arc::new(Gate::default());
        let policy_gate = Arc::new(Gate::default());
        let release = Release(vec![actor_gate.clone(), policy_gate.clone()]);
        let mut actor = probe(&counts);
        actor.fail = true;
        actor.gate = Some(actor_gate.clone());
        actor.panic_drop = panic_drop;
        let child = spawner
            .spawn_with_strategy(
                <ActixContext<ActixActor<Probe>> as ActorSpawnContext<Probe>>::wrap_spawn(actor),
                Box::new(config(&counts)),
                strategy(&policy_gate),
            )
            .await
            .unwrap();
        entered(&actor_gate).await;
        assert_eq!(system.instance_admission.available_permits(), 0);
        actor_gate.release();
        entered(&policy_gate).await;
        assert_eq!(system.instance_admission.available_permits(), 0);
        drop(release);
        until(|| system.instance_admission.available_permits() == 1).await;
        let error = child.stop().await.unwrap_err().to_string();
        assert!(
            error.contains(if panic_drop {
                "child actor destructor receipt"
            } else {
                "child init receipt"
            }),
            "{error}"
        );
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
        assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
        assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
        parent.stop().await.unwrap();
        system.clone().shutdown().await.unwrap();
    }
}

#[actix::test]
async fn child_config_disposal_spends_original_deadline_without_late_initialization() {
    let (mut system, parent, mut spawner) = setup().await;
    system.operation_timeouts.actor_creation = Duration::from_millis(150);
    spawner.service = Some(system.downgrade());
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(vec![gate.clone()]);
    let mut cfg = config(&counts);
    cfg.gate = Some(gate.clone());
    let mut creation = spawner.spawn(
        <ActixContext<ActixActor<Probe>> as ActorSpawnContext<Probe>>::wrap_spawn(probe(&counts)),
        Box::new(cfg),
    );
    assert!(futures::poll!(&mut creation).is_pending());
    entered(&gate).await;
    let result = tokio::time::timeout(Duration::from_millis(800), creation)
        .await
        .unwrap();
    assert!(matches!(result, Err(ActorError::TimeoutDetail(_))));
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 0);
    assert_eq!(system.instance_admission.available_permits(), 0);
    drop(release);
    until(|| system.instance_admission.available_permits() == 1).await;
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    parent.stop().await.unwrap();
    system.clone().shutdown().await.unwrap();
}

struct Reference {
    inner: Arc<dyn ActorRef>,
    gate: Arc<Gate>,
    is_clone: bool,
    path_delay: Duration,
}
impl std::fmt::Debug for Reference {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        panic!("child creation must not format its parent");
    }
}
impl Drop for Reference {
    fn drop(&mut self) {
        if self.is_clone {
            self.gate.block();
        }
    }
}
impl ActorRef for Reference {
    fn send(&self, message: BoxedMessage) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        self.inner.send(message)
    }
    fn send_with_timeout(
        &self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        self.inner.send_with_timeout(message, timeout)
    }
    fn deliver(&self, message: BoxedMessage) -> BoxedFuture<'_, ActorResult<()>> {
        self.inner.deliver(message)
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        self.inner.stop()
    }
    fn path(&self) -> String {
        std::thread::sleep(self.path_delay);
        self.inner.path()
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        self.inner.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            inner: self.inner.clone(),
            gate: self.gate.clone(),
            is_clone: true,
            path_delay: self.path_delay,
        })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[actix::test]
async fn closed_scope_retains_untransferred_parent_until_actual_disposal_after_cancellation() {
    let (system, parent, mut spawner) = setup().await;
    let gate = Arc::new(Gate::default());
    let release = Release(vec![gate.clone()]);
    spawner.parent = Some(Arc::new(Box::new(Reference {
        inner: Arc::from(parent.clone_boxed()),
        gate: gate.clone(),
        is_clone: false,
        path_delay: Duration::ZERO,
    })));
    spawner.scope.as_ref().unwrap().close();
    let counts = Arc::new(Counts::default());
    let (actor, cfg) = inputs(&counts);
    let mut creation = spawner.spawn(actor, cfg);
    assert!(futures::poll!(&mut creation).is_pending());
    entered(&gate).await;
    drop(creation);
    until(|| {
        counts.actors.load(Ordering::SeqCst) == 1 && counts.configs.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(system.instance_admission.available_permits(), 0);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(system.registry.read().unwrap().actors.len(), 1);
    drop(release);
    until(|| system.instance_admission.available_permits() == 1).await;
    assert_eq!(gate.disposed.load(Ordering::SeqCst), 1);
    parent.stop().await.unwrap();
    system.clone().shutdown().await.unwrap();
}

#[actix::test]
async fn synchronous_parent_callback_cannot_publish_after_original_deadline() {
    let (mut system, parent, mut spawner) = setup().await;
    system.operation_timeouts.actor_creation = Duration::from_millis(50);
    spawner.service = Some(system.downgrade());
    let gate = Arc::new(Gate::default());
    gate.release();
    spawner.parent = Some(Arc::new(Box::new(Reference {
        inner: Arc::from(parent.clone_boxed()),
        gate,
        is_clone: false,
        path_delay: Duration::from_millis(200),
    })));
    let counts = Arc::new(Counts::default());
    let (actor, cfg) = inputs(&counts);
    let result = spawner.spawn(actor, cfg).await;
    assert!(matches!(result, Err(ActorError::TimeoutDetail(_))));
    until(|| system.instance_admission.available_permits() == 1).await;
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    assert_eq!(system.registry.read().unwrap().actors.len(), 1);
    parent.stop().await.unwrap();
    system.clone().shutdown().await.unwrap();
}

struct ParentDuringInit {
    counts: Arc<Counts>,
    reply: Option<oneshot::Sender<ActorResult<BoxedActorRef>>>,
}
impl ActixEngineExt for ParentDuringInit {}
impl Actor for ParentDuringInit {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        use parrot_api::context::ActorSpawnerExt;
        Box::pin(async move {
            let result = ctx
                .spawner()
                .spawn_typed(probe(&self.counts), config(&self.counts))
                .await;
            let _ = self.reply.take().unwrap().send(result);
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}
#[actix::test]
async fn child_creation_in_parent_init_rejects_without_waiting_for_parent_slot() {
    let (system, parent, _) = setup().await;
    let counts = Arc::new(Counts::default());
    let (reply, receive) = oneshot::channel();
    let initializing_parent = system
        .spawn_root_typed(
            ParentDuringInit {
                counts: counts.clone(),
                reply: Some(reply),
            },
            EmptyConfig,
        )
        .await
        .unwrap();
    let rejected = rejection(
        tokio::time::timeout(Duration::from_millis(800), receive)
            .await
            .expect("child waited for the creation slot still held by parent init")
            .unwrap(),
    );
    assert_eq!(rejected.reason, ChildInstanceAdmissionFailure::Full);
    drop(rejected.into_inputs());
    initializing_parent.send(Box::new(())).await.unwrap();
    assert_eq!(system.instance_admission.available_permits(), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    initializing_parent.stop().await.unwrap();
    parent.stop().await.unwrap();
    system.clone().shutdown().await.unwrap();
}

#[actix::test]
async fn native_publication_rejection_retires_parent_outside_registry_lock() {
    let (system, parent, _) = setup().await;
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(vec![gate.clone()]);
    let permit = Arc::new(
        system
            .instance_admission
            .clone()
            .try_acquire_owned()
            .unwrap(),
    );
    let rejected = match system.spawn_managed_with_creation(
        probe(&counts),
        config(&counts),
        Some(Box::new(Reference {
            inner: Arc::from(parent.clone_boxed()),
            gate: gate.clone(),
            is_clone: true,
            path_delay: Duration::ZERO,
        })),
        None,
        Some(parent.path()),
        Some(crate::actix::system::Creation {
            permit: permit.clone(),
            deadline: system.instance_creation_deadline(),
        }),
    ) {
        Ok(_) => panic!("duplicate path admitted"),
        Err(rejected) => rejected,
    };
    assert!(matches!(
        rejected.error,
        parrot_api::system::SystemError::ActorCreationError(_)
    ));
    drop(permit);
    entered(&gate).await;
    until(|| {
        counts.actors.load(Ordering::SeqCst) == 1 && counts.configs.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(
        system.registry.try_write().is_ok(),
        "parent destructor held the registry lock"
    );
    assert_eq!(system.instance_admission.available_permits(), 0);
    drop(rejected.retired);
    assert_eq!(system.instance_admission.available_permits(), 0);
    drop(release);
    until(|| system.instance_admission.available_permits() == 1).await;
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    parent.stop().await.unwrap();
    system.clone().shutdown().await.unwrap();
}
