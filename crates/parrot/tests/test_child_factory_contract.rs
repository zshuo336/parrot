//! Repeatable child constructors must retain real parent ownership across generations.
use parrot::thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContext, ActorSpawnerExt},
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
    initialized: AtomicUsize,
    hooked: AtomicUsize,
    made: AtomicUsize,
    dropped: AtomicUsize,
    decisions: AtomicUsize,
    terminated: AtomicUsize,
    watched: AtomicUsize,
}
struct Child {
    generation: usize,
    counts: Arc<Counts>,
    fail_init: bool,
}
impl Drop for Child {
    fn drop(&mut self) {
        self.counts.dropped.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.counts.panic_drop.load(Ordering::SeqCst),
            "cohort output destructor failed"
        );
    }
}
struct Crash;
struct CrashTogether(Arc<tokio::sync::Barrier>);
struct MakeLeaf(Arc<AtomicUsize>);
struct Leaf(Arc<AtomicUsize>);
impl Drop for Leaf {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Leaf {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(Box::new(ctx.parent().unwrap().path()) as BoxedMessage) })
    }
}
struct Children;
struct CancelConstruction {
    factory: Arc<dyn Fn() -> Child + Send + Sync>,
    cancel: Arc<tokio::sync::Notify>,
}
struct FillCapacity {
    gate: Arc<Gate>,
    counts: Arc<Counts>,
    calls: Arc<AtomicUsize>,
    cancel: Arc<tokio::sync::Notify>,
    ready: tokio::sync::oneshot::Sender<usize>,
}
struct ChildRequest {
    factory: Arc<dyn Fn() -> Child + Send + Sync>,
    strategy: Option<SupervisorStrategyType>,
    dynamic: bool,
}
struct SetPolicy(SupervisorStrategyType);
struct StopAndReject(ChildRequest);
struct Parent(Arc<Counts>);
impl Actor for Child {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.initialized.fetch_add(1, Ordering::SeqCst);
            assert_eq!(ctx.parent().unwrap().path(), "/parent");
            assert!(!self.fail_init, "initial child generation failed");
            Ok(())
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.hooked.fetch_add(1, Ordering::SeqCst);
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

            assert!(!msg.is::<Crash>(), "child failure");
            if msg.is::<MakeLeaf>() {
                let leaf = ctx
                    .spawner()
                    .spawn_typed(Leaf(msg.downcast::<MakeLeaf>().unwrap().0), EmptyConfig)
                    .await?;
                return Ok(Box::new(leaf) as BoxedMessage);
            }
            Ok(Box::new((self.generation, ctx.parent().unwrap().path())) as BoxedMessage)
        })
    }
}
impl Actor for Parent {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<parrot_api::context::ActorTerminated>() {
                self.0.watched.fetch_add(1, Ordering::SeqCst);
                return Ok(Box::new(()) as BoxedMessage);
            }
            if msg.is::<CancelConstruction>() {
                let request = *msg.downcast::<CancelConstruction>().unwrap();
                let construction = ctx.spawn_factory(move || (request.factory)());
                tokio::pin!(construction);
                tokio::select! {
                    result = &mut construction => { result?; },
                    _ = request.cancel.notified() => {},
                }
                return Ok(Box::new(()) as BoxedMessage);
            }
            if msg.is::<FillCapacity>() {
                use futures::StreamExt;
                let request = *msg.downcast::<FillCapacity>().unwrap();
                let mut pending = futures::stream::FuturesUnordered::new();
                for _ in 0..=1024 {
                    let gate = request.gate.clone();
                    let counts = request.counts.clone();
                    let calls = request.calls.clone();
                    pending.push(ctx.spawn_factory(move || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        gate.block();
                        Child {
                            generation: counts.made.fetch_add(1, Ordering::SeqCst),
                            counts: counts.clone(),
                            fail_init: false,
                        }
                    }));
                }
                let error = pending.next().await.unwrap().unwrap_err();
                assert!(error.to_string().contains("child capacity exhausted"));
                request.ready.send(ctx.children().unwrap().len()).unwrap();
                request.cancel.notified().await;
                drop(pending);
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
            if msg.is::<SetPolicy>() {
                ctx.set_supervisor_strategy(msg.downcast::<SetPolicy>().unwrap().0);
                return Ok(Box::new(()) as BoxedMessage);
            }
            let request = if msg.is::<StopAndReject>() {
                ctx.stop().await?;
                msg.downcast::<StopAndReject>().unwrap().0
            } else {
                *msg.downcast::<ChildRequest>().unwrap()
            };
            let factory = request.factory;
            let actor = if request.dynamic {
                let context: &mut dyn ActorContext = ctx;
                match request.strategy {
                    Some(strategy) => {
                        context
                            .spawner()
                            .spawn_factory_with_strategy(move || factory(), strategy)
                            .await?
                    }
                    None => context.spawner().spawn_factory(move || factory()).await?,
                }
            } else {
                match request.strategy {
                    Some(strategy) => {
                        ctx.spawn_factory_with_strategy(move || factory(), strategy)
                            .await?
                    }
                    None => ctx.spawn_factory(move || factory()).await?,
                }
            };
            Ok(Box::new(actor) as BoxedMessage)
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
fn policy(counts: &Arc<Counts>, maximum: u32, within: Duration) -> SupervisorStrategyType {
    let counts = counts.clone();
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: maximum,
        within,
        decider: BasicDecisionFn::new(move |_| {
            counts.decisions.fetch_add(1, Ordering::SeqCst);
            SupervisionDecision::Restart
        }),
    })
}
fn factory(counts: &Arc<Counts>, fail_initial: bool) -> Arc<dyn Fn() -> Child + Send + Sync> {
    let counts = counts.clone();
    Arc::new(move || {
        let generation = counts.made.fetch_add(1, Ordering::SeqCst);
        Child {
            generation,
            counts: counts.clone(),
            fail_init: fail_initial && generation == 0,
        }
    })
}
async fn response(actor: &dyn ActorRef, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(reply) = actor
                .send_with_timeout(Box::new(()), Some(Duration::from_millis(50)))
                .await
            {
                let (generation, parent) = *reply.downcast::<(usize, String)>().unwrap();
                assert_eq!(parent, "/parent");
                assert!(generation <= expected, "extra restart");
                if generation == expected {
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn parent(sys: &Arc<ThreadActorSystem>, counts: &Arc<Counts>) -> BoxedActorRef {
    sys.spawn_at(
        Parent(counts.clone()),
        "/parent",
        None,
        parrot::thread::ThreadActorConfig {
            supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Resume),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .clone_boxed()
}
async fn spawn(parent: &dyn ActorRef, request: ChildRequest) -> BoxedActorRef {
    *parent
        .send(Box::new(request))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap()
}
async fn retired(sys: &ThreadActorSystem, path: &str, counts: &Counts) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.get_actor_ref(path).is_some() || counts.terminated.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn common_child_factory_keeps_reference_parent_and_budget_in_all_thread_modes() {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 2,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "family".into(),
        },
    ] {
        let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
            default_scheduling_mode: mode.clone(),
            ..Default::default()
        });
        let counts = Arc::new(Counts::default());
        let parent = parent(&sys, &counts).await;
        let child = spawn(
            &*parent,
            ChildRequest {
                factory: factory(&counts, false),
                strategy: Some(policy(&counts, 2, Duration::from_secs(30))),
                dynamic: true,
            },
        )
        .await;
        child.watch(parent.clone_boxed()).await.unwrap();
        for generation in 0..3 {
            response(&*child, generation).await;
            let children = *parent
                .send(Box::new(Children))
                .await
                .unwrap()
                .downcast::<Vec<String>>()
                .unwrap();
            assert_eq!(children, vec![child.path()]);
            assert_eq!(
                counts.terminated.load(Ordering::SeqCst),
                0,
                "replacement is not logical death"
            );
            assert_eq!(counts.watched.load(Ordering::SeqCst), 0);
            let _ = child.send(Box::new(Crash)).await;
        }
        retired(&sys, &child.path(), &counts).await;
        assert_eq!(counts.made.load(Ordering::SeqCst), 3);
        assert_eq!(counts.dropped.load(Ordering::SeqCst), 3);
        assert_eq!(counts.decisions.load(Ordering::SeqCst), 3);
        assert_eq!(counts.terminated.load(Ordering::SeqCst), 1);
        tokio::time::timeout(Duration::from_secs(2), async {
            while counts.watched.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(counts.watched.load(Ordering::SeqCst), 1);
        sys.shutdown_internal().await.unwrap();
    }
}
#[tokio::test]
async fn inherited_factory_policy_recovers_initialization_failure() {
    let sys = ThreadActorSystem::shared(Default::default());
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
    parent
        .send(Box::new(SetPolicy(policy(
            &counts,
            1,
            Duration::from_secs(30),
        ))))
        .await
        .unwrap();
    let child = spawn(
        &*parent,
        ChildRequest {
            factory: factory(&counts, true),
            strategy: None,
            dynamic: false,
        },
    )
    .await;
    response(&*child, 1).await;
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(counts.terminated.load(Ordering::SeqCst), 0);
    parent.stop().await.unwrap();
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn child_restart_budget_expires_as_a_sliding_window() {
    let sys = ThreadActorSystem::shared(Default::default());
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
    let child = spawn(
        &*parent,
        ChildRequest {
            factory: factory(&counts, false),
            strategy: Some(policy(&counts, 1, Duration::from_millis(60))),
            dynamic: true,
        },
    )
    .await;
    response(&*child, 0).await;
    let _ = child.send(Box::new(Crash)).await;
    response(&*child, 1).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let _ = child.send(Box::new(Crash)).await;
    response(&*child, 2).await;
    assert_eq!(counts.terminated.load(Ordering::SeqCst), 0);
    parent.stop().await.unwrap();
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 3);
    sys.shutdown_internal().await.unwrap();
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
        let (_guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(3), |open| {
                !*open
            })
            .unwrap();
        assert!(!timeout.timed_out());
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
#[tokio::test]
async fn parent_stop_cancels_pending_child_but_waits_for_real_constructor_retirement() {
    let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    });
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let build = factory(&counts, false);
    let blocked = gate.clone();
    let waiting = parent.send(Box::new(ChildRequest {
        factory: Arc::new(move || {
            blocked.block();
            build()
        }),
        strategy: None,
        dynamic: true,
    }));
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(sys.actor_count(), 2);
    assert!(
        parent.stop().await.is_err(),
        "pending constructor is not retired"
    );
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 0);
    assert_eq!(
        sys.actor_count(),
        2,
        "parent and pending child remain owned"
    );
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.actor_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(counts.made.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    assert!(waiting.await.is_err());
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn zero_restart_budget_stops_only_failed_child_and_explicit_policy_overrides_parent() {
    let sys = ThreadActorSystem::shared(Default::default());
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
    parent
        .send(Box::new(SetPolicy(policy(
            &counts,
            5,
            Duration::from_secs(30),
        ))))
        .await
        .unwrap();
    let child = spawn(
        &*parent,
        ChildRequest {
            factory: factory(&counts, false),
            strategy: Some(policy(&counts, 0, Duration::from_secs(30))),
            dynamic: false,
        },
    )
    .await;
    let sibling_counts = Arc::new(Counts::default());
    let sibling = spawn(
        &*parent,
        ChildRequest {
            factory: factory(&sibling_counts, false),
            strategy: None,
            dynamic: true,
        },
    )
    .await;
    response(&*child, 0).await;
    let _ = child.send(Box::new(Crash)).await;
    retired(&sys, &child.path(), &counts).await;
    response(&*sibling, 0).await;
    assert_eq!(counts.made.load(Ordering::SeqCst), 1);
    assert_eq!(counts.decisions.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    parent.stop().await.unwrap();
    assert_eq!(sibling_counts.dropped.load(Ordering::SeqCst), 1);
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn failed_first_constructor_retains_child_until_its_policy_captures_are_disposed() {
    struct Capture(Arc<Gate>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.block();
        }
    }
    let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    });
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let capture = Capture(gate.clone());
    let strategy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(30),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    });
    let failed = parent.send(Box::new(ChildRequest {
        factory: Arc::new(|| panic!("initial constructor failed")),
        strategy: Some(strategy),
        dynamic: true,
    }));
    tokio::pin!(failed);
    assert!(futures::poll!(&mut failed).is_pending());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(sys.actor_count(), 2);
    assert_eq!(counts.terminated.load(Ordering::SeqCst), 0);
    assert!(matches!(
        failed.await,
        Err(parrot_api::errors::ActorError::TimeoutDetail(_))
    ));
    let children = *parent
        .send(Box::new(Children))
        .await
        .unwrap()
        .downcast::<Vec<String>>()
        .unwrap();
    assert_eq!(
        children.len(),
        1,
        "failed construction is still owned during policy disposal"
    );
    gate.release();
    retired(&sys, &children[0], &counts).await;
    assert_eq!(counts.terminated.load(Ordering::SeqCst), 1);
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancelling_child_construction_waiter_retires_ownership_and_parent_can_spawn_again() {
    let sys = ThreadActorSystem::shared(Default::default());
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let captured = gate.clone();
    let build = factory(&counts, false);
    let cancel = Arc::new(tokio::sync::Notify::new());
    let waiter = parent.send(Box::new(CancelConstruction {
        factory: Arc::new(move || {
            captured.block();
            build()
        }),
        cancel: cancel.clone(),
    }));
    tokio::pin!(waiter);
    assert!(futures::poll!(&mut waiter).is_pending());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    cancel.notify_one();
    waiter.await.unwrap();
    assert_eq!(sys.actor_count(), 2);
    let child_path = sys
        .actor_paths()
        .into_iter()
        .find(|p| p != "/parent")
        .unwrap();
    gate.release();
    retired(&sys, &child_path, &counts).await;
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    let replacement = spawn(
        &*parent,
        ChildRequest {
            factory: factory(&counts, false),
            strategy: None,
            dynamic: false,
        },
    )
    .await;
    response(&*replacement, 1).await;
    parent.stop().await.unwrap();
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_factories_consume_parent_capacity_and_cancellation_reclaims_it() {
    let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shutdown_timeout: Duration::from_secs(5),
        ..Default::default()
    });
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let cancel = Arc::new(tokio::sync::Notify::new());
    let (ready, observed) = tokio::sync::oneshot::channel();
    let waiter = parent.send(Box::new(FillCapacity {
        gate: gate.clone(),
        counts: counts.clone(),
        calls: calls.clone(),
        cancel: cancel.clone(),
        ready,
    }));
    tokio::pin!(waiter);
    assert!(futures::poll!(&mut waiter).is_pending());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), observed)
            .await
            .unwrap()
            .unwrap(),
        1024
    );
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(sys.actor_count(), 1025);
    cancel.notify_one();
    waiter.await.unwrap();
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.actor_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(counts.made.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "cancelled queued factories cannot execute"
    );
    let replacement = spawn(
        &*parent,
        ChildRequest {
            factory: factory(&counts, false),
            strategy: None,
            dynamic: false,
        },
    )
    .await;
    response(&*replacement, 1).await;
    sys.shutdown_internal().await.unwrap();
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn rejected_strategy_disposal_remains_owned_after_parent_handler_cancellation() {
    struct Capture {
        gate: Arc<Gate>,
        disposed: Arc<tokio::sync::Notify>,
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            self.gate.block();
            self.disposed.notify_one();
        }
    }
    let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shutdown_timeout: Duration::from_millis(100),
        ..Default::default()
    });
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let disposed = Arc::new(tokio::sync::Notify::new());
    let capture = Capture {
        gate: gate.clone(),
        disposed: disposed.clone(),
    };
    let strategy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(30),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Restart
        }),
    });
    let request = parent.send(Box::new(StopAndReject(ChildRequest {
        factory: factory(&counts, false),
        strategy: Some(strategy),
        dynamic: true,
    })));
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    // Force cancellation of the actor's spawn waiter while rejection still
    // owns a blocking destructor. The registry and business pool stay usable.
    assert!(parent.stop().await.is_err());
    assert_eq!(sys.actor_count(), 1);
    let neighbour = sys
        .spawn_at(
            Parent(Arc::default()),
            "/neighbour",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    neighbour
        .send_with_timeout(Box::new(Children), Some(Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(counts.made.load(Ordering::SeqCst), 0);
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), disposed.notified())
        .await
        .unwrap();
    assert!(request.await.is_err());
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.get_actor_ref("/parent").is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn child_replacement_retires_old_descendants_before_constructing_new_generation() {
    let sys = ThreadActorSystem::shared(Default::default());
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
    let leaves = Arc::new(AtomicUsize::new(0));
    let build = factory(&counts, false);
    let observed = leaves.clone();
    let child = spawn(
        &*parent,
        ChildRequest {
            factory: Arc::new(move || {
                let actor = build();
                assert_eq!(
                    observed.load(Ordering::SeqCst),
                    actor.generation,
                    "old descendant must retire before replacement construction"
                );
                actor
            }),
            strategy: Some(policy(&counts, 1, Duration::from_secs(30))),
            dynamic: true,
        },
    )
    .await;
    response(&*child, 0).await;
    let old_leaf = *child
        .send(Box::new(MakeLeaf(leaves.clone())))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    assert_eq!(
        *old_leaf
            .send(Box::new(()))
            .await
            .unwrap()
            .downcast::<String>()
            .unwrap(),
        child.path()
    );
    let _ = child.send(Box::new(Crash)).await;
    response(&*child, 1).await;
    assert!(!old_leaf.is_alive().await);
    assert_eq!(leaves.load(Ordering::SeqCst), 1);
    let leaf = *child
        .send(Box::new(MakeLeaf(leaves.clone())))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    assert_eq!(
        *leaf
            .send(Box::new(()))
            .await
            .unwrap()
            .downcast::<String>()
            .unwrap(),
        child.path()
    );
    // The old logical reference keeps its completed stop result. Its stopped
    // mailbox must still reject messages and must not affect the replacement.
    old_leaf.stop().await.unwrap();
    assert!(old_leaf.send(Box::new(())).await.is_err());
    leaf.send(Box::new(())).await.unwrap();
    parent.stop().await.unwrap();
    assert_eq!(leaves.load(Ordering::SeqCst), 2);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}

async fn creation_failure_with_full_notification_mailbox(watch: bool) {
    let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_mailbox_capacity: 1,
        shutdown_timeout: Duration::from_millis(300),
        ..Default::default()
    });
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let request = parent.send(Box::new(ChildRequest {
        factory: Arc::new(move || {
            blocked.block();
            panic!("constructor diagnostic");
        }),
        strategy: None,
        dynamic: true,
    }));
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    parent.deliver(Box::new(Children)).await.unwrap();
    if watch {
        let path = sys
            .actor_paths()
            .into_iter()
            .find(|path| path != "/parent")
            .unwrap();
        sys.get_actor_ref(&path)
            .unwrap()
            .watch(parent.clone_boxed())
            .await
            .unwrap();
    }
    gate.release();
    let result = request.await;
    let preserved = matches!(&result, Err(parrot_api::errors::ActorError::InitializationError(message)) if message.contains("constructor diagnostic"));
    sys.shutdown_internal().await.unwrap();
    assert!(
        preserved,
        "full parent mailbox replaced the creation error: {result:?}"
    );
}

#[tokio::test]
async fn first_factory_failure_is_not_delayed_by_full_parent_notification_mailbox() {
    creation_failure_with_full_notification_mailbox(false).await;
}
#[tokio::test]
async fn first_factory_failure_is_not_delayed_by_full_death_watch_mailbox() {
    creation_failure_with_full_notification_mailbox(true).await;
}
#[tokio::test]
async fn constructor_panic_during_parent_stop_is_not_a_child_disposal_failure() {
    let sys = ThreadActorSystem::shared(Default::default());
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let captured = gate.clone();
    let request = parent.send(Box::new(ChildRequest {
        factory: Arc::new(move || {
            captured.block();
            panic!("constructor failed before producing an actor");
        }),
        strategy: None,
        dynamic: true,
    }));
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    let stopped = parent.stop();
    tokio::pin!(stopped);
    assert!(futures::poll!(&mut stopped).is_pending());
    gate.release();
    let result = stopped.await;
    let _ = request.await;
    let _ = sys.shutdown_internal().await;
    assert!(
        result.is_ok(),
        "completed disposal was confused with construction failure: {result:?}"
    );
    assert_eq!(sys.actor_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_preparation_barrier_and_abort_retain_every_member() {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 2,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "cohort".into(),
        },
    ] {
        for outcome in 0..4 {
            let counts = Arc::new(Counts::default());
            let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
                shared_pool_size: 2,
                default_scheduling_mode: mode.clone(),
                ..Default::default()
            });
            let parent = parent(&sys, &counts).await;
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
            let first = spawn(
                &*parent,
                ChildRequest {
                    factory: build,
                    strategy: None,
                    dynamic: true,
                },
            )
            .await;
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
            let second = spawn(
                &*parent,
                ChildRequest {
                    factory: build,
                    strategy: None,
                    dynamic: true,
                },
            )
            .await;
            response(&*first, 0).await;
            response(&*second, 0).await;
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
                response(&*first, 1).await;
                response(&*second, 1).await;
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
            sys.shutdown_internal().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_restart_budget_is_shared_when_failures_alternate_between_siblings() {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 2,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "cohort".into(),
        },
    ] {
        let counts = Arc::new(Counts::default());
        let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 2,
            default_scheduling_mode: mode.clone(),
            ..Default::default()
        });
        let parent = parent(&sys, &counts).await;
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
        let first = spawn(
            &*parent,
            ChildRequest {
                factory: build,
                strategy: None,
                dynamic: true,
            },
        )
        .await;
        let build = factory(&second_counts, false);
        let second = spawn(
            &*parent,
            ChildRequest {
                factory: build,
                strategy: None,
                dynamic: true,
            },
        )
        .await;
        response(&*first, 0).await;
        response(&*second, 0).await;
        for (child, generation) in [(&first, 1), (&second, 2)] {
            let _ = child.send(Box::new(Crash)).await;
            response(&*first, generation).await;
            response(&*second, generation).await;
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
        sys.shutdown_internal().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_larger_than_constructor_pool_recovers_simultaneous_failures_once() {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 2,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "cohort".into(),
        },
    ] {
        let counts = Arc::new(Counts::default());
        let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            default_scheduling_mode: mode,
            ..Default::default()
        });
        let parent = parent(&sys, &counts).await;
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
            let child = spawn(
                &*parent,
                ChildRequest {
                    factory: build,
                    strategy: None,
                    dynamic: true,
                },
            )
            .await;
            response(&*child, 0).await;
            child.watch(parent.clone_boxed()).await.unwrap();
            cohort.push((child, member));
        }
        let together = Arc::new(tokio::sync::Barrier::new(2));
        let (a, b) = tokio::join!(
            cohort[0].0.send(Box::new(CrashTogether(together.clone()))),
            cohort[1].0.send(Box::new(CrashTogether(together)))
        );
        assert!(a.is_err() && b.is_err());
        for (index, (child, member)) in cohort.iter().enumerate() {
            response(&**child, 1).await;
            assert_eq!(member.made.load(Ordering::SeqCst), 2);
            assert_eq!(member.dropped.load(Ordering::SeqCst), 1);
            assert_eq!(
                member.hooked.load(Ordering::SeqCst),
                usize::from(index >= 2),
                "only healthy cohort members run before_stop"
            );
        }
        assert_eq!(counts.terminated.load(Ordering::SeqCst), 0);
        assert_eq!(counts.watched.load(Ordering::SeqCst), 0);
        sys.shutdown_internal().await.unwrap();
        for (_, member) in cohort {
            assert_eq!(member.dropped.load(Ordering::SeqCst), 2);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_recovery_includes_a_sibling_still_in_its_first_constructor() {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 2,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "cohort".into(),
        },
    ] {
        let counts = Arc::new(Counts::default());
        let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 2,
            default_scheduling_mode: mode,
            ..Default::default()
        });
        let parent = parent(&sys, &counts).await;
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
            .send(Box::new(ChildRequest {
                factory: build,
                strategy: None,
                dynamic: true,
            }))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        response(&*first, 0).await;
        let healthy_counts = Arc::new(Counts::default());
        let build = factory(&healthy_counts, false);
        let healthy = *parent
            .send(Box::new(ChildRequest {
                factory: build,
                strategy: None,
                dynamic: true,
            }))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        response(&*healthy, 0).await;
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
        let pending = parent.send(Box::new(ChildRequest {
            factory: build,
            strategy: None,
            dynamic: true,
        }));
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
        response(&*first, 1).await;
        response(&*second, 1).await;
        assert_eq!(first_counts.initialized.load(Ordering::SeqCst), 2);
        assert_eq!(
            second_counts.initialized.load(Ordering::SeqCst),
            1,
            "cancelled first output must never initialize"
        );
        assert_eq!(counts.terminated.load(Ordering::SeqCst), 0);
        sys.shutdown_internal().await.unwrap();
        assert_eq!(first_counts.dropped.load(Ordering::SeqCst), 2);
        assert_eq!(second_counts.dropped.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_partial_scheduler_rejection_disposes_every_output_without_initialization() {
    let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_scheduling_mode: SchedulingMode::DedicatedThread,
        max_dedicated_threads: 3,
        ..Default::default()
    });
    let counts = Arc::new(Counts::default());
    let parent = parent(&sys, &counts).await;
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
    let next = factory(&first_counts, false);
    let publish_system = sys.clone();
    let capacity_drop = Arc::new(AtomicUsize::new(0));
    let capacity_output = capacity_drop.clone();
    let first = spawn(
        &*parent,
        ChildRequest {
            factory: Arc::new(move || {
                let actor = next();
                if actor.generation == 1 {
                    tokio::runtime::Handle::current()
                        .block_on(publish_system.spawn_at(
                            Leaf(capacity_output.clone()),
                            "/capacity-holder",
                            None,
                            Default::default(),
                        ))
                        .unwrap();
                }
                actor
            }),
            strategy: None,
            dynamic: true,
        },
    )
    .await;
    let second = spawn(
        &*parent,
        ChildRequest {
            factory: factory(&second_counts, false),
            strategy: None,
            dynamic: true,
        },
    )
    .await;
    response(&*first, 0).await;
    response(&*second, 0).await;
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
    assert_eq!(first_counts.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(second_counts.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(first_counts.made.load(Ordering::SeqCst), 2);
    assert_eq!(second_counts.made.load(Ordering::SeqCst), 2);
    assert_eq!(first_counts.dropped.load(Ordering::SeqCst), 2);
    assert_eq!(second_counts.dropped.load(Ordering::SeqCst), 2);
    assert_eq!(
        sys.actor_count(),
        2,
        "only parent and capacity holder remain"
    );
    sys.shutdown_internal().await.unwrap();
    assert_eq!(capacity_drop.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_public_deadline_keeps_constructor_ownership_and_child_admission_closed() {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 2,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "cohort".into(),
        },
    ] {
        let counts = Arc::new(Counts::default());
        let sys = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 2,
            default_scheduling_mode: mode,
            shutdown_timeout: Duration::from_millis(150),
            ..Default::default()
        });
        let parent = parent(&sys, &counts).await;
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
            .send(Box::new(ChildRequest {
                factory: build,
                strategy: None,
                dynamic: true,
            }))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        let build = factory(&second_counts, false);
        let second = *parent
            .send(Box::new(ChildRequest {
                factory: build,
                strategy: None,
                dynamic: true,
            }))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        response(&*first, 0).await;
        response(&*second, 0).await;
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
            .send(Box::new(ChildRequest {
                factory: build,
                strategy: None,
                dynamic: true,
            }))
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
            .send(Box::new(ChildRequest {
                factory: build,
                strategy: None,
                dynamic: true,
            }))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap();
        response(&*replacement, 0).await;
        sys.shutdown_internal().await.unwrap();
    }
}
