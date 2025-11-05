use futures::FutureExt;
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContext, ActorSpawnerExt},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Config;
impl parrot_api::actor::ActorConfig for Config {}
impl Drop for Config {
    fn drop(&mut self) {
        panic!("child configuration destructor");
    }
}
struct Child(Arc<AtomicUsize>);
impl Drop for Child {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Child {
    type Config = Config;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
}
struct Parent(Arc<AtomicUsize>);
impl Actor for Parent {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let mode = *msg.downcast::<u8>().unwrap();
            let result = std::panic::AssertUnwindSafe(async {
                match mode {
                    0 => {
                        ctx.spawner()
                            .spawn_typed(Child(self.0.clone()), Config)
                            .await
                    }
                    1 => {
                        ctx.spawner()
                            .spawn_supervised(
                                Child(self.0.clone()),
                                Config,
                                parrot_api::supervisor::SupervisorStrategyType::Default(
                                    parrot_api::supervisor::DefaultStrategy::StopOnFailure,
                                ),
                            )
                            .await
                    }
                    _ => {
                        ctx.spawner()
                            .spawn(Box::new(Child(self.0.clone())), Box::new(Config))
                            .await
                    }
                }
            })
            .catch_unwind()
            .await;
            Ok(Box::new(matches!(result, Ok(Err(_)))) as BoxedMessage)
        })
    }
}
async fn check(mode: u8) {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let drops = Arc::new(AtomicUsize::new(0));
    let parent = system
        .spawn_root_typed_thread(Parent(drops.clone()), EmptyConfig)
        .await
        .unwrap();
    let returned_error = *parent
        .send(Box::new(mode))
        .await
        .unwrap()
        .downcast::<bool>()
        .unwrap();
    let count = system.actor_count();
    system.shutdown_internal().await.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(count, 1, "failed child must not remain registered");
    assert!(
        returned_error,
        "child config panic escaped the creation boundary: mode={mode}"
    );
}
#[tokio::test]
async fn typed_child_configuration_failure_is_returned() {
    check(0).await;
}
#[tokio::test]
async fn supervised_child_configuration_failure_is_returned() {
    check(1).await;
}
#[tokio::test]
async fn malformed_child_payload_separately_retires_configuration() {
    check(2).await;
}

#[derive(Default)]
struct Events {
    config: AtomicUsize,
    init: AtomicUsize,
    actor: AtomicUsize,
    stop: AtomicUsize,
}
struct Gate {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}
impl Gate {
    fn wait(&mut self) {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        self.release
            .get_mut()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("test release gate lost");
    }
}
fn gate() -> (
    Gate,
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    (
        Gate {
            entered: Some(entered),
            release: std::sync::Mutex::new(held),
        },
        observed,
        release,
    )
}
struct ProbeConfig {
    events: Arc<Events>,
    gate: Option<Gate>,
    panic: bool,
}
impl parrot_api::actor::ActorConfig for ProbeConfig {}
impl Drop for ProbeConfig {
    fn drop(&mut self) {
        if let Some(gate) = &mut self.gate {
            gate.wait();
        }
        self.events.config.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "child config retirement failed");
    }
}
struct ProbeChild {
    events: Arc<Events>,
    gate: Option<Gate>,
    panic: bool,
}
impl Drop for ProbeChild {
    fn drop(&mut self) {
        if let Some(gate) = &mut self.gate {
            gate.wait();
        }
        self.events.actor.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "child actor retirement failed");
    }
}
impl Actor for ProbeChild {
    type Config = ProbeConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        assert_eq!(self.events.config.load(Ordering::SeqCst), 1);
        self.events.init.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.events.stop.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(Box::new(ctx.parent().unwrap().path()) as BoxedMessage) })
    }
}
struct Create {
    child: ProbeChild,
    config: ProbeConfig,
    cancel: Option<tokio::sync::oneshot::Receiver<()>>,
    malformed: bool,
    wrong_config: bool,
}
struct ProbeParent;
impl Actor for ProbeParent {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let Ok(request) = message.downcast::<Create>() else {
                return Ok(Box::new(42_u64) as BoxedMessage);
            };
            let Create {
                child,
                config,
                cancel,
                malformed,
                wrong_config,
            } = *request;
            let actor = if malformed {
                Box::new(child) as BoxedMessage
            } else {
                parrot::thread::system::TypedSpawnPayload::new(child).into_boxed()
            };
            let config = if wrong_config {
                Box::new((config,)) as BoxedMessage
            } else {
                Box::new(config) as BoxedMessage
            };
            let spawn = ctx.spawner().spawn(actor, config);
            let result = match cancel {
                None => spawn.await,
                Some(cancel) => tokio::select! {
                    biased;
                    _ = cancel => Err(parrot_api::errors::ActorError::Stopped),
                    result = spawn => result,
                },
            };
            Ok(Box::new(result.map_err(|error| error.to_string())) as BoxedMessage)
        })
    }
}
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(std::time::Duration::from_secs(3), future)
        .await
        .expect("runtime/retirement made no progress")
}
fn request(events: &Arc<Events>) -> Create {
    Create {
        child: ProbeChild {
            events: events.clone(),
            gate: None,
            panic: false,
        },
        config: ProbeConfig {
            events: events.clone(),
            gate: None,
            panic: false,
        },
        cancel: None,
        malformed: false,
        wrong_config: false,
    }
}
#[tokio::test]
async fn parent_stop_waits_for_pending_configuration_and_actor_retirement() {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let parent = system
        .spawn_root_typed_thread(ProbeParent, EmptyConfig)
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let (config_gate, config_entered, release_config) = gate();
    let (actor_gate, actor_entered, release_actor) = gate();
    let mut create = request(&events);
    create.config.gate = Some(config_gate);
    create.child.gate = Some(actor_gate);
    let caller = parent.clone_boxed();
    let spawn = tokio::spawn(async move { caller.send(Box::new(create)).await });
    bounded(config_entered).await.unwrap();
    assert_eq!(
        system.actor_count(),
        2,
        "pending child must already belong to system and parent"
    );
    let stop = parent.stop();
    tokio::pin!(stop);
    assert!(futures::poll!(&mut stop).is_pending());
    release_config.send(()).unwrap();
    bounded(actor_entered).await.unwrap();
    assert!(
        futures::poll!(&mut stop).is_pending(),
        "parent stopped before child's real Drop"
    );
    release_actor.send(()).unwrap();
    bounded(stop).await.unwrap();
    if let Ok(reply) = bounded(spawn).await.unwrap() {
        assert!(
            reply
                .downcast::<Result<parrot_api::types::BoxedActorRef, String>>()
                .unwrap()
                .is_err()
        );
    }
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn cancelled_child_waiter_leaves_parent_responsive_and_cleanup_owned() {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let parent = system
        .spawn_root_typed_thread(ProbeParent, EmptyConfig)
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let (config_gate, entered, release) = gate();
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let mut create = request(&events);
    create.config.gate = Some(config_gate);
    create.cancel = Some(cancelled);
    let caller = parent.clone_boxed();
    let spawn = tokio::spawn(async move { caller.send(Box::new(create)).await });
    bounded(entered).await.unwrap();
    cancel.send(()).unwrap();
    assert!(
        bounded(spawn)
            .await
            .unwrap()
            .unwrap()
            .downcast::<Result<parrot_api::types::BoxedActorRef, String>>()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        *bounded(parent.send(Box::new(())))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    assert_eq!(system.actor_count(), 2);
    release.send(()).unwrap();
    bounded(async {
        while system.actor_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
    system.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn successful_child_keeps_real_parent_address_message_and_lifecycle() {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let parent = system
        .spawn_root_typed_thread(ProbeParent, EmptyConfig)
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let child = *parent
        .send(Box::new(request(&events)))
        .await
        .unwrap()
        .downcast::<Result<parrot_api::types::BoxedActorRef, String>>()
        .unwrap();
    let child = child.unwrap();
    assert!(
        child
            .as_any()
            .is::<parrot::thread::ThreadActorRef<ProbeChild>>()
    );
    assert!(child.path().starts_with(&(parent.path() + "/")));
    assert_eq!(
        *child
            .send(Box::new(()))
            .await
            .unwrap()
            .downcast::<String>()
            .unwrap(),
        parent.path()
    );
    parent.stop().await.unwrap();
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 1);
    assert_eq!(events.stop.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}
#[test]
fn child_configuration_and_actor_panics_are_isolated_in_real_process() {
    const FLAG: &str = "PARROT_CHILD_INPUT_DOUBLE_PANIC";
    if std::env::var_os(FLAG).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "child_configuration_and_actor_panics_are_isolated_in_real_process",
                "--nocapture",
            ])
            .env(FLAG, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for malformed in [false, true] {
                let system = parrot::thread::ThreadActorSystem::shared(Default::default());
                let parent = system
                    .spawn_root_typed_thread(ProbeParent, EmptyConfig)
                    .await
                    .unwrap();
                let events = Arc::new(Events::default());
                let mut create = request(&events);
                create.config.panic = true;
                create.child.panic = true;
                create.malformed = malformed;
                assert!(
                    parent
                        .send(Box::new(create))
                        .await
                        .unwrap()
                        .downcast::<Result<parrot_api::types::BoxedActorRef, String>>()
                        .unwrap()
                        .is_err()
                );
                assert_eq!(events.actor.load(Ordering::SeqCst), 1);
                assert_eq!(events.config.load(Ordering::SeqCst), 1);
                assert_eq!(events.init.load(Ordering::SeqCst), 0);
                assert_eq!(system.actor_count(), 1);
                system.shutdown_internal().await.unwrap();
            }
        });
}
#[tokio::test]
async fn unbound_context_rejection_retires_inputs_without_unwinding() {
    let events = Arc::new(Events::default());
    let mut ctx = parrot::thread::ThreadContext::<ProbeParent>::new_for_test("/unbound");
    let mut create = request(&events);
    create.child.panic = true;
    create.config.panic = true;
    let result =
        std::panic::AssertUnwindSafe(ctx.spawner().spawn_typed(create.child, create.config))
            .catch_unwind()
            .await;
    assert!(matches!(result, Ok(Err(_))));
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn child_deadline_returns_without_releasing_pending_input_ownership() {
    let system =
        parrot::thread::ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
            shutdown_timeout: std::time::Duration::from_millis(40),
            ..Default::default()
        });
    let parent = system
        .spawn_root_typed_thread(ProbeParent, EmptyConfig)
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let (config_gate, entered, release) = gate();
    let mut create = request(&events);
    create.config.gate = Some(config_gate);
    let caller = parent.clone_boxed();
    let spawn = tokio::spawn(async move { caller.send(Box::new(create)).await });
    bounded(entered).await.unwrap();
    let result = *bounded(spawn)
        .await
        .unwrap()
        .unwrap()
        .downcast::<Result<parrot_api::types::BoxedActorRef, String>>()
        .unwrap();
    assert!(result.unwrap_err().contains("deadline"));
    assert_eq!(system.actor_count(), 2);
    assert_eq!(events.actor.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    bounded(async {
        while system.actor_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    system.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn malformed_child_is_owned_before_configuration_retirement() {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let parent = system
        .spawn_root_typed_thread(ProbeParent, EmptyConfig)
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let (config_gate, entered, release) = gate();
    let mut create = request(&events);
    create.config.gate = Some(config_gate);
    create.malformed = true;
    let caller = parent.clone_boxed();
    let spawn = tokio::spawn(async move { caller.send(Box::new(create)).await });
    bounded(entered).await.unwrap();
    assert_eq!(system.actor_count(), 2);
    let stop = parent.stop();
    tokio::pin!(stop);
    assert!(futures::poll!(&mut stop).is_pending());
    release.send(()).unwrap();
    bounded(stop).await.unwrap();
    let _ = bounded(spawn).await.unwrap();
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn child_disposal_failure_reaches_parent_that_is_already_stopping() {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let parent = system
        .spawn_root_typed_thread(ProbeParent, EmptyConfig)
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let (config_gate, entered, release) = gate();
    let mut create = request(&events);
    create.config.gate = Some(config_gate);
    create.config.panic = true;
    let caller = parent.clone_boxed();
    let spawn = tokio::spawn(async move { caller.send(Box::new(create)).await });
    bounded(entered).await.unwrap();
    // Poll stop until it has synchronously requested the parent's retirement.
    let stop = parent.stop();
    tokio::pin!(stop);
    assert!(futures::poll!(&mut stop).is_pending());
    release.send(()).unwrap();
    let error = bounded(stop).await.unwrap_err();
    assert!(
        error.to_string().contains("child config retirement failed"),
        "{error}"
    );
    let _ = bounded(spawn).await.unwrap();
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn cancelling_unbound_rejection_keeps_both_inputs_owned() {
    let events = Arc::new(Events::default());
    let (config_gate, entered, release) = gate();
    let mut create = request(&events);
    create.config.gate = Some(config_gate);
    let ctx = parrot::thread::ThreadContext::<ProbeParent>::new_for_test("/unbound");
    let caller = tokio::spawn(async move {
        use parrot_api::context::ActorSpawner;
        ctx.spawn(
            parrot::thread::system::TypedSpawnPayload::new(create.child).into_boxed(),
            Box::new(create.config),
        )
        .await
    });
    bounded(entered).await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    bounded(async {
        while events.actor.load(Ordering::SeqCst) != 1 || events.config.load(Ordering::SeqCst) != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mismatched_child_config_retires_both_inputs_without_initialization() {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let parent = system
        .spawn_root_typed_thread(ProbeParent, EmptyConfig)
        .await
        .unwrap();
    let events = Arc::new(Events::default());
    let mut create = request(&events);
    create.wrong_config = true;
    let reply = *parent
        .send(Box::new(create))
        .await
        .unwrap()
        .downcast::<Result<parrot_api::types::BoxedActorRef, String>>()
        .unwrap();
    assert!(reply.unwrap_err().contains("configuration type mismatch"));
    assert_eq!(events.config.load(Ordering::SeqCst), 1);
    assert_eq!(events.actor.load(Ordering::SeqCst), 1);
    assert_eq!(events.init.load(Ordering::SeqCst), 0);
    assert_eq!(system.actor_count(), 1);
    system.shutdown_internal().await.unwrap();
}
