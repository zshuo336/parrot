use super::*;
use parrot_api::{
    engine::{LocalActor, LocalRuntimeFactory},
    local_typed::{LocalBoxedFuture, LocalTypedLifecycle, LocalTypedReceive},
};
use std::{cell::Cell, rc::Rc};

#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(Read), local, lifecycle)]
struct Local {
    state: Rc<()>,
    owner: std::thread::ThreadId,
    counts: Arc<Counts>,
}
impl Drop for Local {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
        assert_eq!(Rc::strong_count(&self.state), 1);
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
    }
}
impl LocalTypedLifecycle for Local {
    fn init(&mut self) -> LocalBoxedFuture<'_, ActorResult<()>> {
        self.counts.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}
impl LocalTypedReceive<Read> for Local {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Read,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<TypedContext>> {
        Box::pin(async move { Ok(context.clone()) })
    }
}
impl LocalActor for Local {
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        LocalTypedLifecycle::init(self)
    }
    fn receive<'a>(
        &'a mut self,
        _: parrot_api::types::BoxedMessage,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
        Box::pin(async move { Ok(Box::new(context.clone()) as parrot_api::types::BoxedMessage) })
    }
}
type Make = Box<dyn Fn() -> Local + Send>;
fn source(counts: &Arc<Counts>) -> Make {
    let capture = Capture(counts.clone());
    let calls = Cell::new(0);
    Box::new(move || {
        let _ = &capture;
        calls.set(calls.get() + 1);
        assert_eq!(
            calls.get(),
            1,
            "retry must construct exactly one generation"
        );
        capture.0.constructed.fetch_add(1, Ordering::SeqCst);
        Local {
            state: Rc::new(()),
            owner: std::thread::current().id(),
            counts: capture.0.clone(),
        }
    })
}
fn pending(slots: &Arc<tokio::sync::Semaphore>) -> usize {
    let mut held = Vec::new();
    while let Some(permit) = crate::owned_payload::try_pending_cleanup_permit(slots) {
        held.push(permit);
    }
    held.len()
}
fn recover<F: Send + 'static, T>(
    result: ActorResult<T>,
) -> crate::actix::typed::FactoryAdmissionInputs<F> {
    let ActorError::Other(error) = result.err().expect("expired Local parent admitted a child")
    else {
        panic!("expired Local constructor lost recoverable inputs");
    };
    let error = error.downcast::<FactoryAdmissionError<F>>().unwrap();
    assert_eq!(error.reason, ChildInstanceAdmissionFailure::ShuttingDown);
    error.into_inputs()
}
async fn spawn(
    context: &TypedContext,
    factory: Make,
    strategy: Option<SupervisorStrategyType>,
    repeatable: bool,
) -> ActorResult<LocalTypedActorRef<Local, Read>> {
    match (repeatable, strategy) {
        (false, None) => context.spawn_local_typed_child::<_, Read, _>(factory).await,
        (false, Some(strategy)) => {
            context
                .spawn_local_typed_child_with_strategy::<_, Read, _>(factory, strategy)
                .await
        }
        (true, None) => {
            context
                .spawn_local_typed_child_factory::<_, Read, _>(factory)
                .await
        }
        (true, Some(strategy)) => {
            context
                .spawn_local_typed_child_factory_with_strategy::<_, Read, _>(factory, strategy)
                .await
        }
    }
}
fn check(entry: u8, explicit: bool) {
    let (context, slots) = std::thread::spawn(expired).join().unwrap();
    assert!(tokio::runtime::Handle::try_current().is_err());
    assert!(actix::System::try_current().is_none());
    let before = pending(&slots);
    let counts = Arc::new(Counts::default());
    let strategy = explicit.then(|| policy(&counts));
    let make = source(&counts);
    let recovered = if entry == 2 {
        let factory: LocalRuntimeFactory = Box::new(move || Box::new(make()));
        let identity = std::ptr::from_ref(factory.as_ref()).cast::<()>();
        let result = futures::executor::block_on(
            context
                .local_spawner()
                .unwrap()
                .spawn_local_factory(factory, strategy),
        );
        let inputs = recover::<LocalRuntimeFactory, _>(result);
        assert_eq!(
            std::ptr::from_ref(inputs.factory.as_ref()).cast::<()>(),
            identity
        );
        assert_eq!(inputs.strategy.is_some(), explicit);
        (None, Some(inputs))
    } else {
        let identity = std::ptr::from_ref(make.as_ref()).cast::<()>();
        let result = futures::executor::block_on(spawn(&context, make, strategy, entry == 1));
        let inputs = recover::<Make, _>(result);
        assert_eq!(
            std::ptr::from_ref(inputs.factory.as_ref()).cast::<()>(),
            identity
        );
        assert_eq!(inputs.strategy.is_some(), explicit);
        (Some(inputs), None)
    };
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 0);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
    assert_eq!(counts.captures.load(Ordering::SeqCst), 0);
    assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
    assert_eq!(slots.available_permits(), 1);
    assert_eq!(pending(&slots), before);
    let runner = actix::System::new();
    runner.block_on(async {
        tokio::time::timeout(WAIT * 4, async {
            let (system, live) = parent().await;
            let actual = match recovered {
                (Some(inputs), None) => {
                    let reference = spawn(&live, inputs.factory, inputs.strategy, entry == 1)
                        .await
                        .unwrap();
                    reference.ask(Read).await.unwrap()
                }
                (None, Some(inputs)) => {
                    let reference = live
                        .local_spawner()
                        .unwrap()
                        .spawn_local_factory(inputs.factory, inputs.strategy)
                        .await
                        .unwrap();
                    let reply = reference.send(Box::new(())).await.unwrap();
                    *reply.downcast::<TypedContext>().unwrap()
                }
                _ => unreachable!(),
            };
            assert!(actual.path().starts_with("/parent/"));
            assert_eq!(counts.constructed.load(Ordering::SeqCst), 1);
            assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
            Arc::try_unwrap(system)
                .ok()
                .unwrap()
                .shutdown()
                .await
                .unwrap();
            assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
            assert_eq!(counts.captures.load(Ordering::SeqCst), 1);
            assert_eq!(
                counts.policies.load(Ordering::SeqCst),
                usize::from(explicit)
            );
        })
        .await
        .expect("expired Local rejection and live-parent retry liveness bound");
    });
}
macro_rules! contract {
    ($name:ident, $entry:expr, $policy:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!(
                    "typed_context::expired_actix_contracts::local_recovery::",
                    stringify!($name)
                ),
                || check($entry, $policy),
            );
        }
    };
}
contract!(once_returns_original_constructor, 0, false);
contract!(once_returns_original_policy, 0, true);
contract!(repeatable_returns_original_constructor, 1, false);
contract!(repeatable_returns_original_policy, 1, true);
contract!(neutral_returns_original_constructor, 2, false);
contract!(neutral_returns_original_policy, 2, true);
