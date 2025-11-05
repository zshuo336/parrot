use futures::FutureExt;
use parrot::actix::{
    ActixActor, ActixActorRef, ActixActorSystem, ActixContext, ActixSystemSettings,
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorConfig, ActorState},
    context::{ActorContext, ActorSpawnContext, ActorSpawnerExt},
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (_guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(4), |open| {
                !*open
            })
            .unwrap();
        assert!(!timeout.timed_out(), "test cleanup gate not released");
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(2), self.entered.notified())
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
#[derive(Default)]
struct Counts {
    actor: Arc<AtomicUsize>,
    config: Arc<AtomicUsize>,
    policy: Arc<AtomicUsize>,
    initialized: Arc<AtomicUsize>,
}
struct Resource {
    drops: Arc<AtomicUsize>,
    gate: Option<Arc<Gate>>,
    panic: bool,
    owner: Option<std::thread::ThreadId>,
}
impl Resource {
    fn new(drops: Arc<AtomicUsize>, gate: Option<Arc<Gate>>, panic: bool) -> Self {
        Self {
            drops,
            gate,
            panic,
            owner: None,
        }
    }
}
impl Drop for Resource {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(owner) = self.owner {
            assert_eq!(
                owner,
                std::thread::current().id(),
                "live actor moved off its arbiter during destruction"
            );
        }
        assert!(!self.panic, "intentional input destructor panic");
    }
}
struct Config {
    _resource: Resource,
}
impl ActorConfig for Config {}
struct Instance {
    resource: Resource,
    initialized: Arc<AtomicUsize>,
}
impl ActixEngineExt for Instance {}
struct OwnContext;
impl Actor for Instance {
    type Config = Config;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.resource.owner = Some(std::thread::current().id());
        self.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<OwnContext>() {
                return Ok(Box::new(ctx.clone()) as BoxedMessage);
            }
            Ok(Box::new(42usize) as BoxedMessage)
        })
    }
}
fn instance(counts: &Counts, gate: Option<Arc<Gate>>, panic: bool) -> Instance {
    Instance {
        resource: Resource::new(counts.actor.clone(), gate, panic),
        initialized: counts.initialized.clone(),
    }
}
fn config(counts: &Counts, gate: Option<Arc<Gate>>, panic: bool) -> Config {
    Config {
        _resource: Resource::new(counts.config.clone(), gate, panic),
    }
}
fn strategy(counts: &Counts, panic: bool) -> SupervisorStrategyType {
    let resource = Resource::new(counts.policy.clone(), None, panic);
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(30),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &resource;
            SupervisionDecision::Stop
        }),
    })
}
async fn system() -> ActixActorSystem {
    ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await
    .unwrap()
}
async fn create_parent(
    sys: &ActixActorSystem,
) -> (BoxedActorRef, ActixContext<ActixActor<Instance>>) {
    let counts = Counts::default();
    let actor = sys
        .spawn_root_typed(instance(&counts, None, false), config(&counts, None, false))
        .await
        .unwrap();
    let ctx = *actor
        .send(Box::new(OwnContext))
        .await
        .unwrap()
        .downcast::<ActixContext<ActixActor<Instance>>>()
        .unwrap();
    (actor, ctx)
}
async fn stopped(sys: &ActixActorSystem) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.status().state != parrot_api::system::SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn dropped(count: &AtomicUsize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while count.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[actix::test]
async fn rejected_root_disposes_actor_and_config_even_when_both_panic() {
    let sys = system().await;
    sys.clone().shutdown().await.unwrap();
    let counts = Counts::default();
    let result = std::panic::AssertUnwindSafe(
        sys.spawn_root_typed(instance(&counts, None, true), config(&counts, None, true)),
    )
    .catch_unwind()
    .await;
    assert!(result.is_ok(), "user destructor panic escaped caller");
    assert!(result.unwrap().is_err());
    assert_eq!(counts.actor.load(Ordering::SeqCst), 1);
    assert_eq!(counts.config.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
}
#[actix::test]
async fn retired_context_disposes_all_three_panicking_inputs() {
    let sys = system().await;
    let (_, mut ctx) = create_parent(&sys).await;
    sys.shutdown().await.unwrap();
    let counts = Counts::default();
    let actor = <ActixContext<ActixActor<Instance>> as ActorSpawnContext<Instance>>::wrap_spawn(
        instance(&counts, None, true),
    );
    let spawner = ctx.spawner();
    let result = std::panic::AssertUnwindSafe(spawner.spawn_with_strategy(
        actor,
        Box::new(config(&counts, None, true)),
        strategy(&counts, true),
    ))
    .catch_unwind()
    .await;
    assert!(
        result.is_ok(),
        "retired-context rejection must contain every destructor panic"
    );
    assert!(result.unwrap().is_err());
    for count in [&counts.actor, &counts.config, &counts.policy] {
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
}
#[actix::test]
async fn malformed_bridge_payload_still_retires_config_and_strategy() {
    let sys = system().await;
    let (_, mut ctx) = create_parent(&sys).await;
    let counts = Counts::default();
    let spawner = ctx.spawner();
    let result = std::panic::AssertUnwindSafe(spawner.spawn_with_strategy(
        Box::new(Resource::new(counts.actor.clone(), None, true)),
        Box::new(config(&counts, None, true)),
        strategy(&counts, true),
    ))
    .catch_unwind()
    .await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_err());
    for count in [&counts.actor, &counts.config, &counts.policy] {
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    sys.shutdown().await.unwrap();
}
#[actix::test]
async fn mismatched_config_retires_every_input_without_initializing_actor() {
    let sys = system().await;
    let (_, mut ctx) = create_parent(&sys).await;
    let counts = Counts::default();
    let actor = <ActixContext<ActixActor<Instance>> as ActorSpawnContext<Instance>>::wrap_spawn(
        instance(&counts, None, true),
    );
    let spawner = ctx.spawner();
    let result = std::panic::AssertUnwindSafe(spawner.spawn_with_strategy(
        actor,
        Box::new(Resource::new(counts.config.clone(), None, true)),
        strategy(&counts, true),
    ))
    .catch_unwind()
    .await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_err());
    for count in [&counts.actor, &counts.config, &counts.policy] {
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    sys.shutdown().await.unwrap();
}
#[actix::test]
async fn cancelled_closed_scope_rejection_keeps_cleanup_owned_and_executor_responsive() {
    let sys = system().await;
    let (parent, mut ctx) = create_parent(&sys).await;
    parent.stop().await.unwrap();
    let (healthy, _) = create_parent(&sys).await;
    let counts = Counts::default();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let spawner = ctx.spawner();
    let mut create = Box::pin(spawner.spawn_typed(
        instance(&counts, Some(gate.clone()), false),
        config(&counts, None, false),
    ));
    tokio::select! { result=&mut create=>panic!("early rejection: {result:?}"), _=gate.entered()=>{} }
    drop(create);
    assert_eq!(
        *tokio::time::timeout(Duration::from_millis(50), healthy.send(Box::new(())))
            .await
            .unwrap()
            .unwrap()
            .downcast::<usize>()
            .unwrap(),
        42
    );
    gate.release();
    dropped(&counts.actor).await;
    dropped(&counts.config).await;
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    sys.shutdown().await.unwrap();
}
#[actix::test]
async fn valid_config_cleanup_is_registered_before_shutdown_and_never_initializes_after_stop() {
    let sys = system().await;
    let counts = Counts::default();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut create = Box::pin(sys.spawn_root_typed(
        instance(&counts, None, false),
        config(&counts, Some(gate.clone()), false),
    ));
    tokio::select! { result=&mut create=>panic!("early creation: {result:?}"), _=gate.entered()=>{} }
    assert_eq!(sys.status().active_actors, 1);
    assert!(sys.clone().shutdown().await.is_err());
    assert_eq!(
        sys.status().state,
        parrot_api::system::SystemState::ShuttingDown
    );
    assert_eq!(counts.config.load(Ordering::SeqCst), 0);
    gate.release();
    assert!(create.await.is_err());
    stopped(&sys).await;
    assert_eq!(counts.actor.load(Ordering::SeqCst), 1);
    assert_eq!(counts.config.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
}
#[actix::test]
async fn cancelling_root_creation_does_not_abandon_pending_config_or_actor() {
    let sys = system().await;
    let counts = Counts::default();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut create = Box::pin(sys.spawn_root_typed(
        instance(&counts, None, false),
        config(&counts, Some(gate.clone()), false),
    ));
    tokio::select! { result=&mut create=>panic!("early creation: {result:?}"), _=gate.entered()=>{} }
    drop(create);
    assert!(sys.clone().shutdown().await.is_err());
    assert_eq!(sys.status().active_actors, 1);
    gate.release();
    stopped(&sys).await;
    assert_eq!(counts.actor.load(Ordering::SeqCst), 1);
    assert_eq!(counts.config.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
}
#[actix::test]
async fn parent_stop_keeps_pending_child_membership_until_config_cleanup_finishes() {
    let sys = system().await;
    let (parent, mut ctx) = create_parent(&sys).await;
    let counts = Counts::default();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let snapshot = ctx.clone();
    let spawner = ctx.spawner();
    let mut create = Box::pin(spawner.spawn_typed(
        instance(&counts, None, false),
        config(&counts, Some(gate.clone()), false),
    ));
    tokio::select! { result=&mut create=>panic!("early child creation: {result:?}"), _=gate.entered()=>{} }
    assert_eq!(snapshot.children().unwrap().read_all().len(), 1);
    assert!(parent.stop().await.is_err());
    assert_eq!(sys.status().active_actors, 2);
    assert_eq!(snapshot.children().unwrap().read_all().len(), 1);
    gate.release();
    assert!(create.await.is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.status().active_actors != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(counts.actor.load(Ordering::SeqCst), 1);
    assert_eq!(counts.config.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    sys.shutdown().await.unwrap();
}
#[actix::test]
async fn accepted_instance_keeps_native_reference_and_live_destructor_affinity() {
    let sys = system().await;
    let counts = Counts::default();
    let actor = sys
        .spawn_root_typed(instance(&counts, None, false), config(&counts, None, false))
        .await
        .unwrap();
    assert!(
        actor
            .as_any()
            .downcast_ref::<ActixActorRef<Instance>>()
            .unwrap()
            .get_addr()
            .connected()
    );
    assert_eq!(
        *actor
            .send(Box::new(()))
            .await
            .unwrap()
            .downcast::<usize>()
            .unwrap(),
        42
    );
    actor.stop().await.unwrap();
    assert_eq!(counts.actor.load(Ordering::SeqCst), 1);
    assert_eq!(counts.config.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
    sys.shutdown().await.unwrap();
}

#[actix::test]
async fn accepted_config_panic_retires_instance_without_running_init() {
    let sys = system().await;
    let counts = Counts::default();
    let result = std::panic::AssertUnwindSafe(
        sys.spawn_root_typed(instance(&counts, None, false), config(&counts, None, true)),
    )
    .catch_unwind()
    .await;
    assert!(result.is_ok(), "accepted config panic escaped creation");
    assert!(result.unwrap().is_err());
    assert_eq!(counts.actor.load(Ordering::SeqCst), 1);
    assert_eq!(counts.config.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(sys.status().active_actors, 0);
    sys.shutdown().await.unwrap();
}

#[actix::test]
async fn completed_config_cannot_initialize_late_when_executor_resumes_after_deadline() {
    let sys = system().await;
    let counts = Counts::default();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut create = Box::pin(sys.spawn_root_typed(
        instance(&counts, None, false),
        config(&counts, Some(gate.clone()), false),
    ));
    tokio::select! { result = &mut create => panic!("early creation: {result:?}"), _ = gate.entered() => {} }
    gate.release();
    // Deliberately stall this current-thread executor: config completion and
    // deadline become ready together before the owner can poll either branch.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(counts.config.load(Ordering::SeqCst), 1);
    let failed = create.await.is_err();
    let _ = sys.clone().shutdown().await;
    stopped(&sys).await;
    assert!(
        failed,
        "ready cleanup result must not bypass the elapsed creation deadline"
    );
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actor.load(Ordering::SeqCst), 1);
}
