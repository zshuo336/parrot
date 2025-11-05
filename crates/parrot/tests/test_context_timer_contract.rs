//! Timers register promptly, deliver without ask semantics, and belong to their creator.
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContext, ActorContextScheduler, ScheduledTask},
    errors::ActorError,
    message::CloneableMessage,
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

#[derive(Clone, Debug, Default)]
struct Sink {
    delivered: Arc<AtomicUsize>,
    asked: Arc<AtomicUsize>,
    changed: Arc<tokio::sync::Notify>,
    pending: bool,
    panic_on_poll: bool,
    panic_on_delivery_drop: bool,
    delivery_drops: Arc<AtomicUsize>,
    callback_drops: Arc<AtomicUsize>,
    delivery_delay: Duration,
    delivery_times: Arc<std::sync::Mutex<Vec<std::time::Instant>>>,
}
impl ActorRef for Sink {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(ActorError::MessageHandlingError(
                "timer must use deliver, not ask".into(),
            ))
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(msg)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        let future = Box::pin(async move {
            let _payload = message;
            let _callback = Payload(self.callback_drops.clone());
            self.delivered.fetch_add(1, Ordering::SeqCst);
            self.delivery_times
                .lock()
                .unwrap()
                .push(std::time::Instant::now());
            self.changed.notify_one();
            assert!(!self.panic_on_poll, "timer delivery panic");
            if self.pending {
                std::future::pending::<()>().await;
            }
            if !self.delivery_delay.is_zero() {
                tokio::time::sleep(self.delivery_delay).await;
            }
            Ok(())
        });
        Box::pin(DeliveryOwner {
            future,
            panic_on_drop: self.panic_on_delivery_drop,
            drops: self.delivery_drops.clone(),
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/sink".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[actix::test]
async fn periodic_waits_for_admission_then_interval_without_catchup_bursts() {
    for actix in [false, true] {
        let system = Backend::configured(actix, 1, Duration::from_secs(2)).await;
        let actor = system.actor().await;
        let sink = Sink {
            delivery_delay: Duration::from_millis(60),
            ..Default::default()
        };
        actor
            .send(Box::new(Schedule::Periodic(
                sink.clone_boxed(),
                Duration::ZERO,
                Duration::from_millis(30),
            )))
            .await
            .unwrap();
        sink.wait_for(3).await;
        actor.stop().await.unwrap();
        let times = sink.delivery_times.lock().unwrap().clone();
        assert_eq!(times.len(), 3);
        for pair in times.windows(2) {
            assert!(
                pair[1].duration_since(pair[0]) >= Duration::from_millis(90),
                "interval begins after admission completes"
            );
        }
        system.shutdown().await;
    }
}

struct ActixInitTimers {
    dropped: Arc<AtomicUsize>,
    initialized: Arc<tokio::sync::Notify>,
    hooks: Arc<AtomicUsize>,
    fail: bool,
}
impl parrot_api::actor::ActixEngineExt for ActixInitTimers {}
impl Actor for ActixInitTimers {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            ctx.schedule_once(
                Box::new(Sink::default()),
                Box::new(Payload(self.dropped.clone())),
                Duration::from_secs(3600),
            )
            .await?;
            self.initialized.notify_one();
            if self.fail {
                return Err(ActorError::InitializationError("injected".into()));
            }
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(Box::new(()) as BoxedMessage) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert_eq!(self.dropped.load(Ordering::SeqCst), 1);
            self.hooks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
#[actix::test]
async fn actix_initialization_failure_and_stop_hooks_observe_timer_cleanup() {
    for fail in [false, true] {
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let initialized = Arc::new(tokio::sync::Notify::new());
        let hooks = Arc::new(AtomicUsize::new(0));
        let actor = system
            .spawn_root_typed(
                ActixInitTimers {
                    dropped: dropped.clone(),
                    initialized: initialized.clone(),
                    hooks: hooks.clone(),
                    fail,
                },
                EmptyConfig,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), initialized.notified())
            .await
            .unwrap();
        if fail {
            tokio::time::timeout(Duration::from_secs(1), async {
                while dropped.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(actor.stop().await.is_err());
        } else {
            actor.send(Box::new(())).await.unwrap();
            actor.stop().await.unwrap();
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(hooks.load(Ordering::SeqCst), usize::from(!fail));
        system.shutdown().await.unwrap();
    }
}
impl Sink {
    async fn wait_for(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while self.delivered.load(Ordering::SeqCst) < count {
                self.changed.notified().await;
            }
        })
        .await
        .expect("scheduled delivery must arrive");
    }
}
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Clone)]
struct Tick;
impl parrot_api::message::Message for Tick {
    type Result = ();
}
enum Schedule {
    Once(BoxedActorRef, BoxedMessage, Duration),
    Periodic(BoxedActorRef, Duration, Duration),
}
async fn schedule(message: BoxedMessage, ctx: &mut dyn ActorContext) -> ActorResult<BoxedMessage> {
    match *message.downcast::<Schedule>().unwrap() {
        Schedule::Once(target, payload, delay) => ctx.schedule_once(target, payload, delay).await?,
        Schedule::Periodic(target, delay, interval) => {
            ctx.schedule_periodic(
                target,
                CloneableMessage::from_message(Tick),
                delay,
                interval,
            )
            .await?
        }
    }
    Ok(Box::new(()))
}
struct ThreadTimers;
impl Actor for ThreadTimers {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(schedule(message, ctx))
    }
}

enum TraitTimerCommand {
    Cancel,
}
struct ThreadTraitTimer {
    task: Option<ScheduledTask>,
    ticks: Arc<AtomicUsize>,
    changed: Arc<tokio::sync::Notify>,
}
impl Actor for ThreadTraitTimer {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.task = Some(
                ctx.schedule(Tick, Duration::ZERO, Some(Duration::from_millis(15)))
                    .await?,
            );
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.downcast_ref::<Tick>().is_some() {
                self.ticks.fetch_add(1, Ordering::SeqCst);
                self.changed.notify_waiters();
            } else if msg.downcast_ref::<TraitTimerCommand>().is_some()
                && let Some(task) = self.task.take()
            {
                ctx.cancel_schedule(task);
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}

struct ActixTraitTimer {
    task: Option<ScheduledTask>,
    ticks: Arc<AtomicUsize>,
    changed: Arc<tokio::sync::Notify>,
}
impl parrot_api::actor::ActixEngineExt for ActixTraitTimer {}
impl Actor for ActixTraitTimer {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.task = Some(
                ctx.schedule(Tick, Duration::ZERO, Some(Duration::from_millis(15)))
                    .await?,
            );
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.downcast_ref::<Tick>().is_some() {
                self.ticks.fetch_add(1, Ordering::SeqCst);
                self.changed.notify_waiters();
            } else if msg.downcast_ref::<TraitTimerCommand>().is_some()
                && let Some(task) = self.task.take()
            {
                ctx.cancel_schedule(task);
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}

#[actix::test]
async fn public_scheduler_trait_supports_periodic_cancel_on_thread_and_actix() {
    let ticks = Arc::new(AtomicUsize::new(0));
    let changed = Arc::new(tokio::sync::Notify::new());
    let system = ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_at(
            ThreadTraitTimer {
                task: None,
                ticks: ticks.clone(),
                changed: changed.clone(),
            },
            "/trait-timer",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while ticks.load(Ordering::SeqCst) < 2 {
            changed.notified().await;
        }
    })
    .await
    .unwrap();
    actor
        .send(Box::new(TraitTimerCommand::Cancel))
        .await
        .unwrap();
    let stopped_at = ticks.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(ticks.load(Ordering::SeqCst), stopped_at);
    system.shutdown_internal().await.unwrap();

    let ticks = Arc::new(AtomicUsize::new(0));
    let changed = Arc::new(tokio::sync::Notify::new());
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let actor = system
        .spawn_root_typed(
            ActixTraitTimer {
                task: None,
                ticks: ticks.clone(),
                changed: changed.clone(),
            },
            EmptyConfig,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while ticks.load(Ordering::SeqCst) < 2 {
            changed.notified().await;
        }
    })
    .await
    .unwrap();
    actor
        .send(Box::new(TraitTimerCommand::Cancel))
        .await
        .unwrap();
    let stopped_at = ticks.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(ticks.load(Ordering::SeqCst), stopped_at);
    system.shutdown().await.unwrap();
}
struct ActixTimers;
impl parrot_api::actor::ActixEngineExt for ActixTimers {}
impl Actor for ActixTimers {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(schedule(message, ctx))
    }
}
enum Backend {
    Actix(ActixActorSystem),
    Thread(Arc<ThreadActorSystem>),
}
impl Backend {
    async fn new(actix: bool) -> Self {
        if actix {
            Self::Actix(ActixActorSystem::with_arbiter_count(1).await.unwrap())
        } else {
            Self::Thread(ThreadActorSystem::shared(Default::default()))
        }
    }
    async fn actor(&self) -> BoxedActorRef {
        match self {
            Self::Actix(system) => system
                .spawn_root_typed_with_strategy(
                    ActixTimers,
                    EmptyConfig,
                    parrot_api::supervisor::SupervisorStrategyType::Default(
                        parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
                    ),
                )
                .await
                .unwrap(),
            Self::Thread(system) => system
                .spawn_at(
                    ThreadTimers,
                    "/timers",
                    None,
                    parrot::thread::ThreadActorConfig {
                        supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Resume),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .clone_boxed(),
        }
    }
    async fn shutdown(self) {
        match self {
            Self::Actix(system) => system.shutdown().await.unwrap(),
            Self::Thread(system) => system.shutdown_internal().await.unwrap(),
        }
    }
}

#[tokio::test]
async fn file_and_builder_task_limits_control_actual_admission() {
    let path = std::env::temp_dir().join(format!("parrot-timer-{}.toml", uuid::Uuid::new_v4()));
    std::fs::write(&path, "[thread]\nmax_context_tasks = 1\n").unwrap();
    for override_limit in [None, Some(2)] {
        let mut builder = parrot_config::ParrotConfig::builder()
            .load_file(&path)
            .unwrap();
        if let Some(limit) = override_limit {
            builder = builder.thread_max_context_tasks(limit);
        }
        let resolved = builder.build().unwrap();
        let system = ThreadActorSystem::shared(
            parrot::thread::config::ThreadActorSystemConfig::from_resolved(&resolved),
        );
        let actor = system
            .spawn_at(
                ThreadTimers,
                "/timers",
                None,
                parrot::thread::ThreadActorConfig {
                    supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Resume),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let limit = override_limit.unwrap_or(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        for index in 0..=limit {
            let result = actor
                .send(Box::new(Schedule::Once(
                    Box::new(Sink::default()),
                    Box::new(Payload(dropped.clone())),
                    Duration::from_secs(3600),
                )))
                .await;
            assert_eq!(result.is_ok(), index < limit);
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        system.shutdown_internal().await.unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), limit + 1);
    }
    std::fs::remove_file(path).unwrap();
}

struct RestartTimers {
    generation: usize,
    dropped: Arc<AtomicUsize>,
    fail_init: bool,
}
impl Actor for RestartTimers {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert_eq!(
                self.dropped.load(Ordering::SeqCst),
                self.generation,
                "replacement must follow old generation timer disposal"
            );
            ctx.schedule_once(
                Box::new(Sink::default()),
                Box::new(Payload(self.dropped.clone())),
                Duration::from_secs(3600),
            )
            .await?;
            if self.fail_init && self.generation == 0 {
                return Err(ActorError::InternalError(
                    "initialization failure after timer registration".into(),
                ));
            }
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            assert!(!msg.is::<bool>(), "retire this generation");
            Ok(Box::new(self.generation) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert_eq!(
                self.dropped.load(Ordering::SeqCst),
                self.generation + 1,
                "stop hook must observe released timer resources"
            );
            Ok(())
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_and_failed_initialization_retire_timers_before_replacement() {
    use parrot::thread::config::{SchedulingMode, SupervisorStrategy, ThreadActorSystemConfig};
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 2,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "restart-timers".into(),
        },
    ] {
        for fail_init in [false, true] {
            let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
                default_scheduling_mode: mode.clone(),
                ..Default::default()
            });
            let dropped = Arc::new(AtomicUsize::new(0));
            let generation = Arc::new(AtomicUsize::new(0));
            let resources = dropped.clone();
            let actor = system
                .spawn_supervised(
                    move || RestartTimers {
                        generation: generation.fetch_add(1, Ordering::SeqCst),
                        dropped: resources.clone(),
                        fail_init,
                    },
                    "/timers",
                    SupervisorStrategy::Restart {
                        max_retries: 1,
                        within: Duration::from_secs(60),
                    },
                )
                .await
                .unwrap();
            if !fail_init {
                let _ = actor.send(Box::new(true)).await;
            }
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Ok(reply) = actor
                        .send_with_timeout(Box::new(()), Some(Duration::from_millis(50)))
                        .await
                        && *reply.downcast::<usize>().unwrap() == 1
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("replacement generation must become ready");
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            actor.stop().await.unwrap();
            assert_eq!(dropped.load(Ordering::SeqCst), 2);
            system.shutdown_internal().await.unwrap();
        }
    }
}

#[actix::test]
async fn dropping_system_cancels_timers_even_with_surviving_actor_reference() {
    for actix in [false, true] {
        let system = Backend::new(actix).await;
        let actor = system.actor().await;
        let dropped = Arc::new(AtomicUsize::new(0));
        actor
            .send(Box::new(Schedule::Once(
                Box::new(Sink::default()),
                Box::new(Payload(dropped.clone())),
                Duration::from_secs(3600),
            )))
            .await
            .unwrap();
        drop(system);
        tokio::time::timeout(Duration::from_secs(2), async {
            while dropped.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("system drop must cancel actor-owned timers");
        assert!(!actor.is_alive().await);
    }
}

#[actix::test]
async fn cancelled_stop_waiter_does_not_abandon_timer_disposal() {
    for actix in [false, true] {
        let system = Backend::new(actix).await;
        let actor = system.actor().await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(tokio::sync::Notify::new());
        actor
            .send(Box::new(Schedule::Once(
                Box::new(Sink::default()),
                Box::new(SlowPayload {
                    entered: entered.clone(),
                    finished: finished.clone(),
                }),
                Duration::from_secs(3600),
            )))
            .await
            .unwrap();
        let stopping = actor.clone_boxed();
        let waiter = tokio::spawn(async move { stopping.stop().await });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        waiter.abort();
        let _ = waiter.await;
        tokio::time::timeout(Duration::from_secs(1), actor.stop())
            .await
            .unwrap()
            .unwrap();
        // A notify permit proves disposal finished, independent of registry removal.
        tokio::time::timeout(Duration::from_millis(100), finished.notified())
            .await
            .unwrap();
        system.shutdown().await;
    }
}
async fn periodic_contract(actix: bool) {
    let system = Backend::new(actix).await;
    let actor = system.actor().await;
    let sink = Sink::default();
    tokio::time::timeout(
        Duration::from_millis(200),
        actor.send(Box::new(Schedule::Periodic(
            sink.clone_boxed(),
            Duration::ZERO,
            Duration::from_millis(10),
        ))),
    )
    .await
    .expect("registration must return without waiting for the recurring loop")
    .unwrap();
    sink.wait_for(3).await;
    assert_eq!(sink.asked.load(Ordering::SeqCst), 0);
    actor.stop().await.unwrap();
    let at_stop = sink.delivered.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        sink.delivered.load(Ordering::SeqCst),
        at_stop,
        "stopped creator must leave no recurring publisher"
    );
    system.shutdown().await;
}
async fn stop_releases_delayed_payload(actix: bool) {
    let system = Backend::new(actix).await;
    let actor = system.actor().await;
    let dropped = Arc::new(AtomicUsize::new(0));
    actor
        .send(Box::new(Schedule::Once(
            Box::new(Sink::default()),
            Box::new(Payload(dropped.clone())),
            Duration::from_secs(3600),
        )))
        .await
        .unwrap();
    actor.stop().await.unwrap();
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "stop success must follow delayed payload disposal"
    );
    system.shutdown().await;
}
async fn once_uses_delivery(actix: bool) {
    let system = Backend::new(actix).await;
    let actor = system.actor().await;
    let sink = Sink::default();
    actor
        .send(Box::new(Schedule::Once(
            sink.clone_boxed(),
            Box::new(1u8),
            Duration::ZERO,
        )))
        .await
        .unwrap();
    sink.wait_for(1).await;
    assert_eq!(sink.asked.load(Ordering::SeqCst), 0);
    system.shutdown().await;
}
async fn zero_interval_rejected(actix: bool) {
    let system = Backend::new(actix).await;
    let actor = system.actor().await;
    let result = tokio::time::timeout(
        Duration::from_millis(200),
        actor.send(Box::new(Schedule::Periodic(
            Box::new(Sink::default()),
            Duration::from_secs(3600),
            Duration::ZERO,
        ))),
    )
    .await;
    assert!(
        matches!(result, Ok(Err(_))),
        "zero interval must be rejected by registration"
    );
    system.shutdown().await;
}
#[actix::test]
async fn actix_periodic_registration_and_cancellation() {
    periodic_contract(true).await;
}
#[tokio::test]
async fn thread_periodic_delivery_and_cancellation() {
    periodic_contract(false).await;
}
#[actix::test]
async fn actix_stop_releases_delayed_payload() {
    stop_releases_delayed_payload(true).await;
}
#[tokio::test]
async fn thread_stop_releases_delayed_payload() {
    stop_releases_delayed_payload(false).await;
}
#[actix::test]
async fn actix_once_uses_deliver() {
    once_uses_delivery(true).await;
}
#[tokio::test]
async fn thread_once_uses_deliver() {
    once_uses_delivery(false).await;
}
#[actix::test]
async fn actix_zero_interval_rejected() {
    zero_interval_rejected(true).await;
}
#[tokio::test]
async fn thread_zero_interval_rejected() {
    zero_interval_rejected(false).await;
}

impl Backend {
    async fn configured(actix: bool, limit: usize, timeout: Duration) -> Self {
        if actix {
            Self::Actix(
                ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
                    arbiter_count: 1,
                    max_context_tasks: limit,
                    shutdown_timeout: timeout,
                    ..Default::default()
                })
                .await
                .unwrap(),
            )
        } else {
            Self::Thread(ThreadActorSystem::shared(
                parrot::thread::config::ThreadActorSystemConfig {
                    shared_pool_size: 1,
                    max_context_tasks: limit,
                    shutdown_timeout: timeout,
                    ..Default::default()
                },
            ))
        }
    }
}
#[actix::test]
async fn stop_cancels_inflight_timer_delivery_before_acknowledgement() {
    for actix in [false, true] {
        let system = Backend::new(actix).await;
        let actor = system.actor().await;
        let sink = Sink {
            pending: true,
            ..Default::default()
        };
        let dropped = Arc::new(AtomicUsize::new(0));
        actor
            .send(Box::new(Schedule::Once(
                sink.clone_boxed(),
                Box::new(Payload(dropped.clone())),
                Duration::ZERO,
            )))
            .await
            .unwrap();
        sink.wait_for(1).await;
        actor.stop().await.unwrap();
        assert_eq!(sink.callback_drops.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        system.shutdown().await;
    }
}
#[actix::test]
async fn callback_panic_isolated_and_reported_by_stop() {
    for actix in [false, true] {
        let system = Backend::new(actix).await;
        let actor = system.actor().await;
        let sink = Sink {
            panic_on_poll: true,
            ..Default::default()
        };
        actor
            .send(Box::new(Schedule::Once(
                sink.clone_boxed(),
                Box::new(()),
                Duration::ZERO,
            )))
            .await
            .unwrap();
        sink.wait_for(1).await;
        assert!(
            actor
                .stop()
                .await
                .unwrap_err()
                .to_string()
                .contains("timer delivery panic")
        );
        assert_eq!(sink.callback_drops.load(Ordering::SeqCst), 1);
        system.shutdown().await;
    }
}
struct PanicPayload(Arc<AtomicUsize>);
impl Drop for PanicPayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("timer payload destructor panic");
    }
}
#[actix::test]
async fn cancellation_destructor_panic_does_not_strand_other_timers() {
    for actix in [false, true] {
        let system = Backend::new(actix).await;
        let actor = system.actor().await;
        let dropped = Arc::new(AtomicUsize::new(0));
        actor
            .send(Box::new(Schedule::Once(
                Box::new(Sink::default()),
                Box::new(PanicPayload(dropped.clone())),
                Duration::from_secs(3600),
            )))
            .await
            .unwrap();
        actor
            .send(Box::new(Schedule::Once(
                Box::new(Sink::default()),
                Box::new(Payload(dropped.clone())),
                Duration::from_secs(3600),
            )))
            .await
            .unwrap();
        assert!(
            actor
                .stop()
                .await
                .unwrap_err()
                .to_string()
                .contains("timer payload destructor panic")
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        system.shutdown().await;
    }
}
#[actix::test]
async fn capacity_rejects_excess_tasks_and_recovers_after_completion() {
    for actix in [false, true] {
        let system = Backend::configured(actix, 2, Duration::from_secs(1)).await;
        let actor = system.actor().await;
        let dropped = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            actor
                .send(Box::new(Schedule::Once(
                    Box::new(Sink::default()),
                    Box::new(Payload(dropped.clone())),
                    Duration::from_secs(3600),
                )))
                .await
                .unwrap();
        }
        let rejected = actor
            .send(Box::new(Schedule::Once(
                Box::new(Sink::default()),
                Box::new(Payload(dropped.clone())),
                Duration::ZERO,
            )))
            .await;
        assert!(rejected.unwrap_err().to_string().contains("task limit 2"));
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "rejected payload must be released immediately"
        );
        actor.stop().await.unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
        system.shutdown().await;

        let system = Backend::configured(actix, 1, Duration::from_secs(1)).await;
        let actor = system.actor().await;
        let sink = Sink::default();
        for expected in 1..=5 {
            // Completion publication can race delivery observation. Retry only
            // the explicit capacity result until that one old task retires.
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    match actor
                        .send(Box::new(Schedule::Once(
                            sink.clone_boxed(),
                            Box::new(()),
                            Duration::ZERO,
                        )))
                        .await
                    {
                        Ok(_) => break,
                        Err(error) if error.to_string().contains("task limit 1") => {
                            tokio::task::yield_now().await
                        }
                        Err(error) => panic!("unexpected timer admission error: {error}"),
                    }
                }
            })
            .await
            .unwrap();
            sink.wait_for(expected).await;
        }
        system.shutdown().await;
    }
}
#[actix::test]
async fn impossible_delays_and_disabled_scheduling_fail_at_admission() {
    for actix in [false, true] {
        let system = Backend::new(actix).await;
        let actor = system.actor().await;
        assert!(
            actor
                .send(Box::new(Schedule::Once(
                    Box::new(Sink::default()),
                    Box::new(()),
                    Duration::MAX
                )))
                .await
                .unwrap_err()
                .to_string()
                .contains("clock range")
        );
        assert!(
            actor
                .send(Box::new(Schedule::Periodic(
                    Box::new(Sink::default()),
                    Duration::ZERO,
                    Duration::MAX
                )))
                .await
                .unwrap_err()
                .to_string()
                .contains("clock range")
        );
        system.shutdown().await;
        let system = Backend::configured(actix, 0, Duration::from_secs(1)).await;
        let actor = system.actor().await;
        assert!(
            actor
                .send(Box::new(Schedule::Once(
                    Box::new(Sink::default()),
                    Box::new(()),
                    Duration::ZERO
                )))
                .await
                .unwrap_err()
                .to_string()
                .contains("task limit 0")
        );
        system.shutdown().await;
    }
}
struct SlowPayload {
    entered: Arc<tokio::sync::Notify>,
    finished: Arc<tokio::sync::Notify>,
}
impl Drop for SlowPayload {
    fn drop(&mut self) {
        self.entered.notify_one();
        std::thread::sleep(Duration::from_millis(250));
        self.finished.notify_one();
    }
}
#[actix::test]
async fn blocking_timer_disposal_does_not_block_shutdown_deadline() {
    for actix in [false, true] {
        let system = Backend::configured(actix, 1, Duration::from_millis(40)).await;
        let actor = system.actor().await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(tokio::sync::Notify::new());
        actor
            .send(Box::new(Schedule::Once(
                Box::new(Sink::default()),
                Box::new(SlowPayload {
                    entered: entered.clone(),
                    finished: finished.clone(),
                }),
                Duration::from_secs(3600),
            )))
            .await
            .unwrap();
        let stopping = actor.clone_boxed();
        let stop = tokio::spawn(async move { stopping.stop().await });
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_millis(150), stop)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(result, Err(ActorError::TimeoutDetail(_))),
            "blocking cleanup must report deadline expiry"
        );
        tokio::time::timeout(Duration::from_secs(1), finished.notified())
            .await
            .unwrap();
        // Payload disposal happens inside owned cleanup, before the monitor
        // receives its completion. Wait for retirement, not a scheduling guess.
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let registered = match &system {
                    Backend::Actix(system) => system.get_actor(&actor.path()).await.is_some(),
                    Backend::Thread(system) => system.get_actor_ref(&actor.path()).is_some(),
                };
                if !registered {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup must retire actor registration");
        system.shutdown().await;
    }
}
#[tokio::test]
async fn thread_all_scheduler_modes_join_owned_timers() {
    use parrot::thread::config::{SchedulingMode, ThreadActorSystemConfig};
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 2,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "timer-owner".into(),
        },
    ] {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            default_scheduling_mode: mode,
            ..Default::default()
        });
        let actor = system
            .spawn_at(
                ThreadTimers,
                "/timers",
                None,
                parrot::thread::ThreadActorConfig {
                    supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Resume),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        actor
            .send(Box::new(Schedule::Once(
                Box::new(Sink::default()),
                Box::new(Payload(dropped.clone())),
                Duration::from_secs(3600),
            )))
            .await
            .unwrap();
        actor.stop().await.unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        system.shutdown_internal().await.unwrap();
    }
}

struct DeliveryOwner<'a> {
    future: BoxedFuture<'a, ActorResult<()>>,
    panic_on_drop: bool,
    drops: Arc<AtomicUsize>,
}
impl std::future::Future for DeliveryOwner<'_> {
    type Output = ActorResult<()>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.future.as_mut().poll(cx)
    }
}
impl Drop for DeliveryOwner<'_> {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.panic_on_drop,
            "independent timer delivery destructor panic"
        );
    }
}
fn independent_schedule_panics(actix: bool, periodic: bool, name: &str) {
    const FLAG: &str = "PARROT_NATIVE_SCHEDULE_PANICS";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        ::actix::System::new().block_on(async {
            let system = Backend::configured(actix, 1, Duration::from_secs(2)).await;
            let actor = system.actor().await;
            let sink = Sink {
                panic_on_poll: true,
                panic_on_delivery_drop: true,
                ..Default::default()
            };
            let command = if periodic {
                Schedule::Periodic(
                    sink.clone_boxed(),
                    Duration::ZERO,
                    Duration::from_millis(10),
                )
            } else {
                Schedule::Once(sink.clone_boxed(), Box::new(()), Duration::ZERO)
            };
            actor.send(Box::new(command)).await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while sink.delivery_drops.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("delivery storage not retired");
            let replacement = Sink::default();
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match actor
                        .send(Box::new(Schedule::Once(
                            replacement.clone_boxed(),
                            Box::new(()),
                            Duration::ZERO,
                        )))
                        .await
                    {
                        Ok(_) => break,
                        Err(ActorError::InternalError(message))
                            if message.contains("task limit") =>
                        {
                            tokio::task::yield_now().await
                        }
                        Err(error) => panic!("actor cannot accept replacement timer: {error}"),
                    }
                }
            })
            .await
            .expect("task capacity not reused");
            replacement.wait_for(1).await;
            assert_eq!(sink.delivered.load(Ordering::SeqCst), 1);
            assert_eq!(sink.delivery_drops.load(Ordering::SeqCst), 1);
            let first = tokio::time::timeout(Duration::from_secs(2), actor.stop())
                .await
                .unwrap()
                .unwrap_err()
                .to_string();
            assert!(
                first.contains("timer delivery panic"),
                "lost first failure: {first}"
            );
            let second = tokio::time::timeout(Duration::from_secs(2), actor.stop())
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(
                first,
                second.to_string(),
                "every waiter must retain the timer's original failure after retirement"
            );
            assert!(!actor.is_alive().await);
            system.shutdown().await;
        });
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(FLAG, name)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "isolated native timer: {status}");
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("native timer timeout");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn thread_once_independent_schedule_panics() {
    independent_schedule_panics(false, false, "thread_once_independent_schedule_panics");
}
#[test]
fn thread_periodic_independent_schedule_panics() {
    independent_schedule_panics(false, true, "thread_periodic_independent_schedule_panics");
}
#[test]
fn actix_once_independent_schedule_panics() {
    independent_schedule_panics(true, false, "actix_once_independent_schedule_panics");
}
#[test]
fn actix_periodic_independent_schedule_panics() {
    independent_schedule_panics(true, true, "actix_periodic_independent_schedule_panics");
}

#[actix::test]
async fn actual_thread_and_actix_timers_record_admission_to_cleanup() {
    use parrot_api::trace::{SpanKind, SpanStatus, TraceRecorder};
    for actix in [false, true] {
        for mode in 0..3 {
            let system = Backend::new(actix).await;
            let actor = system.actor().await;
            let recorder = TraceRecorder::new(16).unwrap();
            let root = recorder.root();
            let sink = Sink::default();
            let message = match mode {
                0 => Schedule::Once(sink.clone_boxed(), Box::new(()), Duration::ZERO),
                1 => Schedule::Once(sink.clone_boxed(), Box::new(()), Duration::from_secs(3600)),
                _ => Schedule::Periodic(
                    sink.clone_boxed(),
                    Duration::from_secs(3600),
                    Duration::from_secs(1),
                ),
            };
            tokio::time::timeout(
                Duration::from_secs(2),
                root.in_scope(|| actor.send(Box::new(message))),
            )
            .await
            .unwrap()
            .unwrap();
            if mode == 0 {
                sink.wait_for(1).await;
                tokio::time::timeout(Duration::from_secs(2), async {
                    while !recorder
                        .snapshot()
                        .records
                        .iter()
                        .any(|r| r.kind == SpanKind::ManagedTask)
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            } else {
                assert_eq!(recorder.snapshot().counts.in_flight, 1);
                assert_eq!(sink.delivered.load(Ordering::SeqCst), 0);
            }
            actor.stop().await.unwrap();
            system.shutdown().await;
            recorder.close();
            let snapshot = recorder.drain();
            assert!(snapshot.terminal);
            assert_eq!(snapshot.records.len(), 2);
            let handler = snapshot
                .records
                .iter()
                .find(|r| r.kind == SpanKind::DynamicHandler)
                .unwrap();
            let task = snapshot
                .records
                .iter()
                .find(|r| r.kind == SpanKind::ManagedTask)
                .unwrap();
            assert_eq!(handler.parent_span_id, Some(root.span_id()));
            assert_eq!(task.parent_span_id, Some(handler.span_id));
            assert_eq!(task.trace_id, root.trace_id());
            assert_ne!(task.span_id, handler.span_id);
            assert_eq!(
                task.status,
                if mode == 0 {
                    SpanStatus::Ok
                } else {
                    SpanStatus::Cancelled
                }
            );
            assert_eq!(
                task.target,
                if mode == 2 {
                    "context.schedule.periodic"
                } else {
                    "context.schedule.once"
                }
            );
            assert_eq!(sink.asked.load(Ordering::SeqCst), 0);
        }
    }
}
