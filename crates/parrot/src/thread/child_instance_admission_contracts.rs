use super::*;
use crate::thread::{
    InstanceAdmissionFailure, SchedulingMode, ThreadActorSystemConfig,
    system::{ErasedSpawnable, TypedSpawnPayload},
};
use parrot_api::{context::ActorSpawner, trace::TraceContext};
use std::{
    any::Any,
    sync::{Condvar, atomic::AtomicUsize},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

struct Echo;
struct Spawn {
    actor: BoxedMessage,
    config: BoxedMessage,
    strategy: Option<SupervisorStrategyType>,
    reply: oneshot::Sender<ActorResult<BoxedActorRef>>,
}
impl Actor for Echo {
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
        Box::pin(async move {
            match message.downcast::<Spawn>() {
                Ok(request) => {
                    let result = if let Some(strategy) = request.strategy {
                        ctx.spawn_with_strategy(request.actor, request.config, strategy)
                            .await
                    } else {
                        ctx.spawn(request.actor, request.config).await
                    };
                    let _ = request.reply.send(result);
                    Ok(Box::new(()) as BoxedMessage)
                }
                Err(message) => Ok(message),
            }
        })
    }
}
struct Bridge(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl ErasedSpawnable for Bridge {
    fn accepts_child_config(&self, config: &BoxedMessage) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        config.is::<EmptyConfig>()
    }
    fn spawn_child_on(
        self: Box<Self>,
        system: Arc<ThreadActorSystem>,
        engine: ThreadActorConfig,
        parent: BoxedActorRef,
        receipt: ChildSpawnReceipt,
    ) -> ActorResult<BoxedActorRef> {
        Box::new(TypedSpawnPayload::new(Echo)).spawn_child_on(system, engine, parent, receipt)
    }
    async fn spawn_on(
        self: Box<Self>,
        _: Arc<ThreadActorSystem>,
        _: BoxedMessage,
        _: ThreadActorConfig,
    ) -> ActorResult<BoxedActorRef> {
        panic!("child creation must use child publication");
    }
}
async fn setup() -> (
    Arc<ThreadActorSystem>,
    ThreadActorRef<Echo>,
    Arc<crate::children::ChildScope>,
) {
    setup_mode(ThreadActorSystemConfig::default().default_scheduling_mode).await
}
async fn setup_mode(
    mode: SchedulingMode,
) -> (
    Arc<ThreadActorSystem>,
    ThreadActorRef<Echo>,
    Arc<crate::children::ChildScope>,
) {
    setup_with_timeout(mode, Duration::ZERO).await
}
async fn setup_with_timeout(
    mode: SchedulingMode,
    creation: Duration,
) -> (
    Arc<ThreadActorSystem>,
    ThreadActorRef<Echo>,
    Arc<crate::children::ChildScope>,
) {
    let mut system = ThreadActorSystem::new(ThreadActorSystemConfig {
        default_scheduling_mode: mode,
        shared_pool_size: 2,
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    system.instance_admission = Arc::new(tokio::sync::Semaphore::new(1));
    if !creation.is_zero() {
        system.operation_timeouts.actor_creation = creation;
    }
    let system = Arc::new(system);
    system.set_self_weak(Arc::downgrade(&system));
    let parent = system
        .spawn_root_typed_thread(Echo, EmptyConfig)
        .await
        .unwrap();
    recovered(&system).await;
    let children = match system.registry.read().unwrap().get(&parent.path()).unwrap() {
        ActorRegistryEntry::Dynamic(entry) => entry.children.clone(),
        _ => panic!("real parent was not published"),
    };
    (system, parent, children)
}

fn modes() -> [SchedulingMode; 3] {
    [
        SchedulingMode::DedicatedThread,
        SchedulingMode::SharedPool {
            max_messages_per_run: 4,
        },
        SchedulingMode::Sharded {
            affinity_key: "child-admission".into(),
        },
    ]
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child lifecycle did not reach the expected state");
}
fn rejection(result: ActorResult<BoxedActorRef>) -> ChildInstanceAdmissionError {
    match result {
        Err(ActorError::Other(error)) => error
            .downcast::<ChildInstanceAdmissionError>()
            .expect("typed child admission cause was lost"),
        Err(error) => panic!("unexpected child failure: {error}"),
        Ok(_) => panic!("saturated child request was accepted"),
    }
}
async fn spawn_through_parent(
    parent: &ThreadActorRef<Echo>,
    actor: BoxedMessage,
    config: BoxedMessage,
    strategy: Option<SupervisorStrategyType>,
) -> ActorResult<BoxedActorRef> {
    let (reply, receive) = oneshot::channel();
    parent
        .deliver(Box::new(Spawn {
            actor,
            config,
            strategy,
            reply,
        }))
        .await
        .unwrap();
    tokio::time::timeout(WAIT, receive).await.unwrap().unwrap()
}

#[tokio::test]
async fn public_context_returns_overload_inputs_and_retries_real_child_on_all_schedulers() {
    for mode in modes() {
        let (system, parent, children) = setup_mode(mode).await;
        let held = system
            .try_spawn_typed_with_config(Echo, EmptyConfig, Default::default())
            .unwrap();
        let validation = Arc::new(AtomicUsize::new(0));
        let rejected = rejection(
            spawn_through_parent(
                &parent,
                Box::new(ErasedSpawnBox(Box::new(Bridge(validation.clone())))),
                Box::new(EmptyConfig),
                Some(SupervisorStrategyType::default()),
            )
            .await,
        );
        assert_eq!(rejected.reason, InstanceAdmissionFailure::Full);
        assert_eq!(validation.load(Ordering::SeqCst), 0);
        assert_eq!(system.actor_count(), 1);
        assert!(children.view().is_empty());
        let inputs = rejected.into_inputs();
        assert_eq!(inputs.parent.path(), parent.path());
        drop(held);
        recovered(&system).await;
        let child = spawn_through_parent(&parent, inputs.actor, inputs.config, inputs.strategy)
            .await
            .unwrap();
        assert_eq!(
            *child
                .send(Box::new(42_u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
        assert_eq!(validation.load(Ordering::SeqCst), 1);
        assert!(child.path().starts_with(&(parent.path() + "/")));
        assert_eq!(children.view().len(), 1);
        // Creation capacity is reusable while the successfully published child lives.
        recovered(&system).await;
        let probe = system
            .try_spawn_typed_with_config(Echo, EmptyConfig, Default::default())
            .unwrap();
        drop(probe);
        recovered(&system).await;
        parent.stop().await.unwrap();
        assert!(!child.is_alive().await);
        system.shutdown_internal().await.unwrap();
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
        let (_guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(10), |open| {
                !*open
            })
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "input retirement gate was never released"
        );
    }
    async fn observed(&self) {
        tokio::time::timeout(WAIT, self.entered.notified())
            .await
            .unwrap();
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
#[derive(Default)]
struct Counts {
    drops: [AtomicUsize; 4],
    contexts: Mutex<Vec<(usize, Option<TraceContext>)>>,
}
struct Input {
    counts: Arc<Counts>,
    index: usize,
    gate: Option<Arc<Gate>>,
    panic: bool,
}
impl Drop for Input {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.counts
            .contexts
            .lock()
            .unwrap()
            .push((self.index, TraceContext::current()));
        self.counts.drops[self.index].fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "child input {} retirement failed", self.index);
    }
}
struct ParentRef {
    inner: Arc<dyn ActorRef>,
    _input: Input,
}
impl std::fmt::Debug for ParentRef {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        panic!("child creation must not format its parent");
    }
}
impl ActorRef for ParentRef {
    fn send(&self, msg: BoxedMessage) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        self.inner.send(msg)
    }
    fn send_with_timeout(
        &self,
        msg: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        self.inner.send_with_timeout(msg, timeout)
    }
    fn deliver(&self, msg: BoxedMessage) -> BoxedFuture<'_, ActorResult<()>> {
        self.inner.deliver(msg)
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        self.inner.stop()
    }
    fn path(&self) -> String {
        panic!("child admission must use trusted path metadata");
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        self.inner.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        panic!("child admission must not clone the supplied parent");
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
fn inputs(
    parent: &ThreadActorRef<Echo>,
    counts: &Arc<Counts>,
    held: Option<(usize, Arc<Gate>)>,
    panic: bool,
) -> ChildInstanceAdmissionInputs {
    use parrot_api::supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision};
    let input = |index| Input {
        counts: counts.clone(),
        index,
        gate: held
            .as_ref()
            .filter(|(stage, _)| *stage == index)
            .map(|(_, gate)| gate.clone()),
        panic: panic && held.as_ref().is_some_and(|(stage, _)| *stage == index),
    };
    let capture = input(2);
    ChildInstanceAdmissionInputs {
        actor: Box::new(input(0)),
        config: Box::new(input(1)),
        strategy: Some(SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 1,
            within: WAIT,
            decider: BasicDecisionFn::new(move |_| {
                let _ = &capture;
                SupervisionDecision::Stop
            }),
        })),
        parent: Box::new(ParentRef {
            inner: Arc::new(parent.clone()),
            _input: input(3),
        }),
    }
}
fn identity(message: &BoxedMessage) -> *const () {
    message.as_ref() as *const dyn Any as *const ()
}

#[tokio::test]
async fn full_and_closed_admission_return_exact_inputs_without_user_callbacks_or_cleanup() {
    for closed in [false, true] {
        let (system, parent, children) = setup().await;
        let held = system
            .try_spawn_typed_with_config(Echo, EmptyConfig, Default::default())
            .unwrap();
        if closed {
            system.instance_admission.close();
        }
        let counts = Arc::new(Counts::default());
        let input = inputs(&parent, &counts, None, false);
        let actor_id = identity(&input.actor);
        let config_id = identity(&input.config);
        let parent_id = input.parent.as_ref() as *const dyn ActorRef as *const ();
        let error = rejection(
            system
                .spawn_child_erased(
                    input.actor,
                    input.config,
                    input.strategy,
                    input.parent,
                    parent.path(),
                    &children,
                )
                .await,
        );
        assert_eq!(
            error.reason,
            if closed {
                InstanceAdmissionFailure::ShuttingDown
            } else {
                InstanceAdmissionFailure::Full
            }
        );
        assert!(!error.to_string().is_empty());
        assert!(format!("{error:?}").contains("ChildInstanceAdmissionError"));
        assert!(
            counts
                .drops
                .iter()
                .all(|drop| drop.load(Ordering::SeqCst) == 0)
        );
        assert_eq!(system.actor_count(), 1);
        assert!(children.view().is_empty());
        let input = error.into_inputs();
        assert_eq!(identity(&input.actor), actor_id);
        assert_eq!(identity(&input.config), config_id);
        assert_eq!(
            input.parent.as_ref() as *const dyn ActorRef as *const (),
            parent_id
        );
        assert!(input.strategy.is_some());
        drop(input);
        assert!(
            counts
                .drops
                .iter()
                .all(|drop| drop.load(Ordering::SeqCst) == 1)
        );
        drop(held);
        system.shutdown_internal().await.unwrap();
    }
}

#[tokio::test]
async fn rejection_accepts_send_only_inputs() {
    let (system, parent, children) = setup().await;
    let held = system
        .try_spawn_typed_with_config(Echo, EmptyConfig, Default::default())
        .unwrap();
    let error = rejection(
        system
            .spawn_child_erased(
                Box::new(std::cell::Cell::new(31_u64)),
                Box::new(std::cell::Cell::new(47_u64)),
                None,
                parent.clone_boxed(),
                parent.path(),
                &children,
            )
            .await,
    );
    let input = error.into_inputs();
    assert_eq!(
        input
            .actor
            .downcast::<std::cell::Cell<u64>>()
            .unwrap()
            .get(),
        31
    );
    assert_eq!(
        input
            .config
            .downcast::<std::cell::Cell<u64>>()
            .unwrap()
            .get(),
        47
    );
    drop(held);
    recovered(&system).await;
    system.shutdown_internal().await.unwrap();
}

async fn cancel_with_held_input(mode: SchedulingMode, stage: usize, panic: bool, host_exit: bool) {
    let (system, parent, children) = setup_mode(mode).await;
    let factory = system
        .factory_slots
        .clone()
        .acquire_many_owned(system.factory_slots.available_permits() as u32)
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let input = inputs(&parent, &counts, Some((stage, gate.clone())), panic);
    let trace = TraceContext::root();
    let expected = (trace.trace_id(), trace.span_id());
    let owner = system.clone();
    let scope = children.clone();
    let path = parent.path();
    let work = parrot_api::trace::scope(Some(trace), async move {
        owner
            .spawn_child_erased(
                input.actor,
                input.config,
                input.strategy,
                input.parent,
                path,
                &scope,
            )
            .await
    });
    if host_exit {
        let state = system.clone();
        let host = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let caller = tokio::spawn(work);
                    until(|| state.actor_count() == 2).await;
                    caller.abort();
                    assert!(caller.await.unwrap_err().is_cancelled());
                });
        });
        tokio::time::timeout(
            WAIT,
            tokio::task::spawn_blocking(move || host.join().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
    } else {
        let caller = tokio::spawn(work);
        until(|| system.actor_count() == 2).await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
    }
    gate.observed().await;
    let stopped = tokio::spawn(async move { parent.stop().await });
    until(|| children.is_closed()).await;
    assert!(
        !stopped.is_finished(),
        "parent stop skipped an admitted input destructor"
    );
    assert_eq!(system.instance_admission.available_permits(), 0);
    let rejected = system
        .try_spawn_typed_with_config(Echo, EmptyConfig, Default::default())
        .err()
        .expect("child released capacity before actual retirement");
    assert_eq!(rejected.reason, InstanceAdmissionFailure::Full);
    drop(rejected);
    gate.release();
    let result = tokio::time::timeout(WAIT, stopped).await.unwrap().unwrap();
    if panic {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains(&format!("child input {stage} retirement failed"))
        );
    } else {
        result.unwrap();
    }
    recovered(&system).await;
    assert!(
        counts
            .drops
            .iter()
            .all(|drop| drop.load(Ordering::SeqCst) == 1)
    );
    assert_eq!(counts.contexts.lock().unwrap().len(), 4);
    for (_, context) in counts.contexts.lock().unwrap().iter() {
        let context = context.as_ref().expect("child input lost creation trace");
        assert_eq!((context.trace_id(), context.span_id()), expected);
    }
    drop(factory);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancellation_and_parent_stop_retain_capacity_until_all_four_inputs_really_retire() {
    for mode in modes() {
        for stage in 0..4 {
            cancel_with_held_input(mode.clone(), stage, false, false).await;
        }
    }
}

#[tokio::test]
async fn each_input_retirement_failure_survives_caller_cancellation_and_parent_join() {
    for stage in 0..4 {
        cancel_with_held_input(
            ThreadActorSystemConfig::default().default_scheduling_mode,
            stage,
            true,
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn host_exit_retains_parent_reference_retirement_and_failure_on_all_schedulers() {
    for mode in modes() {
        cancel_with_held_input(mode, 3, true, true).await;
    }
}

#[tokio::test]
async fn creation_timeout_does_not_release_any_input_or_parent_retirement_receipt() {
    for stage in 0..4 {
        let (system, parent, children) = setup_with_timeout(
            ThreadActorSystemConfig::default().default_scheduling_mode,
            Duration::from_millis(100),
        )
        .await;
        let counts = Arc::new(Counts::default());
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let input = inputs(&parent, &counts, Some((stage, gate.clone())), true);
        let owner = system.clone();
        let scope = children.clone();
        let path = parent.path();
        let caller = tokio::spawn(async move {
            owner
                .spawn_child_erased(
                    input.actor,
                    input.config,
                    input.strategy,
                    input.parent,
                    path,
                    &scope,
                )
                .await
        });
        gate.observed().await;
        let result = tokio::time::timeout(WAIT, caller).await.unwrap().unwrap();
        assert!(
            matches!(result, Err(ActorError::TimeoutDetail(_))),
            "creation deadline did not return its timeout receipt"
        );
        assert_eq!(system.instance_admission.available_permits(), 0);
        assert_eq!(system.actor_count(), 2);
        let stopped = tokio::spawn(async move { parent.stop().await });
        until(|| children.is_closed()).await;
        assert!(
            !stopped.is_finished(),
            "timeout detached an input from the parent join"
        );
        gate.release();
        let error = tokio::time::timeout(WAIT, stopped)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("child input {stage} retirement failed")),
            "{error}"
        );
        recovered(&system).await;
        assert!(
            counts
                .drops
                .iter()
                .all(|drop| drop.load(Ordering::SeqCst) == 1)
        );
        assert_eq!(system.actor_count(), 0);
        system.shutdown_internal().await.unwrap();
    }
}

#[tokio::test]
async fn parent_disposal_failure_is_returned_to_an_observing_creator() {
    let (system, parent, children) = setup().await;
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    gate.release();
    let input = inputs(&parent, &counts, Some((3, gate)), true);
    let error = system
        .spawn_child_erased(
            input.actor,
            input.config,
            input.strategy,
            input.parent,
            parent.path(),
            &children,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, ActorError::Panic(_)),
        "parent disposal lost its error category"
    );
    assert!(
        error
            .to_string()
            .contains("child input 3 retirement failed")
    );
    recovered(&system).await;
    assert!(
        counts
            .drops
            .iter()
            .all(|drop| drop.load(Ordering::SeqCst) == 1)
    );
    assert_eq!(system.actor_count(), 1);
    assert_eq!(
        *parent
            .send(Box::new(91_u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        91
    );
    system.shutdown_internal().await.unwrap();
}
async fn recovered(system: &ThreadActorSystem) {
    tokio::time::timeout(WAIT, async {
        while system.instance_admission.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("instance reservation was not released");
}

#[tokio::test]
async fn child_cannot_bypass_a_real_unpolled_root_reservation() {
    let (system, parent, children) = setup().await;
    let reserved = system
        .try_spawn_typed_with_config(Echo, EmptyConfig, Default::default())
        .unwrap();
    let validation = Arc::new(AtomicUsize::new(0));
    let result = tokio::time::timeout(
        WAIT,
        system.spawn_child_erased(
            Box::new(ErasedSpawnBox(Box::new(Bridge(validation.clone())))),
            Box::new(EmptyConfig),
            None,
            parent.clone_boxed(),
            parent.path(),
            &children,
        ),
    )
    .await
    .expect("saturated child admission must return without waiting");
    let rejected = result.is_err();
    if let Ok(child) = result {
        child.stop().await.unwrap();
    }
    drop(reserved);
    recovered(&system).await;
    system.shutdown_internal().await.unwrap();
    assert!(
        rejected,
        "child bypassed the shared instance creation capacity"
    );
    assert_eq!(validation.load(Ordering::SeqCst), 0);
}
