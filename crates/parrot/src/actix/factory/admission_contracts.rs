//! Exercise real factory owners, parent contexts and physical initialization.
use super::*;
use crate::actix::{
    ActixSystemSettings, ChildInstanceAdmissionFailure, typed::FactoryAdmissionError,
};
use parrot_api::{
    actor::{ActorState, EmptyConfig},
    context::{ActorContext, ActorFactoryContext, ActorSpawnerExt},
    supervisor::{BasicDecisionFn, OneForOneStrategy},
};
use std::sync::{Condvar, atomic::AtomicUsize};
use tokio::sync::{Notify, Semaphore};

const WAIT: Duration = Duration::from_secs(5);
#[derive(Default)]
struct Counts {
    constructed: AtomicUsize,
    initialized: AtomicUsize,
    actors: AtomicUsize,
    captures: AtomicUsize,
    rejected: AtomicUsize,
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
        assert!(*open, "test must release blocked user code");
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
struct Capture {
    counts: Arc<Counts>,
    gate: Option<Arc<Gate>>,
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.counts.captures.fetch_add(1, Ordering::SeqCst);
    }
}
struct Request {
    factory: BoxedMessage,
    strategy: Option<SupervisorStrategyType>,
}
struct Crash;
struct Probe {
    counts: Arc<Counts>,
    init: Option<Arc<Semaphore>>,
    disposal: Option<Arc<Gate>>,
    fail: bool,
    panic_drop: bool,
    child_in_init: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(gate) = &self.disposal {
            gate.block();
        }
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "dynamic factory actor destructor receipt");
    }
}
impl ActixEngineExt for Probe {}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.initialized.fetch_add(1, Ordering::SeqCst);
            if self.child_in_init {
                let counts = self.counts.clone();
                let result = ctx.spawner().spawn_factory(move || probe(&counts)).await;
                let ActorError::Other(error) =
                    result.err().expect("parent init must not wait on itself")
                else {
                    panic!("recoverable factory error expected")
                };
                let error = error
                    .downcast::<FactoryAdmissionError<BoxedMessage>>()
                    .unwrap();
                assert_eq!(error.reason, ChildInstanceAdmissionFailure::Full);
                self.counts.rejected.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(init) = &self.init {
                init.acquire().await.unwrap().forget();
            }
            if self.fail {
                Err(ActorError::InitializationError(
                    "dynamic factory init receipt".into(),
                ))
            } else {
                Ok(())
            }
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<Crash>() {
                return Err(ActorError::InternalError("reconstruct factory".into()));
            }
            match message.downcast::<Request>() {
                Ok(request) => Ok(Box::new(
                    ctx.spawner()
                        .spawn_factory_erased(request.factory, request.strategy)
                        .await,
                ) as BoxedMessage),
                Err(message) => Ok(message),
            }
        })
    }
}
fn probe(counts: &Arc<Counts>) -> Probe {
    Probe {
        counts: counts.clone(),
        init: None,
        disposal: None,
        fail: false,
        panic_drop: false,
        child_in_init: false,
    }
}
fn source(
    counts: &Arc<Counts>,
    init: Option<Arc<Semaphore>>,
    actor_gate: Option<Arc<Gate>>,
    capture_gate: Option<Arc<Gate>>,
    fail: bool,
    panic_drop: bool,
) -> impl Fn() -> Probe + Send + Sync + 'static {
    let counts = counts.clone();
    let capture = Capture {
        counts: counts.clone(),
        gate: capture_gate,
    };
    move || {
        let _ = &capture;
        counts.constructed.fetch_add(1, Ordering::SeqCst);
        let mut actor = probe(&counts);
        actor.init = init.clone();
        actor.disposal = actor_gate.clone();
        actor.fail = fail;
        actor.panic_drop = panic_drop;
        actor
    }
}
fn stop_policy() -> OneForOneStrategy {
    OneForOneStrategy {
        max_restarts: 0,
        within: WAIT,
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Stop),
    }
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
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn entered(gate: &Gate) {
    tokio::time::timeout(WAIT, gate.entered.notified())
        .await
        .unwrap();
}
fn wrapped(factory: impl Fn() -> Probe + Send + Sync + 'static) -> BoxedMessage {
    <ActixContext<ActixActor<Probe>> as ActorFactoryContext<Probe>>::wrap_factory(factory)
}
async fn request(
    parent: &BoxedActorRef,
    factory: BoxedMessage,
    strategy: Option<SupervisorStrategyType>,
) -> ActorResult<BoxedActorRef> {
    *parent
        .send(Box::new(Request { factory, strategy }))
        .await
        .unwrap()
        .downcast::<ActorResult<BoxedActorRef>>()
        .unwrap()
}

pub struct StaticRead;
impl parrot_api::message::Message for StaticRead {
    type Result = ();
}
#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(StaticRead))]
struct StaticProbe;
impl parrot_api::typed::TypedReceive<StaticRead> for StaticProbe {
    fn receive_typed<'a>(&'a mut self, _: StaticRead) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn queued_creation_deadlines_survive_original_host_exit_for_all_send_root_sources() {
    let host = actix::System::new();
    let mut system = host
        .block_on(ActixActorSystem::with_arbiter_count(1))
        .unwrap();
    system.operation_timeouts.actor_creation = Duration::from_millis(100);
    system.instance_admission = Arc::new(Semaphore::new(0));
    let system = Arc::new(system);
    let counts = Arc::new(Counts::default());
    let mut factory =
        Box::pin(system.spawn_root_factory(source(&counts, None, None, None, false, false)));
    let mut instance = Box::pin(system.spawn_root_typed(probe(&counts), EmptyConfig));
    let mut typed = Box::pin(system.spawn_typed::<_, StaticRead>(StaticProbe, "/host-timer"));
    host.block_on(async {
        assert!(futures::poll!(&mut factory).is_pending());
        assert!(futures::poll!(&mut instance).is_pending());
        assert!(futures::poll!(&mut typed).is_pending());
    });
    drop(host);
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    observer.block_on(async {
        let (factory, instance, typed) = tokio::time::timeout(WAIT, async {
            tokio::join!(&mut factory, &mut instance, &mut typed)
        })
        .await
        .unwrap();
        assert!(matches!(factory, Err(ActorError::TimeoutDetail(_))));
        assert!(matches!(
            instance,
            Err(parrot_api::system::SystemError::ActorError(
                ActorError::TimeoutDetail(_)
            ))
        ));
        assert!(matches!(typed, Err(ActorError::TimeoutDetail(_))));
        until(|| {
            counts.actors.load(Ordering::SeqCst) == 1 && counts.captures.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(counts.constructed.load(Ordering::SeqCst), 0);
        assert_eq!(system.status().active_actors, 0);
        system.as_ref().clone().shutdown().await.unwrap();
    });
}

#[actix::test]
async fn all_dynamic_root_factory_entries_release_only_after_successful_physical_init() {
    for entry in 0..4 {
        let mut system = if entry == 3 {
            ActixActorSystem::with_guardian(
                ActixSystemSettings {
                    arbiter_count: 2,
                    shutdown_timeout: WAIT,
                    ..Default::default()
                },
                Default::default(),
            )
            .await
            .unwrap()
        } else {
            ActixActorSystem::with_arbiter_count(2).await.unwrap()
        };
        system.instance_admission = Arc::new(Semaphore::new(1));
        let system = Arc::new(system);
        let counts = Arc::new(Counts::default());
        let init = Arc::new(Semaphore::new(0));
        let make = source(&counts, Some(init.clone()), None, None, false, false);
        let cohort = system
            .root_cohort(SupervisorStrategyType::OneForOne(stop_policy()))
            .await
            .unwrap();
        let first = match entry {
            0 | 3 => system.spawn_root_factory(make).await,
            1 => {
                system
                    .spawn_root_factory_with_strategy(make, stop_policy())
                    .await
            }
            _ => {
                system
                    .spawn_root_factory_in_cohort(make, "/factory", &cohort)
                    .await
            }
        }
        .unwrap();
        until(|| counts.initialized.load(Ordering::SeqCst) == 1).await;
        assert_eq!(
            system.instance_admission.available_permits(),
            0,
            "entry={entry}"
        );
        let clone = Arc::new(system.downgrade().upgrade().unwrap());
        let second_counts = Arc::new(Counts::default());
        let mut second = Box::pin(clone.spawn_root_typed(probe(&second_counts), EmptyConfig));
        assert!(futures::poll!(&mut second).is_pending());
        assert_eq!(system.status().active_actors, 1);
        init.add_permits(1);
        first.send(Box::new(())).await.unwrap();
        let second = second.await.unwrap();
        second.send(Box::new(())).await.unwrap();
        until(|| system.instance_admission.available_permits() == 1).await;
        assert_eq!(system.status().active_actors, 2);
        second.stop().await.unwrap();
        first.stop().await.unwrap();
        cohort.stop().await.unwrap();
        system.as_ref().clone().shutdown().await.unwrap();
        assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    }
}

#[actix::test]
async fn overloaded_dynamic_child_returns_original_boxed_factory_for_parent_retry() {
    let system = system().await;
    let parent = system
        .spawn_root_typed(probe(&Arc::new(Counts::default())), EmptyConfig)
        .await
        .unwrap();
    parent.send(Box::new(())).await.unwrap();
    for closed in [false, true] {
        let occupied = system
            .instance_admission
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        if closed {
            system.instance_admission.close();
        }
        let counts = Arc::new(Counts::default());
        let factory = wrapped(source(&counts, None, None, None, false, false));
        let identity = factory.as_ref() as *const dyn Any as *const () as usize;
        let error = request(
            &parent,
            factory,
            Some(SupervisorStrategyType::OneForOne(stop_policy())),
        )
        .await
        .err()
        .unwrap();
        let ActorError::Other(error) = error else {
            panic!("recoverable factory error expected")
        };
        let error = error
            .downcast::<FactoryAdmissionError<BoxedMessage>>()
            .unwrap();
        assert_eq!(
            error.reason,
            if closed {
                ChildInstanceAdmissionFailure::ShuttingDown
            } else {
                ChildInstanceAdmissionFailure::Full
            }
        );
        let inputs = error.into_inputs();
        assert_eq!(
            inputs.factory.as_ref() as *const dyn Any as *const () as usize,
            identity
        );
        assert_eq!(counts.constructed.load(Ordering::SeqCst), 0);
        assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
        assert_eq!(system.status().active_actors, 1);
        drop(occupied);
        if closed {
            drop(inputs);
        } else {
            let child = request(&parent, inputs.factory, inputs.strategy)
                .await
                .unwrap();
            child.send(Box::new(())).await.unwrap();
            child.stop().await.unwrap();
            until(|| system.instance_admission.available_permits() == 1).await;
            assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
        }
        assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    }
    parent.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn dynamic_parent_init_rejects_child_without_self_wait_and_child_init_holds_capacity() {
    let system = system().await;
    let counts = Arc::new(Counts::default());
    let make = {
        let counts = counts.clone();
        move || {
            let mut actor = probe(&counts);
            actor.child_in_init = true;
            actor
        }
    };
    let parent = system.spawn_root_factory(make).await.unwrap();
    tokio::time::timeout(WAIT, parent.send(Box::new(())))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(counts.rejected.load(Ordering::SeqCst), 1);
    assert_eq!(system.status().active_actors, 1);
    let counts = Arc::new(Counts::default());
    let init = Arc::new(Semaphore::new(0));
    let child = request(
        &parent,
        wrapped(source(
            &counts,
            Some(init.clone()),
            None,
            None,
            false,
            false,
        )),
        None,
    )
    .await
    .unwrap();
    until(|| counts.initialized.load(Ordering::SeqCst) == 1).await;
    assert_eq!(system.instance_admission.available_permits(), 0);
    init.add_permits(1);
    child.send(Box::new(())).await.unwrap();
    until(|| system.instance_admission.available_permits() == 1).await;
    child.stop().await.unwrap();
    parent.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn dynamic_init_failure_retains_slot_through_actor_and_factory_capture_disposal() {
    let system = system().await;
    let counts = Arc::new(Counts::default());
    let actor_gate = Arc::new(Gate::default());
    let release_actor = Release(actor_gate.clone());
    let capture_gate = Arc::new(Gate::default());
    let release_capture = Release(capture_gate.clone());
    let actor = system
        .spawn_root_factory_with_strategy(
            source(
                &counts,
                None,
                Some(actor_gate.clone()),
                Some(capture_gate.clone()),
                true,
                true,
            ),
            stop_policy(),
        )
        .await
        .unwrap();
    entered(&actor_gate).await;
    assert_eq!(system.instance_admission.available_permits(), 0);
    drop(release_actor);
    entered(&capture_gate).await;
    assert_eq!(system.instance_admission.available_permits(), 0);
    drop(release_capture);
    let error = actor.stop().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("dynamic factory actor destructor receipt"),
        "{error:?}"
    );
    until(|| system.instance_admission.available_permits() == 1).await;
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    let _ = system.as_ref().clone().shutdown().await;
}

#[actix::test]
async fn cancelled_constructor_keeps_first_creation_slot_through_real_retirement() {
    let system = system().await;
    let counts = Arc::new(Counts::default());
    let build = Arc::new(Gate::default());
    let release_build = Release(build.clone());
    let capture_gate = Arc::new(Gate::default());
    let release_capture = Release(capture_gate.clone());
    let next = source(
        &counts,
        None,
        None,
        Some(capture_gate.clone()),
        false,
        false,
    );
    let gate = build.clone();
    let mut pending = Box::pin(system.spawn_root_factory(move || {
        gate.block();
        next()
    }));
    assert!(futures::poll!(&mut pending).is_pending());
    entered(&build).await;
    drop(pending);
    assert_eq!(system.instance_admission.available_permits(), 0);
    drop(release_build);
    entered(&capture_gate).await;
    assert_eq!(system.instance_admission.available_permits(), 0);
    drop(release_capture);
    until(|| {
        system.instance_admission.available_permits() == 1 && system.status().active_actors == 0
    })
    .await;
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn queued_and_unpolled_dynamic_factory_inputs_do_not_construct_or_consume_capacity() {
    let system = system().await;
    let occupied = system
        .instance_admission
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    for polled in [false, true] {
        let counts = Arc::new(Counts::default());
        let mut pending =
            Box::pin(system.spawn_root_factory(source(&counts, None, None, None, false, false)));
        if polled {
            assert!(futures::poll!(&mut pending).is_pending());
        }
        drop(pending);
        until(|| counts.captures.load(Ordering::SeqCst) == 1).await;
        assert_eq!(counts.constructed.load(Ordering::SeqCst), 0);
        assert_eq!(system.status().active_actors, 0);
        assert_eq!(system.instance_admission.available_permits(), 0);
    }
    drop(occupied);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn ignored_guardian_strategy_and_queue_share_original_factory_deadline() {
    let mut system = ActixActorSystem::with_guardian(
        ActixSystemSettings {
            arbiter_count: 2,
            shutdown_timeout: Duration::from_millis(150),
            ..Default::default()
        },
        Default::default(),
    )
    .await
    .unwrap();
    system.instance_admission = Arc::new(Semaphore::new(1));
    let system = Arc::new(system);
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let capture = Capture {
        counts: counts.clone(),
        gate: Some(gate.clone()),
    };
    let strategy = OneForOneStrategy {
        max_restarts: 0,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            SupervisionDecision::Stop
        }),
    };
    let mut pending = Box::pin(system.spawn_root_factory_with_strategy(
        source(&counts, None, None, None, false, false),
        strategy,
    ));
    assert!(futures::poll!(&mut pending).is_pending());
    entered(&gate).await;
    let outcome = tokio::time::timeout(Duration::from_millis(800), &mut pending)
        .await
        .unwrap();
    assert!(matches!(outcome, Err(ActorError::TimeoutDetail(_))));
    drop(pending);
    assert_eq!(system.status().active_actors, 0);
    assert_eq!(system.instance_admission.available_permits(), 0);
    drop(release);
    until(|| system.instance_admission.available_permits() == 1).await;
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 0);
    let occupied = system
        .instance_admission
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let error = tokio::time::timeout(
        Duration::from_millis(800),
        system.spawn_root_factory(source(&counts, None, None, None, false, false)),
    )
    .await
    .unwrap()
    .err()
    .unwrap();
    assert!(matches!(error, ActorError::TimeoutDetail(_)));
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 0);
    drop(occupied);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn reconstruction_does_not_reacquire_first_creation_capacity() {
    let system = system().await;
    let counts = Arc::new(Counts::default());
    let actor = system
        .spawn_root_factory(source(&counts, None, None, None, false, false))
        .await
        .unwrap();
    actor.send(Box::new(())).await.unwrap();
    let occupied = system
        .instance_admission
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    assert!(actor.send(Box::new(Crash)).await.is_err());
    until(|| counts.initialized.load(Ordering::SeqCst) == 2).await;
    actor.send(Box::new(())).await.unwrap();
    assert_eq!(system.instance_admission.available_permits(), 0);
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 2);
    drop(occupied);
    actor.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn cancelled_duplicate_factory_rejection_waits_for_actual_capture_disposal() {
    let system = system().await;
    let cohort = system
        .root_cohort(SupervisorStrategyType::OneForOne(stop_policy()))
        .await
        .unwrap();
    let first = system
        .spawn_root_factory_in_cohort(
            source(&Arc::new(Counts::default()), None, None, None, false, false),
            "/duplicate",
            &cohort,
        )
        .await
        .unwrap();
    first.send(Box::new(())).await.unwrap();
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let mut pending = Box::pin(system.spawn_root_factory_in_cohort(
        source(&counts, None, None, Some(gate.clone()), false, false),
        "/duplicate",
        &cohort,
    ));
    assert!(futures::poll!(&mut pending).is_pending());
    tokio::select! {
        result = &mut pending => panic!("capture disposal finished while blocked: {result:?}"),
        _ = entered(&gate) => {},
    }
    drop(pending);
    assert_eq!(system.instance_admission.available_permits(), 0);
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 0);
    assert_eq!(system.status().active_actors, 1);
    drop(release);
    until(|| system.instance_admission.available_permits() == 1).await;
    assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
    first.stop().await.unwrap();
    cohort.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}
