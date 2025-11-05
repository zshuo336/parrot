//! Managed typed actors obey the same namespace and stop boundary as dynamic actors.
use parrot::actix::ActixActorSystem;
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

#[actix::test]
async fn typed_actor_is_registered_and_resolvable() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let actor = system
        .spawn_typed::<Counter, Read>(counter(42), "/typed")
        .await
        .unwrap();
    assert_eq!(actor.ask(Read).await.unwrap(), 42);
    assert_eq!(system.status().active_actors, 1);
    assert_eq!(actor.path(), "/typed");
    let resolved = system
        .get_actor(&"/typed".to_owned())
        .await
        .expect("typed actor in registry");
    let answer = resolved
        .send_with_timeout(Box::new(Read), Some(Duration::from_secs(1)))
        .await
        .unwrap();
    assert_eq!(*answer.downcast::<usize>().unwrap(), 42);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn duplicate_typed_path_is_rejected() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
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
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn shutdown_stops_typed_actor_even_when_references_survive() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let state = counter(42);
    let drops = state.drops.clone();
    let actor = system
        .spawn_typed::<Counter, Read>(state, "/typed")
        .await
        .unwrap();
    assert_eq!(actor.ask(Read).await.unwrap(), 42);
    system.as_ref().clone().shutdown().await.unwrap();
    assert!(!actor.is_alive(), "shutdown must revoke typed references");
    assert!(actor.ask(Read).await.is_err());
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "shutdown joins actor disposal"
    );
}

#[actix::test]
async fn dynamic_bridge_stop_stops_all_typed_views() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let actor = system
        .spawn_typed::<Counter, Read>(counter(42), "/typed")
        .await
        .unwrap();
    let bridge = actor.clone().into_dyn();
    bridge.stop().await.unwrap();
    assert!(!actor.is_alive());
    assert!(actor.ask(Read).await.is_err());
    assert_eq!(system.status().active_actors, 0);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn typed_spawn_after_shutdown_is_rejected() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    system.as_ref().clone().shutdown().await.unwrap();
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
    system: &Arc<ActixActorSystem>,
    hook: Hook,
) -> (
    parrot::actix::typed::TypedActorRef<Managed, Command>,
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

#[actix::test]
async fn stop_cancels_handler_and_backpressured_sends_and_drains_queue() {
    let system = Arc::new(
        ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
            mailbox_capacity: 1,
            ..Default::default()
        })
        .await
        .unwrap(),
    );
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
    assert_eq!(system.status().active_actors, 0);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn cancelling_stop_caller_does_not_abandon_typed_cleanup() {
    let system = Arc::new(
        ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
            shutdown_timeout: Duration::from_millis(60),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let (actor, mut events) = spawn_managed(&system, Hook::Pending).await;
    let stopping = actor.clone();
    let caller = tokio::spawn(async move { stopping.stop().await });
    event(&mut events, "hook").await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let result = actor.stop().await;
    assert!(matches!(
        result,
        Err(parrot_api::errors::ActorError::TimeoutDetail(_))
    ));
    event(&mut events, "actor_drop").await;
    // A timeout receipt precedes disposal; the Drop signal in turn precedes
    // the lifecycle monitor receiving completion and removing this generation.
    tokio::time::timeout(Duration::from_secs(1), async {
        while system.get_actor(&actor.path().to_owned()).await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled caller must leave retirement owned by the monitor");
    assert_eq!(system.status().active_actors, 0);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn typed_hook_error_is_reported_after_cleanup() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
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
    assert_eq!(system.status().active_actors, 0);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn synchronous_typed_handler_does_not_block_shutdown_timer() {
    let system = Arc::new(
        ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: Duration::from_millis(40),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let (actor, mut events) = spawn_managed(&system, Hook::Ready).await;
    actor
        .tell(Command::Sleep(Duration::from_millis(250)))
        .await
        .unwrap();
    event(&mut events, "sleeping").await;
    let result = tokio::time::timeout(
        Duration::from_millis(150),
        system.as_ref().clone().shutdown(),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(parrot_api::system::SystemError::ActorError(
            parrot_api::errors::ActorError::TimeoutDetail(_)
        ))
    ));
    assert!(!actor.is_alive());
    event(&mut events, "actor_drop").await;
}

#[actix::test]
async fn typed_panic_removes_registration_and_does_not_run_stop_hook() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
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
        while system.status().active_actors != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!actor.is_alive());
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn stale_typed_reference_cannot_stop_reused_path() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
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
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn dropping_system_closes_typed_actor_with_surviving_reference() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let (actor, mut events) = spawn_managed(&system, Hook::Ready).await;
    drop(system);
    event(&mut events, "actor_drop").await;
    assert!(!actor.is_alive());
}

#[actix::test]
async fn shutdown_leaves_other_system_and_host_runtime_alive() {
    let first = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let other = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let actor = other
        .spawn_typed::<Counter, Read>(counter(43), "/other")
        .await
        .unwrap();
    first.as_ref().clone().shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(actor.ask(Read).await.unwrap(), 43);
    assert_eq!(
        first.status().state,
        parrot_api::system::SystemState::Stopped
    );
    assert_eq!(first.status().resources.thread_count, 0);
    other.as_ref().clone().shutdown().await.unwrap();
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
#[actix::test]
async fn pending_and_panicking_initialization_are_owned_and_cleaned_up() {
    for panic in [false, true] {
        let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
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
            while system.status().active_actors != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!actor.is_alive());
        system.as_ref().clone().shutdown().await.unwrap();
    }
}

#[actix::test]
async fn managed_ask_uses_configured_deadline() {
    let system = Arc::new(
        ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
            arbiter_count: 1,
            ask_timeout: Duration::from_millis(20),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let (actor, _events) = spawn_managed(&system, Hook::Ready).await;
    let result = tokio::time::timeout(Duration::from_secs(1), actor.ask(Command::Hold))
        .await
        .unwrap();
    assert!(matches!(
        result,
        Err(parrot_api::errors::ActorError::TimeoutDetail(_))
    ));
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn cancelling_shutdown_caller_still_finishes_owned_shutdown() {
    let system = Arc::new(
        ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: Duration::from_millis(80),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let (actor, mut events) = spawn_managed(&system, Hook::Pending).await;
    let stopping = system.as_ref().clone();
    let first = tokio::spawn(async move { stopping.shutdown().await });
    event(&mut events, "hook").await;
    first.abort();
    let (second, third) = tokio::join!(
        system.as_ref().clone().shutdown(),
        system.as_ref().clone().shutdown()
    );
    assert!(second.is_err());
    assert_eq!(
        second.unwrap_err().to_string(),
        third.unwrap_err().to_string()
    );
    event(&mut events, "actor_drop").await;
    assert!(!actor.is_alive());
    tokio::time::timeout(Duration::from_secs(1), async {
        while system.status().state != parrot_api::system::SystemState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[actix::test]
async fn spawn_shutdown_race_from_foreign_threads_is_atomic() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let drops = Arc::new(AtomicUsize::new(0));
    let (send, receive) = tokio::sync::oneshot::channel();
    let spawning = system.clone();
    let dropped = drops.clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(33));
    let start = barrier.clone();
    // This runtime has no Actix System TLS; the actor's arbiter is pre-owned.
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let refs = runtime.block_on(async move {
            let mut tasks = Vec::new();
            for index in 0..32 {
                let (system, barrier, drops) = (spawning.clone(), start.clone(), dropped.clone());
                tasks.push(tokio::spawn(async move {
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
            let mut refs = Vec::new();
            for task in tasks {
                refs.push(task.await.unwrap());
            }
            refs
        });
        let _ = send.send(refs);
    });
    barrier.wait().await;
    system.as_ref().clone().shutdown().await.unwrap();
    for actor in receive.await.unwrap().into_iter().flatten() {
        assert!(!actor.is_alive());
    }
    thread.join().unwrap();
    assert_eq!(system.status().active_actors, 0);
    assert_eq!(drops.load(Ordering::SeqCst), 32);
}

type AffinityEvents = Arc<std::sync::Mutex<Vec<(&'static str, std::thread::ThreadId)>>>;
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
impl parrot_api::typed::ParrotTypedDispatch for Affinity {
    type Msg = Read;
    type Reply = usize;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.record("init_create");
        Box::pin(async move {
            self.record("init_poll");
            tokio::task::yield_now().await;
            self.record("init_resume");
            Ok(())
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.record("hook_create");
        Box::pin(async move {
            self.record("hook_poll");
            tokio::task::yield_now().await;
            self.record("hook_resume");
            Ok(())
        })
    }
    fn dispatch(&mut self, _: Read) -> BoxedFuture<'_, ActorResult<usize>> {
        self.record("dispatch_create");
        Box::pin(async move {
            self.record("dispatch_poll");
            tokio::task::yield_now().await;
            self.record("dispatch_resume");
            Ok(42)
        })
    }
}
impl parrot_api::typed::ParrotMsgVariant<Affinity> for Read {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}
#[actix::test]
async fn hooks_dispatch_and_disposal_stay_on_owning_arbiter() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let events = AffinityEvents::default();
    let actor = system
        .spawn_typed::<Affinity, Read>(Affinity(events.clone()), "/affinity")
        .await
        .unwrap();
    assert_eq!(actor.ask(Read).await.unwrap(), 42);
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
    system.as_ref().clone().shutdown().await.unwrap();
}

struct PanicPayload(Arc<AtomicUsize>);
impl Drop for PanicPayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("payload destructor failed");
    }
}
struct CleanupQueue(tokio::sync::mpsc::UnboundedSender<&'static str>);
impl Drop for CleanupQueue {
    fn drop(&mut self) {
        let _ = self.0.send("actor_drop");
    }
}
enum CleanupCommand {
    Hold,
    Bomb(PanicPayload),
    Tracked(Payload),
}
impl Message for CleanupCommand {
    type Result = ();
}
impl parrot_api::typed::ParrotTypedDispatch for CleanupQueue {
    type Msg = CleanupCommand;
    type Reply = ();
    fn dispatch(&mut self, message: CleanupCommand) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            match message {
                CleanupCommand::Hold => {
                    let _ = self.0.send("held");
                    std::future::pending().await
                }
                CleanupCommand::Bomb(payload) => {
                    drop(payload);
                    Ok(())
                }
                CleanupCommand::Tracked(payload) => {
                    drop(payload);
                    Ok(())
                }
            }
        })
    }
}
impl parrot_api::typed::ParrotMsgVariant<CleanupQueue> for CleanupCommand {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(_: ()) -> ActorResult<()> {
        Ok(())
    }
}
#[actix::test]
async fn panicking_queued_destructor_does_not_strand_following_payloads() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let actor = system
        .spawn_typed::<CleanupQueue, CleanupCommand>(CleanupQueue(events), "/cleanup")
        .await
        .unwrap();
    actor.tell(CleanupCommand::Hold).await.unwrap();
    event(&mut receiver, "held").await;
    let dropped = Arc::new(AtomicUsize::new(0));
    actor
        .tell(CleanupCommand::Bomb(PanicPayload(dropped.clone())))
        .await
        .unwrap();
    actor
        .tell(CleanupCommand::Tracked(Payload(dropped.clone())))
        .await
        .unwrap();
    let result = actor.stop().await;
    assert!(
        matches!(result, Err(parrot_api::errors::ActorError::Panic(ref s)) if s.contains("payload destructor failed"))
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    event(&mut receiver, "actor_drop").await;
    system.as_ref().clone().shutdown().await.unwrap();
}
