use futures::Stream;
use parrot::typed_context::TypedContextExt;
use parrot_api::{
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalStreamContext, LocalTypedDispatch},
    message::Message,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    trace::{self, TraceContext},
    typed::ParrotMsgVariant,
    typed_context::TypedContext,
    types::ActorResult,
};
use std::{
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    thread::ThreadId,
    time::Duration,
};

type Event = (usize, &'static str, Option<TraceContext>, ThreadId);
struct State {
    made: AtomicUsize,
    dropped: AtomicUsize,
    streams_dropped: AtomicUsize,
    policies: AtomicUsize,
    events: Mutex<Vec<Event>>,
    polled: tokio::sync::watch::Sender<bool>,
    point: Option<usize>,
    panic: bool,
}
impl State {
    fn new(point: Option<usize>, panic: bool) -> Arc<Self> {
        Arc::new(Self {
            made: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            streams_dropped: AtomicUsize::new(0),
            policies: AtomicUsize::new(0),
            events: Mutex::default(),
            polled: tokio::sync::watch::channel(false).0,
            point,
            panic,
        })
    }
    fn note(&self, generation: usize, phase: &'static str) {
        self.events.lock().unwrap().push((
            generation,
            phase,
            TraceContext::current(),
            std::thread::current().id(),
        ));
    }
}
struct Parent(Rc<()>);
struct GetContext;
impl Message for GetContext {
    type Result = TypedContext;
}
impl ParrotMsgVariant<LocalProtocol<Parent>> for GetContext {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: TypedContext) -> ActorResult<TypedContext> {
        Ok(reply)
    }
}
impl LocalTypedDispatch for Parent {
    type Msg = GetContext;
    type Reply = TypedContext;
    fn dispatch<'a>(
        &'a mut self,
        _: GetContext,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<TypedContext>> {
        assert_eq!(Rc::strong_count(&self.0), 1);
        Box::pin(async move { Ok(context.clone()) })
    }
}
struct Source {
    state: Arc<State>,
    generation: usize,
    owner: Rc<()>,
    first: bool,
}
impl Stream for Source {
    type Item = ();
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<()>> {
        assert_eq!(Rc::strong_count(&self.owner), 2);
        if self.first {
            self.first = false;
            self.state.note(self.generation, "source-poll");
            self.state.polled.send_replace(true);
        }
        Poll::Pending
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.state.note(self.generation, "source-drop");
        self.state.streams_dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Actor {
    state: Arc<State>,
    generation: usize,
    owner: Rc<()>,
}
impl Actor {
    fn new(state: Arc<State>) -> Self {
        let generation = state.made.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            state.dropped.load(Ordering::SeqCst),
            generation,
            "previous actor actually retired"
        );
        assert_eq!(
            state.streams_dropped.load(Ordering::SeqCst),
            generation,
            "previous stream actually retired"
        );
        assert!(
            generation < 3,
            "stream initialization restart budget exceeded"
        );
        state.note(generation, "constructor");
        Self {
            state,
            generation,
            owner: Rc::new(()),
        }
    }
    fn failure(&self) -> ActorResult<()> {
        self.state.note(self.generation, "failure");
        assert!(
            !self.state.panic,
            "deliberate local stream initialization panic"
        );
        Err(ActorError::MessageHandlingError(
            "deliberate local stream initialization error".into(),
        ))
    }
}
impl Drop for Actor {
    fn drop(&mut self) {
        self.state.note(self.generation, "actor-drop");
        self.state.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Query;
type Receipt = (usize, Option<TraceContext>);
impl Message for Query {
    type Result = Receipt;
}
impl ParrotMsgVariant<LocalProtocol<Actor>> for Query {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: Receipt) -> ActorResult<Receipt> {
        Ok(reply)
    }
}
impl LocalTypedDispatch for Actor {
    type Msg = Query;
    type Reply = Receipt;
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.state.note(self.generation, "init-call");
        Box::pin(async move {
            self.state.note(self.generation, "init-poll");
            tokio::task::yield_now().await;
            self.state.note(self.generation, "init-resumed");
            Ok(())
        })
    }
    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        streams: &'a mut LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.state.note(self.generation, "streams-call");
        streams
            .add_stream(Source {
                state: self.state.clone(),
                generation: self.generation,
                owner: self.owner.clone(),
                first: true,
            })
            .unwrap();
        if self.generation < 2 && self.state.point == Some(0) {
            let result = self.failure();
            return Box::pin(async move { result });
        }
        Box::pin(async move {
            self.state.note(self.generation, "streams-poll");
            if self.generation < 2 && self.state.point == Some(1) {
                return self.failure();
            }
            tokio::task::yield_now().await;
            self.state.note(self.generation, "streams-resumed");
            if self.generation < 2 && self.state.point.is_some() {
                return self.failure();
            }
            Ok(())
        })
    }
    fn dispatch<'a>(
        &'a mut self,
        _: Query,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<Receipt>> {
        assert_eq!(Rc::strong_count(&self.owner), 2);
        self.state.note(self.generation, "message-call");
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.state.note(self.generation, "message-resumed");
            Ok((self.generation, TraceContext::current()))
        })
    }
}
async fn scenario(
    factory: bool,
    child: bool,
    point: Option<usize>,
    panic: bool,
    traced: bool,
    poll_traced: bool,
) -> (Vec<Option<TraceContext>>, Vec<Option<TraceContext>>) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let state = State::new(point, panic);
        let system = Arc::new(parrot::actix::ActixActorSystem::with_arbiter_count(1).await.unwrap());
        let parent = if child { Some(system.spawn_local_typed(|| Parent(Rc::new(())), "/parent").await.unwrap()) } else { None };
        let context = if let Some(parent) = &parent { Some(parent.ask(GetContext).await.unwrap()) } else { None };
        let origin = traced.then(TraceContext::root);
        let ambient = poll_traced.then(TraceContext::root);
        let made = state.clone();
        let make = move || Actor::new(made.clone());
        let policy_state = state.clone();
        let strategy = OneForOneStrategy {
            max_restarts: 2,
            within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(move |_| {
                let generation = policy_state.policies.fetch_add(1, Ordering::SeqCst);
                policy_state.note(generation, "policy");
                SupervisionDecision::Restart
            }),
        };
        let actor = if let Some(context) = &context {
            if factory {
                let work = trace::in_scope(origin.clone(), || context.spawn_local_typed_child_factory_with_strategy(make, SupervisorStrategyType::OneForOne(strategy)));
                trace::scope(ambient.clone(), work).await.unwrap()
            } else {
                let work = trace::in_scope(origin.clone(), || context.spawn_local_typed_child(make));
                trace::scope(ambient.clone(), work).await.unwrap()
            }
        } else if factory {
            let work = trace::in_scope(origin.clone(), || system.spawn_local_typed_factory_with_strategy(make, "/root", strategy));
            trace::scope(ambient.clone(), work).await.unwrap()
        } else {
            let work = trace::in_scope(origin.clone(), || system.spawn_local_typed(make, "/root"));
            trace::scope(ambient.clone(), work).await.unwrap()
        };
        let expected_generation = if point.is_some() { 2 } else { 0 };
        let work = trace::in_scope(None, || actor.ask(Query));
        assert_eq!(trace::scope(ambient.clone(), work).await.unwrap(), (expected_generation, None), "initialization must not contaminate later messages");
        let independent = TraceContext::root();
        let work = independent.in_scope(|| actor.ask(Query));
        let receipt = trace::scope(ambient, work).await.unwrap();
        assert_eq!(receipt.0, expected_generation);
        let observed = receipt.1.unwrap();
        assert_eq!(observed.trace_id(), independent.trace_id());
        assert_ne!(observed.span_id(), independent.span_id());
        state.polled.subscribe().wait_for(|value| *value).await.unwrap();
        if let Some(parent) = parent {
            parent.stop().await.unwrap();
            assert!(context.as_ref().unwrap().children().is_empty());
        } else { actor.stop().await.unwrap(); }
        let generations = expected_generation + 1;
        assert_eq!(state.made.load(Ordering::SeqCst), generations);
        assert_eq!(state.dropped.load(Ordering::SeqCst), generations, "stop joins actual actor retirement");
        assert_eq!(state.streams_dropped.load(Ordering::SeqCst), generations, "stop joins actual source retirement");
        assert_eq!(state.policies.load(Ordering::SeqCst), expected_generation);
        system.as_ref().clone().shutdown().await.unwrap();
        assert_eq!(system.status().active_actors, 0);
        assert!(TraceContext::current().is_none());
        let events = state.events.lock().unwrap();
        let owner = events[0].3;
        assert!(events.iter().filter(|event| event.1 != "policy").all(|event| event.3 == owner), "local lifecycle and sources remain on the owner thread");
        let mut actual = Vec::new();
        for generation in 0..generations {
            let mut phases = vec!["constructor", "init-call", "init-poll", "init-resumed", "streams-call", "source-drop"];
            if generation == expected_generation || point != Some(0) { phases.push("streams-poll"); }
            if generation == expected_generation || point == Some(2) { phases.push("streams-resumed"); }
            if generation < expected_generation { phases.extend(["failure", "policy"]); }
            else { phases.push("source-poll"); }
            for phase in phases {
                let found: Vec<_> = events.iter().filter(|event| event.0 == generation && event.1 == phase).collect();
                assert_eq!(found.len(), 1, "each actual lifecycle phase occurs once: {generation}/{phase}");
                actual.push(found[0].2.clone());
            }
        }
        let expected = vec![origin; actual.len()];
        println!("PARROT_LOCAL_STREAM_INIT_RETIRED factory={factory} child={child} point={point:?} panic={panic} traced={traced} poll_traced={poll_traced}");
        (actual, expected)
    }).await.expect("local stream initialization independent liveness bound")
}
async fn matrix(factory: bool, child: bool, point: Option<usize>, panic: bool) {
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for traced in [false, true] {
        for poll_traced in [false, true] {
            let (observed, required) =
                scenario(factory, child, point, panic, traced, poll_traced).await;
            actual.extend(observed);
            expected.extend(required);
        }
    }
    assert_eq!(
        actual, expected,
        "local stream initialization and recovery must retain the creation cause"
    );
}
macro_rules! case {
    ($name:ident, $factory:literal, $child:literal, $point:expr, $panic:literal) => {
        #[actix::test]
        async fn $name() {
            matrix($factory, $child, $point, $panic).await;
        }
    };
}
case!(factory_root_call_error, true, false, Some(0), false);
case!(factory_root_call_panic, true, false, Some(0), true);
case!(factory_root_poll_error, true, false, Some(1), false);
case!(factory_root_poll_panic, true, false, Some(1), true);
case!(factory_root_resume_error, true, false, Some(2), false);
case!(factory_root_resume_panic, true, false, Some(2), true);
case!(factory_child_call_error, true, true, Some(0), false);
case!(factory_child_call_panic, true, true, Some(0), true);
case!(factory_child_poll_error, true, true, Some(1), false);
case!(factory_child_poll_panic, true, true, Some(1), true);
case!(factory_child_resume_error, true, true, Some(2), false);
case!(factory_child_resume_panic, true, true, Some(2), true);
case!(factory_root_success, true, false, None, false);
case!(factory_child_success, true, true, None, false);
case!(instance_root_success, false, false, None, false);
case!(instance_child_success, false, true, None, false);
