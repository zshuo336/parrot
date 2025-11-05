//! Akka (single-node) parity test suite.
//!
//! Every test mirrors a documented Akka concept and asserts parrot's
//! equivalent behavior. Sections follow the Akka documentation layout:
//!
//!  §1  Actor lifecycle      (preStart/postStop/restart hooks)
//!  §2  DeathWatch           (watch/unwatch/Terminated delivery)
//!  §3  Supervision          (Restart/Resume/Stop/Escalate, panic isolation)
//!  §4  Mailboxes            (bounded/unbounded, overflow policies)
//!  §5  Scheduling           (single-shot & periodic timers)
//!  §6  Dispatchers          (default pool vs PinnedDispatcher ≙ DedicatedThread)
//!  §7  Message flow         (tell/ask/forward/pipeTo/aggregate)
//!  §8  State & behavior     (become/unbecome, FSM, stash)
//!  §9  Addressing           (actorFor/ActorSelection by path)
//!  §10 Graceful shutdown    (poisonPill-equivalent, coordinated stop)
//!
//! Known gaps found during construction are documented in the test bodies
//! and summarized at the bottom of this file (see `GAP_MANIFEST`).

mod common;

use common::*;
use parrot::thread::config::{
    BackpressureStrategy, SchedulingMode, SupervisorStrategy, ThreadActorConfig,
};
use parrot::thread::context::ThreadContext;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRefExt;
use parrot_api::errors::ActorError;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use parrot_api_derive::Message;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ===========================================================================
// §1 Actor lifecycle — Akka: preStart → receive* → postStop
//    parrot: init → receive_message → before_stop (via stop)
// ===========================================================================

#[derive(Clone, Debug, Message)]
#[message(result = "String")]
struct LifecycleReport;

#[derive(Clone, Debug, Message)]
#[message(result = "()")]
struct Tick;

/// Records the exact ordering of lifecycle callback invocations, mirroring
/// Akka's documented preStart/receive/postStop ordering guarantees.
#[derive(Debug, Default)]
struct LifecycleProbe {
    events: Mutex<Vec<String>>,
}

impl LifecycleProbe {
    fn mark(&self, e: &str) {
        self.events.lock().unwrap().push(e.to_string());
    }
}

impl Actor for LifecycleProbe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        // Akka preStart: runs before the first message is processed.
        self.mark("init");
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            // First message processing happens strictly after init.
            self.mark("message");
            let _ = msg;
            Ok(Box::new("alive".to_string()) as BoxedMessage)
        })
    }

    fn before_stop<'a>(
        &'a mut self,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        // Akka postStop: runs after the last message, before removal.
        self.mark("before_stop");
        Box::pin(async { Ok(()) })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

#[tokio::test]
async fn akka_parity_lifecycle_ordering() {
    let ts = mk_system();
    let probe = LifecycleProbe::default();
    let events_handle = probe.events.lock().unwrap().clone();
    let _ = events_handle;

    // Spawn with a shared probe we can read after the actor is consumed by
    // the system — use an external Arc instead.
    let shared = Arc::new(Mutex::new(Vec::<String>::new()));
    struct Probe(Arc<Mutex<Vec<String>>>);
    impl Actor for Probe {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            self.0.lock().unwrap().push("init".into());
            Box::pin(async { Ok(()) })
        }
        fn receive_message<'a>(
            &'a mut self,
            _m: BoxedMessage,
            _c: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.0.lock().unwrap().push("message".into());
            Box::pin(async { Ok(Box::new("alive".to_string()) as BoxedMessage) })
        }
        fn before_stop<'a>(
            &'a mut self,
            _c: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            self.0.lock().unwrap().push("before_stop".into());
            Box::pin(async { Ok(()) })
        }
        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    let aref = ts
        .spawn_at::<Probe>(
            Probe(shared.clone()),
            "/akka/lifecycle",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .expect("spawn");

    // Akka preStart ordering: init strictly precedes the first message.
    // (spawn returns before the async init task runs; the guarantee is
    // established by the engine's init-then-drain sequencing, observed
    // here once the first message completes.)
    let _: String = ask(&aref, LifecycleReport).await.unwrap();
    let ev = shared.lock().unwrap().clone();
    assert_eq!(
        ev.first().map(String::as_str),
        Some("init"),
        "init must precede the first message: {:?}",
        ev
    );
    assert!(ev.iter().filter(|e| *e == "message").count() >= 1);

    // Stop drives before_stop (Akka postStop), then removal.
    ts.stop_actor("/akka/lifecycle").await.expect("stop");
    let ev = shared.lock().unwrap().clone();
    assert_eq!(
        ev,
        vec!["init", "message", "before_stop"],
        "Akka lifecycle ordering: preStart → receive → postStop"
    );

    // After stop, the path is gone from the registry (actorFor → dead).
    assert!(ts.get_actor_ref("/akka/lifecycle").is_none());
}

// ===========================================================================
// §2 DeathWatch — Akka: context.watch(target); on Terminated { ... }
//    parrot: system.watch(watcher, watched); Terminated delivered to watcher
// ===========================================================================

/// Legacy path watches deliver the public Terminated payload. Arbitrary
/// application messages must never count as termination evidence.
#[derive(Debug, Default)]
struct DeathWatcher {
    notifications: Vec<parrot::thread::system::Terminated>,
}

#[derive(Clone, Debug, Message)]
#[message(result = "Vec<parrot::thread::system::Terminated>")]
struct WatcherQuery;

#[derive(Clone, Debug, Message)]
#[message(result = "String")]
struct Ping;

impl Actor for DeathWatcher {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<WatcherQuery>() {
                return Ok(Box::new(self.notifications.clone()) as BoxedMessage);
            }
            if let Some(event) = msg.downcast_ref::<parrot::thread::system::Terminated>() {
                self.notifications.push(event.clone());
                return Ok(Box::new(()) as BoxedMessage);
            }
            if msg.is::<Ping>() {
                return Ok(Box::new("pong".to_string()) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError(
                "unexpected watcher payload".into(),
            ))
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

fn assert_normal_terminations(
    events: &[parrot::thread::system::Terminated],
    expected_paths: &[&str],
) {
    assert_eq!(
        events
            .iter()
            .map(|event| event.path.as_str())
            .collect::<Vec<_>>(),
        expected_paths,
        "DeathWatch must deliver exactly the expected public event paths"
    );
    assert!(
        events
            .iter()
            .all(|event| matches!(event.reason, parrot_api::supervisor::DeathReason::Normal)),
        "successful stop must carry DeathReason::Normal: {events:?}"
    );
}

#[tokio::test]
async fn akka_parity_deathwatch_watch_and_terminate() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let ts = mk_system();
        let watcher = ts
            .spawn_at::<DeathWatcher>(
                DeathWatcher::default(),
                "/akka/watcher",
                None,
                // The invalid application message below must not retire this
                // observer before the DeathWatch contract runs.
                ThreadActorConfig {
                    supervisor_strategy: Some(SupervisorStrategy::Resume),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        for path in [
            "/akka/watched",
            "/akka/cancelled",
            "/akka/rewatched",
            "/akka/stranger",
        ] {
            let target = ts
                .spawn_at::<SimpleWatched>(
                    SimpleWatched::default(),
                    path,
                    None,
                    ThreadActorConfig::default(),
                )
                .await
                .unwrap();
            assert_eq!(ask(&target, common::Ping).await.unwrap(), "pong");
        }

        assert_eq!(ask(&watcher, Ping).await.unwrap(), "pong");
        let unknown = ask(&watcher, Tick).await;
        assert!(
            matches!(unknown, Err(ActorError::MessageHandlingError(ref message))
            if message == "unexpected watcher payload"),
            "unknown payload must not masquerade as a termination: {unknown:?}"
        );
        assert_normal_terminations(&ask(&watcher, WatcherQuery).await.unwrap(), &[]);

        for path in ["/akka/watched", "/akka/cancelled", "/akka/rewatched"] {
            for _ in 0..2 {
                ts.watch("/akka/watcher".into(), path.into()).await.unwrap();
            }
        }
        // Cancel while the target is alive, then re-register the same pair.
        // Cancellation is idempotent and must not poison future registration.
        for _ in 0..2 {
            ts.unwatch("/akka/watcher".into(), "/akka/rewatched".into())
                .await
                .unwrap();
        }
        for _ in 0..2 {
            ts.watch("/akka/watcher".into(), "/akka/rewatched".into())
                .await
                .expect("rewatch after unwatch must succeed");
        }
        for _ in 0..2 {
            ts.unwatch("/akka/watcher".into(), "/akka/cancelled".into())
                .await
                .unwrap();
        }

        // Successful stop joins notification admission. A subsequent query
        // drains earlier notifications in the watcher's default mailbox.
        // No sleep or probabilistic quiet window establishes cancellation.
        ts.stop_actor("/akka/cancelled").await.unwrap();
        let cancelled = ask(&watcher, WatcherQuery).await.unwrap();
        assert!(
            cancelled.is_empty(),
            "cancelled live subscription must not notify: {cancelled:?}"
        );
        ts.stop_actor("/akka/stranger").await.unwrap();
        assert_normal_terminations(&ask(&watcher, WatcherQuery).await.unwrap(), &[]);

        // Removing another edge must preserve this live subscription.
        ts.stop_actor("/akka/watched").await.unwrap();
        assert_normal_terminations(
            &ask(&watcher, WatcherQuery).await.unwrap(),
            &["/akka/watched"],
        );
        ts.stop_actor("/akka/rewatched").await.unwrap();
        assert_normal_terminations(
            &ask(&watcher, WatcherQuery).await.unwrap(),
            &["/akka/watched", "/akka/rewatched"],
        );
        ts.shutdown_internal()
            .await
            .expect("DeathWatch shutdown must succeed");
        assert_eq!(ts.actor_count(), 0);
    })
    .await
    .expect("DeathWatch contract exceeded its independent liveness watchdog");
}

// ===========================================================================
// §3 Supervision — Akka: one-for-one { Restart | Resume | Stop | Escalate }
//    parrot: live-instance Resume, factory-backed Restart with an exact budget,
//    terminal Stop, and escalation through an actual managed parent.
// ===========================================================================

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct SupervisedCount;

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct SupervisedBoom(u64);

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct SupervisedError;

#[derive(Clone, Debug, Message)]
#[message(result = "(usize, u64, u64)")]
struct SupervisedState;

#[derive(Debug, Default)]
struct SupervisionLifetime {
    created: std::sync::atomic::AtomicUsize,
    dropped: std::sync::atomic::AtomicUsize,
}

#[derive(Debug, Default)]
struct PanickyWorker {
    count: u64,
    booms: u64,
    generation: usize,
    lifetime: Arc<SupervisionLifetime>,
}

impl PanickyWorker {
    fn observed(lifetime: Arc<SupervisionLifetime>) -> Self {
        let generation = lifetime
            .created
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self {
            lifetime,
            generation,
            ..Default::default()
        }
    }
}

impl Drop for PanickyWorker {
    fn drop(&mut self) {
        self.lifetime
            .dropped
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Actor for PanickyWorker {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<SupervisedState>() {
                return Ok(Box::new((self.generation, self.count, self.booms)) as BoxedMessage);
            }
            if msg.is::<SupervisedError>() {
                self.booms += 1;
                return Err(ActorError::MessageHandlingError(
                    "supervised business failure".into(),
                ));
            }
            if let Some(b) = msg.downcast_ref::<SupervisedBoom>() {
                self.booms += 1;
                if b.0 != 0 {
                    panic!("supervised failure #{}", b.0);
                }
                return Ok(Box::new(self.booms) as BoxedMessage);
            }
            if msg.downcast_ref::<SupervisedCount>().is_some() {
                self.count += 1;
                return Ok(Box::new(self.count) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError("unknown".into()))
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

async fn supervised_failure(actor: &dyn parrot_api::address::ActorRef, panic: bool) {
    let message: BoxedMessage = if panic {
        Box::new(SupervisedBoom(1))
    } else {
        Box::new(SupervisedError)
    };
    let error = actor
        .send_with_timeout(message, Some(Duration::from_secs(3)))
        .await
        .expect_err("the actual failed handler must report failure");
    if panic {
        assert!(
            matches!(error, ActorError::Panic(ref message)
            if message.contains("supervised failure #1")),
            "actual panic cause was lost: {error:?}"
        );
    } else {
        assert!(
            matches!(error, ActorError::MessageHandlingError(ref message)
            if message == "supervised business failure"),
            "actual business cause was lost: {error:?}"
        );
    }
}

async fn supervised_removed(system: &parrot::thread::system::ThreadActorSystem, path: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while system.get_actor_ref(path).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed actor did not retire from the registry");
}

async fn supervised_generation(
    system: &parrot::thread::system::ThreadActorSystem,
    actor: &parrot::thread::address::ThreadActorRef<PanickyWorker>,
    path: &str,
    expected: usize,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            assert!(
                system.get_actor_ref(path).is_some(),
                "restart retired the logical actor instead of reconstructing"
            );
            if let Ok(reply) = actor
                .ask_with_timeout(Box::new(SupervisedState), Duration::from_millis(100))
                .await
            {
                let state = *reply.downcast::<(usize, u64, u64)>().unwrap();
                assert!(
                    state.0 <= expected,
                    "restart produced an unexpected extra generation: {state:?}"
                );
                if state.0 == expected {
                    assert_eq!(
                        state,
                        (expected, 0, 0),
                        "restart must construct fresh business state"
                    );
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected restarted generation did not become responsive");
}

#[tokio::test]
async fn akka_parity_supervision_panic_isolation() {
    use std::sync::atomic::Ordering;
    tokio::time::timeout(Duration::from_secs(15), async {
        let ts = mk_system();
        let lifetime = Arc::new(SupervisionLifetime::default());
        let victim = ts
            .spawn_at(
                PanickyWorker::observed(lifetime.clone()),
                "/akka/panic-victim",
                None,
                ThreadActorConfig {
                    supervisor_strategy: Some(SupervisorStrategy::Stop),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let neighbor = ts
            .spawn_at(
                PanickyWorker::default(),
                "/akka/panic-neighbor",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        assert_eq!(ask(&victim, SupervisedCount).await.unwrap(), 1);
        assert_eq!(ask(&neighbor, SupervisedCount).await.unwrap(), 1);
        // The failed ASK is a processing barrier, rather than a guessed sleep.
        supervised_failure(&victim, true).await;
        assert_eq!(ask(&neighbor, SupervisedCount).await.unwrap(), 2);
        supervised_removed(&ts, "/akka/panic-victim").await;
        assert_eq!(lifetime.created.load(Ordering::SeqCst), 1);
        assert_eq!(
            lifetime.dropped.load(Ordering::SeqCst),
            1,
            "retirement must release actor state even while its reference is retained"
        );
        assert!(matches!(
            ask(&victim, SupervisedCount).await,
            Err(ActorError::Stopped)
        ));
        assert!(matches!(
            victim.tell(SupervisedBoom(0)).await,
            Err(ActorError::Stopped)
        ));
        assert_eq!(ask(&neighbor, SupervisedCount).await.unwrap(), 3);
        ts.shutdown_internal().await.unwrap();
    })
    .await
    .expect("panic isolation contract exceeded its watchdog");
}

#[tokio::test]
async fn akka_parity_supervision_directives() {
    use std::sync::atomic::Ordering;
    tokio::time::timeout(Duration::from_secs(25), async {
        assert!(matches!(SupervisorStrategy::default(),
            SupervisorStrategy::Restart { max_retries: 3, within: d } if d == Duration::from_secs(10)));
        for panic in [false, true] {
            for resume in [false, true] {
                let ts = mk_system();
                let lifetime = Arc::new(SupervisionLifetime::default());
                let path = "/akka/strategy-worker";
                let actor = ts.spawn_at(PanickyWorker::observed(lifetime.clone()), path, None,
                    ThreadActorConfig { supervisor_strategy: Some(if resume { SupervisorStrategy::Resume } else { SupervisorStrategy::Stop }),
                        ..Default::default() }).await.unwrap();
                assert_eq!(ask(&actor, SupervisedCount).await.unwrap(), 1);
                supervised_failure(&actor, panic).await;
                if resume {
                    assert_eq!(ask(&actor, SupervisedState).await.unwrap(), (0, 1, 1),
                        "Resume must preserve effects before failure without replaying the message");
                    assert_eq!(ask(&actor, SupervisedCount).await.unwrap(), 2);
                    assert_eq!(lifetime.dropped.load(Ordering::SeqCst), 0);
                } else {
                    let rejected = actor.ask_with_timeout(Box::new(SupervisedState), Duration::from_secs(3)).await;
                    // A request admitted before retirement can lose its reply during draining.
                    assert!(matches!(rejected, Err(ActorError::Stopped | ActorError::ReplyChannelError(_))),
                        "Stop must reject subsequent business messages: {rejected:?}");
                    supervised_removed(&ts, path).await;
                    assert_eq!(lifetime.dropped.load(Ordering::SeqCst), 1);
                    assert!(matches!(ask(&actor, SupervisedCount).await, Err(ActorError::Stopped)));
                }
                assert_eq!(lifetime.created.load(Ordering::SeqCst), 1);
                ts.shutdown_internal().await.unwrap();
                assert_eq!(lifetime.dropped.load(Ordering::SeqCst), 1);
            }
            let ts = mk_system();
            let lifetime = Arc::new(SupervisionLifetime::default());
            let observed = lifetime.clone();
            let path = "/akka/restart-worker";
            let actor = ts.spawn_supervised(move || PanickyWorker::observed(observed.clone()), path,
                SupervisorStrategy::Restart { max_retries: 2, within: Duration::from_secs(60) }).await.unwrap();
            for generation in 0..3 {
                supervised_generation(&ts, &actor, path, generation).await;
                assert_eq!(ask(&actor, SupervisedCount).await.unwrap(), 1);
                supervised_failure(&actor, panic).await;
            }
            tokio::time::timeout(Duration::from_secs(3), async {
                while ts.get_actor_ref(path).is_some() {
                    if let Ok(reply) = actor.ask_with_timeout(Box::new(SupervisedState), Duration::from_millis(100)).await {
                        let state = *reply.downcast::<(usize, u64, u64)>().unwrap();
                        assert!(state.0 <= 2, "restart exceeded the configured two-retry budget: {state:?}");
                    }
                    tokio::task::yield_now().await;
                }
            }).await.expect("exhausted restart budget did not retire the logical actor");
            assert_eq!(lifetime.created.load(Ordering::SeqCst), 3);
            assert_eq!(lifetime.dropped.load(Ordering::SeqCst), 3);
            assert!(matches!(ask(&actor, SupervisedCount).await, Err(ActorError::Stopped)));
            ts.shutdown_internal().await.unwrap();
        }
        supervised_escalation().await;
    }).await.expect("supervision directives contract exceeded its watchdog");
}

struct EscalationParent {
    lifetime: Arc<SupervisionLifetime>,
    child: Arc<SupervisionLifetime>,
    sibling: Arc<SupervisionLifetime>,
    decisions: Arc<std::sync::atomic::AtomicUsize>,
}
impl Drop for EscalationParent {
    fn drop(&mut self) {
        self.lifetime
            .dropped
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
impl Actor for EscalationParent {
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
        use parrot_api::context::{ActorContext, ActorSpawnerExt};
        use parrot_api::supervisor::{
            BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType,
        };
        Box::pin(async move {
            if message.is::<SupervisedState>() {
                return Ok(Box::new(()) as BoxedMessage);
            }
            let decisions = self.decisions.clone();
            let child = ctx.spawner().spawn_supervised(PanickyWorker::observed(self.child.clone()), EmptyConfig,
                SupervisorStrategyType::OneForOne(OneForOneStrategy {
                    max_restarts: 0, within: Duration::from_secs(60),
                    decider: BasicDecisionFn::new(move |error| {
                        assert!(matches!(error, ActorError::Panic(message) if message.contains("supervised failure #1")));
                        decisions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        SupervisionDecision::Escalate
                    }),
                })).await?;
            let sibling = ctx
                .spawner()
                .spawn_typed(PanickyWorker::observed(self.sibling.clone()), EmptyConfig)
                .await?;
            Ok(Box::new((child, sibling)) as BoxedMessage)
        })
    }
}

async fn supervised_escalation() {
    use std::sync::atomic::Ordering;
    let ts = mk_system();
    let parent_lifetime = Arc::new(SupervisionLifetime::default());
    let child_lifetime = Arc::new(SupervisionLifetime::default());
    let sibling_lifetime = Arc::new(SupervisionLifetime::default());
    let decisions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let parent = ts
        .spawn_at(
            EscalationParent {
                lifetime: parent_lifetime.clone(),
                child: child_lifetime.clone(),
                sibling: sibling_lifetime.clone(),
                decisions: decisions.clone(),
            },
            "/akka/escalation-parent",
            None,
            ThreadActorConfig {
                supervisor_strategy: Some(SupervisorStrategy::Stop),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let reply = parent
        .ask_with_timeout(Box::new(()), Duration::from_secs(3))
        .await
        .unwrap();
    let (child, sibling) = *reply
        .downcast::<(
            parrot_api::types::BoxedActorRef,
            parrot_api::types::BoxedActorRef,
        )>()
        .unwrap();
    let outsider = ts
        .spawn_at(
            PanickyWorker::default(),
            "/akka/independent",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(ask(&outsider, SupervisedCount).await.unwrap(), 1);
    supervised_failure(&*child, true).await;
    supervised_removed(&ts, &child.path()).await;
    assert!(
        matches!(
            parent
                .ask_with_timeout(Box::new(SupervisedState), Duration::from_secs(3))
                .await,
            Err(ActorError::Stopped | ActorError::ReplyChannelError(_))
        ),
        "a managed child escalation must stop its configured parent"
    );
    // Only a managed parent relationship can retire this parent and its other child.
    for path in [
        child.path(),
        sibling.path(),
        "/akka/escalation-parent".into(),
    ] {
        supervised_removed(&ts, &path).await;
    }
    assert_eq!(parent_lifetime.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(child_lifetime.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_lifetime.dropped.load(Ordering::SeqCst), 1);
    assert_eq!(child_lifetime.created.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_lifetime.created.load(Ordering::SeqCst), 1);
    assert_eq!(
        decisions.load(Ordering::SeqCst),
        1,
        "escalation must decide the original child failure exactly once"
    );
    assert!(matches!(
        parent
            .ask_with_timeout(Box::new(()), Duration::from_secs(3))
            .await,
        Err(ActorError::Stopped)
    ));
    for actor in [&child, &sibling] {
        assert!(matches!(
            actor.send(Box::new(SupervisedCount)).await,
            Err(ActorError::Stopped)
        ));
    }
    assert_eq!(ask(&outsider, SupervisedCount).await.unwrap(), 2);
    ts.shutdown_internal().await.unwrap();
}

// ===========================================================================
// §4 Mailboxes — Akka: bounded mailbox with Push/Drop/Failed policies;
//    parrot: BackpressureStrategy { Block, Error, DropNewest, DropOldest }
// ===========================================================================

struct MailboxGate {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

struct MailboxRecorder {
    observed: tokio::sync::mpsc::UnboundedSender<u64>,
    values: Vec<u64>,
}

impl Actor for MailboxRecorder {
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
            if msg.is::<MailboxGate>() {
                let gate = *msg.downcast::<MailboxGate>().unwrap();
                gate.entered.send(()).unwrap();
                gate.release.await.unwrap();
                Ok(Box::new(()) as BoxedMessage)
            } else if let Some(value) = msg.downcast_ref::<u64>() {
                self.values.push(*value);
                self.observed.send(*value).unwrap();
                Ok(Box::new(*value) as BoxedMessage)
            } else {
                assert!(msg.is::<()>(), "unexpected mailbox probe message");
                Ok(Box::new(self.values.clone()) as BoxedMessage)
            }
        })
    }
}

fn mailbox_rejection(result: ActorResult<BoxedMessage>, strategy: &BackpressureStrategy) {
    let expected = match strategy {
        BackpressureStrategy::Error => "Full",
        BackpressureStrategy::DropNewest => "Dropped",
        _ => panic!("only rejecting policies have admission errors"),
    };
    assert!(
        matches!(result, Err(ActorError::InternalError(ref message)) if message.contains(expected)),
        "full mailbox must report {expected}, not success or a reply timeout: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn akka_parity_mailbox_overflow_policies() {
    use parrot_api::address::ActorRef;

    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 2,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "akka-mailbox".into(),
        },
    ] {
        let ts = mk_system();
        for strategy in [
            BackpressureStrategy::Error,
            BackpressureStrategy::DropNewest,
            BackpressureStrategy::DropOldest,
            BackpressureStrategy::Block,
        ] {
            for use_ask in [false, true] {
                let (observed, mut received) = tokio::sync::mpsc::unbounded_channel();
                let actor = ts
                    .spawn_at(
                        MailboxRecorder {
                            observed,
                            values: Vec::new(),
                        },
                        "/akka/bounded",
                        None,
                        ThreadActorConfig {
                            scheduling_mode: Some(mode.clone()),
                            mailbox_capacity: Some(2),
                            backpressure_strategy: Some(strategy.clone()),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                let (entered, started) = tokio::sync::oneshot::channel();
                let (release, gate) = tokio::sync::oneshot::channel();
                actor
                    .deliver(Box::new(MailboxGate {
                        entered,
                        release: gate,
                    }))
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(5), started)
                    .await
                    .unwrap()
                    .unwrap();

                // The consumer is inside the gate: polling these asks admits
                // exactly two envelopes and cannot process their payloads yet.
                let expected = if use_ask {
                    let first = actor.send(Box::new(10_u64));
                    let second = actor.send(Box::new(20_u64));
                    tokio::pin!(first, second);
                    assert!(futures::poll!(&mut first).is_pending());
                    assert!(futures::poll!(&mut second).is_pending());
                    let third = actor.send(Box::new(30_u64));
                    tokio::pin!(third);
                    let third_poll = futures::poll!(&mut third);
                    if matches!(
                        strategy,
                        BackpressureStrategy::Error | BackpressureStrategy::DropNewest
                    ) {
                        let std::task::Poll::Ready(result) = third_poll else {
                            panic!("full ASK must reject immediately");
                        };
                        mailbox_rejection(result, &strategy);
                    } else {
                        assert!(
                            third_poll.is_pending(),
                            "admitted ASK must await its real reply"
                        );
                    }
                    if strategy == BackpressureStrategy::DropOldest {
                        assert!(
                            matches!(
                                futures::poll!(&mut first),
                                std::task::Poll::Ready(Err(ActorError::ReplyChannelError(_)))
                            ),
                            "evicted ASK must close its original reply channel immediately"
                        );
                    } else {
                        assert!(
                            futures::poll!(&mut first).is_pending(),
                            "oldest ASK must survive"
                        );
                    }
                    assert!(
                        futures::poll!(&mut second).is_pending(),
                        "second ASK must survive"
                    );
                    release.send(()).unwrap();
                    if strategy != BackpressureStrategy::DropOldest {
                        let reply = tokio::time::timeout(Duration::from_secs(5), &mut first)
                            .await
                            .unwrap()
                            .unwrap();
                        assert_eq!(*reply.downcast::<u64>().unwrap(), 10);
                    }
                    let reply = tokio::time::timeout(Duration::from_secs(5), &mut second)
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(*reply.downcast::<u64>().unwrap(), 20);
                    if matches!(
                        strategy,
                        BackpressureStrategy::DropOldest | BackpressureStrategy::Block
                    ) {
                        let reply = tokio::time::timeout(Duration::from_secs(5), &mut third)
                            .await
                            .unwrap()
                            .unwrap();
                        assert_eq!(*reply.downcast::<u64>().unwrap(), 30);
                    }
                    match strategy {
                        BackpressureStrategy::DropOldest => vec![20, 30],
                        BackpressureStrategy::Block => vec![10, 20, 30],
                        _ => vec![10, 20],
                    }
                } else {
                    actor.deliver(Box::new(10_u64)).await.unwrap();
                    actor.deliver(Box::new(20_u64)).await.unwrap();
                    let third = actor.deliver(Box::new(30_u64));
                    tokio::pin!(third);
                    match strategy {
                        BackpressureStrategy::Block => {
                            assert!(
                                futures::poll!(&mut third).is_pending(),
                                "full TELL must block admission"
                            );
                            release.send(()).unwrap();
                            tokio::time::timeout(Duration::from_secs(5), third)
                                .await
                                .unwrap()
                                .unwrap();
                            vec![10, 20, 30]
                        }
                        BackpressureStrategy::DropOldest => {
                            let std::task::Poll::Ready(result) = futures::poll!(&mut third) else {
                                panic!("eviction must admit immediately");
                            };
                            result.unwrap();
                            release.send(()).unwrap();
                            vec![20, 30]
                        }
                        _ => {
                            let std::task::Poll::Ready(result) = futures::poll!(&mut third) else {
                                panic!("full TELL must reject immediately");
                            };
                            mailbox_rejection(
                                result.map(|()| Box::new(()) as BoxedMessage),
                                &strategy,
                            );
                            release.send(()).unwrap();
                            vec![10, 20]
                        }
                    }
                };
                for value in &expected {
                    let actual = tokio::time::timeout(Duration::from_secs(5), received.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(actual, *value, "{mode:?} / {strategy:?} / ASK={use_ask}");
                }
                // Observe the drain before querying: a query admitted into a
                // full DropOldest mailbox would itself evict a valid payload.
                let snapshot = actor
                    .send_with_timeout(Box::new(()), Some(Duration::from_secs(5)))
                    .await
                    .unwrap();
                assert_eq!(*snapshot.downcast::<Vec<u64>>().unwrap(), expected);
                actor.stop().await.unwrap();
            }
        }
        ts.shutdown_internal().await.unwrap();
    }
}

// ===========================================================================
// §5 Scheduling — real Parrot one-shot and fixed-delay context timers
//    parrot: ThreadContext::schedule_once / schedule_periodic
// ===========================================================================

#[tokio::test]
async fn akka_parity_scheduler_once_and_periodic() {
    use parrot_api::address::ActorRef;
    use parrot_api::context::ActorContext;
    use parrot_api::message::CloneableMessage;
    use parrot_api::types::BoxedActorRef;
    use std::time::Instant;
    #[derive(Clone, Debug)]
    enum TimerTick {
        Once,
        Periodic,
    }
    struct ScheduleTimers(BoxedActorRef);
    struct TimerOwner;
    impl Actor for TimerOwner {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                let request = msg
                    .downcast::<ScheduleTimers>()
                    .expect("expected schedule request");
                let once_start = Instant::now();
                ctx.schedule_once(
                    request.0.clone_boxed(),
                    Box::new(TimerTick::Once),
                    Duration::from_millis(30),
                )
                .await
                .expect("real schedule_once registration must succeed");
                let periodic_start = Instant::now();
                ctx.schedule_periodic(
                    request.0,
                    CloneableMessage::from_cloneable(TimerTick::Periodic),
                    Duration::from_millis(50),
                    Duration::from_millis(25),
                )
                .await
                .expect("real schedule_periodic registration must succeed");
                Ok(Box::new((once_start, periodic_start)) as BoxedMessage)
            })
        }
        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }
    struct TimerSink(tokio::sync::mpsc::UnboundedSender<(TimerTick, Instant)>);
    impl Actor for TimerSink {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if msg.is::<()>() {
                    return Ok(Box::new(()) as BoxedMessage);
                }
                let tick = *msg
                    .downcast::<TimerTick>()
                    .expect("expected real timer payload");
                self.0
                    .send((tick, Instant::now()))
                    .expect("timer observer must remain alive");
                Ok(Box::new(()) as BoxedMessage)
            })
        }
        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        let ts = mk_system();
        let (events, mut incoming) = tokio::sync::mpsc::unbounded_channel();
        let sink = ts
            .spawn_at::<TimerSink>(
                TimerSink(events),
                "/akka/timer-sink",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        let owner = ts
            .spawn_at::<TimerOwner>(
                TimerOwner,
                "/akka/timer",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        let reply = owner
            .send(Box::new(ScheduleTimers(Box::new(sink.clone()))))
            .await
            .expect("real context timer registration must return its receipt");
        let (once_start, periodic_start) = *reply.downcast::<(Instant, Instant)>().unwrap();
        let mut once = Vec::new();
        let mut periodic = Vec::new();
        while once.is_empty() || periodic.len() < 3 {
            let (kind, timestamp) = incoming
                .recv()
                .await
                .expect("timer channel unexpectedly closed");
            match kind {
                TimerTick::Once => once.push(timestamp),
                TimerTick::Periodic => periodic.push(timestamp),
            }
        }
        // Successful owner stop joins its timer tasks. A FIFO query to the
        // surviving sink drains every delivery admitted before that retirement.
        owner
            .stop()
            .await
            .expect("timer owner retirement must succeed");
        sink.send(Box::new(()))
            .await
            .expect("timer sink drain barrier must succeed");
        while let Ok((kind, timestamp)) = incoming.try_recv() {
            match kind {
                TimerTick::Once => once.push(timestamp),
                TimerTick::Periodic => periodic.push(timestamp),
            }
        }
        assert_eq!(once.len(), 1, "schedule_once must deliver exactly once");
        assert!(
            once[0].duration_since(once_start) >= Duration::from_millis(30),
            "schedule_once must honor its delay"
        );
        assert!(
            periodic[0].duration_since(periodic_start) >= Duration::from_millis(50),
            "schedule_periodic must honor its initial delay"
        );
        assert!(
            periodic.len() >= 3,
            "real periodic timer must produce repeated deliveries"
        );
        // Handler execution can lag behind queue admission. Adjacent handler
        // timestamps therefore cannot prove the fixed-delay admission interval.
        assert_eq!(
            ts.actor_count(),
            1,
            "owner stop must leave the independent timer sink alive"
        );
        ts.shutdown_internal()
            .await
            .expect("timer system shutdown must succeed");
    })
    .await
    .expect("timer contract exceeded its independent liveness watchdog");
}

// ===========================================================================
// §6 Dispatchers — Akka: default dispatcher vs PinnedDispatcher.
//    parrot: SharedPool vs DedicatedThread.
// ===========================================================================

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct Tid;

#[derive(Debug, Default)]
struct ThreadReporter {
    threads: Vec<(std::thread::ThreadId, Option<String>)>,
}

impl Actor for ThreadReporter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.downcast_ref::<Tid>().is_some() {
                let thread = std::thread::current();
                self.threads
                    .push((thread.id(), thread.name().map(str::to_owned)));
                return Ok(Box::new(self.threads.len() as u64) as BoxedMessage);
            }
            if let Some(r) = msg.downcast_ref::<ThreadIdsQuery>() {
                let _ = r;
                return Ok(Box::new(self.threads.clone()) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError("unknown".into()))
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

#[derive(Clone, Debug, Message)]
#[message(result = "Vec<(std::thread::ThreadId, Option<String>)>")]
struct ThreadIdsQuery;

#[tokio::test]
async fn akka_parity_pinned_dispatcher_dedicated_thread() {
    use parrot::thread::config::ThreadActorSystemConfig;
    use parrot::thread::system::ThreadActorSystem;
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        max_dedicated_threads: 2,
        ..Default::default()
    });
    let pinned = ts
        .spawn_at::<ThreadReporter>(
            ThreadReporter::default(),
            "/akka/pinned",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(SchedulingMode::DedicatedThread),
                ..Default::default()
            },
        )
        .await
        .expect("spawn dedicated");
    let shared = ts
        .spawn_at::<ThreadReporter>(
            ThreadReporter::default(),
            "/akka/shared",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .expect("spawn shared");
    for count in 1..=5u64 {
        assert_eq!(ask(&pinned, Tid).await.unwrap(), count);
        assert_eq!(ask(&shared, Tid).await.unwrap(), count);
        tokio::task::yield_now().await;
    }
    let pinned_ids = ask(&pinned, ThreadIdsQuery).await.unwrap();
    let shared_ids = ask(&shared, ThreadIdsQuery).await.unwrap();
    assert_eq!(pinned_ids.len(), 5);
    assert_eq!(shared_ids.len(), 5);
    assert!(
        pinned_ids.iter().all(|(id, _)| *id == pinned_ids[0].0),
        "pinned actor must remain on one OS thread"
    );
    assert!(
        pinned_ids
            .iter()
            .all(|(_, name)| name.as_deref() == Some("parrot-dedicated-/akka/pinned")),
        "dedicated actor must run on its owned named OS thread"
    );
    // Shared workers bound concurrent polling, not physical OS-thread identity.
    // Their blocking executor may choose a different OS thread on each poll.
    assert!(
        shared_ids.iter().all(|(id, _)| *id != pinned_ids[0].0),
        "dedicated actor must not run on the shared worker"
    );
    ts.shutdown_internal()
        .await
        .expect("dispatcher shutdown must succeed");
}

// ===========================================================================
// §7 Message flow — Akka: tell (!), ask (?), forward, pipeTo, aggregate.
// ===========================================================================

#[tokio::test]
async fn akka_parity_ask_pattern_with_timeout() {
    use parrot_api::address::ActorRef;
    struct Gate {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    struct ReleaseOnDrop(Arc<Gate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release.notify_one();
        }
    }
    struct GatedEcho(Arc<Gate>);
    impl Actor for GatedEcho {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if msg.is::<SlowEchoU64>() {
                    self.0.entered.notify_one();
                    self.0.release.notified().await;
                    return Ok(Box::new(99u64) as BoxedMessage);
                }
                let echo = msg.downcast::<EchoU64>().expect("expected echo message");
                Ok(Box::new(echo.0) as BoxedMessage)
            })
        }
        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        let ts = mk_system();
        let gate = Arc::new(Gate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let release = ReleaseOnDrop(gate.clone());
        let actor = ts
            .spawn_at::<GatedEcho>(
                GatedEcho(gate.clone()),
                "/akka/slow",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        let reply = actor
            .send_with_timeout(Box::new(EchoU64(17)), Some(Duration::from_secs(5)))
            .await
            .expect("bounded ASK must return the actual successful reply");
        assert_eq!(*reply.downcast::<u64>().unwrap(), 17);
        let held_actor = actor.clone();
        let held = tokio::spawn(async move {
            held_actor
                .send_with_timeout(Box::new(SlowEchoU64(0)), None)
                .await
        });
        gate.entered.notified().await;
        let duration = Duration::from_millis(30);
        let started = std::time::Instant::now();
        let result = actor
            .send_with_timeout(Box::new(EchoU64(23)), Some(duration))
            .await;
        assert!(
            matches!(result, Err(ActorError::TimeoutDetail(_))),
            "Parrot bounded ASK must report TimeoutDetail, got {result:?}"
        );
        assert!(
            started.elapsed() >= duration,
            "Parrot deadline must not expire before requested duration"
        );
        drop(release);
        let held = held
            .await
            .expect("held caller panicked")
            .expect("held request failed");
        assert_eq!(*held.downcast::<u64>().unwrap(), 99);
        let recovered = actor
            .send_with_timeout(Box::new(EchoU64(41)), None)
            .await
            .unwrap();
        assert_eq!(
            *recovered.downcast::<u64>().unwrap(),
            41,
            "ASK must recover after timed out caller and released handler"
        );
        ts.shutdown_internal()
            .await
            .expect("ASK shutdown must succeed");
    })
    .await
    .expect("ASK contract exceeded its independent liveness watchdog");
}

#[tokio::test]
async fn akka_parity_pipe_to_and_forward() {
    use parrot_api::address::ActorRef;

    // An application composition, not an engine-level original-sender API:
    // the computed payload crosses a relay mailbox and the relay awaits its sink.
    #[derive(Debug)]
    struct Relay(parrot_api::types::BoxedActorRef);
    impl Actor for Relay {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.0.send(msg)
        }
    }

    tokio::time::timeout(Duration::from_secs(10), async {
        let ts = mk_system();
        let target = ts
            .spawn_at(
                Counter::default(),
                "/akka/pipeto",
                None,
                ThreadActorConfig {
                    supervisor_strategy: Some(SupervisorStrategy::Resume),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let relay = ts
            .spawn_at(
                Relay(Box::new(target.clone())),
                "/akka/forward",
                None,
                ThreadActorConfig {
                    supervisor_strategy: Some(SupervisorStrategy::Resume),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let destination = relay.clone();
        let (resolve, computation) = tokio::sync::oneshot::channel::<u64>();
        let producer = tokio::spawn(async move {
            let value = computation.await.expect("computation owner disappeared");
            ask(&destination, IncN(value)).await
        });
        assert_eq!(ask(&target, GetTotal).await.unwrap(), 0);
        resolve.send(42).unwrap();
        assert_eq!(
            producer.await.unwrap().unwrap(),
            42,
            "forwarded reply must reach the producer"
        );
        assert_eq!(ask(&target, GetTotal).await.unwrap(), 42);
        assert_eq!(ask(&relay, IncN(7)).await.unwrap(), 49);
        assert_eq!(
            ask(&target, GetTotal).await.unwrap(),
            49,
            "each computed value is applied once"
        );

        let error = relay.send(Box::new(()) as BoxedMessage).await;
        assert!(
            matches!(error, Err(ActorError::MessageHandlingError(message)) if message == "unknown"),
            "a downstream business error must cross the relay unchanged"
        );
        assert_eq!(ask(&relay, GetTotal).await.unwrap(), 49);
        ts.stop_actor("/akka/pipeto").await.unwrap();
        let error = ask(&relay, IncN(1)).await;
        assert!(
            matches!(error, Err(ActorError::Stopped)),
            "a stopped destination must fail the forwarded ASK: {error:?}"
        );
        ts.shutdown_internal().await.unwrap();
    })
    .await
    .expect("pipe/forward exceeded its independent liveness watchdog");
}

// ===========================================================================
// §8 State & behavior — Akka: become/unbecome, FSM, stash.
// ===========================================================================

// Application-owned behavior stack and deferred business queue. The engine
// supplies mailbox serialization; it does not expose native become or stash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Behavior {
    Plain,
    Scaled(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BehaviorSnapshot {
    stack: Vec<Behavior>,
    deferred: Vec<i64>,
    suspended: bool,
    effects: Vec<(i64, i64)>,
    total: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BehaviorReply {
    Accepted,
    Buffered,
    Drained(usize),
    Snapshot(BehaviorSnapshot),
}

#[derive(Clone, Debug)]
enum BehaviorCommand {
    Become(Behavior),
    Unbecome,
    Suspend,
    Resume,
    Add(i64),
    Snapshot,
}

impl parrot_api::message::Message for BehaviorCommand {
    type Result = BehaviorReply;
}

#[derive(Debug)]
struct HotSwapActor {
    state: BehaviorSnapshot,
}

impl HotSwapActor {
    fn apply(&mut self, value: i64) {
        let result = match self.state.stack.last().unwrap() {
            Behavior::Plain => value,
            Behavior::Scaled(scale) => value * scale,
        };
        self.state.effects.push((value, result));
        self.state.total += result;
    }
}

impl Actor for HotSwapActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let command = msg.downcast_ref::<BehaviorCommand>().ok_or_else(|| {
                ActorError::MessageHandlingError("unknown behavior command".into())
            })?;
            let reply = match command {
                BehaviorCommand::Become(behavior) => {
                    self.state.stack.push(*behavior);
                    BehaviorReply::Accepted
                }
                BehaviorCommand::Unbecome => {
                    if self.state.stack.len() == 1 {
                        return Err(ActorError::MessageHandlingError(
                            "cannot remove base behavior".into(),
                        ));
                    }
                    self.state.stack.pop();
                    BehaviorReply::Accepted
                }
                BehaviorCommand::Suspend => {
                    self.state.suspended = true;
                    BehaviorReply::Accepted
                }
                BehaviorCommand::Resume => {
                    self.state.suspended = false;
                    let pending = std::mem::take(&mut self.state.deferred);
                    let count = pending.len();
                    for value in pending {
                        self.apply(value);
                    }
                    BehaviorReply::Drained(count)
                }
                BehaviorCommand::Add(value) if self.state.suspended => {
                    self.state.deferred.push(*value);
                    BehaviorReply::Buffered
                }
                BehaviorCommand::Add(value) => {
                    self.apply(*value);
                    BehaviorReply::Accepted
                }
                BehaviorCommand::Snapshot => BehaviorReply::Snapshot(self.state.clone()),
            };
            Ok(Box::new(reply) as BoxedMessage)
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

#[tokio::test]
async fn akka_parity_become_unbecome_stash() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let ts = mk_system();
        let mut expected = BehaviorSnapshot {
            stack: vec![Behavior::Plain],
            deferred: vec![],
            suspended: false,
            effects: vec![],
            total: 0,
        };
        let actor = ts
            .spawn_at::<HotSwapActor>(
                HotSwapActor {
                    state: expected.clone(),
                },
                "/akka/hotswap",
                None,
                ThreadActorConfig {
                    supervisor_strategy: Some(SupervisorStrategy::Resume),
                    ..ThreadActorConfig::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            ask(&actor, BehaviorCommand::Snapshot).await.unwrap(),
            BehaviorReply::Snapshot(expected.clone())
        );
        for behavior in [Behavior::Scaled(3), Behavior::Scaled(-2)] {
            assert_eq!(
                ask(&actor, BehaviorCommand::Become(behavior))
                    .await
                    .unwrap(),
                BehaviorReply::Accepted
            );
            expected.stack.push(behavior);
        }
        assert_eq!(
            ask(&actor, BehaviorCommand::Add(5)).await.unwrap(),
            BehaviorReply::Accepted
        );
        expected.effects.push((5, -10));
        expected.total = -10;
        assert_eq!(
            ask(&actor, BehaviorCommand::Unbecome).await.unwrap(),
            BehaviorReply::Accepted
        );
        expected.stack.pop();
        assert_eq!(
            ask(&actor, BehaviorCommand::Suspend).await.unwrap(),
            BehaviorReply::Accepted
        );
        expected.suspended = true;
        for value in [3, -2, 3, 4] {
            assert_eq!(
                ask(&actor, BehaviorCommand::Add(value)).await.unwrap(),
                BehaviorReply::Buffered
            );
            expected.deferred.push(value);
        }
        assert_eq!(
            ask(&actor, BehaviorCommand::Snapshot).await.unwrap(),
            BehaviorReply::Snapshot(expected.clone()),
            "buffered work must have no business effects"
        );
        assert_eq!(
            ask(&actor, BehaviorCommand::Resume).await.unwrap(),
            BehaviorReply::Drained(4)
        );
        expected.suspended = false;
        expected.deferred.clear();
        expected.effects.extend([(3, 9), (-2, -6), (3, 9), (4, 12)]);
        expected.total = 14;
        assert_eq!(
            ask(&actor, BehaviorCommand::Snapshot).await.unwrap(),
            BehaviorReply::Snapshot(expected.clone()),
            "drain must execute every payload once in FIFO order"
        );
        assert_eq!(
            ask(&actor, BehaviorCommand::Resume).await.unwrap(),
            BehaviorReply::Drained(0)
        );
        assert_eq!(
            ask(&actor, BehaviorCommand::Snapshot).await.unwrap(),
            BehaviorReply::Snapshot(expected.clone()),
            "empty drain must not replay previous effects"
        );
        assert_eq!(
            ask(&actor, BehaviorCommand::Unbecome).await.unwrap(),
            BehaviorReply::Accepted
        );
        expected.stack.pop();
        assert_eq!(
            ask(&actor, BehaviorCommand::Add(4)).await.unwrap(),
            BehaviorReply::Accepted
        );
        expected.effects.push((4, 4));
        expected.total = 18;
        assert_eq!(
            ask(&actor, BehaviorCommand::Snapshot).await.unwrap(),
            BehaviorReply::Snapshot(expected.clone()),
            "unbecome restores the previous handler"
        );
        let failure = ask(&actor, BehaviorCommand::Unbecome).await;
        assert!(
            matches!(failure, Err(ActorError::MessageHandlingError(reason))
            if reason == "cannot remove base behavior")
        );
        assert_eq!(
            ask(&actor, BehaviorCommand::Snapshot).await.unwrap(),
            BehaviorReply::Snapshot(expected),
            "rejected control must preserve business state"
        );
        ts.shutdown_internal().await.unwrap();
    })
    .await
    .expect("state/behavior exceeded its independent liveness watchdog");
}

// ===========================================================================
// §9 Addressing — Akka: system.actorSelection("/user/x"), actorFor.
//    parrot: ThreadActorSystem::get_actor_ref(path).
// ===========================================================================

#[tokio::test]
async fn akka_parity_actor_selection_by_path() {
    let ts = mk_system();
    let a = ts
        .spawn_at::<Counter>(
            Counter::default(),
            "/akka/select/me",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    let _: u64 = ask(&a, IncN(5)).await.unwrap();

    // Resolve by path (actorSelection analog) — returns a working ref.
    let resolved = ts
        .get_actor_ref("/akka/select/me")
        .expect("selection resolves");
    let total: u64 = parrot_api::address::ActorRefExt::ask(&*resolved, GetTotal)
        .await
        .unwrap();
    assert_eq!(total, 5, "selected actor shares identity & state");

    // Dead-letter: unknown path resolves to None (Akka dead letters).
    assert!(ts.get_actor_ref("/akka/select/missing").is_none());

    // Stop removes the path from selection.
    ts.stop_actor("/akka/select/me").await.unwrap();
    assert!(ts.get_actor_ref("/akka/select/me").is_none());
}

// ===========================================================================
// §10 Graceful shutdown — Akka: PoisonPill, gracefulStop, CoordinatedShutdown.
//     parrot: stop_actor / shutdown (drains before_stop, notifies watchers).
// ===========================================================================

#[tokio::test]
async fn akka_parity_graceful_stop_and_shutdown() {
    let ts = mk_system();

    // Watcher observes shutdown notifications (CoordinatedShutdown analog).
    let watcher = ts
        .spawn_at::<DeathWatcher>(
            DeathWatcher::default(),
            "/akka/shutdown/watcher",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    let victim = ts
        .spawn_at::<SimpleWatched>(
            SimpleWatched::default(),
            "/akka/shutdown/victim",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    let _ = victim;
    ts.watch(
        "/akka/shutdown/watcher".into(),
        "/akka/shutdown/victim".into(),
    )
    .await
    .unwrap();

    // Successful single-actor stop admits the public termination event
    // before returning. The watcher query drains that earlier event.
    ts.stop_actor("/akka/shutdown/victim").await.unwrap();
    assert_normal_terminations(
        &ask(&watcher, WatcherQuery).await.unwrap(),
        &["/akka/shutdown/victim"],
    );
    assert!(ts.get_actor_ref("/akka/shutdown/victim").is_none());

    // System-wide coordinated shutdown: every actor stopped & registry empty.
    for i in 0..5 {
        let _ = ts
            .spawn_at::<Counter>(
                Counter::default(),
                &format!("/akka/shutdown/c{}", i),
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
    }
    assert!(ts.actor_count() >= 5);
    ts.shutdown_internal().await.expect("coordinated shutdown");
    assert_eq!(
        ts.actor_count(),
        0,
        "CoordinatedShutdown drains the registry"
    );
    assert!(ts.is_shutting_down());

    // Post-shutdown: spawns are refused (system gate).
    let r = ts
        .spawn_at::<Counter>(
            Counter::default(),
            "/akka/shutdown/late",
            None,
            ThreadActorConfig::default(),
        )
        .await;
    assert!(r.is_err(), "spawn after shutdown must fail fast");
}

// ===========================================================================
// GAP manifest — Akka single-node features not yet at parity.
// Documented here so the suite doubles as a living comparison report.
// ===========================================================================

// Remaining differences are documentation, not executable capability tests.
// Public termination events, configured Thread restart, priority ordering and
// receive-timeout now have behavioral regression suites. Checking nonempty
// strings cannot establish whether any engine capability is implemented.
// Open differences: actor event-sourcing/snapshots; engine become/stash;
// exponential restart backoff; an Akka-style receive builder (Parrot has typed
// generated dispatch); per-task public timer cancellation (actor stop cancels
// owned timers); automatic mailbox deadlock detection.

// ===========================================================================
// §11 Router pools — Akka: RoundRobinPool / BroadcastPool / random pool.
// ===========================================================================

#[derive(Clone, Debug, Message)]
#[message(result = "Vec<(usize, u64)>")]
struct PoolDispatch(u64);

/// Application router idiom: every request crosses the router mailbox before
/// reaching its routees. This does not assert a built-in Akka router facility.
#[derive(Debug)]
struct PoolRouter {
    routees: Vec<parrot_api::types::BoxedActorRef>,
    next: usize,
    broadcast: bool,
}

impl Actor for PoolRouter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn state(&self) -> ActorState {
        ActorState::Running
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let Some(dispatch) = msg.downcast_ref::<PoolDispatch>() else {
                return Err(ActorError::MessageHandlingError(
                    "unknown router payload".into(),
                ));
            };
            let mut replies = Vec::new();
            if self.broadcast {
                for (index, routee) in self.routees.iter().enumerate() {
                    let total: u64 = routee.ask(IncN(dispatch.0)).await?;
                    replies.push((index, total));
                }
            } else {
                let index = self.next;
                self.next = (self.next + 1) % self.routees.len();
                let total: u64 = self.routees[index].ask(IncN(dispatch.0)).await?;
                replies.push((index, total));
            }
            Ok(Box::new(replies) as BoxedMessage)
        })
    }
}

#[tokio::test]
async fn akka_parity_round_robin_pool() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let ts = mk_system();
        let mut routees = Vec::new();
        for i in 0..4 {
            routees.push(
                ts.spawn_at(
                    Counter::default(),
                    &format!("/akka/rr/r{i}"),
                    None,
                    ThreadActorConfig::default(),
                )
                .await
                .unwrap(),
            );
        }
        let router = ts
            .spawn_at(
                PoolRouter {
                    routees: routees
                        .iter()
                        .cloned()
                        .map(|r| Box::new(r) as parrot_api::types::BoxedActorRef)
                        .collect(),
                    next: 0,
                    broadcast: false,
                },
                "/akka/rr/router",
                None,
                ThreadActorConfig {
                    supervisor_strategy: Some(SupervisorStrategy::Resume),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let expected = [
            (0, 1),
            (1, 2),
            (2, 3),
            (3, 4),
            (0, 6),
            (1, 8),
            (2, 10),
            (3, 12),
        ];
        let replies =
            futures::future::join_all((1..=8).map(|value| ask(&router, PoolDispatch(value)))).await;
        for (reply, assignment) in replies.into_iter().zip(expected) {
            assert_eq!(
                reply.unwrap(),
                vec![assignment],
                "real router assignment or reply differs"
            );
        }
        for (routee, total) in routees.iter().zip([6, 8, 10, 12]) {
            assert_eq!(
                ask(routee, GetTotal).await.unwrap(),
                total,
                "routee effects must match the router's replies"
            );
        }
        ts.stop_actor("/akka/rr/r0").await.unwrap();
        let error = ask(&router, PoolDispatch(100)).await;
        assert!(
            matches!(error, Err(ActorError::Stopped)),
            "failed routee must fail router ASK: {error:?}"
        );
        assert_eq!(
            ask(&router, PoolDispatch(3)).await.unwrap(),
            vec![(1, 11)],
            "a failed assignment must not prevent routing to the next live routee"
        );
        ts.shutdown_internal().await.unwrap();
    })
    .await
    .expect("round-robin exceeded its independent liveness watchdog");
}

#[tokio::test]
async fn akka_parity_broadcast_pool() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let ts = mk_system();
        let mut routees = Vec::new();
        for i in 0..5 {
            routees.push(
                ts.spawn_at(
                    Counter::default(),
                    &format!("/akka/bc/r{i}"),
                    None,
                    ThreadActorConfig::default(),
                )
                .await
                .unwrap(),
            );
        }
        let router = ts
            .spawn_at(
                PoolRouter {
                    routees: routees
                        .iter()
                        .cloned()
                        .map(|r| Box::new(r) as parrot_api::types::BoxedActorRef)
                        .collect(),
                    next: 0,
                    broadcast: true,
                },
                "/akka/bc/router",
                None,
                ThreadActorConfig {
                    supervisor_strategy: Some(SupervisorStrategy::Resume),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        for (value, total) in [(7, 7), (11, 18)] {
            assert_eq!(
                ask(&router, PoolDispatch(value)).await.unwrap(),
                (0..5).map(|index| (index, total)).collect::<Vec<_>>(),
                "broadcast must return an actual reply from every routee"
            );
        }
        for routee in &routees {
            assert_eq!(
                ask(routee, GetTotal).await.unwrap(),
                18,
                "each broadcast must have exactly one effect per routee"
            );
        }
        // Stop the first routee: fail-fast broadcast must not silently claim
        // delivery or touch later routees after that observed failure.
        ts.stop_actor("/akka/bc/r0").await.unwrap();
        let error = ask(&router, PoolDispatch(100)).await;
        assert!(
            matches!(error, Err(ActorError::Stopped)),
            "failed routee must fail broadcast ASK: {error:?}"
        );
        for routee in &routees[1..] {
            assert_eq!(ask(routee, GetTotal).await.unwrap(), 18);
        }
        ts.shutdown_internal().await.unwrap();
    })
    .await
    .expect("broadcast exceeded its independent liveness watchdog");
}

// ===========================================================================
// §12 At-least-once delivery: owned durable outbox -> real Thread consumer.
//     This covers WAL recovery/retry/ACK, not Akka actor event sourcing.
// ===========================================================================

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct DeliverAttempt {
    seq: u64,
}

impl parrot_api::message::Message for DeliverAttempt {
    type Result = String;
}

#[derive(Clone, Debug, Message)]
#[message(result = "u64")]
struct DeliveredCount;

#[derive(Clone, Debug, Message)]
#[message(result = "Vec<u64>")]
struct DeliveredSeqs;

/// Application idempotence survives a negative receipt while this actor lives.
/// Durable business state across receiver restarts needs TransactionalInbox.
#[derive(Debug)]
struct IdempotentSink {
    dedup: parrot_remote::durable::DedupTable,
    order: Vec<u64>,
    attempts: tokio::sync::mpsc::UnboundedSender<u64>,
    release_first: Arc<tokio::sync::Semaphore>,
    fail_receipt_once: bool,
}

impl Actor for IdempotentSink {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(d) = msg.downcast_ref::<DeliverAttempt>() {
                if let parrot_remote::durable::DedupDecision::Process(permit) =
                    self.dedup.begin("parity-stream", d.seq).unwrap()
                {
                    self.order.push(d.seq);
                    permit.commit();
                }
                self.attempts.send(d.seq).unwrap();
                if self.fail_receipt_once {
                    self.fail_receipt_once = false;
                    self.release_first.acquire().await.unwrap().forget();
                    return Err(ActorError::MessageHandlingError(
                        "injected negative receipt after business effect".into(),
                    ));
                }
                return Ok(Box::new(format!("ack-{}", d.seq)) as BoxedMessage);
            }
            if msg.downcast_ref::<DeliveredCount>().is_some() {
                return Ok(Box::new(self.order.len() as u64) as BoxedMessage);
            }
            if msg.downcast_ref::<DeliveredSeqs>().is_some() {
                return Ok(Box::new(self.order.clone()) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError("unknown".into()))
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn akka_parity_at_least_once_delivery() {
    use parrot_api::message::RemoteMessage;
    use parrot_remote::durable::{DurableOutbox, OutboxConfig, OutboxError, Wal};
    use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};

    struct Lookup(Option<parrot_api::types::BoxedActorRef>);
    #[async_trait::async_trait]
    impl LocalLookup for Lookup {
        async fn lookup(&self, path: &str) -> Option<parrot_api::types::BoxedActorRef> {
            if path == "/akka/alod/sink" {
                self.0.as_ref().map(|actor| actor.clone_boxed())
            } else {
                None
            }
        }
    }

    tokio::time::timeout(Duration::from_secs(30), async {
        let id = uuid::Uuid::new_v4();
        let source_id = format!("parity-source-{id}");
        let target_id = format!("parity-target-{id}");
        let evidence =
            std::env::var_os("PARROT_PARITY_DURABLE_EVIDENCE").map(std::path::PathBuf::from);
        let base = evidence.clone().unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&base).unwrap();
        let directory = base.join(format!("parrot-parity-outbox-{id}"));
        std::fs::create_dir(&directory).unwrap();
        let source =
            RemoteActorSystem::new(RemoteConfig::mem(&source_id), Arc::new(Lookup(None))).unwrap();
        source.start().await.unwrap();
        let peer = NodeAddr::mem(&target_id);
        let config = OutboxConfig {
            attempt_timeout: Duration::from_secs(5),
            retry_min: Duration::from_millis(10),
            retry_max: Duration::from_millis(50),
            ..Default::default()
        };
        let mut outbox = DurableOutbox::open(
            &directory,
            &source,
            peer.clone(),
            "/akka/alod/sink",
            "session",
            config.clone(),
        )
        .await
        .unwrap();
        let handle = outbox.handle();
        // Payload sequence differs from the outbox sequence to catch byte/ID substitution.
        let sequence = [41, 12, 99];
        for (index, seq) in sequence.into_iter().enumerate() {
            let payload =
                parrot_api::message::serde_remote_serialize(&DeliverAttempt { seq }).unwrap();
            assert_eq!(
                handle
                    .enqueue(DeliverAttempt::TYPE_KEY, &payload)
                    .await
                    .unwrap(),
                index as u64 + 1
            );
        }
        assert_eq!(handle.status().pending, 3);
        assert_eq!(handle.status().last_acked, 0);
        outbox.close().await.unwrap();
        assert!(matches!(
            handle.enqueue(DeliverAttempt::TYPE_KEY, &[]).await,
            Err(OutboxError::Closed)
        ));
        {
            let wal = Wal::open(&directory).unwrap();
            let records = wal.replay(&target_id).unwrap();
            assert_eq!(
                records.len(),
                3,
                "offline accepted records must survive close"
            );
            for (index, record) in records.iter().enumerate() {
                assert_eq!(record.seq, index as u64 + 1);
                let payload: DeliverAttempt =
                    parrot_api::message::serde_remote_deserialize(&record.payload).unwrap();
                assert_eq!(payload.seq, sequence[index]);
            }
        }
        source.shutdown().await.unwrap();
        let source =
            RemoteActorSystem::new(RemoteConfig::mem(&source_id), Arc::new(Lookup(None))).unwrap();
        source.start().await.unwrap();
        let ts = mk_system();
        let (attempts, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let release_first = Arc::new(tokio::sync::Semaphore::new(0));
        let sink = ts
            .spawn_at(
                IdempotentSink {
                    dedup: parrot_remote::durable::DedupTable::new(),
                    order: vec![],
                    attempts,
                    release_first: release_first.clone(),
                    fail_receipt_once: true,
                },
                "/akka/alod/sink",
                None,
                ThreadActorConfig {
                    supervisor_strategy: Some(SupervisorStrategy::Resume),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let target = RemoteActorSystem::new(
            RemoteConfig::mem(&target_id),
            Arc::new(Lookup(Some(Box::new(sink.clone())))),
        )
        .unwrap();
        target.start().await.unwrap();
        // Only the production worker may connect, replay, retry and persist ACKs.
        let mut outbox = DurableOutbox::open(
            &directory,
            &source,
            peer,
            "/akka/alod/sink",
            "session",
            config,
        )
        .await
        .unwrap();
        let handle = outbox.handle();
        assert_eq!(observed.recv().await.unwrap(), 41);
        assert_eq!(
            handle.status().last_acked,
            0,
            "business execution alone is not a receipt"
        );
        assert_eq!(
            handle.status().pending,
            3,
            "no record may be retired before its completion receipt"
        );
        release_first.add_permits(1);
        handle.wait_acked(3).await.unwrap();
        let mut attempts = Vec::new();
        while let Ok(seq) = observed.try_recv() {
            attempts.push(seq);
        }
        assert!(
            attempts.contains(&41),
            "negative receipt must trigger a real automatic retry"
        );
        assert!(
            attempts.contains(&12) && attempts.contains(&99),
            "every recovered payload must reach the actor"
        );
        assert_eq!(ask(&sink, DeliveredCount).await.unwrap(), 3);
        assert_eq!(
            ask(&sink, DeliveredSeqs).await.unwrap(),
            sequence,
            "retry must preserve first-effect order without duplication"
        );
        let status = handle.status();
        assert!(status.attempts >= 4);
        assert_eq!(status.last_acked, 3);
        assert_eq!(status.pending, 0);
        outbox.close().await.unwrap();
        {
            let wal = Wal::open(&directory).unwrap();
            assert_eq!(
                wal.pending_len().unwrap(),
                0,
                "completion ACK must survive reopening WAL"
            );
        }
        source.shutdown().await.unwrap();
        target.shutdown().await.unwrap();
        ts.shutdown_internal().await.unwrap();
        if evidence.is_none() {
            std::fs::remove_dir_all(&directory).unwrap();
        }
    })
    .await
    .expect("durable delivery exceeded its independent liveness watchdog");
}

// ===========================================================================
// §13 Receive timeout — Akka: context.setReceiveTimeout → ReceiveTimeout msg.
// ===========================================================================

#[tokio::test]
async fn akka_parity_receive_timeout_idle_signal() {
    use parrot_api::address::ActorRef;
    use parrot_api::context::{ActorContext, LifecycleEvent};
    struct IdleProbe(tokio::sync::mpsc::UnboundedSender<()>);
    impl Actor for IdleProbe {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                ctx.set_receive_timeout(Some(Duration::from_millis(25)));
                Ok(())
            })
        }
        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                assert!(matches!(
                    msg.downcast_ref::<LifecycleEvent>(),
                    Some(LifecycleEvent::ReceiveTimeout)
                ));
                ctx.set_receive_timeout(None);
                self.0.send(()).unwrap();
                Ok(Box::new(()) as BoxedMessage)
            })
        }
    }
    let ts = mk_system();
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let idle = ts
        .spawn_at(
            IdleProbe(events),
            "/akka/idle",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), received.recv())
            .await
            .is_err(),
        "disabled receive timeout must not repeat"
    );
    idle.stop().await.unwrap();
    ts.shutdown_internal().await.unwrap();
}

// ===========================================================================
// §14 FSM stop reason — Akka: FSM stop(Failure/Shutdown/Normal) semantics.
// ===========================================================================

#[derive(Clone, Debug, Message)]
#[message(result = "String")]
struct FsmInput(String);

#[derive(Debug)]
struct MachineFsm {
    state: &'static str,
    transitions: Vec<String>,
    stop_reason: Option<&'static str>,
}

impl Actor for MachineFsm {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let Some(i) = msg.downcast_ref::<FsmInput>() else {
                return Err(ActorError::MessageHandlingError("unknown".into()));
            };
            match (self.state, i.0.as_str()) {
                ("idle", "start") => {
                    self.state = "running";
                    self.transitions.push("idle→running".into());
                }
                ("running", "finish") => {
                    self.state = "done";
                    self.stop_reason = Some("Normal");
                    self.transitions.push("running→done".into());
                }
                ("running", "error") => {
                    self.state = "failed";
                    self.stop_reason = Some("Failure");
                    self.transitions.push("running→failed".into());
                }
                _ => {
                    return Err(ActorError::MessageHandlingError(
                        "invalid transition".into(),
                    ));
                }
            }
            Ok(Box::new(format!("{}|{:?}", self.state, self.stop_reason)) as BoxedMessage)
        })
    }

    fn state(&self) -> ActorState {
        match self.stop_reason {
            None => ActorState::Running,
            Some(_) => ActorState::Stopping,
        }
    }
}

#[tokio::test]
async fn akka_parity_fsm_stop_reasons() {
    let ts = mk_system();
    for (name, transition, expected) in [
        ("normal", "finish", "done|Some(\"Normal\")"),
        ("failure", "error", "failed|Some(\"Failure\")"),
    ] {
        let actor = ts
            .spawn_at::<MachineFsm>(
                MachineFsm {
                    state: "idle",
                    transitions: vec![],
                    stop_reason: None,
                },
                &format!("/akka/fsm/{name}"),
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            ask(&actor, FsmInput("start".into())).await.unwrap(),
            "running|None",
            "FSM start must return the exact state and reason"
        );
        assert_eq!(
            ask(&actor, FsmInput(transition.into())).await.unwrap(),
            expected,
            "FSM terminal transition must return the exact state and reason"
        );
    }
    let bad = ts
        .spawn_at::<MachineFsm>(
            MachineFsm {
                state: "idle",
                transitions: vec![],
                stop_reason: None,
            },
            "/akka/fsm/invalid",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    let result = ask(&bad, FsmInput("finish".into())).await;
    assert!(
        matches!(result, Err(ActorError::MessageHandlingError(ref error)) if error == "invalid transition"),
        "FSM invalid transition must preserve its business error, got {result:?}"
    );
    // This actor's state string is a fixture-level FSM; engine retirement is
    // explicitly joined below rather than inferred from the word 'done'.
    ts.shutdown_internal()
        .await
        .expect("FSM shutdown must succeed");
}
