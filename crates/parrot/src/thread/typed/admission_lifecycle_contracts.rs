use super::admission_contracts::{Probe, Request, system, system_with_timeout};
use super::*;
use crate::typed_context::{TypedContext, TypedContextExt};
use parrot_api::{supervisor::*, trace::TraceContext};
use std::{
    cell::Cell,
    sync::{
        Condvar,
        atomic::{AtomicUsize, Ordering},
    },
};

const WAIT: Duration = Duration::from_secs(5);

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("lifecycle condition was never reached");
}
async fn recovered(system: &ThreadActorSystem) {
    until(|| system.instance_admission.available_permits() == 1).await;
}
fn probe() -> Probe {
    Probe {
        initialized: Arc::new(AtomicUsize::new(0)),
        init_gate: None,
    }
}
impl TypedReceive<Request> for Probe {
    fn receive_typed(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async { Ok(42) })
    }
}

#[tokio::test]
async fn all_static_root_adapters_share_real_dynamic_reservations() {
    use crate::thread::{tests_support::DummyActor, typed::SingleVariant};
    for entry in 0..5 {
        let system = system();
        let cohort = system.root_cohort(Default::default()).await.unwrap();
        let dynamic = system
            .try_spawn_typed_with_config(
                DummyActor,
                parrot_api::actor::EmptyConfig,
                Default::default(),
            )
            .unwrap();
        let actor = probe();
        let creation: BoxedFuture<'_, ActorResult<usize>> = match entry {
            0 => Box::pin(async {
                system
                    .spawn_typed::<_, Request>(actor, "/adapter")
                    .await?
                    .ask(Request)
                    .await
            }),
            1 => Box::pin(async {
                system
                    .spawn_typed_with_strategy::<_, Request>(actor, "/adapter", Default::default())
                    .await?
                    .ask(Request)
                    .await
            }),
            2 => Box::pin(async {
                system
                    .spawn_typed_in_cohort::<_, Request>(actor, "/adapter", &cohort)
                    .await?
                    .ask(Request)
                    .await
            }),
            3 => Box::pin(async {
                system
                    .spawn_typed_single::<_, Request>(actor, "/adapter")
                    .await?
                    .ask(SingleVariant::wrap(Request))
                    .await
            }),
            _ => Box::pin(async {
                system
                    .spawn_typed_single_with_strategy::<_, Request>(
                        actor,
                        "/adapter",
                        Default::default(),
                    )
                    .await?
                    .ask(SingleVariant::wrap(Request))
                    .await
            }),
        };
        tokio::pin!(creation);
        assert!(
            futures::poll!(&mut creation).is_pending(),
            "adapter {entry} bypassed dynamic admission"
        );
        assert_eq!(system.actor_count(), 0);
        drop(dynamic);
        assert_eq!(
            tokio::time::timeout(WAIT, creation).await.unwrap().unwrap(),
            42
        );
        recovered(&system).await;
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
        assert!(!timeout.timed_out(), "test must release destructor");
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
struct Record {
    token: Arc<()>,
    drops: Arc<AtomicUsize>,
    contexts: Arc<Mutex<Vec<Option<TraceContext>>>>,
    gate: Option<Arc<Gate>>,
    panic: bool,
}
impl Record {
    fn gated(mut self, gate: Option<Arc<Gate>>, panic: bool) -> Self {
        self.gate = gate;
        self.panic = panic;
        self
    }
}
impl Drop for Record {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.contexts.lock().unwrap().push(TraceContext::current());
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "static admission destructor failed");
    }
}
struct Tracked {
    record: Record,
    inner: Probe,
    fail_init: bool,
    // The public rejection must support Send actors that are not Sync.
    _send_only: Cell<usize>,
}
impl ParrotTypedDispatch for Tracked {
    type Msg = Request;
    type Reply = usize;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        if self.fail_init {
            Box::pin(async { Err(ActorError::InitializationError("static init failed".into())) })
        } else {
            self.inner.init()
        }
    }
    fn dispatch(&mut self, request: Request) -> BoxedFuture<'_, ActorResult<usize>> {
        self.inner.dispatch(request)
    }
}
impl ParrotMsgVariant<Tracked> for Request {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}
fn tracked(record: Record) -> Tracked {
    Tracked {
        record,
        inner: probe(),
        fail_init: false,
        _send_only: Cell::new(0),
    }
}
fn strategy(record: Record) -> SupervisorStrategyType {
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(1),
        decider: BasicDecisionFn::new(move |_| {
            let _ = &record;
            SupervisionDecision::Stop
        }),
    })
}

#[tokio::test]
async fn unpolled_and_capacity_cancelled_inputs_retire_once_with_creation_trace() {
    for polled in [false, true] {
        let system = system();
        let held = system
            .instance_admission
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let actor_record = Record::default();
        let policy_record = Record::default();
        let actor_drops = actor_record.drops.clone();
        let policy_drops = policy_record.drops.clone();
        let actor_contexts = actor_record.contexts.clone();
        let policy_contexts = policy_record.contexts.clone();
        let trace = TraceContext::root();
        let mut creation = Box::pin(parrot_api::trace::scope(Some(trace.clone()), async {
            let mut creation = Box::pin(system.spawn_typed_with_strategy::<_, Request>(
                tracked(actor_record),
                "/cancelled",
                strategy(policy_record),
            ));
            if polled {
                assert!(futures::poll!(&mut creation).is_pending());
            }
            drop(creation);
        }));
        assert!(futures::poll!(&mut creation).is_ready());
        drop(creation);
        until(|| {
            actor_drops.load(Ordering::SeqCst) == 1 && policy_drops.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(
            actor_contexts.lock().unwrap().as_slice(),
            &[Some(trace.clone())]
        );
        assert_eq!(policy_contexts.lock().unwrap().as_slice(), &[Some(trace)]);
        assert_eq!(system.actor_count(), 0);
        drop(held);
        recovered(&system).await;
        system.shutdown_internal().await.unwrap();
    }
}

#[tokio::test]
async fn timeout_and_shutdown_wait_for_both_rejected_input_destructors() {
    for shutdown in [false, true] {
        for stage in 0..2 {
            let system = system_with_timeout(Duration::from_millis(50));
            let held = system
                .instance_admission
                .clone()
                .acquire_owned()
                .await
                .unwrap();
            let gate = Arc::new(Gate::default());
            let _release = Release(gate.clone());
            let actor_record = Record::default().gated((stage == 0).then(|| gate.clone()), true);
            let policy_record = Record::default().gated((stage == 1).then(|| gate.clone()), true);
            let actor_drops = actor_record.drops.clone();
            let policy_drops = policy_record.drops.clone();
            let owner = system.clone();
            let caller = tokio::spawn(async move {
                owner
                    .spawn_typed_with_strategy::<_, Request>(
                        tracked(actor_record),
                        "/rejected",
                        strategy(policy_record),
                    )
                    .await
            });
            if shutdown {
                system.shutdown_internal().await.unwrap();
            }
            gate.observed().await;
            assert!(
                !caller.is_finished(),
                "rejection returned before real input disposal"
            );
            assert_eq!(system.actor_count(), 0);
            gate.release();
            let result = tokio::time::timeout(WAIT, caller).await.unwrap().unwrap();
            assert!(
                matches!(&result, Err(ActorError::InternalError(message)) if message.contains("static admission destructor failed")),
                "actual disposal failure must take precedence: {}",
                result.err().unwrap()
            );
            assert_eq!(actor_drops.load(Ordering::SeqCst), 1);
            assert_eq!(policy_drops.load(Ordering::SeqCst), 1);
            drop(held);
            recovered(&system).await;
            if !shutdown {
                system.shutdown_internal().await.unwrap();
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn capacity_wait_does_not_restart_static_initialization_deadline() {
    let system = system_with_timeout(Duration::from_secs(10));
    let slots = system
        .typed_slots
        .clone()
        .acquire_many_owned(system.typed_slots.available_permits() as u32)
        .await
        .unwrap();
    let held = system
        .instance_admission
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let mut creation = Box::pin(system.spawn_typed::<_, Request>(probe(), "/original-budget"));
    let expected = tokio::time::Instant::now() + Duration::from_secs(10);
    assert!(futures::poll!(&mut creation).is_pending());
    tokio::time::advance(Duration::from_secs(4)).await;
    drop(held);
    let reference = creation.await.unwrap();
    let control = control(&system, "/original-budget");
    assert_eq!(control.initialization_deadline(), Some(expected));
    // The independent retirement runtime has a separate clock.
    tokio::time::resume();
    control.request_stop(tokio::time::Instant::now() + WAIT);
    drop(slots);
    let _ = reference.stop().await;
    recovered(&system).await;
    system.shutdown_internal().await.unwrap();
}
fn control(system: &ThreadActorSystem, path: &str) -> Arc<TypedControl> {
    match system.registry.read().unwrap().get(path).unwrap() {
        ActorRegistryEntry::Typed(entry) => entry.control.clone(),
        _ => panic!("not a static actor"),
    }
}

#[tokio::test]
async fn failed_initialization_retains_admission_through_actor_and_policy_retirement() {
    for stage in 0..2 {
        for panic in [false, true] {
            let system = system();
            let gate = Arc::new(Gate::default());
            let _release = Release(gate.clone());
            let actor_record = Record::default().gated((stage == 0).then(|| gate.clone()), panic);
            let policy_record = Record::default().gated((stage == 1).then(|| gate.clone()), panic);
            let actor_drops = actor_record.drops.clone();
            let policy_drops = policy_record.drops.clone();
            let mut input = tracked(actor_record);
            input.fail_init = true;
            // Keep the failed reference alive: it must not keep capacity forever.
            let reference = system
                .spawn_typed_with_strategy::<_, Request>(
                    input,
                    "/failed-init",
                    strategy(policy_record),
                )
                .await
                .unwrap();
            gate.observed().await;
            let lifecycle = control(&system, "/failed-init");
            assert_eq!(system.instance_admission.available_permits(), 0);
            assert_eq!(system.actor_count(), 1);
            gate.release();
            recovered(&system).await;
            assert_eq!(actor_drops.load(Ordering::SeqCst), 1);
            assert_eq!(policy_drops.load(Ordering::SeqCst), 1);
            assert_eq!(system.actor_count(), 0);
            let error = await_stop(lifecycle.retirement())
                .await
                .expect_err("failed initialization has a terminal cause");
            let expected = if panic {
                "static admission destructor failed"
            } else {
                "static init failed"
            };
            assert!(error.to_string().contains(expected), "{error}");
            assert!(reference.ask(Request).await.is_err());
            let next = system
                .spawn_typed::<_, Request>(probe(), "/after-failure")
                .await
                .unwrap();
            assert_eq!(next.ask(Request).await.unwrap(), 42);
            system.shutdown_internal().await.unwrap();
        }
    }
}

struct Parent(Option<tokio::sync::oneshot::Sender<TypedContext>>);
impl ParrotTypedDispatch for Parent {
    type Msg = Request;
    type Reply = usize;
    fn init_with_context<'a>(
        &'a mut self,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.take().unwrap().send(context.clone()).ok().unwrap();
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async { Ok(42) })
    }
}
impl ParrotMsgVariant<Parent> for Request {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}
async fn parent(system: &Arc<ThreadActorSystem>) -> (TypedActorRef<Parent, Request>, TypedContext) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let reference = system
        .spawn_typed::<_, Request>(Parent(Some(tx)), "/parent")
        .await
        .unwrap();
    let context = tokio::time::timeout(WAIT, rx).await.unwrap().unwrap();
    assert_eq!(reference.ask(Request).await.unwrap(), 42);
    recovered(system).await;
    (reference, context)
}

#[tokio::test]
async fn real_static_context_returns_send_only_inputs_and_retries_them() {
    for explicit in [false, true] {
        let system = system();
        let (parent, context) = parent(&system).await;
        let held = system
            .instance_admission
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let record = Record::default();
        let identity = record.token.clone();
        let drops = record.drops.clone();
        let policy = Record::default();
        let policy_drops = policy.drops.clone();
        let policy_identity = policy.token.clone();
        let creation = if explicit {
            context.spawn_typed_child_with_strategy::<_, Request>(tracked(record), strategy(policy))
        } else {
            drop(policy);
            context.spawn_typed_child::<_, Request>(tracked(record))
        };
        let result = tokio::time::timeout(WAIT, creation).await.unwrap();
        let error = match result {
            Err(ActorError::Other(error)) => error
                .downcast::<TypedInstanceAdmissionError<Tracked>>()
                .unwrap(),
            _ => panic!("missing recoverable static rejection"),
        };
        assert_eq!(error.reason, crate::thread::InstanceAdmissionFailure::Full);
        assert!(!format!("{error:?}: {error}").is_empty());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        if explicit {
            assert_eq!(policy_drops.load(Ordering::SeqCst), 0);
            assert_eq!(Arc::strong_count(&policy_identity), 2);
        }
        assert_eq!(context.children().len(), 0);
        let inputs = error.into_inputs();
        assert!(Arc::ptr_eq(&identity, &inputs.actor.record.token));
        assert_eq!(inputs.actor.inner.initialized.load(Ordering::SeqCst), 0);
        drop(held);
        let child = match inputs.strategy {
            Some(policy) => {
                context
                    .spawn_typed_child_with_strategy::<_, Request>(inputs.actor, policy)
                    .await
            }
            None => context.spawn_typed_child::<_, Request>(inputs.actor).await,
        }
        .unwrap();
        assert_eq!(child.ask(Request).await.unwrap(), 42);
        recovered(&system).await;
        parent.stop().await.unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(policy_drops.load(Ordering::SeqCst), 1);
        system.shutdown_internal().await.unwrap();
    }
}

#[tokio::test]
async fn static_child_closed_admission_returns_original_actor_after_system_shutdown() {
    let system = system();
    let (_, context) = parent(&system).await;
    system.shutdown_internal().await.unwrap();
    let record = Record::default();
    let identity = record.token.clone();
    let drops = record.drops.clone();
    let error = match context
        .spawn_typed_child::<_, Request>(tracked(record))
        .await
    {
        Err(ActorError::Other(error)) => error
            .downcast::<TypedInstanceAdmissionError<Tracked>>()
            .unwrap(),
        _ => panic!("closed admission must return caller inputs"),
    };
    assert_eq!(
        error.reason,
        crate::thread::InstanceAdmissionFailure::ShuttingDown
    );
    let inputs = error.into_inputs();
    assert!(Arc::ptr_eq(&identity, &inputs.actor.record.token));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(inputs);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stopping_parent_joins_initializing_child_actual_retirement() {
    let system = system();
    let (parent, context) = parent(&system).await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let record = Record::default().gated(Some(gate.clone()), false);
    let drops = record.drops.clone();
    let mut input = tracked(record);
    input.inner.init_gate = Some(Arc::new(tokio::sync::Notify::new()));
    let child = context
        .spawn_typed_child::<_, Request>(input)
        .await
        .unwrap();
    assert_eq!(system.instance_admission.available_permits(), 0);
    let stopped = tokio::spawn(async move { parent.stop().await });
    gate.observed().await;
    assert!(
        !stopped.is_finished(),
        "parent detached static child retirement"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(system.instance_admission.available_permits(), 0);
    gate.release();
    tokio::time::timeout(WAIT, stopped)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    recovered(&system).await;
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(child.ask(Request).await.is_err());
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn capacity_timeout_and_shutdown_preserve_rejection_when_disposal_succeeds() {
    for shutdown in [false, true] {
        let system = system_with_timeout(Duration::from_millis(50));
        let held = system
            .instance_admission
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let record = Record::default();
        let drops = record.drops.clone();
        let owner = system.clone();
        let caller = tokio::spawn(async move {
            owner
                .spawn_typed::<_, Request>(tracked(record), "/not-admitted")
                .await
        });
        if shutdown {
            system.shutdown_internal().await.unwrap();
        }
        let result = tokio::time::timeout(WAIT, caller).await.unwrap().unwrap();
        if shutdown {
            assert!(matches!(result, Err(ActorError::Stopped)));
        } else {
            assert!(matches!(result, Err(ActorError::TimeoutDetail(_))));
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(system.actor_count(), 0);
        drop(held);
        if !shutdown {
            system.shutdown_internal().await.unwrap();
        }
    }
}

#[tokio::test]
async fn guardian_override_retirement_failure_or_cancellation_keeps_reservation() {
    for cancel in [false, true] {
        let system = system();
        system
            .root_cohorts
            .install_guardian(Default::default(), WAIT, system.cleanup_slots.clone())
            .await
            .unwrap();
        let actor_gate = Arc::new(Gate::default());
        let policy_gate = Arc::new(Gate::default());
        let _actor_release = Release(actor_gate.clone());
        let _policy_release = Release(policy_gate.clone());
        let actor_record = Record::default().gated(Some(actor_gate.clone()), true);
        let policy_record = Record::default().gated(Some(policy_gate.clone()), true);
        let actor_drops = actor_record.drops.clone();
        let policy_drops = policy_record.drops.clone();
        let owner = system.clone();
        let caller = tokio::spawn(async move {
            owner
                .spawn_typed_with_strategy::<_, Request>(
                    tracked(actor_record),
                    "/guardian-input",
                    strategy(policy_record),
                )
                .await
        });
        policy_gate.observed().await;
        assert_eq!(system.instance_admission.available_permits(), 0);
        assert_eq!(system.actor_count(), 0);
        if cancel {
            caller.abort();
        }
        policy_gate.release();
        actor_gate.observed().await;
        assert_eq!(system.instance_admission.available_permits(), 0);
        if !cancel {
            assert!(!caller.is_finished());
        }
        actor_gate.release();
        let result = tokio::time::timeout(WAIT, caller).await.unwrap();
        if cancel {
            assert!(result.err().expect("caller was cancelled").is_cancelled());
        } else {
            assert!(matches!(result.unwrap(), Err(ActorError::InternalError(_))));
        }
        recovered(&system).await;
        assert_eq!(actor_drops.load(Ordering::SeqCst), 1);
        assert_eq!(policy_drops.load(Ordering::SeqCst), 1);
        system.shutdown_internal().await.unwrap();
    }
}

#[tokio::test]
async fn init_timeout_and_stop_receipt_do_not_release_capacity_before_drop() {
    let system = system_with_timeout(Duration::from_millis(50));
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let record = Record::default().gated(Some(gate.clone()), true);
    let drops = record.drops.clone();
    let mut input = tracked(record);
    input.inner.init_gate = Some(Arc::new(tokio::sync::Notify::new()));
    let reference = system
        .spawn_typed::<_, Request>(input, "/init-timeout")
        .await
        .unwrap();
    let control = control(&system, "/init-timeout");
    gate.observed().await;
    control.request_stop(tokio::time::Instant::now() + Duration::from_millis(30));
    let stopped = tokio::time::timeout(WAIT, reference.stop()).await.unwrap();
    assert!(stopped.is_err());
    assert_eq!(system.instance_admission.available_permits(), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(system.actor_count(), 1);
    gate.release();
    recovered(&system).await;
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(system.actor_count(), 0);
    let error = await_stop(control.retirement()).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("static admission destructor failed"),
        "{error}"
    );
    assert!(
        reference.stop().await.is_err(),
        "timeout receipt must remain terminal"
    );
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn original_runtime_exit_keeps_failed_reference_reservation_until_retirement() {
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let record = Record::default().gated(Some(gate.clone()), true);
    let drops = record.drops.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (exit, exited) = std::sync::mpsc::channel();
    let host = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let pair = runtime.block_on(async {
            let system = system();
            let mut input = tracked(record);
            let initialized = input.inner.initialized.clone();
            input.inner.init_gate = Some(Arc::new(tokio::sync::Notify::new()));
            let reference = system
                .spawn_typed::<_, Request>(input, "/host-exit")
                .await
                .unwrap();
            until(|| initialized.load(Ordering::SeqCst) == 1).await;
            (system, reference)
        });
        tx.send(pair).ok().unwrap();
        let _ = exited.recv();
        runtime.shutdown_background();
    });
    let (system, reference) = tokio::time::timeout(WAIT, rx).await.unwrap().unwrap();
    let lifecycle = control(&system, "/host-exit");
    exit.send(()).unwrap();
    gate.observed().await;
    tokio::time::timeout(
        WAIT,
        tokio::task::spawn_blocking(move || host.join().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(system.instance_admission.available_permits(), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    gate.release();
    recovered(&system).await;
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(system.actor_count(), 0);
    let error = await_stop(lifecycle.retirement()).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("static admission destructor failed"),
        "{error}"
    );
    assert!(reference.ask(Request).await.is_err());
    let error = system.shutdown_internal().await.unwrap_err();
    assert!(
        matches!(&error, SystemError::ShutdownError(message) if message.contains("shared task unexpectedly cancelled")),
        "{error}"
    );
}
