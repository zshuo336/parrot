use super::*;
use crate::actix::{
    ActixActorSystem, ActixSystemSettings, ChildInstanceAdmissionFailure,
    typed::{FactoryAdmissionError, TypedInstanceAdmissionError},
};
use parrot_api::{
    supervisor::{BasicDecisionFn, OneForOneStrategy},
    typed::{TypedLifecycle, TypedReceive},
};
use std::sync::atomic::AtomicUsize;

mod local_recovery;
mod unpolled;

const WAIT: Duration = Duration::from_secs(5);

pub struct Read;
impl parrot_api::Message for Read {
    type Result = TypedContext;
}
#[derive(Default)]
struct Counts {
    initialized: AtomicUsize,
    actors: AtomicUsize,
    constructed: AtomicUsize,
    captures: AtomicUsize,
    policies: AtomicUsize,
}
#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(Read), lifecycle)]
struct Sample(Option<Arc<Counts>>);
impl Drop for Sample {
    fn drop(&mut self) {
        if let Some(counts) = &self.0 {
            counts.actors.fetch_add(1, Ordering::SeqCst);
        }
    }
}
impl TypedLifecycle for Sample {
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        if let Some(counts) = &self.0 {
            counts.initialized.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async { Ok(()) })
    }
}
impl TypedReceive<Read> for Sample {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Read,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<TypedContext>> {
        Box::pin(async move { Ok(context.clone()) })
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
            panic!("a successful retry must not invoke supervision");
        }),
    })
}
async fn parent() -> (Arc<ActixActorSystem>, TypedContext) {
    let system = Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: WAIT,
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let reference = system
        .spawn_typed::<_, Read>(Sample(None), "/parent")
        .await
        .unwrap();
    let context = reference.ask(Read).await.unwrap();
    (system, context)
}
fn expired() -> (TypedContext, Arc<tokio::sync::Semaphore>) {
    let host = actix::System::new();
    let (context, slots, weak) = host.block_on(async {
        let (system, context) = parent().await;
        let services = context.services().downcast_ref::<ManagedContext>().unwrap();
        let Backend::Actix(weak) = &services.system else {
            panic!("real Actix parent expected");
        };
        let weak = weak.clone();
        let slots = services.cleanup.clone();
        Arc::try_unwrap(system)
            .ok()
            .unwrap()
            .shutdown()
            .await
            .unwrap();
        assert!(weak.upgrade().is_err());
        (context, slots, weak)
    });
    drop(host);
    assert!(weak.upgrade().is_err());
    (context, slots)
}
type Make = Box<dyn Fn() -> Sample + Send + Sync>;
fn check_recovery(factory_entry: bool, explicit_policy: bool) {
    let (context, slots) = expired();
    let counts = Arc::new(Counts::default());
    let host = actix::System::new();
    host.block_on(async {
        let strategy = explicit_policy.then(|| policy(&counts));
        let (actor, factory, strategy) = if factory_entry {
            let capture = Capture(counts.clone());
            let make: Make = Box::new(move || {
                capture.0.constructed.fetch_add(1, Ordering::SeqCst);
                Sample(Some(capture.0.clone()))
            });
            let result = match strategy {
                Some(strategy) => {
                    context
                        .spawn_typed_child_factory_with_strategy::<_, Read, _>(make, strategy)
                        .await
                }
                None => context.spawn_typed_child_factory::<_, Read, _>(make).await,
            };
            let ActorError::Other(error) = result.err().expect("expired parent admitted a child")
            else {
                panic!("expired static factory lost recoverable inputs");
            };
            let error = error.downcast::<FactoryAdmissionError<Make>>().unwrap();
            assert_eq!(error.reason, ChildInstanceAdmissionFailure::ShuttingDown);
            let inputs = error.into_inputs();
            (None, Some(inputs.factory), inputs.strategy)
        } else {
            let actor = Sample(Some(counts.clone()));
            let result = match strategy {
                Some(strategy) => {
                    context
                        .spawn_typed_child_with_strategy::<_, Read>(actor, strategy)
                        .await
                }
                None => context.spawn_typed_child::<_, Read>(actor).await,
            };
            let ActorError::Other(error) = result.err().expect("expired parent admitted a child")
            else {
                panic!("expired static instance lost recoverable inputs");
            };
            let error = error
                .downcast::<TypedInstanceAdmissionError<Sample>>()
                .unwrap();
            assert_eq!(error.reason, ChildInstanceAdmissionFailure::ShuttingDown);
            let inputs = error.into_inputs();
            assert!(Arc::ptr_eq(inputs.actor.0.as_ref().unwrap(), &counts));
            (Some(inputs.actor), None, inputs.strategy)
        };
        assert_eq!(strategy.is_some(), explicit_policy);
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
        assert_eq!(counts.constructed.load(Ordering::SeqCst), 0);
        assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
        assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
        assert_eq!(slots.available_permits(), 1);
        let (system, live) = parent().await;
        let reference = match (actor, factory, strategy) {
            (Some(actor), None, Some(strategy)) => {
                live.spawn_typed_child_with_strategy::<_, Read>(actor, strategy)
                    .await
            }
            (Some(actor), None, None) => live.spawn_typed_child::<_, Read>(actor).await,
            (None, Some(factory), Some(strategy)) => {
                live.spawn_typed_child_factory_with_strategy::<_, Read, _>(factory, strategy)
                    .await
            }
            (None, Some(factory), None) => {
                live.spawn_typed_child_factory::<_, Read, _>(factory).await
            }
            _ => unreachable!(),
        }
        .unwrap();
        assert!(
            reference
                .ask(Read)
                .await
                .unwrap()
                .path()
                .starts_with("/parent/")
        );
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
        Arc::try_unwrap(system)
            .ok()
            .unwrap()
            .shutdown()
            .await
            .unwrap();
        assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts.constructed.load(Ordering::SeqCst),
            usize::from(factory_entry)
        );
        assert_eq!(
            counts.captures.load(Ordering::SeqCst),
            usize::from(factory_entry)
        );
        assert_eq!(
            counts.policies.load(Ordering::SeqCst),
            usize::from(explicit_policy)
        );
    });
}
macro_rules! recovery {
    ($name:ident, $factory:expr, $policy:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!(
                    "typed_context::expired_actix_contracts::",
                    stringify!($name)
                ),
                || check_recovery($factory, $policy),
            );
        }
    };
}
recovery!(expired_instance_returns_retryable_input, false, false);
recovery!(expired_instance_returns_retryable_policy, false, true);
recovery!(expired_factory_returns_retryable_capture, true, false);
recovery!(expired_factory_returns_retryable_policy, true, true);
