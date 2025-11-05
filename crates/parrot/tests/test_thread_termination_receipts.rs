//! Termination requires disposal; a caller deadline cannot release a live path.
use parrot::thread::{
    config::ThreadActorSystemConfig,
    system::{Terminated, ThreadActorSystem},
};
use parrot_api::{
    DeathReason, Message,
    errors::ActorError,
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc, oneshot};

pub enum Command {
    Ping,
    Block(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>),
}
impl Message for Command {
    type Result = ();
}
struct Probe {
    disposed: Arc<AtomicBool>,
    init_gate: Option<Arc<Notify>>,
    init_error: Option<ActorError>,
    hook_error: Option<ActorError>,
    drop_gate: Option<(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}
impl Probe {
    fn new() -> Self {
        Self {
            disposed: Arc::new(AtomicBool::new(false)),
            init_gate: None,
            init_error: None,
            hook_error: None,
            drop_gate: None,
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some((entered, gate)) = self.drop_gate.take() {
            let _ = entered.send(());
            let _ = gate.recv_timeout(Duration::from_secs(3));
        }
        self.disposed.store(true, Ordering::SeqCst);
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Command;
    type Reply = ();
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            if let Some(gate) = &self.init_gate {
                gate.notified().await;
            }
            self.init_error.take().map_or(Ok(()), Err)
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move { self.hook_error.take().map_or(Ok(()), Err) })
    }
    fn dispatch(&mut self, message: Command) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            if let Command::Block(entered, gate) = message {
                let _ = entered.send(());
                let _ = gate.recv_timeout(Duration::from_secs(3));
            }
            Ok(())
        })
    }
}
impl ParrotMsgVariant<Probe> for Command {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(_: ()) -> ActorResult<()> {
        Ok(())
    }
}
struct Watcher(mpsc::UnboundedSender<Terminated>);
impl ParrotTypedDispatch for Watcher {
    type Msg = Terminated;
    type Reply = ();
    fn dispatch(&mut self, message: Terminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.0.send(message).unwrap();
            Ok(())
        })
    }
}
impl ParrotMsgVariant<Watcher> for Terminated {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(_: ()) -> ActorResult<()> {
        Ok(())
    }
}
fn system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 2,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_millis(60),
        ..Default::default()
    })
}
async fn watcher(system: &Arc<ThreadActorSystem>) -> mpsc::UnboundedReceiver<Terminated> {
    let (send, receive) = mpsc::unbounded_channel();
    system
        .spawn_typed::<Watcher, Terminated>(Watcher(send), "/watcher")
        .await
        .unwrap();
    system
        .watch("/watcher".into(), "/target".into())
        .await
        .unwrap();
    receive
}
async fn retired(system: &ThreadActorSystem) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while system.get_actor_ref("/target").is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual completion must retire the registered generation");
}
async fn blocked_contract(block_drop: bool) {
    let system = system();
    let mut actor = Probe::new();
    let disposed = actor.disposed.clone();
    let (entered, ready) = oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let command = if block_drop {
        actor.drop_gate = Some((entered, gate));
        Command::Ping
    } else {
        Command::Block(entered, gate)
    };
    let target = system
        .spawn_typed::<Probe, Command>(actor, "/target")
        .await
        .unwrap();
    let mut events = watcher(&system).await;
    if block_drop {
        target.ask(command).await.unwrap();
    } else {
        target.tell(command).await.unwrap();
    }
    let mut ready = Some(ready);
    if !block_drop {
        tokio::time::timeout(Duration::from_secs(1), ready.take().unwrap())
            .await
            .unwrap()
            .unwrap();
    }
    let stopping = target.clone();
    let stop = tokio::spawn(async move { stopping.stop().await });
    if block_drop {
        tokio::time::timeout(Duration::from_secs(1), ready.take().unwrap())
            .await
            .unwrap()
            .unwrap();
    }
    let result = stop.await.unwrap();
    // Keep the gate closed while collecting independent evidence. Always
    // release it before asserting, so a failing old version leaves no worker.
    let registered = system.get_actor_ref("/target").is_some();
    let still_owned = !disposed.load(Ordering::SeqCst);
    let early = tokio::time::timeout(Duration::from_millis(20), events.recv()).await;
    let replacement = system
        .spawn_typed::<Probe, Command>(Probe::new(), "/target")
        .await;
    let duplicate_rejected = replacement.is_err();
    release.send(()).unwrap();
    if let Ok(replacement) = replacement {
        let _ = replacement.stop().await;
    }
    retired(&system).await;
    let notice = if early.is_err() {
        tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let final_disposed = disposed.load(Ordering::SeqCst);
    let _ = system.shutdown_internal().await;
    assert!(matches!(result, Err(ActorError::TimeoutDetail(_))));
    assert!(
        registered,
        "a stop timeout must retain the live registration"
    );
    assert!(
        still_owned,
        "gate must prove user code or disposal was still live"
    );
    assert!(early.is_err(), "a timeout is not a termination receipt");
    assert!(
        duplicate_rejected,
        "a blocked generation must reserve its path"
    );
    assert!(final_disposed);
    let notice = notice.expect("late completion must still notify registered watchers");
    let expected_kill = !block_drop;
    assert_eq!(
        matches!(notice.reason, DeathReason::Killed),
        expected_kill,
        "blocking disposal after successful cleanup is Normal; an expired hook is Killed: {notice:?}"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_timeout_preserves_blocked_handler_path_until_disposal() {
    blocked_contract(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_timeout_preserves_blocked_destructor_path_until_disposal() {
    blocked_contract(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialization_and_hook_errors_are_failures_including_application_timeouts() {
    let system = system();
    for during_init in [true, false] {
        for timeout in [false, true] {
            let mut actor = Probe::new();
            let gate = Arc::new(Notify::new());
            actor.init_gate = Some(gate.clone());
            let error = if timeout {
                ActorError::Timeout
            } else {
                ActorError::MessageHandlingError("application lifecycle failure".into())
            };
            if during_init {
                actor.init_error = Some(error);
            } else {
                actor.hook_error = Some(error);
            }
            let target = system
                .spawn_typed::<Probe, Command>(actor, "/target")
                .await
                .unwrap();
            let (send, mut events) = mpsc::unbounded_channel();
            let watch = system
                .spawn_typed::<Watcher, Terminated>(Watcher(send), "/watcher")
                .await
                .unwrap();
            system
                .watch("/watcher".into(), "/target".into())
                .await
                .unwrap();
            gate.notify_one();
            if !during_init {
                target.ask(Command::Ping).await.unwrap();
                assert!(target.stop().await.is_err());
            }
            let notice = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            retired(&system).await;
            watch.stop().await.unwrap();
            assert!(
                matches!(notice.reason, DeathReason::Failed(_)),
                "{notice:?}"
            );
        }
    }
    system.shutdown_internal().await.unwrap();
}

// A real actor on another engine consumes the public generation-bearing event.
struct ActixWatcher(mpsc::UnboundedSender<parrot_api::context::ActorTerminated>);
impl parrot_api::actor::ActixEngineExt for ActixWatcher {}
impl parrot_api::actor::Actor for ActixWatcher {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: parrot_api::types::BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
        use parrot_api::context::ActorContext;
        Box::pin(async move {
            if msg.is::<parrot_api::context::ActorTerminated>() {
                self.0
                    .send(
                        *msg.downcast::<parrot_api::context::ActorTerminated>()
                            .unwrap(),
                    )
                    .unwrap();
            } else {
                ctx.watch(*msg.downcast::<parrot_api::types::BoxedActorRef>().unwrap())
                    .await?;
            }
            Ok(Box::new(()) as parrot_api::types::BoxedMessage)
        })
    }
}
#[actix::test]
async fn actix_context_watches_thread_targets_by_generation_and_cleans_up_on_stop() {
    let first = system();
    let second = system();
    let actix = parrot::actix::ActixActorSystem::with_arbiter_count(1)
        .await
        .unwrap();
    let (send, mut events) = mpsc::unbounded_channel();
    let observer = actix
        .spawn_root_typed(ActixWatcher(send), parrot_api::actor::EmptyConfig)
        .await
        .unwrap();
    let a = first
        .spawn_typed::<Probe, Command>(Probe::new(), "/target")
        .await
        .unwrap()
        .into_dyn();
    let b = second
        .spawn_typed::<Probe, Command>(Probe::new(), "/target")
        .await
        .unwrap()
        .into_dyn();
    for target in [&a, &b] {
        observer.send(Box::new(target.clone_boxed())).await.unwrap();
    }
    assert_ne!(a.death_watch().unwrap().id(), b.death_watch().unwrap().id());
    a.stop().await.unwrap();
    let old = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.generation, a.death_watch().unwrap().id());
    assert!(matches!(old.reason, DeathReason::Normal));
    let replacement = first
        .spawn_typed::<Probe, Command>(Probe::new(), "/target")
        .await
        .unwrap()
        .into_dyn();
    observer
        .send(Box::new(replacement.clone_boxed()))
        .await
        .unwrap();
    a.unwatch(observer.clone_boxed()).await.unwrap();
    replacement.stop().await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.generation, replacement.death_watch().unwrap().id());
    assert_ne!(old.generation, next.generation);
    assert_eq!(b.death_watch().unwrap().subscriptions().0, 1);
    observer.stop().await.unwrap();
    assert_eq!(b.death_watch().unwrap().subscriptions(), (0, 0));
    b.stop().await.unwrap();
    first.shutdown_internal().await.unwrap();
    second.shutdown_internal().await.unwrap();
    actix.shutdown().await.unwrap();
}
