//! An idle receive deadline must signal the actor and must not change ask timeouts.
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContext, LifecycleEvent},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::time::Duration;
use tokio::sync::mpsc;

enum Command {
    Enable,
    Configure(Option<Duration>),
    Read,
    Hold(
        tokio::sync::oneshot::Sender<()>,
        std::sync::Arc<tokio::sync::Notify>,
    ),
    Crash,
    Ask(BoxedActorRef),
}
struct ThreadProbe(mpsc::UnboundedSender<()>);
struct ActixProbe(mpsc::UnboundedSender<()>);
impl ActixEngineExt for ActixProbe {}
macro_rules! probe {
    ($actor:ident, $context:ty) => {
        impl Actor for $actor {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    if msg
                        .downcast_ref::<LifecycleEvent>()
                        .is_some_and(|event| matches!(event, LifecycleEvent::ReceiveTimeout))
                    {
                        let _ = self.0.send(());
                    } else {
                        match *msg.downcast::<Command>().unwrap() {
                            Command::Enable => {
                                ctx.set_receive_timeout(Some(Duration::from_millis(20)))
                            }
                            Command::Configure(timeout) => {
                                ctx.set_receive_timeout(timeout);
                                return Ok(Box::new(ctx.receive_timeout()) as BoxedMessage);
                            }
                            Command::Read => {
                                return Ok(Box::new(ctx.receive_timeout()) as BoxedMessage);
                            }
                            Command::Hold(entered, gate) => {
                                let _ = entered.send(());
                                gate.notified().await;
                            }
                            Command::Crash => panic!("idle actor restart"),
                            Command::Ask(target) => {
                                ctx.set_receive_timeout(Some(Duration::from_millis(5)));
                                return ctx.ask(target, Box::new(())).await;
                            }
                        }
                    }
                    Ok(Box::new(()) as BoxedMessage)
                })
            }
        }
    };
}

fn modes() -> [parrot::thread::SchedulingMode; 3] {
    [
        parrot::thread::SchedulingMode::SharedPool {
            max_messages_per_run: 8,
        },
        parrot::thread::SchedulingMode::DedicatedThread,
        parrot::thread::SchedulingMode::Sharded {
            affinity_key: "idle".into(),
        },
    ]
}
async fn configure(actor: &dyn ActorRef, duration: Option<Duration>) -> Option<Duration> {
    *actor
        .send(Box::new(Command::Configure(duration)))
        .await
        .unwrap()
        .downcast::<Option<Duration>>()
        .unwrap()
}
async fn event(events: &mut mpsc::UnboundedReceiver<()>) {
    tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
}
async fn activity_contract(actor: BoxedActorRef, mut events: mpsc::UnboundedReceiver<()>) {
    let interval = Duration::from_millis(40);
    assert_eq!(configure(&*actor, Some(interval)).await, Some(interval));
    let (entered, ready) = tokio::sync::oneshot::channel();
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    actor
        .deliver(Box::new(Command::Hold(entered, gate.clone())))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), ready)
        .await
        .unwrap()
        .unwrap();
    while events.try_recv().is_ok() {}
    assert!(
        tokio::time::timeout(interval * 2, events.recv())
            .await
            .is_err(),
        "a busy handler must not accumulate idle events"
    );
    let released = std::time::Instant::now();
    gate.notify_one();
    event(&mut events).await;
    assert!(
        released.elapsed() >= interval,
        "count idle time from dispatch completion"
    );
    event(&mut events).await; // Repeated idle periods remain observable.
    assert_eq!(configure(&*actor, None).await, None);
    while events.try_recv().is_ok() {}
    assert!(
        tokio::time::timeout(interval * 2, events.recv())
            .await
            .is_err()
    );
    assert_eq!(configure(&*actor, Some(Duration::ZERO)).await, None);
    assert_eq!(configure(&*actor, Some(Duration::MAX)).await, None);
    assert_eq!(configure(&*actor, Some(interval)).await, Some(interval));
    actor.stop().await.unwrap();
    while events.try_recv().is_ok() {}
    assert!(
        tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .is_none(),
        "stop must join the actor and its clock"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_modes_reset_disable_and_stop_the_owned_idle_clock() {
    for mode in modes() {
        let sys =
            parrot::thread::ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
                default_scheduling_mode: mode,
                shared_pool_size: 2,
                max_context_tasks: 0,
                ..Default::default()
            });
        let (send, events) = mpsc::unbounded_channel();
        let actor = sys
            .spawn_at(ThreadProbe(send), "/idle", None, Default::default())
            .await
            .unwrap();
        activity_contract(actor.clone_boxed(), events).await;
        sys.shutdown_internal().await.unwrap();
    }
}
#[actix::test]
async fn actix_resets_disables_and_stops_the_owned_idle_clock() {
    let sys = parrot::actix::ActixActorSystem::with_settings(
        parrot::actix::system::ActixSystemSettings {
            max_context_tasks: 0,
            arbiter_count: 2,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (send, events) = mpsc::unbounded_channel();
    let actor = sys
        .spawn_root_typed(ActixProbe(send), EmptyConfig)
        .await
        .unwrap();
    activity_contract(actor.clone_boxed(), events).await;
    sys.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervised_replacement_does_not_inherit_old_idle_configuration_or_events() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for mode in modes() {
        let sys =
            parrot::thread::ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
                default_scheduling_mode: mode.clone(),
                shared_pool_size: 2,
                ..Default::default()
            });
        let (send, mut events) = mpsc::unbounded_channel();
        let builds = Arc::new(AtomicUsize::new(0));
        let counter = builds.clone();
        let actor = sys
            .spawn_supervised_with_config(
                move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    ThreadProbe(send.clone())
                },
                "/idle",
                parrot::thread::ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Restart {
                        max_retries: 1,
                        within: Duration::from_secs(5),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        configure(&actor, Some(Duration::from_millis(20))).await;
        event(&mut events).await;
        assert!(actor.send(Box::new(Command::Crash)).await.is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if builds.load(Ordering::SeqCst) == 2
                    && let Ok(setting) = actor.send(Box::new(Command::Read)).await
                {
                    assert_eq!(*setting.downcast::<Option<Duration>>().unwrap(), None);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        while events.try_recv().is_ok() {}
        assert!(
            tokio::time::timeout(Duration::from_millis(80), events.recv())
                .await
                .is_err()
        );
        configure(&actor, Some(Duration::from_millis(20))).await;
        event(&mut events).await;
        sys.shutdown_internal().await.unwrap();
    }
}

struct InitData {
    events: mpsc::UnboundedSender<()>,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: std::sync::Arc<tokio::sync::Notify>,
}
struct ThreadInit(InitData);
struct ActixInit(InitData);
impl ActixEngineExt for ActixInit {}
macro_rules! init_probe {
    ($actor:ident, $context:ty) => {
        impl Actor for $actor {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn init<'a>(
                &'a mut self,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    ctx.set_receive_timeout(Some(Duration::from_millis(20)));
                    self.0.entered.take().unwrap().send(()).unwrap();
                    self.0.release.notified().await;
                    Ok(())
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    assert!(matches!(
                        msg.downcast_ref::<LifecycleEvent>(),
                        Some(LifecycleEvent::ReceiveTimeout)
                    ));
                    self.0.events.send(()).unwrap();
                    Ok(Box::new(()) as BoxedMessage)
                })
            }
        }
    };
}
init_probe!(ThreadInit, parrot::thread::ThreadContext<Self>);
init_probe!(
    ActixInit,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);

#[actix::test]
async fn configuring_during_initialization_waits_for_successful_initialization() {
    for threaded in [false, true] {
        let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
        let actix = parrot::actix::ActixActorSystem::new().await.unwrap();
        let (send, mut events) = mpsc::unbounded_channel();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let data = InitData {
            events: send,
            entered: Some(entered),
            release: release.clone(),
        };
        let actor = if threaded {
            thread
                .spawn_at(ThreadInit(data), "/init", None, Default::default())
                .await
                .unwrap()
                .clone_boxed()
        } else {
            actix
                .spawn_root_typed(ActixInit(data), EmptyConfig)
                .await
                .unwrap()
                .clone_boxed()
        };
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(80), events.recv())
                .await
                .is_err()
        );
        let released = std::time::Instant::now();
        release.notify_one();
        event(&mut events).await;
        assert!(released.elapsed() >= Duration::from_millis(20));
        actor.stop().await.unwrap();
        thread.shutdown_internal().await.unwrap();
        actix.shutdown().await.unwrap();
    }
}

struct SyncProbe(mpsc::UnboundedSender<()>);
impl Actor for SyncProbe {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { panic!("the synchronous actor must not use async dispatch") })
    }
}
impl ActixEngineExt for SyncProbe {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
        _: parrot_api::actor::EngineContextHandle<'_>,
    ) -> Option<ActorResult<BoxedMessage>> {
        if matches!(
            msg.downcast_ref::<LifecycleEvent>(),
            Some(LifecycleEvent::ReceiveTimeout)
        ) {
            self.0.send(()).unwrap();
        } else if matches!(msg.downcast_ref::<Command>(), Some(Command::Enable)) {
            ctx.set_receive_timeout(Some(Duration::from_millis(20)));
        } else {
            return None;
        }
        Some(Ok(Box::new(())))
    }
}
#[actix::test]
async fn synchronous_actix_dispatch_receives_the_public_idle_event() {
    let sys = parrot::actix::ActixActorSystem::new().await.unwrap();
    let (send, mut events) = mpsc::unbounded_channel();
    let actor = sys
        .spawn_root_typed(SyncProbe(send), EmptyConfig)
        .await
        .unwrap();
    actor.send(Box::new(Command::Enable)).await.unwrap();
    event(&mut events).await;
    event(&mut events).await;
    sys.shutdown().await.unwrap();
}

#[derive(Debug)]
struct BusinessAdmission;
impl parrot::thread::mailbox::admission::MessageAdmission for BusinessAdmission {
    fn prepare(&self, msg: BoxedMessage) -> Result<BoxedMessage, parrot::thread::MailboxError> {
        if msg.is::<Command>() || msg.is::<()>() {
            Ok(msg)
        } else {
            Err(parrot::thread::MailboxError::PushError(
                "business-only protocol".into(),
            ))
        }
    }
}
struct BatchProbe(mpsc::UnboundedSender<()>);
impl Actor for BatchProbe {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn can_batch_message(&self, msg: &BoxedMessage) -> bool {
        !msg.is::<Command>()
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        Box::pin(async move { messages.into_iter().map(Ok).collect() })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if matches!(msg.downcast_ref::<Command>(), Some(Command::Enable)) {
                ctx.set_receive_timeout(Some(Duration::from_millis(20)));
            } else if matches!(
                msg.downcast_ref::<LifecycleEvent>(),
                Some(LifecycleEvent::ReceiveTimeout)
            ) {
                self.0.send(()).unwrap();
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_events_bypass_business_admission_and_batching_in_every_thread_mode() {
    for mode in modes() {
        let sys =
            parrot::thread::ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
                default_scheduling_mode: mode,
                shared_pool_size: 2,
                ..Default::default()
            });
        let (send, mut events) = mpsc::unbounded_channel();
        let actor = sys
            .spawn_at(
                BatchProbe(send),
                "/batch",
                None,
                parrot::thread::ThreadActorConfig {
                    message_admission: Some(std::sync::Arc::new(BusinessAdmission)),
                    message_batch_size: Some(8),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        actor.send(Box::new(Command::Enable)).await.unwrap();
        for _ in 0..8 {
            actor.deliver(Box::new(())).await.unwrap();
        }
        event(&mut events).await;
        sys.shutdown_internal().await.unwrap();
    }
}
probe!(ThreadProbe, parrot::thread::ThreadContext<Self>);
probe!(
    ActixProbe,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
struct Slow;
impl Actor for Slow {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(Box::new(17_u64) as BoxedMessage)
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_receive_timeout_emits_an_actual_idle_event() {
    let sys = parrot::thread::ThreadActorSystem::shared(Default::default());
    let (send, mut events) = mpsc::unbounded_channel();
    let actor = sys
        .spawn_at(ThreadProbe(send), "/idle", None, Default::default())
        .await
        .unwrap();
    actor.send(Box::new(Command::Enable)).await.unwrap();
    let result = tokio::time::timeout(Duration::from_millis(300), events.recv()).await;
    sys.shutdown_internal().await.unwrap();
    assert!(
        matches!(result, Ok(Some(()))),
        "receive timeout must be delivered to the idle actor"
    );
}
#[actix::test]
async fn actix_receive_timeout_emits_an_actual_idle_event() {
    let sys = parrot::actix::ActixActorSystem::new().await.unwrap();
    let (send, mut events) = mpsc::unbounded_channel();
    let actor = sys
        .spawn_root_typed(ActixProbe(send), EmptyConfig)
        .await
        .unwrap();
    actor.send(Box::new(Command::Enable)).await.unwrap();
    let result = tokio::time::timeout(Duration::from_millis(300), events.recv()).await;
    sys.shutdown().await.unwrap();
    assert!(
        matches!(result, Ok(Some(()))),
        "receive timeout must be delivered to the idle actor"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_timeout_does_not_shorten_an_unrelated_context_ask() {
    let sys = parrot::thread::ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
        default_ask_timeout: Duration::from_millis(500),
        shared_pool_size: 2,
        ..Default::default()
    });
    let (send, _) = mpsc::unbounded_channel();
    let actor = sys
        .spawn_at(ThreadProbe(send), "/caller", None, Default::default())
        .await
        .unwrap();
    let slow = sys
        .spawn_at(Slow, "/slow", None, Default::default())
        .await
        .unwrap();
    let result = actor.send(Box::new(Command::Ask(slow.clone_boxed()))).await;
    sys.shutdown_internal().await.unwrap();
    assert_eq!(
        *result
            .expect("idle receive timeout must not override the configured ask deadline")
            .downcast::<u64>()
            .unwrap(),
        17
    );
}
