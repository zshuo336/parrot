//! Managed typed actors obey the same namespace and stop boundary as dynamic actors.
use parrot::thread::{config::ThreadActorSystemConfig, system::ThreadActorSystem};
use parrot_api::{
    message::Message,
    typed::TypedReceive,
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
pub struct Read;
impl Message for Read {
    type Result = usize;
}
#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(Read))]
struct Counter {
    value: usize,
    drops: Arc<AtomicUsize>,
}
impl Drop for Counter {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl TypedReceive<Read> for Counter {
    fn receive_typed<'a>(&'a mut self, _: Read) -> BoxedFuture<'a, ActorResult<usize>> {
        Box::pin(async move { Ok(self.value) })
    }
}
fn counter(value: usize) -> Counter {
    Counter {
        value,
        drops: Arc::new(AtomicUsize::new(0)),
    }
}

#[tokio::test]
async fn typed_actor_is_registered_and_resolvable() {
    let system = ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_typed::<Counter, Read>(counter(42), "/typed")
        .await
        .unwrap();
    assert_eq!(actor.ask(Read).await.unwrap(), 42);
    assert_eq!(system.actor_count(), 1);
    assert_eq!(system.actor_paths(), vec!["/typed"]);
    let resolved = system
        .get_actor_ref("/typed")
        .expect("typed actor in registry");
    let answer = resolved
        .send_with_timeout(Box::new(Read), Some(Duration::from_secs(1)))
        .await
        .unwrap();
    assert_eq!(*answer.downcast::<usize>().unwrap(), 42);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn duplicate_typed_path_is_rejected() {
    let system = ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_typed::<Counter, Read>(counter(42), "/typed")
        .await
        .unwrap();
    let duplicate = system
        .spawn_typed::<Counter, Read>(counter(43), "/typed")
        .await;
    assert!(
        duplicate.is_err(),
        "one logical path must identify one actor"
    );
    assert_eq!(actor.ask(Read).await.unwrap(), 42);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn shutdown_stops_typed_actor_even_when_references_survive() {
    let system = ThreadActorSystem::shared(Default::default());
    let state = counter(42);
    let drops = state.drops.clone();
    let actor = system
        .spawn_typed::<Counter, Read>(state, "/typed")
        .await
        .unwrap();
    assert_eq!(actor.ask(Read).await.unwrap(), 42);
    system.shutdown_internal().await.unwrap();
    assert!(!actor.is_alive(), "shutdown must revoke typed references");
    assert!(actor.ask(Read).await.is_err());
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "shutdown joins actor disposal"
    );
}

#[tokio::test]
async fn dynamic_bridge_stop_stops_all_typed_views() {
    let system = ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_typed::<Counter, Read>(counter(42), "/typed")
        .await
        .unwrap();
    let bridge = actor.clone().into_dyn();
    bridge.stop().await.unwrap();
    assert!(!actor.is_alive());
    assert!(actor.ask(Read).await.is_err());
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn typed_spawn_after_shutdown_is_rejected() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    system.shutdown_internal().await.unwrap();
    assert!(
        system
            .spawn_typed::<Counter, Read>(counter(42), "/typed")
            .await
            .is_err()
    );
}

pub struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
pub enum Command {
    Ping,
    Tracked(Payload),
    Hold,
    Gate(Arc<tokio::sync::Notify>),
    Crash,
    Sleep(Duration),
}
impl Message for Command {
    type Result = usize;
}
struct Managed {
    events: tokio::sync::mpsc::UnboundedSender<&'static str>,
    hook: Hook,
}
#[derive(Clone, Copy)]
enum Hook {
    Ready,
    Pending,
    Error,
}
struct EventOnDrop(
    tokio::sync::mpsc::UnboundedSender<&'static str>,
    &'static str,
);
impl Drop for EventOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(self.1);
    }
}
impl Drop for Managed {
    fn drop(&mut self) {
        let _ = self.events.send("actor_drop");
    }
}
impl parrot_api::typed::ParrotTypedDispatch for Managed {
    type Msg = Command;
    type Reply = usize;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let _ = self.events.send("init");
            Ok(())
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let _drop = EventOnDrop(self.events.clone(), "hook_drop");
            let _ = self.events.send("hook");
            match self.hook {
                Hook::Ready => Ok(()),
                Hook::Pending => std::future::pending().await,
                Hook::Error => Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "stop hook failed".into(),
                )),
            }
        })
    }
    fn dispatch(&mut self, message: Command) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async move {
            let _drop = EventOnDrop(self.events.clone(), "dispatch_drop");
            match message {
                Command::Ping | Command::Tracked(_) => Ok(42),
                Command::Hold => {
                    let _ = self.events.send("held");
                    std::future::pending().await
                }
                Command::Gate(gate) => {
                    let _ = self.events.send("held");
                    gate.notified().await;
                    Ok(42)
                }
                Command::Crash => panic!("typed crash detail"),
                Command::Sleep(duration) => {
                    let _ = self.events.send("sleeping");
                    std::thread::sleep(duration);
                    Ok(42)
                }
            }
        })
    }
}
impl parrot_api::typed::ParrotMsgVariant<Managed> for Command {
    fn inject(message: Self) -> Command {
        message
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}
async fn event(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<&'static str>, wanted: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if receiver.recv().await.expect("event channel closed") == wanted {
                return;
            }
        }
    })
    .await
    .unwrap();
}
async fn spawn_managed(
    system: &Arc<ThreadActorSystem>,
    hook: Hook,
) -> (
    parrot::thread::typed::TypedActorRef<Managed, Command>,
    tokio::sync::mpsc::UnboundedReceiver<&'static str>,
) {
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let actor = system
        .spawn_typed(Managed { events, hook }, "/managed")
        .await
        .unwrap();
    event(&mut receiver, "init").await;
    (actor, receiver)
}

#[tokio::test]
async fn stop_cancels_handler_and_backpressured_sends_and_drains_queue() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_mailbox_capacity: 1,
        ..Default::default()
    });
    let (actor, mut events) = spawn_managed(&system, Hook::Ready).await;
    let active = {
        let actor = actor.clone();
        tokio::spawn(async move { actor.ask_unbounded(Command::Hold).await })
    };
    event(&mut events, "held").await;
    let drops = Arc::new(AtomicUsize::new(0));
    actor
        .tell(Command::Tracked(Payload(drops.clone())))
        .await
        .unwrap();
    let blocked = actor.tell(Command::Tracked(Payload(drops.clone())));
    tokio::pin!(blocked);
    assert!(futures::poll!(&mut blocked).is_pending());
    system.stop_actor("/managed").await.unwrap();
    assert!(blocked.await.is_err());
    assert_eq!(
        drops.load(Ordering::SeqCst),
        2,
        "both queued and unadmitted payloads must be released"
    );
    assert!(active.await.unwrap().is_err());
    let remaining: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    assert_eq!(
        remaining,
        ["dispatch_drop", "hook", "hook_drop", "actor_drop"]
    );
    assert!(!actor.is_alive());
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancelling_stop_caller_does_not_abandon_typed_cleanup() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shutdown_timeout: Duration::from_millis(60),
        ..Default::default()
    });
    let (actor, mut events) = spawn_managed(&system, Hook::Pending).await;
    let caller = tokio::spawn(async move { actor.stop().await });
    event(&mut events, "hook").await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let result = system.stop_actor("/managed").await;
    assert!(matches!(
        result,
        Err(parrot::thread::error::SystemError::Timeout(_))
    ));
    event(&mut events, "actor_drop").await;
    // The destructor signal precedes the monitor receiving its completion.
    tokio::time::timeout(Duration::from_secs(1), async {
        while system.get_actor_ref("/managed").is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned cleanup must retire the timed-out generation");
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn typed_hook_error_is_reported_after_cleanup() {
    let system = ThreadActorSystem::shared(Default::default());
    let (actor, mut events) = spawn_managed(&system, Hook::Error).await;
    assert!(
        actor
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("stop hook failed")
    );
    event(&mut events, "actor_drop").await;
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_typed_handler_does_not_block_shutdown_timer() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shutdown_timeout: Duration::from_millis(40),
        ..Default::default()
    });
    let (actor, mut events) = spawn_managed(&system, Hook::Ready).await;
    actor
        .tell(Command::Sleep(Duration::from_millis(250)))
        .await
        .unwrap();
    event(&mut events, "sleeping").await;
    let result = tokio::time::timeout(Duration::from_millis(150), system.shutdown_internal())
        .await
        .unwrap();
    assert!(matches!(
        result,
        Err(parrot::thread::error::SystemError::Timeout(_))
    ));
    assert!(!actor.is_alive());
    event(&mut events, "actor_drop").await;
}

#[tokio::test]
async fn typed_panic_removes_registration_and_does_not_run_stop_hook() {
    let system = ThreadActorSystem::shared(Default::default());
    let (actor, mut events) = spawn_managed(&system, Hook::Ready).await;
    assert!(actor.ask(Command::Crash).await.is_err());
    let drops = tokio::time::timeout(Duration::from_secs(2), async {
        let mut seen = Vec::new();
        while let Some(value) = events.recv().await {
            seen.push(value);
            if value == "actor_drop" {
                break;
            }
        }
        seen
    })
    .await
    .unwrap();
    assert_eq!(
        drops,
        ["dispatch_drop", "actor_drop"],
        "panic must skip before_stop on the poisoned actor"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while system.actor_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!actor.is_alive());
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn stale_typed_reference_cannot_stop_reused_path() {
    let system = ThreadActorSystem::shared(Default::default());
    let old = system
        .spawn_typed::<Counter, Read>(counter(42), "/typed")
        .await
        .unwrap();
    old.stop().await.unwrap();
    let fresh = system
        .spawn_typed::<Counter, Read>(counter(43), "/typed")
        .await
        .unwrap();
    old.stop().await.unwrap();
    assert_eq!(fresh.ask(Read).await.unwrap(), 43);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn dropping_system_closes_typed_actor_with_surviving_reference() {
    let system = ThreadActorSystem::shared(Default::default());
    let (actor, mut events) = spawn_managed(&system, Hook::Ready).await;
    drop(system);
    event(&mut events, "actor_drop").await;
    assert!(!actor.is_alive());
}

struct Dynamic;
impl parrot_api::actor::Actor for Dynamic {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = parrot::thread::context::ThreadContext<Self>;
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: parrot_api::types::BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_and_dynamic_spawns_share_one_atomic_namespace() {
    let system = ThreadActorSystem::shared(Default::default());
    let mut attempts = Vec::new();
    for index in 0..32 {
        let system = system.clone();
        attempts.push(tokio::spawn(async move {
            if index % 2 == 0 {
                system
                    .spawn_typed::<Counter, Read>(counter(42), "/shared")
                    .await
                    .is_ok()
            } else {
                system
                    .spawn_at(Dynamic, "/shared", None, Default::default())
                    .await
                    .is_ok()
            }
        }));
    }
    let mut successes = 0;
    for task in attempts {
        successes += usize::from(task.await.unwrap());
    }
    assert_eq!(successes, 1);
    assert_eq!(system.actor_count(), 1);
    system.stop_actor("/shared").await.unwrap();
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn typed_ask_uses_configured_default_timeout() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_ask_timeout: Duration::from_millis(20),
        ..Default::default()
    });
    let (actor, _events) = spawn_managed(&system, Hook::Ready).await;
    let result = tokio::time::timeout(Duration::from_millis(500), actor.ask(Command::Hold))
        .await
        .unwrap();
    assert!(matches!(
        result,
        Err(parrot_api::errors::ActorError::TimeoutDetail(_))
    ));
    system.shutdown_internal().await.unwrap();
}

struct TypedWatcher(tokio::sync::mpsc::UnboundedSender<parrot::thread::system::Terminated>);
impl parrot_api::typed::ParrotTypedDispatch for TypedWatcher {
    type Msg = parrot::thread::system::Terminated;
    type Reply = ();
    fn dispatch(&mut self, event: Self::Msg) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.0.send(event).unwrap();
            Ok(())
        })
    }
}
impl parrot_api::typed::ParrotMsgVariant<TypedWatcher> for parrot::thread::system::Terminated {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(_: ()) -> ActorResult<()> {
        Ok(())
    }
}
#[tokio::test]
async fn typed_watcher_receives_actual_typed_panic_reason() {
    let system = ThreadActorSystem::shared(Default::default());
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let _watcher = system
        .spawn_typed::<TypedWatcher, parrot::thread::system::Terminated>(
            TypedWatcher(events),
            "/watcher",
        )
        .await
        .unwrap();
    let (actor, _events) = spawn_managed(&system, Hook::Ready).await;
    system
        .watch("/watcher".into(), "/managed".into())
        .await
        .unwrap();
    assert!(actor.ask(Command::Crash).await.is_err());
    let death = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(death.reason, parrot_api::supervisor::DeathReason::Panic(ref reason) if reason.contains("typed crash detail"))
    );
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_spawn_shutdown_race_never_leaves_live_registered_actors() {
    let system = ThreadActorSystem::shared(Default::default());
    let barrier = Arc::new(tokio::sync::Barrier::new(33));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut spawns = Vec::new();
    for index in 0..32 {
        let (system, barrier, drops) = (system.clone(), barrier.clone(), drops.clone());
        spawns.push(tokio::spawn(async move {
            barrier.wait().await;
            system
                .spawn_typed::<Counter, Read>(
                    Counter {
                        value: index,
                        drops,
                    },
                    &format!("/race/{index}"),
                )
                .await
        }));
    }
    barrier.wait().await;
    system.shutdown_internal().await.unwrap();
    for spawn in spawns {
        if let Ok(actor) = spawn.await.unwrap() {
            assert!(!actor.is_alive());
        }
    }
    assert_eq!(system.actor_count(), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 32);
}

#[tokio::test(flavor = "current_thread")]
async fn waiting_typed_actor_releases_single_execution_permit() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        ..Default::default()
    });
    let (held, mut events) = spawn_managed(&system, Hook::Ready).await;
    held.tell(Command::Hold).await.unwrap();
    event(&mut events, "held").await;
    let other = system
        .spawn_typed::<Counter, Read>(counter(42), "/other")
        .await
        .unwrap();
    assert_eq!(
        other
            .ask_with_timeout(Read, Duration::from_millis(500))
            .await
            .unwrap(),
        42
    );
    system.shutdown_internal().await.unwrap();
}

struct Initializing {
    events: tokio::sync::mpsc::UnboundedSender<&'static str>,
    panic: bool,
}
impl Drop for Initializing {
    fn drop(&mut self) {
        let _ = self.events.send("actor_drop");
    }
}
impl parrot_api::typed::ParrotTypedDispatch for Initializing {
    type Msg = Read;
    type Reply = usize;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            let _drop = EventOnDrop(self.events.clone(), "init_drop");
            let _ = self.events.send("init_entered");
            assert!(!self.panic, "typed initialization panic");
            std::future::pending().await
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        panic!("before_stop must not run on an uninitialized actor");
    }
    fn dispatch(&mut self, _: Read) -> BoxedFuture<'_, ActorResult<usize>> {
        panic!("dispatch must not run before initialization finishes");
    }
}
impl parrot_api::typed::ParrotMsgVariant<Initializing> for Read {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}
#[tokio::test]
async fn pending_and_panicking_initialization_are_owned_and_cleaned_up() {
    for panic in [false, true] {
        let system = ThreadActorSystem::shared(Default::default());
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let actor = system
            .spawn_typed::<Initializing, Read>(Initializing { events, panic }, "/initializing")
            .await
            .unwrap();
        event(&mut receiver, "init_entered").await;
        if !panic {
            actor.stop().await.unwrap();
        }
        let events = tokio::time::timeout(Duration::from_secs(2), async {
            let mut seen = vec![];
            while let Some(value) = receiver.recv().await {
                seen.push(value);
                if value == "actor_drop" {
                    break;
                }
            }
            seen
        })
        .await
        .unwrap();
        assert_eq!(events, ["init_drop", "actor_drop"]);
        tokio::time::timeout(Duration::from_secs(2), async {
            while system.actor_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!actor.is_alive());
        system.shutdown_internal().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn busy_typed_actor_yields_before_exhausting_its_backlog() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        default_mailbox_capacity: 64,
        default_scheduling_mode: parrot::thread::config::SchedulingMode::SharedPool {
            max_messages_per_run: 2,
        },
        ..Default::default()
    });
    let (busy, mut events) = spawn_managed(&system, Hook::Ready).await;
    let other = system
        .spawn_typed::<Counter, Read>(counter(42), "/other")
        .await
        .unwrap();
    assert_eq!(other.ask(Read).await.unwrap(), 42);
    let gate = Arc::new(tokio::sync::Notify::new());
    busy.tell(Command::Gate(gate.clone())).await.unwrap();
    event(&mut events, "held").await;
    for _ in 0..64 {
        busy.tell(Command::Sleep(Duration::from_millis(5)))
            .await
            .unwrap();
    }
    gate.notify_one();
    event(&mut events, "sleeping").await;
    assert_eq!(
        other
            .ask_with_timeout(Read, Duration::from_secs(2))
            .await
            .unwrap(),
        42
    );
    let processed = 1 + std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| *event == "sleeping")
        .count();
    assert!(
        processed < 64,
        "a waiting actor must run before its neighbour exhausts the whole backlog"
    );
    system.shutdown_internal().await.unwrap();
}
