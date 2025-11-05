//! An external runtime implementation executes real actors through the facade.
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{
        AsyncEngineRuntime, AsyncFactoryRuntime, FactoryOptions, InstanceConfig, RuntimeActor,
        RuntimeFactory, SpawnOptions,
    },
    system::{SystemError, SystemState, SystemStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
async fn bounded<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(8), f)
        .await
        .expect("facade contract deadline")
}
struct Echo;
impl Actor for Echo {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}
struct Extension {
    engine: Arc<parrot::thread::ThreadActorSystem>,
    shutdowns: AtomicUsize,
    panic_shutdown: bool,
    callback: Option<Box<dyn Fn() + Send + Sync>>,
}
impl Extension {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            engine: parrot::thread::ThreadActorSystem::shared(Default::default()),
            shutdowns: AtomicUsize::new(0),
            panic_shutdown: false,
            callback: None,
        })
    }
}
impl AsyncEngineRuntime for Extension {
    fn factory_runtime(&self) -> Option<&dyn AsyncFactoryRuntime> {
        Some(self)
    }
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        AsyncEngineRuntime::spawn(self.engine.as_ref(), actor, config, options)
    }
    fn get_actor<'a>(&'a self, path: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        AsyncEngineRuntime::get_actor(self.engine.as_ref(), path)
    }
    fn status(&self) -> SystemStatus {
        AsyncEngineRuntime::status(self.engine.as_ref())
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        if let Some(callback) = &self.callback {
            callback();
        }
        assert!(
            !self.panic_shutdown,
            "external runtime shutdown entry panicked"
        );
        AsyncEngineRuntime::shutdown(self.engine.as_ref())
    }
}
impl AsyncFactoryRuntime for Extension {
    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        AsyncFactoryRuntime::spawn_factory(self.engine.as_ref(), factory, options)
    }
}
async fn echo(reference: &BoxedActorRef) {
    assert_eq!(
        *bounded(reference.send(Box::new(42_u64)))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
}
#[tokio::test]
async fn external_runtime_executes_instances_factories_lookup_and_owned_shutdown() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let external = Extension::new();
    assert!(
        facade
            .register_runtime("extension".into(), external.clone(), true)
            .unwrap()
            .is_none()
    );
    let root = facade
        .spawn(Box::new(Echo), Box::new(()), SpawnOptions::at("/instance"))
        .await
        .unwrap();
    echo(&root).await;
    let factory = facade
        .factory_runtime()
        .unwrap()
        .spawn_factory(
            Box::new(|| Box::new(Echo)),
            FactoryOptions {
                spawn: SpawnOptions::at("/factory"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    echo(&factory).await;
    assert_eq!(
        facade
            .get_actor("/instance")
            .await
            .unwrap()
            .death_watch()
            .unwrap()
            .id(),
        root.death_watch().unwrap().id()
    );
    assert_eq!(
        facade
            .get_actor("/factory")
            .await
            .unwrap()
            .death_watch()
            .unwrap()
            .id(),
        factory.death_watch().unwrap().id()
    );
    assert_eq!(facade.status().active_actors, 2);
    bounded(facade.shutdown_runtime()).await.unwrap();
    assert_eq!(facade.status().state, SystemState::Stopped);
    assert_eq!(external.shutdowns.load(Ordering::SeqCst), 1);
    assert!(!root.is_alive().await && !factory.is_alive().await);
    facade.shutdown_runtime().await.unwrap();
    assert_eq!(external.shutdowns.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn replacement_and_removal_do_not_redirect_captured_creation_or_stop_old_runtime() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let old = Extension::new();
    let new = Extension::new();
    facade
        .register_runtime("same".into(), old.clone(), true)
        .unwrap();
    let pending = facade.spawn_on(
        "same",
        Box::new(Echo),
        Box::new(()),
        SpawnOptions::at("/captured"),
    );
    let retired = facade
        .register_runtime("same".into(), new.clone(), true)
        .unwrap()
        .unwrap();
    let reference = pending.await.unwrap();
    echo(&reference).await;
    assert!(old.get_actor("/captured").await.is_some());
    assert!(new.get_actor("/captured").await.is_none());
    let removed = facade.unregister_system("same").unwrap().unwrap();
    assert!(facade.runtime("same").is_err());
    assert!(reference.is_alive().await);
    facade.shutdown_runtime().await.unwrap();
    assert_eq!(old.shutdowns.load(Ordering::SeqCst), 0);
    assert_eq!(new.shutdowns.load(Ordering::SeqCst), 0);
    retired.shutdown().await.unwrap();
    removed.shutdown().await.unwrap();
}
#[tokio::test]
async fn concurrent_waiters_and_external_aliases_share_one_shutdown() {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let runtime = Extension::new();
    facade
        .register_runtime("a".into(), runtime.clone(), true)
        .unwrap();
    facade
        .register_runtime("b".into(), runtime.clone(), false)
        .unwrap();
    facade
        .spawn(Box::new(Echo), Box::new(()), SpawnOptions::at("/alias"))
        .await
        .unwrap();
    assert_eq!(facade.status().active_actors, 1);
    let mut waiters = Vec::new();
    for _ in 0..8 {
        let facade = facade.clone();
        waiters.push(tokio::spawn(async move { facade.shutdown_runtime().await }));
    }
    for waiter in waiters {
        bounded(waiter).await.unwrap().unwrap();
    }
    assert_eq!(runtime.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(facade.status().state, SystemState::Stopped);
}
#[actix::test]
async fn mixed_native_and_open_aliases_preserve_physical_engine_identity() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
    let actix = parrot::actix::ActixActorSystem::with_arbiter_count(1)
        .await
        .unwrap();
    facade
        .register_thread_system("thread-typed".into(), thread.clone(), true)
        .await
        .unwrap();
    facade
        .register_actix_system("actix-typed".into(), actix.clone(), false)
        .await
        .unwrap();
    facade
        .register_runtime("thread-open".into(), thread.clone(), false)
        .unwrap();
    facade
        .register_runtime("actix-open".into(), Arc::new(actix.clone()), false)
        .unwrap();
    for name in ["thread-open", "actix-open"] {
        let reference = facade
            .spawn_on(
                name,
                Box::new(Echo),
                Box::new(()),
                SpawnOptions::at(format!("/{name}")),
            )
            .await
            .unwrap();
        echo(&reference).await;
    }
    assert_eq!(facade.status().active_actors, 2);
    assert_eq!(
        facade.status().resources.thread_count,
        AsyncEngineRuntime::status(thread.as_ref())
            .resources
            .thread_count
            + actix.status().resources.thread_count
    );
    facade.shutdown_runtime().await.unwrap();
    assert_eq!(
        AsyncEngineRuntime::status(thread.as_ref()).state,
        SystemState::Stopped
    );
    assert_eq!(actix.status().state, SystemState::Stopped);
}
#[tokio::test]
async fn failed_extension_does_not_skip_other_engines_and_error_is_stable() {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let healthy = Extension::new();
    let weak = Arc::downgrade(&facade);
    let faulty = Arc::new(Extension {
        engine: parrot::thread::ThreadActorSystem::shared(Default::default()),
        shutdowns: AtomicUsize::new(0),
        panic_shutdown: true,
        callback: Some(Box::new(move || {
            assert_eq!(
                weak.upgrade()
                    .unwrap()
                    .list_registered_systems()
                    .unwrap()
                    .len(),
                2
            );
        })),
    });
    facade
        .register_runtime("a-failed".into(), faulty.clone(), true)
        .unwrap();
    facade
        .register_runtime("z-healthy".into(), healthy.clone(), false)
        .unwrap();
    for name in ["a-failed", "z-healthy"] {
        facade
            .spawn_on(
                name,
                Box::new(Echo),
                Box::new(()),
                SpawnOptions::at("/root"),
            )
            .await
            .unwrap();
    }
    let first = bounded(facade.shutdown_runtime())
        .await
        .unwrap_err()
        .to_string();
    assert!(first.contains("external runtime shutdown entry panicked"));
    assert_eq!(healthy.status().state, SystemState::Stopped);
    assert_eq!(facade.status().state, SystemState::ShuttingDown);
    assert_eq!(faulty.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(healthy.shutdowns.load(Ordering::SeqCst), 1);
    faulty.engine.shutdown_internal().await.unwrap();
    assert_eq!(facade.status().state, SystemState::Stopped);
    assert_eq!(
        facade.shutdown_runtime().await.unwrap_err().to_string(),
        first
    );
    assert_eq!(faulty.shutdowns.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn creation_and_engine_registration_close_after_shutdown_starts() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let runtime = Extension::new();
    facade
        .register_runtime("default".into(), runtime, true)
        .unwrap();
    let pending = facade.spawn(Box::new(Echo), Box::new(()), SpawnOptions::at("/late"));
    facade.shutdown_runtime().await.unwrap();
    assert!(matches!(pending.await, Err(SystemError::ShuttingDown)));
    let rejected = Extension::new();
    assert!(matches!(
        facade.register_runtime("late".into(), rejected.clone(), false),
        Err(SystemError::ShuttingDown)
    ));
    assert_eq!(rejected.status().state, SystemState::Running);
    assert!(matches!(
        facade.unregister_system("default"),
        Err(SystemError::ShuttingDown)
    ));
    assert!(matches!(
        facade.set_default_system("default"),
        Err(SystemError::ShuttingDown)
    ));
    assert!(facade.factory_runtime().is_none());
    rejected.shutdown().await.unwrap();
}
struct Hostile(Arc<AtomicUsize>);
impl Drop for Hostile {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("facade input retirement");
    }
}
struct HostileActor(Hostile);
impl Actor for HostileActor {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        let _ = &self.0;
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(m) })
    }
}
#[tokio::test]
async fn absent_closed_and_unpolled_facade_inputs_retire_separately() {
    use futures::FutureExt;
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    for mode in 0..3 {
        if mode == 2 {
            facade.shutdown_runtime().await.unwrap();
        }
        let count = Arc::new(AtomicUsize::new(0));
        let future = facade.spawn(
            Box::new(HostileActor(Hostile(count.clone()))),
            Box::new(Hostile(count.clone())),
            Default::default(),
        );
        if mode == 0 {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok()
            );
        } else {
            assert!(matches!(
                std::panic::AssertUnwindSafe(future).catch_unwind().await,
                Ok(Err(_))
            ));
        }
        bounded(async {
            while count.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let capture = Hostile(count.clone());
        let factory = facade.spawn_factory_on(
            "missing",
            Box::new(move || {
                let _ = &capture;
                Box::new(Echo)
            }),
            Default::default(),
        );
        if mode == 0 {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(factory))).is_ok()
            );
        } else {
            assert!(matches!(
                std::panic::AssertUnwindSafe(factory).catch_unwind().await,
                Ok(Err(_))
            ));
        }
        bounded(async {
            while count.load(Ordering::SeqCst) != 3 {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
}
struct BlockDrop {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    retired: Arc<AtomicUsize>,
}
impl Drop for BlockDrop {
    fn drop(&mut self) {
        self.entered.take().unwrap().send(()).unwrap();
        self.release
            .get_mut()
            .unwrap()
            .recv_timeout(Duration::from_secs(8))
            .unwrap();
        self.retired.fetch_add(1, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn facade_timeout_keeps_blocked_copier_cleanup_owned_and_result_stable() {
    let facade = Arc::new(
        ParrotActorSystem::new(parrot_api::system::ActorSystemConfig {
            timeouts: parrot_api::system::SystemTimeouts {
                system_shutdown: Duration::from_millis(100),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let engine = Extension::new();
    facade
        .register_runtime("live".into(), engine.clone(), true)
        .unwrap();
    facade
        .spawn(Box::new(Echo), Box::new(()), Default::default())
        .await
        .unwrap();
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let count = Arc::new(AtomicUsize::new(0));
    let capture = BlockDrop {
        entered: Some(entered),
        release: std::sync::Mutex::new(held),
        retired: count.clone(),
    };
    facade
        .register_mirror_copier(parrot::mirror::MirrorCopier::cloned::<u64>(move |_| {
            let _ = &capture;
            8
        }))
        .unwrap();
    let waiter = {
        let facade = facade.clone();
        tokio::spawn(async move { facade.shutdown_runtime().await })
    };
    bounded(observed).await.unwrap();
    let error = bounded(waiter).await.unwrap().unwrap_err().to_string();
    assert!(error.contains("timed out"));
    assert_eq!(facade.status().state, SystemState::ShuttingDown);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    // Prove the engine can finish while copier cleanup is still blocked. Its
    // completion need not precede the facade's deliberately short receipt deadline.
    bounded(async {
        while engine.status().state != SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await;
    release.send(()).unwrap();
    bounded(async {
        while facade.status().state != SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        facade.shutdown_runtime().await.unwrap_err().to_string(),
        error
    );
    assert_eq!(engine.shutdowns.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn hostile_mirror_fields_retire_independently_and_shutdown_error_is_stable() {
    use parrot::mirror::{MirrorCopier, MirrorError};
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let measure = Hostile(count.clone());
    let copy = Hostile(count.clone());
    facade
        .register_mirror_copier(MirrorCopier::new::<u64>(
            move |_| {
                let _ = &measure;
                8
            },
            move |value| {
                let _ = &copy;
                Ok(*value)
            },
        ))
        .unwrap();
    let first = facade.shutdown_runtime().await.unwrap_err().to_string();
    assert!(first.contains("facade input retirement"));
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(
        facade.shutdown_runtime().await.unwrap_err().to_string(),
        first
    );
    let capture = Hostile(count.clone());
    assert!(matches!(
        facade.register_mirror_copier(MirrorCopier::cloned::<u32>(move |_| {
            let _ = &capture;
            4
        })),
        Err(MirrorError::CopierDisposal {
            committed: false,
            ..
        })
    ));
    assert_eq!(count.load(Ordering::SeqCst), 3);
}
#[tokio::test]
async fn facade_rejects_registering_itself() {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    assert!(
        facade
            .register_runtime("self".into(), facade.clone(), true)
            .is_err()
    );
    assert!(facade.list_registered_systems().unwrap().is_empty());
    facade.shutdown_runtime().await.unwrap();
}

#[tokio::test]
async fn nested_facades_forward_real_work_and_reject_indirect_cycles() {
    let a = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let b = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let c = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let external = Extension::new();
    c.register_runtime("leaf".into(), external.clone(), true)
        .unwrap();
    b.register_runtime("nested".into(), c.clone(), true)
        .unwrap();
    a.register_runtime("nested".into(), b.clone(), true)
        .unwrap();
    assert!(
        c.register_runtime("cycle".into(), a.clone(), false)
            .is_err()
    );
    let actor = a
        .spawn(Box::new(Echo), Box::new(()), SpawnOptions::at("/nested"))
        .await
        .unwrap();
    echo(&actor).await;
    assert!(a.get_actor("/nested").await.is_some());
    bounded(a.shutdown_runtime()).await.unwrap();
    assert_eq!(external.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(c.status().state, SystemState::Stopped);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_opposite_facade_registrations_cannot_commit_a_cycle() {
    let a = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let b = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let start = Arc::new(tokio::sync::Barrier::new(2));
    let first = {
        let (a, b, start) = (a.clone(), b.clone(), start.clone());
        tokio::spawn(async move {
            start.wait().await;
            a.register_runtime("edge".into(), b, true).is_ok()
        })
    };
    let second = {
        let (a, b, start) = (a.clone(), b.clone(), start.clone());
        tokio::spawn(async move {
            start.wait().await;
            b.register_runtime("edge".into(), a, true).is_ok()
        })
    };
    assert_ne!(
        bounded(first).await.unwrap(),
        bounded(second).await.unwrap()
    );
    bounded(a.shutdown_runtime()).await.unwrap();
    bounded(b.shutdown_runtime()).await.unwrap();
}
#[tokio::test]
async fn removing_or_replacing_a_facade_edge_releases_cycle_restrictions() {
    for native_replace in [false, true] {
        let a = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
        let b = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
        a.register_runtime("edge".into(), b.clone(), true).unwrap();
        if native_replace {
            a.register_thread_system(
                "edge".into(),
                parrot::thread::ThreadActorSystem::shared(Default::default()),
                true,
            )
            .await
            .unwrap();
        } else {
            drop(a.unregister_system("edge").unwrap());
        }
        b.register_runtime("reverse".into(), a.clone(), true)
            .unwrap();
        bounded(b.shutdown_runtime()).await.unwrap();
        assert_eq!(a.status().state, SystemState::Stopped);
    }
}

#[derive(Clone)]
struct Increment;
impl parrot_api::message::Message for Increment {
    type Result = ();
}
struct Count(u64);
impl Actor for Count {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<Increment>() {
                self.0 += 1;
            }
            Ok(Box::new(self.0) as BoxedMessage)
        })
    }
}
#[tokio::test]
async fn unsupported_broadcast_rejects_before_delivering_to_any_native_engine() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let external = Extension::new();
    let native = parrot::thread::ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("native".into(), native.clone(), true)
        .await
        .unwrap();
    facade
        .register_runtime("extension".into(), external, false)
        .unwrap();
    let counter = AsyncEngineRuntime::spawn(
        native.as_ref(),
        Box::new(Count(0)),
        Box::new(()),
        Default::default(),
    )
    .await
    .unwrap();
    let failure = facade.internal_broadcast(Increment).await.unwrap_err();
    assert!(matches!(&failure, SystemError::Other(_)));
    assert!(failure.to_string().contains("does not expose broadcast"));
    let count = *bounded(counter.send(Box::new(())))
        .await
        .unwrap()
        .downcast::<u64>()
        .unwrap();
    bounded(facade.shutdown_runtime()).await.unwrap();
    assert_eq!(
        count, 0,
        "unsupported broadcast partially delivered before rejection"
    );
}

// A deliberately non-validating provider records entry. Facade validation must
// reject malformed options before calling the provider, not rely on its behavior.
struct AdmissionProbe {
    calls: AtomicUsize,
}
impl AsyncEngineRuntime for AdmissionProbe {
    fn factory_runtime(&self) -> Option<&dyn AsyncFactoryRuntime> {
        Some(self)
    }
    fn spawn<'a>(
        &'a self,
        _actor: RuntimeActor,
        _config: InstanceConfig,
        _options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(SystemError::ShuttingDown) })
    }
    fn get_actor<'a>(&'a self, _: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async { None })
    }
    fn status(&self) -> SystemStatus {
        SystemStatus {
            state: SystemState::Stopped,
            active_actors: 0,
            uptime: Duration::ZERO,
            resources: parrot_api::system::SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: 0,
            },
        }
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
}
impl AsyncFactoryRuntime for AdmissionProbe {
    fn spawn_factory<'a>(
        &'a self,
        _factory: RuntimeFactory,
        _options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(SystemError::ShuttingDown) })
    }
}
#[tokio::test]
async fn invalid_options_never_enter_external_creation_callbacks() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let provider = Arc::new(AdmissionProbe {
        calls: AtomicUsize::new(0),
    });
    facade
        .register_runtime("probe".into(), provider.clone(), true)
        .unwrap();
    for path in ["".to_string(), "a\nb".to_string(), "x".repeat(4097)] {
        let result = facade
            .spawn(Box::new(Echo), Box::new(()), SpawnOptions::at(path.clone()))
            .await;
        assert!(matches!(result, Err(SystemError::ActorCreationError(_))));
        let result = facade
            .spawn_factory_on(
                "probe",
                Box::new(|| Box::new(Echo)),
                FactoryOptions {
                    spawn: SpawnOptions::at(path),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(result, Err(SystemError::ActorCreationError(_))));
    }
    let result = facade
        .spawn_factory_on(
            "probe",
            Box::new(|| Box::new(Echo)),
            FactoryOptions {
                within: Duration::ZERO,
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(SystemError::ActorCreationError(_))));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    bounded(facade.shutdown_runtime()).await.unwrap();
}

#[actix::test]
async fn native_registration_also_closes_after_facade_shutdown() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    facade.shutdown_runtime().await.unwrap();
    let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
    let actix = parrot::actix::ActixActorSystem::with_arbiter_count(1)
        .await
        .unwrap();
    assert!(matches!(
        facade
            .register_thread_system("thread".into(), thread.clone(), true)
            .await,
        Err(SystemError::ShuttingDown)
    ));
    assert!(matches!(
        facade
            .register_actix_system("actix".into(), actix.clone(), true)
            .await,
        Err(SystemError::ShuttingDown)
    ));
    assert!(facade.list_registered_systems().unwrap().is_empty());
    assert_eq!(facade.status().state, SystemState::Stopped);
    AsyncEngineRuntime::shutdown(thread.as_ref()).await.unwrap();
    AsyncEngineRuntime::shutdown(&actix).await.unwrap();
}

struct InstanceOnly(Arc<Extension>);
impl AsyncEngineRuntime for InstanceOnly {
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.0.spawn(actor, config, options)
    }
    fn get_actor<'a>(&'a self, path: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        self.0.get_actor(path)
    }
    fn status(&self) -> SystemStatus {
        self.0.status()
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        self.0.shutdown()
    }
}
#[tokio::test]
async fn absent_factory_capability_retires_capture_without_running_constructor() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    facade
        .register_runtime(
            "instance".into(),
            Arc::new(InstanceOnly(Extension::new())),
            true,
        )
        .unwrap();
    assert!(facade.factory_runtime().is_none());
    let count = Arc::new(AtomicUsize::new(0));
    let capture = Hostile(count.clone());
    let entered = Arc::new(AtomicUsize::new(0));
    let recorded = entered.clone();
    let result = bounded(facade.spawn_factory_on(
        "instance",
        Box::new(move || {
            let _ = &capture;
            recorded.fetch_add(1, Ordering::SeqCst);
            Box::new(Echo)
        }),
        Default::default(),
    ))
    .await;
    assert!(matches!(result, Err(SystemError::ActorError(_))));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(entered.load(Ordering::SeqCst), 0);
    bounded(facade.shutdown_runtime()).await.unwrap();
}
