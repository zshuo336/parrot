//! Real Local owners retain creation admission through both init stages and disposal.
use super::*;
use crate::actix::{
    ActixSystemSettings, ChildInstanceAdmissionFailure, typed::FactoryAdmissionError,
};
use crate::typed_context::TypedContextExt;
use parrot_api::{
    local_typed::{LocalBoxedFuture, LocalMsgVariant},
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    typed_context::TypedContext,
};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

const WAIT: Duration = Duration::from_secs(5);
#[derive(Default)]
struct Counts {
    built: AtomicUsize,
    actors: AtomicUsize,
    captures: AtomicUsize,
    initialized: Notify,
    streams: Notify,
}
#[derive(Default)]
struct Gate {
    entered: Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(10), |open| {
                !*open
            })
            .unwrap();
        assert!(*open, "observer must release actual Local retirement");
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Vec<Arc<Gate>>);
impl Drop for Release {
    fn drop(&mut self) {
        for gate in &self.0 {
            gate.release();
        }
    }
}
#[derive(Clone, Default)]
struct Options {
    init: Option<Arc<Semaphore>>,
    streams: Option<Arc<Semaphore>>,
    actor_drop: Option<Arc<Gate>>,
    stream_drop: Option<Arc<Gate>>,
    capture_drop: Option<Arc<Gate>>,
    fail_init: bool,
    fail_streams: bool,
    panic_drop: bool,
    child_in_init: Option<Arc<Counts>>,
}
struct Probe {
    local: Rc<()>,
    owner: std::thread::ThreadId,
    counts: Arc<Counts>,
    options: Options,
}
impl Drop for Probe {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
        assert_eq!(Rc::strong_count(&self.local), 1);
        if let Some(gate) = &self.options.actor_drop {
            gate.block();
        }
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
        assert!(!self.options.panic_drop, "Local actor destructor receipt");
    }
}
struct Capture(Arc<Counts>, Option<Arc<Gate>>);
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(gate) = &self.1 {
            gate.block();
        }
        self.0.captures.fetch_add(1, Ordering::SeqCst);
    }
}
struct RetainedStream {
    local: Rc<()>,
    owner: std::thread::ThreadId,
    gate: Arc<Gate>,
}
impl futures::Stream for RetainedStream {
    type Item = ();
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<()>> {
        std::task::Poll::Pending
    }
}
impl Drop for RetainedStream {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
        assert_eq!(Rc::strong_count(&self.local), 1);
        self.gate.block();
    }
}
struct Read;
impl parrot_api::Message for Read {
    type Result = TypedContext;
}
impl LocalMsgVariant<Read> for Probe {
    fn inject(_: Read) -> bool {
        false
    }
    fn extract(reply: TypedContext) -> ActorResult<TypedContext> {
        Ok(reply)
    }
}
struct Crash;
impl parrot_api::Message for Crash {
    type Result = TypedContext;
}
impl LocalMsgVariant<Crash> for Probe {
    fn inject(_: Crash) -> bool {
        true
    }
    fn extract(reply: TypedContext) -> ActorResult<TypedContext> {
        Ok(reply)
    }
}
impl LocalTypedDispatch for Probe {
    type Msg = bool;
    type Reply = TypedContext;
    fn init<'a>(&'a mut self, context: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.initialized.notify_one();
            if let Some(counts) = &self.options.child_in_init {
                let result = context
                    .spawn_local_typed_child_factory::<Probe, Read, Make>(source(
                        counts,
                        Options::default(),
                    ))
                    .await;
                let error = rejection(result);
                assert_eq!(error.reason, ChildInstanceAdmissionFailure::Full);
                drop(error.into_inputs());
            }
            if let Some(init) = &self.options.init {
                init.acquire().await.unwrap().forget();
            }
            if self.options.fail_init {
                Err(ActorError::InitializationError(
                    "Local actor init failed".into(),
                ))
            } else {
                Ok(())
            }
        })
    }
    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        context: &'a mut LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if let Some(gate) = &self.options.stream_drop {
                context
                    .add_stream(RetainedStream {
                        local: Rc::new(()),
                        owner: std::thread::current().id(),
                        gate: gate.clone(),
                    })
                    .unwrap();
            }
            self.counts.streams.notify_one();
            if let Some(streams) = &self.options.streams {
                streams.acquire().await.unwrap().forget();
            }
            if self.options.fail_streams {
                Err(ActorError::InitializationError(
                    "Local stream init failed".into(),
                ))
            } else {
                Ok(())
            }
        })
    }
    fn dispatch<'a>(
        &'a mut self,
        crash: bool,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<TypedContext>> {
        Box::pin(async move {
            if crash {
                Err(ActorError::InternalError("replace Local generation".into()))
            } else {
                Ok(context.clone())
            }
        })
    }
}

impl parrot_api::engine::LocalActor for Probe {
    fn init<'a>(&'a mut self, context: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        LocalTypedDispatch::init(self, context)
    }
    fn receive<'a>(
        &'a mut self,
        _: parrot_api::types::BoxedMessage,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
        Box::pin(async move { Ok(Box::new(context.clone()) as parrot_api::types::BoxedMessage) })
    }
}
type Make = Box<dyn Fn() -> Probe + Send>;
fn source(counts: &Arc<Counts>, options: Options) -> Make {
    let capture = Capture(counts.clone(), options.capture_drop.clone());
    let serial = Cell::new(0); // Send-only constructors are part of the contract.
    Box::new(move || {
        std::hint::black_box(&capture);
        serial.set(serial.get() + 1);
        capture.0.built.fetch_add(1, Ordering::SeqCst);
        Probe {
            local: Rc::new(()),
            owner: std::thread::current().id(),
            counts: capture.0.clone(),
            options: options.clone(),
        }
    })
}
async fn system() -> Arc<ActixActorSystem> {
    let mut system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 2,
        shutdown_timeout: WAIT,
        ..Default::default()
    })
    .await
    .unwrap();
    system.instance_admission = Arc::new(Semaphore::new(1));
    Arc::new(system)
}
fn stop_policy() -> OneForOneStrategy {
    OneForOneStrategy {
        max_restarts: 0,
        within: WAIT,
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Stop),
    }
}
async fn root(
    system: &Arc<ActixActorSystem>,
    make: Make,
    path: &str,
    repeatable: bool,
) -> ActorResult<LocalTypedActorRef<Probe, Read>> {
    if repeatable {
        system
            .spawn_local_typed_factory_with_strategy(make, path, stop_policy())
            .await
    } else {
        system.spawn_local_typed(make, path).await
    }
}
async fn until(check: impl Fn() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !check() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("contract progress deadline");
}
fn rejection<T>(result: ActorResult<T>) -> FactoryAdmissionError<Make> {
    match result {
        Err(ActorError::Other(error)) => error.downcast::<FactoryAdmissionError<Make>>().unwrap(),
        _ => panic!("recoverable Local child admission error expected"),
    }
}

async fn successful_two_stage_init(repeatable: bool) {
    let system = system().await;
    let counts = Arc::new(Counts::default());
    let init = Arc::new(Semaphore::new(0));
    let streams = Arc::new(Semaphore::new(0));
    let first = root(
        &system,
        source(
            &counts,
            Options {
                init: Some(init.clone()),
                streams: Some(streams.clone()),
                ..Default::default()
            },
        ),
        "/first",
        repeatable,
    )
    .await
    .unwrap();
    counts.initialized.notified().await;
    assert_eq!(system.instance_admission.available_permits(), 0);
    let second_counts = Arc::new(Counts::default());
    let clone = Arc::new(system.downgrade().upgrade().unwrap());
    let mut second = Box::pin(root(
        &clone,
        source(&second_counts, Options::default()),
        "/second",
        !repeatable,
    ));
    assert!(futures::poll!(&mut second).is_pending());
    init.add_permits(1);
    counts.streams.notified().await;
    assert_eq!(
        system.instance_admission.available_permits(),
        0,
        "actor init alone released admission"
    );
    assert_eq!(second_counts.built.load(Ordering::SeqCst), 0);
    streams.add_permits(1);
    first.ask(Read).await.unwrap();
    let second = second.await.unwrap();
    second.ask(Read).await.unwrap();
    until(|| system.instance_admission.available_permits() == 1).await;
    assert_eq!(system.status().active_actors, 2);
    second.stop().await.unwrap();
    first.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
}
#[actix::test]
async fn one_shot_holds_shared_admission_through_actor_and_stream_init() {
    successful_two_stage_init(false).await;
}
#[actix::test]
async fn repeatable_releases_source_and_control_only_after_both_init_stages() {
    successful_two_stage_init(true).await;
}

async fn failed_init_retirement(repeatable: bool, streams: bool) {
    let system = system().await;
    let counts = Arc::new(Counts::default());
    let actor_drop = Arc::new(Gate::default());
    let stream_drop = Arc::new(Gate::default());
    let capture_drop = Arc::new(Gate::default());
    let release = Release(vec![
        stream_drop.clone(),
        actor_drop.clone(),
        capture_drop.clone(),
    ]);
    let actor = root(
        &system,
        source(
            &counts,
            Options {
                fail_init: !streams,
                fail_streams: streams,
                panic_drop: true,
                actor_drop: Some(actor_drop.clone()),
                stream_drop: streams.then(|| stream_drop.clone()),
                capture_drop: repeatable.then(|| capture_drop.clone()),
                ..Default::default()
            },
        ),
        "/failed",
        repeatable,
    )
    .await
    .unwrap();
    if streams {
        tokio::time::timeout(WAIT, stream_drop.entered.notified())
            .await
            .unwrap();
        assert_eq!(
            system.instance_admission.available_permits(),
            0,
            "failed stream init released before actual stream disposal"
        );
        stream_drop.release();
    }
    tokio::time::timeout(WAIT, actor_drop.entered.notified())
        .await
        .unwrap();
    assert_eq!(
        system.instance_admission.available_permits(),
        0,
        "failed init released before actual actor disposal"
    );
    actor_drop.release();
    if repeatable {
        capture_drop.entered.notified().await;
        assert_eq!(
            system.instance_admission.available_permits(),
            0,
            "capture still retains failed creation"
        );
        capture_drop.release();
    }
    let error = actor
        .stop()
        .await
        .expect_err("actual destructor panic must survive retirement");
    assert!(
        error.to_string().contains("Local actor destructor receipt"),
        "{error}"
    );
    until(|| system.instance_admission.available_permits() == 1).await;
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    drop(release);
    let _ = system.as_ref().clone().shutdown().await;
}
#[actix::test]
async fn one_shot_actor_init_failure_keeps_permit_until_actual_destructor() {
    failed_init_retirement(false, false).await;
}
#[actix::test]
async fn one_shot_stream_init_failure_keeps_permit_until_actual_destructor() {
    failed_init_retirement(false, true).await;
}
#[actix::test]
async fn repeatable_actor_init_failure_keeps_permit_through_capture_retirement() {
    failed_init_retirement(true, false).await;
}
#[actix::test]
async fn repeatable_stream_init_failure_keeps_permit_through_capture_retirement() {
    failed_init_retirement(true, true).await;
}

#[actix::test]
async fn local_child_rejections_return_exact_send_only_constructor_and_policy_for_retry() {
    for repeatable in [false, true] {
        let system = system().await;
        let parent = root(
            &system,
            source(&Arc::default(), Options::default()),
            "/parent",
            false,
        )
        .await
        .unwrap();
        let context = parent.ask(Read).await.unwrap();
        for closed in [false, true] {
            let counts = Arc::new(Counts::default());
            let held = system
                .instance_admission
                .clone()
                .acquire_owned()
                .await
                .unwrap();
            if closed {
                system.instance_admission.close();
            }
            let factory = source(&counts, Options::default());
            let address = &*factory as *const _ as *const () as usize;
            let strategy = SupervisorStrategyType::OneForOne(stop_policy());
            let result = if repeatable {
                context
                    .spawn_local_typed_child_factory_with_strategy::<Probe, Read, Make>(
                        factory, strategy,
                    )
                    .await
            } else {
                context
                    .spawn_local_typed_child_with_strategy::<Probe, Read, Make>(factory, strategy)
                    .await
            };
            let error = rejection(result);
            assert_eq!(
                error.reason,
                if closed {
                    ChildInstanceAdmissionFailure::ShuttingDown
                } else {
                    ChildInstanceAdmissionFailure::Full
                }
            );
            let inputs = error.into_inputs();
            assert_eq!(&*inputs.factory as *const _ as *const () as usize, address);
            assert_eq!(counts.built.load(Ordering::SeqCst), 0);
            assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
            drop(held);
            if closed {
                drop(inputs);
            } else {
                let child = if repeatable {
                    context
                        .spawn_local_typed_child_factory_with_strategy::<Probe, Read, Make>(
                            inputs.factory,
                            inputs.strategy.unwrap(),
                        )
                        .await
                        .unwrap()
                } else {
                    context
                        .spawn_local_typed_child_with_strategy::<Probe, Read, Make>(
                            inputs.factory,
                            inputs.strategy.unwrap(),
                        )
                        .await
                        .unwrap()
                };
                child.ask(Read).await.unwrap();
                child.stop().await.unwrap();
            }
            assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
            if closed {
                break;
            }
        }
        parent.stop().await.unwrap();
        system.as_ref().clone().shutdown().await.unwrap();
    }
}

#[actix::test]
async fn parent_init_rejects_local_child_without_waiting_on_its_own_permit() {
    let system = system().await;
    let child_counts = Arc::new(Counts::default());
    let parent = root(
        &system,
        source(
            &Arc::default(),
            Options {
                child_in_init: Some(child_counts.clone()),
                ..Default::default()
            },
        ),
        "/parent",
        true,
    )
    .await
    .unwrap();
    tokio::time::timeout(WAIT, parent.ask(Read))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child_counts.built.load(Ordering::SeqCst), 0);
    assert_eq!(child_counts.captures.load(Ordering::SeqCst), 1);
    parent.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn cancelled_duplicate_path_retains_capacity_through_actual_capture_disposal() {
    let system = system().await;
    let actor = root(
        &system,
        source(&Arc::default(), Options::default()),
        "/same",
        false,
    )
    .await
    .unwrap();
    actor.ask(Read).await.unwrap();
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(vec![gate.clone()]);
    let mut duplicate = Box::pin(root(
        &system,
        source(
            &counts,
            Options {
                capture_drop: Some(gate.clone()),
                ..Default::default()
            },
        ),
        "/same",
        true,
    ));
    assert!(futures::poll!(&mut duplicate).is_pending());
    gate.entered.notified().await;
    drop(duplicate);
    assert_eq!(system.instance_admission.available_permits(), 0);
    assert_eq!(counts.built.load(Ordering::SeqCst), 0);
    drop(release);
    until(|| system.instance_admission.available_permits() == 1).await;
    actor.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
}

#[test]
fn queued_local_creation_uses_original_deadline_after_original_host_exits() {
    let host = actix::System::new();
    let mut system = host
        .block_on(ActixActorSystem::with_arbiter_count(1))
        .unwrap();
    system.operation_timeouts.actor_creation = Duration::from_millis(50);
    system.instance_admission = Arc::new(Semaphore::new(0));
    let system = Arc::new(system);
    let counts = Arc::new(Counts::default());
    let mut first = Box::pin(root(
        &system,
        source(&counts, Options::default()),
        "/one-shot",
        false,
    ));
    let mut second = Box::pin(root(
        &system,
        source(&counts, Options::default()),
        "/repeatable",
        true,
    ));
    host.block_on(async {
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
    });
    drop(host);
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    observer.block_on(async {
        let (first, second) = tokio::time::timeout(WAIT, async { tokio::join!(first, second) })
            .await
            .unwrap();
        assert!(matches!(first, Err(ActorError::TimeoutDetail(_))));
        assert!(matches!(second, Err(ActorError::TimeoutDetail(_))));
        until(|| counts.captures.load(Ordering::SeqCst) == 2).await;
        assert_eq!(counts.built.load(Ordering::SeqCst), 0);
        assert_eq!(system.status().active_actors, 0);
        system.as_ref().clone().shutdown().await.unwrap();
    });
}

#[actix::test]
async fn local_reconstruction_does_not_reacquire_initial_creation_capacity() {
    let system = system().await;
    let counts = Arc::new(Counts::default());
    let actor = system
        .spawn_local_typed_factory::<Probe, Read, Make>(
            source(&counts, Options::default()),
            "/recover",
        )
        .await
        .unwrap();
    actor.ask(Read).await.unwrap();
    let held = system
        .instance_admission
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    assert!(actor.ref_for::<Crash>().ask(Crash).await.is_err());
    tokio::time::timeout(WAIT, actor.ask(Read))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(counts.built.load(Ordering::SeqCst), 2);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(system.instance_admission.available_permits(), 0);
    drop(held);
    actor.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
    assert_eq!(counts.actors.load(Ordering::SeqCst), 2);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
}

struct Native;
type NativeContext = crate::actix::ActixContext<crate::actix::ActixActor<Native>>;
impl parrot_api::actor::ActixEngineExt for Native {}
impl parrot_api::actor::Actor for Native {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = NativeContext;
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        _: parrot_api::types::BoxedMessage,
        context: &'a mut NativeContext,
    ) -> parrot_api::types::BoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
        Box::pin(async move { Ok(Box::new(context.clone()) as parrot_api::types::BoxedMessage) })
    }
}

async fn neutral_rejection(
    system: &Arc<ActixActorSystem>,
    spawner: &dyn parrot_api::engine::AsyncLocalActorSpawner,
    closed: bool,
) {
    use parrot_api::engine::LocalRuntimeFactory;
    let held = if closed {
        None
    } else {
        Some(
            system
                .instance_admission
                .clone()
                .acquire_owned()
                .await
                .unwrap(),
        )
    };
    let counts = Arc::new(Counts::default());
    let make = source(&counts, Options::default());
    let factory: LocalRuntimeFactory = Box::new(move || Box::new(make()));
    let address = &*factory as *const _ as *const () as usize;
    let policies = Arc::new(Counts::default());
    let capture = Capture(policies.clone(), None);
    let policy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| {
            std::hint::black_box(&capture);
            SupervisionDecision::Stop
        }),
    });
    let error = match spawner.spawn_local_factory(factory, Some(policy)).await {
        Err(ActorError::Other(error)) => error
            .downcast::<FactoryAdmissionError<LocalRuntimeFactory>>()
            .unwrap(),
        _ => panic!("original neutral Local factory must be returned"),
    };
    assert_eq!(
        error.reason,
        if closed {
            ChildInstanceAdmissionFailure::ShuttingDown
        } else {
            ChildInstanceAdmissionFailure::Full
        }
    );
    let inputs = error.into_inputs();
    assert_eq!(&*inputs.factory as *const _ as *const () as usize, address);
    assert_eq!(counts.built.load(Ordering::SeqCst), 0);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
    assert_eq!(policies.captures.load(Ordering::SeqCst), 0);
    drop(held);
    if closed {
        drop(inputs);
    } else {
        let child = spawner
            .spawn_local_factory(inputs.factory, inputs.strategy)
            .await
            .unwrap();
        child.send(Box::new(())).await.unwrap();
        child.stop().await.unwrap();
        assert_eq!(counts.built.load(Ordering::SeqCst), 1);
        assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    }
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    assert_eq!(policies.captures.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn native_and_typed_parent_return_neutral_factory_before_adapter_conversion() {
    use parrot_api::context::ActorContext;
    let system = system().await;
    let local = root(
        &system,
        source(&Arc::default(), Options::default()),
        "/parent",
        false,
    )
    .await
    .unwrap();
    let typed = local.ask(Read).await.unwrap();
    let native = system
        .spawn_root_typed(Native, parrot_api::actor::EmptyConfig)
        .await
        .unwrap();
    let context = *native
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<NativeContext>()
        .unwrap();
    neutral_rejection(&system, typed.local_spawner().unwrap(), false).await;
    neutral_rejection(&system, context.local_spawner().unwrap(), false).await;
    system.instance_admission.close();
    neutral_rejection(&system, typed.local_spawner().unwrap(), true).await;
    neutral_rejection(&system, context.local_spawner().unwrap(), true).await;
    native.stop().await.unwrap();
    local.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}
