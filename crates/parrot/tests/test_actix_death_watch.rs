//! Actual actor/context contracts, including mailbox pressure and disposal.
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext, system::ActixSystemSettings};
use parrot_api::{
    DeathReason,
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorTerminated},
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc, oneshot};

enum Command {
    Watch(BoxedActorRef),
    Unwatch(BoxedActorRef),
    SelfRef,
    Ping,
    Panic,
    Hold(oneshot::Sender<()>, Arc<Notify>),
    Block(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>),
}
struct Probe {
    events: mpsc::UnboundedSender<ActorTerminated>,
    dropped: Arc<AtomicBool>,
    hook_error: Option<ActorError>,
    init: Option<Arc<Notify>>,
    init_error: Option<ActorError>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
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
        Box::pin(async move {
            if let Some(gate) = &self.init {
                gate.notified().await;
            }
            if let Some(error) = self.init_error.take() {
                return Err(error);
            }
            Ok(())
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if let Some(error) = self.hook_error.take() {
                Err(error)
            } else {
                Ok(())
            }
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<ActorTerminated>() {
                let _ = self
                    .events
                    .send(*msg.downcast::<ActorTerminated>().unwrap());
            } else {
                match *msg.downcast::<Command>().unwrap() {
                    Command::Watch(target) => ctx.watch(target).await?,
                    Command::Unwatch(target) => ctx.unwatch(target).await?,
                    Command::SelfRef => return Ok(Box::new(ctx.get_self_ref()) as BoxedMessage),
                    Command::Ping => {}
                    Command::Panic => panic!("watched actor panic"),
                    Command::Hold(entered, gate) => {
                        let _ = entered.send(());
                        gate.notified().await;
                    }
                    Command::Block(entered, gate) => {
                        let _ = entered.send(());
                        gate.recv_timeout(Duration::from_secs(3)).unwrap();
                    }
                }
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
fn probe() -> (Probe, mpsc::UnboundedReceiver<ActorTerminated>) {
    let (events, rx) = mpsc::unbounded_channel();
    (
        Probe {
            events,
            dropped: Arc::new(AtomicBool::new(false)),
            hook_error: None,
            init: None,
            init_error: None,
        },
        rx,
    )
}
async fn system(timeout: Duration) -> ActixActorSystem {
    ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 3,
        mailbox_capacity: 1,
        shutdown_timeout: timeout,
        ..Default::default()
    })
    .await
    .unwrap()
}
async fn spawn(
    system: &ActixActorSystem,
) -> (BoxedActorRef, mpsc::UnboundedReceiver<ActorTerminated>) {
    let (probe, events) = probe();
    let reference = system.spawn_root_typed(probe, EmptyConfig).await.unwrap();
    reference.send(Box::new(Command::Ping)).await.unwrap();
    (reference, events)
}
async fn event(events: &mut mpsc::UnboundedReceiver<ActorTerminated>) -> ActorTerminated {
    tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn settled(reference: &dyn parrot_api::ActorRef) {
    let watch = reference.death_watch().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while watch.subscriptions().1 != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[actix::test]
async fn context_and_external_clones_share_identity_and_unwatch() {
    let system = system(Duration::from_secs(1)).await;
    let (observer, mut events) = spawn(&system).await;
    let (target, _) = spawn(&system).await;
    let self_ref = *observer
        .send(Box::new(Command::SelfRef))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap();
    assert_eq!(
        observer.death_watch().unwrap().id(),
        self_ref.death_watch().unwrap().id()
    );
    observer
        .send(Box::new(Command::Watch(target.clone_boxed())))
        .await
        .unwrap();
    target.watch(observer.clone_boxed()).await.unwrap();
    assert_eq!(target.death_watch().unwrap().subscriptions(), (1, 0));
    // Remove via an external ref, although registration used the actor context.
    target.unwatch(observer.clone_boxed()).await.unwrap();
    assert_eq!(target.death_watch().unwrap().subscriptions(), (0, 0));
    observer
        .send(Box::new(Command::Watch(target.clone_boxed())))
        .await
        .unwrap();
    observer
        .send(Box::new(Command::Unwatch(target.clone_boxed())))
        .await
        .unwrap();
    assert_eq!(observer.death_watch().unwrap().subscriptions(), (0, 0));
    target.stop().await.unwrap();
    assert!(events.try_recv().is_err());
    assert!(matches!(
        target.watch(observer.clone_boxed()).await,
        Err(ActorError::Stopped)
    ));
    system.shutdown().await.unwrap();
}

#[actix::test]
async fn full_mailbox_waits_and_slow_recipient_does_not_block_other_watcher() {
    let system = system(Duration::from_secs(1)).await;
    let (slow, mut slow_events) = spawn(&system).await;
    let (healthy, mut events) = spawn(&system).await;
    let (target, _) = spawn(&system).await;
    target.watch(slow.clone_boxed()).await.unwrap();
    target.watch(healthy.clone_boxed()).await.unwrap();
    let (entered, ready) = oneshot::channel();
    let gate = Arc::new(Notify::new());
    slow.deliver(Box::new(Command::Hold(entered, gate.clone())))
        .await
        .unwrap();
    ready.await.unwrap();
    slow.deliver(Box::new(Command::Ping)).await.unwrap(); // mailbox capacity is now held
    target.stop().await.unwrap();
    let notice = event(&mut events).await;
    assert_eq!(notice.generation, target.death_watch().unwrap().id());
    assert_eq!(notice.reason, DeathReason::Normal);
    assert!(slow_events.try_recv().is_err());
    gate.notify_one();
    assert_eq!(event(&mut slow_events).await.generation, notice.generation);
    settled(&*slow).await;
    settled(&*healthy).await;
    assert_eq!(target.death_watch().unwrap().delivery_failures(), 0);
    assert!(events.try_recv().is_err());
    system.shutdown().await.unwrap();
}

#[actix::test]
async fn stop_timeout_is_not_a_death_event_and_disposal_precedes_notice() {
    let system = system(Duration::from_millis(80)).await;
    let (observer, mut events) = spawn(&system).await;
    let (probe, _) = probe();
    let dropped = probe.dropped.clone();
    let target = system.spawn_root_typed(probe, EmptyConfig).await.unwrap();
    target.watch(observer.clone_boxed()).await.unwrap();
    let (entered, ready) = oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    target
        .deliver(Box::new(Command::Block(entered, held)))
        .await
        .unwrap();
    ready.await.unwrap();
    assert!(matches!(
        target.stop().await,
        Err(ActorError::TimeoutDetail(_))
    ));
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(events.try_recv().is_err());
    // Keep the exact generation registered until disposal, even after timeout.
    assert!(system.get_actor(&target.path()).await.is_some());
    release.send(()).unwrap();
    let notice = event(&mut events).await;
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(notice.generation, target.death_watch().unwrap().id());
    assert_eq!(notice.reason, DeathReason::Killed);
    assert!(system.get_actor(&target.path()).await.is_none());
    system.shutdown().await.unwrap();
}

#[actix::test]
async fn panic_hook_and_initialization_failures_keep_distinct_death_reasons() {
    let system = system(Duration::from_secs(1)).await;
    let (observer, mut events) = spawn(&system).await;
    let (panic_target, _) = spawn(&system).await;
    panic_target.watch(observer.clone_boxed()).await.unwrap();
    let _ = panic_target.send(Box::new(Command::Panic)).await;
    assert!(
        matches!(event(&mut events).await.reason, DeathReason::Panic(s) if s.contains("watched actor panic"))
    );
    for init_failure in [false, true] {
        let (mut probe, _) = probe();
        let gate = Arc::new(Notify::new());
        probe.init = Some(gate.clone());
        if init_failure {
            probe.init_error = Some(ActorError::InitializationError(
                "rejected initialization".into(),
            ));
        } else {
            probe.hook_error = Some(ActorError::MessageHandlingError("hook failure".into()));
        }
        let target = system.spawn_root_typed(probe, EmptyConfig).await.unwrap();
        target.watch(observer.clone_boxed()).await.unwrap();
        gate.notify_one();
        if !init_failure {
            target.send(Box::new(Command::Ping)).await.unwrap();
            assert!(target.stop().await.is_err());
        }
        assert!(matches!(
            event(&mut events).await.reason,
            DeathReason::Failed(_)
        ));
    }
    system.shutdown().await.unwrap();
}

#[actix::test]
async fn watcher_stop_and_system_drop_release_mutual_subscriptions() {
    let system = system(Duration::from_secs(1)).await;
    let (a, _) = spawn(&system).await;
    let (b, _) = spawn(&system).await;
    a.watch(a.clone_boxed()).await.unwrap(); // explicitly documented self-watch no-op
    a.watch(b.clone_boxed()).await.unwrap();
    b.watch(a.clone_boxed()).await.unwrap();
    a.stop().await.unwrap();
    assert_eq!(b.death_watch().unwrap().subscriptions().0, 0);
    system.shutdown().await.unwrap();
    assert_eq!(a.death_watch().unwrap().subscriptions(), (0, 0));
    assert_eq!(b.death_watch().unwrap().subscriptions(), (0, 0));
}

#[actix::test]
async fn stalled_notification_is_bounded_counted_and_cancelled_by_unwatch() {
    let system = system(Duration::from_millis(100)).await;
    let (observer, mut events) = spawn(&system).await;
    let (entered, ready) = oneshot::channel();
    let gate = Arc::new(Notify::new());
    observer
        .deliver(Box::new(Command::Hold(entered, gate.clone())))
        .await
        .unwrap();
    ready.await.unwrap();
    observer.deliver(Box::new(Command::Ping)).await.unwrap();
    let (expired, _) = spawn(&system).await;
    expired.watch(observer.clone_boxed()).await.unwrap();
    expired.stop().await.unwrap();
    settled(&*observer).await;
    assert_eq!(expired.death_watch().unwrap().delivery_failures(), 1);
    let (cancelled, _) = spawn(&system).await;
    cancelled.watch(observer.clone_boxed()).await.unwrap();
    cancelled.stop().await.unwrap();
    cancelled.unwatch(observer.clone_boxed()).await.unwrap();
    settled(&*observer).await;
    gate.notify_one();
    observer.send(Box::new(Command::Ping)).await.unwrap();
    assert!(events.try_recv().is_err());
    assert_eq!(cancelled.death_watch().unwrap().delivery_failures(), 0);
    system.shutdown().await.unwrap();
}

#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(Read))]
struct Typed;
pub struct Read;
impl parrot_api::Message for Read {
    type Result = ();
}
impl parrot_api::TypedReceive<Read> for Typed {
    fn receive_typed<'a>(&'a mut self, _: Read) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
#[actix::test]
async fn typed_targets_same_path_across_systems_and_reuse_have_distinct_generations() {
    let first = Arc::new(system(Duration::from_secs(1)).await);
    let second = Arc::new(system(Duration::from_secs(1)).await);
    let (observer, mut events) = spawn(&first).await;
    let a = first
        .spawn_typed::<Typed, Read>(Typed, "/same")
        .await
        .unwrap()
        .into_dyn();
    let b = second
        .spawn_typed::<Typed, Read>(Typed, "/same")
        .await
        .unwrap()
        .into_dyn();
    a.watch(observer.clone_boxed()).await.unwrap();
    b.watch(observer.clone_boxed()).await.unwrap();
    a.stop().await.unwrap();
    let old_generation = event(&mut events).await.generation;
    let replacement = first
        .spawn_typed::<Typed, Read>(Typed, "/same")
        .await
        .unwrap()
        .into_dyn();
    replacement.watch(observer.clone_boxed()).await.unwrap();
    a.unwatch(observer.clone_boxed()).await.unwrap(); // must not remove replacement
    b.stop().await.unwrap();
    assert_eq!(
        event(&mut events).await.generation,
        b.death_watch().unwrap().id()
    );
    replacement.stop().await.unwrap();
    let next = event(&mut events).await;
    assert_eq!(next.generation, replacement.death_watch().unwrap().id());
    assert_ne!(old_generation, next.generation);
    first.as_ref().clone().shutdown().await.unwrap();
    second.as_ref().clone().shutdown().await.unwrap();
}

// Application timeouts are ordinary hook failures. Only an engine-enforced
// stop deadline can classify termination as Killed.
#[actix::test]
async fn application_timeouts_in_dynamic_lifecycle_are_failures_not_kills() {
    let system = system(Duration::from_secs(1)).await;
    let (observer, mut events) = spawn(&system).await;
    for during_init in [true, false] {
        for detailed in [false, true] {
            let (mut actor, _) = probe();
            let gate = Arc::new(Notify::new());
            actor.init = Some(gate.clone());
            let error = if detailed {
                ActorError::TimeoutDetail("application dependency deadline".into())
            } else {
                ActorError::Timeout
            };
            if during_init {
                actor.init_error = Some(error);
            } else {
                actor.hook_error = Some(error);
            }
            let target = system.spawn_root_typed(actor, EmptyConfig).await.unwrap();
            target.watch(observer.clone_boxed()).await.unwrap();
            gate.notify_one();
            if !during_init {
                target.send(Box::new(Command::Ping)).await.unwrap();
                assert!(target.stop().await.is_err());
            }
            let notice = event(&mut events).await;
            assert!(
                matches!(notice.reason, DeathReason::Failed(_)),
                "{notice:?}"
            );
        }
    }
    system.shutdown().await.unwrap();
}

struct TypedFailure {
    gate: Arc<Notify>,
    init_error: Option<ActorError>,
    hook_error: Option<ActorError>,
    stall_hook: bool,
}
impl parrot_api::typed::ParrotTypedDispatch for TypedFailure {
    type Msg = Read;
    type Reply = ();
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.gate.notified().await;
            self.init_error.take().map_or(Ok(()), Err)
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            if self.stall_hook {
                std::future::pending::<()>().await;
            }
            self.hook_error.take().map_or(Ok(()), Err)
        })
    }
    fn dispatch(&mut self, _: Read) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl parrot_api::typed::ParrotMsgVariant<TypedFailure> for Read {
    fn inject(msg: Self) -> Read {
        msg
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
#[actix::test]
async fn application_timeouts_in_typed_lifecycle_are_failures_and_real_deadlines_kill() {
    let system = Arc::new(system(Duration::from_millis(100)).await);
    let (observer, mut events) = spawn(&system).await;
    for (case, (during_init, detailed, stall_hook)) in [
        (true, false, false),
        (true, true, false),
        (false, false, false),
        (false, true, false),
        (false, false, true),
    ]
    .into_iter()
    .enumerate()
    {
        let gate = Arc::new(Notify::new());
        let error = if detailed {
            ActorError::TimeoutDetail("application dependency deadline".into())
        } else {
            ActorError::Timeout
        };
        let (init_error, hook_error) = if during_init {
            (Some(error), None)
        } else {
            (None, Some(error))
        };
        let target = system
            .spawn_typed::<TypedFailure, Read>(
                TypedFailure {
                    gate: gate.clone(),
                    init_error,
                    hook_error,
                    stall_hook,
                },
                &format!("/timeout-classification-{case}"),
            )
            .await
            .unwrap();
        let dynamic = target.clone().into_dyn();
        dynamic.watch(observer.clone_boxed()).await.unwrap();
        gate.notify_one();
        if !during_init {
            target.ask(Read).await.unwrap();
            assert!(dynamic.stop().await.is_err());
        }
        let notice = event(&mut events).await;
        if stall_hook {
            assert!(matches!(notice.reason, DeathReason::Killed), "{notice:?}");
        } else {
            assert!(
                matches!(notice.reason, DeathReason::Failed(_)),
                "{notice:?}"
            );
        }
    }
    system.as_ref().clone().shutdown().await.unwrap();
}
