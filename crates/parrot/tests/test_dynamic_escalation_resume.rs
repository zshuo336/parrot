use futures::FutureExt;
// A dynamic parent's escalation decision must precede destroying its live state.
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorSpawnerExt},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
#[derive(Default)]
struct Stats {
    initialization_error: Mutex<Option<ActorError>>,
    cleanup_error: Mutex<Option<ActorError>>,
    built: AtomicUsize,
    inits: AtomicUsize,
    hooks: AtomicUsize,
    drops: AtomicUsize,
    decisions: AtomicUsize,
    sync_calls: AtomicUsize,
    batches: AtomicUsize,
    batch_width: AtomicUsize,
    sync: AtomicBool,
    spawn_in_init: AtomicBool,
    init_hold: Mutex<Option<Arc<Hold>>>,
    init_child: Mutex<Option<BoxedActorRef>>,
}
struct Make {
    stats: Arc<Stats>,
    factory: bool,
    policy: SupervisorStrategyType,
}
struct Fail;
struct Query;
macro_rules! node {
    ($name:ident,$context:ty) => {
        struct $name {
            stats: Arc<Stats>,
            value: usize,
        }
        impl $name {
            fn new(stats: Arc<Stats>) -> Self {
                stats.built.fetch_add(1, Ordering::SeqCst);
                Self { stats, value: 0 }
            }
        }
        impl Drop for $name {
            fn drop(&mut self) {
                self.stats.drops.fetch_add(1, Ordering::SeqCst);
            }
        }
        impl Actor for $name {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                !self.stats.sync.load(Ordering::SeqCst)
            }
            fn init<'a>(
                &'a mut self,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    self.stats.inits.fetch_add(1, Ordering::SeqCst);
                    if let Some(error) = self.stats.initialization_error.lock().unwrap().take() {
                        return Err(error);
                    }
                    self.value = 10;
                    if self.stats.spawn_in_init.load(Ordering::SeqCst) {
                        let child = ctx
                            .spawner()
                            .spawn_supervised(
                                Self::new(Arc::new(Stats::default())),
                                EmptyConfig,
                                escalation_policy(),
                            )
                            .await?;
                        *self.stats.init_child.lock().unwrap() = Some(child);
                    }
                    let hold = self.stats.init_hold.lock().unwrap().clone();
                    if let Some(hold) = hold {
                        hold.wait().await;
                        self.value += 10;
                    }
                    Ok(())
                })
            }
            fn before_stop<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    self.stats.hooks.fetch_add(1, Ordering::SeqCst);
                    self.stats
                        .cleanup_error
                        .lock()
                        .unwrap()
                        .take()
                        .map_or(Ok(()), Err)
                })
            }
            fn can_batch_message(&self, message: &BoxedMessage) -> bool {
                message.is::<BatchHold>()
            }
            fn receive_batch<'a>(
                &'a mut self,
                messages: Vec<BoxedMessage>,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
                Box::pin(async move {
                    let width = messages.len();
                    self.stats.batches.fetch_add(1, Ordering::SeqCst);
                    self.stats.batch_width.store(width, Ordering::SeqCst);
                    let hold = messages[0].downcast_ref::<BatchHold>().unwrap().0.clone();
                    assert!(
                        messages
                            .iter()
                            .all(|m| Arc::ptr_eq(&m.downcast_ref::<BatchHold>().unwrap().0, &hold))
                    );
                    self.value += width;
                    hold.wait().await;
                    self.value += 1000;
                    (0..width)
                        .map(|_| Ok(Box::new(self.value) as BoxedMessage))
                        .collect()
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    if msg.is::<initialization_sources::TryMake>() {
                        let m = msg.downcast::<initialization_sources::TryMake>().unwrap().0;
                        let result = if m.factory {
                            ctx.spawner()
                                .spawn_factory_with_strategy(
                                    move || Self::new(m.stats.clone()),
                                    m.policy,
                                )
                                .await
                        } else {
                            ctx.spawner()
                                .spawn_supervised(Self::new(m.stats), EmptyConfig, m.policy)
                                .await
                        };
                        return Ok(Box::new(result) as BoxedMessage);
                    }
                    if msg.is::<Make>() {
                        let m = *msg.downcast::<Make>().unwrap();
                        self.value += 1;
                        let child = if m.factory {
                            ctx.spawner()
                                .spawn_factory_with_strategy(
                                    move || Self::new(m.stats.clone()),
                                    m.policy,
                                )
                                .await?
                        } else {
                            ctx.spawner()
                                .spawn_supervised(Self::new(m.stats), EmptyConfig, m.policy)
                                .await?
                        };
                        return Ok(Box::new(child) as BoxedMessage);
                    }
                    if msg.is::<ParentPath>() {
                        return Ok(Box::new(ctx.parent().map(|p| p.path())) as BoxedMessage);
                    }
                    if msg.is::<OwnFail>() {
                        self.value += 500;
                        return Err(ActorError::MessageHandlingError("parent-message".into()));
                    }
                    if msg.is::<HoldMessage>() {
                        self.value += 100;
                        msg.downcast::<HoldMessage>().unwrap().0.wait().await;
                        self.value += 1000;
                        return Ok(Box::new(self.value) as BoxedMessage);
                    }
                    if msg.is::<FailLabel>() {
                        return Err(ActorError::MessageHandlingError(
                            msg.downcast::<FailLabel>().unwrap().0.into(),
                        ));
                    }
                    if msg.is::<native_sources::Failure>() {
                        return Err(msg.downcast::<native_sources::Failure>().unwrap().0);
                    }
                    if msg.is::<Fail>() {
                        self.value += 100;
                        return Err(ActorError::MessageHandlingError("child-escalation".into()));
                    }
                    assert!(msg.is::<Query>());
                    Ok(Box::new(self.value) as BoxedMessage)
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
impl ActixEngineExt for ThreadNode {}
impl ActixEngineExt for ActixNode {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
        mut engine: parrot_api::actor::EngineContextHandle<'_>,
    ) -> Option<ActorResult<BoxedMessage>> {
        assert!(
            engine
                .downcast_mut::<actix::Context<parrot::actix::ActixActor<Self>>>()
                .is_some()
        );
        assert!(msg.is::<Query>());
        self.stats.sync_calls.fetch_add(1, Ordering::SeqCst);
        Some(Ok(Box::new(self.value)))
    }
}
enum Engine {
    Thread(Arc<parrot::thread::ThreadActorSystem>),
    Actix(parrot::actix::ActixActorSystem),
}
impl Engine {
    async fn new(actix: bool) -> Self {
        Self::with_settings(actix, 0, Duration::from_secs(3)).await
    }
    async fn with_settings(actix: bool, mode: usize, timeout: Duration) -> Self {
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
                parrot::thread::ThreadActorSystemConfig {
                    shared_pool_size: 2,
                    shared_burst_workers_max: 0,
                    default_scheduling_mode: match mode {
                        0 => parrot::thread::SchedulingMode::SharedPool {
                            max_messages_per_run: 4,
                        },
                        1 => parrot::thread::SchedulingMode::DedicatedThread,
                        _ => parrot::thread::SchedulingMode::Sharded {
                            affinity_key: "resume".into(),
                        },
                    },
                    shutdown_timeout: timeout,
                    ..Default::default()
                },
            ))
        }
    }
    async fn root(&self) -> BoxedActorRef {
        let stats = Arc::new(Stats::default());
        match self {
            Self::Thread(s) => Box::new(
                s.spawn_at(ThreadNode::new(stats), "/root", None, Default::default())
                    .await
                    .unwrap(),
            ),
            Self::Actix(s) => s
                .spawn_root_typed(ActixNode::new(stats), EmptyConfig)
                .await
                .unwrap(),
        }
    }
    fn count(&self) -> usize {
        match self {
            Self::Thread(s) => s.actor_count(),
            Self::Actix(s) => s.status().active_actors,
        }
    }
    async fn shutdown(self) {
        match self {
            Self::Thread(s) => {
                assert_eq!(s.actor_count(), 0);
                s.shutdown_internal().await.unwrap();
            }
            Self::Actix(s) => {
                assert_eq!(s.status().active_actors, 0);
                s.shutdown().await.unwrap();
            }
        }
    }
}
fn policy(decider: BasicDecisionFn) -> SupervisorStrategyType {
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(30),
        decider,
    })
}
async fn make(
    parent: &BoxedActorRef,
    stats: Arc<Stats>,
    factory: bool,
    policy: SupervisorStrategyType,
) -> BoxedActorRef {
    *parent
        .send(Box::new(Make {
            stats,
            factory,
            policy,
        }))
        .await
        .unwrap()
        .downcast::<BoxedActorRef>()
        .unwrap()
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn idle_resume(actix: bool, factory: bool) {
    let e = Engine::new(actix).await;
    let root = e.root().await;
    let stats = Arc::new(Stats::default());
    let calls = stats.clone();
    let parent = make(
        &root,
        stats.clone(),
        factory,
        policy(BasicDecisionFn::new(move |e| {
            assert!(matches!(e,ActorError::MessageHandlingError(s) if s=="child-escalation"));
            calls.decisions.fetch_add(1, Ordering::SeqCst);
            SupervisionDecision::Resume
        })),
    )
    .await;
    let watcher = parrot_api::death_watch::DeathWatch::new(8);
    let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
    watcher.set_recipient(Arc::new(Events(send))).unwrap();
    let watched = parent.death_watch().unwrap();
    let identity = watched.id();
    watched.watch(&watcher).unwrap();
    let child = make(
        &parent,
        Arc::new(Stats::default()),
        false,
        policy(BasicDecisionFn::new(|_| SupervisionDecision::Escalate)),
    )
    .await;
    assert!(
        matches!(child.send(Box::new(Fail)).await,Err(ActorError::MessageHandlingError(ref s)) if s=="child-escalation")
    );
    until(|| stats.decisions.load(Ordering::SeqCst) == 1).await;
    let result = parent.send(Box::new(Query)).await;
    assert_eq!(parent.death_watch().unwrap().id(), identity);
    assert!(
        events.try_recv().is_err(),
        "Resume must not emit termination"
    );
    root.stop().await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.generation, identity);
    assert_eq!(event.path, parent.path());
    assert!(matches!(
        event.reason,
        parrot_api::supervisor::DeathReason::Normal
    ));
    assert!(events.try_recv().is_err());
    e.shutdown().await;
    assert_eq!(
        *result
            .expect("Resume must keep the original parent usable")
            .downcast::<usize>()
            .unwrap(),
        11
    );
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(stats.inits.load(Ordering::SeqCst), 1);
    assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
}
#[actix::test]
async fn thread_dynamic_instance_resumes_child_escalation() {
    idle_resume(false, false).await;
}
#[actix::test]
async fn thread_dynamic_factory_resumes_child_escalation() {
    idle_resume(false, true).await;
}
#[actix::test]
async fn actix_dynamic_instance_resumes_child_escalation() {
    idle_resume(true, false).await;
}
#[actix::test]
async fn actix_dynamic_factory_resumes_child_escalation() {
    idle_resume(true, true).await;
}

struct OwnFail;
struct HoldMessage(Arc<Hold>);
#[derive(Default)]
struct Hold {
    open: AtomicBool,
    changed: tokio::sync::Notify,
    started: AtomicUsize,
    completed: AtomicUsize,
    cancelled: AtomicUsize,
}
impl Hold {
    async fn wait(self: &Arc<Self>) {
        struct Guard {
            hold: Arc<Hold>,
            completed: bool,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                if !self.completed {
                    self.hold.cancelled.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        let mut guard = Guard {
            hold: self.clone(),
            completed: false,
        };
        self.started.fetch_add(1, Ordering::SeqCst);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.open.load(Ordering::SeqCst) {
                break;
            }
            changed.await;
        }
        guard.completed = true;
        self.completed.fetch_add(1, Ordering::SeqCst);
    }
    fn release(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}
#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (open, timed) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(8), |o| !*o)
            .unwrap();
        assert!(!timed.timed_out() || *open, "test gate not released");
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(3), self.entered.notified())
            .await
            .unwrap();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
fn escalation_policy() -> SupervisorStrategyType {
    policy(BasicDecisionFn::new(|_| SupervisionDecision::Escalate))
}
fn resume_policy(stats: &Arc<Stats>) -> SupervisorStrategyType {
    let stats = stats.clone();
    policy(BasicDecisionFn::new(move |error| {
        assert!(matches!(error,ActorError::MessageHandlingError(s) if s=="child-escalation"));
        stats.decisions.fetch_add(1, Ordering::SeqCst);
        SupervisionDecision::Resume
    }))
}
async fn query(actor: &BoxedActorRef) -> usize {
    *actor
        .send(Box::new(Query))
        .await
        .unwrap()
        .downcast::<usize>()
        .unwrap()
}
async fn fail(child: &BoxedActorRef) {
    assert!(
        matches!(child.send(Box::new(Fail)).await,Err(ActorError::MessageHandlingError(ref s)) if s=="child-escalation")
    );
}

#[actix::test]
async fn pending_dynamic_initializer_and_admitted_request_survive_resume() {
    for actix in [false, true] {
        for factory in [false, true] {
            let e = Engine::new(actix).await;
            let root = e.root().await;
            let stats = Arc::new(Stats::default());
            let hold = Arc::new(Hold::default());
            stats.spawn_in_init.store(true, Ordering::SeqCst);
            *stats.init_hold.lock().unwrap() = Some(hold.clone());
            let parent = make(&root, stats.clone(), factory, resume_policy(&stats)).await;
            until(|| hold.started.load(Ordering::SeqCst) == 1).await;
            let child = stats
                .init_child
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .clone_boxed();
            let mut queued = Box::pin(parent.send(Box::new(Query)));
            assert!(queued.as_mut().now_or_never().is_none());
            fail(&child).await;
            until(|| stats.decisions.load(Ordering::SeqCst) == 1).await;
            assert!(queued.as_mut().now_or_never().is_none());
            assert_eq!(hold.cancelled.load(Ordering::SeqCst), 0);
            hold.release();
            assert_eq!(*queued.await.unwrap().downcast::<usize>().unwrap(), 20);
            assert_eq!(hold.completed.load(Ordering::SeqCst), 1);
            assert_eq!(stats.inits.load(Ordering::SeqCst), 1);
            root.stop().await.unwrap();
            assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
            e.shutdown().await;
        }
    }
}
async fn pending_handler(actix: bool, mode: usize, factory: bool) {
    let e = Engine::with_settings(actix, mode, Duration::from_secs(3)).await;
    let root = e.root().await;
    let stats = Arc::new(Stats::default());
    let parent = make(&root, stats.clone(), factory, resume_policy(&stats)).await;
    let child = make(
        &parent,
        Arc::new(Stats::default()),
        false,
        escalation_policy(),
    )
    .await;
    let hold = Arc::new(Hold::default());
    let mut reply = Box::pin(parent.send(Box::new(HoldMessage(hold.clone()))));
    assert!(reply.as_mut().now_or_never().is_none());
    until(|| hold.started.load(Ordering::SeqCst) == 1).await;
    fail(&child).await;
    until(|| stats.decisions.load(Ordering::SeqCst) == 1).await;
    assert!(reply.as_mut().now_or_never().is_none());
    assert_eq!(hold.cancelled.load(Ordering::SeqCst), 0);
    hold.release();
    assert_eq!(*reply.await.unwrap().downcast::<usize>().unwrap(), 1111);
    assert_eq!(query(&parent).await, 1111);
    assert_eq!(hold.completed.load(Ordering::SeqCst), 1);
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    root.stop().await.unwrap();
    assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
    e.shutdown().await;
}
#[actix::test]
async fn pending_dynamic_handler_keeps_its_future_mutation_and_reply() {
    for actix in [false, true] {
        for factory in [false, true] {
            pending_handler(actix, 0, factory).await;
        }
    }
}
#[actix::test]
async fn dedicated_and_sharded_resume_preserve_the_same_pending_execution() {
    for mode in [1, 2] {
        for factory in [false, true] {
            pending_handler(false, mode, factory).await;
        }
    }
}

#[actix::test]
async fn synchronous_actix_handler_waits_for_the_parent_escalation_decision() {
    for factory in [false, true] {
        let e = Engine::new(true).await;
        let root = e.root().await;
        let stats = Arc::new(Stats::default());
        stats.sync.store(true, Ordering::SeqCst);
        stats.spawn_in_init.store(true, Ordering::SeqCst);
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        let parent = make(
            &root,
            stats.clone(),
            factory,
            policy(BasicDecisionFn::new(move |_| {
                blocked.block();
                SupervisionDecision::Resume
            })),
        )
        .await;
        assert_eq!(query(&parent).await, 10);
        let child = stats
            .init_child
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone_boxed();
        fail(&child).await;
        gate.entered().await;
        let mut queued = Box::pin(parent.send(Box::new(Query)));
        assert!(queued.as_mut().now_or_never().is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), queued.as_mut())
                .await
                .is_err()
        );
        assert_eq!(stats.sync_calls.load(Ordering::SeqCst), 1);
        gate.release();
        assert_eq!(*queued.await.unwrap().downcast::<usize>().unwrap(), 10);
        assert_eq!(stats.sync_calls.load(Ordering::SeqCst), 2);
        root.stop().await.unwrap();
        assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
        e.shutdown().await;
    }
}

#[actix::test]
async fn dynamic_parent_and_child_failures_have_independent_decision_receipts() {
    for actix in [false, true] {
        for factory in [false, true] {
            for stop in [false, true] {
                let e = Engine::new(actix).await;
                let root = e.root().await;
                let stats = Arc::new(Stats::default());
                let message = Arc::new(Gate::default());
                let escalation = Arc::new(Gate::default());
                let _r1 = Release(message.clone());
                let _r2 = Release(escalation.clone());
                let mg = message.clone();
                let eg = escalation.clone();
                let message_calls = Arc::new(AtomicUsize::new(0));
                let escalation_calls = Arc::new(AtomicUsize::new(0));
                let mc = message_calls.clone();
                let ec = escalation_calls.clone();
                let parent = make(
                    &root,
                    stats.clone(),
                    factory,
                    policy(BasicDecisionFn::new(move |error| match error {
                        ActorError::MessageHandlingError(s) if s == "parent-message" => {
                            mc.fetch_add(1, Ordering::SeqCst);
                            mg.block();
                            SupervisionDecision::Resume
                        }
                        ActorError::MessageHandlingError(s) if s == "child-escalation" => {
                            ec.fetch_add(1, Ordering::SeqCst);
                            eg.block();
                            if stop {
                                SupervisionDecision::Stop
                            } else {
                                SupervisionDecision::Resume
                            }
                        }
                        _ => panic!("unexpected failure {error}"),
                    })),
                )
                .await;
                let child = make(
                    &parent,
                    Arc::new(Stats::default()),
                    false,
                    escalation_policy(),
                )
                .await;
                let mut reply = Box::pin(parent.send(Box::new(OwnFail)));
                assert!(reply.as_mut().now_or_never().is_none());
                message.entered().await;
                fail(&child).await;
                escalation.entered().await;
                assert_eq!(message_calls.load(Ordering::SeqCst), 1);
                assert_eq!(escalation_calls.load(Ordering::SeqCst), 1);
                escalation.release();
                if stop {
                    until(|| stats.drops.load(Ordering::SeqCst) == 1).await;
                    until(|| e.count() == 2).await;
                    assert!(matches!(
                        parent.send(Box::new(Query)).await,
                        Err(ActorError::Stopped)
                    ));
                    message.release();
                    until(|| e.count() == 1).await;
                    assert!(reply.await.is_err());
                    assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
                } else {
                    assert!(reply.as_mut().now_or_never().is_none());
                    message.release();
                    assert!(
                        matches!(reply.await,Err(ActorError::MessageHandlingError(ref s)) if s=="parent-message")
                    );
                    assert_eq!(query(&parent).await, 511);
                }
                root.stop().await.unwrap();
                e.shutdown().await;
            }
        }
    }
}

struct Events(tokio::sync::mpsc::UnboundedSender<parrot_api::context::ActorTerminated>);
impl parrot_api::death_watch::TerminationRecipient for Events {
    fn deliver(
        &self,
        event: parrot_api::context::ActorTerminated,
    ) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.0.send(event).unwrap();
            Ok(())
        })
    }
}
async fn interrupted_decision(actix: bool, factory: bool, kind: usize) {
    let e = Engine::with_settings(actix, 0, Duration::from_millis(100)).await;
    let root = e.root().await;
    let stats = Arc::new(Stats::default());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocked = gate.clone();
    let parent = make(
        &root,
        stats.clone(),
        factory,
        policy(BasicDecisionFn::new(move |_| {
            blocked.block();
            SupervisionDecision::Resume
        })),
    )
    .await;
    let child = make(
        &parent,
        Arc::new(Stats::default()),
        false,
        escalation_policy(),
    )
    .await;
    let hold = Arc::new(Hold::default());
    let mut reply = Box::pin(parent.send(Box::new(HoldMessage(hold.clone()))));
    assert!(reply.as_mut().now_or_never().is_none());
    until(|| hold.started.load(Ordering::SeqCst) == 1).await;
    fail(&child).await;
    gate.entered().await;
    if kind == 2 {
        until(|| hold.cancelled.load(Ordering::SeqCst) == 1).await;
    } else {
        if kind == 1 {
            let target = parent.clone_boxed();
            let waiter = tokio::spawn(async move { target.stop().await });
            until(|| hold.cancelled.load(Ordering::SeqCst) == 1).await;
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
        }
        let error = parent.stop().await.unwrap_err();
        let text = error.to_string().to_lowercase();
        assert!(
            text.contains("timeout") || text.contains("exceeded") || text.contains("deadline"),
            "{error}"
        );
    }
    until(|| e.count() == 2).await;
    assert_eq!(hold.cancelled.load(Ordering::SeqCst), 1);
    assert!(matches!(
        parent.send(Box::new(Query)).await,
        Err(ActorError::Stopped)
    ));
    assert_eq!(stats.hooks.load(Ordering::SeqCst), 0);
    gate.release();
    until(|| e.count() == 1).await;
    assert!(reply.await.is_err());
    assert_eq!(stats.built.load(Ordering::SeqCst), 1);
    assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
    root.stop().await.unwrap();
    e.shutdown().await;
}
#[actix::test]
async fn stop_and_cancelled_stop_waiter_keep_actual_dynamic_decision_ownership() {
    for actix in [false, true] {
        for factory in [false, true] {
            for kind in [0, 1] {
                interrupted_decision(actix, factory, kind).await;
            }
        }
    }
}
#[actix::test]
async fn expired_dynamic_escalation_cancels_stage_without_late_revival() {
    for actix in [false, true] {
        for factory in [false, true] {
            interrupted_decision(actix, factory, 2).await;
        }
    }
}

#[actix::test]
async fn dynamic_replacement_waits_for_interrupted_message_decision_and_starts_fresh() {
    for actix in [false, true] {
        let e = Engine::new(actix).await;
        let root = e.root().await;
        let stats = Arc::new(Stats::default());
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let blocked = gate.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let invoked = calls.clone();
        let parent = make(
            &root,
            stats.clone(),
            true,
            SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(30),
                decider: BasicDecisionFn::new(move |error| match error {
                    ActorError::MessageHandlingError(s) if s == "parent-message" => {
                        invoked.fetch_add(1, Ordering::SeqCst);
                        blocked.block();
                        SupervisionDecision::Resume
                    }
                    ActorError::MessageHandlingError(s) if s == "child-escalation" => {
                        SupervisionDecision::Restart
                    }
                    _ => panic!("unexpected failure {error}"),
                }),
            }),
        )
        .await;
        let child = make(
            &parent,
            Arc::new(Stats::default()),
            false,
            escalation_policy(),
        )
        .await;
        let mut reply = Box::pin(parent.send(Box::new(OwnFail)));
        assert!(reply.as_mut().now_or_never().is_none());
        gate.entered().await;
        fail(&child).await;
        until(|| stats.drops.load(Ordering::SeqCst) == 1).await;
        // Dynamic replacement has a new mailbox. During retirement the logical
        // reference rejects this request; it does not promise static queue retention.
        assert!(matches!(
            parent.send(Box::new(Query)).await,
            Err(ActorError::Stopped)
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            stats.built.load(Ordering::SeqCst),
            1,
            "old decision must retire before replacement construction"
        );
        gate.release();
        until(|| stats.inits.load(Ordering::SeqCst) == 2).await;
        assert_eq!(query(&parent).await, 10);
        assert!(reply.await.is_err());
        assert!(
            matches!(parent.send(Box::new(OwnFail)).await,Err(ActorError::MessageHandlingError(ref s)) if s=="parent-message")
        );
        assert_eq!(query(&parent).await, 510);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(stats.built.load(Ordering::SeqCst), 2);
        root.stop().await.unwrap();
        e.shutdown().await;
    }
}
struct FailLabel(&'static str);
#[actix::test]
async fn dynamic_concurrent_escalations_keep_first_cause_and_allow_a_later_fresh_decision() {
    for actix in [false, true] {
        for factory in [false, true] {
            let timeout = Duration::from_millis(500);
            let e = Engine::with_settings(actix, 0, timeout).await;
            let root = e.root().await;
            let stats = Arc::new(Stats::default());
            let gate = Arc::new(Gate::default());
            let _release = Release(gate.clone());
            let blocked = gate.clone();
            let causes = Arc::new(Mutex::new(Vec::new()));
            let observed = causes.clone();
            let parent = make(
                &root,
                stats.clone(),
                factory,
                policy(BasicDecisionFn::new(move |e| {
                    let ActorError::MessageHandlingError(s) = e else {
                        panic!("unexpected cause {e}")
                    };
                    observed.lock().unwrap().push(s.clone());
                    blocked.block();
                    SupervisionDecision::Resume
                })),
            )
            .await;
            let first = make(
                &parent,
                Arc::new(Stats::default()),
                false,
                escalation_policy(),
            )
            .await;
            let second = make(
                &parent,
                Arc::new(Stats::default()),
                false,
                escalation_policy(),
            )
            .await;
            assert!(
                matches!(first.send(Box::new(FailLabel("first"))).await,Err(ActorError::MessageHandlingError(ref s)) if s=="first")
            );
            gate.entered().await;
            assert!(
                matches!(second.send(Box::new(FailLabel("coalesced"))).await,Err(ActorError::MessageHandlingError(ref s)) if s=="coalesced")
            );
            until(|| e.count() == 2).await;
            let mut queued = Box::pin(parent.send(Box::new(Query)));
            assert!(queued.as_mut().now_or_never().is_none());
            gate.release();
            assert_eq!(*queued.await.unwrap().downcast::<usize>().unwrap(), 12);
            tokio::time::sleep(timeout * 2).await;
            assert_eq!(query(&parent).await, 12);
            assert_eq!(*causes.lock().unwrap(), ["first"]);
            let later = make(
                &parent,
                Arc::new(Stats::default()),
                false,
                escalation_policy(),
            )
            .await;
            assert!(
                matches!(later.send(Box::new(FailLabel("later"))).await,Err(ActorError::MessageHandlingError(ref s)) if s=="later")
            );
            until(|| causes.lock().unwrap().len() == 2).await;
            assert_eq!(query(&parent).await, 13);
            assert_eq!(*causes.lock().unwrap(), ["first", "later"]);
            assert_eq!(stats.inits.load(Ordering::SeqCst), 1);
            root.stop().await.unwrap();
            e.shutdown().await;
        }
    }
}
#[actix::test]
async fn dynamic_one_for_all_resume_preserves_actual_siblings() {
    for actix in [false, true] {
        for factory in [false, true] {
            let e = Engine::new(actix).await;
            let root = e.root().await;
            let stats = Arc::new(Stats::default());
            let calls = stats.clone();
            let sibling_stats = Arc::new(Stats::default());
            let parent = make(
                &root,
                stats.clone(),
                factory,
                SupervisorStrategyType::OneForAll(parrot_api::supervisor::OneForAllStrategy {
                    max_restarts: 0,
                    within: Duration::from_secs(30),
                    decider: BasicDecisionFn::new(move |_| {
                        calls.decisions.fetch_add(1, Ordering::SeqCst);
                        SupervisionDecision::Resume
                    }),
                }),
            )
            .await;
            let sibling = make(
                &root,
                sibling_stats.clone(),
                factory,
                policy(BasicDecisionFn::new(|_| SupervisionDecision::Stop)),
            )
            .await;
            assert_eq!(query(&sibling).await, 10);
            let child = make(
                &parent,
                Arc::new(Stats::default()),
                false,
                escalation_policy(),
            )
            .await;
            fail(&child).await;
            until(|| stats.decisions.load(Ordering::SeqCst) == 1).await;
            assert_eq!(query(&parent).await, 11);
            assert_eq!(query(&sibling).await, 10);
            assert_eq!(sibling_stats.drops.load(Ordering::SeqCst), 0);
            assert_eq!(stats.built.load(Ordering::SeqCst), 1);
            assert_eq!(sibling_stats.built.load(Ordering::SeqCst), 1);
            root.stop().await.unwrap();
            assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
            assert_eq!(sibling_stats.hooks.load(Ordering::SeqCst), 1);
            e.shutdown().await;
        }
    }
}
#[actix::test]
async fn dynamic_escalation_resume_does_not_consume_later_factory_restart_budget() {
    for actix in [false, true] {
        let e = Engine::new(actix).await;
        let root = e.root().await;
        let stats = Arc::new(Stats::default());
        let calls = stats.clone();
        let parent = make(
            &root,
            stats.clone(),
            true,
            SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(30),
                decider: BasicDecisionFn::new(move |e| match e {
                    ActorError::MessageHandlingError(s) if s == "child-escalation" => {
                        calls.decisions.fetch_add(1, Ordering::SeqCst);
                        SupervisionDecision::Resume
                    }
                    ActorError::MessageHandlingError(s) if s == "parent-message" => {
                        SupervisionDecision::Restart
                    }
                    _ => panic!("unexpected {e}"),
                }),
            }),
        )
        .await;
        for count in 1..=2 {
            let child = make(
                &parent,
                Arc::new(Stats::default()),
                false,
                escalation_policy(),
            )
            .await;
            fail(&child).await;
            until(|| stats.decisions.load(Ordering::SeqCst) == count).await;
            assert_eq!(query(&parent).await, 10 + count);
        }
        assert!(
            matches!(parent.send(Box::new(OwnFail)).await,Err(ActorError::MessageHandlingError(ref s)) if s=="parent-message")
        );
        until(|| stats.inits.load(Ordering::SeqCst) == 2).await;
        assert_eq!(query(&parent).await, 10);
        assert_eq!(stats.built.load(Ordering::SeqCst), 2);
        assert_eq!(stats.inits.load(Ordering::SeqCst), 2);
        root.stop().await.unwrap();
        e.shutdown().await;
    }
}

#[actix::test]
async fn direct_dynamic_roots_apply_resume_without_a_fabricated_parent() {
    for actix in [false, true] {
        for mode in if actix { vec![0] } else { vec![0, 1, 2] } {
            let e = Engine::with_settings(actix, mode, Duration::from_secs(3)).await;
            let stats = Arc::new(Stats::default());
            let parent: BoxedActorRef = match &e {
                Engine::Thread(s) => Box::new(
                    s.spawn_at(
                        ThreadNode::new(stats.clone()),
                        "/parent",
                        None,
                        parrot::thread::ThreadActorConfig {
                            supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Resume),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap(),
                ),
                Engine::Actix(s) => s
                    .spawn_root_typed_with_strategy(
                        ActixNode::new(stats.clone()),
                        EmptyConfig,
                        SupervisorStrategyType::Default(
                            parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
                        ),
                    )
                    .await
                    .unwrap(),
            };
            let child = make(
                &parent,
                Arc::new(Stats::default()),
                false,
                escalation_policy(),
            )
            .await;
            fail(&child).await;
            until(|| e.count() == 1).await;
            assert_eq!(query(&parent).await, 11);
            assert_eq!(stats.inits.load(Ordering::SeqCst), 1);
            parent.stop().await.unwrap();
            assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
            e.shutdown().await;
        }
    }
}

struct ParentPath;
#[actix::test]
async fn direct_dynamic_root_factories_resume_without_reconstruction_or_parent() {
    for actix in [false, true] {
        let e = Engine::new(actix).await;
        let stats = Arc::new(Stats::default());
        let made = stats.clone();
        let parent: BoxedActorRef = match &e {
            Engine::Thread(s) => Box::new(
                s.spawn_supervised(
                    move || ThreadNode::new(made.clone()),
                    "/parent",
                    parrot::thread::SupervisorStrategy::Resume,
                )
                .await
                .unwrap(),
            ),
            Engine::Actix(s) => s
                .spawn_root_factory_with_strategy(
                    move || ActixNode::new(made.clone()),
                    OneForOneStrategy {
                        max_restarts: 0,
                        within: Duration::from_secs(30),
                        decider: BasicDecisionFn::new(|_| SupervisionDecision::Resume),
                    },
                )
                .await
                .unwrap(),
        };
        assert!(
            parent
                .send(Box::new(ParentPath))
                .await
                .unwrap()
                .downcast::<Option<String>>()
                .unwrap()
                .is_none()
        );
        let child = make(
            &parent,
            Arc::new(Stats::default()),
            false,
            escalation_policy(),
        )
        .await;
        fail(&child).await;
        until(|| e.count() == 1).await;
        assert_eq!(query(&parent).await, 11);
        assert_eq!(stats.built.load(Ordering::SeqCst), 1);
        assert_eq!(stats.inits.load(Ordering::SeqCst), 1);
        parent.stop().await.unwrap();
        assert_eq!(stats.hooks.load(Ordering::SeqCst), 1);
        e.shutdown().await;
    }
}
struct BatchHold(Arc<Hold>);
#[actix::test]
async fn real_multi_message_batch_survives_escalation_in_every_thread_scheduler() {
    for mode in [0, 1, 2] {
        let e = Engine::with_settings(false, mode, Duration::from_secs(3)).await;
        let stats = Arc::new(Stats::default());
        let init = Arc::new(Hold::default());
        let batch = Arc::new(Hold::default());
        stats.spawn_in_init.store(true, Ordering::SeqCst);
        *stats.init_hold.lock().unwrap() = Some(init.clone());
        let Engine::Thread(system) = &e else {
            unreachable!()
        };
        let parent: BoxedActorRef = Box::new(
            system
                .spawn_at(
                    ThreadNode::new(stats.clone()),
                    "/parent",
                    None,
                    parrot::thread::ThreadActorConfig {
                        supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Resume),
                        message_batch_size: Some(4),
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
        );
        until(|| init.started.load(Ordering::SeqCst) == 1).await;
        let child = stats
            .init_child
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone_boxed();
        let mut first = Box::pin(parent.send(Box::new(BatchHold(batch.clone()))));
        let mut second = Box::pin(parent.send(Box::new(BatchHold(batch.clone()))));
        assert!(first.as_mut().now_or_never().is_none());
        assert!(second.as_mut().now_or_never().is_none());
        init.release();
        until(|| batch.started.load(Ordering::SeqCst) == 1).await;
        assert_eq!(stats.batches.load(Ordering::SeqCst), 1);
        assert_eq!(stats.batch_width.load(Ordering::SeqCst), 2);
        fail(&child).await;
        until(|| e.count() == 1).await;
        assert!(first.as_mut().now_or_never().is_none());
        assert!(second.as_mut().now_or_never().is_none());
        assert_eq!(batch.cancelled.load(Ordering::SeqCst), 0);
        batch.release();
        assert_eq!(*first.await.unwrap().downcast::<usize>().unwrap(), 1022);
        assert_eq!(*second.await.unwrap().downcast::<usize>().unwrap(), 1022);
        assert_eq!(query(&parent).await, 1022);
        assert_eq!(batch.completed.load(Ordering::SeqCst), 1);
        assert_eq!(stats.batches.load(Ordering::SeqCst), 1);
        parent.stop().await.unwrap();
        e.shutdown().await;
    }
}

mod native_sources {
    use super::*;
    use std::error::Error;
    pub(super) struct Failure(pub ActorError);
    struct Source {
        hostile: bool,
        formats: AtomicUsize,
        sources: AtomicUsize,
    }
    struct Business(Arc<Source>);
    impl std::fmt::Debug for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("NativeBusiness")
        }
    }
    impl std::fmt::Display for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.formats.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.0.hostile,
                "native supervision called business Display"
            );
            f.write_str("original native business failure")
        }
    }
    impl Error for Business {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.sources.fetch_add(1, Ordering::SeqCst);
            panic!("native supervision called business source")
        }
    }
    fn identity(error: &ActorError, source: &Arc<Source>) -> Option<usize> {
        let mut current = Some(error as &(dyn Error + 'static));
        for _ in 0..128 {
            let error = current?;
            if let Some(business) = error.downcast_ref::<Business>() {
                return Arc::ptr_eq(&business.0, source)
                    .then_some(business as *const Business as usize);
            }
            current = error.source();
        }
        None
    }
    pub(super) fn logging() {
        static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        INIT.get_or_init(|| {
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::ERROR)
                    .with_writer(std::io::sink)
                    .finish(),
            )
            .expect("this integration target has one tracing initializer");
        });
    }
    async fn run(mode: usize, factory: bool, fail: bool, hostile: bool, tell: bool) {
        logging();
        let e = Engine::with_settings(mode == 3, mode, Duration::from_secs(3)).await;
        let root = e.root().await;
        let source = Arc::new(Source {
            hostile,
            formats: AtomicUsize::new(0),
            sources: AtomicUsize::new(0),
        });
        let parent_stats = Arc::new(Stats::default());
        let child_stats = Arc::new(Stats::default());
        let parent_seen = Arc::new(AtomicUsize::new(0));
        let seen = parent_seen.clone();
        let cause = source.clone();
        let counts = parent_stats.clone();
        let parent = make(
            &root,
            parent_stats.clone(),
            factory,
            policy(BasicDecisionFn::new(move |error| {
                counts.decisions.fetch_add(1, Ordering::SeqCst);
                seen.store(identity(error, &cause).unwrap_or(0), Ordering::SeqCst);
                SupervisionDecision::Stop
            })),
        )
        .await;
        let watcher = parrot_api::death_watch::DeathWatch::new(8);
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        watcher.set_recipient(Arc::new(Events(tx))).unwrap();
        let watched = parent.death_watch().unwrap();
        let generation = watched.id();
        watched.watch(&watcher).unwrap();
        let calls = child_stats.clone();
        let child = make(
            &parent,
            child_stats.clone(),
            false,
            policy(BasicDecisionFn::new(move |_| {
                calls.decisions.fetch_add(1, Ordering::SeqCst);
                SupervisionDecision::Escalate
            })),
        )
        .await;
        child.send(Box::new(Query)).await.unwrap();
        let error = anyhow::Error::new(Business(source.clone()));
        let pointer = error.downcast_ref::<Business>().unwrap() as *const Business as usize;
        let reply = if fail && tell {
            child
                .deliver(Box::new(Failure(ActorError::Other(error))))
                .await
                .unwrap();
            None
        } else if fail {
            Some(
                tokio::time::timeout(
                    Duration::from_secs(3),
                    child.send(Box::new(Failure(ActorError::Other(error)))),
                )
                .await,
            )
        } else {
            drop(error);
            None
        };
        let autonomous = if fail {
            tokio::time::timeout(Duration::from_secs(3), async {
                while e.count() != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok()
        } else {
            true
        };
        // Always finish real cleanup before asserting the negative-control result.
        let root_live = root.send(Box::new(Query)).await.is_ok();
        let stopped = root.stop().await;
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv()).await;
        let remaining = e.count();
        match e {
            Engine::Thread(s) => {
                let _ = s.shutdown_internal().await;
            }
            Engine::Actix(s) => {
                let _ = s.shutdown().await;
            }
        }
        assert!(
            root_live,
            "a failed descendant must not poison an open healthy root"
        );
        assert!(stopped.is_ok(), "root cleanup must remain healthy");
        assert_eq!(remaining, 0);
        assert!(
            autonomous,
            "supervision must autonomously retire failed child and parent"
        );
        assert_eq!(parent_stats.built.load(Ordering::SeqCst), 1);
        assert_eq!(parent_stats.drops.load(Ordering::SeqCst), 1);
        assert_eq!(child_stats.drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            parent_stats.decisions.load(Ordering::SeqCst),
            usize::from(fail)
        );
        assert_eq!(
            child_stats.decisions.load(Ordering::SeqCst),
            usize::from(fail)
        );
        let event = event.unwrap().unwrap();
        assert_eq!(event.generation, generation);
        assert_eq!(event.path, parent.path());
        if fail {
            if !tell {
                let reply = reply
                    .unwrap()
                    .expect("ASK must receive the original failure without timing out");
                let error = match reply {
                    Err(error) => error,
                    Ok(_) => panic!("failed ASK reported success"),
                };
                assert_eq!(identity(&error, &source), Some(pointer));
            }
            assert_eq!(parent_seen.load(Ordering::SeqCst), pointer);
            assert!(matches!(
                event.reason,
                parrot_api::supervisor::DeathReason::Escalated(_)
            ));
        } else {
            assert!(matches!(
                event.reason,
                parrot_api::supervisor::DeathReason::Normal
            ));
        }
        assert!(events.try_recv().is_err());
        assert_eq!(
            source.formats.load(Ordering::SeqCst),
            0,
            "framework diagnostics called business Display"
        );
        assert_eq!(source.sources.load(Ordering::SeqCst), 0);
    }
    macro_rules! cases {($($name:ident:$mode:expr,$factory:expr,$fail:expr,$hostile:expr;)*)=>{$(
        #[actix::test] async fn $name(){run($mode,$factory,$fail,$hostile,false).await;}
    )*};}
    cases! {
        shared_instance_healthy:0,false,false,false;
        dedicated_instance_healthy:1,false,false,false;
        sharded_instance_healthy:2,false,false,false;
        actix_instance_healthy:3,false,false,false;
        shared_factory_healthy:0,true,false,false;
        dedicated_factory_healthy:1,true,false,false;
        sharded_factory_healthy:2,true,false,false;
        actix_factory_healthy:3,true,false,false;
        shared_instance_source:0,false,true,false;
        dedicated_instance_source:1,false,true,false;
        sharded_instance_source:2,false,true,false;
        actix_instance_source:3,false,true,false;
        shared_factory_source:0,true,true,false;
        dedicated_factory_source:1,true,true,false;
        sharded_factory_source:2,true,true,false;
        actix_factory_source:3,true,true,false;
        shared_instance_hostile:0,false,true,true;
        dedicated_instance_hostile:1,false,true,true;
        sharded_instance_hostile:2,false,true,true;
        actix_instance_hostile:3,false,true,true;
        shared_factory_hostile:0,true,true,true;
        dedicated_factory_hostile:1,true,true,true;
        sharded_factory_hostile:2,true,true,true;
        actix_factory_hostile:3,true,true,true;
    }
    #[actix::test]
    async fn shared_tell_source() {
        run(0, false, true, false, true).await;
    }
    #[actix::test]
    async fn dedicated_tell_source() {
        run(1, false, true, false, true).await;
    }
    #[actix::test]
    async fn sharded_tell_source() {
        run(2, false, true, false, true).await;
    }
    #[actix::test]
    async fn actix_tell_source() {
        run(3, false, true, false, true).await;
    }
    #[actix::test]
    async fn shared_tell_hostile() {
        run(0, false, true, true, true).await;
    }
    #[actix::test]
    async fn dedicated_tell_hostile() {
        run(1, false, true, true, true).await;
    }
    #[actix::test]
    async fn sharded_tell_hostile() {
        run(2, false, true, true, true).await;
    }
    #[actix::test]
    async fn actix_tell_hostile() {
        run(3, false, true, true, true).await;
    }
}

mod initialization_sources {
    use super::*;
    use std::error::Error;
    pub(super) struct TryMake(pub Make);
    struct Source {
        hostile: bool,
        formats: AtomicUsize,
        sources: AtomicUsize,
    }
    struct Business(Arc<Source>);
    impl std::fmt::Debug for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("NativeInitializationBusiness")
        }
    }
    impl std::fmt::Display for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.formats.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.0.hostile,
                "initialization diagnostics called business Display"
            );
            f.write_str("original initialization failure")
        }
    }
    impl Error for Business {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.sources.fetch_add(1, Ordering::SeqCst);
            panic!("initialization diagnostics called business source")
        }
    }
    fn identity(error: &ActorError, source: &Arc<Source>) -> Option<usize> {
        let mut current = Some(error as &(dyn Error + 'static));
        for _ in 0..128 {
            let error = current?;
            if let Some(business) = error.downcast_ref::<Business>() {
                return Arc::ptr_eq(&business.0, source)
                    .then_some(business as *const Business as usize);
            }
            current = error.source();
        }
        None
    }
    async fn run(mode: usize, factory: bool, fail: bool, hostile: bool) {
        native_sources::logging();
        let e = Engine::with_settings(mode == 3, mode, Duration::from_secs(3)).await;
        let root = e.root().await;
        let stats = Arc::new(Stats::default());
        let source = Arc::new(Source {
            hostile,
            formats: AtomicUsize::new(0),
            sources: AtomicUsize::new(0),
        });
        let error = anyhow::Error::new(Business(source.clone()));
        let pointer = error.downcast_ref::<Business>().unwrap() as *const Business as usize;
        if fail {
            *stats.initialization_error.lock().unwrap() = Some(ActorError::Other(error));
        } else {
            drop(error);
        }
        let called = stats.clone();
        let captured = source.clone();
        let observed = Arc::new(AtomicUsize::new(0));
        let seen = observed.clone();
        let strategy = policy(BasicDecisionFn::new(move |error| {
            called.decisions.fetch_add(1, Ordering::SeqCst);
            seen.store(identity(error, &captured).unwrap_or(0), Ordering::SeqCst);
            SupervisionDecision::Stop
        }));
        // Treat a child creation failure as a domain reply here so this fixture
        // does not accidentally fail the healthy parent handler with `?`.
        let response = root
            .send(Box::new(TryMake(Make {
                stats: stats.clone(),
                factory,
                policy: strategy,
            })))
            .await
            .unwrap();
        let created = *response.downcast::<ActorResult<BoxedActorRef>>().unwrap();
        let autonomous = if fail {
            tokio::time::timeout(Duration::from_secs(3), async {
                while e.count() != 1 || stats.drops.load(Ordering::SeqCst) != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok()
        } else {
            let child = created.as_ref().expect("healthy child creation succeeds");
            assert_eq!(
                *child
                    .send(Box::new(Query))
                    .await
                    .unwrap()
                    .downcast::<usize>()
                    .unwrap(),
                10
            );
            true
        };
        let parent_live = root.send(Box::new(Query)).await.is_ok();
        let stopped = root.stop().await;
        let remaining = e.count();
        match e {
            Engine::Thread(s) => {
                let _ = s.shutdown_internal().await;
            }
            Engine::Actix(s) => {
                let _ = s.shutdown().await;
            }
        }
        assert!(parent_live);
        assert!(stopped.is_ok());
        assert_eq!(remaining, 0);
        assert!(autonomous);
        assert_eq!(stats.built.load(Ordering::SeqCst), 1);
        assert_eq!(stats.inits.load(Ordering::SeqCst), 1);
        assert_eq!(stats.drops.load(Ordering::SeqCst), 1);
        assert_eq!(stats.hooks.load(Ordering::SeqCst), usize::from(!fail));
        assert_eq!(stats.decisions.load(Ordering::SeqCst), usize::from(fail));
        if fail {
            assert_eq!(
                observed.load(Ordering::SeqCst),
                pointer,
                "supervision lost original initialization source"
            );
            if let Err(error) = created {
                assert_eq!(identity(&error, &source), Some(pointer));
            }
        }
        assert_eq!(source.formats.load(Ordering::SeqCst), 0);
        assert_eq!(source.sources.load(Ordering::SeqCst), 0);
    }
    macro_rules! cases {($($name:ident:$mode:expr,$factory:expr,$fail:expr,$hostile:expr;)*)=>{$(
        #[actix::test] async fn $name(){run($mode,$factory,$fail,$hostile).await;}
    )*};}
    cases! {
        shared_instance_healthy:0,false,false,false;
        dedicated_instance_healthy:1,false,false,false;
        sharded_instance_healthy:2,false,false,false;
        actix_instance_healthy:3,false,false,false;
        shared_factory_healthy:0,true,false,false;
        dedicated_factory_healthy:1,true,false,false;
        sharded_factory_healthy:2,true,false,false;
        actix_factory_healthy:3,true,false,false;
        shared_instance_source:0,false,true,false;
        dedicated_instance_source:1,false,true,false;
        sharded_instance_source:2,false,true,false;
        actix_instance_source:3,false,true,false;
        shared_factory_source:0,true,true,false;
        dedicated_factory_source:1,true,true,false;
        sharded_factory_source:2,true,true,false;
        actix_factory_source:3,true,true,false;
        shared_instance_hostile:0,false,true,true;
        dedicated_instance_hostile:1,false,true,true;
        sharded_instance_hostile:2,false,true,true;
        actix_instance_hostile:3,false,true,true;
        shared_factory_hostile:0,true,true,true;
        dedicated_factory_hostile:1,true,true,true;
        sharded_factory_hostile:2,true,true,true;
        actix_factory_hostile:3,true,true,true;
    }
}

mod batch_sources {
    use super::*;
    use std::error::Error;
    struct Source {
        hostile: bool,
        formats: AtomicUsize,
        sources: AtomicUsize,
    }
    struct Business(Arc<Source>);
    impl std::fmt::Debug for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("BatchBusiness")
        }
    }
    impl std::fmt::Display for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.formats.fetch_add(1, Ordering::SeqCst);
            assert!(!self.0.hostile, "batch boundary called business Display");
            f.write_str("original batch failure")
        }
    }
    impl Error for Business {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.sources.fetch_add(1, Ordering::SeqCst);
            panic!("batch boundary called business source")
        }
    }
    struct Request {
        value: usize,
        error: Option<ActorError>,
        eligible: bool,
    }
    struct Counts {
        drops: AtomicUsize,
        single: AtomicUsize,
        batches: AtomicUsize,
        width: AtomicUsize,
    }
    struct Probe {
        init: Arc<Hold>,
        counts: Arc<Counts>,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            self.counts.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn respond(message: BoxedMessage) -> ActorResult<BoxedMessage> {
        let Request { value, error, .. } = *message.downcast::<Request>().unwrap();
        error.map_or_else(|| Ok(Box::new(value) as BoxedMessage), Err)
    }
    impl Actor for Probe {
        type Config = EmptyConfig;
        type Context = parrot::thread::ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn use_async_handler(&self) -> bool {
            true
        }
        fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.init.wait().await;
                Ok(())
            })
        }
        fn can_batch_message(&self, message: &BoxedMessage) -> bool {
            message
                .downcast_ref::<Request>()
                .is_some_and(|message| message.eligible)
        }
        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.counts.single.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { respond(message) })
        }
        fn receive_batch<'a>(
            &'a mut self,
            messages: Vec<BoxedMessage>,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
            self.counts.batches.fetch_add(1, Ordering::SeqCst);
            self.counts.width.store(messages.len(), Ordering::SeqCst);
            Box::pin(async move { messages.into_iter().map(respond).collect() })
        }
    }
    fn identity(error: &ActorError, source: &Arc<Source>) -> Option<usize> {
        let mut current = Some(error as &(dyn Error + 'static));
        for _ in 0..128 {
            let error = current?;
            if let Some(business) = error.downcast_ref::<Business>() {
                return Arc::ptr_eq(&business.0, source)
                    .then_some(business as *const Business as usize);
            }
            current = error.source();
        }
        None
    }
    async fn run(mode: usize, eligible: bool, fail: bool, hostile: bool) {
        native_sources::logging();
        let e = Engine::with_settings(false, mode, Duration::from_secs(3)).await;
        let init = Arc::new(Hold::default());
        let counts = Arc::new(Counts {
            drops: AtomicUsize::new(0),
            single: AtomicUsize::new(0),
            batches: AtomicUsize::new(0),
            width: AtomicUsize::new(0),
        });
        let Engine::Thread(system) = &e else {
            unreachable!()
        };
        let actor: BoxedActorRef = Box::new(
            system
                .spawn_at(
                    Probe {
                        init: init.clone(),
                        counts: counts.clone(),
                    },
                    "/batch",
                    None,
                    parrot::thread::ThreadActorConfig {
                        supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Stop),
                        message_batch_size: Some(4),
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
        );
        until(|| init.started.load(Ordering::SeqCst) == 1).await;
        let watcher = parrot_api::death_watch::DeathWatch::new(8);
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        watcher.set_recipient(Arc::new(Events(tx))).unwrap();
        let watched = actor.death_watch().unwrap();
        let generation = watched.id();
        watched.watch(&watcher).unwrap();
        let source = Arc::new(Source {
            hostile,
            formats: AtomicUsize::new(0),
            sources: AtomicUsize::new(0),
        });
        let error = anyhow::Error::new(Business(source.clone()));
        let pointer = error.downcast_ref::<Business>().unwrap() as *const Business as usize;
        let error = if fail {
            Some(ActorError::Other(error))
        } else {
            drop(error);
            None
        };
        let mut first = Box::pin(actor.send(Box::new(Request {
            value: 1,
            error,
            eligible,
        })));
        let mut second = Box::pin(actor.send(Box::new(Request {
            value: 2,
            error: None,
            eligible,
        })));
        assert!(first.as_mut().now_or_never().is_none());
        assert!(second.as_mut().now_or_never().is_none());
        init.release();
        let replies = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(first, second)
        })
        .await;
        let autonomous = if fail {
            tokio::time::timeout(Duration::from_secs(3), async {
                while e.count() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok()
        } else {
            true
        };
        let _ = actor.stop().await;
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv()).await;
        let remaining = e.count();
        let _ = system.shutdown_internal().await;
        assert_eq!(remaining, 0);
        assert!(autonomous);
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        assert_eq!(counts.batches.load(Ordering::SeqCst), usize::from(eligible));
        assert_eq!(
            counts.width.load(Ordering::SeqCst),
            if eligible { 2 } else { 0 }
        );
        assert_eq!(
            counts.single.load(Ordering::SeqCst),
            if eligible {
                0
            } else if fail {
                1
            } else {
                2
            }
        );
        let (first, second) = replies.expect("every accepted ASK must resolve");
        if fail {
            let error = match first {
                Err(error) => error,
                Ok(_) => panic!("failed first message reported success"),
            };
            assert_eq!(identity(&error, &source), Some(pointer));
        } else {
            assert_eq!(*first.unwrap().downcast::<usize>().unwrap(), 1);
        }
        if eligible || !fail {
            assert_eq!(
                *second.unwrap().downcast::<usize>().unwrap(),
                2,
                "already executed batch sibling retains its reply"
            );
        } else {
            assert!(
                second.is_err(),
                "unexecuted fallback message must be retired"
            );
        }
        let event = event.unwrap().unwrap();
        assert_eq!(event.generation, generation);
        if fail {
            assert!(matches!(
                event.reason,
                parrot_api::supervisor::DeathReason::Failed(_)
            ));
        } else {
            assert!(matches!(
                event.reason,
                parrot_api::supervisor::DeathReason::Normal
            ));
        }
        assert!(events.try_recv().is_err());
        assert_eq!(source.formats.load(Ordering::SeqCst), 0);
        assert_eq!(source.sources.load(Ordering::SeqCst), 0);
    }
    macro_rules! cases {($($name:ident:$mode:expr,$eligible:expr,$fail:expr,$hostile:expr;)*)=>{$(
        #[actix::test] async fn $name(){run($mode,$eligible,$fail,$hostile).await;}
    )*};}
    cases! {
        shared_batch_healthy:0,true,false,false;
        dedicated_batch_healthy:1,true,false,false;
        sharded_batch_healthy:2,true,false,false;
        shared_fallback_healthy:0,false,false,false;
        dedicated_fallback_healthy:1,false,false,false;
        sharded_fallback_healthy:2,false,false,false;
        shared_batch_source:0,true,true,false;
        dedicated_batch_source:1,true,true,false;
        sharded_batch_source:2,true,true,false;
        shared_fallback_source:0,false,true,false;
        dedicated_fallback_source:1,false,true,false;
        sharded_fallback_source:2,false,true,false;
        shared_batch_hostile:0,true,true,true;
        dedicated_batch_hostile:1,true,true,true;
        sharded_batch_hostile:2,true,true,true;
        shared_fallback_hostile:0,false,true,true;
        dedicated_fallback_hostile:1,false,true,true;
        sharded_fallback_hostile:2,false,true,true;
    }
}

mod native_recovery_sources {
    use super::*;
    use std::error::Error;
    struct Source {
        hostile: bool,
        formats: AtomicUsize,
        sources: AtomicUsize,
    }
    struct Business(Arc<Source>);
    impl std::fmt::Debug for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("NativeRecoveryBusiness")
        }
    }
    impl std::fmt::Display for Business {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.formats.fetch_add(1, Ordering::SeqCst);
            assert!(!self.0.hostile, "native recovery called business Display");
            f.write_str("original descendant cleanup failure")
        }
    }
    impl Error for Business {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.0.sources.fetch_add(1, Ordering::SeqCst);
            panic!("native recovery called business source")
        }
    }
    fn identity(error: &ActorError, source: &Arc<Source>) -> Option<usize> {
        let mut current = Some(error as &(dyn Error + 'static));
        for _ in 0..128 {
            let error = current?;
            if let Some(business) = error.downcast_ref::<Business>() {
                return Arc::ptr_eq(&business.0, source)
                    .then_some(business as *const Business as usize);
            }
            current = error.source();
        }
        None
    }
    async fn run(mode: usize, factory: bool, fail: bool, hostile: bool) {
        native_sources::logging();
        let e = Engine::with_settings(mode == 3, mode, Duration::from_secs(3)).await;
        let root = e.root().await;
        let parent_stats = Arc::new(Stats::default());
        let child_stats = Arc::new(Stats::default());
        let decisions = parent_stats.clone();
        let parent = make(
            &root,
            parent_stats.clone(),
            factory,
            SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(30),
                decider: BasicDecisionFn::new(move |_| {
                    decisions.decisions.fetch_add(1, Ordering::SeqCst);
                    if factory {
                        SupervisionDecision::Restart
                    } else {
                        SupervisionDecision::Stop
                    }
                }),
            }),
        )
        .await;
        let source = Arc::new(Source {
            hostile,
            formats: AtomicUsize::new(0),
            sources: AtomicUsize::new(0),
        });
        let business = anyhow::Error::new(Business(source.clone()));
        let pointer = business.downcast_ref::<Business>().unwrap() as *const Business as usize;
        if fail {
            *child_stats.cleanup_error.lock().unwrap() = Some(ActorError::Other(business));
        } else {
            drop(business);
        }
        let child = make(
            &parent,
            child_stats.clone(),
            false,
            policy(BasicDecisionFn::new(|_| SupervisionDecision::Stop)),
        )
        .await;
        child.send(Box::new(Query)).await.unwrap();
        let watcher = parrot_api::death_watch::DeathWatch::new(8);
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        watcher.set_recipient(Arc::new(Events(tx))).unwrap();
        let watched = parent.death_watch().unwrap();
        let generation = watched.id();
        watched.watch(&watcher).unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(3), parent.send(Box::new(Fail))).await;
        let recovery = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if factory && !fail {
                    if parent_stats.built.load(Ordering::SeqCst) == 2
                        && parent.send(Box::new(Query)).await.is_ok()
                    {
                        break;
                    }
                } else if e.count() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        let child_result = child.stop().await;
        let root_live = root.send(Box::new(Query)).await.is_ok();
        // Finish explicit cleanup before reporting a failed negative control.
        let root_stop = root.stop().await;
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv()).await;
        let remaining = e.count();
        match e {
            Engine::Thread(s) => {
                let _ = s.shutdown_internal().await;
            }
            Engine::Actix(s) => {
                let _ = s.shutdown().await;
            }
        }
        assert!(reply.expect("failed ASK must complete").is_err());
        assert!(
            recovery,
            "failed subtree must retire autonomously; healthy factory must recover"
        );
        assert!(root_live);
        assert!(
            root_stop.is_ok(),
            "already retired subtree cannot poison open root"
        );
        assert_eq!(remaining, 0);
        assert_eq!(parent_stats.decisions.load(Ordering::SeqCst), 1);
        assert_eq!(
            parent_stats.built.load(Ordering::SeqCst),
            if factory && !fail { 2 } else { 1 }
        );
        assert_eq!(
            parent_stats.drops.load(Ordering::SeqCst),
            parent_stats.built.load(Ordering::SeqCst)
        );
        assert_eq!(child_stats.hooks.load(Ordering::SeqCst), 1);
        assert_eq!(child_stats.drops.load(Ordering::SeqCst), 1);
        if fail {
            assert_eq!(
                identity(
                    &child_result.expect_err("child stop must retain cleanup cause"),
                    &source
                ),
                Some(pointer)
            );
        } else {
            assert!(child_result.is_ok());
        }
        let event = event.unwrap().unwrap();
        assert_eq!(event.generation, generation);
        assert_eq!(event.path, parent.path());
        if fail {
            assert!(matches!(
                event.reason,
                parrot_api::supervisor::DeathReason::Failed(_)
            ));
        } else if factory {
            assert!(matches!(
                event.reason,
                parrot_api::supervisor::DeathReason::Normal
            ));
        } else {
            assert!(matches!(
                event.reason,
                parrot_api::supervisor::DeathReason::Failed(_)
            ));
        }
        assert!(events.try_recv().is_err());
        assert_eq!(source.formats.load(Ordering::SeqCst), 0);
        assert_eq!(source.sources.load(Ordering::SeqCst), 0);
    }
    macro_rules! cases {($($name:ident:$mode:expr,$factory:expr,$fail:expr,$hostile:expr;)*)=>{$(
        #[actix::test] async fn $name(){run($mode,$factory,$fail,$hostile).await;}
    )*};}
    cases! {
        shared_instance_healthy:0,false,false,false;
        dedicated_instance_healthy:1,false,false,false;
        sharded_instance_healthy:2,false,false,false;
        actix_instance_healthy:3,false,false,false;
        shared_factory_healthy:0,true,false,false;
        dedicated_factory_healthy:1,true,false,false;
        sharded_factory_healthy:2,true,false,false;
        actix_factory_healthy:3,true,false,false;
        shared_instance_source:0,false,true,false;
        dedicated_instance_source:1,false,true,false;
        sharded_instance_source:2,false,true,false;
        actix_instance_source:3,false,true,false;
        shared_factory_source:0,true,true,false;
        dedicated_factory_source:1,true,true,false;
        sharded_factory_source:2,true,true,false;
        actix_factory_source:3,true,true,false;
        shared_instance_hostile:0,false,true,true;
        dedicated_instance_hostile:1,false,true,true;
        sharded_instance_hostile:2,false,true,true;
        actix_instance_hostile:3,false,true,true;
        shared_factory_hostile:0,true,true,true;
        dedicated_factory_hostile:1,true,true,true;
        sharded_factory_hostile:2,true,true,true;
        actix_factory_hostile:3,true,true,true;
    }
}
