use parrot::typed_context::TypedContextExt;
use parrot_api::{
    errors::ActorError,
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

struct Context;
impl Message for Context {
    type Result = TypedContext;
}
struct Parent;
impl ParrotMsgVariant<Parent> for Context {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: TypedContext) -> ActorResult<TypedContext> {
        Ok(reply)
    }
}
impl ParrotTypedDispatch for Parent {
    type Msg = Context;
    type Reply = TypedContext;
    fn dispatch(&mut self, _: Context) -> BoxedFuture<'_, ActorResult<TypedContext>> {
        Box::pin(async { Err(ActorError::InternalError("managed context required".into())) })
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        _: Context,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<TypedContext>> {
        Box::pin(async move { Ok(context.clone()) })
    }
}
struct LocalParent(Rc<()>);
impl ParrotMsgVariant<LocalProtocol<LocalParent>> for Context {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: TypedContext) -> ActorResult<TypedContext> {
        Ok(reply)
    }
}
impl LocalTypedDispatch for LocalParent {
    type Msg = Context;
    type Reply = TypedContext;
    fn dispatch<'a>(
        &'a mut self,
        _: Context,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<TypedContext>> {
        assert_eq!(Rc::strong_count(&self.0), 1);
        Box::pin(async move { Ok(context.clone()) })
    }
}
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
        self.0.note("drop");
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
            self.0.note("message-call");
            tokio::task::yield_now().await;
            self.0.note("message-resumed");
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
fn policy() -> SupervisorStrategyType {
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    })
}
enum Root {
    Typed(TypedActorRef<Parent, Context>),
    Local(LocalTypedActorRef<LocalParent, Context>),
}
impl Root {
    async fn context(&self) -> TypedContext {
        match self {
            Self::Typed(actor) => actor.ask(Context).await.unwrap(),
            Self::Local(actor) => actor.ask(Context).await.unwrap(),
        }
    }
    async fn stop(&self) {
        match self {
            Self::Typed(actor) => actor.stop().await.unwrap(),
            Self::Local(actor) => actor.stop().await.unwrap(),
        }
    }
}
enum Engine {
    Thread(Arc<parrot::thread::ThreadActorSystem>),
    Actix(Arc<parrot::actix::ActixActorSystem>),
}
impl Engine {
    async fn setup(engine: usize) -> (Self, Root) {
        if engine == 0 {
            let system = parrot::thread::ThreadActorSystem::shared(
                parrot::thread::ThreadActorSystemConfig {
                    shared_pool_size: 1,
                    shared_burst_workers_max: 0,
                    ..Default::default()
                },
            );
            let parent = system.spawn_typed(Parent, "/parent").await.unwrap();
            (Self::Thread(system), Root::Typed(parent))
        } else {
            let system = Arc::new(
                parrot::actix::ActixActorSystem::with_arbiter_count(1)
                    .await
                    .unwrap(),
            );
            let parent = if engine == 1 {
                Root::Typed(system.spawn_typed(Parent, "/parent").await.unwrap())
            } else {
                Root::Local(
                    system
                        .spawn_local_typed(|| LocalParent(Rc::new(())), "/parent")
                        .await
                        .unwrap(),
                )
            };
            (Self::Actix(system), parent)
        }
    }
    async fn shutdown(&self) {
        match self {
            Self::Thread(system) => {
                system.shutdown_internal().await.unwrap();
                assert_eq!(system.actor_count(), 0);
            }
            Self::Actix(system) => {
                system.as_ref().clone().shutdown().await.unwrap();
                assert_eq!(system.status().active_actors, 0);
            }
        }
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
            "creation must not leak into later messages"
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
) -> (Vec<Option<TraceContext>>, Vec<Option<TraceContext>>) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let (system, parent) = Engine::setup(engine).await;
        let context = parent.context().await;
        let state = State::new();
        let origin = traced.then(TraceContext::root);
        let ambient = poll_traced.then(TraceContext::root);
        let made = state.clone();
        if engine == 2 {
            let work = trace::in_scope(origin.clone(), || {
                let constructor = move || Local { actor: Probe::new(made.clone()), owner: Rc::new(()) };
                if repeatable {
                    if explicit { context.spawn_local_typed_child_factory_with_strategy(constructor, policy()) }
                    else { context.spawn_local_typed_child_factory(constructor) }
                } else if explicit { context.spawn_local_typed_child_with_strategy(constructor, policy()) }
                else { context.spawn_local_typed_child(constructor) }
            });
            let actor = trace::scope(ambient.clone(), work).await.unwrap();
            state.wait().await;
            messages!(actor, ambient);
        } else {
            let work = trace::in_scope(origin.clone(), || {
                let factory = move || Probe::new(made.clone());
                if repeatable {
                    if explicit { context.spawn_typed_child_factory_with_strategy(factory, policy()) }
                    else { context.spawn_typed_child_factory(factory) }
                } else if explicit { context.spawn_typed_child_with_strategy(factory(), policy()) }
                else { context.spawn_typed_child(factory()) }
            });
            let actor = trace::scope(ambient.clone(), work).await.unwrap();
            state.wait().await;
            messages!(actor, ambient);
        }
        assert_eq!(context.children().len(), 1);
        parent.stop().await;
        system.shutdown().await;
        assert!(context.children().is_empty());
        assert_eq!(state.made.load(Ordering::SeqCst), 1);
        assert_eq!(state.dropped.load(Ordering::SeqCst), 1);
        assert!(TraceContext::current().is_none());
        let events = state.events.lock().unwrap();
        let lifecycle: Vec<_> = events.iter().filter(|event| matches!(event.0, "constructor" | "init-call" | "init-poll" | "init-resumed")).collect();
        assert_eq!(lifecycle.iter().map(|event| event.0).collect::<Vec<_>>(),
                   ["constructor", "init-call", "init-poll", "init-resumed"]);
        if engine == 2 { assert!(events.iter().all(|event| event.2 == events[0].2), "Local lifecycle and messages stay on their owner"); }
        let observed: Vec<_> = lifecycle.iter().map(|event| event.1.clone()).collect();
        println!("PARROT_TYPED_CHILD_CREATION_RETIRED engine={engine} repeatable={repeatable} explicit={explicit} traced={traced} poll_traced={poll_traced}");
        (observed, vec![origin; 4])
    }).await.expect("typed child creation independent liveness bound")
}
async fn matrix(engine: usize, repeatable: bool, explicit: bool) {
    let mut observed = Vec::new();
    let mut expected = Vec::new();
    for traced in [false, true] {
        for poll_traced in [false, true] {
            let (actual, required) =
                scenario(engine, repeatable, explicit, traced, poll_traced).await;
            observed.extend(actual);
            expected.extend(required);
        }
    }
    assert_eq!(
        observed, expected,
        "typed child construction and initialization must retain their call-time identity"
    );
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
