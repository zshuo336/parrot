use super::*;
use crate::actix::{
    ActixActor, ActixActorSystem, ActixSystemSettings, ChildInstanceAdmissionError,
    ChildInstanceAdmissionFailure, typed::FactoryAdmissionError,
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorConfig, ActorState},
    context::{ActorFactoryContext, ActorSpawnContext},
    engine::{InstanceConfig, LocalActor, LocalRuntimeFactory, RuntimeActor, RuntimeFactory},
    local_typed::LocalBoxedFuture,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisorStrategyType},
    typed_context::TypedContext,
    types::BoxedMessage,
};
use std::{
    cell::Cell,
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);
type Context = ActixContext<ActixActor<Native>>;
#[derive(Default)]
struct Counts {
    initialized: AtomicUsize,
    actors: AtomicUsize,
    configs: AtomicUsize,
    constructors: AtomicUsize,
    captures: AtomicUsize,
    policies: AtomicUsize,
}
#[derive(Default)]
struct Config(Option<Arc<Counts>>);
impl ActorConfig for Config {}
impl Drop for Config {
    fn drop(&mut self) {
        if let Some(counts) = &self.0 {
            counts.configs.fetch_add(1, Ordering::SeqCst);
        }
    }
}
struct Native(Option<Arc<Counts>>);
struct Dynamic(Arc<Counts>);
struct Local {
    counts: Arc<Counts>,
    owner: std::thread::ThreadId,
    state: Rc<()>,
}
impl Drop for Native {
    fn drop(&mut self) {
        if let Some(counts) = &self.0 {
            counts.actors.fetch_add(1, Ordering::SeqCst);
        }
    }
}
impl Drop for Dynamic {
    fn drop(&mut self) {
        self.0.actors.fetch_add(1, Ordering::SeqCst);
    }
}
impl Drop for Local {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
        assert_eq!(Rc::strong_count(&self.state), 1);
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActixEngineExt for Native {}
impl Actor for Native {
    type Config = Config;
    type Context = Context;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Context) -> BoxedFuture<'a, ActorResult<()>> {
        if let Some(counts) = &self.0 {
            counts.initialized.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a mut Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match message.downcast::<tokio::sync::oneshot::Sender<Context>>() {
                Ok(reply) => {
                    assert!(reply.send(context.clone()).is_ok());
                    Ok(Box::new(()) as BoxedMessage)
                }
                Err(message) => Ok(message),
            }
        })
    }
}
impl Actor for Dynamic {
    type Config = InstanceConfig;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}
impl LocalActor for Local {
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.counts.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}
struct Capture(Arc<Counts>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.captures.fetch_add(1, Ordering::SeqCst);
    }
}
struct Policy(Arc<Counts>);
impl Drop for Policy {
    fn drop(&mut self) {
        self.0.policies.fetch_add(1, Ordering::SeqCst);
    }
}
fn policy(counts: &Arc<Counts>) -> SupervisorStrategyType {
    let capture = Policy(counts.clone());
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            panic!("successful child retry must not invoke supervision");
        }),
    })
}
async fn parent() -> (ActixActorSystem, Context) {
    let system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: WAIT,
        ..Default::default()
    })
    .await
    .unwrap();
    let parent = system
        .spawn_root_typed(Native(None), Config(None))
        .await
        .unwrap();
    let (reply, receive) = tokio::sync::oneshot::channel::<Context>();
    parent.deliver(Box::new(reply)).await.unwrap();
    (system, receive.await.unwrap())
}
fn expired() -> (Context, Arc<tokio::sync::Semaphore>) {
    let host = actix::System::new();
    let (context, slots, weak) = host.block_on(async {
        let (system, context) = parent().await;
        let slots = system.cleanup_slots.clone();
        let weak = system.downgrade();
        system.shutdown().await.unwrap();
        assert!(weak.upgrade().is_err());
        (context, slots, weak)
    });
    drop(host);
    assert!(weak.upgrade().is_err());
    (context, slots)
}
fn pending(slots: &Arc<tokio::sync::Semaphore>) -> usize {
    let mut held = Vec::new();
    while let Some(permit) = crate::owned_payload::try_pending_cleanup_permit(slots) {
        held.push(permit);
    }
    held.len()
}
enum Inputs {
    Instance(crate::actix::ChildInstanceAdmissionInputs),
    Factory(crate::actix::typed::FactoryAdmissionInputs<BoxedMessage>),
    Local(crate::actix::typed::FactoryAdmissionInputs<LocalRuntimeFactory>),
}
fn factory_inputs<F: Send + 'static>(
    error: ActorError,
) -> crate::actix::typed::FactoryAdmissionInputs<F> {
    let ActorError::Other(error) = error else {
        panic!("expired factory lost recoverable inputs");
    };
    let error = error.downcast::<FactoryAdmissionError<F>>().unwrap();
    assert_eq!(error.reason, ChildInstanceAdmissionFailure::ShuttingDown);
    error.into_inputs()
}
fn check(entry: u8, explicit: bool) {
    let (mut context, slots) = std::thread::spawn(expired).join().unwrap();
    assert!(tokio::runtime::Handle::try_current().is_err());
    assert!(actix::System::try_current().is_none());
    let before = pending(&slots);
    let counts = Arc::new(Counts::default());
    let strategy = explicit.then(|| policy(&counts));
    let inputs = match entry {
        0 | 2 => {
            let config = Config(Some(counts.clone()));
            let result = if entry == 0 {
                let payload = <Context as ActorSpawnContext<Native>>::wrap_spawn(Native(Some(
                    counts.clone(),
                )));
                match strategy {
                    Some(strategy) => futures::executor::block_on(
                        context
                            .spawner()
                            .spawn_with_strategy(payload, Box::new(config), strategy),
                    ),
                    None => futures::executor::block_on(
                        context.spawner().spawn(payload, Box::new(config)),
                    ),
                }
            } else {
                futures::executor::block_on(context.async_spawner().unwrap().spawn(
                    Box::new(Dynamic(counts.clone())) as RuntimeActor,
                    Box::new(config),
                    strategy,
                ))
            };
            let ActorError::Other(error) = result.expect_err("expired instance admitted") else {
                panic!("expired instance lost recoverable inputs");
            };
            let error = error.downcast::<ChildInstanceAdmissionError>().unwrap();
            assert_eq!(error.reason, ChildInstanceAdmissionFailure::ShuttingDown);
            Inputs::Instance(error.into_inputs())
        }
        1 | 3 => {
            let capture = Capture(counts.clone());
            let result = if entry == 1 {
                let payload = <Context as ActorFactoryContext<Native>>::wrap_factory(move || {
                    let _ = &capture;
                    capture.0.constructors.fetch_add(1, Ordering::SeqCst);
                    Native(Some(capture.0.clone()))
                });
                futures::executor::block_on(
                    context.spawner().spawn_factory_erased(payload, strategy),
                )
            } else {
                let factory: RuntimeFactory = Box::new(move || {
                    let _ = &capture;
                    capture.0.constructors.fetch_add(1, Ordering::SeqCst);
                    Box::new(Dynamic(capture.0.clone()))
                });
                futures::executor::block_on(
                    context
                        .async_spawner()
                        .unwrap()
                        .spawn_factory(factory, strategy),
                )
            };
            Inputs::Factory(factory_inputs(
                result.expect_err("expired factory admitted"),
            ))
        }
        4 => {
            let capture = Capture(counts.clone());
            let calls = Cell::new(0);
            let factory: LocalRuntimeFactory = Box::new(move || {
                let _ = &capture;
                calls.set(calls.get() + 1);
                assert_eq!(calls.get(), 1);
                capture.0.constructors.fetch_add(1, Ordering::SeqCst);
                Box::new(Local {
                    counts: capture.0.clone(),
                    owner: std::thread::current().id(),
                    state: Rc::new(()),
                })
            });
            let identity = std::ptr::from_ref(factory.as_ref()).cast::<()>();
            let result = futures::executor::block_on(
                context
                    .local_spawner()
                    .unwrap()
                    .spawn_local_factory(factory, strategy),
            );
            let inputs = factory_inputs::<LocalRuntimeFactory>(
                result.expect_err("expired Local factory admitted"),
            );
            assert_eq!(
                std::ptr::from_ref(inputs.factory.as_ref()).cast::<()>(),
                identity
            );
            Inputs::Local(inputs)
        }
        _ => unreachable!(),
    };
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 0);
    assert_eq!(counts.constructors.load(Ordering::SeqCst), 0);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
    assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
    assert_eq!(slots.available_permits(), 1);
    assert_eq!(pending(&slots), before);
    let host = actix::System::new();
    host.block_on(async {
        tokio::time::timeout(WAIT * 4, async {
            let (system, mut live) = parent().await;
            let reference = match inputs {
                Inputs::Instance(inputs) => {
                    assert_eq!(inputs.strategy.is_some(), explicit);
                    match inputs.strategy {
                        Some(strategy) => {
                            live.spawner()
                                .spawn_with_strategy(inputs.actor, inputs.config, strategy)
                                .await
                        }
                        None => live.spawner().spawn(inputs.actor, inputs.config).await,
                    }
                }
                Inputs::Factory(inputs) => {
                    assert_eq!(inputs.strategy.is_some(), explicit);
                    live.spawner()
                        .spawn_factory_erased(inputs.factory, inputs.strategy)
                        .await
                }
                Inputs::Local(inputs) => {
                    assert_eq!(inputs.strategy.is_some(), explicit);
                    live.local_spawner()
                        .unwrap()
                        .spawn_local_factory(inputs.factory, inputs.strategy)
                        .await
                }
            }
            .unwrap();
            let reply = reference.send(Box::new(0x51a7_u64)).await.unwrap();
            assert_eq!(*reply.downcast::<u64>().unwrap(), 0x51a7);
            assert!(
                reference
                    .path()
                    .starts_with(&format!("{}/", live.path().path()))
            );
            assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
            assert_eq!(
                counts.constructors.load(Ordering::SeqCst),
                usize::from(entry == 1 || entry == 3 || entry == 4)
            );
            system.shutdown().await.unwrap();
            assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
            assert_eq!(
                counts.configs.load(Ordering::SeqCst),
                usize::from(entry == 0 || entry == 2)
            );
            assert_eq!(
                counts.captures.load(Ordering::SeqCst),
                usize::from(entry == 1 || entry == 3 || entry == 4)
            );
            assert_eq!(
                counts.policies.load(Ordering::SeqCst),
                usize::from(explicit)
            );
        })
        .await
        .expect("expired native child rejection and retry liveness bound");
    });
}
macro_rules! contract {
    ($name:ident, $entry:expr, $policy:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!("actix::context::recovery_contracts::", stringify!($name)),
                || check($entry, $policy),
            );
        }
    };
}
contract!(native_instance_returns_original_inputs, 0, false);
contract!(native_instance_returns_original_policy, 0, true);
contract!(native_factory_returns_original_capture, 1, false);
contract!(native_factory_returns_original_policy, 1, true);
contract!(dynamic_instance_returns_original_inputs, 2, false);
contract!(dynamic_instance_returns_original_policy, 2, true);
contract!(dynamic_factory_returns_original_capture, 3, false);
contract!(dynamic_factory_returns_original_policy, 3, true);
contract!(local_factory_returns_original_capture, 4, false);
contract!(local_factory_returns_original_policy, 4, true);
