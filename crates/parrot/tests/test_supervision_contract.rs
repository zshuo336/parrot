//! Behavioral supervision contracts: exact budgets and generation ownership.
use parrot::thread::{
    address::ThreadActorRef,
    config::{SupervisorStrategy, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct PausedMessage {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
    crash: bool,
}

struct Generation {
    number: usize,
    panic_init: bool,
}
impl Actor for Generation {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert!(!self.panic_init, "initial generation failed");
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<PausedMessage>() {
                let pause = *msg.downcast::<PausedMessage>().unwrap();
                pause.entered.send(()).unwrap();
                let _ = pause.release.await;
                assert!(!pause.crash, "generation crash after release");
                return Ok(Box::new(self.number) as BoxedMessage);
            }
            assert!(
                msg.downcast_ref::<&str>() != Some(&"crash"),
                "generation crash"
            );
            if msg.downcast_ref::<&str>() == Some(&"thread") {
                return Ok(Box::new(
                    std::thread::current()
                        .name()
                        .unwrap_or("unnamed")
                        .to_owned(),
                ) as BoxedMessage);
            }
            Ok(Box::new(self.number) as BoxedMessage)
        })
    }
}
fn strategy() -> SupervisorStrategy {
    SupervisorStrategy::Restart {
        max_retries: 2,
        within: Duration::from_secs(60),
    }
}
async fn generation(actor: &dyn ActorRef, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(reply) = actor
                .send_with_timeout(Box::new(()), Some(Duration::from_millis(50)))
                .await
            {
                let observed = *reply.downcast::<usize>().unwrap();
                assert!(observed <= expected, "unexpected extra restart");
                if observed == expected {
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected generation did not become responsive");
}
async fn removed(system: &ThreadActorSystem) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while system.get_actor_ref("/victim").is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed actor was not removed");
}
async fn supervised(
    system: &Arc<ThreadActorSystem>,
    count: Arc<AtomicUsize>,
) -> ThreadActorRef<Generation> {
    system
        .spawn_supervised(
            move || Generation {
                number: count.fetch_add(1, Ordering::SeqCst),
                panic_init: false,
            },
            "/victim",
            strategy(),
        )
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn original_typed_and_erased_references_survive_restart() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    let count = Arc::new(AtomicUsize::new(0));
    let actor = supervised(&system, count.clone()).await;
    let erased = actor.clone_boxed();
    generation(&actor, 0).await;
    actor.send_msg(Box::new("crash")).await.unwrap();
    generation(&actor, 1).await;
    generation(erased.as_ref(), 1).await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_retries_produce_exactly_three_generations() {
    let system = ThreadActorSystem::shared(Default::default());
    let count = Arc::new(AtomicUsize::new(0));
    let _actor = supervised(&system, count.clone()).await;
    // Deliberately resolve the registry here to isolate budget correctness
    // from the independent stable-reference contract above.
    for number in 0..3 {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(actor) = system.get_actor_ref("/victim")
                    && let Ok(reply) = actor
                        .send_with_timeout(Box::new(()), Some(Duration::from_millis(50)))
                        .await
                    && *reply.downcast::<usize>().unwrap() == number
                {
                    let _ = actor
                        .send_with_timeout(Box::new("crash"), Some(Duration::from_millis(50)))
                        .await;
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("restart budget was consumed too early");
    }
    removed(&system).await;
    assert_eq!(count.load(Ordering::SeqCst), 3);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn obsolete_processor_cannot_stop_manual_replacement() {
    let system = ThreadActorSystem::shared(Default::default());
    let _actor = supervised(&system, Arc::new(AtomicUsize::new(0))).await;
    let old_mailbox = system.get_mailbox("/victim").unwrap();
    let old_processor = old_mailbox.get_processor().unwrap();
    system.stop_actor("/victim").await.unwrap();
    let replacement = system
        .spawn_at(
            Generation {
                number: 99,
                panic_init: false,
            },
            "/victim",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    old_processor.notify_system_panic("late old-generation callback".into());
    // Repeated replies across scheduler turns exercise the asynchronously
    // dispatched callback, rather than reading registry immediately once.
    for _ in 0..100 {
        generation(&replacement, 99).await;
    }
    system.shutdown_internal().await.unwrap();
}

fn modes() -> [parrot::thread::config::SchedulingMode; 3] {
    use parrot::thread::config::SchedulingMode;
    [
        SchedulingMode::SharedPool {
            max_messages_per_run: 2,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "supervision".into(),
        },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_panic_has_supervision_registered_in_every_mode() {
    for mode in modes() {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            default_scheduling_mode: mode,
            ..Default::default()
        });
        let count = Arc::new(AtomicUsize::new(0));
        let generations = count.clone();
        let actor = system
            .spawn_supervised(
                move || {
                    let number = generations.fetch_add(1, Ordering::SeqCst);
                    Generation {
                        number,
                        panic_init: number == 0,
                    }
                },
                "/victim",
                strategy(),
            )
            .await
            .unwrap();
        generation(&actor, 1).await;
        actor.send_msg(Box::new("crash")).await.unwrap();
        generation(&actor, 2).await;
        actor.send_msg(Box::new("crash")).await.unwrap();
        removed(&system).await;
        assert_eq!(count.load(Ordering::SeqCst), 3);
        system.shutdown_internal().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_and_old_generation_callbacks_do_not_charge_next_generation() {
    let system = ThreadActorSystem::shared(Default::default());
    let count = Arc::new(AtomicUsize::new(0));
    let actor = supervised(&system, count.clone()).await;
    generation(&actor, 0).await;
    let old = system.get_mailbox("/victim").unwrap();
    let processor = old.get_processor().unwrap();
    for _ in 0..20 {
        processor.notify_system_panic("duplicate callback".into());
    }
    generation(&actor, 1).await;
    for _ in 0..20 {
        processor.notify_system_panic("late callback".into());
    }
    actor.send_msg(Box::new("crash")).await.unwrap();
    generation(&actor, 2).await;
    assert_eq!(count.load(Ordering::SeqCst), 3);
    actor.send_msg(Box::new("crash")).await.unwrap();
    removed(&system).await;
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_configuration_survives_restart() {
    use parrot::thread::config::{BackpressureStrategy, SchedulingMode, ThreadActorConfig};
    for mode in modes() {
        let system = ThreadActorSystem::shared(Default::default());
        let count = Arc::new(AtomicUsize::new(0));
        let generations = count.clone();
        let actor = system
            .spawn_supervised_with_config(
                move || Generation {
                    number: generations.fetch_add(1, Ordering::SeqCst),
                    panic_init: false,
                },
                "/victim",
                ThreadActorConfig {
                    scheduling_mode: Some(mode.clone()),
                    mailbox_capacity: Some(7),
                    backpressure_strategy: Some(BackpressureStrategy::Error),
                    ask_timeout: Some(Duration::from_millis(30)),
                    supervisor_strategy: Some(SupervisorStrategy::Restart {
                        max_retries: 1,
                        within: Duration::from_secs(60),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        generation(&actor, 0).await;
        let first_thread = actor
            .ask_with_timeout(Box::new("thread"), Duration::from_secs(2))
            .await
            .unwrap()
            .downcast::<String>()
            .unwrap();
        actor.send_msg(Box::new("crash")).await.unwrap();
        generation(&actor, 1).await;
        let mailbox = system.get_mailbox("/victim").unwrap();
        assert_eq!(mailbox.capacity(), 7);
        let current = system.get_actor_ref("/victim").unwrap();
        let typed = current
            .as_any()
            .downcast_ref::<ThreadActorRef<Generation>>()
            .unwrap();
        assert_eq!(typed.default_timeout(), Duration::from_millis(30));
        // Hold the consumer so Error policy and capacity are observable.
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, paused) = tokio::sync::oneshot::channel();
        current
            .deliver(Box::new(PausedMessage {
                entered,
                release: paused,
                crash: false,
            }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), started)
            .await
            .unwrap()
            .unwrap();
        for _ in 0..7 {
            current.deliver(Box::new(())).await.unwrap();
        }
        let rejected =
            tokio::time::timeout(Duration::from_millis(500), current.deliver(Box::new(())))
                .await
                .unwrap();
        assert!(rejected.unwrap_err().to_string().contains("Full"));
        release.send(()).unwrap();
        generation(&actor, 1).await;

        let next_thread = actor
            .ask_with_timeout(Box::new("thread"), Duration::from_secs(2))
            .await
            .unwrap()
            .downcast::<String>()
            .unwrap();
        if matches!(
            mode,
            SchedulingMode::Sharded { .. } | SchedulingMode::DedicatedThread
        ) {
            assert_eq!(
                first_thread, next_thread,
                "configured owner thread name must be retained"
            );
            assert!(!next_thread.contains("tokio"));
        }
        // Stop at the explicitly configured one-retry budget, not the default 3.
        actor.send_msg(Box::new("crash")).await.unwrap();
        removed(&system).await;
        assert_eq!(count.load(Ordering::SeqCst), 2);
        system.shutdown_internal().await.unwrap();
    }
}

struct Watcher {
    events: tokio::sync::mpsc::UnboundedSender<parrot::thread::system::Terminated>,
}
impl Actor for Watcher {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(event) = msg.downcast_ref::<parrot::thread::system::Terminated>() {
                self.events.send(event.clone()).unwrap();
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_survives_restarts_and_receives_one_final_panic_reason() {
    use parrot_api::supervisor::DeathReason;
    let system = ThreadActorSystem::shared(Default::default());
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let watcher = system
        .spawn_at(Watcher { events }, "/watcher", None, Default::default())
        .await
        .unwrap();
    let actor = supervised(&system, Arc::new(AtomicUsize::new(0))).await;
    system
        .watch("/watcher".into(), "/victim".into())
        .await
        .unwrap();
    for number in 0..3 {
        generation(&actor, number).await;
        actor.send_msg(Box::new("crash")).await.unwrap();
    }
    removed(&system).await;
    // Final notification was enqueued before removal. The query proves the
    // watcher has drained the high-priority events before inspecting them.
    watcher
        .ask_with_timeout(Box::new(()), Duration::from_secs(2))
        .await
        .unwrap();
    let event = received
        .try_recv()
        .expect("final termination must be delivered");
    assert_eq!(event.path, "/victim");
    assert!(
        matches!(event.reason, DeathReason::Panic(ref message) if message.contains("generation crash")),
        "actual panic reason: {:?}",
        event.reason
    );
    assert!(
        received.try_recv().is_err(),
        "restart must not emit terminal death"
    );
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn panicking_restart_factory_is_cleaned_up() {
    let system = ThreadActorSystem::shared(Default::default());
    let count = Arc::new(AtomicUsize::new(0));
    let generations = count.clone();
    let actor = system
        .spawn_supervised(
            move || {
                assert_eq!(
                    generations.fetch_add(1, Ordering::SeqCst),
                    0,
                    "factory panic"
                );
                Generation {
                    number: 0,
                    panic_init: false,
                }
            },
            "/victim",
            strategy(),
        )
        .await
        .unwrap();
    generation(&actor, 0).await;
    actor.send_msg(Box::new("crash")).await.unwrap();
    removed(&system).await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
    system.shutdown_internal().await.unwrap();
}

struct FactoryGate {
    started: tokio::sync::Notify,
    released: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
    finished: tokio::sync::Notify,
}
impl FactoryGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: Default::default(),
            released: Default::default(),
            wake: Default::default(),
            finished: Default::default(),
        })
    }
    fn block(&self) {
        self.started.notify_one();
        let (_guard, timeout) = self
            .wake
            .wait_timeout_while(
                self.released.lock().unwrap(),
                Duration::from_secs(3),
                |released| !*released,
            )
            .unwrap();
        assert!(!timeout.timed_out(), "test failed to release factory");
        self.finished.notify_one();
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}
struct ReleaseFactory(Arc<FactoryGate>);
impl Drop for ReleaseFactory {
    fn drop(&mut self) {
        self.0.release();
    }
}

async fn blocked_factory(
    system: &Arc<ThreadActorSystem>,
    gate: Arc<FactoryGate>,
) -> ThreadActorRef<Generation> {
    let count = AtomicUsize::new(0);
    system
        .spawn_supervised(
            move || {
                let number = count.fetch_add(1, Ordering::SeqCst);
                if number > 0 {
                    gate.block();
                }
                Generation {
                    number,
                    panic_init: false,
                }
            },
            "/victim",
            strategy(),
        )
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_stop_cancels_blocked_factory_and_reserves_path() {
    let system = ThreadActorSystem::shared(Default::default());
    let gate = FactoryGate::new();
    let _release = ReleaseFactory(gate.clone());
    let actor = blocked_factory(&system, gate.clone()).await;
    generation(&actor, 0).await;
    actor.send_msg(Box::new("crash")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.started.notified())
        .await
        .unwrap();
    let duplicate = system
        .spawn_at(
            Generation {
                number: 99,
                panic_init: false,
            },
            "/victim",
            None,
            Default::default(),
        )
        .await;
    assert!(
        matches!(
            duplicate,
            Err(parrot::thread::error::SpawnError::ActorPathAlreadyExists(_))
        ),
        "restart must reserve the path"
    );
    // The only runtime thread must stay responsive while the factory blocks.
    tokio::time::timeout(Duration::from_millis(500), actor.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(system.get_actor_ref("/victim").is_none());
    let replacement = system
        .spawn_at(
            Generation {
                number: 99,
                panic_init: false,
            },
            "/victim",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), gate.finished.notified())
        .await
        .unwrap();
    for _ in 0..100 {
        generation(&replacement, 99).await;
    }
    assert!(
        !actor.is_alive().await,
        "stopped logical ref must not alias manual replacement"
    );
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_cancels_blocked_factory_without_late_publication() {
    let system = ThreadActorSystem::shared(Default::default());
    let gate = FactoryGate::new();
    let _release = ReleaseFactory(gate.clone());
    let actor = blocked_factory(&system, gate.clone()).await;
    generation(&actor, 0).await;
    actor.send_msg(Box::new("crash")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.started.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_millis(500), system.shutdown_internal())
        .await
        .unwrap()
        .unwrap();
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), gate.finished.notified())
        .await
        .unwrap();
    assert_eq!(system.actor_count(), 0);
    assert!(!actor.is_alive().await);
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_factory_times_out_without_poisoning_path() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shutdown_timeout: Duration::from_millis(80),
        ..Default::default()
    });
    let gate = FactoryGate::new();
    let _release = ReleaseFactory(gate.clone());
    let actor = blocked_factory(&system, gate.clone()).await;
    generation(&actor, 0).await;
    actor.send_msg(Box::new("crash")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.started.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_millis(500), removed(&system))
        .await
        .unwrap();
    let replacement = system
        .spawn_at(
            Generation {
                number: 99,
                panic_init: false,
            },
            "/victim",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    gate.release();
    tokio::time::timeout(Duration::from_secs(2), gate.finished.notified())
        .await
        .unwrap();
    for _ in 0..100 {
        generation(&replacement, 99).await;
    }
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failure_notification_does_not_require_context_lock() {
    let system = ThreadActorSystem::shared(Default::default());
    let actor = supervised(&system, Arc::new(AtomicUsize::new(0))).await;
    let (entered, started) = tokio::sync::oneshot::channel();
    let (_release, paused) = tokio::sync::oneshot::channel();
    actor
        .deliver(Box::new(PausedMessage {
            entered,
            release: paused,
            crash: false,
        }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), started)
        .await
        .unwrap()
        .unwrap();
    let mailbox = system.get_mailbox("/victim").unwrap();
    mailbox
        .get_processor()
        .unwrap()
        .notify_system_panic("failure while context is borrowed".into());
    generation(&actor, 1).await;
    system.shutdown_internal().await.unwrap();
}

struct BlockingDrop(Arc<FactoryGate>);
impl Drop for BlockingDrop {
    fn drop(&mut self) {
        self.0.block();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn panic_cleanup_is_bounded_and_failed_retirement_does_not_restart() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shutdown_timeout: Duration::from_millis(80),
        ..Default::default()
    });
    let count = Arc::new(AtomicUsize::new(0));
    let actor = supervised(&system, count.clone()).await;
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, paused) = tokio::sync::oneshot::channel();
    actor
        .deliver(Box::new(PausedMessage {
            entered,
            release: paused,
            crash: true,
        }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), started)
        .await
        .unwrap()
        .unwrap();
    let drop_gate = FactoryGate::new();
    let _release = ReleaseFactory(drop_gate.clone());
    actor
        .deliver(Box::new(BlockingDrop(drop_gate.clone())))
        .await
        .unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), drop_gate.started.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_millis(500), removed(&system))
        .await
        .unwrap();
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "unsafe to restart while retirement is incomplete"
    );
    drop_gate.release();
    tokio::time::timeout(Duration::from_secs(2), drop_gate.finished.notified())
        .await
        .unwrap();
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_replacement_scheduling_rolls_back_address_and_reservation() {
    use parrot::thread::config::SchedulingMode;
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_scheduling_mode: SchedulingMode::DedicatedThread,
        max_dedicated_threads: 1,
        ..Default::default()
    });
    let gate = FactoryGate::new();
    let _release = ReleaseFactory(gate.clone());
    let actor = blocked_factory(&system, gate.clone()).await;
    generation(&actor, 0).await;
    actor.deliver(Box::new("crash")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), gate.started.notified())
        .await
        .unwrap();
    let occupant = system
        .spawn_at(
            Generation {
                number: 99,
                panic_init: false,
            },
            "/occupant",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    gate.release();
    removed(&system).await;
    assert!(!actor.is_alive().await);
    generation(&occupant, 99).await;
    system.stop_actor("/occupant").await.unwrap();
    let fresh = system
        .spawn_at(
            Generation {
                number: 88,
                panic_init: false,
            },
            "/victim",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    generation(&fresh, 88).await;
    assert!(
        !actor.is_alive().await,
        "failed logical ref cannot alias a later manual spawn"
    );
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reference_stop_racing_factory_publication_stops_the_logical_actor() {
    let system = ThreadActorSystem::shared(Default::default());
    for _ in 0..64 {
        let gate = FactoryGate::new();
        let _release = ReleaseFactory(gate.clone());
        let actor = blocked_factory(&system, gate.clone()).await;
        generation(&actor, 0).await;
        actor.deliver(Box::new("crash")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), gate.started.notified())
            .await
            .unwrap();
        let stop = tokio::spawn(async move { actor.stop().await });
        gate.release();
        tokio::time::timeout(Duration::from_secs(2), stop)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(system.get_actor_ref("/victim").is_none());
    }
    system.shutdown_internal().await.unwrap();
}
