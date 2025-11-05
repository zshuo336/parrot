use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig, EngineContextHandle},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision},
    trace::{self, TraceContext},
    types::{ActorResult, BoxedFuture, BoxedMessage},
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
    fn note(&self, generation: usize, phase: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((generation, phase, TraceContext::current()));
    }
}
struct Native {
    state: Arc<State>,
    generation: usize,
    asynchronous: bool,
}
struct Fail(bool);
struct Receipt(usize, Option<TraceContext>);
impl Native {
    fn process(&mut self, message: BoxedMessage) -> ActorResult<BoxedMessage> {
        if let Ok(failure) = message.downcast::<Fail>() {
            self.state.note(self.generation, "failure-handler");
            assert!(!failure.0, "deliberate Actix restart causal probe panic");
            return Err(ActorError::MessageHandlingError(
                "deliberate Actix restart causal probe error".into(),
            ));
        }
        Ok(Box::new(Receipt(self.generation, TraceContext::current())))
    }
}
impl Actor for Native {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        self.asynchronous
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
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert!(
            self.asynchronous,
            "sync probe must use the actual engine dispatch"
        );
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.process(message)
        })
    }
}
impl ActixEngineExt for Native {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
        _: EngineContextHandle<'_>,
    ) -> Option<ActorResult<BoxedMessage>> {
        assert!(
            !self.asynchronous,
            "async probe must use the actual future dispatch"
        );
        Some(self.process(message))
    }
}
async fn ready(state: &State, expected: usize) {
    let mut receiver = state.ready.subscribe();
    loop {
        let observed = *receiver.borrow_and_update();
        if observed == Some(expected) {
            return;
        }
        assert!(observed.is_none_or(|generation| generation < expected));
        receiver.changed().await.unwrap();
    }
}
async fn scenario(asynchronous: bool, tell: bool, panic: bool, traced: bool, poll_traced: bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let state = Arc::new(State {
            next: AtomicUsize::new(0), events: Mutex::default(),
            ready: tokio::sync::watch::channel(None).0,
        });
        let constructed = state.clone();
        let policy_state = state.clone();
        let creation = TraceContext::root();
        let ambient = poll_traced.then(TraceContext::root);
        let work = creation.in_scope(|| system.spawn_root_factory_with_strategy(move || {
            let generation = constructed.next.fetch_add(1, Ordering::SeqCst);
            constructed.note(generation, "constructor");
            Native { state: constructed.clone(), generation, asynchronous }
        }, OneForOneStrategy {
            max_restarts: 1, within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(move |_| {
                policy_state.note(0, "policy");
                SupervisionDecision::Restart
            }),
        }));
        let actor = trace::scope(ambient.clone(), work).await.unwrap();
        ready(&state, 0).await;
        let cause = traced.then(TraceContext::root);
        let failing: BoxedMessage = Box::new(Fail(panic));
        if tell {
            let work = trace::in_scope(cause.clone(), || actor.deliver(failing));
            trace::scope(ambient.clone(), work).await.unwrap();
        } else {
            let work = trace::in_scope(cause.clone(), || actor.send(failing));
            assert!(trace::scope(ambient.clone(), work).await.is_err());
        }
        ready(&state, 1).await;
        let work = trace::in_scope(None, || actor.send(Box::new(())));
        let receipt = trace::scope(ambient.clone(), work).await.unwrap().downcast::<Receipt>().unwrap();
        assert_eq!(receipt.0, 1);
        assert!(receipt.1.is_none(), "restart identity must not leak into subsequent messages");
        let followup = TraceContext::root();
        let work = followup.in_scope(|| actor.send(Box::new(())));
        let receipt = trace::scope(ambient.clone(), work).await.unwrap().downcast::<Receipt>().unwrap();
        assert_eq!(receipt.0, 1);
        let observed = receipt.1.as_ref().unwrap();
        assert_eq!(observed.trace_id(), followup.trace_id());
        assert_ne!(observed.span_id(), followup.span_id());
        actor.stop().await.unwrap();
        system.clone().shutdown().await.unwrap();
        assert_eq!(system.status().active_actors, 0);
        assert_eq!(state.next.load(Ordering::SeqCst), 2);
        assert!(TraceContext::current().is_none());
        let events = state.events.lock().unwrap().clone();
        let phase = |generation, name| {
            let matches: Vec<_> = events.iter().filter(|event| event.0 == generation && event.1 == name).collect();
            assert_eq!(matches.len(), 1, "each restart phase executes once: {generation}/{name}");
            matches[0].2.clone()
        };
        for name in ["constructor", "init-call", "init-poll", "init-resumed"] {
            assert_eq!(phase(0, name), Some(creation.clone()), "initial creation ancestry");
        }
        let failure = phase(0, "failure-handler");
        match (&cause, &failure) {
            (Some(parent), Some(child)) => {
                assert_eq!(child.trace_id(), parent.trace_id());
                assert_ne!(child.span_id(), parent.span_id());
            },
            (None, None) => {},
            _ => panic!("failure handler must retain its message ancestry"),
        }
        let actual: Vec<_> = [(0, "policy"), (1, "constructor"), (1, "init-call"), (1, "init-poll"), (1, "init-resumed")]
            .into_iter().map(|(generation, name)| (name, phase(generation, name))).collect();
        let expected: Vec<_> = actual.iter().map(|(name, _)| (*name, failure.clone())).collect();
        assert_eq!(actual, expected, "Actix restart must preserve the failing handler identity");
        println!("PARROT_ACTIX_RESTART_CAUSAL_CASE_OK async={asynchronous} tell={tell} panic={panic} traced={traced} poll_traced={poll_traced}");
    }).await.expect("Actix restart independent liveness bound");
}
async fn matrix(asynchronous: bool, tell: bool, panic: bool) {
    for traced in [false, true] {
        for poll_traced in [false, true] {
            scenario(asynchronous, tell, panic, traced, poll_traced).await;
        }
    }
}
macro_rules! case {
    ($name:ident, $asynchronous:literal, $tell:literal, $panic:literal) => {
        #[actix::test]
        async fn $name() {
            matrix($asynchronous, $tell, $panic).await;
        }
    };
}
case!(sync_ask_error, false, false, false);
case!(sync_ask_panic, false, false, true);
case!(sync_tell_error, false, true, false);
case!(sync_tell_panic, false, true, true);
case!(async_ask_error, true, false, false);
case!(async_ask_panic, true, false, true);
case!(async_tell_error, true, true, false);
case!(async_tell_panic, true, true, true);
