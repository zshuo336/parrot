use super::*;
use crate::thread::ThreadActorSystemConfig;
use crate::typed_context::{TypedContext, TypedContextExt};
#[path = "factory_shared_admission_lifecycle_contracts.rs"]
mod lifecycle;
use parrot_api::{
    actor::{ActorState, EmptyConfig},
    context::{ActorFactoryContext, ActorSpawner},
    errors::ActorError,
    message::Message,
    supervisor::SupervisorStrategyType,
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct SpawnChild {
    factory: BoxedMessage,
    strategy: Option<SupervisorStrategyType>,
    reply: tokio::sync::oneshot::Sender<ActorResult<BoxedActorRef>>,
}

struct Dynamic;
impl Actor for Dynamic {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match message.downcast::<SpawnChild>() {
                Ok(request) => {
                    let result = context
                        .spawn_factory_erased(request.factory, request.strategy)
                        .await;
                    let _ = request.reply.send(result);
                    Ok(Box::new(()) as BoxedMessage)
                }
                Err(message) => Ok(message),
            }
        })
    }
}
struct Request;
impl Message for Request {
    type Result = usize;
}
struct Static;
impl ParrotTypedDispatch for Static {
    type Msg = Request;
    type Reply = usize;
    fn dispatch(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async { Ok(42) })
    }
}
impl ParrotMsgVariant<Static> for Request {
    fn inject(message: Self) -> Request {
        message
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}
impl parrot_api::typed::TypedReceive<Request> for Static {
    fn receive_typed(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async { Ok(42) })
    }
}
fn system() -> Arc<ThreadActorSystem> {
    let mut system = ThreadActorSystem::new(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_secs(5),
        ..Default::default()
    });
    system.instance_admission = Arc::new(tokio::sync::Semaphore::new(1));
    let system = Arc::new(system);
    system.set_self_weak(Arc::downgrade(&system));
    system
}
async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("shared factory admission watchdog")
}
async fn recovered(system: &ThreadActorSystem) {
    within(async {
        while system.instance_admission.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
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
    fn inject(message: Self) -> Request {
        message
    }
    fn extract(reply: usize) -> ActorResult<usize> {
        Ok(reply)
    }
}
async fn parent(system: &Arc<ThreadActorSystem>) -> TypedContext {
    let (send, receive) = tokio::sync::oneshot::channel();
    let actor = system
        .spawn_typed::<_, Request>(Parent(Some(send)), "/parent")
        .await
        .unwrap();
    let context = within(receive).await.unwrap();
    assert_eq!(within(actor.ask(Request)).await.unwrap(), 42);
    recovered(system).await;
    context
}
struct Capture(Arc<AtomicUsize>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn static_context_factory_rejection_returns_captures_and_retries() {
    type Source = Box<dyn Fn() -> Static + Send + Sync>;
    let system = system();
    let context = parent(&system).await;
    let reservation = system
        .try_spawn_typed_with_config(Dynamic, EmptyConfig, Default::default())
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let capture = Capture(drops.clone());
    let observed = calls.clone();
    let factory: Source = Box::new(move || {
        std::hint::black_box(&capture);
        observed.fetch_add(1, Ordering::SeqCst);
        Static
    });
    let result = within(context.spawn_typed_child_factory::<_, Request, _>(factory)).await;
    let error = match result {
        Err(ActorError::Other(error)) => error
            .downcast::<crate::thread::FactoryAdmissionError<Source>>()
            .unwrap(),
        _ => panic!("factory must return recoverable shared-capacity rejection"),
    };
    assert_eq!(error.reason, crate::thread::InstanceAdmissionFailure::Full);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(context.children().is_empty());
    let inputs = error.into_inputs();
    drop(reservation);
    recovered(&system).await;
    let child = within(
        context.spawn_typed_child_factory_with_strategy::<_, Request, _>(
            inputs.factory,
            inputs.strategy.unwrap_or_default(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(within(child.ask(Request)).await.unwrap(), 42);
    recovered(&system).await;
    within(system.shutdown_internal()).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dynamic_context_factory_rejection_returns_erased_payload_and_retries() {
    let system = system();
    let parent = system
        .spawn_root_typed_thread(Dynamic, EmptyConfig)
        .await
        .unwrap();
    recovered(&system).await;
    let reservation = system
        .try_spawn_typed_with_config(Dynamic, EmptyConfig, Default::default())
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture(drops.clone());
    let factory =
        <ThreadContext<Dynamic> as ActorFactoryContext<Dynamic>>::wrap_factory(move || {
            std::hint::black_box(&capture);
            Dynamic
        });
    let (reply, receive) = tokio::sync::oneshot::channel();
    parent
        .deliver(Box::new(SpawnChild {
            factory,
            strategy: None,
            reply,
        }))
        .await
        .unwrap();
    let result = within(receive).await.unwrap();
    let error = match result {
        Err(ActorError::Other(error)) => error
            .downcast::<crate::thread::FactoryAdmissionError<BoxedMessage>>()
            .unwrap(),
        _ => panic!("erased factory must return recoverable shared-capacity rejection"),
    };
    assert_eq!(error.reason, crate::thread::InstanceAdmissionFailure::Full);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let inputs = error.into_inputs();
    drop(reservation);
    recovered(&system).await;
    let (reply, receive) = tokio::sync::oneshot::channel();
    parent
        .deliver(Box::new(SpawnChild {
            factory: inputs.factory,
            strategy: inputs.strategy,
            reply,
        }))
        .await
        .unwrap();
    let child = within(receive).await.unwrap().unwrap();
    assert_eq!(
        *within(child.send(Box::new(42usize)))
            .await
            .unwrap()
            .downcast::<usize>()
            .unwrap(),
        42
    );
    recovered(&system).await;
    within(system.shutdown_internal()).await.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn dynamic_factory_capacity_wait_preserves_original_creation_deadline() {
    let mut system = ThreadActorSystem::new(ThreadActorSystemConfig::default());
    system.instance_admission = Arc::new(tokio::sync::Semaphore::new(1));
    system.operation_timeouts.actor_creation = Duration::from_secs(10);
    let system = Arc::new(system);
    system.set_self_weak(Arc::downgrade(&system));
    let worker = system
        .factory_slots
        .clone()
        .acquire_many_owned(system.factory_slots.available_permits() as u32)
        .await
        .unwrap();
    let held = system
        .instance_admission
        .clone()
        .acquire_owned()
        .await
        .unwrap();
    let mut creation = Box::pin(system.spawn_supervised_with_config(
        || Dynamic,
        "/deadline-factory",
        Default::default(),
    ));
    let expected = tokio::time::Instant::now() + Duration::from_secs(10);
    assert!(futures::poll!(&mut creation).is_pending());
    tokio::time::advance(Duration::from_secs(4)).await;
    drop(held);
    assert!(futures::poll!(&mut creation).is_pending());
    let pending = match system
        .registry
        .read()
        .unwrap()
        .get("/deadline-factory")
        .unwrap()
    {
        ActorRegistryEntry::Pending(pending) => pending.clone(),
        _ => panic!("factory must still be pending construction"),
    };
    let actual = pending.control.deadline();
    tokio::time::resume();
    drop(worker);
    within(creation).await.unwrap();
    within(system.shutdown_internal()).await.unwrap();
    assert_eq!(
        actual,
        Some(expected),
        "capacity queue restarted the creation deadline"
    );
}

#[tokio::test]
async fn root_factory_adapters_wait_for_real_instance_reservations() {
    let strategy = || parrot_api::supervisor::OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(60),
        decider: parrot_api::supervisor::BasicDecisionFn::new(|_| {
            parrot_api::supervisor::SupervisionDecision::Stop
        }),
    };
    for entry in 0..7 {
        let system = system();
        let cohort = system.root_cohort(Default::default()).await.unwrap();
        let held = system
            .try_spawn_typed_with_config(Dynamic, EmptyConfig, Default::default())
            .unwrap();
        let mut work: BoxedFuture<'_, ActorResult<usize>> = match entry {
            0 => Box::pin(async {
                let actor = system
                    .spawn_supervised_with_config(
                        || Dynamic,
                        "/adapter-factory",
                        Default::default(),
                    )
                    .await
                    .map_err(ActorError::from)?;
                Ok(*actor
                    .send(Box::new(42usize))
                    .await?
                    .downcast::<usize>()
                    .unwrap())
            }),
            1 => Box::pin(async {
                let actor = system
                    .spawn_supervised_in_cohort(
                        || Dynamic,
                        "/adapter-factory",
                        Default::default(),
                        &cohort,
                    )
                    .await
                    .map_err(ActorError::from)?;
                Ok(*actor
                    .send(Box::new(42usize))
                    .await?
                    .downcast::<usize>()
                    .unwrap())
            }),
            2 => Box::pin(async {
                system
                    .spawn_typed_factory::<Static, Request, _>(|| Static, "/adapter-factory")
                    .await?
                    .ask(Request)
                    .await
            }),
            3 => Box::pin(async {
                system
                    .spawn_typed_factory_with_strategy::<Static, Request, _>(
                        || Static,
                        "/adapter-factory",
                        strategy(),
                    )
                    .await?
                    .ask(Request)
                    .await
            }),
            4 => Box::pin(async {
                system
                    .spawn_typed_factory_in_cohort::<Static, Request, _>(
                        || Static,
                        "/adapter-factory",
                        &cohort,
                    )
                    .await?
                    .ask(Request)
                    .await
            }),
            5 => Box::pin(async {
                system
                    .spawn_typed_single_factory::<Static, Request, _>(|| Static, "/adapter-factory")
                    .await?
                    .ask(crate::thread::typed::SingleVariant::wrap(Request))
                    .await
            }),
            _ => Box::pin(async {
                system
                    .spawn_typed_single_factory_with_strategy::<Static, Request, _>(
                        || Static,
                        "/adapter-factory",
                        strategy(),
                    )
                    .await?
                    .ask(crate::thread::typed::SingleVariant::wrap(Request))
                    .await
            }),
        };
        assert!(
            futures::poll!(&mut work).is_pending(),
            "adapter {entry} bypassed shared admission"
        );
        assert_eq!(system.actor_count(), 0);
        drop(held);
        assert_eq!(within(work).await.unwrap(), 42);
        recovered(&system).await;
        within(system.shutdown_internal()).await.unwrap();
    }
}

#[tokio::test]
async fn dynamic_factories_wait_before_registration_and_release_after_publication() {
    let system = system();
    let worker = system.factory_slots.clone().acquire_owned().await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let first = system.spawn_supervised_with_config(
        move || {
            observed.fetch_add(1, Ordering::SeqCst);
            Dynamic
        },
        "/factory-first",
        ThreadActorConfig::default(),
    );
    tokio::pin!(first);
    assert!(futures::poll!(&mut first).is_pending());
    let second = system.spawn_supervised_with_config(
        || Dynamic,
        "/factory-second",
        ThreadActorConfig::default(),
    );
    let mut second = Box::pin(second);
    assert!(futures::poll!(&mut second).is_pending());
    let held = system.instance_admission.available_permits() == 0;
    let deferred = !system
        .registry
        .read()
        .unwrap()
        .contains_key("/factory-second");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    drop(worker);
    let first = within(first).await.unwrap();
    let second = within(second).await.unwrap();
    let reply = first.send(Box::new(42usize)).await.unwrap();
    assert_eq!(*reply.downcast::<usize>().unwrap(), 42);
    assert_eq!(system.instance_admission.available_permits(), 1);
    second.stop().await.unwrap();
    within(system.shutdown_internal()).await.unwrap();
    assert!(held, "factory did not reserve shared creation capacity");
    assert!(
        deferred,
        "another factory registered beyond shared capacity"
    );
}

#[tokio::test]
async fn static_factories_wait_before_registration_and_release_after_initialization() {
    let system = system();
    let worker = system.factory_slots.clone().acquire_owned().await.unwrap();
    let first = system
        .spawn_typed_factory::<Static, Request, _>(|| Static, "/static-factory-first")
        .await
        .unwrap();
    let mut second = Box::pin(
        system.spawn_typed_factory::<Static, Request, _>(|| Static, "/static-factory-second"),
    );
    let pending = futures::poll!(&mut second).is_pending();
    let held = system.instance_admission.available_permits() == 0;
    let deferred = !system
        .registry
        .read()
        .unwrap()
        .contains_key("/static-factory-second");
    drop(worker);
    assert_eq!(within(first.ask(Request)).await.unwrap(), 42);
    if pending {
        let second = within(second).await.unwrap();
        assert_eq!(within(second.ask(Request)).await.unwrap(), 42);
    }
    within(system.shutdown_internal()).await.unwrap();
    assert!(
        held,
        "static factory did not reserve shared creation capacity"
    );
    assert!(
        pending && deferred,
        "static factory bypassed shared admission"
    );
}
