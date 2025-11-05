//! Common typed spawner must create real owned children on both engines.
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContext, ActorSpawnerExt},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
struct Spawn;
struct LegacySpawn;
struct ParentPath;
struct ThreadChild;
struct ActixChild;
impl ActixEngineExt for ActixChild {}
macro_rules! child {
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
                _: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(
                    async move { Ok(Box::new(ctx.parent().map(|p| p.path())) as BoxedMessage) },
                )
            }
        }
    };
}
child!(ThreadChild, parrot::thread::ThreadContext<Self>);
child!(
    ActixChild,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
struct ThreadParent;
struct ActixParent;
impl ActixEngineExt for ActixParent {}
impl Actor for ThreadParent {
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
            let child = if msg.is::<LegacySpawn>() {
                ctx.spawner()
                    .spawn(
                        parrot::thread::system::TypedSpawnPayload::new(ThreadChild).into_boxed(),
                        Box::new(EmptyConfig),
                    )
                    .await?
            } else {
                ctx.spawner().spawn_typed(ThreadChild, EmptyConfig).await?
            };
            Ok(Box::new(child) as BoxedMessage)
        })
    }
}
impl Actor for ActixParent {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let child = ctx.spawner().spawn_typed(ActixChild, EmptyConfig).await?;
            Ok(Box::new(child) as BoxedMessage)
        })
    }
}
async fn spawn(parent: &dyn ActorRef, request: BoxedMessage) -> BoxedActorRef {
    *parent
        .send(request)
        .await
        .expect("typed context spawner must create a child")
        .downcast::<BoxedActorRef>()
        .unwrap()
}
async fn check_parent(child: &dyn ActorRef, expected: String) {
    let actual = *child
        .send(Box::new(ParentPath))
        .await
        .unwrap()
        .downcast::<Option<String>>()
        .unwrap();
    assert_eq!(
        actual,
        Some(expected),
        "child context must hold its actual parent"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_common_typed_spawner_creates_a_child() {
    let sys = parrot::thread::ThreadActorSystem::shared(Default::default());
    let parent = sys
        .spawn_at(ThreadParent, "/parent", None, Default::default())
        .await
        .unwrap();
    let child = spawn(&parent, Box::new(Spawn)).await;
    check_parent(&*child, parent.path()).await;
    sys.shutdown_internal().await.unwrap();
}
#[actix::test]
async fn actix_common_typed_spawner_creates_a_child() {
    let sys = parrot::actix::ActixActorSystem::new().await.unwrap();
    let parent = sys
        .spawn_root_typed(ActixParent, EmptyConfig)
        .await
        .unwrap();
    let child = spawn(&*parent, Box::new(Spawn)).await;
    check_parent(&*child, parent.path()).await;
    sys.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_legacy_payload_keeps_real_parent_relationship() {
    let sys = parrot::thread::ThreadActorSystem::shared(Default::default());
    let parent = sys
        .spawn_at(ThreadParent, "/parent", None, Default::default())
        .await
        .unwrap();
    let child = spawn(&parent, Box::new(LegacySpawn)).await;
    check_parent(&*child, parent.path()).await;
    sys.shutdown_internal().await.unwrap();
}

use parrot_api::errors::ActorError;
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

#[derive(Default)]
struct DropGate {
    entered: AtomicBool,
    open: Mutex<bool>,
    wake: Condvar,
}
impl DropGate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.wake.notify_all();
    }
}
#[derive(Clone)]
struct Spec {
    name: String,
    events: mpsc::UnboundedSender<String>,
    depth: usize,
    fail_init: bool,
    gate: Option<Arc<DropGate>>,
    panic_drop: bool,
}
impl Spec {
    fn child(&self, name: &str) -> Self {
        Self {
            name: name.into(),
            events: self.events.clone(),
            depth: 0,
            fail_init: false,
            gate: None,
            panic_drop: false,
        }
    }
}
struct ThreadNode(Spec);
struct ActixNode(Spec);
impl ActixEngineExt for ActixNode {}
enum Command {
    Spawn(Spec),
    Snapshot,
    ScheduleDrop,
    StopChild(BoxedActorRef),
    Metadata(BoxedActorRef),
    SelfStop,
    Crash,
}
struct TimerDrop(mpsc::UnboundedSender<String>);
impl Drop for TimerDrop {
    fn drop(&mut self) {
        let _ = self.0.send("timer-dropped".into());
    }
}
macro_rules! node {
    ($name:ident, $ctx:ty) => {
        impl Drop for $name {
            fn drop(&mut self) {
                if let Some(gate) = &self.0.gate {
                    gate.entered.store(true, Ordering::Release);
                    let mut open = gate.open.lock().unwrap();
                    while !*open {
                        open = gate.wake.wait(open).unwrap();
                    }
                }
                let _ = self.0.events.send(format!("drop:{}", self.0.name));
                assert!(!self.0.panic_drop, "child destructor panic");
            }
        }
        impl Actor for $name {
            type Config = EmptyConfig;
            type Context = $ctx;
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
                    if self.0.depth > 0 {
                        let mut spec = self.0.child(&format!("{}-child", self.0.name));
                        spec.depth = self.0.depth - 1;
                        let child = ctx.spawner().spawn_typed(Self(spec), EmptyConfig).await?;
                        // Waiting for a real message proves the child's initialization completed.
                        child.send(Box::new(Command::Snapshot)).await?;
                    }
                    let _ = self.0.events.send(format!("init:{}", self.0.name));
                    if self.0.fail_init {
                        return Err(ActorError::InitializationError(
                            "intentional parent init failure".into(),
                        ));
                    }
                    Ok(())
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    match *msg.downcast::<Command>().unwrap() {
                        Command::Spawn(spec) => Ok(Box::new(
                            ctx.spawner().spawn_typed(Self(spec), EmptyConfig).await?,
                        ) as BoxedMessage),
                        Command::Snapshot => Ok(Box::new((
                            ctx.parent().map(|p| p.path()),
                            ctx.children()
                                .map(|c| c.read_all().iter().map(|a| a.path()).collect::<Vec<_>>())
                                .unwrap_or_default(),
                        )) as BoxedMessage),
                        Command::StopChild(child) => {
                            child.stop().await?;
                            Ok(Box::new(()) as BoxedMessage)
                        }
                        Command::Metadata(child) => {
                            ctx.add_child(child);
                            Ok(Box::new(()) as BoxedMessage)
                        }
                        Command::ScheduleDrop => {
                            ctx.schedule_once(
                                ctx.get_self_ref(),
                                Box::new(TimerDrop(self.0.events.clone())),
                                Duration::from_secs(60),
                            )
                            .await?;
                            Ok(Box::new(()) as BoxedMessage)
                        }
                        Command::SelfStop => {
                            ctx.stop().await?;
                            Ok(Box::new(()) as BoxedMessage)
                        }
                        Command::Crash => panic!("parent crashed"),
                    }
                })
            }
            fn before_stop<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    let _ = self.0.events.send(format!("stop:{}", self.0.name));
                    Ok(())
                })
            }
            fn handle_child_terminated<'a>(
                &'a mut self,
                child: BoxedActorRef,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    assert!(
                        !ctx.children()
                            .unwrap()
                            .read_all()
                            .iter()
                            .any(|c| c.path() == child.path()),
                        "dead child must already be removed from topology"
                    );
                    let _ = self.0.events.send(format!("terminated:{}", self.0.name));
                    Ok(())
                })
            }
        }
    };
}
node!(ThreadNode, parrot::thread::ThreadContext<Self>);
node!(
    ActixNode,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);

enum System {
    Thread(Arc<parrot::thread::ThreadActorSystem>),
    Actix(parrot::actix::ActixActorSystem),
}
impl System {
    async fn new(actix: bool, timeout: Duration) -> Self {
        if actix {
            Self::Actix(
                parrot::actix::ActixActorSystem::with_settings(
                    parrot::actix::ActixSystemSettings {
                        arbiter_count: 2,
                        shutdown_timeout: timeout,
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
            )
        } else {
            Self::Thread(parrot::thread::ThreadActorSystem::shared(
                parrot::thread::config::ThreadActorSystemConfig {
                    shutdown_timeout: timeout,
                    ..Default::default()
                },
            ))
        }
    }
    async fn root(&self, spec: Spec) -> BoxedActorRef {
        match self {
            Self::Actix(s) => s
                .spawn_root_typed(ActixNode(spec), EmptyConfig)
                .await
                .unwrap(),
            Self::Thread(s) => Box::new(
                s.spawn_at(
                    ThreadNode(spec),
                    &format!("/{}", uuid::Uuid::new_v4()),
                    None,
                    Default::default(),
                )
                .await
                .unwrap(),
            ),
        }
    }
    async fn exists(&self, path: &str) -> bool {
        match self {
            Self::Thread(s) => s.get_actor_ref(path).is_some(),
            Self::Actix(s) => s.get_actor(&path.to_owned()).await.is_some(),
        }
    }
    async fn shutdown(self) {
        match self {
            Self::Thread(s) => s.shutdown_internal().await.unwrap(),
            Self::Actix(s) => s.shutdown().await.unwrap(),
        }
    }
}
fn spec() -> (Spec, mpsc::UnboundedReceiver<String>) {
    let (events, recv) = mpsc::unbounded_channel();
    (
        Spec {
            name: "parent".into(),
            events,
            depth: 0,
            fail_init: false,
            gate: None,
            panic_drop: false,
        },
        recv,
    )
}
async fn snapshot(actor: &dyn ActorRef) -> (Option<String>, Vec<String>) {
    *actor
        .send(Box::new(Command::Snapshot))
        .await
        .unwrap()
        .downcast()
        .unwrap()
}
async fn make_child(parent: &dyn ActorRef, spec: Spec) -> BoxedActorRef {
    *parent
        .send(Box::new(Command::Spawn(spec)))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap()
}
async fn event(events: &mut mpsc::UnboundedReceiver<String>, expected: &str) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(actual) = events.recv().await {
            if actual == expected {
                return;
            }
        }
        panic!("event channel closed before {expected}");
    })
    .await
    .unwrap();
}
async fn removed(system: &System, path: &str) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while system.exists(path).await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual retirement must remove registration");
}
#[actix::test]
async fn parent_stop_joins_entire_tree_before_parent_hook_and_drop() {
    for actix in [false, true] {
        let sys = System::new(actix, Duration::from_secs(3)).await;
        let (mut spec, mut events) = spec();
        spec.depth = 2;
        let parent = sys.root(spec).await;
        let (_, children) = snapshot(&*parent).await;
        assert_eq!(children.len(), 1);
        parent.stop().await.unwrap();
        let mut observed = Vec::new();
        while let Ok(e) = events.try_recv() {
            observed.push(e);
        }
        let position = |value: &str| observed.iter().position(|e| e == value).unwrap();
        assert!(position("drop:parent-child-child") < position("stop:parent-child"));
        assert!(position("drop:parent-child") < position("stop:parent"));
        assert!(position("stop:parent") < position("drop:parent"));
        assert!(!sys.exists(&children[0]).await);
        sys.shutdown().await;
    }
}
#[actix::test]
async fn child_stop_inside_parent_handler_does_not_wait_for_parent_callback() {
    for actix in [false, true] {
        let sys = System::new(actix, Duration::from_secs(2)).await;
        let (spec, mut events) = spec();
        let child_spec = spec.child("child");
        let parent = sys.root(spec).await;
        let child = make_child(&*parent, child_spec).await;
        assert_eq!(snapshot(&*child).await.0, Some(parent.path()));
        assert_eq!(snapshot(&*parent).await.1, vec![child.path()]);
        parent
            .send_with_timeout(
                Box::new(Command::StopChild(child.clone_boxed())),
                Some(Duration::from_secs(1)),
            )
            .await
            .unwrap();
        event(&mut events, "terminated:parent").await;
        assert!(snapshot(&*parent).await.1.is_empty());
        assert!(!sys.exists(&child.path()).await);
        sys.shutdown().await;
    }
}
#[actix::test]
async fn failed_parent_initialization_cleans_initialized_children() {
    for actix in [false, true] {
        let sys = System::new(actix, Duration::from_secs(2)).await;
        let (mut spec, mut events) = spec();
        spec.depth = 1;
        spec.fail_init = true;
        let parent = sys.root(spec).await;
        event(&mut events, "init:parent").await;
        removed(&sys, &parent.path()).await;
        let mut seen = Vec::new();
        while let Ok(e) = events.try_recv() {
            seen.push(e);
        }
        assert!(seen.iter().any(|e| e == "drop:parent-child"));
        assert!(seen.iter().any(|e| e == "drop:parent"));
        sys.shutdown().await;
    }
}
#[actix::test]
async fn manual_child_metadata_does_not_take_ownership_of_another_root() {
    for actix in [false, true] {
        let sys = System::new(actix, Duration::from_secs(2)).await;
        let (spec, _) = spec();
        let outsider = sys.root(spec.child("outsider")).await;
        let parent = sys.root(spec).await;
        parent
            .send(Box::new(Command::Metadata(outsider.clone_boxed())))
            .await
            .unwrap();
        parent.stop().await.unwrap();
        snapshot(&*outsider).await;
        sys.shutdown().await;
    }
}
#[actix::test]
async fn system_drop_reclaims_children_even_with_retained_references() {
    for actix in [false, true] {
        let sys = System::new(actix, Duration::from_secs(2)).await;
        let (spec, mut events) = spec();
        let child_spec = spec.child("child");
        let parent = sys.root(spec).await;
        let child = make_child(&*parent, child_spec).await;
        snapshot(&*child).await;
        drop(sys);
        event(&mut events, "drop:child").await;
        event(&mut events, "drop:parent").await;
        assert!(child.send(Box::new(Command::Snapshot)).await.is_err());
    }
}
#[actix::test]
async fn self_stop_reclaims_children() {
    for actix in [false, true] {
        let sys = System::new(actix, Duration::from_secs(2)).await;
        let (spec, mut events) = spec();
        let child_spec = spec.child("child");
        let parent = sys.root(spec).await;
        let child = make_child(&*parent, child_spec).await;
        snapshot(&*child).await;
        let _ = parent.send(Box::new(Command::SelfStop)).await;
        event(&mut events, "drop:child").await;
        removed(&sys, &parent.path()).await;
        sys.shutdown().await;
    }
}
#[actix::test]
async fn timed_out_parent_retains_registration_until_child_destructor_finishes() {
    for actix in [false, true] {
        let sys = System::new(actix, Duration::from_millis(100)).await;
        let (spec, mut events) = spec();
        let mut child_spec = spec.child("child");
        let gate = Arc::new(DropGate::default());
        child_spec.gate = Some(gate.clone());
        let parent = sys.root(spec).await;
        let child = make_child(&*parent, child_spec).await;
        snapshot(&*child).await;
        assert!(parent.stop().await.is_err());
        let retained = sys.exists(&parent.path()).await && sys.exists(&child.path()).await;
        // Always release before asserting so an assertion cannot strand a worker.
        gate.release();
        assert!(retained, "stop deadline is not a retirement receipt");
        event(&mut events, "drop:child").await;
        removed(&sys, &parent.path()).await;
        sys.shutdown().await;
    }
}
#[actix::test]
async fn child_destructor_panic_does_not_lose_parent_cleanup_receipt() {
    for actix in [false, true] {
        let sys = System::new(actix, Duration::from_secs(2)).await;
        let (spec, mut events) = spec();
        let mut child_spec = spec.child("child");
        child_spec.panic_drop = true;
        let parent = sys.root(spec).await;
        let child = make_child(&*parent, child_spec).await;
        snapshot(&*child).await;
        assert!(parent.stop().await.is_err());
        event(&mut events, "drop:parent").await;
        removed(&sys, &child.path()).await;
        removed(&sys, &parent.path()).await;
        sys.shutdown().await;
    }
}

async fn descendant_cleanup_failure_reaches_root(actix: bool, cancel_waiter: bool) {
    struct Release(Arc<DropGate>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let sys = System::new(actix, Duration::from_secs(3)).await;
    let (spec, mut events) = spec();
    let parent_spec = spec.child("middle");
    let mut leaf_spec = spec.child("leaf");
    leaf_spec.panic_drop = true;
    let gate = Arc::new(DropGate::default());
    let release = Release(gate.clone());
    if cancel_waiter {
        leaf_spec.gate = Some(gate.clone());
    }
    let root = sys.root(spec).await;
    let parent = make_child(&*root, parent_spec).await;
    let leaf = make_child(&*parent, leaf_spec).await;
    snapshot(&*leaf).await;
    if cancel_waiter {
        let owned = root.clone_boxed();
        let waiter = tokio::spawn(async move { owned.stop().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !gate.entered.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        drop(release);
    }
    let stopped = root.stop().await;
    assert!(
        stopped.is_err(),
        "grandparent lost descendant cleanup failure: actix={actix}, cancelled={cancel_waiter}"
    );
    let message = stopped.unwrap_err().to_string();
    assert!(
        message.contains("child destructor panic"),
        "wrong descendant diagnostic: {message}"
    );
    assert!(
        root.stop().await.is_err(),
        "repeated stop lost terminal failure"
    );
    for reference in [&*leaf, &*parent, &*root] {
        removed(&sys, &reference.path()).await;
    }
    event(&mut events, "drop:parent").await;
    sys.shutdown().await;
}

#[actix::test]
async fn grandparent_stop_preserves_descendant_destructor_failure_in_both_engines() {
    for actix in [false, true] {
        descendant_cleanup_failure_reaches_root(actix, false).await;
    }
}

#[actix::test]
async fn cancelled_grandparent_waiter_cannot_erase_descendant_cleanup_failure() {
    for actix in [false, true] {
        descendant_cleanup_failure_reaches_root(actix, true).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervised_parent_replacement_retires_old_generation_children() {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let (spec, mut events) = spec();
    let child_spec = spec.child("child");
    let parent = system
        .spawn_supervised(
            move || ThreadNode(spec.clone()),
            "/parent",
            parrot::thread::config::SupervisorStrategy::Restart {
                max_retries: 1,
                within: Duration::from_secs(30),
            },
        )
        .await
        .unwrap();
    let child = make_child(&parent, child_spec).await;
    snapshot(&*child).await;
    let _ = parent.send(Box::new(Command::Crash)).await;
    event(&mut events, "drop:child").await;
    event(&mut events, "init:parent").await;
    assert!(snapshot(&parent).await.1.is_empty());
    assert!(system.get_actor_ref(&child.path()).is_none());
    system.shutdown_internal().await.unwrap();
}

struct ExportActix;
impl ActixEngineExt for ExportActix {}
impl Actor for ExportActix {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(Box::new(ctx.clone()) as BoxedMessage) })
    }
}
#[actix::test]
async fn escaped_actix_context_cannot_spawn_after_parent_retirement() {
    let sys = parrot::actix::ActixActorSystem::with_arbiter_count(1)
        .await
        .unwrap();
    let parent = sys
        .spawn_root_typed(ExportActix, EmptyConfig)
        .await
        .unwrap();
    let mut ctx = *parent
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<<ExportActix as Actor>::Context>()
        .unwrap();
    let child = ctx
        .spawner()
        .spawn_typed(ActixChild, EmptyConfig)
        .await
        .unwrap();
    check_parent(&*child, parent.path()).await;
    parent.stop().await.unwrap();
    assert!(
        ctx.spawner()
            .spawn_typed(ActixChild, EmptyConfig)
            .await
            .is_err()
    );
    assert_eq!(sys.status().active_actors, 0);
    sys.shutdown().await.unwrap();
}
#[actix::test]
async fn concurrent_spawn_and_parent_stop_leave_no_orphan() {
    let sys = parrot::actix::ActixActorSystem::with_arbiter_count(2)
        .await
        .unwrap();
    let parent = sys
        .spawn_root_typed(ExportActix, EmptyConfig)
        .await
        .unwrap();
    let ctx = *parent
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<<ExportActix as Actor>::Context>()
        .unwrap();
    let workers = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(65));
    let mut tasks = Vec::new();
    for _ in 0..64 {
        let mut ctx = ctx.clone();
        let barrier = barrier.clone();
        tasks.push(workers.spawn(async move {
            barrier.wait().await;
            ctx.spawner().spawn_typed(ActixChild, EmptyConfig).await
        }));
    }
    barrier.wait().await;
    parent.stop().await.unwrap();
    for task in tasks {
        let _ = task.await.unwrap();
    }
    workers.shutdown_background();
    assert_eq!(sys.status().active_actors, 0);
    sys.shutdown().await.unwrap();
}
#[actix::test]
async fn child_capacity_rejects_overflow_and_is_reusable_after_retirement() {
    let sys = parrot::actix::ActixActorSystem::with_arbiter_count(2)
        .await
        .unwrap();
    let parent = sys
        .spawn_root_typed(ExportActix, EmptyConfig)
        .await
        .unwrap();
    let mut ctx = *parent
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<<ExportActix as Actor>::Context>()
        .unwrap();
    let mut children = Vec::new();
    for _ in 0..1024 {
        children.push(
            ctx.spawner()
                .spawn_typed(ActixChild, EmptyConfig)
                .await
                .unwrap(),
        );
    }
    assert!(
        ctx.spawner()
            .spawn_typed(ActixChild, EmptyConfig)
            .await
            .unwrap_err()
            .to_string()
            .contains("capacity")
    );
    children.pop().unwrap().stop().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if ctx
                .spawner()
                .spawn_typed(ActixChild, EmptyConfig)
                .await
                .is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(ctx.children().unwrap().len(), 1024);
    parent.stop().await.unwrap();
    assert_eq!(sys.status().active_actors, 0);
    sys.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_ownership_works_with_all_thread_parent_schedulers() {
    use parrot::thread::config::{SchedulingMode, ThreadActorConfig};
    for mode in [
        SchedulingMode::DedicatedThread,
        SchedulingMode::SharedPool {
            max_messages_per_run: 16,
        },
        SchedulingMode::Sharded {
            affinity_key: "tree".into(),
        },
    ] {
        let sys = parrot::thread::ThreadActorSystem::shared(Default::default());
        let (spec, mut events) = spec();
        let child_spec = spec.child("child");
        let parent = sys
            .spawn_at(
                ThreadNode(spec),
                "/parent",
                None,
                ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let child = make_child(&parent, child_spec).await;
        snapshot(&*child).await;
        parent.stop().await.unwrap();
        event(&mut events, "drop:child").await;
        assert!(sys.get_actor_ref(&child.path()).is_none());
        sys.shutdown_internal().await.unwrap();
    }
}
#[actix::test]
async fn child_config_type_mismatch_never_publishes_an_actor() {
    let sys = parrot::actix::ActixActorSystem::with_arbiter_count(1)
        .await
        .unwrap();
    let parent = sys
        .spawn_root_typed(ExportActix, EmptyConfig)
        .await
        .unwrap();
    let mut ctx = *parent
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<<ExportActix as Actor>::Context>()
        .unwrap();
    let payload = <<ActixChild as Actor>::Context as parrot_api::context::ActorSpawnContext<
        ActixChild,
    >>::wrap_spawn(ActixChild);
    assert!(
        ctx.spawner()
            .spawn(payload, Box::new(12_u32))
            .await
            .is_err()
    );
    assert!(ctx.children().unwrap().is_empty());
    assert_eq!(sys.status().active_actors, 1);
    sys.shutdown().await.unwrap();
}

#[actix::test]
async fn holding_a_children_snapshot_does_not_block_spawn_or_retirement() {
    let sys = parrot::actix::ActixActorSystem::with_arbiter_count(1)
        .await
        .unwrap();
    let parent = sys
        .spawn_root_typed(ExportActix, EmptyConfig)
        .await
        .unwrap();
    let mut ctx = *parent
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<<ExportActix as Actor>::Context>()
        .unwrap();
    let snapshot = ctx.children().unwrap();
    let guard = snapshot.read_all();
    let child = ctx
        .spawner()
        .spawn_typed(ActixChild, EmptyConfig)
        .await
        .unwrap();
    assert!(guard.is_empty());
    assert_eq!(ctx.children().unwrap().len(), 1);
    child.stop().await.unwrap();
    parent.stop().await.unwrap();
    sys.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_thread_context_cannot_create_unowned_children() {
    let sys = parrot::thread::ThreadActorSystem::shared(Default::default());
    let mut ctx = parrot::thread::ThreadContext::<ThreadChild>::new_for_test_with(
        "/detached",
        Arc::downgrade(&sys),
    );
    assert!(
        ctx.spawner()
            .spawn_typed(ThreadChild, EmptyConfig)
            .await
            .is_err()
    );
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}

#[actix::test]
async fn blocked_child_cleanup_does_not_delay_parent_timer_cancellation() {
    for actix in [false, true] {
        let sys = System::new(actix, Duration::from_millis(100)).await;
        let (spec, mut events) = spec();
        let mut child_spec = spec.child("child");
        let gate = Arc::new(DropGate::default());
        child_spec.gate = Some(gate.clone());
        let parent = sys.root(spec).await;
        let child = make_child(&*parent, child_spec).await;
        snapshot(&*child).await;
        parent.send(Box::new(Command::ScheduleDrop)).await.unwrap();
        assert!(parent.stop().await.is_err());
        let cancelled = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(value) = events.recv().await {
                if value == "timer-dropped" {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        gate.release();
        assert!(
            cancelled,
            "parent tasks must close while child disposal remains blocked"
        );
        removed(&sys, &parent.path()).await;
        sys.shutdown().await;
    }
}

mod native_descendant_sources {
    use super::*;
    use std::{error::Error, sync::atomic::AtomicUsize};
    struct State {
        factory: bool,
        fail: bool,
        hostile: bool,
        formats: AtomicUsize,
        sources: AtomicUsize,
        drops: AtomicUsize,
        leaf_hooks: AtomicUsize,
        entered: tokio::sync::Notify,
        gate: tokio::sync::Semaphore,
    }
    struct Business(Arc<State>);
    impl std::fmt::Debug for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("NativeDescendantBusiness")
        }
    }
    impl std::fmt::Display for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.formats.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.0.hostile,
                "native descendant Display must not execute during retirement"
            );
            f.write_str("native descendant original failure")
        }
    }
    impl Error for Business {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.sources.fetch_add(1, Ordering::SeqCst);
            panic!("framework must not traverse native business sources");
        }
    }
    struct ThreadTree {
        state: Arc<State>,
        depth: usize,
    }
    struct ActixTree {
        state: Arc<State>,
        depth: usize,
    }
    impl ActixEngineExt for ActixTree {}
    macro_rules! tree {
        ($name:ident, $context:ty) => {
            impl Drop for $name {
                fn drop(&mut self) {
                    self.state.drops.fetch_add(1, Ordering::SeqCst);
                }
            }
            impl Actor for $name {
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
                    context: &'a mut Self::Context,
                ) -> BoxedFuture<'a, ActorResult<()>> {
                    Box::pin(async move {
                        if self.depth > 0 {
                            let state = self.state.clone();
                            let depth = self.depth - 1;
                            let child = if self.state.factory {
                                context
                                    .spawner()
                                    .spawn_factory(move || Self {
                                        state: state.clone(),
                                        depth,
                                    })
                                    .await?
                            } else {
                                context
                                    .spawner()
                                    .spawn_typed(Self { state, depth }, EmptyConfig)
                                    .await?
                            };
                            child.send(Box::new(())).await?;
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
                fn before_stop<'a>(
                    &'a mut self,
                    _: &'a mut Self::Context,
                ) -> BoxedFuture<'a, ActorResult<()>> {
                    Box::pin(async move {
                        if self.depth == 0 {
                            self.state.leaf_hooks.fetch_add(1, Ordering::SeqCst);
                            self.state.entered.notify_one();
                            let _permit = self.state.gate.acquire().await.unwrap();
                            if self.state.fail {
                                return Err(ActorError::Other(anyhow::Error::new(Business(
                                    self.state.clone(),
                                ))));
                            }
                        }
                        Ok(())
                    })
                }
            }
        };
    }
    tree!(ThreadTree, parrot::thread::ThreadContext<Self>);
    tree!(
        ActixTree,
        parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
    );
    fn original(error: &ActorError, state: &Arc<State>) {
        let mut source = Some(error as &(dyn Error + 'static));
        for _ in 0..128 {
            let Some(error) = source else { break };
            if let Some(business) = error.downcast_ref::<Business>() {
                assert!(Arc::ptr_eq(&business.0, state));
                return;
            }
            source = error.source();
        }
        panic!("native descendant business source/identity was replaced by text");
    }
    async fn run(engine: usize, factory: bool, hostile: bool, fail: bool) {
        run_with_owner(engine, factory, hostile, fail, false).await;
    }
    async fn run_with_owner(
        engine: usize,
        factory: bool,
        hostile: bool,
        fail: bool,
        system_stop: bool,
    ) {
        let state = Arc::new(State {
            factory,
            fail,
            hostile,
            formats: AtomicUsize::new(0),
            sources: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            leaf_hooks: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            gate: tokio::sync::Semaphore::new(0),
        });
        let mut thread = None;
        let mut actix = None;
        let root: BoxedActorRef = if engine == 3 {
            let system = parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap();
            let actor = system
                .spawn_root_typed(
                    ActixTree {
                        state: state.clone(),
                        depth: 3,
                    },
                    EmptyConfig,
                )
                .await
                .unwrap();
            actix = Some(system);
            actor
        } else {
            let mode = match engine {
                0 => parrot::thread::SchedulingMode::SharedPool {
                    max_messages_per_run: 8,
                },
                1 => parrot::thread::SchedulingMode::DedicatedThread,
                _ => parrot::thread::SchedulingMode::Sharded {
                    affinity_key: "native-source".into(),
                },
            };
            let system = parrot::thread::ThreadActorSystem::shared(
                parrot::thread::ThreadActorSystemConfig {
                    default_scheduling_mode: mode,
                    ..Default::default()
                },
            );
            let actor = system
                .spawn_at(
                    ThreadTree {
                        state: state.clone(),
                        depth: 3,
                    },
                    "/native-source-tree",
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
            thread = Some(system);
            Box::new(actor)
        };
        root.send(Box::new(())).await.unwrap();
        let late_reference = root.clone_boxed();
        let result = if system_stop {
            let system = thread.as_ref().unwrap();
            let path = root.path();
            let mut stop = Box::pin(system.stop_actor(&path));
            assert!(futures::poll!(&mut stop).is_pending());
            tokio::time::timeout(Duration::from_secs(3), state.entered.notified())
                .await
                .unwrap();
            state.gate.add_permits(1);
            tokio::time::timeout(Duration::from_secs(3), stop)
                .await
                .unwrap()
                .map_err(ActorError::from)
        } else {
            let mut cancelled = root.stop();
            let mut waiter = root.stop();
            assert!(futures::poll!(&mut cancelled).is_pending());
            assert!(futures::poll!(&mut waiter).is_pending());
            tokio::time::timeout(Duration::from_secs(3), state.entered.notified())
                .await
                .unwrap();
            drop(cancelled);
            state.gate.add_permits(1);
            tokio::time::timeout(Duration::from_secs(3), waiter)
                .await
                .unwrap()
        };
        let late = late_reference.stop().await;
        assert_eq!(state.leaf_hooks.load(Ordering::SeqCst), 1);
        assert_eq!(state.drops.load(Ordering::SeqCst), 4);
        if let Some(system) = thread {
            assert_eq!(system.actor_count(), 0);
            system.shutdown_internal().await.unwrap();
        }
        if let Some(system) = actix {
            system.shutdown().await.unwrap();
        }
        if fail {
            original(
                result
                    .as_ref()
                    .expect_err("root lost native descendant failure"),
                &state,
            );
            original(
                late.as_ref()
                    .expect_err("late waiter lost native descendant failure"),
                &state,
            );
        } else {
            assert!(result.is_ok());
            assert!(late.is_ok());
        }
        assert_eq!(state.formats.load(Ordering::SeqCst), 0);
        assert_eq!(state.sources.load(Ordering::SeqCst), 0);
    }
    #[actix::test]
    async fn shared_instance_source() {
        run(0, false, false, true).await;
    }
    #[actix::test]
    async fn shared_instance_hostile() {
        run(0, false, true, true).await;
    }
    #[actix::test]
    async fn shared_instance_healthy() {
        run(0, false, false, false).await;
    }
    #[actix::test]
    async fn shared_factory_source() {
        run(0, true, false, true).await;
    }
    #[actix::test]
    async fn shared_factory_hostile() {
        run(0, true, true, true).await;
    }
    #[actix::test]
    async fn shared_factory_healthy() {
        run(0, true, false, false).await;
    }
    #[actix::test]
    async fn dedicated_instance_source() {
        run(1, false, false, true).await;
    }
    #[actix::test]
    async fn dedicated_instance_hostile() {
        run(1, false, true, true).await;
    }
    #[actix::test]
    async fn dedicated_instance_healthy() {
        run(1, false, false, false).await;
    }
    #[actix::test]
    async fn dedicated_factory_source() {
        run(1, true, false, true).await;
    }
    #[actix::test]
    async fn dedicated_factory_hostile() {
        run(1, true, true, true).await;
    }
    #[actix::test]
    async fn dedicated_factory_healthy() {
        run(1, true, false, false).await;
    }
    #[actix::test]
    async fn sharded_instance_source() {
        run(2, false, false, true).await;
    }
    #[actix::test]
    async fn sharded_instance_hostile() {
        run(2, false, true, true).await;
    }
    #[actix::test]
    async fn sharded_instance_healthy() {
        run(2, false, false, false).await;
    }
    #[actix::test]
    async fn sharded_factory_source() {
        run(2, true, false, true).await;
    }
    #[actix::test]
    async fn sharded_factory_hostile() {
        run(2, true, true, true).await;
    }
    #[actix::test]
    async fn sharded_factory_healthy() {
        run(2, true, false, false).await;
    }
    #[actix::test]
    async fn actix_instance_source() {
        run(3, false, false, true).await;
    }
    #[actix::test]
    async fn actix_instance_hostile() {
        run(3, false, true, true).await;
    }
    #[actix::test]
    async fn actix_instance_healthy() {
        run(3, false, false, false).await;
    }
    #[actix::test]
    async fn actix_factory_source() {
        run(3, true, false, true).await;
    }
    #[actix::test]
    async fn actix_factory_hostile() {
        run(3, true, true, true).await;
    }
    #[actix::test]
    async fn actix_factory_healthy() {
        run(3, true, false, false).await;
    }
    #[actix::test]
    async fn shared_system_stop_healthy() {
        run_with_owner(0, false, false, false, true).await;
    }
    #[actix::test]
    async fn shared_system_stop_source() {
        run_with_owner(0, false, false, true, true).await;
    }
    #[actix::test]
    async fn dedicated_system_stop_healthy() {
        run_with_owner(1, false, false, false, true).await;
    }
    #[actix::test]
    async fn dedicated_system_stop_source() {
        run_with_owner(1, false, false, true, true).await;
    }
    #[actix::test]
    async fn sharded_system_stop_healthy() {
        run_with_owner(2, false, false, false, true).await;
    }
    #[actix::test]
    async fn sharded_system_stop_source() {
        run_with_owner(2, false, false, true, true).await;
    }
}
