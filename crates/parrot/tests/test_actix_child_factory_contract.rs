use parrot::actix::{ActixActor, ActixActorSystem, ActixContext, ActixSystemSettings};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorSpawnerExt, ActorTerminated},
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
struct Counts {
    panic_drop: std::sync::atomic::AtomicBool,
    hooked: AtomicUsize,
    made: AtomicUsize,
    initialized: AtomicUsize,
    dropped: AtomicUsize,
    terminated: AtomicUsize,
    watched: AtomicUsize,
    reasons: Mutex<Vec<parrot_api::supervisor::DeathReason>>,
}
struct Child {
    counts: Arc<Counts>,
    generation: usize,
    fail_initial: bool,
    hook_pending: bool,
    drop_gate: Option<Arc<Gate>>,
}
impl Drop for Child {
    fn drop(&mut self) {
        if let Some(gate) = &self.drop_gate {
            gate.block();
        }
        self.counts.dropped.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.counts.panic_drop.load(Ordering::SeqCst),
            "cohort output destructor failed"
        );
    }
}
impl ActixEngineExt for Child {}
impl Actor for Child {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert!(ctx.parent().is_some());
            self.counts.initialized.fetch_add(1, Ordering::SeqCst);
            assert!(
                !(self.fail_initial && self.generation == 0),
                "first init failure"
            );
            Ok(())
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.hooked.fetch_add(1, Ordering::SeqCst);
            if self.hook_pending {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<CrashTogether>() {
                msg.downcast::<CrashTogether>().unwrap().0.wait().await;
                panic!("simultaneous cohort failure");
            }

            assert!(!msg.is::<Crash>(), "factory child crash");
            if msg.is::<MakeSlowLeaf>() {
                let leaf = ctx
                    .spawner()
                    .spawn_typed(
                        SlowLeaf(*msg.downcast::<MakeSlowLeaf>().unwrap()),
                        EmptyConfig,
                    )
                    .await?;
                leaf.send(Box::new(())).await?;
                return Ok(Box::new(leaf) as BoxedMessage);
            }
            if msg.is::<OwnReference>() {
                return Ok(Box::new(ctx.get_self_ref()) as BoxedMessage);
            }
            if msg.is::<BlockStop>() {
                self.hook_pending = true;
                return Ok(Box::new(()) as BoxedMessage);
            }
            if msg.is::<EscalateFromChild>() {
                ctx.set_supervisor_strategy(SupervisorStrategyType::Default(
                    parrot_api::supervisor::DefaultStrategy::EscalateFailure,
                ));
                let leaf = ctx.spawner().spawn_typed(FailingLeaf, EmptyConfig).await?;
                let _ = leaf.send(Box::new(())).await;
                return Ok(Box::new(()) as BoxedMessage);
            }
            Ok(Box::new((self.generation, ctx.parent().unwrap().path())) as BoxedMessage)
        })
    }
}
struct Crash;
struct CrashTogether(Arc<tokio::sync::Barrier>);
struct OwnReference;
struct MakeSlowLeaf {
    gate: Arc<Gate>,
    dropped: Arc<AtomicUsize>,
}
struct SlowLeaf(MakeSlowLeaf);
impl Drop for SlowLeaf {
    fn drop(&mut self) {
        self.0.gate.block();
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActixEngineExt for SlowLeaf {}
impl Actor for SlowLeaf {
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
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(Box::new(()) as BoxedMessage) })
    }
}
struct BlockStop;
struct EscalateFromChild;
struct FailingLeaf;
impl ActixEngineExt for FailingLeaf {}
impl Actor for FailingLeaf {
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
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async {
            panic!("descendant escalation");
        })
    }
}
struct Children;
struct SetPolicy(SupervisorStrategyType);
struct FailConstruction {
    gate: Arc<Gate>,
    ready: tokio::sync::oneshot::Sender<BoxedActorRef>,
}
struct OwnContext;
struct MakeSendOnly;
struct SendOnlyChild {
    value: std::cell::Cell<usize>,
    drops: Arc<AtomicUsize>,
}
impl Drop for SendOnlyChild {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActixEngineExt for SendOnlyChild {}
impl Actor for SendOnlyChild {
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
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.value.set(self.value.get() + 1);
            Ok(Box::new(self.value.get()) as BoxedMessage)
        })
    }
}
struct Inherited(Arc<dyn Fn() -> Child + Send + Sync>);
struct FillCapacity {
    gate: Arc<Gate>,
    counts: Arc<Counts>,
    calls: Arc<AtomicUsize>,
    cancel: Arc<tokio::sync::Notify>,
    ready: tokio::sync::oneshot::Sender<usize>,
}
struct Create {
    factory: Arc<dyn Fn() -> Child + Send + Sync>,
    maximum: u32,
    dynamic: bool,
}
struct Parent(Arc<Counts>);
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
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<ActorTerminated>() {
                self.0
                    .reasons
                    .lock()
                    .unwrap()
                    .push(msg.downcast::<ActorTerminated>().unwrap().reason);
                self.0.watched.fetch_add(1, Ordering::SeqCst);
                return Ok(Box::new(()) as BoxedMessage);
            }
            if msg.is::<Children>() {
                return Ok(Box::new(
                    ctx.children()
                        .unwrap()
                        .read_all()
                        .iter()
                        .map(|child| child.path())
                        .collect::<Vec<_>>(),
                ) as BoxedMessage);
            }
            if msg.is::<MakeSendOnly>() {
                let drops = Arc::new(AtomicUsize::new(0));
                let observed = drops.clone();
                let child = ctx
                    .spawner()
                    .spawn_factory(move || SendOnlyChild {
                        value: std::cell::Cell::new(0),
                        drops: observed.clone(),
                    })
                    .await?;
                return Ok(Box::new((child, drops)) as BoxedMessage);
            }
            if msg.is::<OwnContext>() {
                return Ok(Box::new(ctx.clone()) as BoxedMessage);
            }
            if msg.is::<FailConstruction>() {
                let request = *msg.downcast::<FailConstruction>().unwrap();
                let snapshot = ctx.clone();
                let construction = ctx.spawner().spawn_factory(move || -> Child {
                    request.gate.block();
                    panic!("constructor diagnostic must survive observer backpressure");
                });
                tokio::pin!(construction);
                assert!(futures::poll!(&mut construction).is_pending());
                request
                    .ready
                    .send(snapshot.children().unwrap().read_all()[0].clone_boxed())
                    .unwrap();
                let child = construction.await?;
                return Ok(Box::new(child) as BoxedMessage);
            }
            if msg.is::<SetPolicy>() {
                ctx.set_supervisor_strategy(msg.downcast::<SetPolicy>().unwrap().0);
                return Ok(Box::new(()) as BoxedMessage);
            }
            if msg.is::<Inherited>() {
                let factory = msg.downcast::<Inherited>().unwrap().0;
                let child = ctx.spawner().spawn_factory(move || factory()).await?;
                return Ok(Box::new(child) as BoxedMessage);
            }
            if msg.is::<FillCapacity>() {
                use futures::StreamExt;
                let request = *msg.downcast::<FillCapacity>().unwrap();
                let spawner = ctx.spawner();
                let mut pending = futures::stream::FuturesUnordered::new();
                for _ in 0..=1024 {
                    let gate = request.gate.clone();
                    let counts = request.counts.clone();
                    let calls = request.calls.clone();
                    pending.push(spawner.spawn_factory(move || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        gate.block();
                        Child {
                            generation: counts.made.fetch_add(1, Ordering::SeqCst),
                            counts: counts.clone(),
                            fail_initial: false,
                            hook_pending: false,
                            drop_gate: None,
                        }
                    }));
                }
                let error = pending.next().await.unwrap().unwrap_err();
                assert!(error.to_string().contains("child capacity exhausted"));
                request.ready.send(1024).unwrap();
                request.cancel.notified().await;
                drop(pending);
                return Ok(Box::new(()) as BoxedMessage);
            }
            let request = *msg.downcast::<Create>().unwrap();
            let policy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: request.maximum,
                within: Duration::from_secs(30),
                decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
            });
            let factory = request.factory;
            let child = if request.dynamic {
                let ctx: &mut dyn ActorContext = ctx;
                ctx.spawner()
                    .spawn_factory_with_strategy(move || factory(), policy)
                    .await?
            } else {
                ctx.spawner()
                    .spawn_factory_with_strategy(move || factory(), policy)
                    .await?
            };
            Ok(Box::new(child) as BoxedMessage)
        })
    }
    fn handle_child_terminated<'a>(
        &'a mut self,
        _: BoxedActorRef,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.terminated.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
fn factory(counts: &Arc<Counts>, fail_initial: bool) -> Arc<dyn Fn() -> Child + Send + Sync> {
    let counts = counts.clone();
    Arc::new(move || Child {
        generation: counts.made.fetch_add(1, Ordering::SeqCst),
        counts: counts.clone(),
        fail_initial,
        hook_pending: false,
        drop_gate: None,
    })
}
async fn generation(child: &dyn parrot_api::address::ActorRef, expected: usize) -> String {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(result) = child.send(Box::new(())).await {
                let (generation, parent) = *result.downcast::<(usize, String)>().unwrap();
                if generation == expected {
                    return parent;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
async fn absent(sys: &ActixActorSystem, path: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while sys.get_actor(&path.to_owned()).await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[actix::test]
async fn concrete_and_dyn_actix_factory_keep_reference_parent_and_final_observers() {
    for dynamic in [false, true] {
        let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let counts = Arc::new(Counts::default());
        let parent = sys
            .spawn_root_typed_with_strategy(
                Parent(counts.clone()),
                EmptyConfig,
                parrot_api::supervisor::SupervisorStrategyType::Default(
                    parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
                ),
            )
            .await
            .unwrap();
        let child = *parent
            .send(Box::new(Create {
                factory: factory(&counts, false),
                maximum: 2,
                dynamic,
            }))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        child.watch(parent.clone_boxed()).await.unwrap();
        for number in 0..3 {
            assert_eq!(generation(&*child, number).await, parent.path());
            let paths = *parent
                .send(Box::new(Children))
                .await
                .unwrap()
                .downcast::<Vec<String>>()
                .unwrap();
            assert_eq!(paths, vec![child.path()]);
            assert_eq!(counts.terminated.load(Ordering::SeqCst), 0);
            assert_eq!(counts.watched.load(Ordering::SeqCst), 0);
            assert!(child.send(Box::new(Crash)).await.is_err());
        }
        absent(&sys, &child.path()).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while counts.terminated.load(Ordering::SeqCst) != 1
                || counts.watched.load(Ordering::SeqCst) != 1
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(counts.made.load(Ordering::SeqCst), 3);
        assert_eq!(counts.dropped.load(Ordering::SeqCst), 3);
        assert!(
            parent
                .send(Box::new(Children))
                .await
                .unwrap()
                .downcast::<Vec<String>>()
                .unwrap()
                .is_empty()
        );
        sys.shutdown().await.unwrap();
    }
}
#[actix::test]
async fn actix_factory_init_failure_restarts_and_zero_budget_stops() {
    for maximum in [0, 1] {
        let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let counts = Arc::new(Counts::default());
        let parent = sys
            .spawn_root_typed_with_strategy(
                Parent(counts.clone()),
                EmptyConfig,
                parrot_api::supervisor::SupervisorStrategyType::Default(
                    parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
                ),
            )
            .await
            .unwrap();
        let child = *parent
            .send(Box::new(Create {
                factory: factory(&counts, true),
                maximum,
                dynamic: false,
            }))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        if maximum == 1 {
            assert_eq!(generation(&*child, 1).await, parent.path());
            parent.stop().await.unwrap();
        }
        absent(&sys, &child.path()).await;
        assert_eq!(counts.made.load(Ordering::SeqCst), maximum as usize + 1);
        assert_eq!(counts.dropped.load(Ordering::SeqCst), maximum as usize + 1);
        sys.shutdown().await.unwrap();
    }
}
#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (_guard, timed) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(3), |v| !*v)
            .unwrap();
        assert!(!timed.timed_out());
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
#[actix::test]
async fn shutdown_waits_for_pending_actix_constructor_and_never_initializes_cancelled_output() {
    let sys = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await
    .unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let next = factory(&counts, false);
    let request = parent.send(Box::new(Create {
        factory: Arc::new(move || {
            blocked.block();
            next()
        }),
        maximum: 1,
        dynamic: true,
    }));
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    assert!(sys.clone().shutdown().await.is_err());
    let retained = sys.status().active_actors;
    let state = sys.status().state;
    gate.release();
    let _ = request.await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while sys.status().state != parrot_api::system::SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(state, parrot_api::system::SystemState::ShuttingDown);
    assert_eq!(retained, 2);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn logical_factory_reports_enforced_hook_deadline_as_killed() {
    let sys = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await
    .unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let child = *parent
        .send(Box::new(Create {
            factory: factory(&counts, false),
            maximum: 0,
            dynamic: false,
        }))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    generation(&*child, 0).await;
    child.watch(parent.clone_boxed()).await.unwrap();
    child.send(Box::new(BlockStop)).await.unwrap();
    assert!(child.stop().await.is_err());
    absent(&sys, &child.path()).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while counts.watched.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    sys.shutdown().await.unwrap();
    assert!(matches!(
        &counts.reasons.lock().unwrap()[0],
        parrot_api::supervisor::DeathReason::Killed
    ));
}

#[actix::test]
async fn logical_factory_preserves_descendant_escalation_reason() {
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let child = *parent
        .send(Box::new(Create {
            factory: factory(&counts, false),
            maximum: 0,
            dynamic: true,
        }))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    generation(&*child, 0).await;
    child.watch(parent.clone_boxed()).await.unwrap();
    let _ = child.send(Box::new(EscalateFromChild)).await;
    absent(&sys, &child.path()).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while counts.watched.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    sys.shutdown().await.unwrap();
    assert!(
        matches!(&counts.reasons.lock().unwrap()[0],parrot_api::supervisor::DeathReason::Escalated(reason) if reason.contains("descendant escalation"))
    );
}

#[actix::test]
async fn native_reference_from_retired_generation_cannot_stop_replacement() {
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let child = *parent
        .send(Box::new(Create {
            factory: factory(&counts, false),
            maximum: 1,
            dynamic: false,
        }))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    generation(&*child, 0).await;
    let obsolete = *child
        .send(Box::new(OwnReference))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    assert!(child.send(Box::new(Crash)).await.is_err());
    generation(&*child, 1).await;
    assert!(obsolete.stop().await.is_err());
    assert_eq!(generation(&*child, 1).await, parent.path());
    parent.stop().await.unwrap();
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
    sys.shutdown().await.unwrap();
}

#[actix::test]
async fn inherited_actix_factory_policy_uses_sliding_window_and_current_parent_setting() {
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    parent
        .send(Box::new(SetPolicy(SupervisorStrategyType::OneForOne(
            OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_millis(50),
                decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
            },
        ))))
        .await
        .unwrap();
    let child = *parent
        .send(Box::new(Inherited(factory(&counts, false))))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    generation(&*child, 0).await;
    assert!(child.send(Box::new(Crash)).await.is_err());
    generation(&*child, 1).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(child.send(Box::new(Crash)).await.is_err());
    generation(&*child, 2).await;
    parent
        .send(Box::new(SetPolicy(SupervisorStrategyType::Default(
            parrot_api::supervisor::DefaultStrategy::StopOnFailure,
        ))))
        .await
        .unwrap();
    assert!(child.send(Box::new(Crash)).await.is_err());
    absent(&sys, &child.path()).await;
    assert_eq!(counts.made.load(Ordering::SeqCst), 3);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 3);
    sys.shutdown().await.unwrap();
}

#[actix::test]
async fn actix_pending_factory_capacity_and_constructor_workers_are_bounded_and_reusable() {
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let cancel = Arc::new(tokio::sync::Notify::new());
    let (send, ready) = tokio::sync::oneshot::channel();
    let request = parent.send(Box::new(FillCapacity {
        gate: gate.clone(),
        counts: counts.clone(),
        calls: calls.clone(),
        cancel: cancel.clone(),
        ready: send,
    }));
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap(),
        1024
    );
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(sys.status().active_actors, 1025);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    cancel.notify_one();
    request.await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.status().active_actors != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "cancelled queued constructors must not execute"
    );
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.status().active_actors != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    let child = *parent
        .send(Box::new(Inherited(factory(&counts, false))))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    generation(&*child, 1).await;
    sys.shutdown().await.unwrap();
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
}
struct FactoryCapture(Arc<Gate>);
impl Drop for FactoryCapture {
    fn drop(&mut self) {
        self.0.block();
    }
}
#[actix::test]
async fn factory_capture_disposal_retains_actix_parent_and_logical_child() {
    let sys = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await
    .unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let capture = FactoryCapture(gate.clone());
    let next = factory(&counts, false);
    let child = *parent
        .send(Box::new(Inherited(Arc::new(move || {
            let _ = &capture;
            next()
        }))))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    generation(&*child, 0).await;
    let stopped = parent.stop().await;
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    let retained = sys.get_actor(&parent.path()).await.is_some()
        && sys.get_actor(&child.path()).await.is_some();
    gate.release();
    absent(&sys, &parent.path()).await;
    assert!(stopped.is_err());
    assert!(retained);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    sys.shutdown().await.unwrap();
}

#[actix::test]
async fn cancelled_actix_output_must_really_drop_before_parent_or_path_retirement() {
    let sys = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await
    .unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let constructor = Arc::new(Gate::default());
    let _release_constructor = Release(constructor.clone());
    let output = Arc::new(Gate::default());
    let _release_output = Release(output.clone());
    let blocked = constructor.clone();
    let dropped = output.clone();
    let next = factory(&counts, false);
    let request = parent.send(Box::new(Inherited(Arc::new(move || {
        blocked.block();
        let mut actor = next();
        actor.drop_gate = Some(dropped.clone());
        actor
    }))));
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    tokio::time::timeout(Duration::from_secs(2), constructor.entered.notified())
        .await
        .unwrap();
    assert!(parent.stop().await.is_err());
    constructor.release();
    tokio::time::timeout(Duration::from_secs(2), output.entered.notified())
        .await
        .unwrap();
    let retained = sys.status().active_actors == 2;
    let early_drop = counts.dropped.load(Ordering::SeqCst);
    output.release();
    let _ = request.await;
    absent(&sys, &parent.path()).await;
    assert!(retained);
    assert_eq!(early_drop, 0);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    sys.shutdown().await.unwrap();
}

#[actix::test]
async fn first_actix_constructor_diagnostic_precedes_full_parent_and_death_watch_notifications() {
    let sys = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        mailbox_capacity: 1,
        shutdown_timeout: Duration::from_millis(150),
        ..Default::default()
    })
    .await
    .unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (send, ready) = tokio::sync::oneshot::channel();
    let request = parent.send(Box::new(FailConstruction {
        gate: gate.clone(),
        ready: send,
    }));
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    let child = tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    child.watch(parent.clone_boxed()).await.unwrap();
    parent.deliver(Box::new(Children)).await.unwrap();
    gate.release();
    let result = request.await;
    absent(&sys, &child.path()).await;
    sys.shutdown().await.unwrap();
    assert!(
        matches!(result,Err(parrot_api::errors::ActorError::InitializationError(message)) if message.contains("constructor diagnostic must survive observer backpressure"))
    );
    assert_eq!(counts.made.load(Ordering::SeqCst), 0);
}
struct PanicCapture;
impl Drop for PanicCapture {
    fn drop(&mut self) {
        panic!("factory capture destructor failed");
    }
}
#[actix::test]
async fn actix_factory_capture_panic_is_a_cleanup_error_and_does_not_strand_membership() {
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let capture = PanicCapture;
    let next = factory(&counts, false);
    let child = *parent
        .send(Box::new(Inherited(Arc::new(move || {
            let _ = &capture;
            next()
        }))))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    generation(&*child, 0).await;
    child.watch(parent.clone_boxed()).await.unwrap();
    let result = child.stop().await;
    absent(&sys, &child.path()).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while counts.watched.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    sys.shutdown().await.unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("factory capture disposal failed")
    );
    assert!(
        matches!(&counts.reasons.lock().unwrap()[0],parrot_api::supervisor::DeathReason::Failed(reason) if reason.contains("factory capture disposal failed"))
    );
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
}
struct OwnedCapture {
    gate: Arc<Gate>,
    dropped: Arc<AtomicUsize>,
}
impl Drop for OwnedCapture {
    fn drop(&mut self) {
        self.gate.block();
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
#[actix::test]
async fn rejected_actix_factory_capture_cleanup_survives_waiter_cancellation() {
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let mut context = *parent
        .send(Box::new(OwnContext))
        .await
        .unwrap()
        .downcast::<ActixContext<ActixActor<Parent>>>()
        .unwrap();
    parent.stop().await.unwrap();
    let neighbor = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let dropped = Arc::new(AtomicUsize::new(0));
    let capture = OwnedCapture {
        gate: gate.clone(),
        dropped: dropped.clone(),
    };
    let next = factory(&counts, false);
    {
        let request = context.spawner().spawn_factory(move || {
            let _ = &capture;
            next()
        });
        tokio::pin!(request);
        assert!(futures::poll!(&mut request).is_pending());
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
    }
    assert!(
        neighbor
            .send(Box::new(Children))
            .await
            .unwrap()
            .downcast::<Vec<String>>()
            .unwrap()
            .is_empty()
    );
    assert_eq!(counts.made.load(Ordering::SeqCst), 0);
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), async {
        while dropped.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    sys.shutdown().await.unwrap();
}

#[actix::test]
async fn actix_replacement_constructor_follows_actual_descendant_disposal() {
    let sys = ActixActorSystem::with_arbiter_count(2).await.unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let leaf_drops = Arc::new(AtomicUsize::new(0));
    let observed = leaf_drops.clone();
    let made = counts.clone();
    let next = factory(&counts, false);
    let child = *parent
        .send(Box::new(Create {
            factory: Arc::new(move || {
                if made.made.load(Ordering::SeqCst) > 0 {
                    assert_eq!(
                        observed.load(Ordering::SeqCst),
                        1,
                        "replacement preceded old descendant destructor"
                    );
                }
                next()
            }),
            maximum: 1,
            dynamic: true,
        }))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    generation(&*child, 0).await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let old_leaf = *child
        .send(Box::new(MakeSlowLeaf {
            gate: gate.clone(),
            dropped: leaf_drops.clone(),
        }))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    assert!(child.send(Box::new(Crash)).await.is_err());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(counts.made.load(Ordering::SeqCst), 1);
    assert!(sys.get_actor(&child.path()).await.is_some());
    gate.release();
    assert_eq!(generation(&*child, 1).await, parent.path());
    old_leaf.stop().await.unwrap();
    assert_eq!(generation(&*child, 1).await, parent.path());
    sys.shutdown().await.unwrap();
    assert_eq!(leaf_drops.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
}

#[actix::test]
async fn expired_generation_cleanup_prevents_late_actix_reconstruction_without_claiming_kill() {
    let sys = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 2,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    })
    .await
    .unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let child = *parent
        .send(Box::new(Create {
            factory: factory(&counts, false),
            maximum: 1,
            dynamic: true,
        }))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    generation(&*child, 0).await;
    child.watch(parent.clone_boxed()).await.unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let leaf_drops = Arc::new(AtomicUsize::new(0));
    child
        .send(Box::new(MakeSlowLeaf {
            gate: gate.clone(),
            dropped: leaf_drops.clone(),
        }))
        .await
        .unwrap();
    assert!(child.send(Box::new(Crash)).await.is_err());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    gate.release();
    let stopped = tokio::time::timeout(Duration::from_millis(300), async {
        while sys.get_actor(&child.path()).await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    if stopped {
        tokio::time::timeout(Duration::from_secs(2), async {
            while counts.watched.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    sys.shutdown().await.unwrap();
    assert!(
        stopped,
        "recovery began after the failed generation's cleanup deadline expired"
    );
    assert_eq!(counts.made.load(Ordering::SeqCst), 1);
    assert_eq!(leaf_drops.load(Ordering::SeqCst), 1);
    assert!(
        matches!(&counts.reasons.lock().unwrap()[0],parrot_api::supervisor::DeathReason::Panic(reason) if reason.contains("factory child crash"))
    );
}

// Append to the Actix child-factory contract fixture for independent execution.
struct CaptureThread(Arc<Mutex<Option<std::thread::ThreadId>>>);
impl Drop for CaptureThread {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = Some(std::thread::current().id());
    }
}
#[actix::test]
async fn retired_system_factory_rejection_keeps_capture_disposal_off_the_caller_executor() {
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let mut context = *parent
        .send(Box::new(OwnContext))
        .await
        .unwrap()
        .downcast::<ActixContext<ActixActor<Parent>>>()
        .unwrap();
    sys.shutdown().await.unwrap();
    let disposed_on = Arc::new(Mutex::new(None));
    let captured = CaptureThread(disposed_on.clone());
    let next = factory(&counts, false);
    let result = context
        .spawner()
        .spawn_factory(move || {
            let _ = &captured;
            next()
        })
        .await;
    assert!(matches!(
        result,
        Err(parrot_api::errors::ActorError::Stopped)
    ));
    assert_eq!(counts.made.load(Ordering::SeqCst), 0);
    let disposed_on = disposed_on
        .lock()
        .unwrap()
        .expect("factory captures must be disposed before rejection receipt");
    assert_ne!(
        disposed_on,
        std::thread::current().id(),
        "rejected factory capture destructor ran on the caller's executor thread"
    );
}
#[actix::test]
async fn retired_system_factory_rejection_contains_capture_destructor_panic() {
    use futures::FutureExt;
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let mut context = *parent
        .send(Box::new(OwnContext))
        .await
        .unwrap()
        .downcast::<ActixContext<ActixActor<Parent>>>()
        .unwrap();
    sys.shutdown().await.unwrap();
    let captured = PanicCapture;
    let next = factory(&counts, false);
    let result = std::panic::AssertUnwindSafe(context.spawner().spawn_factory(move || {
        let _ = &captured;
        next()
    }))
    .catch_unwind()
    .await;
    assert!(
        matches!(result, Ok(Err(_))),
        "factory rejection leaked a capture destructor panic into its caller"
    );
    assert_eq!(counts.made.load(Ordering::SeqCst), 0);
}

#[actix::test]
async fn retired_system_rejection_keeps_cleanup_owned_after_the_waiter_is_cancelled() {
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let mut context = *parent
        .send(Box::new(OwnContext))
        .await
        .unwrap()
        .downcast::<ActixContext<ActixActor<Parent>>>()
        .unwrap();
    sys.shutdown().await.unwrap();
    let neighbor_system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let neighbor = neighbor_system
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let dropped = Arc::new(AtomicUsize::new(0));
    let captured = OwnedCapture {
        gate: gate.clone(),
        dropped: dropped.clone(),
    };
    let next = factory(&counts, false);
    {
        let request = context.spawner().spawn_factory(move || {
            let _ = &captured;
            next()
        });
        tokio::pin!(request);
        assert!(futures::poll!(&mut request).is_pending());
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
    }
    neighbor.send(Box::new(Children)).await.unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    assert_eq!(counts.made.load(Ordering::SeqCst), 0);
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), async {
        while dropped.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    neighbor_system.shutdown().await.unwrap();
}
struct CountPanicCapture(Arc<AtomicUsize>);
impl Drop for CountPanicCapture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("rejected payload or policy capture panic");
    }
}
#[actix::test]
async fn wrong_factory_payload_disposes_both_panicking_payload_and_strategy() {
    use futures::FutureExt;
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let counts = Arc::new(Counts::default());
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(counts.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let mut context = *parent
        .send(Box::new(OwnContext))
        .await
        .unwrap()
        .downcast::<ActixContext<ActixActor<Parent>>>()
        .unwrap();
    let payload_drops = Arc::new(AtomicUsize::new(0));
    let policy_drops = Arc::new(AtomicUsize::new(0));
    let captured = CountPanicCapture(policy_drops.clone());
    let strategy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(1),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &captured;
            SupervisionDecision::Restart
        }),
    });
    let result = std::panic::AssertUnwindSafe(context.spawner().spawn_factory_erased(
        Box::new(CountPanicCapture(payload_drops.clone())),
        Some(strategy),
    ))
    .catch_unwind()
    .await;
    sys.shutdown().await.unwrap();
    assert!(matches!(result, Ok(Err(_))));
    assert_eq!(payload_drops.load(Ordering::SeqCst), 1);
    assert_eq!(policy_drops.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn actix_factory_preserves_send_only_actor_support() {
    let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(Arc::new(Counts::default())),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let (child, drops) = *parent
        .send(Box::new(MakeSendOnly))
        .await
        .unwrap()
        .downcast::<(BoxedActorRef, Arc<AtomicUsize>)>()
        .unwrap();
    assert_eq!(
        *child
            .send(Box::new(()))
            .await
            .unwrap()
            .downcast::<usize>()
            .unwrap(),
        1
    );
    assert_eq!(
        *child
            .send(Box::new(()))
            .await
            .unwrap()
            .downcast::<usize>()
            .unwrap(),
        2
    );
    parent.stop().await.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    sys.shutdown().await.unwrap();
}

#[actix::test]
async fn group_preparation_barrier_and_abort_retain_every_member() {
    {
        for outcome in 0..4 {
            let counts = Arc::new(Counts::default());
            let sys = ActixActorSystem::with_settings(ActixSystemSettings {
                arbiter_count: 2,
                ..Default::default()
            })
            .await
            .unwrap();
            let parent = sys
                .spawn_root_typed_with_strategy(
                    Parent(counts.clone()),
                    EmptyConfig,
                    parrot_api::supervisor::SupervisorStrategyType::Default(
                        parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
                    ),
                )
                .await
                .unwrap();
            parent
                .send(Box::new(SetPolicy(SupervisorStrategyType::OneForAll(
                    parrot_api::supervisor::DefaultSupervisorStrategyFactory::one_for_all(
                        2,
                        Duration::from_secs(30),
                    ),
                ))))
                .await
                .unwrap();
            let first_counts = Arc::new(Counts::default());
            let second_counts = Arc::new(Counts::default());
            let gate = Arc::new(Gate::default());
            let _release = Release(gate.clone());
            let next = factory(&first_counts, false);
            let blocked = gate.clone();
            let seen = first_counts.clone();
            let build: Arc<dyn Fn() -> Child + Send + Sync> = Arc::new(move || {
                if seen.made.load(Ordering::SeqCst) != 0 {
                    blocked.block();
                    assert_ne!(
                        outcome, 2,
                        "replacement constructor failed after sibling prepared"
                    );
                }
                next()
            });
            let first = *parent
                .send(Box::new(Inherited(build)))
                .await
                .unwrap()
                .downcast::<BoxedActorRef>()
                .unwrap();
            let next = factory(&second_counts, false);
            let old_first = first_counts.clone();
            let old_second = second_counts.clone();
            let build: Arc<dyn Fn() -> Child + Send + Sync> = Arc::new(move || {
                if old_second.made.load(Ordering::SeqCst) != 0 {
                    assert_eq!(
                        old_first.dropped.load(Ordering::SeqCst),
                        1,
                        "all old actors must retire before any replacement constructor"
                    );
                    assert_eq!(old_second.dropped.load(Ordering::SeqCst), 1);
                }
                next()
            });
            let second = *parent
                .send(Box::new(Inherited(build)))
                .await
                .unwrap()
                .downcast::<BoxedActorRef>()
                .unwrap();
            generation(&*first, 0).await;
            generation(&*second, 0).await;
            let _ = first.send(Box::new(Crash)).await;
            tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while second_counts.made.load(Ordering::SeqCst) != 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(first_counts.initialized.load(Ordering::SeqCst), 1);
            assert_eq!(
                second_counts.initialized.load(Ordering::SeqCst),
                1,
                "prepared sibling must not initialize independently"
            );
            assert_eq!(
                second_counts.hooked.load(Ordering::SeqCst),
                1,
                "healthy sibling executes its lifecycle hook"
            );
            assert_eq!(counts.terminated.load(Ordering::SeqCst), 0);
            let stopped = first.stop();
            tokio::pin!(stopped);
            if outcome == 1 || outcome == 3 {
                assert!(futures::poll!(&mut stopped).is_pending());
            }
            if outcome == 3 {
                first_counts.panic_drop.store(true, Ordering::SeqCst);
            }
            gate.release();
            if outcome == 0 {
                generation(&*first, 1).await;
                generation(&*second, 1).await;
                assert_eq!(counts.terminated.load(Ordering::SeqCst), 0);
            } else {
                if outcome == 1 {
                    stopped
                        .as_mut()
                        .await
                        .expect("explicit stop with successful actual disposal must succeed");
                }
                if outcome == 3 {
                    let error = stopped.as_mut().await.expect_err(
                        "real disposal failure cannot be normalized into successful cancellation",
                    );
                    assert!(
                        error
                            .to_string()
                            .contains("cohort output destructor failed"),
                        "{error}"
                    );
                }
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        let children = *parent
                            .send(Box::new(Children))
                            .await
                            .unwrap()
                            .downcast::<Vec<String>>()
                            .unwrap();
                        if children.is_empty() {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(first_counts.initialized.load(Ordering::SeqCst), 1);
                assert_eq!(second_counts.initialized.load(Ordering::SeqCst), 1);
                assert_eq!(
                    first_counts.dropped.load(Ordering::SeqCst),
                    first_counts.made.load(Ordering::SeqCst)
                );
                assert_eq!(
                    second_counts.dropped.load(Ordering::SeqCst),
                    2,
                    "prepared output must actually be discarded"
                );
            }
            sys.shutdown().await.unwrap();
        }
    }
}

#[actix::test]
async fn group_restart_budget_is_shared_when_failures_alternate_between_siblings() {
    {
        let counts = Arc::new(Counts::default());
        let sys = ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 2,
            ..Default::default()
        })
        .await
        .unwrap();
        let parent = sys
            .spawn_root_typed_with_strategy(
                Parent(counts.clone()),
                EmptyConfig,
                parrot_api::supervisor::SupervisorStrategyType::Default(
                    parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
                ),
            )
            .await
            .unwrap();
        parent
            .send(Box::new(SetPolicy(SupervisorStrategyType::OneForAll(
                parrot_api::supervisor::DefaultSupervisorStrategyFactory::one_for_all(
                    2,
                    Duration::from_secs(30),
                ),
            ))))
            .await
            .unwrap();
        let first_counts = Arc::new(Counts::default());
        let second_counts = Arc::new(Counts::default());
        let build = factory(&first_counts, false);
        let first = *parent
            .send(Box::new(Inherited(build)))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        let build = factory(&second_counts, false);
        let second = *parent
            .send(Box::new(Inherited(build)))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        generation(&*first, 0).await;
        generation(&*second, 0).await;
        for (child, expected_generation) in [(&first, 1), (&second, 2)] {
            let _ = child.send(Box::new(Crash)).await;
            generation(&*first, expected_generation).await;
            generation(&*second, expected_generation).await;
            assert_eq!(counts.terminated.load(Ordering::SeqCst), 0);
        }
        let _ = first.send(Box::new(Crash)).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let children = *parent
                    .send(Box::new(Children))
                    .await
                    .unwrap()
                    .downcast::<Vec<String>>()
                    .unwrap();
                if children.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(first_counts.made.load(Ordering::SeqCst), 3);
        assert_eq!(second_counts.made.load(Ordering::SeqCst), 3);
        assert_eq!(first_counts.dropped.load(Ordering::SeqCst), 3);
        assert_eq!(second_counts.dropped.load(Ordering::SeqCst), 3);
        sys.shutdown().await.unwrap();
    }
}

#[actix::test]
async fn group_larger_than_constructor_pool_recovers_simultaneous_failures_once() {
    {
        let counts = Arc::new(Counts::default());
        let sys = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let parent = sys
            .spawn_root_typed_with_strategy(
                Parent(counts.clone()),
                EmptyConfig,
                parrot_api::supervisor::SupervisorStrategyType::Default(
                    parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
                ),
            )
            .await
            .unwrap();
        parent
            .send(Box::new(SetPolicy(SupervisorStrategyType::OneForAll(
                parrot_api::supervisor::DefaultSupervisorStrategyFactory::one_for_all(
                    1,
                    Duration::from_secs(30),
                ),
            ))))
            .await
            .unwrap();
        let mut cohort = Vec::new();
        for _ in 0..5 {
            let member = Arc::new(Counts::default());
            let build = factory(&member, false);
            let child = *parent
                .send(Box::new(Inherited(build)))
                .await
                .unwrap()
                .downcast::<BoxedActorRef>()
                .unwrap();
            generation(&*child, 0).await;
            child.watch(parent.clone_boxed()).await.unwrap();
            cohort.push((child, member));
        }
        let together = Arc::new(tokio::sync::Barrier::new(2));
        let (a, b) = tokio::join!(
            cohort[0].0.send(Box::new(CrashTogether(together.clone()))),
            cohort[1].0.send(Box::new(CrashTogether(together)))
        );
        assert!(a.is_err() && b.is_err());
        for (child, member) in &cohort {
            generation(&**child, 1).await;
            assert_eq!(member.made.load(Ordering::SeqCst), 2);
            assert_eq!(member.dropped.load(Ordering::SeqCst), 1);
        }
        assert_eq!(counts.terminated.load(Ordering::SeqCst), 0);
        assert_eq!(counts.watched.load(Ordering::SeqCst), 0);
        sys.shutdown().await.unwrap();
        for (_, member) in cohort {
            assert_eq!(member.dropped.load(Ordering::SeqCst), 2);
        }
    }
}

#[actix::test]
async fn group_recovery_includes_a_sibling_still_in_its_first_constructor() {
    {
        let counts = Arc::new(Counts::default());
        let sys = ActixActorSystem::with_arbiter_count(2).await.unwrap();
        let parent = sys
            .spawn_root_typed_with_strategy(
                Parent(counts.clone()),
                EmptyConfig,
                parrot_api::supervisor::SupervisorStrategyType::Default(
                    parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
                ),
            )
            .await
            .unwrap();
        parent
            .send(Box::new(SetPolicy(SupervisorStrategyType::OneForAll(
                parrot_api::supervisor::DefaultSupervisorStrategyFactory::one_for_all(
                    1,
                    Duration::from_secs(30),
                ),
            ))))
            .await
            .unwrap();
        let first_counts = Arc::new(Counts::default());
        let second_counts = Arc::new(Counts::default());
        let build = factory(&first_counts, false);
        let first = *parent
            .send(Box::new(Inherited(build)))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        generation(&*first, 0).await;
        let healthy_counts = Arc::new(Counts::default());
        let build = factory(&healthy_counts, false);
        let healthy = *parent
            .send(Box::new(Inherited(build)))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        generation(&*healthy, 0).await;
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let next = factory(&second_counts, false);
        let blocked = gate.clone();
        let build: Arc<dyn Fn() -> Child + Send + Sync> = Arc::new(move || {
            let actor = next();
            if actor.generation == 0 {
                blocked.block();
            }
            actor
        });
        let pending = parent.send(Box::new(Inherited(build)));
        tokio::pin!(pending);
        assert!(futures::poll!(&mut pending).is_pending());
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        let _ = first.send(Box::new(Crash)).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while first_counts.dropped.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            first_counts.made.load(Ordering::SeqCst),
            1,
            "pending old output is part of the retirement barrier"
        );
        // Closure of a previously live healthy sibling proves the parent has
        // selected the cohort, rather than merely disposing the failed actor.
        tokio::time::timeout(Duration::from_secs(2), async {
            while healthy.is_alive().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        gate.release();
        let second = *tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        generation(&*first, 1).await;
        generation(&*second, 1).await;
        assert_eq!(first_counts.initialized.load(Ordering::SeqCst), 2);
        assert_eq!(
            second_counts.initialized.load(Ordering::SeqCst),
            1,
            "cancelled first output must never initialize"
        );
        assert_eq!(counts.terminated.load(Ordering::SeqCst), 0);
        sys.shutdown().await.unwrap();
        assert_eq!(first_counts.dropped.load(Ordering::SeqCst), 2);
        assert_eq!(second_counts.dropped.load(Ordering::SeqCst), 2);
    }
}

#[actix::test]
async fn group_public_deadline_keeps_constructor_ownership_and_child_admission_closed() {
    {
        let counts = Arc::new(Counts::default());
        let sys = ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 2,
            shutdown_timeout: Duration::from_millis(150),
            ..Default::default()
        })
        .await
        .unwrap();
        let parent = sys
            .spawn_root_typed_with_strategy(
                Parent(counts.clone()),
                EmptyConfig,
                parrot_api::supervisor::SupervisorStrategyType::Default(
                    parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
                ),
            )
            .await
            .unwrap();
        parent
            .send(Box::new(SetPolicy(SupervisorStrategyType::OneForAll(
                parrot_api::supervisor::DefaultSupervisorStrategyFactory::one_for_all(
                    1,
                    Duration::from_secs(30),
                ),
            ))))
            .await
            .unwrap();
        let first_counts = Arc::new(Counts::default());
        let second_counts = Arc::new(Counts::default());
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let next = factory(&first_counts, false);
        let blocked = gate.clone();
        let build: Arc<dyn Fn() -> Child + Send + Sync> = Arc::new(move || {
            let actor = next();
            if actor.generation == 1 {
                blocked.block();
            }
            actor
        });
        let first = *parent
            .send(Box::new(Inherited(build)))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        let build = factory(&second_counts, false);
        let second = *parent
            .send(Box::new(Inherited(build)))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        generation(&*first, 0).await;
        generation(&*second, 0).await;
        let _ = first.send(Box::new(Crash)).await;
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), first.stop())
            .await
            .unwrap()
            .expect_err("a blocked constructor is not completed disposal");
        assert!(
            matches!(
                error,
                parrot_api::errors::ActorError::Timeout
                    | parrot_api::errors::ActorError::TimeoutDetail(_)
            ),
            "{error}"
        );
        assert_eq!(first_counts.dropped.load(Ordering::SeqCst), 1);
        let children = *parent
            .send(Box::new(Children))
            .await
            .unwrap()
            .downcast::<Vec<String>>()
            .unwrap();
        assert!(
            children.contains(&first.path()),
            "public deadline must not remove the live constructor owner"
        );
        let rejected_counts = Arc::new(Counts::default());
        let build = factory(&rejected_counts, false);
        let rejection = parent
            .send(Box::new(Inherited(build)))
            .await
            .expect_err("cohort admission remains closed until actual retirement");
        assert!(rejection.to_string().contains("child group"), "{rejection}");
        assert_eq!(rejected_counts.made.load(Ordering::SeqCst), 0);
        gate.release();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let children = *parent
                    .send(Box::new(Children))
                    .await
                    .unwrap()
                    .downcast::<Vec<String>>()
                    .unwrap();
                if children.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(first_counts.initialized.load(Ordering::SeqCst), 1);
        assert_eq!(second_counts.initialized.load(Ordering::SeqCst), 1);
        assert_eq!(first_counts.dropped.load(Ordering::SeqCst), 2);
        let build = factory(&rejected_counts, false);
        let replacement = *parent
            .send(Box::new(Inherited(build)))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        generation(&*replacement, 0).await;
        sys.shutdown().await.unwrap();
    }
}

#[actix::test]
async fn cohort_joins_interrupted_decider_before_replacement_and_never_reuses_its_result() {
    use parrot_api::supervisor::OneForAllStrategy;
    let sys = ActixActorSystem::with_arbiter_count(2).await.unwrap();
    let parent = sys
        .spawn_root_typed_with_strategy(
            Parent(Arc::new(Counts::default())),
            EmptyConfig,
            SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    let mut ctx = *parent
        .send(Box::new(OwnContext))
        .await
        .unwrap()
        .downcast::<ActixContext<ActixActor<Parent>>>()
        .unwrap();
    ctx.set_supervisor_strategy(SupervisorStrategyType::OneForAll(OneForAllStrategy {
        max_restarts: 1,
        within: Duration::from_secs(30),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    }));
    let first_counts = Arc::new(Counts::default());
    let first_factory = factory(&first_counts, false);
    let first = ctx
        .spawner()
        .spawn_factory(move || first_factory())
        .await
        .unwrap();
    let second_counts = Arc::new(Counts::default());
    let second_factory = factory(&second_counts, false);
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    let second = ctx
        .spawner()
        .spawn_factory_with_strategy(
            move || second_factory(),
            SupervisorStrategyType::OneForAll(OneForAllStrategy {
                max_restarts: 1,
                within: Duration::from_secs(30),
                decider: BasicDecisionFn::new(move |error| {
                    assert!(matches!(error, parrot_api::errors::ActorError::Panic(_)));
                    if called.fetch_add(1, Ordering::SeqCst) == 0 {
                        blocked.block();
                    }
                    SupervisionDecision::Restart
                }),
            }),
        )
        .await
        .unwrap();
    generation(&*first, 0).await;
    generation(&*second, 0).await;
    let pending = second.send(Box::new(Crash));
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    assert!(first.send(Box::new(Crash)).await.is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(40), async {
            while first_counts.made.load(Ordering::SeqCst) == 1
                && second_counts.made.load(Ordering::SeqCst) == 1
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "new constructors ran while old failure callback still owned its decision"
    );
    gate.release();
    assert!(pending.await.is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while first_counts.initialized.load(Ordering::SeqCst) != 2
            || second_counts.initialized.load(Ordering::SeqCst) != 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    generation(&*first, 1).await;
    generation(&*second, 1).await;
    assert!(second.send(Box::new(Crash)).await.is_err());
    absent(&sys, &first.path()).await;
    absent(&sys, &second.path()).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "new generation reused an old supervision result"
    );
    assert_eq!(first_counts.made.load(Ordering::SeqCst), 2);
    assert_eq!(second_counts.made.load(Ordering::SeqCst), 2);
    parent.stop().await.unwrap();
    sys.shutdown().await.unwrap();
}
