use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorSpawnerExt},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

type Event = (usize, &'static str, Option<TraceContext>);
struct State {
    next: AtomicUsize,
    events: Mutex<Vec<Event>>,
    ready: tokio::sync::watch::Sender<Option<usize>>,
}
impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            next: AtomicUsize::new(0),
            events: Mutex::default(),
            ready: tokio::sync::watch::channel(None).0,
        })
    }
    fn note(&self, generation: usize, phase: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((generation, phase, TraceContext::current()));
    }
}
fn policy(state: Arc<State>, parent: bool) -> OneForOneStrategy {
    OneForOneStrategy {
        max_restarts: u32::from(parent),
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |error| {
            assert!(matches!(
                error,
                ActorError::MessageHandlingError(_) | ActorError::Panic(_)
            ));
            state.note(
                0,
                if parent {
                    "parent-policy"
                } else {
                    "child-policy"
                },
            );
            if parent {
                SupervisionDecision::Restart
            } else {
                SupervisionDecision::Escalate
            }
        }),
    }
}
struct Parent {
    state: Arc<State>,
    generation: usize,
}
impl Parent {
    fn new(state: Arc<State>) -> Self {
        let generation = state.next.fetch_add(1, Ordering::SeqCst);
        state.note(generation, "constructor");
        Self { state, generation }
    }
}
struct MakeParent(Arc<State>);
struct MakeChild(bool);
struct Fail(bool);
type Receipt = (usize, Option<TraceContext>);
impl Actor for Parent {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.state.note(self.generation, "init-call");
        Box::pin(async move {
            self.state.note(self.generation, "init-poll");
            tokio::task::yield_now().await;
            self.state.note(self.generation, "init-resumed");
            self.state.ready.send_replace(Some(self.generation));
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<MakeParent>() {
                let state = message.downcast::<MakeParent>().unwrap().0;
                let strategy = SupervisorStrategyType::OneForOne(policy(state.clone(), true));
                let parent = ctx
                    .spawner()
                    .spawn_factory_with_strategy(move || Self::new(state.clone()), strategy)
                    .await?;
                return Ok(Box::new(parent) as BoxedMessage);
            }
            if message.is::<MakeChild>() {
                let factory = message.downcast::<MakeChild>().unwrap().0;
                let state = self.state.clone();
                let strategy = SupervisorStrategyType::OneForOne(policy(state.clone(), false));
                let child = if factory {
                    ctx.spawner()
                        .spawn_factory_with_strategy(move || Leaf(state.clone()), strategy)
                        .await?
                } else {
                    ctx.spawner()
                        .spawn_supervised(Leaf(state), EmptyConfig, strategy)
                        .await?
                };
                return Ok(Box::new(child) as BoxedMessage);
            }
            Ok(Box::new((self.generation, TraceContext::current())) as BoxedMessage)
        })
    }
}
impl ActixEngineExt for Parent {}
struct Leaf(Arc<State>);
impl Actor for Leaf {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let panic = message.downcast::<Fail>().unwrap().0;
            self.0.note(0, "failure-call");
            tokio::task::yield_now().await;
            self.0.note(0, "failure-resumed");
            assert!(!panic, "deliberate Actix escalation causal probe panic");
            Err(ActorError::MessageHandlingError(
                "deliberate Actix escalation causal probe error".into(),
            ))
        })
    }
}
impl ActixEngineExt for Leaf {}
async fn ready(state: &State, generation: usize) {
    let mut ready = state.ready.subscribe();
    loop {
        let current = *ready.borrow_and_update();
        if current == Some(generation) {
            return;
        }
        assert!(current.is_none_or(|current| current < generation));
        ready.changed().await.unwrap();
    }
}
async fn scenario(
    managed: bool,
    factory: bool,
    tell: bool,
    panic: bool,
    traced: bool,
    poll_traced: bool,
) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let state = State::new();
        let original = TraceContext::root();
        let ambient = poll_traced.then(TraceContext::root);
        let parent: BoxedActorRef = if managed {
            let host = system.spawn_root_typed(Parent::new(State::new()), EmptyConfig).await.unwrap();
            let creation = original.in_scope(|| host.send(Box::new(MakeParent(state.clone()))));
            *trace::scope(ambient.clone(), creation).await.unwrap().downcast::<BoxedActorRef>().unwrap()
        } else {
            let constructed = state.clone();
            let creation = original.in_scope(|| system.spawn_root_factory_with_strategy(
                move || Parent::new(constructed.clone()), policy(state.clone(), true)));
            trace::scope(ambient.clone(), creation).await.unwrap()
        };
        ready(&state, 0).await;
        let child = *parent.send(Box::new(MakeChild(factory))).await.unwrap().downcast::<BoxedActorRef>().unwrap();
        let cause = traced.then(TraceContext::root);
        if tell {
            let work = trace::in_scope(cause.clone(), || child.deliver(Box::new(Fail(panic))));
            trace::scope(ambient.clone(), work).await.unwrap();
        } else {
            let work = trace::in_scope(cause.clone(), || child.send(Box::new(Fail(panic))));
            assert!(trace::scope(ambient.clone(), work).await.is_err());
        }
        ready(&state, 1).await;
        let work = trace::in_scope(None, || parent.send(Box::new(())));
        let receipt = trace::scope(ambient.clone(), work).await.unwrap().downcast::<Receipt>().unwrap();
        assert_eq!(receipt.0, 1);
        assert!(receipt.1.is_none(), "escalated identity must not leak into later messages");
        let followup = TraceContext::root();
        let work = followup.in_scope(|| parent.send(Box::new(())));
        let receipt = trace::scope(ambient, work).await.unwrap().downcast::<Receipt>().unwrap();
        assert_eq!(receipt.0, 1);
        let observed = receipt.1.as_ref().unwrap();
        assert_eq!(observed.trace_id(), followup.trace_id());
        assert_ne!(observed.span_id(), followup.span_id());
        parent.stop().await.unwrap();
        system.clone().shutdown().await.unwrap();
        assert_eq!(system.status().active_actors, 0);
        assert_eq!(state.next.load(Ordering::SeqCst), 2);
        assert!(TraceContext::current().is_none());
        let events = state.events.lock().unwrap().clone();
        let phase = |generation, name| {
            let values: Vec<_> = events.iter().filter(|event| event.0 == generation && event.1 == name).collect();
            assert_eq!(values.len(), 1, "each escalation phase executes once: {generation}/{name}");
            values[0].2.clone()
        };
        let initial = phase(0, "constructor").unwrap();
        assert_eq!(initial.trace_id(), original.trace_id());
        for name in ["init-call", "init-poll", "init-resumed"] {
            assert_eq!(phase(0, name), Some(initial.clone()));
        }
        let failure = phase(0, "failure-call");
        assert_eq!(phase(0, "failure-resumed"), failure);
        match (&cause, &failure) {
            (Some(parent), Some(observed)) => {
                assert_eq!(observed.trace_id(), parent.trace_id());
                assert_ne!(observed.span_id(), parent.span_id());
            }
            (None, None) => {},
            mismatch => panic!("child failure ancestry mismatch: {mismatch:?}"),
        }
        let actual: Vec<_> = [(0, "child-policy"), (0, "parent-policy"), (1, "constructor"),
            (1, "init-call"), (1, "init-poll"), (1, "init-resumed")]
            .into_iter().map(|(generation, name)| (name, phase(generation, name))).collect();
        let expected: Vec<_> = actual.iter().map(|(name, _)| (*name, failure.clone())).collect();
        assert_eq!(actual, expected, "Actix escalation recovery must preserve the child failure identity");
        println!("PARROT_ACTIX_ESCALATION_CAUSAL_CASE_OK managed={managed} factory={factory} tell={tell} panic={panic} traced={traced} poll_traced={poll_traced}");
    }).await.expect("Actix escalation recovery independent liveness bound");
}
async fn matrix(managed: bool, factory: bool, tell: bool, panic: bool) {
    for traced in [false, true] {
        for poll_traced in [false, true] {
            scenario(managed, factory, tell, panic, traced, poll_traced).await;
        }
    }
}
macro_rules! case {
    ($name:ident, $managed:literal, $factory:literal, $tell:literal, $panic:literal) => {
        #[actix::test]
        async fn $name() {
            matrix($managed, $factory, $tell, $panic).await;
        }
    };
}
case!(root_instance_ask_error, false, false, false, false);
case!(root_instance_tell_error, false, false, true, false);
case!(root_instance_ask_panic, false, false, false, true);
case!(root_instance_tell_panic, false, false, true, true);
case!(root_factory_ask_error, false, true, false, false);
case!(root_factory_tell_error, false, true, true, false);
case!(root_factory_ask_panic, false, true, false, true);
case!(root_factory_tell_panic, false, true, true, true);
case!(managed_instance_ask_error, true, false, false, false);
case!(managed_instance_tell_error, true, false, true, false);
case!(managed_instance_ask_panic, true, false, false, true);
case!(managed_instance_tell_panic, true, false, true, true);
case!(managed_factory_ask_error, true, true, false, false);
case!(managed_factory_tell_error, true, true, true, false);
case!(managed_factory_ask_panic, true, true, false, true);
case!(managed_factory_tell_panic, true, true, true, true);
