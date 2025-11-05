use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::ActorContext,
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::time::Duration;
#[derive(Clone, Copy)]
enum Behavior {
    Ready,
    Pending,
    Error,
    Panic,
    PanicCreation,
}
struct Probe {
    events: tokio::sync::mpsc::UnboundedSender<&'static str>,
    initialized: bool,
    init_behavior: Behavior,
    hook_behavior: Behavior,
}
impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.events.send("actor_drop");
    }
}
impl ActixEngineExt for Probe {}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        if matches!(self.init_behavior, Behavior::PanicCreation) {
            let _ = self.events.send("init");
            panic!("initialization creation panic");
        }
        Box::pin(async move {
            let _ = self.events.send("init");
            match self.init_behavior {
                Behavior::Ready => {}
                Behavior::Pending => std::future::pending().await,
                Behavior::Error => {
                    return Err(ActorError::InitializationError(
                        "initialization rejected".into(),
                    ));
                }
                Behavior::Panic | Behavior::PanicCreation => panic!("initialization panic"),
            }
            self.initialized = true;
            Ok(())
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        if matches!(self.hook_behavior, Behavior::PanicCreation) {
            let _ = self.events.send("hook");
            panic!("hook panic during creation");
        }
        Box::pin(async move {
            let _ = self.events.send("hook");
            match self.hook_behavior {
                Behavior::Ready => Ok(()),
                Behavior::Pending => std::future::pending().await,
                Behavior::Error => Err(ActorError::MessageHandlingError("hook rejected".into())),
                Behavior::Panic | Behavior::PanicCreation => panic!("hook panic"),
            }
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match *message.downcast::<u8>().unwrap() {
                0 => Ok(Box::new(self.initialized) as BoxedMessage),
                1 => {
                    let _ = self.events.send("held");
                    std::future::pending().await
                }
                2 => {
                    ctx.stop().await?;
                    Ok(Box::new(()) as BoxedMessage)
                }
                3 => Ok(Box::new(ctx.get_self_ref().path()) as BoxedMessage),
                4 => panic!("dispatch panic"),
                5 => {
                    let _ = self.events.send("blocked");
                    std::thread::sleep(Duration::from_millis(250));
                    Ok(Box::new(()) as BoxedMessage)
                }
                _ => Err(ActorError::MessageHandlingError("unknown command".into())),
            }
        })
    }
}
async fn wait_for(events: &mut tokio::sync::mpsc::UnboundedReceiver<&'static str>, wanted: &str) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if events.recv().await.expect("events closed") == wanted {
                return;
            }
        }
    })
    .await
    .unwrap();
}
async fn wait_for_retirement(system: &ActixActorSystem, path: &String) {
    // A timeout receipt and an actor's Drop signal can both precede the
    // lifecycle monitor observing disposal and removing the registration.
    tokio::time::timeout(Duration::from_secs(1), async {
        while system.get_actor(path).await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed-out actor must eventually retire after disposal");
}
async fn spawn(
    system: &ActixActorSystem,
) -> (
    parrot_api::types::BoxedActorRef,
    tokio::sync::mpsc::UnboundedReceiver<&'static str>,
) {
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        system
            .spawn_root_typed(
                Probe {
                    events,
                    initialized: false,
                    init_behavior: Behavior::Ready,
                    hook_behavior: Behavior::Ready,
                },
                EmptyConfig,
            )
            .await
            .unwrap(),
        receiver,
    )
}
#[actix::test]
async fn initialization_precedes_dispatch() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let (actor, _events) = spawn(&system).await;
    let result = actor.send(Box::new(0u8)).await.unwrap();
    assert!(*result.downcast::<bool>().unwrap());
    system.shutdown().await.unwrap();
}
#[actix::test]
async fn stop_joins_hook_disposal_and_registry_removal() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let (actor, mut events) = spawn(&system).await;
    actor.send(Box::new(0u8)).await.unwrap();
    actor.stop().await.unwrap();
    let events: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    assert_eq!(events, ["init", "hook", "actor_drop"]);
    assert_eq!(system.status().active_actors, 0);
    assert!(!actor.is_alive().await);
    system.shutdown().await.unwrap();
}
#[actix::test]
async fn stop_cancels_pending_handler() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let (actor, mut events) = spawn(&system).await;
    actor.deliver(Box::new(1u8)).await.unwrap();
    wait_for(&mut events, "held").await;
    tokio::time::timeout(Duration::from_secs(1), actor.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !actor.is_alive().await,
        "stop must complete actual cancellation"
    );
    wait_for(&mut events, "actor_drop").await;
    system.shutdown().await.unwrap();
}
#[actix::test]
async fn context_stop_reaches_same_lifecycle() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let (actor, mut events) = spawn(&system).await;
    actor.deliver(Box::new(2u8)).await.unwrap();
    wait_for(&mut events, "actor_drop").await;
    assert!(!actor.is_alive().await);
    system.shutdown().await.unwrap();
}
#[actix::test]
async fn context_uses_registered_path() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let (actor, _events) = spawn(&system).await;
    let path = actor.send(Box::new(3u8)).await.unwrap();
    assert_eq!(*path.downcast::<String>().unwrap(), actor.path());
    system.shutdown().await.unwrap();
}

async fn spawn_config(
    system: &ActixActorSystem,
    init_behavior: Behavior,
    hook_behavior: Behavior,
) -> (
    parrot_api::types::BoxedActorRef,
    tokio::sync::mpsc::UnboundedReceiver<&'static str>,
) {
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        system
            .spawn_root_typed(
                Probe {
                    events,
                    initialized: false,
                    init_behavior,
                    hook_behavior,
                },
                EmptyConfig,
            )
            .await
            .unwrap(),
        receiver,
    )
}
#[actix::test]
async fn initialization_failure_and_cancellation_skip_dispatch_and_hook() {
    for behavior in [
        Behavior::Pending,
        Behavior::Error,
        Behavior::Panic,
        Behavior::PanicCreation,
    ] {
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let (actor, mut events) = spawn_config(&system, behavior, Behavior::Ready).await;
        wait_for(&mut events, "init").await;
        let result = actor.stop().await;
        match behavior {
            Behavior::Pending => assert!(result.is_ok()),
            _ => assert!(result.is_err()),
        }
        assert_eq!(
            events.recv().await.unwrap(),
            "actor_drop",
            "uninitialized actor must not run its hook"
        );
        assert!(actor.send(Box::new(0u8)).await.is_err());
        assert_eq!(system.status().active_actors, 0);
        system.shutdown().await.unwrap();
    }
}
#[actix::test]
async fn stop_reports_hook_errors_panics_and_deadlines_with_owned_cleanup() {
    for behavior in [
        Behavior::Error,
        Behavior::Panic,
        Behavior::PanicCreation,
        Behavior::Pending,
    ] {
        let system = ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: Duration::from_millis(50),
            ..Default::default()
        })
        .await
        .unwrap();
        let (actor, mut events) = spawn_config(&system, Behavior::Ready, behavior).await;
        wait_for(&mut events, "init").await;
        let error = actor.stop().await.expect_err("hook failure must propagate");
        match behavior {
            Behavior::Pending => assert!(matches!(error, ActorError::TimeoutDetail(_))),
            Behavior::Panic | Behavior::PanicCreation => assert!(
                matches!(error, ActorError::Panic(ref message) if message.contains("hook panic"))
            ),
            _ => assert!(error.to_string().contains("hook rejected")),
        }
        wait_for(&mut events, "actor_drop").await;
        wait_for_retirement(&system, &actor.path()).await;
        assert_eq!(system.status().active_actors, 0);
        system.shutdown().await.unwrap();
    }
}
#[actix::test]
async fn cancelled_stop_caller_does_not_cancel_cleanup() {
    let system = ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(50),
        ..Default::default()
    })
    .await
    .unwrap();
    let (actor, mut events) = spawn_config(&system, Behavior::Ready, Behavior::Pending).await;
    wait_for(&mut events, "init").await;
    let stopping = actor.clone_boxed();
    let task = tokio::spawn(async move { stopping.stop().await });
    wait_for(&mut events, "hook").await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(matches!(
        actor.stop().await,
        Err(ActorError::TimeoutDetail(_))
    ));
    wait_for(&mut events, "actor_drop").await;
    wait_for_retirement(&system, &actor.path()).await;
    system.shutdown().await.unwrap();
}
#[actix::test]
async fn dispatch_panic_skips_hook_and_preserves_actual_reason() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let (actor, mut events) = spawn(&system).await;
    wait_for(&mut events, "init").await;
    actor
        .send(Box::new(4u8))
        .await
        .expect_err("panic must fail request");
    assert!(
        matches!(actor.stop().await, Err(ActorError::Panic(ref message)) if message.contains("dispatch panic"))
    );
    assert_eq!(events.recv().await.unwrap(), "actor_drop");
    system.shutdown().await.unwrap();
}
#[actix::test]
async fn synchronous_handler_cannot_hold_shutdown_timer_and_late_hook_is_skipped() {
    let system = ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(40),
        ..Default::default()
    })
    .await
    .unwrap();
    let (actor, mut events) = spawn(&system).await;
    actor.deliver(Box::new(5u8)).await.unwrap();
    wait_for(&mut events, "blocked").await;
    let result = tokio::time::timeout(Duration::from_millis(150), system.clone().shutdown())
        .await
        .unwrap();
    assert!(result.is_err());
    assert!(!actor.is_alive().await);
    assert_eq!(
        events.recv().await.unwrap(),
        "actor_drop",
        "expired stop must not construct a late hook"
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while system.status().state != parrot_api::system::SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
struct Tracked(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
#[actix::test]
async fn bounded_delivery_rejects_full_mailbox_and_stop_joins_queued_disposal() {
    let system = ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
        arbiter_count: 1,
        mailbox_capacity: 2,
        ..Default::default()
    })
    .await
    .unwrap();
    let (actor, mut events) = spawn(&system).await;
    actor.deliver(Box::new(1u8)).await.unwrap();
    wait_for(&mut events, "held").await;
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    actor
        .deliver(Box::new(Tracked(dropped.clone())))
        .await
        .unwrap();
    actor
        .deliver(Box::new(Tracked(dropped.clone())))
        .await
        .unwrap();
    assert!(
        actor
            .deliver(Box::new(Tracked(dropped.clone())))
            .await
            .is_err()
    );
    assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
    actor.stop().await.unwrap();
    assert_eq!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "success must follow whole ContextFut disposal, including mailbox"
    );
    system.shutdown().await.unwrap();
}
#[actix::test]
async fn system_drop_stops_dynamic_actor_with_surviving_reference() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let (actor, mut events) = spawn(&system).await;
    wait_for(&mut events, "init").await;
    drop(system);
    assert_eq!(events.recv().await.unwrap(), "hook");
    assert_eq!(events.recv().await.unwrap(), "actor_drop");
    assert!(!actor.is_alive().await);
}

#[actix::test]
async fn managed_explicit_ask_timeout_and_post_shutdown_admission() {
    let system = ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
        arbiter_count: 1,
        ask_timeout: Duration::from_millis(20),
        ..Default::default()
    })
    .await
    .unwrap();
    let (actor, _events) = spawn(&system).await;
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        actor.send_with_timeout(Box::new(1u8), Some(Duration::from_millis(20))),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(ActorError::TimeoutDetail(_))));
    system.clone().shutdown().await.unwrap();
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    assert!(
        system
            .spawn_root_typed(
                Probe {
                    events,
                    initialized: false,
                    init_behavior: Behavior::Ready,
                    hook_behavior: Behavior::Ready
                },
                EmptyConfig
            )
            .await
            .is_err()
    );
    assert!(actor.deliver(Box::new(0u8)).await.is_err());
}

type AffinityEvents = std::sync::Arc<std::sync::Mutex<Vec<(&'static str, std::thread::ThreadId)>>>;
struct Affinity(AffinityEvents);
impl Affinity {
    fn record(&self, event: &'static str) {
        self.0
            .lock()
            .unwrap()
            .push((event, std::thread::current().id()));
    }
}
impl Drop for Affinity {
    fn drop(&mut self) {
        self.record("drop");
    }
}
impl ActixEngineExt for Affinity {}
impl Actor for Affinity {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.record("init_create");
        Box::pin(async move {
            self.record("init_poll");
            tokio::task::yield_now().await;
            self.record("init_resume");
            Ok(())
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.record("hook_create");
        Box::pin(async move {
            self.record("hook_poll");
            tokio::task::yield_now().await;
            self.record("hook_resume");
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.record("dispatch_create");
        Box::pin(async move {
            self.record("dispatch_poll");
            tokio::task::yield_now().await;
            self.record("dispatch_resume");
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
#[actix::test]
async fn dynamic_lifecycle_stays_on_owning_arbiter() {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let events = AffinityEvents::default();
    let actor = system
        .spawn_root_typed(Affinity(events.clone()), EmptyConfig)
        .await
        .unwrap();
    actor.send(Box::new(())).await.unwrap();
    actor.stop().await.unwrap();
    let snapshot = events.lock().unwrap().clone();
    assert_eq!(
        snapshot.iter().map(|(event, _)| *event).collect::<Vec<_>>(),
        [
            "init_create",
            "init_poll",
            "init_resume",
            "dispatch_create",
            "dispatch_poll",
            "dispatch_resume",
            "hook_create",
            "hook_poll",
            "hook_resume",
            "drop"
        ]
    );
    let owner = snapshot[0].1;
    assert_ne!(owner, std::thread::current().id());
    assert!(snapshot.iter().all(|(_, thread)| *thread == owner));
    system.shutdown().await.unwrap();
}

#[test]
fn host_runtime_teardown_does_not_construct_hook_without_a_reactor() {
    const CHILD: &str = "PARROT_ACTIX_TEARDOWN_PROBE";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host_runtime_teardown_does_not_construct_hook_without_a_reactor",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated teardown regression failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let panics = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = panics.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |_| {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    {
        let runtime = actix::System::new();
        runtime.block_on(async {
            use actix::Actor as _;
            let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
            let address = ActixActor::new(Probe {
                events,
                initialized: false,
                init_behavior: Behavior::Ready,
                hook_behavior: Behavior::Ready,
            })
            .start();
            let actor = parrot::actix::ActixActorRef::new(address, "/raw".into());
            parrot_api::address::ActorRef::send(&actor, Box::new(0u8))
                .await
                .unwrap();
            // No graceful stop: teardown exercises ContextFut::drop after the
            // Actix runtime has left its entered Tokio context.
        });
    }
    std::panic::set_hook(previous);
    assert_eq!(
        panics.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "teardown must not hide timer-construction panic behind catch_unwind"
    );
}
