//! Opted-in derives use the same real lifecycle as hand-written dispatchers.
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
    typed_context::{TypedContext, TypedContextExt},
};
use parrot_api::{
    Message, ParrotTypedActor,
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision},
    typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedReceive},
    typed_channel::TypedActorRef,
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

enum Engine {
    Thread(Arc<ThreadActorSystem>),
    Actix(Arc<ActixActorSystem>),
}
impl Engine {
    async fn new(actix: bool) -> Self {
        if actix {
            Self::Actix(Arc::new(
                ActixActorSystem::with_settings(ActixSystemSettings {
                    arbiter_count: 2,
                    shutdown_timeout: Duration::from_secs(3),
                    ..Default::default()
                })
                .await
                .unwrap(),
            ))
        } else {
            Self::Thread(ThreadActorSystem::shared(ThreadActorSystemConfig {
                shared_pool_size: 2,
                shared_burst_workers_max: 0,
                shutdown_timeout: Duration::from_secs(3),
                ..Default::default()
            }))
        }
    }
    async fn spawn<A: ParrotTypedDispatch, M: ParrotMsgVariant<A>>(
        &self,
        actor: A,
    ) -> TypedActorRef<A, M> {
        match self {
            Self::Thread(s) => s.spawn_typed(actor, "/derived").await.unwrap(),
            Self::Actix(s) => s.spawn_typed(actor, "/derived").await.unwrap(),
        }
    }
    fn count(&self) -> usize {
        match self {
            Self::Thread(s) => s.actor_count(),
            Self::Actix(s) => s.status().active_actors,
        }
    }
    async fn shutdown(&self) {
        assert_eq!(self.count(), 0);
        match self {
            Self::Thread(s) => s.shutdown_internal().await.unwrap(),
            Self::Actix(s) => s.as_ref().clone().shutdown().await.unwrap(),
        }
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[derive(Default)]
struct Stats {
    built: AtomicUsize,
    init: AtomicUsize,
    handlers: AtomicUsize,
    hooks: AtomicUsize,
    drops: AtomicUsize,
    child_drops: AtomicUsize,
    cancelled: AtomicUsize,
    entered: Notify,
    events: Mutex<Vec<(usize, &'static str)>>,
}
impl Stats {
    fn event(&self, generation: usize, event: &'static str) {
        self.events.lock().unwrap().push((generation, event));
    }
}
#[derive(Clone, Copy)]
enum Mode {
    Ok,
    Error,
    Panic,
    Pending,
}
pub struct Query;
pub struct Fail;
impl Message for Query {
    type Result = (usize, TypedContext);
}
impl Message for Fail {
    type Result = ();
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Query, Fail), lifecycle)]
struct Service {
    stats: Arc<Stats>,
    generation: usize,
    init: Mode,
    stop: Mode,
}
impl Service {
    fn new(stats: &Arc<Stats>, init: Mode, stop: Mode) -> Self {
        let generation = stats.built.fetch_add(1, Ordering::SeqCst);
        Self {
            stats: stats.clone(),
            generation,
            init,
            stop,
        }
    }
}
struct Pending(Arc<Stats>);
impl Drop for Pending {
    fn drop(&mut self) {
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
    }
}
impl ::parrot_api::typed::TypedLifecycle for Service {
    fn init_with_context<'a>(
        &'a mut self,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert_eq!(ctx.path(), "/derived");
            assert!(ctx.parent().is_none());
            self.stats.init.fetch_add(1, Ordering::SeqCst);
            self.stats.event(self.generation, "init");
            let child = ctx
                .spawn_typed_child::<_, ChildQuery>(Child(self.stats.clone()))
                .await?;
            assert_eq!(child.ask(ChildQuery).await?, "/derived");
            self.stats.entered.notify_one();
            match self.init {
                Mode::Ok => Ok(()),
                Mode::Error => Err(ActorError::InitializationError("derived init error".into())),
                Mode::Panic => panic!("derived init panic"),
                Mode::Pending => {
                    let _guard = Pending(self.stats.clone());
                    std::future::pending().await
                }
            }
        })
    }
    fn before_stop_with_context<'a>(
        &'a mut self,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert_eq!(ctx.path(), "/derived");
            assert!(ctx.children().is_empty());
            assert_eq!(
                self.stats.child_drops.load(Ordering::SeqCst),
                self.stats.built.load(Ordering::SeqCst)
            );
            self.stats.hooks.fetch_add(1, Ordering::SeqCst);
            self.stats.event(self.generation, "hook");
            match self.stop {
                Mode::Ok => Ok(()),
                Mode::Error => Err(ActorError::InternalError("derived stop error".into())),
                Mode::Panic => panic!("derived stop panic"),
                Mode::Pending => unreachable!(),
            }
        })
    }
}
impl TypedReceive<Query> for Service {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Query,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<(usize, TypedContext)>> {
        Box::pin(async move {
            self.stats.handlers.fetch_add(1, Ordering::SeqCst);
            self.stats.event(self.generation, "dispatch");
            Ok((self.generation, ctx.clone()))
        })
    }
}
impl TypedReceive<Fail> for Service {
    fn receive_typed(&mut self, _: Fail) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async {
            Err(ActorError::MessageHandlingError(
                "derived handler error".into(),
            ))
        })
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.stats.event(self.generation, "drop");
        self.stats.drops.fetch_add(1, Ordering::SeqCst);
    }
}
pub struct ChildQuery;
impl Message for ChildQuery {
    type Result = String;
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(ChildQuery))]
struct Child(Arc<Stats>);
impl TypedReceive<ChildQuery> for Child {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: ChildQuery,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<String>> {
        Box::pin(async move { Ok(ctx.parent().unwrap().path().to_owned()) })
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        self.0.child_drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[actix::test]
async fn derived_context_hooks_own_real_children_and_run_in_lifecycle_order() {
    for actix in [false, true] {
        let e = Engine::new(actix).await;
        let stats = Arc::new(Stats::default());
        let actor = e
            .spawn::<_, Query>(Service::new(&stats, Mode::Ok, Mode::Ok))
            .await;
        let (generation, ctx) = actor.ask(Query).await.unwrap();
        assert_eq!(generation, 0);
        assert_eq!(ctx.children().len(), 1);
        actor.stop().await.unwrap();
        assert_eq!(
            *stats.events.lock().unwrap(),
            vec![(0, "init"), (0, "dispatch"), (0, "hook"), (0, "drop")]
        );
        assert_eq!(stats.child_drops.load(Ordering::SeqCst), 1);
        e.shutdown().await;
    }
}
#[actix::test]
async fn derived_initialization_error_or_panic_never_dispatches_or_runs_normal_hook() {
    for actix in [false, true] {
        for (mode, diagnostic) in [
            (Mode::Error, "derived init error"),
            (Mode::Panic, "derived init panic"),
        ] {
            let e = Engine::new(actix).await;
            let stats = Arc::new(Stats::default());
            let actor = e
                .spawn::<_, Query>(Service::new(&stats, mode, Mode::Ok))
                .await;
            assert!(actor.ask(Query).await.is_err());
            until(|| e.count() == 0).await;
            assert!(
                actor
                    .stop()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(diagnostic)
            );
            assert_eq!(stats.handlers.load(Ordering::SeqCst), 0);
            assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
            assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
            assert_eq!(stats.child_drops.load(Ordering::SeqCst), 1);
            e.shutdown().await;
        }
    }
}
#[actix::test]
async fn derived_stop_error_or_panic_remains_observable_after_actual_retirement() {
    for actix in [false, true] {
        for (mode, diagnostic) in [
            (Mode::Error, "derived stop error"),
            (Mode::Panic, "derived stop panic"),
        ] {
            let e = Engine::new(actix).await;
            let stats = Arc::new(Stats::default());
            let actor = e
                .spawn::<_, Query>(Service::new(&stats, Mode::Ok, mode))
                .await;
            actor.ask(Query).await.unwrap();
            assert!(
                actor
                    .stop()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(diagnostic)
            );
            assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
            assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
            e.shutdown().await;
        }
    }
}
#[actix::test]
async fn derived_pending_init_is_cancelled_and_children_retire_without_stop_hook() {
    for actix in [false, true] {
        let e = Engine::new(actix).await;
        let stats = Arc::new(Stats::default());
        let actor = e
            .spawn::<_, Query>(Service::new(&stats, Mode::Pending, Mode::Ok))
            .await;
        tokio::time::timeout(Duration::from_secs(3), stats.entered.notified())
            .await
            .unwrap();
        actor.stop().await.unwrap();
        assert_eq!(stats.cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(stats.handlers.load(Ordering::SeqCst), 0);
        assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
        assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
        assert_eq!(stats.child_drops.load(Ordering::SeqCst), 1);
        e.shutdown().await;
    }
}
#[actix::test]
async fn derived_factory_hooks_follow_real_generations_and_original_reference() {
    for actix in [false, true] {
        let e = Engine::new(actix).await;
        let stats = Arc::new(Stats::default());
        let made = stats.clone();
        let factory = move || {
            let mode = if made.built.load(Ordering::SeqCst) == 0 {
                Mode::Error
            } else {
                Mode::Ok
            };
            Service::new(&made, mode, Mode::Ok)
        };
        let policy = OneForOneStrategy {
            max_restarts: 2,
            within: Duration::from_secs(30),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
        };
        let actor: TypedActorRef<Service, Query> = match &e {
            Engine::Thread(s) => s
                .spawn_typed_factory_with_strategy(factory, "/derived", policy)
                .await
                .unwrap(),
            Engine::Actix(s) => s
                .spawn_typed_factory_with_strategy(factory, "/derived", policy)
                .await
                .unwrap(),
        };
        let (generation, old) = actor.ask(Query).await.unwrap();
        assert_eq!(generation, 1);
        assert!(
            matches!(actor.ref_for::<Fail>().ask(Fail).await,Err(ActorError::MessageHandlingError(ref s)) if s=="derived handler error")
        );
        let (generation, _) = actor.ask(Query).await.unwrap();
        assert_eq!(generation, 2);
        let rejected = Arc::new(Stats::default());
        assert!(
            old.spawn_typed_child::<_, ChildQuery>(Child(rejected.clone()))
                .await
                .is_err()
        );
        assert_eq!(rejected.child_drops.load(Ordering::SeqCst), 1);
        assert_eq!(stats.child_drops.load(Ordering::SeqCst), 2);
        actor.stop().await.unwrap();
        assert_eq!(stats.init.load(Ordering::SeqCst), 3);
        assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
        assert_eq!(stats.drops.load(Ordering::SeqCst), 3);
        let events = stats.events.lock().unwrap().clone();
        for generation in [0, 1] {
            assert!(
                events
                    .iter()
                    .position(|x| *x == (generation, "drop"))
                    .unwrap()
                    < events
                        .iter()
                        .position(|x| *x == (generation + 1, "init"))
                        .unwrap()
            );
        }
        e.shutdown().await;
    }
}

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(ChildQuery), lifecycle)]
struct PlainHooks(Arc<Stats>);
impl ::parrot_api::typed::TypedLifecycle for PlainHooks {
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.0.init.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.0.hooks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
impl TypedReceive<ChildQuery> for PlainHooks {
    fn receive_typed(&mut self, _: ChildQuery) -> BoxedFuture<'_, ActorResult<String>> {
        Box::pin(async move {
            assert_eq!(self.0.init.load(Ordering::SeqCst), 1);
            Ok("ready".into())
        })
    }
}
#[actix::test]
async fn derived_contextual_defaults_call_context_free_hooks_in_both_engines() {
    for actix in [false, true] {
        let e = Engine::new(actix).await;
        let stats = Arc::new(Stats::default());
        let actor = e.spawn::<_, ChildQuery>(PlainHooks(stats.clone())).await;
        assert_eq!(actor.ask(ChildQuery).await.unwrap(), "ready");
        actor.stop().await.unwrap();
        assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
        e.shutdown().await;
    }
}
