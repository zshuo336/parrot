use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorSpawnerExt},
    engine::{AsyncFactoryRuntime, FactoryOptions, RuntimeActor},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{Arc, Mutex},
    time::Duration,
};

type Events = Arc<Mutex<Vec<(&'static str, Option<TraceContext>)>>>;
fn note(events: &Events, name: &'static str) {
    events.lock().unwrap().push((name, TraceContext::current()));
}
struct OnDrop(Events);
impl Drop for OnDrop {
    fn drop(&mut self) {
        note(&self.0, "factory-drop");
    }
}
struct Native(Events);
struct Erased(Events);
struct MakeChild(Events);
fn policy() -> OneForOneStrategy {
    OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Stop),
    }
}
fn native_factory(events: Events) -> impl Fn() -> Native + Send + Sync {
    let capture = OnDrop(events.clone());
    move || {
        std::hint::black_box(&capture);
        note(&events, "constructor");
        Native(events.clone())
    }
}
macro_rules! actor {
    ($actor:ident, $config:ty, $context:ty) => {
        impl Actor for $actor {
            type Config = $config;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn init<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                note(&self.0, "init-call");
                Box::pin(async move {
                    note(&self.0, "init-poll");
                    tokio::task::yield_now().await;
                    note(&self.0, "init-resumed");
                    Ok(())
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    if message.is::<MakeChild>() {
                        let events = message.downcast::<MakeChild>().unwrap().0;
                        note(&events, "call");
                        let child = ctx
                            .spawner()
                            .spawn_factory_with_strategy(
                                native_factory(events),
                                SupervisorStrategyType::OneForOne(policy()),
                            )
                            .await?;
                        return Ok(Box::new(child) as BoxedMessage);
                    }
                    let index = *message.downcast::<usize>().unwrap();
                    note(
                        &self.0,
                        if index == 0 {
                            "untraced-handler"
                        } else {
                            "traced-handler"
                        },
                    );
                    tokio::task::yield_now().await;
                    Ok(Box::new(TraceContext::current()) as BoxedMessage)
                })
            }
        }
    };
}
actor!(Native, EmptyConfig, ActixContext<ActixActor<Self>>);
actor!(Erased, Box<dyn Any + Send>, dyn ActorContext);
impl ActixEngineExt for Native {}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Default,
    Custom,
    Erased,
    Child,
}
fn spawn<'a>(
    system: &'a ActixActorSystem,
    host: Option<&'a BoxedActorRef>,
    events: Events,
    entry: Entry,
) -> BoxedFuture<'a, ActorResult<BoxedActorRef>> {
    if matches!(entry, Entry::Child) {
        let delivery = host.unwrap().send(Box::new(MakeChild(events)));
        return Box::pin(async move { Ok(*delivery.await?.downcast::<BoxedActorRef>().unwrap()) });
    }
    note(&events, "call");
    match entry {
        Entry::Default => Box::pin(system.spawn_root_factory(native_factory(events))),
        Entry::Custom => {
            Box::pin(system.spawn_root_factory_with_strategy(native_factory(events), policy()))
        }
        Entry::Erased => {
            let capture = OnDrop(events.clone());
            let work = system.spawn_factory(
                Box::new(move || {
                    std::hint::black_box(&capture);
                    note(&events, "constructor");
                    Box::new(Erased(events.clone())) as RuntimeActor
                }),
                FactoryOptions {
                    max_restarts: 0,
                    ..Default::default()
                },
            );
            Box::pin(async move {
                work.await
                    .map_err(|error| ActorError::InternalError(error.to_string()))
            })
        }
        Entry::Child => unreachable!(),
    }
}
async fn scenario(entry: Entry, traced: bool, poll_traced: bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let host = if matches!(entry, Entry::Child) {
            Some(system.spawn_root_typed(Native(Arc::default()), EmptyConfig).await.unwrap())
        } else { None };
        let events = Events::default();
        let origin = traced.then(TraceContext::root);
        let polling = poll_traced.then(TraceContext::root);
        let work = trace::in_scope(origin.clone(), || spawn(&system, host.as_ref(), events.clone(), entry));
        let reference = trace::scope(polling.clone(), work).await.unwrap();
        let work = trace::in_scope(None, || reference.send(Box::new(0_usize)));
        let observed = *trace::scope(polling.clone(), work).await.unwrap().downcast::<Option<TraceContext>>().unwrap();
        assert!(observed.is_none(), "factory creation must not leak into later untraced messages");
        let followup = TraceContext::root();
        let work = followup.in_scope(|| reference.send(Box::new(1_usize)));
        let observed = *trace::scope(polling.clone(), work).await.unwrap().downcast::<Option<TraceContext>>().unwrap();
        let observed = observed.unwrap();
        assert_eq!(observed.trace_id(), followup.trace_id());
        assert_ne!(observed.span_id(), followup.span_id());
        trace::scope(polling, reference.stop()).await.unwrap();
        system.clone().shutdown().await.unwrap();
        assert_eq!(system.status().active_actors, 0);
        assert!(TraceContext::current().is_none());
        // Snapshot after capture retirement, before any negative identity assertion.
        let rows = events.lock().unwrap().clone();
        let phase = |name| {
            let values: Vec<_> = rows.iter().filter(|row| row.0 == name).map(|row| row.1.clone()).collect();
            assert_eq!(values.len(), 1, "each phase executes once: {name}");
            values[0].clone()
        };
        let called = phase("call");
        if matches!(entry, Entry::Child) {
            match (&origin, &called) {
                (Some(parent), Some(child)) => {
                    assert_eq!(child.trace_id(), parent.trace_id());
                    assert_ne!(child.span_id(), parent.span_id());
                }
                (None, None) => {},
                _ => panic!("child spawn call lost its actual parent handler identity"),
            }
        } else { assert_eq!(called, origin); }
        assert!(phase("untraced-handler").is_none());
        assert_eq!(phase("traced-handler"), Some(observed));
        let actual: Vec<_> = ["constructor", "init-call", "init-poll", "init-resumed", "factory-drop"]
            .into_iter().map(|name| (name, phase(name))).collect();
        let expected: Vec<_> = actual.iter().map(|(name, _)| (*name, called.clone())).collect();
        assert_eq!(actual, expected, "Actix factory creation must preserve its call identity");
        println!("PARROT_ACTIX_FACTORY_CREATION_CASE_OK entry={entry:?} traced={traced} poll_traced={poll_traced}");
    }).await.expect("Actix factory creation independent liveness bound");
}
macro_rules! case {
    ($name:ident, $entry:ident, $traced:literal) => {
        #[actix::test]
        async fn $name() {
            for poll_traced in [false, true] {
                scenario(Entry::$entry, $traced, poll_traced).await;
            }
        }
    };
}
case!(default_factory_untraced, Default, false);
case!(default_factory_traced, Default, true);
case!(custom_factory_untraced, Custom, false);
case!(custom_factory_traced, Custom, true);
case!(erased_factory_untraced, Erased, false);
case!(erased_factory_traced, Erased, true);
case!(child_factory_untraced, Child, false);
case!(child_factory_traced, Child, true);
