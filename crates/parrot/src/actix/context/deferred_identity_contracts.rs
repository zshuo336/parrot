use super::*;
use crate::actix::{ActixActor, ActixActorSystem, ActixSystemSettings};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorFactoryContext, ActorSpawnContext},
    engine::{InstanceConfig, RuntimeActor, RuntimeFactory},
    supervisor::{DefaultStrategy, SupervisorStrategyType},
    trace::{self, TraceContext},
    types::BoxedMessage,
};
use std::{any::Any, sync::Mutex, time::Duration};

const WAIT: Duration = Duration::from_secs(5);
type Events = Arc<Mutex<Vec<(&'static str, Option<TraceContext>)>>>;
type Context = ActixContext<ActixActor<Native>>;
struct Native(Events);
struct Dynamic(Events);
fn note(events: &Events, phase: &'static str) {
    events
        .lock()
        .unwrap()
        .push((phase, TraceContext::current()));
}
fn initialize(events: &Events) -> BoxedFuture<'_, ActorResult<()>> {
    note(events, "init-call");
    Box::pin(async move {
        note(events, "init-poll");
        tokio::task::yield_now().await;
        note(events, "init-resumed");
        Ok(())
    })
}
impl ActixEngineExt for Native {}
impl Actor for Native {
    type Config = EmptyConfig;
    type Context = Context;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Context) -> BoxedFuture<'a, ActorResult<()>> {
        initialize(&self.0)
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a mut Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Ok(reply) = message.downcast::<tokio::sync::oneshot::Sender<Context>>() {
                assert!(reply.send(context.clone()).is_ok());
                return Ok(Box::new(()) as BoxedMessage);
            }
            Ok(Box::new(TraceContext::current()) as BoxedMessage)
        })
    }
}
impl Actor for Dynamic {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        initialize(&self.0)
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(Box::new(TraceContext::current()) as BoxedMessage) })
    }
}
struct Capture(Events);
impl Drop for Capture {
    fn drop(&mut self) {
        note(&self.0, "capture-drop");
    }
}
fn request(
    context: &mut Context,
    events: Events,
    entry: u8,
    policy: bool,
) -> BoxedFuture<'_, ActorResult<BoxedActorRef>> {
    let policy = policy.then_some(SupervisorStrategyType::Default(
        DefaultStrategy::StopOnFailure,
    ));
    match entry {
        0 => {
            let actor = <Context as ActorSpawnContext<Native>>::wrap_spawn(Native(events));
            match policy {
                Some(policy) => {
                    context
                        .spawner()
                        .spawn_with_strategy(actor, Box::new(EmptyConfig), policy)
                }
                None => context.spawner().spawn(actor, Box::new(EmptyConfig)),
            }
        }
        1 => {
            let capture = Capture(events.clone());
            let factory = <Context as ActorFactoryContext<Native>>::wrap_factory(move || {
                let _ = &capture;
                note(&events, "constructor");
                Native(events.clone())
            });
            context.spawner().spawn_factory_erased(factory, policy)
        }
        2 => context.async_spawner().unwrap().spawn(
            Box::new(Dynamic(events)) as RuntimeActor,
            Box::new(()) as InstanceConfig,
            policy,
        ),
        3 => {
            let capture = Capture(events.clone());
            let factory: RuntimeFactory = Box::new(move || {
                let _ = &capture;
                note(&events, "constructor");
                Box::new(Dynamic(events.clone())) as RuntimeActor
            });
            context
                .async_spawner()
                .unwrap()
                .spawn_factory(factory, policy)
        }
        _ => unreachable!(),
    }
}
fn check(entry: u8, traced: bool) {
    let host = actix::System::new();
    host.block_on(async {
        tokio::time::timeout(WAIT * 4, async {
            for polling_traced in [false, true] {
                for explicit_policy in [false, true] {
                    let system = ActixActorSystem::with_settings(ActixSystemSettings {
                        arbiter_count: 1,
                        shutdown_timeout: WAIT,
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                    let parent = system
                        .spawn_root_typed(Native(Arc::default()), EmptyConfig)
                        .await
                        .unwrap();
                    let (reply, receive) = tokio::sync::oneshot::channel::<Context>();
                    parent.deliver(Box::new(reply)).await.unwrap();
                    let mut context = receive.await.unwrap();
                    let events = Events::default();
                    let origin = traced.then(TraceContext::root);
                    let polling = polling_traced.then(TraceContext::root);
                    let work = trace::in_scope(origin.clone(), || {
                        request(&mut context, events.clone(), entry, explicit_policy)
                    });
                    assert!(
                        events.lock().unwrap().is_empty(),
                        "child creation must remain lazy"
                    );
                    let child = trace::scope(polling.clone(), work).await.unwrap();
                    let work = trace::in_scope(None, || child.send(Box::new(())));
                    let reply = trace::scope(polling.clone(), work).await.unwrap();
                    assert!(
                        reply.downcast::<Option<TraceContext>>().unwrap().is_none(),
                        "creation leaked into an untraced message"
                    );
                    trace::scope(polling, child.stop()).await.unwrap();
                    system.shutdown().await.unwrap();
                    assert!(TraceContext::current().is_none());
                    let rows = events.lock().unwrap().clone();
                    let phase = |name| {
                        let found: Vec<_> = rows
                            .iter()
                            .filter(|(stage, _)| *stage == name)
                            .map(|(_, context)| context.clone())
                            .collect();
                        assert_eq!(found.len(), 1, "phase {name} must execute once: {rows:?}");
                        found[0].clone()
                    };
                    let initialization = phase("init-call");
                    match (&origin, &initialization) {
                        (Some(origin), Some(initialization)) => {
                            assert_eq!(
                                initialization.trace_id(),
                                origin.trace_id(),
                                "deferred child initialization borrowed the polling trace: {rows:?}"
                            );
                            assert_ne!(initialization.span_id(), origin.span_id());
                        }
                        (None, None) => {}
                        _ => panic!(
                            "deferred child initialization changed invocation identity: {rows:?}"
                        ),
                    }
                    assert_eq!(phase("init-poll"), initialization);
                    assert_eq!(phase("init-resumed"), initialization);
                    if entry % 2 == 1 {
                        assert_eq!(
                            phase("constructor"),
                            origin,
                            "deferred child factory borrowed the polling trace"
                        );
                        assert_eq!(phase("capture-drop"), origin);
                    }
                }
            }
        })
        .await
        .expect("deferred child creation liveness bound");
    });
}
macro_rules! contract {
    ($name:ident, $entry:expr, $traced:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!(
                    "actix::context::deferred_identity_contracts::",
                    stringify!($name)
                ),
                || check($entry, $traced),
            );
        }
    };
}
contract!(native_instance_untraced, 0, false);
contract!(native_instance_traced, 0, true);
contract!(native_factory_untraced, 1, false);
contract!(native_factory_traced, 1, true);
contract!(dynamic_instance_untraced, 2, false);
contract!(dynamic_instance_traced, 2, true);
contract!(dynamic_factory_untraced, 3, false);
contract!(dynamic_factory_traced, 3, true);
