use parrot_api::{
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalTypedActorRef, LocalTypedDispatch},
    message::Message,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    trace::{self, TraceContext},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::TypedActorRef,
    typed_context::TypedContext,
    types::{ActorResult, BoxedFuture},
};
use std::{
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::ThreadId,
    time::Duration,
};

type Event = (&'static str, Option<TraceContext>, ThreadId);
struct State {
    made: AtomicUsize,
    dropped: AtomicUsize,
    events: Mutex<Vec<Event>>,
    ready: tokio::sync::watch::Sender<bool>,
}
impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            made: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            events: Mutex::default(),
            ready: tokio::sync::watch::channel(false).0,
        })
    }
    fn note(&self, phase: &'static str) {
        self.events.lock().unwrap().push((
            phase,
            TraceContext::current(),
            std::thread::current().id(),
        ));
    }
    async fn wait(&self) {
        let mut ready = self.ready.subscribe();
        while !*ready.borrow_and_update() {
            ready.changed().await.unwrap();
        }
    }
}
struct Inspect;
impl Message for Inspect {
    type Result = Option<TraceContext>;
}
struct Probe(Arc<State>);
impl Probe {
    fn new(state: Arc<State>) -> Self {
        state.made.fetch_add(1, Ordering::SeqCst);
        state.note("constructor");
        Self(state)
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl ParrotMsgVariant<Probe> for Inspect {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: Option<TraceContext>) -> ActorResult<Option<TraceContext>> {
        Ok(reply)
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Inspect;
    type Reply = Option<TraceContext>;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.0.note("init-call");
        Box::pin(async move {
            self.0.note("init-poll");
            tokio::task::yield_now().await;
            self.0.note("init-resumed");
            self.0.ready.send_replace(true);
            Ok(())
        })
    }
    fn dispatch(&mut self, _: Inspect) -> BoxedFuture<'_, ActorResult<Option<TraceContext>>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            Ok(TraceContext::current())
        })
    }
}
struct Local {
    actor: Probe,
    owner: Rc<()>,
}
impl ParrotMsgVariant<LocalProtocol<Local>> for Inspect {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: Option<TraceContext>) -> ActorResult<Option<TraceContext>> {
        Ok(reply)
    }
}
impl LocalTypedDispatch for Local {
    type Msg = Inspect;
    type Reply = Option<TraceContext>;
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        assert_eq!(Rc::strong_count(&self.owner), 1);
        ParrotTypedDispatch::init(&mut self.actor)
    }
    fn dispatch<'a>(
        &'a mut self,
        message: Inspect,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<Option<TraceContext>>> {
        assert_eq!(Rc::strong_count(&self.owner), 1);
        ParrotTypedDispatch::dispatch(&mut self.actor, message)
    }
}
fn policy() -> OneForOneStrategy {
    OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    }
}
macro_rules! messages {
    ($actor:expr, $ambient:expr) => {{
        let work = trace::in_scope(None, || $actor.ask(Inspect));
        assert!(
            trace::scope($ambient.clone(), work)
                .await
                .unwrap()
                .is_none(),
            "creation context must not leak into ordinary messages"
        );
        let followup = TraceContext::root();
        let work = followup.in_scope(|| $actor.ask(Inspect));
        let observed = trace::scope($ambient.clone(), work).await.unwrap().unwrap();
        assert_eq!(observed.trace_id(), followup.trace_id());
        assert_ne!(observed.span_id(), followup.span_id());
    }};
}
async fn scenario(
    engine: usize,
    repeatable: bool,
    explicit: bool,
    traced: bool,
    poll_traced: bool,
) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let state = State::new();
        let origin = traced.then(TraceContext::root);
        let ambient = poll_traced.then(TraceContext::root);
        let made = state.clone();
        match engine {
            0 => {
                let system = parrot::thread::ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
                    shared_pool_size: 1, shared_burst_workers_max: 0, ..Default::default()
                });
                let work = trace::in_scope(origin.clone(), || -> BoxedFuture<'_, ActorResult<TypedActorRef<Probe, Inspect>>> {
                    let factory = move || Probe::new(made.clone());
                    if repeatable {
                        if explicit { Box::pin(system.spawn_typed_factory_with_strategy(factory, "/probe", policy())) }
                        else { Box::pin(system.spawn_typed_factory(factory, "/probe")) }
                    } else if explicit {
                        Box::pin(system.spawn_typed_with_strategy(factory(), "/probe", SupervisorStrategyType::OneForOne(policy())))
                    } else { Box::pin(system.spawn_typed(factory(), "/probe")) }
                });
                let actor = trace::scope(ambient.clone(), work).await.unwrap();
                state.wait().await;
                messages!(actor, ambient);
                actor.stop().await.unwrap();
                system.shutdown_internal().await.unwrap();
                assert_eq!(system.actor_count(), 0);
            },
            1 => {
                let system = Arc::new(parrot::actix::ActixActorSystem::with_arbiter_count(1).await.unwrap());
                let work = trace::in_scope(origin.clone(), || -> BoxedFuture<'_, ActorResult<TypedActorRef<Probe, Inspect>>> {
                    let factory = move || Probe::new(made.clone());
                    if repeatable {
                        if explicit { Box::pin(system.spawn_typed_factory_with_strategy(factory, "/probe", policy())) }
                        else { Box::pin(system.spawn_typed_factory(factory, "/probe")) }
                    } else if explicit {
                        Box::pin(system.spawn_typed_with_strategy(factory(), "/probe", SupervisorStrategyType::OneForOne(policy())))
                    } else { Box::pin(system.spawn_typed(factory(), "/probe")) }
                });
                let actor = trace::scope(ambient.clone(), work).await.unwrap();
                state.wait().await;
                messages!(actor, ambient);
                actor.stop().await.unwrap();
                system.as_ref().clone().shutdown().await.unwrap();
                assert_eq!(system.status().active_actors, 0);
            },
            2 => {
                let system = Arc::new(parrot::actix::ActixActorSystem::with_arbiter_count(1).await.unwrap());
                let work = trace::in_scope(origin.clone(), || -> BoxedFuture<'_, ActorResult<LocalTypedActorRef<Local, Inspect>>> {
                    let factory = move || Local { actor: Probe::new(made.clone()), owner: Rc::new(()) };
                    if repeatable {
                        if explicit { Box::pin(system.spawn_local_typed_factory_with_strategy(factory, "/probe", policy())) }
                        else { Box::pin(system.spawn_local_typed_factory(factory, "/probe")) }
                    } else if explicit {
                        Box::pin(system.spawn_local_typed_with_strategy(factory, "/probe", SupervisorStrategyType::OneForOne(policy())))
                    } else { Box::pin(system.spawn_local_typed(factory, "/probe")) }
                });
                let actor = trace::scope(ambient.clone(), work).await.unwrap();
                state.wait().await;
                messages!(actor, ambient);
                actor.stop().await.unwrap();
                system.as_ref().clone().shutdown().await.unwrap();
                assert_eq!(system.status().active_actors, 0);
            },
            _ => unreachable!(),
        }
        assert_eq!(state.made.load(Ordering::SeqCst), 1);
        assert_eq!(state.dropped.load(Ordering::SeqCst), 1);
        assert!(TraceContext::current().is_none());
        let events = state.events.lock().unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events.iter().map(|event| event.0).collect::<Vec<_>>(),
                   ["constructor", "init-call", "init-poll", "init-resumed"]);
        if engine == 2 { assert!(events.iter().all(|event| event.2 == events[0].2), "Local construction and init stay on the owning arbiter"); }
        assert_eq!(events.iter().map(|event| event.1.clone()).collect::<Vec<_>>(), vec![origin; 4],
                   "typed factory construction and initialization must retain their call-time identity");
        println!("PARROT_TYPED_FACTORY_CREATION_CASE_OK engine={engine} repeatable={repeatable} explicit={explicit} traced={traced} poll_traced={poll_traced}");
    }).await.expect("typed factory creation independent liveness bound");
}
async fn matrix(engine: usize, repeatable: bool, explicit: bool) {
    for traced in [false, true] {
        for poll_traced in [false, true] {
            scenario(engine, repeatable, explicit, traced, poll_traced).await;
        }
    }
}
macro_rules! case {
    ($name:ident, $engine:literal, $repeatable:literal, $explicit:literal) => {
        #[actix::test]
        async fn $name() {
            matrix($engine, $repeatable, $explicit).await;
        }
    };
}
case!(thread_default_factory, 0, true, false);
case!(thread_explicit_factory, 0, true, true);
case!(actix_default_factory, 1, true, false);
case!(actix_explicit_factory, 1, true, true);
case!(local_default_factory, 2, true, false);
case!(local_explicit_factory, 2, true, true);
case!(thread_default_instance, 0, false, false);
case!(thread_explicit_instance, 0, false, true);
case!(actix_default_instance, 1, false, false);
case!(actix_explicit_instance, 1, false, true);
case!(local_default_one_shot, 2, false, false);
case!(local_explicit_one_shot, 2, false, true);
