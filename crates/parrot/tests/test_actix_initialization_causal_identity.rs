use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Call,
    Poll,
    Resume,
}
type Event = (usize, &'static str, Option<TraceContext>);
struct State {
    next: AtomicUsize,
    events: Mutex<Vec<Event>>,
    ready: tokio::sync::watch::Sender<bool>,
}
impl State {
    fn note(&self, generation: usize, name: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((generation, name, TraceContext::current()));
    }
}
struct Native {
    state: Arc<State>,
    generation: usize,
    phase: Phase,
    panic: bool,
}
impl Native {
    fn fail(&self, phase: Phase) -> ActorResult<()> {
        if self.generation == 0 && self.phase == phase {
            self.state.note(self.generation, "failure");
            assert!(
                !self.panic,
                "deliberate Actix initialization causal probe panic"
            );
            return Err(ActorError::InitializationError(
                "deliberate Actix initialization causal probe error".into(),
            ));
        }
        Ok(())
    }
}
impl Actor for Native {
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
        if let Err(error) = self.fail(Phase::Call) {
            return Box::pin(async { Err(error) });
        }
        Box::pin(async move {
            self.state.note(self.generation, "init-poll");
            self.fail(Phase::Poll)?;
            tokio::task::yield_now().await;
            self.state.note(self.generation, "init-resumed");
            self.fail(Phase::Resume)?;
            self.state.ready.send_replace(true);
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(
            async move { Ok(Box::new((self.generation, TraceContext::current())) as BoxedMessage) },
        )
    }
}
impl ActixEngineExt for Native {}

async fn scenario(phase: Phase, panic: bool, traced: bool, poll_traced: bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let state = Arc::new(State {
            next: AtomicUsize::new(0), events: Mutex::default(), ready: tokio::sync::watch::channel(false).0,
        });
        let creation = traced.then(TraceContext::root);
        let ambient = poll_traced.then(TraceContext::root);
        let constructed = state.clone();
        let policy_state = state.clone();
        let work = trace::in_scope(creation.clone(), || system.spawn_root_factory_with_strategy(move || {
            let generation = constructed.next.fetch_add(1, Ordering::SeqCst);
            constructed.note(generation, "constructor");
            Native { state: constructed.clone(), generation, phase, panic }
        }, OneForOneStrategy {
            max_restarts: 1, within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(move |_| {
                policy_state.note(0, "policy");
                SupervisionDecision::Restart
            }),
        }));
        let actor = trace::scope(ambient.clone(), work).await.unwrap();
        let mut ready = state.ready.subscribe();
        while !*ready.borrow_and_update() { ready.changed().await.unwrap(); }
        let work = trace::in_scope(None, || actor.send(Box::new(())));
        let receipt = trace::scope(ambient.clone(), work).await.unwrap().downcast::<(usize, Option<TraceContext>)>().unwrap();
        assert_eq!(receipt.0, 1);
        assert!(receipt.1.is_none(), "initialization failure identity must not leak into later messages");
        let followup = TraceContext::root();
        let work = followup.in_scope(|| actor.send(Box::new(())));
        let receipt = trace::scope(ambient, work).await.unwrap().downcast::<(usize, Option<TraceContext>)>().unwrap();
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
        let event = |generation, name| {
            let values: Vec<_> = events.iter().filter(|event| event.0 == generation && event.1 == name).collect();
            assert_eq!(values.len(), 1, "each initialization phase executes once: {generation}/{name}");
            values[0].2.clone()
        };
        assert_eq!(event(0, "constructor"), creation);
        let failure = event(0, "failure");
        assert_eq!(failure, creation, "initialization failure must belong to the creation call");
        for name in ["init-call", "init-poll", "init-resumed"] {
            let present = name == "init-call" || phase != Phase::Call && (name != "init-resumed" || phase == Phase::Resume);
            if present { assert_eq!(event(0, name), creation); }
            else { assert!(!events.iter().any(|event| event.0 == 0 && event.1 == name)); }
        }
        let actual: Vec<_> = [(0, "policy"), (1, "constructor"), (1, "init-call"), (1, "init-poll"), (1, "init-resumed")]
            .into_iter().map(|(generation, name)| (name, event(generation, name))).collect();
        let expected: Vec<_> = actual.iter().map(|(name, _)| (*name, failure.clone())).collect();
        assert_eq!(actual, expected, "Actix initialization recovery must preserve the failure identity");
        println!("PARROT_ACTIX_INITIALIZATION_CAUSAL_CASE_OK phase={phase:?} panic={panic} traced={traced} poll_traced={poll_traced}");
    }).await.expect("Actix initialization recovery independent liveness bound");
}
async fn matrix(phase: Phase, panic: bool) {
    for traced in [false, true] {
        for poll_traced in [false, true] {
            scenario(phase, panic, traced, poll_traced).await;
        }
    }
}
macro_rules! case {
    ($name:ident, $phase:ident, $panic:literal) => {
        #[actix::test]
        async fn $name() {
            matrix(Phase::$phase, $panic).await;
        }
    };
}
case!(call_error, Call, false);
case!(call_panic, Call, true);
case!(poll_error, Poll, false);
case!(poll_panic, Poll, true);
case!(resume_error, Resume, false);
case!(resume_panic, Resume, true);
