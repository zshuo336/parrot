//! Public facade entry points must preserve concrete protocols and native ownership.
use parrot::{
    actix::ActixActorSystem, system::ParrotActorSystem, thread::ThreadActorSystem,
    typed_context::TypedContextExt,
};
use parrot_api::{
    engine::AsyncEngineRuntime,
    errors::ActorError,
    message::Message,
    supervisor::{
        BasicDecisionFn, DefaultStrategy, OneForOneStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_context::TypedContext,
    types::{ActorResult, BoxedFuture},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Default)]
struct Stats {
    built: AtomicUsize,
    initialized: AtomicUsize,
    dropped: AtomicUsize,
    hooks: AtomicUsize,
}
struct Probe {
    stats: Arc<Stats>,
    generation: usize,
    value: usize,
    bad_drop: bool,
    stop: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
}
impl Probe {
    fn new(stats: &Arc<Stats>) -> Self {
        Self {
            stats: stats.clone(),
            generation: stats.built.fetch_add(1, Ordering::SeqCst),
            value: 0,
            bad_drop: false,
            stop: None,
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.stats.dropped.fetch_add(1, Ordering::SeqCst);
        assert!(!self.bad_drop, "static facade input destructor failed");
    }
}
enum Op {
    Read,
    Fail,
    Child,
}
impl Message for Op {
    type Result = (usize, usize, String);
}
impl ParrotMsgVariant<Probe> for Op {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: Self::Result) -> ActorResult<Self::Result> {
        Ok(reply)
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Op;
    type Reply = <Op as Message>::Result;
    fn dispatch(&mut self, _: Op) -> BoxedFuture<'_, ActorResult<Self::Reply>> {
        Box::pin(async {
            Err(ActorError::InternalError(
                "managed typed context required".into(),
            ))
        })
    }
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.stats.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.stats.hooks.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some((entered, release)) = self.stop.take() {
                let _ = entered.send(());
                let _ = release.await;
            }
            Ok(())
        })
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        op: Op,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<Self::Reply>> {
        Box::pin(async move {
            match op {
                Op::Read => {}
                Op::Fail => {
                    self.value += 1;
                    return Err(ActorError::MessageHandlingError("requested failure".into()));
                }
                Op::Child => {
                    let child = context
                        .spawn_typed_child::<_, Op>(Probe::new(&self.stats))
                        .await?;
                    let (_, _, path) = child.ask(Op::Read).await?;
                    assert!(path.starts_with(&format!("{}/", context.path())));
                    assert_eq!(context.children().len(), 1);
                    return Ok((self.generation, self.value, path));
                }
            }
            Ok((self.generation, self.value, context.path().into()))
        })
    }
}
async fn facade(actix: bool) -> ParrotActorSystem {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    register(&facade, "native", actix, true).await;
    facade
}
async fn register(facade: &ParrotActorSystem, name: &str, actix: bool, default: bool) {
    if actix {
        facade
            .register_actix_system(
                name.into(),
                ActixActorSystem::with_arbiter_count(1).await.unwrap(),
                default,
            )
            .await
            .unwrap();
    } else {
        facade
            .register_thread_system(
                name.into(),
                ThreadActorSystem::shared(Default::default()),
                default,
            )
            .await
            .unwrap();
    }
}
fn restart() -> OneForOneStrategy {
    OneForOneStrategy {
        max_restarts: 2,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    }
}
fn resume() -> SupervisorStrategyType {
    SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure)
}
async fn bounded(future: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(12), future)
        .await
        .unwrap();
}

#[actix::test]
async fn all_eight_routes_create_real_typed_mailboxes_and_join_inputs() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let stats = Arc::new(Stats::default());
            for route in 0..8 {
                let path = format!("/route-{route}");
                let made = stats.clone();
                let factory = move || Probe::new(&made);
                let actor = match route {
                    0 => system.spawn_typed::<_, Op>(Probe::new(&stats), &path).await,
                    1 => {
                        system
                            .spawn_typed_on::<_, Op>("native", Probe::new(&stats), &path)
                            .await
                    }
                    2 => {
                        system
                            .spawn_typed_with_strategy::<_, Op>(Probe::new(&stats), &path, resume())
                            .await
                    }
                    3 => {
                        system
                            .spawn_typed_with_strategy_on::<_, Op>(
                                "native",
                                Probe::new(&stats),
                                &path,
                                resume(),
                            )
                            .await
                    }
                    4 => system.spawn_typed_factory::<_, Op, _>(factory, &path).await,
                    5 => {
                        system
                            .spawn_typed_factory_on::<_, Op, _>("native", factory, &path)
                            .await
                    }
                    6 => {
                        system
                            .spawn_typed_factory_with_strategy::<_, Op, _>(
                                factory,
                                &path,
                                restart(),
                            )
                            .await
                    }
                    _ => {
                        system
                            .spawn_typed_factory_with_strategy_on::<_, Op, _>(
                                "native",
                                factory,
                                &path,
                                restart(),
                            )
                            .await
                    }
                }
                .unwrap();
                assert_eq!(actor.ask(Op::Read).await.unwrap(), (route, 0, path.clone()));
                assert!(
                    AsyncEngineRuntime::get_actor(&system, &path)
                        .await
                        .is_some()
                );
                actor.stop().await.unwrap();
                assert!(
                    AsyncEngineRuntime::get_actor(&system, &path)
                        .await
                        .is_none()
                );
            }
            AsyncEngineRuntime::shutdown(&system).await.unwrap();
            assert_eq!(stats.initialized.load(Ordering::SeqCst), 8);
            assert_eq!(stats.dropped.load(Ordering::SeqCst), 8);
            assert_eq!(stats.hooks.load(Ordering::SeqCst), 8);
        }
    })
    .await;
}

#[actix::test]
async fn factory_reconstruction_preserves_original_typed_reference_and_budget() {
    bounded(async {
        for actix in [false, true] {
            for named in [false, true] {
                let system = facade(actix).await;
                let stats = Arc::new(Stats::default());
                let made = stats.clone();
                let factory = move || Probe::new(&made);
                let actor = if named {
                    system
                        .spawn_typed_factory_with_strategy_on::<_, Op, _>(
                            "native",
                            factory,
                            "/factory",
                            restart(),
                        )
                        .await
                } else {
                    system
                        .spawn_typed_factory_with_strategy::<_, Op, _>(
                            factory,
                            "/factory",
                            restart(),
                        )
                        .await
                }
                .unwrap();
                for generation in 0..3 {
                    assert_eq!(actor.ask(Op::Read).await.unwrap().0, generation);
                    assert!(actor.ask(Op::Fail).await.is_err());
                }
                assert!(actor.ask(Op::Read).await.is_err());
                // The original actor retains its failure after leaving the registry.
                // A later system shutdown only joins actors still registered.
                let error = actor.stop().await.unwrap_err();
                assert!(error.to_string().contains("requested failure"), "{error}");
                assert_eq!(
                    actor.stop().await.unwrap_err().to_string(),
                    error.to_string()
                );
                assert!(
                    AsyncEngineRuntime::get_actor(&system, "/factory")
                        .await
                        .is_none()
                );
                AsyncEngineRuntime::shutdown(&system).await.unwrap();
                assert_eq!(stats.built.load(Ordering::SeqCst), 3);
                assert_eq!(stats.dropped.load(Ordering::SeqCst), 3);
            }
        }
    })
    .await;
}

#[actix::test]
async fn instance_resume_retains_state_through_both_public_routes() {
    bounded(async {
        for actix in [false, true] {
            for named in [false, true] {
                let system = facade(actix).await;
                let stats = Arc::new(Stats::default());
                let actor = if named {
                    system
                        .spawn_typed_with_strategy_on::<_, Op>(
                            "native",
                            Probe::new(&stats),
                            "/resume",
                            resume(),
                        )
                        .await
                } else {
                    system
                        .spawn_typed_with_strategy::<_, Op>(Probe::new(&stats), "/resume", resume())
                        .await
                }
                .unwrap();
                assert!(actor.ask(Op::Fail).await.is_err());
                assert_eq!(actor.ask(Op::Read).await.unwrap(), (0, 1, "/resume".into()));
                AsyncEngineRuntime::shutdown(&system).await.unwrap();
                assert_eq!(stats.dropped.load(Ordering::SeqCst), 1);
            }
        }
    })
    .await;
}

#[actix::test]
async fn typed_parent_owns_real_context_child_and_shutdown_joins_both() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let stats = Arc::new(Stats::default());
            let actor = system
                .spawn_typed::<_, Op>(Probe::new(&stats), "/parent")
                .await
                .unwrap();
            let path = actor.ask(Op::Child).await.unwrap().2;
            assert!(
                AsyncEngineRuntime::get_actor(&system, &path)
                    .await
                    .is_some()
            );
            AsyncEngineRuntime::shutdown(&system).await.unwrap();
            assert!(actor.ask(Op::Read).await.is_err());
            assert!(
                AsyncEngineRuntime::get_actor(&system, &path)
                    .await
                    .is_none()
            );
            assert_eq!(stats.dropped.load(Ordering::SeqCst), 2);
            assert_eq!(stats.hooks.load(Ordering::SeqCst), 2);
        }
    })
    .await;
}

#[actix::test]
async fn pending_default_and_named_creation_keep_original_engine_after_replacement() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let original = system.runtime("native").unwrap();
            let stats = Arc::new(Stats::default());
            let pending = system.spawn_typed::<_, Op>(Probe::new(&stats), "/captured-instance");
            let made = stats.clone();
            let factory = system.spawn_typed_factory_on::<_, Op, _>(
                "native",
                move || Probe::new(&made),
                "/captured-factory",
            );
            register(&system, "replacement", !actix, true).await;
            system.unregister_system("native").unwrap();
            register(&system, "native", !actix, false).await;
            let instance = pending.await.unwrap();
            let factory = factory.await.unwrap();
            assert_eq!(
                instance.ask(Op::Read).await.unwrap().2,
                "/captured-instance"
            );
            assert_eq!(factory.ask(Op::Read).await.unwrap().2, "/captured-factory");
            for path in ["/captured-instance", "/captured-factory"] {
                assert!(original.get_actor(path).await.is_some());
                assert!(
                    system
                        .runtime("native")
                        .unwrap()
                        .get_actor(path)
                        .await
                        .is_none()
                );
                assert!(
                    system
                        .runtime("replacement")
                        .unwrap()
                        .get_actor(path)
                        .await
                        .is_none()
                );
            }
            original.shutdown().await.unwrap();
            AsyncEngineRuntime::shutdown(&system).await.unwrap();
            assert_eq!(stats.dropped.load(Ordering::SeqCst), 2);
        }
    })
    .await;
}

#[actix::test]
async fn nested_facade_captures_leaf_before_inner_default_changes() {
    bounded(async {
        for actix in [false, true] {
            let inner = Arc::new(facade(actix).await);
            let leaf = inner.runtime("native").unwrap();
            let outer = ParrotActorSystem::new(Default::default()).await.unwrap();
            outer
                .register_runtime("nested".into(), inner.clone(), true)
                .unwrap();
            let stats = Arc::new(Stats::default());
            let pending = outer.spawn_typed::<_, Op>(Probe::new(&stats), "/nested");
            register(&inner, "new-default", !actix, true).await;
            let actor = pending.await.unwrap();
            assert_eq!(actor.ask(Op::Read).await.unwrap().2, "/nested");
            assert!(leaf.get_actor("/nested").await.is_some());
            assert!(
                inner
                    .runtime("new-default")
                    .unwrap()
                    .get_actor("/nested")
                    .await
                    .is_none()
            );
            AsyncEngineRuntime::shutdown(&outer).await.unwrap();
            assert_eq!(stats.dropped.load(Ordering::SeqCst), 1);
        }
    })
    .await;
}

struct FactoryCapture(Arc<AtomicUsize>, bool);
impl Drop for FactoryCapture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "static facade factory capture destructor failed");
    }
}
fn factory_capture(capture: FactoryCapture, stats: Arc<Stats>) -> impl Fn() -> Probe + Send + Sync {
    move || {
        let _keep = &capture;
        Probe::new(&stats)
    }
}

#[actix::test]
async fn unpolled_instance_factory_and_policy_retire_without_constructing() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let stats = Arc::new(Stats::default());
            let capture_drops = Arc::new(AtomicUsize::new(0));
            let policy_drops = Arc::new(AtomicUsize::new(0));
            let capture = FactoryCapture(policy_drops.clone(), true);
            let policy = OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(move |_| {
                    let _keep = &capture;
                    SupervisionDecision::Restart
                }),
            };
            let mut input = Probe::new(&stats);
            input.bad_drop = true;
            drop(system.spawn_typed_with_strategy::<_, Op>(
                input,
                "/unused-instance",
                SupervisorStrategyType::OneForOne(policy),
            ));
            drop(system.spawn_typed_factory_on::<_, Op, _>(
                "native",
                factory_capture(FactoryCapture(capture_drops.clone(), true), stats.clone()),
                "/unused-factory",
            ));
            while stats.dropped.load(Ordering::SeqCst) != 1
                || capture_drops.load(Ordering::SeqCst) != 1
                || policy_drops.load(Ordering::SeqCst) != 1
            {
                tokio::task::yield_now().await;
            }
            AsyncEngineRuntime::shutdown(&system).await.unwrap();
            assert_eq!(stats.built.load(Ordering::SeqCst), 1);
            assert_eq!(stats.initialized.load(Ordering::SeqCst), 0);
            assert_eq!(stats.dropped.load(Ordering::SeqCst), 1);
            assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
            assert_eq!(policy_drops.load(Ordering::SeqCst), 1);
        }
    })
    .await;
}

#[actix::test]
async fn missing_engine_and_closed_facade_return_cleanup_errors_after_retiring_all_inputs() {
    bounded(async {
        for closed in [false, true] {
            let system = facade(false).await;
            if closed {
                AsyncEngineRuntime::shutdown(&system).await.unwrap();
            }
            let stats = Arc::new(Stats::default());
            let mut input = Probe::new(&stats);
            input.bad_drop = true;
            let result = system
                .spawn_typed_on::<_, Op>("missing", input, "/rejected")
                .await;
            assert!(
                result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("disposal failed")
            );
            let drops = Arc::new(AtomicUsize::new(0));
            let result = system
                .spawn_typed_factory_on::<_, Op, _>(
                    "missing",
                    factory_capture(FactoryCapture(drops.clone(), true), stats.clone()),
                    "/rejected-factory",
                )
                .await;
            assert!(
                result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("disposal failed")
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(stats.dropped.load(Ordering::SeqCst), 1);
            assert_eq!(stats.initialized.load(Ordering::SeqCst), 0);
            AsyncEngineRuntime::shutdown(&system).await.unwrap();
        }
    })
    .await;
}

#[actix::test]
async fn captured_future_polled_after_facade_shutdown_cannot_publish() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let stats = Arc::new(Stats::default());
            let input = system.spawn_typed::<_, Op>(Probe::new(&stats), "/late");
            let made = stats.clone();
            let factory =
                system.spawn_typed_factory::<_, Op, _>(move || Probe::new(&made), "/late-factory");
            AsyncEngineRuntime::shutdown(&system).await.unwrap();
            assert!(matches!(input.await, Err(ActorError::Stopped)));
            assert!(matches!(factory.await, Err(ActorError::Stopped)));
            assert_eq!(stats.built.load(Ordering::SeqCst), 1);
            assert_eq!(stats.dropped.load(Ordering::SeqCst), 1);
            assert_eq!(stats.initialized.load(Ordering::SeqCst), 0);
        }
    })
    .await;
}

#[actix::test]
async fn opaque_native_paths_are_preserved_and_duplicates_do_not_construct() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let stats = Arc::new(Stats::default());
            let actor = system
                .spawn_typed::<_, Op>(Probe::new(&stats), "/occupied")
                .await
                .unwrap();
            actor.ask(Op::Read).await.unwrap();
            {
                let path = "/occupied";
                let drops = Arc::new(AtomicUsize::new(0));
                assert!(
                    system
                        .spawn_typed_factory_on::<_, Op, _>(
                            "native",
                            factory_capture(FactoryCapture(drops.clone(), false), stats.clone()),
                            path
                        )
                        .await
                        .is_err()
                );
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            }
            assert_eq!(stats.built.load(Ordering::SeqCst), 1);
            let opaque = system
                .spawn_typed::<_, Op>(Probe::new(&stats), "opaque native key")
                .await
                .unwrap();
            assert_eq!(opaque.ask(Op::Read).await.unwrap().2, "opaque native key");
            AsyncEngineRuntime::shutdown(&system).await.unwrap();
            assert_eq!(stats.dropped.load(Ordering::SeqCst), 2);
        }
    })
    .await;
}

#[actix::test]
async fn cancelled_stop_waiter_keeps_native_retirement_owned_until_shutdown_join() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let stats = Arc::new(Stats::default());
            let (entered, entry) = tokio::sync::oneshot::channel();
            let (release, gate) = tokio::sync::oneshot::channel();
            let mut input = Probe::new(&stats);
            input.stop = Some((entered, gate));
            let actor = system
                .spawn_typed::<_, Op>(input, "/cancelled-stop")
                .await
                .unwrap();
            actor.ask(Op::Read).await.unwrap();
            let mut stop = Box::pin(actor.stop());
            tokio::select! {
                result = &mut stop => panic!("stop completed before gate: {result:?}"),
                result = entry => result.unwrap(),
            }
            drop(stop);
            let mut shutdown = AsyncEngineRuntime::shutdown(&system);
            assert!(futures::poll!(shutdown.as_mut()).is_pending());
            assert_eq!(stats.dropped.load(Ordering::SeqCst), 0);
            release.send(()).unwrap();
            shutdown.await.unwrap();
            assert_eq!(stats.dropped.load(Ordering::SeqCst), 1);
            assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
        }
    })
    .await;
}

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (_guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(8), |open| {
                !*open
            })
            .unwrap();
        assert!(!timeout.timed_out(), "constructor test gate not released");
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
async fn cancelled_factory_stop_owns_late_constructor_output_without_initializing() {
    bounded(async {
        for actix in [false, true] {
            let system = facade(actix).await;
            let stats = Arc::new(Stats::default());
            let made = stats.clone();
            let gate = Arc::new(Gate::default());
            let _release = Release(gate.clone());
            let blocked = gate.clone();
            let actor = system
                .spawn_typed_factory_on::<_, Op, _>(
                    "native",
                    move || {
                        blocked.block();
                        Probe::new(&made)
                    },
                    "/late-constructor",
                )
                .await
                .unwrap();
            gate.entered.notified().await;
            let mut stop = Box::pin(actor.stop());
            assert!(futures::poll!(stop.as_mut()).is_pending());
            drop(stop);
            let mut shutdown = AsyncEngineRuntime::shutdown(&system);
            assert!(futures::poll!(shutdown.as_mut()).is_pending());
            assert_eq!(stats.built.load(Ordering::SeqCst), 0);
            gate.release();
            shutdown.await.unwrap();
            assert_eq!(stats.built.load(Ordering::SeqCst), 1);
            assert_eq!(stats.dropped.load(Ordering::SeqCst), 1);
            assert_eq!(stats.initialized.load(Ordering::SeqCst), 0);
            assert!(actor.ask(Op::Read).await.is_err());
        }
    })
    .await;
}

struct External(Arc<ThreadActorSystem>, Arc<AtomicUsize>);
impl AsyncEngineRuntime for External {
    fn spawn<'a>(
        &'a self,
        actor: parrot_api::engine::RuntimeActor,
        config: parrot_api::engine::InstanceConfig,
        options: parrot_api::engine::SpawnOptions,
    ) -> BoxedFuture<'a, Result<parrot_api::types::BoxedActorRef, parrot_api::system::SystemError>>
    {
        self.1.fetch_add(1, Ordering::SeqCst);
        AsyncEngineRuntime::spawn(self.0.as_ref(), actor, config, options)
    }
    fn get_actor<'a>(
        &'a self,
        path: &'a str,
    ) -> BoxedFuture<'a, Option<parrot_api::types::BoxedActorRef>> {
        AsyncEngineRuntime::get_actor(self.0.as_ref(), path)
    }
    fn status(&self) -> parrot_api::system::SystemStatus {
        AsyncEngineRuntime::status(self.0.as_ref())
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), parrot_api::system::SystemError>> {
        AsyncEngineRuntime::shutdown(self.0.as_ref())
    }
}

#[actix::test]
async fn external_dynamic_runtime_is_not_silently_used_as_static_mailbox() {
    bounded(async {
        let system = ParrotActorSystem::new(Default::default()).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        system
            .register_runtime(
                "external".into(),
                Arc::new(External(
                    ThreadActorSystem::shared(Default::default()),
                    calls.clone(),
                )),
                true,
            )
            .unwrap();
        let stats = Arc::new(Stats::default());
        let error = system
            .spawn_typed::<_, Op>(Probe::new(&stats), "/unsupported")
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("native static typed bridge"));
        let captures = Arc::new(AtomicUsize::new(0));
        let error = system
            .spawn_typed_factory_on::<_, Op, _>(
                "external",
                factory_capture(FactoryCapture(captures.clone(), false), stats.clone()),
                "/unsupported-factory",
            )
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("native static typed bridge"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(stats.built.load(Ordering::SeqCst), 1);
        assert_eq!(stats.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(stats.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        AsyncEngineRuntime::shutdown(&system).await.unwrap();
    })
    .await;
}
