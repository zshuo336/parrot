//! Neutral Local children belong to the actual parent generation and cohort.
use parrot::actix::ActixActorSystem;
use parrot_api::{
    Message,
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{
        AsyncEngineRuntime, AsyncFactoryRuntime, AsyncLocalRuntime, FactoryOptions, LocalActor,
        LocalRuntimeFactory, SpawnOptions,
    },
    errors::ActorError,
    local_typed::LocalBoxedFuture,
    supervisor::{
        BasicDecisionFn, OneForAllStrategy, OneForOneStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    cell::Cell,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
async fn bounded<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(8), f)
        .await
        .expect("contract deadline")
}
#[derive(Default)]
struct Stats {
    built: AtomicUsize,
    drops: AtomicUsize,
    captures: AtomicUsize,
    events: Mutex<Vec<(usize, &'static str, std::thread::ThreadId)>>,
}
impl Stats {
    fn record(&self, n: usize, phase: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((n, phase, std::thread::current().id()));
    }
    async fn initialized(&self, n: usize) {
        bounded(async {
            while !self
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.0 == n && e.1 == "init")
            {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    fn affinity(&self) {
        let e = self.events.lock().unwrap();
        assert!(!e.is_empty());
        assert!(e.iter().all(|x| x.2 == e[0].2));
    }
}
struct Capture(Arc<Stats>, bool);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.captures.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "capture retirement failed");
    }
}
#[derive(Clone, Default)]
struct Setup {
    nested: Option<Arc<Stats>>,
    stop_gate: Option<Arc<tokio::sync::Semaphore>>,
    bad_drop: bool,
    bad_capture: bool,
    cohort: Option<Vec<Arc<Stats>>>,
    fail_construction: Option<usize>,
}
struct Node {
    stats: Arc<Stats>,
    generation: usize,
    local: Rc<Cell<usize>>,
    setup: Setup,
}
struct Report {
    generation: usize,
    context: TypedContext,
}
enum Cmd {
    Read,
    Context,
    Children,
    Create(LocalRuntimeFactory, Option<SupervisorStrategyType>),
    CreateSend(Arc<Stats>),
    Policy(SupervisorStrategyType),
    Fail,
}
impl Message for Cmd {
    type Result = BoxedMessage;
}
fn factory(stats: &Arc<Stats>, setup: Setup) -> LocalRuntimeFactory {
    let capture = Capture(stats.clone(), setup.bad_capture);
    let generation = Cell::new(0);
    Box::new(move || {
        let capture = &capture;
        generation.set(generation.get() + 1);
        let n = generation.get();
        assert_ne!(setup.fail_construction, Some(n), "child constructor failed");
        if n > 1
            && let Some(cohort) = &setup.cohort
        {
            assert!(
                cohort.iter().all(|s| s.drops.load(Ordering::SeqCst) >= 1),
                "constructor preceded sibling retirement"
            );
        }
        capture.0.built.fetch_add(1, Ordering::SeqCst);
        capture.0.record(n, "construct");
        Box::new(Node {
            stats: capture.0.clone(),
            generation: n,
            local: Rc::new(Cell::new(0)),
            setup: setup.clone(),
        })
    })
}
async fn handle(message: Cmd, ctx: &TypedContext) -> ActorResult<BoxedMessage> {
    match message {
        Cmd::Create(factory, strategy) => Ok(Box::new(
            ctx.local_spawner()
                .ok_or_else(|| ActorError::InitializationError("Local unsupported".into()))?
                .spawn_local_factory(factory, strategy)
                .await?,
        ) as BoxedMessage),
        Cmd::CreateSend(stats) => {
            use parrot::typed_context::TypedContextExt;
            let actor = ctx
                .spawn_typed_child_factory::<SendSibling, SiblingRead, _>(move || {
                    SendSibling::new(stats.clone())
                })
                .await?;
            Ok(Box::new(actor.into_dyn()) as BoxedMessage)
        }
        Cmd::Context => Ok(Box::new(ctx.clone())),
        Cmd::Children => Ok(Box::new(ctx.children().len())),
        Cmd::Policy(strategy) => {
            ctx.set_child_strategy(strategy);
            Ok(Box::new(()))
        }
        Cmd::Fail => Err(ActorError::InternalError("requested failure".into())),
        Cmd::Read => Ok(Box::new(ctx.local_spawner().is_some())),
    }
}
impl LocalActor for Node {
    fn init<'a>(&'a mut self, ctx: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let local = self.local.clone();
            if let Some(stats) = &self.setup.nested {
                let child = ctx
                    .local_spawner()
                    .unwrap()
                    .spawn_local_factory(factory(stats, Setup::default()), None)
                    .await?;
                child.send(Box::new(Cmd::Read)).await?;
            }
            tokio::task::yield_now().await;
            assert!(Rc::ptr_eq(&local, &self.local));
            if self.generation > 1
                && let Some(cohort) = &self.setup.cohort
            {
                assert!(
                    cohort.iter().all(|s| s.built.load(Ordering::SeqCst) >= 2),
                    "init preceded complete cohort construction"
                );
            }
            self.stats.record(self.generation, "init");
            Ok(())
        })
    }
    fn before_stop<'a>(
        &'a mut self,
        ctx: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert!(ctx.children().is_empty());
            self.stats.record(self.generation, "hook");
            if let Some(gate) = &self.setup.stop_gate {
                gate.acquire().await.unwrap().forget();
            }
            Ok(())
        })
    }
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let local = self.local.clone();
            tokio::task::yield_now().await;
            self.stats.record(self.generation, "message");
            local.set(local.get() + 1);
            let message = *message.downcast::<Cmd>().unwrap();
            if matches!(message, Cmd::Read) {
                return Ok(Box::new(Report {
                    generation: self.generation,
                    context: ctx.clone(),
                }) as BoxedMessage);
            }
            handle(message, ctx).await
        })
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        self.stats.record(self.generation, "drop");
        self.stats.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.setup.bad_drop, "child instance drop failed");
    }
}
struct TypedParent;
impl ParrotTypedDispatch for TypedParent {
    type Msg = Cmd;
    type Reply = BoxedMessage;
    fn dispatch(&mut self, _: Cmd) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        Box::pin(async { unreachable!() })
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        message: Cmd,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(handle(message, ctx))
    }
}
impl ParrotMsgVariant<TypedParent> for Cmd {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: BoxedMessage) -> ActorResult<BoxedMessage> {
        Ok(reply)
    }
}
struct NativeParent {
    ready: Arc<AtomicUsize>,
}
impl Actor for NativeParent {
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.ready.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match *message.downcast::<Cmd>().unwrap() {
                Cmd::Create(factory, strategy) => Ok(Box::new(
                    ctx.local_spawner()
                        .ok_or_else(|| ActorError::InitializationError("Local unsupported".into()))?
                        .spawn_local_factory(factory, strategy)
                        .await?,
                ) as BoxedMessage),
                Cmd::CreateSend(stats) => {
                    let actor = ctx
                        .async_spawner()
                        .unwrap()
                        .spawn_factory(
                            Box::new(move || {
                                Box::new(NativeSibling(SendSibling::new(stats.clone())))
                            }),
                            None,
                        )
                        .await?;
                    Ok(Box::new(actor) as BoxedMessage)
                }
                Cmd::Policy(strategy) => {
                    ctx.set_supervisor_strategy(strategy);
                    Ok(Box::new(()) as BoxedMessage)
                }
                Cmd::Children => {
                    Ok(Box::new(ctx.children().map_or(0, |c| c.len())) as BoxedMessage)
                }
                Cmd::Read => Ok(Box::new(ctx.local_spawner().is_some()) as BoxedMessage),
                Cmd::Fail => Err(ActorError::InternalError("requested failure".into())),
                Cmd::Context => panic!("native erased context cannot be cloned"),
            }
        })
    }
    fn before_stop<'a>(
        &'a mut self,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert!(ctx.children().is_none_or(|c| c.is_empty()));
            Ok(())
        })
    }
}
#[derive(Clone, Copy, Debug)]
enum Flavor {
    Local,
    Typed,
    Native,
}
async fn system() -> Arc<ActixActorSystem> {
    Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap())
}
// Typed parents are called through TypedActorRef::ask; its declared result is
// already BoxedMessage. The ordinary into_dyn bridge adds its documented result
// box, so treating that bridge's result as a raw Local reply is a test error.
struct Parent {
    reference: BoxedActorRef,
    native_ready: Arc<AtomicUsize>,
    typed: Option<parrot_api::typed_channel::TypedActorRef<TypedParent, Cmd>>,
}
impl std::ops::Deref for Parent {
    type Target = BoxedActorRef;
    fn deref(&self) -> &Self::Target {
        &self.reference
    }
}
impl Parent {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        match &self.typed {
            Some(typed) => Box::pin(typed.ask(*message.downcast::<Cmd>().unwrap())),
            None => self.reference.send(message),
        }
    }
}
async fn root(s: &Arc<ActixActorSystem>, flavor: Flavor) -> Parent {
    let mut typed = None;
    let native_ready = Arc::new(AtomicUsize::new(0));
    let ready = native_ready.clone();
    let reference = match flavor {
        Flavor::Local => s
            .spawn_local_factory(
                factory(&Arc::new(Stats::default()), Setup::default()),
                FactoryOptions {
                    spawn: SpawnOptions::at("/parent"),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        Flavor::Typed => {
            let actor = s
                .spawn_typed_factory::<_, Cmd, _>(|| TypedParent, "/parent")
                .await
                .unwrap();
            let reference = actor.clone().into_dyn();
            typed = Some(actor);
            reference
        }
        Flavor::Native => s
            .spawn_factory(
                Box::new(move || {
                    Box::new(NativeParent {
                        ready: ready.clone(),
                    })
                }),
                FactoryOptions {
                    spawn: SpawnOptions::at("/parent"),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
    };
    Parent {
        reference,
        typed,
        native_ready,
    }
}

async fn child(
    parent: &Parent,
    stats: &Arc<Stats>,
    setup: Setup,
    strategy: Option<SupervisorStrategyType>,
) -> BoxedActorRef {
    *parent
        .send(Box::new(Cmd::Create(factory(stats, setup), strategy)))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap()
}
async fn report(actor: &BoxedActorRef) -> Report {
    *actor
        .send(Box::new(Cmd::Read))
        .await
        .unwrap()
        .downcast::<Report>()
        .unwrap()
}
fn one(max: u32) -> SupervisorStrategyType {
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: max,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    })
}
fn all() -> SupervisorStrategyType {
    SupervisorStrategyType::OneForAll(OneForAllStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    })
}

#[actix::test]
async fn all_parent_flavors_own_real_local_children_and_parent_identity() {
    bounded(async {
        for flavor in [Flavor::Local, Flavor::Typed, Flavor::Native] {
            let s = system().await;
            let parent = root(&s, flavor).await;
            let stats = Arc::new(Stats::default());
            let actor = child(&parent, &stats, Setup::default(), None).await;
            let r = report(&actor).await;
            assert!(actor.path().starts_with("/parent/"));
            let p = r.context.parent().unwrap();
            assert_eq!(p.path(), parent.path());
            assert_eq!(
                p.death_watch().unwrap().id(),
                parent.death_watch().unwrap().id()
            );
            assert_eq!(
                *parent
                    .send(Box::new(Cmd::Children))
                    .await
                    .unwrap()
                    .downcast::<usize>()
                    .unwrap(),
                1
            );
            let found = AsyncEngineRuntime::get_actor(s.as_ref(), &actor.path())
                .await
                .unwrap();
            assert_eq!(
                found.death_watch().unwrap().id(),
                actor.death_watch().unwrap().id()
            );
            assert_eq!(report(&found).await.generation, 1);
            parent.stop().await.unwrap();
            assert!(!actor.is_alive().await);
            assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
            assert_eq!(stats.captures.load(Ordering::SeqCst), 1);
            stats.affinity();
            assert!(
                AsyncEngineRuntime::get_actor(s.as_ref(), &actor.path())
                    .await
                    .is_none()
            );
            s.as_ref().clone().shutdown().await.unwrap();
        }
    })
    .await;
}
#[actix::test]
async fn single_owner_init_can_create_and_wait_for_neutral_local_child() {
    bounded(async {
        let s = system().await;
        let nested = Arc::new(Stats::default());
        let outer = Arc::new(Stats::default());
        let parent = s
            .spawn_local_factory(
                factory(
                    &outer,
                    Setup {
                        nested: Some(nested.clone()),
                        ..Default::default()
                    },
                ),
                FactoryOptions::default(),
            )
            .await
            .unwrap();
        report(&parent).await;
        assert_eq!(nested.built.load(Ordering::SeqCst), 1);
        parent.stop().await.unwrap();
        assert_eq!(nested.drops.load(Ordering::SeqCst), 1);
        nested.affinity();
        outer.affinity();
        s.as_ref().clone().shutdown().await.unwrap();
    })
    .await;
}
#[actix::test]
async fn parent_replacement_retires_old_subtree_and_old_context_cannot_admit() {
    bounded(async {
        for flavor in [Flavor::Local, Flavor::Typed, Flavor::Native] {
            let s = system().await;
            let parent = root(&s, flavor).await;
            let stats = Arc::new(Stats::default());
            let nested = Arc::new(Stats::default());
            let actor = child(
                &parent,
                &stats,
                Setup {
                    nested: Some(nested.clone()),
                    ..Default::default()
                },
                None,
            )
            .await;
            let old = report(&actor).await.context;
            assert!(parent.send(Box::new(Cmd::Fail)).await.is_err());
            // Native factories temporarily have no current generation; failure
            // reply is not a publication barrier. Observe actual replacement init.
            if matches!(flavor, Flavor::Native) {
                bounded(async {
                    while parent.native_ready.load(Ordering::SeqCst) < 2 {
                        tokio::task::yield_now().await;
                    }
                })
                .await;
            }
            parent.send(Box::new(Cmd::Read)).await.unwrap();
            assert!(!actor.is_alive().await);
            assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
            assert_eq!(nested.drops.load(Ordering::SeqCst), 1);
            let rejected = Arc::new(Stats::default());
            let error = old
                .local_spawner()
                .unwrap()
                .spawn_local_factory(factory(&rejected, Setup::default()), None)
                .await;
            assert!(error.is_err());
            assert_eq!(rejected.built.load(Ordering::SeqCst), 0);
            assert_eq!(rejected.captures.load(Ordering::SeqCst), 1);
            let fresh = child(&parent, &Arc::new(Stats::default()), Setup::default(), None).await;
            report(&fresh).await;
            parent.stop().await.unwrap();
            s.as_ref().clone().shutdown().await.unwrap();
        }
    })
    .await;
}
#[actix::test]
async fn local_child_inherits_live_policy_and_respects_override() {
    bounded(async {
        for flavor in [Flavor::Local, Flavor::Typed, Flavor::Native] {
            let s = system().await;
            let parent = root(&s, flavor).await;
            // Leave unused budget in the original policy: after changing to
            // zero, a cached policy must still allow a restart and fail this test.
            parent.send(Box::new(Cmd::Policy(one(3)))).await.unwrap();
            let stats = Arc::new(Stats::default());
            let actor = child(&parent, &stats, Setup::default(), None).await;
            report(&actor).await;
            let id = actor.death_watch().unwrap().id();
            assert!(actor.send(Box::new(Cmd::Fail)).await.is_err());
            stats.initialized(2).await;
            assert_eq!(report(&actor).await.generation, 2);
            assert_eq!(actor.death_watch().unwrap().id(), id);
            parent.send(Box::new(Cmd::Policy(one(0)))).await.unwrap();
            assert!(actor.send(Box::new(Cmd::Fail)).await.is_err());
            bounded(async {
                while actor.is_alive().await {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert!(actor.stop().await.is_err());
            let own = Arc::new(Stats::default());
            let overridden = child(&parent, &own, Setup::default(), Some(one(1))).await;
            report(&overridden).await;
            assert!(overridden.send(Box::new(Cmd::Fail)).await.is_err());
            own.initialized(2).await;
            assert_eq!(report(&overridden).await.generation, 2);
            parent.stop().await.unwrap();
            s.as_ref().clone().shutdown().await.unwrap();
        }
    })
    .await;
}
#[actix::test]
async fn one_for_all_rebuilds_local_siblings_under_every_parent_flavor() {
    bounded(async {
        for flavor in [Flavor::Local, Flavor::Typed, Flavor::Native] {
            let s = system().await;
            let parent = root(&s, flavor).await;
            parent.send(Box::new(Cmd::Policy(all()))).await.unwrap();
            let a = Arc::new(Stats::default());
            let b = Arc::new(Stats::default());
            let first = child(&parent, &a, Setup::default(), None).await;
            let second = child(&parent, &b, Setup::default(), None).await;
            let old = report(&second).await.context;
            report(&first).await;
            assert!(first.send(Box::new(Cmd::Fail)).await.is_err());
            a.initialized(2).await;
            b.initialized(2).await;
            assert_eq!(a.drops.load(Ordering::SeqCst), 1);
            assert_eq!(b.drops.load(Ordering::SeqCst), 1);
            assert_eq!(report(&second).await.generation, 2);
            let denied = Arc::new(Stats::default());
            assert!(
                old.local_spawner()
                    .unwrap()
                    .spawn_local_factory(factory(&denied, Setup::default()), None)
                    .await
                    .is_err()
            );
            assert_eq!(denied.built.load(Ordering::SeqCst), 0);
            parent.stop().await.unwrap();
            assert_eq!(a.drops.load(Ordering::SeqCst), 2);
            assert_eq!(b.drops.load(Ordering::SeqCst), 2);
            s.as_ref().clone().shutdown().await.unwrap();
        }
    })
    .await;
}
#[actix::test]
async fn unpolled_and_closed_context_inputs_retire_without_construction() {
    bounded(async {
        let s = system().await;
        let parent = root(&s, Flavor::Local).await;
        let ctx = *parent
            .send(Box::new(Cmd::Context))
            .await
            .unwrap()
            .downcast::<TypedContext>()
            .unwrap();
        let stats = Arc::new(Stats::default());
        let pending = ctx.local_spawner().unwrap().spawn_local_factory(
            factory(
                &stats,
                Setup {
                    bad_capture: true,
                    ..Default::default()
                },
            ),
            None,
        );
        drop(pending);
        bounded(async {
            while stats.captures.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(stats.built.load(Ordering::SeqCst), 0);
        parent.stop().await.unwrap();
        let closed = Arc::new(Stats::default());
        let error = ctx
            .local_spawner()
            .unwrap()
            .spawn_local_factory(
                factory(
                    &closed,
                    Setup {
                        bad_capture: true,
                        ..Default::default()
                    },
                ),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("capture retirement failed"),
            "{error}"
        );
        assert_eq!(closed.built.load(Ordering::SeqCst), 0);
        assert_eq!(closed.captures.load(Ordering::SeqCst), 1);
        s.as_ref().clone().shutdown().await.unwrap();
    })
    .await;
}
#[actix::test]
async fn parent_stop_cancellation_keeps_local_child_cleanup_owned() {
    bounded(async {
        for flavor in [Flavor::Local, Flavor::Typed, Flavor::Native] {
            let s = system().await;
            let parent = root(&s, flavor).await;
            let stats = Arc::new(Stats::default());
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let actor = child(
                &parent,
                &stats,
                Setup {
                    stop_gate: Some(gate.clone()),
                    ..Default::default()
                },
                None,
            )
            .await;
            report(&actor).await;
            let stop = parent.clone_boxed();
            let waiter = tokio::spawn(async move { stop.stop().await });
            bounded(async {
                while !stats.events.lock().unwrap().iter().any(|e| e.1 == "hook") {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
            assert_eq!(stats.drops.load(Ordering::SeqCst), 0);
            assert_eq!(stats.captures.load(Ordering::SeqCst), 0);
            gate.add_permits(1);
            parent.stop().await.unwrap();
            assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
            assert_eq!(stats.captures.load(Ordering::SeqCst), 1);
            s.as_ref().clone().shutdown().await.unwrap();
        }
    })
    .await;
}
#[actix::test]
async fn thread_contexts_advertise_no_neutral_local_child_executor() {
    bounded(async {
        let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
        let native = AsyncEngineRuntime::spawn(
            thread.as_ref(),
            Box::new(NativeParent {
                ready: Arc::new(AtomicUsize::new(0)),
            }),
            Box::new(()),
            SpawnOptions::default(),
        )
        .await
        .unwrap();
        assert!(
            !*native
                .send(Box::new(Cmd::Read))
                .await
                .unwrap()
                .downcast::<bool>()
                .unwrap()
        );
        let typed = thread
            .spawn_typed_factory::<_, Cmd, _>(|| TypedParent, "/typed")
            .await
            .unwrap();
        assert!(
            !*typed
                .ask(Cmd::Read)
                .await
                .unwrap()
                .downcast::<bool>()
                .unwrap()
        );
        AsyncEngineRuntime::shutdown(thread.as_ref()).await.unwrap();
    })
    .await;
}

struct SiblingRead;
impl Message for SiblingRead {
    type Result = usize;
}
struct SendSibling {
    stats: Arc<Stats>,
    generation: usize,
}
impl SendSibling {
    fn new(stats: Arc<Stats>) -> Self {
        let generation = stats.built.fetch_add(1, Ordering::SeqCst) + 1;
        Self { stats, generation }
    }
}
impl Drop for SendSibling {
    fn drop(&mut self) {
        self.stats.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ParrotTypedDispatch for SendSibling {
    type Msg = SiblingRead;
    type Reply = usize;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async {
            self.stats.record(self.generation, "init");
            Ok(())
        })
    }
    fn dispatch(&mut self, _: SiblingRead) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async { Ok(self.generation) })
    }
}
impl ParrotMsgVariant<SendSibling> for SiblingRead {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}
struct NativeSibling(SendSibling);
impl Actor for NativeSibling {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.init()
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert!(message.is::<SiblingRead>());
        Box::pin(async { Ok(Box::new(self.0.generation) as BoxedMessage) })
    }
}
#[actix::test]
async fn neutral_local_and_send_factories_form_one_native_parent_cohort() {
    bounded(async {
        for flavor in [Flavor::Local, Flavor::Typed, Flavor::Native] {
            let s = system().await;
            let parent = root(&s, flavor).await;
            parent.send(Box::new(Cmd::Policy(all()))).await.unwrap();
            let local = Arc::new(Stats::default());
            let send = Arc::new(Stats::default());
            let actor = child(
                &parent,
                &local,
                Setup {
                    cohort: Some(vec![local.clone(), send.clone()]),
                    ..Default::default()
                },
                None,
            )
            .await;
            let sibling = *parent
                .send(Box::new(Cmd::CreateSend(send.clone())))
                .await
                .unwrap()
                .downcast::<BoxedActorRef>()
                .unwrap();
            report(&actor).await;
            assert_eq!(
                *sibling
                    .send(Box::new(SiblingRead))
                    .await
                    .unwrap()
                    .downcast::<usize>()
                    .unwrap(),
                1
            );
            assert!(actor.send(Box::new(Cmd::Fail)).await.is_err());
            local.initialized(2).await;
            send.initialized(2).await;
            assert_eq!(report(&actor).await.generation, 2);
            assert_eq!(
                *sibling
                    .send(Box::new(SiblingRead))
                    .await
                    .unwrap()
                    .downcast::<usize>()
                    .unwrap(),
                2
            );
            assert_eq!(local.drops.load(Ordering::SeqCst), 1);
            assert_eq!(send.drops.load(Ordering::SeqCst), 1);
            parent.stop().await.unwrap();
            assert_eq!(local.drops.load(Ordering::SeqCst), 2);
            assert_eq!(send.drops.load(Ordering::SeqCst), 2);
            s.as_ref().clone().shutdown().await.unwrap();
        }
    })
    .await;
}
#[actix::test]
async fn parent_stop_propagates_child_cleanup_failure_without_later_capture_overwrite() {
    bounded(async {
        for flavor in [Flavor::Local, Flavor::Typed, Flavor::Native] {
            let s = system().await;
            let parent = root(&s, flavor).await;
            let stats = Arc::new(Stats::default());
            let actor = child(
                &parent,
                &stats,
                Setup {
                    bad_drop: true,
                    bad_capture: true,
                    ..Default::default()
                },
                None,
            )
            .await;
            report(&actor).await;
            let error = parent.stop().await.unwrap_err().to_string();
            assert!(
                error.contains("child instance drop failed"),
                "{flavor:?}: {error}"
            );
            assert!(
                !error.contains("capture retirement failed"),
                "{flavor:?}: {error}"
            );
            assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
            assert_eq!(stats.captures.load(Ordering::SeqCst), 1);
            assert!(
                actor
                    .stop()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("child instance drop failed")
            );
            s.as_ref().clone().shutdown().await.unwrap();
        }
    })
    .await;
}

struct RetainedNative {
    ready: Arc<AtomicUsize>,
}
impl parrot_api::actor::ActixEngineExt for RetainedNative {}
type NativeContext = parrot::actix::ActixContext<parrot::actix::ActixActor<RetainedNative>>;
impl Actor for RetainedNative {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = NativeContext;
    fn use_async_handler(&self) -> bool {
        true
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async {
            self.ready.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match *message.downcast::<Cmd>().unwrap() {
                Cmd::Context => Ok(Box::new(ctx.clone()) as BoxedMessage),
                Cmd::Fail => Err(ActorError::InternalError("replace native parent".into())),
                _ => Ok(Box::new(()) as BoxedMessage),
            }
        })
    }
}
#[actix::test]
async fn retained_native_context_cannot_admit_into_replacement_or_reused_parent_path() {
    bounded(async {
        let s = system().await;
        let ready = Arc::new(AtomicUsize::new(0));
        let made = ready.clone();
        let parent = s
            .spawn_root_factory(move || RetainedNative {
                ready: made.clone(),
            })
            .await
            .unwrap();
        let context = *parent
            .send(Box::new(Cmd::Context))
            .await
            .unwrap()
            .downcast::<NativeContext>()
            .unwrap();
        let counts = Arc::new(Stats::default());
        let actor = context
            .local_spawner()
            .unwrap()
            .spawn_local_factory(factory(&counts, Setup::default()), None)
            .await
            .unwrap();
        assert_eq!(
            report(&actor)
                .await
                .context
                .parent()
                .unwrap()
                .death_watch()
                .unwrap()
                .id(),
            parent.death_watch().unwrap().id()
        );
        assert!(parent.send(Box::new(Cmd::Fail)).await.is_err());
        bounded(async {
            while ready.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(!actor.is_alive().await);
        let rejected = Arc::new(Stats::default());
        assert!(
            context
                .local_spawner()
                .unwrap()
                .spawn_local_factory(factory(&rejected, Setup::default()), None)
                .await
                .is_err()
        );
        assert_eq!(rejected.built.load(Ordering::SeqCst), 0);
        parent.stop().await.unwrap();
        let fresh = s
            .spawn_local_factory(
                factory(&Arc::new(Stats::default()), Setup::default()),
                FactoryOptions {
                    spawn: SpawnOptions::at(parent.path()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        report(&fresh).await;
        let id = fresh.death_watch().unwrap().id();
        assert!(
            context
                .local_spawner()
                .unwrap()
                .spawn_local_factory(factory(&rejected, Setup::default()), None)
                .await
                .is_err()
        );
        assert_eq!(rejected.built.load(Ordering::SeqCst), 0);
        assert_eq!(rejected.captures.load(Ordering::SeqCst), 2);
        assert_eq!(
            AsyncEngineRuntime::get_actor(s.as_ref(), &fresh.path())
                .await
                .unwrap()
                .death_watch()
                .unwrap()
                .id(),
            id
        );
        fresh.stop().await.unwrap();
        s.as_ref().clone().shutdown().await.unwrap();
    })
    .await;
}

async fn hostile_inputs(spawner: &dyn parrot_api::engine::AsyncLocalActorSpawner, unpolled: bool) {
    let constructors = Arc::new(Stats::default());
    let policies = Arc::new(Stats::default());
    let capture = Capture(policies.clone(), true);
    let policy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |_| {
            std::hint::black_box(&capture);
            SupervisionDecision::Stop
        }),
    });
    let future = spawner.spawn_local_factory(
        factory(
            &constructors,
            Setup {
                bad_capture: true,
                ..Default::default()
            },
        ),
        Some(policy),
    );
    if unpolled {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok());
    } else {
        assert!(
            future
                .await
                .unwrap_err()
                .to_string()
                .contains("capture retirement failed")
        );
    }
    bounded(async {
        while constructors.captures.load(Ordering::SeqCst) != 1
            || policies.captures.load(Ordering::SeqCst) != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(constructors.built.load(Ordering::SeqCst), 0);
}
#[actix::test]
async fn native_and_typed_child_factory_inputs_have_independent_cancellation_and_rejection_cleanup()
{
    bounded(async {
        let s = system().await;
        let local = root(&s, Flavor::Local).await;
        let typed = *local
            .send(Box::new(Cmd::Context))
            .await
            .unwrap()
            .downcast::<TypedContext>()
            .unwrap();
        let parent = s
            .spawn_root_factory(|| RetainedNative {
                ready: Arc::new(AtomicUsize::new(0)),
            })
            .await
            .unwrap();
        let native = *parent
            .send(Box::new(Cmd::Context))
            .await
            .unwrap()
            .downcast::<NativeContext>()
            .unwrap();
        hostile_inputs(typed.local_spawner().unwrap(), true).await;
        hostile_inputs(native.local_spawner().unwrap(), true).await;
        local.stop().await.unwrap();
        parent.stop().await.unwrap();
        hostile_inputs(typed.local_spawner().unwrap(), false).await;
        hostile_inputs(native.local_spawner().unwrap(), false).await;
        s.as_ref().clone().shutdown().await.unwrap();
    })
    .await;
}

#[actix::test]
async fn failed_cohort_construction_never_activates_partial_local_replacements() {
    bounded(async {
        for flavor in [Flavor::Local, Flavor::Typed, Flavor::Native] {
            let s = system().await;
            let parent = root(&s, flavor).await;
            parent.send(Box::new(Cmd::Policy(all()))).await.unwrap();
            let mut children = Vec::new();
            let mut stats = Vec::new();
            for index in 0..3 {
                let counts = Arc::new(Stats::default());
                let actor = child(
                    &parent,
                    &counts,
                    Setup {
                        fail_construction: (index == 1).then_some(2),
                        ..Default::default()
                    },
                    None,
                )
                .await;
                report(&actor).await;
                children.push(actor);
                stats.push(counts);
            }
            assert!(children[0].send(Box::new(Cmd::Fail)).await.is_err());
            // Scope removal is the joined retirement boundary; an is_alive
            // transition alone does not prove that all destructors finished.
            bounded(async {
                loop {
                    let remaining = *parent
                        .send(Box::new(Cmd::Children))
                        .await
                        .unwrap()
                        .downcast::<usize>()
                        .unwrap();
                    if remaining == 0 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await;
            for (actor, counts) in children.iter().zip(&stats) {
                assert!(!actor.is_alive().await);
                assert!(actor.stop().await.is_err());
                assert!(
                    AsyncEngineRuntime::get_actor(s.as_ref(), &actor.path())
                        .await
                        .is_none()
                );
                assert_eq!(
                    counts.built.load(Ordering::SeqCst),
                    counts.drops.load(Ordering::SeqCst)
                );
                assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
                assert!(
                    !counts
                        .events
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|e| e.0 > 1 && e.1 == "init")
                );
                counts.affinity();
            }
            parent.stop().await.unwrap();
            s.as_ref().clone().shutdown().await.unwrap();
        }
    })
    .await;
}
