use parrot::actix::{ActixActor, ActixActorSystem, ActixContext, ActixSystemSettings};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::ActorContext,
    errors::ActorError,
    supervisor::{BasicDecisionFn, DefaultSupervisorStrategyFactory, SupervisionDecision},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    cell::Cell,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[derive(Default)]
struct Counts {
    made: AtomicUsize,
    initialized: AtomicUsize,
    dropped: AtomicUsize,
    captures: AtomicUsize,
    watched: AtomicUsize,
}
struct Root {
    counts: Arc<Counts>,
    generation: usize,
    messages: Cell<usize>,
    fail_init: bool,
}
impl Drop for Root {
    fn drop(&mut self) {
        self.counts.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActixEngineExt for Root {}
impl Actor for Root {
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
            assert!(
                ctx.parent().is_none(),
                "factory root must not invent a parent"
            );
            self.counts.initialized.fetch_add(1, Ordering::SeqCst);
            assert!(!self.fail_init, "root init failure");
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            assert!(!msg.is::<Crash>(), "root handler failure");
            if msg.is::<parrot_api::context::ActorTerminated>() {
                self.counts.watched.fetch_add(1, Ordering::SeqCst);
                return Ok(Box::new(()) as BoxedMessage);
            }
            if msg.is::<Native>() {
                return Ok(Box::new(ctx.get_self_ref()) as BoxedMessage);
            }
            self.messages.set(self.messages.get() + 1);
            Ok(Box::new((self.generation, self.messages.get())) as BoxedMessage)
        })
    }
}
struct Crash;
struct Native;
fn factory(counts: &Arc<Counts>, fail_init: bool) -> impl Fn() -> Root + Send + Sync + 'static {
    let counts = counts.clone();
    move || Root {
        generation: counts.made.fetch_add(1, Ordering::SeqCst),
        counts: counts.clone(),
        messages: Cell::new(0),
        fail_init,
    }
}
async fn generation(actor: &BoxedActorRef, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(reply) = actor.send(Box::new(())).await {
                let (actual, _) = *reply.downcast::<(usize, usize)>().unwrap();
                if actual == expected {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
async fn absent(sys: &ActixActorSystem, path: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.get_actor(&path.to_owned()).await.is_some() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
async fn system(timeout: Duration) -> ActixActorSystem {
    ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: timeout,
        ..Default::default()
    })
    .await
    .unwrap()
}
#[actix::test]
async fn root_default_factory_retains_reference_across_generations_and_accepts_send_only_actor() {
    let sys = system(Duration::from_secs(2)).await;
    let counts = Arc::new(Counts::default());
    let root = sys
        .spawn_root_factory(factory(&counts, false))
        .await
        .unwrap();
    let observed = Arc::new(Counts::default());
    let watcher = sys
        .spawn_root_typed(factory(&observed, false)(), EmptyConfig)
        .await
        .unwrap();
    root.watch(watcher.clone_boxed()).await.unwrap();
    let identity = root.death_watch().unwrap().id();
    generation(&root, 0).await;
    let old = *root
        .send(Box::new(Native))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    for next in 1..=2 {
        let _ = root.send(Box::new(Crash)).await;
        generation(&root, next).await;
        assert_eq!(root.death_watch().unwrap().id(), identity);
        assert_eq!(counts.dropped.load(Ordering::SeqCst), next);
        assert_eq!(sys.status().active_actors, 2);
        assert_eq!(observed.watched.load(Ordering::SeqCst), 0);
    }
    let _ = old.stop().await;
    generation(&root, 2).await;
    root.stop().await.unwrap();
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 3);
    assert!(sys.get_actor(&root.path()).await.is_none());
    tokio::time::timeout(Duration::from_secs(2), async {
        while observed.watched.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    watcher.send(Box::new(())).await.unwrap();
    assert_eq!(observed.watched.load(Ordering::SeqCst), 1);
    sys.shutdown().await.unwrap();
}
#[actix::test]
async fn root_restart_budget_limits_actual_reconstruction() {
    for maximum in [0, 2] {
        let sys = system(Duration::from_secs(2)).await;
        let counts = Arc::new(Counts::default());
        let root = sys
            .spawn_root_factory_with_strategy(
                factory(&counts, false),
                DefaultSupervisorStrategyFactory::one_for_one(maximum, Duration::from_secs(60)),
            )
            .await
            .unwrap();
        for next in 0..=maximum {
            generation(&root, next as usize).await;
            let _ = root.send(Box::new(Crash)).await;
        }
        absent(&sys, &root.path()).await;
        assert_eq!(counts.made.load(Ordering::SeqCst), maximum as usize + 1);
        assert_eq!(counts.dropped.load(Ordering::SeqCst), maximum as usize + 1);
        sys.shutdown().await.unwrap();
    }
}
#[actix::test]
async fn root_initialization_failure_runs_custom_decider_and_respects_zero_budget() {
    let sys = system(Duration::from_secs(2)).await;
    let counts = Arc::new(Counts::default());
    let decided = Arc::new(AtomicUsize::new(0));
    let called = decided.clone();
    let mut strategy = DefaultSupervisorStrategyFactory::one_for_one(0, Duration::from_secs(60));
    strategy.decider = BasicDecisionFn::new(move |_| {
        called.fetch_add(1, Ordering::SeqCst);
        SupervisionDecision::Restart
    });
    let root = sys
        .spawn_root_factory_with_strategy(factory(&counts, true), strategy)
        .await
        .unwrap();
    absent(&sys, &root.path()).await;
    assert_eq!(decided.load(Ordering::SeqCst), 1);
    assert_eq!(counts.made.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    sys.shutdown().await.unwrap();
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
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(4), |open| {
                !*open
            })
            .unwrap();
        assert!(!timed.timed_out(), "test gate not released");
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
struct Capture {
    counts: Arc<Counts>,
    gate: Option<Arc<Gate>>,
    panic: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.counts.captures.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "capture disposal failed");
    }
}
#[actix::test]
async fn cancelled_pending_root_keeps_registration_until_output_and_captures_retire() {
    let sys = system(Duration::from_millis(80)).await;
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let next = factory(&counts, false);
    let blocked = gate.clone();
    let capture = Capture {
        counts: counts.clone(),
        gate: None,
        panic: false,
    };
    let mut create = Box::pin(sys.spawn_root_factory(move || {
        let _ = &capture;
        blocked.block();
        next()
    }));
    tokio::select! { result=&mut create => panic!("early result: {result:?}"), _=gate.entered()=>{} }
    assert_eq!(sys.status().active_actors, 1);
    drop(create);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(sys.status().active_actors, 1);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.status().active_actors != 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    sys.shutdown().await.unwrap();
}
#[actix::test]
async fn shutdown_rejection_does_not_invoke_constructor_and_contains_capture_panic() {
    let sys = system(Duration::from_secs(2)).await;
    sys.clone().shutdown().await.unwrap();
    let counts = Arc::new(Counts::default());
    let capture = Capture {
        counts: counts.clone(),
        gate: None,
        panic: true,
    };
    let next = factory(&counts, false);
    let result = sys
        .spawn_root_factory(move || {
            let _ = &capture;
            next()
        })
        .await;
    assert!(matches!(result, Err(ActorError::InternalError(_))));
    assert_eq!(counts.made.load(Ordering::SeqCst), 0);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
}
#[actix::test]
async fn root_stop_deadline_remains_sticky_until_capture_retirement() {
    let sys = system(Duration::from_millis(80)).await;
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let capture = Capture {
        counts: counts.clone(),
        gate: Some(gate.clone()),
        panic: false,
    };
    let next = factory(&counts, false);
    let root = sys
        .spawn_root_factory(move || {
            let _ = &capture;
            next()
        })
        .await
        .unwrap();
    generation(&root, 0).await;
    let stop = root.stop();
    tokio::pin!(stop);
    tokio::select! { result=&mut stop=>panic!("early stop: {result:?}"), _=gate.entered()=>{} }
    assert!(stop.await.is_err());
    assert!(sys.get_actor(&root.path()).await.is_some());
    gate.release();
    absent(&sys, &root.path()).await;
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    assert!(root.stop().await.is_err());
    sys.shutdown().await.unwrap();
}

#[actix::test]
async fn shutdown_during_root_decider_keeps_policy_owned_and_prevents_late_restart() {
    let sys = system(Duration::from_millis(80)).await;
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let capture = Capture {
        counts: counts.clone(),
        gate: None,
        panic: false,
    };
    let mut strategy = DefaultSupervisorStrategyFactory::one_for_one(3, Duration::from_secs(60));
    strategy.decider = BasicDecisionFn::new(move |_| {
        let _ = &capture;
        blocked.block();
        SupervisionDecision::Restart
    });
    let root = sys
        .spawn_root_factory_with_strategy(factory(&counts, false), strategy)
        .await
        .unwrap();
    generation(&root, 0).await;
    let _ = root.send(Box::new(Crash)).await;
    gate.entered().await;
    assert!(sys.clone().shutdown().await.is_err());
    assert_eq!(
        sys.status().state,
        parrot_api::system::SystemState::ShuttingDown
    );
    assert!(sys.get_actor(&root.path()).await.is_some());
    assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), async {
        while sys.status().state != parrot_api::system::SystemState::Stopped {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(counts.made.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    assert!(sys.shutdown().await.is_err());
}

#[actix::test]
async fn root_constructor_panic_is_initialization_error_and_retires_captures() {
    let sys = system(Duration::from_secs(2)).await;
    let counts = Arc::new(Counts::default());
    let capture = Capture {
        counts: counts.clone(),
        gate: None,
        panic: false,
    };
    let result = sys
        .spawn_root_factory(move || -> Root {
            let _ = &capture;
            panic!("constructor failed")
        })
        .await;
    assert!(matches!(result, Err(ActorError::InitializationError(_))));
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    assert_eq!(sys.status().active_actors, 0);
    sys.shutdown().await.unwrap();
}

#[actix::test]
async fn root_factories_share_constructor_capacity_and_cancel_queued_work_without_invocation() {
    let sys = system(Duration::from_secs(2)).await;
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let next = factory(&counts, false);
    let service = sys.clone();
    let first = tokio::spawn(async move {
        service
            .spawn_root_factory(move || {
                blocked.block();
                next()
            })
            .await
    });
    gate.entered().await;
    let mut queued = Vec::new();
    for _ in 0..2 {
        let next = factory(&counts, false);
        let service = sys.clone();
        queued.push(tokio::spawn(async move {
            service.spawn_root_factory(next).await
        }));
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while sys.status().active_actors != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        counts.made.load(Ordering::SeqCst),
        0,
        "queued constructors must not execute while sole worker is occupied"
    );
    let cancelled = queued.pop().unwrap();
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        while sys.status().active_actors != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    gate.release();
    let first = first.await.unwrap().unwrap();
    let second = queued.pop().unwrap().await.unwrap().unwrap();
    generation(&first, 0).await;
    generation(&second, 1).await;
    assert_eq!(counts.made.load(Ordering::SeqCst), 2);
    sys.shutdown().await.unwrap();
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 2);
}
