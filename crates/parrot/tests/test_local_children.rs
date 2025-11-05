//! Real parent scopes own non-Send children, including failed publication.
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    typed_context::TypedContextExt,
};
use parrot_api::{
    Message, ParrotTypedActor,
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalTypedActorRef, LocalTypedLifecycle, LocalTypedReceive},
    supervisor::{
        BasicDecisionFn, DefaultStrategy, OneForOneStrategy, SupervisionDecision,
        SupervisorStrategyType,
    },
    typed::TypedReceive,
    typed_context::TypedContext,
    types::{ActorResult, BoxedFuture},
};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::ThreadId,
    time::Duration,
};

type Events = Arc<Mutex<Vec<(String, &'static str, ThreadId)>>>;
fn event(events: &Events, label: &str, phase: &'static str) {
    events
        .lock()
        .unwrap()
        .push((label.into(), phase, std::thread::current().id()));
}
pub struct Info;
impl Message for Info {
    type Result = (usize, TypedContext);
}
pub struct Fail;
impl Message for Fail {
    type Result = ();
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Info, Fail), local, lifecycle)]
struct Node {
    state: Rc<Cell<usize>>,
    events: Events,
    label: String,
    drop_fails: bool,
}
impl Node {
    fn new(events: Events, label: &str) -> Self {
        event(&events, label, "construct");
        Self {
            state: Rc::new(Cell::new(0)),
            events,
            label: label.into(),
            drop_fails: false,
        }
    }
}
impl LocalTypedLifecycle for Node {
    fn init(&mut self) -> LocalBoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            event(&self.events, &self.label, "init");
            Ok(())
        })
    }
    fn before_stop_with_context<'a>(
        &'a mut self,
        ctx: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            assert!(ctx.children().is_empty());
            event(&self.events, &self.label, "stop");
            Ok(())
        })
    }
}
impl LocalTypedReceive<Info> for Node {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Info,
        ctx: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<(usize, TypedContext)>> {
        let state = self.state.clone();
        Box::pin(async move {
            tokio::task::yield_now().await;
            event(&self.events, &self.label, "message");
            Ok((state.get(), ctx.clone()))
        })
    }
}
impl LocalTypedReceive<Fail> for Node {
    fn receive_typed(&mut self, _: Fail) -> LocalBoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.state.set(self.state.get() + 1);
            Err(ActorError::MessageHandlingError(
                "local-child-business".into(),
            ))
        })
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        assert_eq!(Rc::strong_count(&self.state), 1);
        event(&self.events, &self.label, "drop");
        assert!(!self.drop_fails, "local-child-drop-failure");
    }
}
type Ref = LocalTypedActorRef<Node, Info>;
fn strategy(value: DefaultStrategy) -> SupervisorStrategyType {
    SupervisorStrategyType::Default(value)
}
async fn setup(timeout: Duration) -> (Arc<ActixActorSystem>, Ref, TypedContext, Events) {
    let system = Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 2,
            shutdown_timeout: timeout,
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let events = Events::default();
    let made = events.clone();
    let parent = system
        .spawn_local_typed_with_strategy(
            move || Node::new(made.clone(), "parent"),
            "/local-parent",
            strategy(DefaultStrategy::StopOnFailure),
        )
        .await
        .unwrap();
    let (_, ctx) = parent.ask(Info).await.unwrap();
    (system, parent, ctx, events)
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}
fn phases(events: &Events, phase: &str) -> Vec<String> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.1 == phase)
        .map(|e| e.0.clone())
        .collect()
}
fn check_owners(events: &Events, caller: ThreadId) {
    let entries = events.lock().unwrap();
    for label in entries.iter().map(|e| &e.0) {
        let owner = entries.iter().find(|e| &e.0 == label).unwrap().2;
        assert_ne!(owner, caller);
        assert!(
            entries
                .iter()
                .filter(|e| &e.0 == label)
                .all(|e| e.2 == owner)
        );
    }
}
#[actix::test]
async fn local_children_and_grandchildren_are_real_parent_owned_generations() {
    let caller = std::thread::current().id();
    let (system, parent, ctx, events) = setup(Duration::from_secs(3)).await;
    let made = events.clone();
    let child: Ref = ctx
        .spawn_local_typed_child(move || Node::new(made.clone(), "child"))
        .await
        .unwrap();
    let (_, child_ctx) = child.ask(Info).await.unwrap();
    assert_eq!(child_ctx.parent().unwrap().path(), parent.path());
    assert!(child.path().starts_with(&format!("{}/", parent.path())));
    assert_eq!(ctx.children().len(), 1);
    let made = events.clone();
    let grandchild: Ref = child_ctx
        .spawn_local_typed_child(move || Node::new(made.clone(), "grandchild"))
        .await
        .unwrap();
    assert_eq!(
        grandchild
            .ask(Info)
            .await
            .unwrap()
            .1
            .parent()
            .unwrap()
            .path(),
        child.path()
    );
    parent.stop().await.unwrap();
    assert!(!child.is_alive() && !grandchild.is_alive());
    assert!(ctx.children().is_empty() && child_ctx.children().is_empty());
    assert_eq!(system.status().active_actors, 0);
    system.as_ref().clone().shutdown().await.unwrap();
    assert_eq!(phases(&events, "drop"), ["grandchild", "child", "parent"]);
    assert_eq!(phases(&events, "stop"), ["grandchild", "child", "parent"]);
    check_owners(&events, caller);
}

struct Capture {
    drops: Arc<AtomicUsize>,
    panic: bool,
}
impl Capture {
    fn touch(&self) {
        std::hint::black_box(self);
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "local-input-drop-failure");
    }
}
fn counted_policy(capture: Capture) -> SupervisorStrategyType {
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(3),
        decider: BasicDecisionFn::new(move |_| {
            capture.touch();
            SupervisionDecision::Stop
        }),
    })
}
#[actix::test]
async fn unpolled_local_child_owns_constructor_and_policy_without_constructing() {
    let (system, parent, ctx, events) = setup(Duration::from_secs(3)).await;
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture {
        drops: drops.clone(),
        panic: false,
    };
    let policy = counted_policy(Capture {
        drops: drops.clone(),
        panic: false,
    });
    let made = events.clone();
    let not_sync = Cell::new(0);
    let pending = ctx.spawn_local_typed_child_with_strategy::<Node, Info, _>(
        move || {
            capture.touch();
            not_sync.set(not_sync.get() + 1);
            Node::new(made.clone(), "must-not-build")
        },
        policy,
    );
    drop(pending);
    until(|| drops.load(Ordering::SeqCst) == 2).await;
    assert!(ctx.children().is_empty());
    assert_eq!(phases(&events, "construct"), ["parent"]);
    parent.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn closed_parent_rejects_child_and_joins_both_input_destructors() {
    let (system, parent, ctx, events) = setup(Duration::from_secs(3)).await;
    parent.stop().await.unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture {
        drops: drops.clone(),
        panic: false,
    };
    let made = events.clone();
    let result = ctx
        .spawn_local_typed_child_with_strategy::<Node, Info, _>(
            move || {
                capture.touch();
                Node::new(made.clone(), "rejected")
            },
            counted_policy(Capture {
                drops: drops.clone(),
                panic: false,
            }),
        )
        .await;
    assert!(matches!(result, Err(ActorError::Stopped)));
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(phases(&events, "construct"), ["parent"]);
    system.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn rejection_disposal_error_is_reported_without_skipping_other_input() {
    let (system, parent, ctx, _) = setup(Duration::from_secs(3)).await;
    parent.stop().await.unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture {
        drops: drops.clone(),
        panic: true,
    };
    let error = ctx
        .spawn_local_typed_child_with_strategy::<Node, Info, _>(
            move || {
                capture.touch();
                panic!("must not construct")
            },
            counted_policy(Capture {
                drops: drops.clone(),
                panic: false,
            }),
        )
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("local-input-drop-failure"));
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    system.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn child_inherits_live_parent_policy_and_override_remains_independent() {
    let (system, parent, ctx, events) = setup(Duration::from_secs(3)).await;
    ctx.set_child_strategy(strategy(DefaultStrategy::ResumeOnFailure));
    let made = events.clone();
    let child: Ref = ctx
        .spawn_local_typed_child(move || Node::new(made.clone(), "resumed"))
        .await
        .unwrap();
    child.ask(Info).await.unwrap();
    assert!(child.ref_for::<Fail>().ask(Fail).await.is_err());
    assert_eq!(child.ask(Info).await.unwrap().0, 1);
    let made = events.clone();
    let stopped: Ref = ctx
        .spawn_local_typed_child_with_strategy(
            move || Node::new(made.clone(), "stopped"),
            strategy(DefaultStrategy::StopOnFailure),
        )
        .await
        .unwrap();
    stopped.ask(Info).await.unwrap();
    assert!(stopped.ref_for::<Fail>().ask(Fail).await.is_err());
    until(|| ctx.children().len() == 1).await;
    assert!(
        stopped
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("local-child-business")
    );
    assert_eq!(child.ask(Info).await.unwrap().0, 1);
    ctx.set_child_strategy(strategy(DefaultStrategy::StopOnFailure));
    assert!(child.ref_for::<Fail>().ask(Fail).await.is_err());
    until(|| ctx.children().is_empty()).await;
    parent.ask(Info).await.unwrap();
    parent.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn local_child_escalation_reaches_actual_parent() {
    let (system, parent, ctx, events) = setup(Duration::from_secs(3)).await;
    let made = events.clone();
    let child: Ref = ctx
        .spawn_local_typed_child_with_strategy(
            move || Node::new(made.clone(), "escalated"),
            strategy(DefaultStrategy::EscalateFailure),
        )
        .await
        .unwrap();
    child.ask(Info).await.unwrap();
    assert!(child.ref_for::<Fail>().ask(Fail).await.is_err());
    until(|| system.status().active_actors == 0).await;
    assert!(!parent.is_alive());
    assert!(
        parent
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("local-child-business")
    );
    assert_eq!(phases(&events, "drop"), ["escalated", "parent"]);
    system.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn child_cleanup_failure_prevents_successful_parent_stop() {
    let (system, parent, ctx, events) = setup(Duration::from_secs(3)).await;
    let made = events.clone();
    let child: Ref = ctx
        .spawn_local_typed_child(move || {
            let mut node = Node::new(made.clone(), "failed-cleanup");
            node.drop_fails = true;
            node
        })
        .await
        .unwrap();
    child.ask(Info).await.unwrap();
    assert!(
        parent
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("local-child-drop-failure")
    );
    assert_eq!(phases(&events, "drop"), ["failed-cleanup", "parent"]);
    assert_eq!(phases(&events, "stop"), ["failed-cleanup"]);
    assert_eq!(system.status().active_actors, 0);
    system.as_ref().clone().shutdown().await.unwrap();
}
#[actix::test]
async fn retired_constructor_failure_is_reported_by_child_without_poisoning_later_parent_stop() {
    let (system, parent, ctx, events) = setup(Duration::from_secs(3)).await;
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture {
        drops: drops.clone(),
        panic: true,
    };
    let made = events.clone();
    let child: Ref = ctx
        .spawn_local_typed_child(move || {
            capture.touch();
            Node::new(made.clone(), "capture-failed")
        })
        .await
        .unwrap();
    assert!(child.ask(Info).await.is_err());
    assert!(
        child
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("local-input-drop-failure")
    );
    // This child has already retired independently. A later parent stop must
    // not retroactively inherit an earlier child's failure.
    assert!(ctx.children().is_empty());
    parent.stop().await.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(phases(&events, "init"), ["parent"]);
    assert_eq!(phases(&events, "drop"), ["capture-failed", "parent"]);
    system.as_ref().clone().shutdown().await.unwrap();
}

struct Gate {
    released: Mutex<bool>,
    changed: Condvar,
    entered: tokio::sync::Notify,
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            released: Mutex::new(false),
            changed: Condvar::new(),
            entered: tokio::sync::Notify::new(),
        })
    }
    fn block(&self) {
        self.entered.notify_one();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.changed.wait(released).unwrap();
        }
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
#[actix::test]
async fn parent_timeout_does_not_abandon_child_blocked_in_local_constructor() {
    let (system, parent, ctx, events) = setup(Duration::from_millis(150)).await;
    let gate = Gate::new();
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let made = events.clone();
    let child: Ref = ctx
        .spawn_local_typed_child(move || {
            blocked.block();
            Node::new(made.clone(), "blocked")
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    assert!(matches!(
        parent.stop().await,
        Err(ActorError::TimeoutDetail(_))
    ));
    assert_eq!(system.status().active_actors, 2);
    assert_eq!(ctx.children().len(), 1);
    assert!(phases(&events, "drop").is_empty());
    assert!(
        tokio::time::timeout(
            Duration::from_millis(350),
            until(|| { phases(&events, "drop").contains(&"parent".to_owned()) })
        )
        .await
        .is_err(),
        "parent retired while child constructor still owned resources"
    );
    assert_eq!(system.status().active_actors, 2);
    assert_eq!(ctx.children().len(), 1);
    gate.release();
    until(|| system.status().active_actors == 0).await;
    assert!(!child.is_alive());
    assert!(ctx.children().is_empty());
    assert_eq!(phases(&events, "drop"), ["blocked", "parent"]);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Info, Fail))]
struct SendParent;
impl TypedReceive<Info> for SendParent {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Info,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<(usize, TypedContext)>> {
        Box::pin(async move { Ok((0, ctx.clone())) })
    }
}
impl TypedReceive<Fail> for SendParent {
    fn receive_typed(&mut self, _: Fail) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Err(ActorError::MessageHandlingError("restart-parent".into())) })
    }
}
#[actix::test]
async fn restarted_send_parent_closes_old_local_child_scope() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(2).await.unwrap());
    let parent = system
        .spawn_typed_factory_with_strategy::<SendParent, Info, _>(
            || SendParent,
            "/restart-parent",
            OneForOneStrategy {
                max_restarts: 3,
                within: Duration::from_secs(5),
                decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
            },
        )
        .await
        .unwrap();
    let (_, old) = parent.ask(Info).await.unwrap();
    let events = Events::default();
    let made = events.clone();
    let child: Ref = old
        .spawn_local_typed_child(move || Node::new(made.clone(), "old-child"))
        .await
        .unwrap();
    child.ask(Info).await.unwrap();
    assert!(parent.ref_for::<Fail>().ask(Fail).await.is_err());
    let (_, new) = parent.ask(Info).await.unwrap();
    assert!(!child.is_alive());
    assert!(
        old.spawn_local_typed_child::<Node, Info, _>(|| panic!("stale context constructed actor"))
            .await
            .is_err()
    );
    let made = events.clone();
    let replacement: Ref = new
        .spawn_local_typed_child(move || Node::new(made.clone(), "new-child"))
        .await
        .unwrap();
    replacement.ask(Info).await.unwrap();
    parent.stop().await.unwrap();
    assert!(!replacement.is_alive());
    assert_eq!(phases(&events, "drop"), ["old-child", "new-child"]);
    system.as_ref().clone().shutdown().await.unwrap();
}
#[tokio::test]
async fn thread_context_rejects_local_execution_and_retires_captures() {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let parent = system
        .spawn_typed::<SendParent, Info>(SendParent, "/thread-parent")
        .await
        .unwrap();
    let (_, ctx) = parent.ask(Info).await.unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture {
        drops: drops.clone(),
        panic: false,
    };
    let error = ctx
        .spawn_local_typed_child::<Node, Info, _>(move || {
            capture.touch();
            panic!("unsupported backend constructed actor")
        })
        .await
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("no Local child execution backend")
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(ctx.children().is_empty());
    parent.stop().await.unwrap();
    system.shutdown_internal().await.unwrap();
}

struct BlockedCapture {
    gate: Arc<Gate>,
    drops: Arc<AtomicUsize>,
}
impl Drop for BlockedCapture {
    fn drop(&mut self) {
        self.gate.block();
        self.drops.fetch_add(1, Ordering::SeqCst);
        panic!("blocked-capture-cleanup-failed");
    }
}
#[actix::test]
async fn parent_retirement_joins_pending_constructor_capture_failure() {
    let (system, parent, ctx, events) = setup(Duration::from_secs(3)).await;
    let gate = Gate::new();
    let _release = Release(gate.clone());
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = BlockedCapture {
        gate: gate.clone(),
        drops: drops.clone(),
    };
    let made = events.clone();
    let child: Ref = ctx
        .spawn_local_typed_child(move || {
            std::hint::black_box(&capture);
            Node::new(made.clone(), "pending-capture")
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    // Synchronously close the actual parent generation before the cleanup owner
    // publishes its failure; no sleep or scheduler race determines this test.
    ctx.stop();
    gate.release();
    let error = parent.stop().await.unwrap_err();
    assert!(error.to_string().contains("blocked-capture-cleanup-failed"));
    assert!(
        child
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("blocked-capture-cleanup-failed")
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(phases(&events, "init"), ["parent"]);
    assert_eq!(phases(&events, "drop"), ["pending-capture", "parent"]);
    assert!(phases(&events, "stop").is_empty());
    assert!(ctx.children().is_empty());
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn one_shot_local_child_reports_restart_unavailable_without_reconstruction() {
    let (system, parent, ctx, events) = setup(Duration::from_secs(3)).await;
    ctx.set_child_strategy(strategy(DefaultStrategy::RestartOnFailure));
    let made = events.clone();
    let child: Ref = ctx
        .spawn_local_typed_child(move || Node::new(made.clone(), "one-shot"))
        .await
        .unwrap();
    child.ask(Info).await.unwrap();
    assert!(child.ref_for::<Fail>().ask(Fail).await.is_err());
    until(|| ctx.children().is_empty()).await;
    assert!(
        child
            .stop()
            .await
            .unwrap_err()
            .to_string()
            .contains("without a repeatable factory")
    );
    assert_eq!(phases(&events, "construct"), ["parent", "one-shot"]);
    parent.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}
struct Foreign;
impl parrot_api::typed_context::TypedContextServices for Foreign {
    fn path(&self) -> &str {
        "/foreign"
    }
    fn actor_ref(&self) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
    fn parent(&self) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
    fn children(&self) -> parrot_api::context::ReadOnlyChildrenVec {
        parrot_api::context::ReadOnlyChildrenVec::new(Arc::new(std::sync::RwLock::new(vec![])))
    }
    fn stop(&self) {}
    fn set_child_strategy(&self, _: SupervisorStrategyType) {}
}
#[test]
fn foreign_context_without_runtime_rejects_local_and_disposes_both_inputs() {
    let ctx = TypedContext::from_services(Arc::new(Foreign));
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture {
        drops: drops.clone(),
        panic: true,
    };
    let policy = counted_policy(Capture {
        drops: drops.clone(),
        panic: false,
    });
    let result =
        futures::executor::block_on(ctx.spawn_local_typed_child_with_strategy::<Node, Info, _>(
            move || {
                capture.touch();
                panic!("foreign context constructed actor")
            },
            policy,
        ));
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("local-input-drop-failure")
    );
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    let result =
        futures::executor::block_on(ctx.spawn_local_typed_child::<Node, Info, _>(|| {
            panic!("foreign context constructed actor")
        }));
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("no Local child execution backend")
    );
}
