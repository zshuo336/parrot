use futures::FutureExt;
use parrot::actix::system::ActixActorSystem;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState};
use parrot_api::context::ActorContext;
use parrot_api::system::ActorSystem;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::any::Any;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Erased(Arc<AtomicUsize>);
impl Drop for Erased {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Erased {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
struct HostileConfig;
impl Drop for HostileConfig {
    fn drop(&mut self) {
        panic!("boxed configuration destructor failure");
    }
}

#[tokio::test]
async fn thread_boxed_config_failure_is_a_returned_creation_error() {
    let system = ThreadActorSystem::shared(Default::default());
    let dropped = Arc::new(AtomicUsize::new(0));
    let result = std::panic::AssertUnwindSafe(
        system.spawn_root_boxed(Box::new(Erased(dropped.clone())), Box::new(HostileConfig)),
    )
    .catch_unwind()
    .await;
    system.shutdown_internal().await.unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(
        matches!(result, Ok(Err(_))),
        "configuration destructor escaped the creation boundary"
    );
}
#[actix::test]
async fn actix_boxed_config_failure_is_a_returned_creation_error() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let dropped = Arc::new(AtomicUsize::new(0));
    let result = std::panic::AssertUnwindSafe(
        system.spawn_root_boxed(Box::new(Erased(dropped.clone())), Box::new(HostileConfig)),
    )
    .catch_unwind()
    .await;
    system.shutdown().await.unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(
        matches!(result, Ok(Err(_))),
        "configuration destructor escaped the creation boundary"
    );
}

impl parrot_api::actor::ActorConfig for HostileConfig {}
struct Concrete;
impl Actor for Concrete {
    type Config = HostileConfig;
    type Context = parrot::thread::context::ThreadContext<Self>;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
async fn typed_creation(explicit_engine_config: bool) {
    let system = ThreadActorSystem::shared(Default::default());
    let result = std::panic::AssertUnwindSafe(async {
        if explicit_engine_config {
            system
                .spawn_typed_with_config(Concrete, HostileConfig, Default::default())
                .await
        } else {
            system
                .spawn_root_typed_thread(Concrete, HostileConfig)
                .await
        }
    })
    .catch_unwind()
    .await;
    let registered_after_failure = system.actor_count();
    system.shutdown_internal().await.unwrap();
    assert!(
        matches!(result, Ok(Err(_))),
        "typed creation unwound with {registered_after_failure} actors still registered"
    );
    assert_eq!(
        registered_after_failure, 0,
        "a failed creation retained a published actor"
    );
}
#[tokio::test]
async fn thread_typed_root_config_failure_does_not_leave_a_published_actor() {
    typed_creation(false).await;
}
#[tokio::test]
async fn thread_typed_engine_config_failure_does_not_leave_a_published_actor() {
    typed_creation(true).await;
}

#[derive(Default)]
struct Events {
    config: AtomicUsize,
    init: AtomicUsize,
    stop: AtomicUsize,
    actor: AtomicUsize,
}
struct ConfigProbe {
    events: Arc<Events>,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Option<std::sync::mpsc::Receiver<()>>,
    panic: bool,
}
impl parrot_api::actor::ActorConfig for ConfigProbe {}
impl Drop for ConfigProbe {
    fn drop(&mut self) {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        if let Some(release) = self.release.take() {
            let _ = release.recv_timeout(std::time::Duration::from_secs(10));
        }
        self.events.config.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "configuration retirement failed");
    }
}
struct ThreadProbe {
    events: Arc<Events>,
    panic_drop: bool,
}
struct BoxedProbe {
    events: Arc<Events>,
    panic_drop: bool,
}
macro_rules! probe_actor {
    ($actor:ident, $config:ty, $context:ty) => {
        impl Drop for $actor {
            fn drop(&mut self) {
                self.events.actor.fetch_add(1, Ordering::SeqCst);
                assert!(!self.panic_drop, "actor input retirement failed");
            }
        }
        impl Actor for $actor {
            type Config = $config;
            type Context = $context;
            fn init<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                assert_eq!(
                    self.events.config.load(Ordering::SeqCst),
                    1,
                    "actor initialized before config retired"
                );
                self.events.init.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async { Ok(msg) })
            }
            fn before_stop<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                self.events.stop.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }
    };
}
probe_actor!(
    ThreadProbe,
    ConfigProbe,
    parrot::thread::context::ThreadContext<Self>
);
probe_actor!(BoxedProbe, Box<dyn Any + Send>, dyn ActorContext);
fn simple_config(events: &Arc<Events>, panic: bool) -> ConfigProbe {
    ConfigProbe {
        events: events.clone(),
        entered: None,
        release: None,
        panic,
    }
}
fn blocked_config(
    events: &Arc<Events>,
) -> (
    ConfigProbe,
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, receipt) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    (
        ConfigProbe {
            events: events.clone(),
            entered: Some(entered),
            release: Some(held),
            panic: false,
        },
        receipt,
        release,
    )
}
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(std::time::Duration::from_secs(3), future)
        .await
        .expect("caller runtime did not progress")
}

#[tokio::test]
async fn thread_cancelled_creation_keeps_inputs_owned_through_system_shutdown() {
    use parrot_api::system::SystemState;
    let system = ThreadActorSystem::shared(Default::default());
    let events = Arc::new(Events::default());
    let (config, entered, release) = blocked_config(&events);
    let owner = system.clone();
    let actor = ThreadProbe {
        events: events.clone(),
        panic_drop: false,
    };
    let caller = tokio::spawn(async move { owner.spawn_root_typed_thread(actor, config).await });
    bounded(entered).await.unwrap();
    assert_eq!(
        system.actor_count(),
        1,
        "pending creation lost registry ownership"
    );
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let owner = system.clone();
    let stop = tokio::spawn(async move { owner.shutdown_internal().await });
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(!stop.is_finished());
    assert_eq!(system.status().state, SystemState::ShuttingDown);
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
    assert_eq!(events.actor.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    bounded(stop).await.unwrap().unwrap();
    assert_eq!(system.actor_count(), 0);
    assert_eq!(system.status().state, SystemState::Stopped);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn thread_creation_deadline_does_not_release_pending_inputs_or_activate_actor() {
    let system = ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
        shutdown_timeout: std::time::Duration::from_millis(30),
        ..Default::default()
    });
    let events = Arc::new(Events::default());
    let (config, entered, release) = blocked_config(&events);
    let owner = system.clone();
    let actor = ThreadProbe {
        events: events.clone(),
        panic_drop: false,
    };
    let caller = tokio::spawn(async move { owner.spawn_root_typed_thread(actor, config).await });
    bounded(entered).await.unwrap();
    let result = bounded(caller).await.unwrap();
    assert!(
        matches!(result, Err(parrot::thread::SpawnError::Timeout(_))),
        "{result:?}"
    );
    assert_eq!(system.actor_count(), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    bounded(async {
        while system.actor_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    system.shutdown_internal().await.unwrap();
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn thread_success_retires_config_once_before_real_message_and_lifecycle() {
    use parrot_api::address::ActorRef;
    let system = ThreadActorSystem::shared(Default::default());
    let events = Arc::new(Events::default());
    let actor = system
        .spawn_root_typed_thread(
            ThreadProbe {
                events: events.clone(),
                panic_drop: false,
            },
            simple_config(&events, false),
        )
        .await
        .unwrap();
    let result = actor.send(Box::new(42_u64)).await.unwrap();
    assert_eq!(*result.downcast::<u64>().unwrap(), 42);
    system.shutdown_internal().await.unwrap();
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 1);
    assert_eq!(events.stop.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn actix_boxed_cancelled_creation_retains_real_inputs_until_shutdown() {
    use parrot_api::system::SystemState;
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let events = Arc::new(Events::default());
    let (config, entered, release) = blocked_config(&events);
    let owner = system.clone();
    let actor = BoxedProbe {
        events: events.clone(),
        panic_drop: false,
    };
    let caller = tokio::spawn(async move {
        owner
            .spawn_root_boxed(Box::new(actor), Box::new(config))
            .await
    });
    bounded(entered).await.unwrap();
    assert_eq!(system.status().active_actors, 1);
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let owner = system.clone();
    let stop = tokio::spawn(async move { owner.shutdown().await });
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(!stop.is_finished());
    assert_eq!(system.status().state, SystemState::ShuttingDown);
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    bounded(stop).await.unwrap().unwrap();
    assert_eq!(system.status().state, SystemState::Stopped);
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn actix_boxed_success_retires_config_once_before_real_message_and_lifecycle() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let events = Arc::new(Events::default());
    let actor = system
        .spawn_root_boxed(
            Box::new(BoxedProbe {
                events: events.clone(),
                panic_drop: false,
            }),
            Box::new(simple_config(&events, false)),
        )
        .await
        .unwrap();
    let result = actor.send(Box::new(42_u64)).await.unwrap();
    assert_eq!(*result.downcast::<u64>().unwrap(), 42);
    system.shutdown().await.unwrap();
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 1);
    assert_eq!(events.stop.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
}

#[test]
fn simultaneous_config_and_actor_destructor_panics_do_not_abort_the_process() {
    const CHILD: &str = "PARROT_INSTANCE_INPUT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "simultaneous_config_and_actor_destructor_panics_do_not_abort_the_process",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    actix::System::new().block_on(async {
        let events = Arc::new(Events::default());
        let thread = ThreadActorSystem::shared(Default::default());
        let result = thread
            .spawn_root_typed_thread(
                ThreadProbe {
                    events: events.clone(),
                    panic_drop: true,
                },
                simple_config(&events, true),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(thread.actor_count(), 0);
        assert_eq!(events.config.load(Ordering::SeqCst), 1);
        assert_eq!(events.actor.load(Ordering::SeqCst), 1);
        thread.shutdown_internal().await.unwrap();
        let events = Arc::new(Events::default());
        let actix = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let result = actix
            .spawn_root_boxed(
                Box::new(BoxedProbe {
                    events: events.clone(),
                    panic_drop: true,
                }),
                Box::new(simple_config(&events, true)),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(actix.status().active_actors, 0);
        assert_eq!(events.config.load(Ordering::SeqCst), 1);
        assert_eq!(events.actor.load(Ordering::SeqCst), 1);
        actix.shutdown().await.unwrap();
    });
}

#[tokio::test]
async fn rejected_thread_creation_waits_for_both_input_destructors() {
    let system = ThreadActorSystem::shared(Default::default());
    system.shutdown_internal().await.unwrap();
    let events = Arc::new(Events::default());
    let (config, entered, release) = blocked_config(&events);
    let owner = system.clone();
    let actor = ThreadProbe {
        events: events.clone(),
        panic_drop: false,
    };
    let caller = tokio::spawn(async move {
        owner
            .spawn_typed_with_config(actor, config, Default::default())
            .await
    });
    bounded(entered).await.unwrap();
    assert!(!caller.is_finished());
    release.send(()).unwrap();
    assert!(bounded(caller).await.unwrap().is_err());
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
    assert_eq!(system.actor_count(), 0);
}

#[tokio::test]
async fn unbound_thread_boxed_rejection_retires_inputs_without_unwinding() {
    let system = ThreadActorSystem::new(Default::default());
    let events = Arc::new(Events::default());
    let result = std::panic::AssertUnwindSafe(system.spawn_root_boxed(
        Box::new(BoxedProbe {
            events: events.clone(),
            panic_drop: true,
        }),
        Box::new(simple_config(&events, true)),
    ))
    .catch_unwind()
    .await;
    assert!(
        matches!(result, Ok(Err(_))),
        "unbound rejection escaped cleanup"
    );
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    system.shutdown_internal().await.unwrap();
}

#[actix::test]
async fn rejected_actix_boxed_creation_waits_for_both_input_destructors() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    system.clone().shutdown().await.unwrap();
    let events = Arc::new(Events::default());
    let (config, entered, release) = blocked_config(&events);
    let owner = system.clone();
    let actor = BoxedProbe {
        events: events.clone(),
        panic_drop: false,
    };
    let caller = tokio::spawn(async move {
        owner
            .spawn_root_boxed(Box::new(actor), Box::new(config))
            .await
    });
    bounded(entered).await.unwrap();
    assert!(!caller.is_finished());
    release.send(()).unwrap();
    assert!(bounded(caller).await.unwrap().is_err());
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
    assert_eq!(system.status().active_actors, 0);
}

#[tokio::test]
async fn recursive_config_panic_payload_destruction_cannot_lose_actor_retirement() {
    struct Secondary;
    impl Drop for Secondary {
        fn drop(&mut self) {
            panic!("secondary payload drop");
        }
    }
    struct Primary;
    impl Drop for Primary {
        fn drop(&mut self) {
            std::panic::panic_any(Secondary);
        }
    }
    struct Config;
    impl Drop for Config {
        fn drop(&mut self) {
            std::panic::panic_any(Primary);
        }
    }
    let system = ThreadActorSystem::shared(Default::default());
    let dropped = Arc::new(AtomicUsize::new(0));
    let result =
        bounded(system.spawn_root_boxed(Box::new(Erased(dropped.clone())), Box::new(Config))).await;
    match result {
        Err(error) => assert!(error.to_string().contains("payload destructor"), "{error}"),
        Ok(_) => panic!("hostile configuration failure became success"),
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn thread_erased_typed_payload_uses_owned_configuration_before_publication() {
    let system = ThreadActorSystem::shared(Default::default());
    let events = Arc::new(Events::default());
    let actor = parrot::thread::system::TypedSpawnPayload::new(ThreadProbe {
        events: events.clone(),
        panic_drop: false,
    })
    .into_boxed();
    let result = std::panic::AssertUnwindSafe(system.spawn_erased_actor(
        actor,
        Box::new(simple_config(&events, true)),
        None,
    ))
    .catch_unwind()
    .await;
    assert!(matches!(result, Ok(Err(_))));
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn malformed_erased_payloads_retire_both_inputs_without_unwinding() {
    let system = ThreadActorSystem::shared(Default::default());
    let events = Arc::new(Events::default());
    let actor: BoxedMessage = Box::new(ThreadProbe {
        events: events.clone(),
        panic_drop: true,
    });
    let config: BoxedMessage = Box::new(simple_config(&events, true));
    let result = std::panic::AssertUnwindSafe(system.spawn_erased_actor(actor, config, None))
        .catch_unwind()
        .await;
    assert!(matches!(result, Ok(Err(_))));
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn wrong_erased_configuration_retires_actor_and_configuration_separately() {
    let system = ThreadActorSystem::shared(Default::default());
    let events = Arc::new(Events::default());
    let actor = parrot::thread::system::TypedSpawnPayload::new(ThreadProbe {
        events: events.clone(),
        panic_drop: true,
    })
    .into_boxed();
    let result = std::panic::AssertUnwindSafe(system.spawn_erased_actor(
        actor,
        Box::new(HostileConfig),
        None,
    ))
    .catch_unwind()
    .await;
    assert!(matches!(result, Ok(Err(_))));
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn facade_boxed_creation_propagates_configuration_failure_without_leaking_actor() {
    let facade = parrot::system::ParrotActorSystem::new(Default::default())
        .await
        .unwrap();
    let system = ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("inputs".into(), system.clone(), true)
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let result = std::panic::AssertUnwindSafe(facade.spawn_root_boxed(
        Box::new(BoxedProbe {
            events: events.clone(),
            panic_drop: false,
        }),
        Box::new(simple_config(&events, true)),
    ))
    .catch_unwind()
    .await;
    assert!(matches!(result, Ok(Err(_))));
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(facade.status().active_actors, 0);
    facade.shutdown().await.unwrap();
}
