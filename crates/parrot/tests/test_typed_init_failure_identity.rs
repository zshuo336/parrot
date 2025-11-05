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

type Event = (usize, &'static str, Option<TraceContext>, ThreadId);
struct State {
    made: AtomicUsize,
    dropped: AtomicUsize,
    policies: AtomicUsize,
    events: Mutex<Vec<Event>>,
    ready: tokio::sync::watch::Sender<bool>,
    point: usize,
    panic: bool,
}
impl State {
    fn new(point: usize, panic: bool) -> Arc<Self> {
        Arc::new(Self {
            made: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            policies: AtomicUsize::new(0),
            events: Mutex::default(),
            ready: tokio::sync::watch::channel(false).0,
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
    async fn initialized(&self) {
        let mut ready = self.ready.subscribe();
        ready.wait_for(|value| *value).await.unwrap();
    }
}
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
struct Query;
type Receipt = (usize, Option<TraceContext>);
impl Message for Query {
    type Result = Receipt;
}
struct Leaf {
    state: Arc<State>,
    generation: usize,
}
impl Leaf {
    fn new(state: Arc<State>) -> Self {
        let generation = state.made.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            state.dropped.load(Ordering::SeqCst),
            generation,
            "failed generation must actually retire before replacement"
        );
        assert!(generation < 3, "initialization restart budget exceeded");
        state.note(generation, "constructor");
        Self { state, generation }
    }
    fn failure(&self) -> ActorResult<()> {
        self.state.note(self.generation, "failure");
        assert!(
            !self.state.panic,
            "deliberate typed initialization causal panic"
        );
        Err(ActorError::MessageHandlingError(
            "deliberate typed initialization causal error".into(),
        ))
    }
}
impl Drop for Leaf {
    fn drop(&mut self) {
        self.state.note(self.generation, "drop");
        self.state.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl ParrotMsgVariant<Leaf> for Query {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: Receipt) -> ActorResult<Receipt> {
        Ok(reply)
    }
}
impl ParrotTypedDispatch for Leaf {
    type Msg = Query;
    type Reply = Receipt;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.state.note(self.generation, "init-call");
        if self.generation < 2 && self.state.point == 0 {
            let failure = self.failure();
            return Box::pin(async move { failure });
        }
        Box::pin(async move {
            self.state.note(self.generation, "init-poll");
            if self.generation < 2 && self.state.point == 1 {
                return self.failure();
            }
            tokio::task::yield_now().await;
            self.state.note(self.generation, "init-resumed");
            if self.generation < 2 {
                return self.failure();
            }
            self.state.ready.send_replace(true);
            Ok(())
        })
    }
    fn dispatch(&mut self, _: Query) -> BoxedFuture<'_, ActorResult<Receipt>> {
        self.state.note(self.generation, "message-call");
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.state.note(self.generation, "message-resumed");
            Ok((self.generation, TraceContext::current()))
        })
    }
}
struct LocalLeaf {
    actor: Leaf,
    owner: Rc<()>,
}
impl ParrotMsgVariant<LocalProtocol<LocalLeaf>> for Query {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: Receipt) -> ActorResult<Receipt> {
        Ok(reply)
    }
}
impl LocalTypedDispatch for LocalLeaf {
    type Msg = Query;
    type Reply = Receipt;
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        assert_eq!(Rc::strong_count(&self.owner), 1);
        ParrotTypedDispatch::init(&mut self.actor)
    }
    fn dispatch<'a>(
        &'a mut self,
        query: Query,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<Receipt>> {
        assert_eq!(Rc::strong_count(&self.owner), 1);
        ParrotTypedDispatch::dispatch(&mut self.actor, query)
    }
}
enum Reference {
    Typed(TypedActorRef<Leaf, Query>),
    Local(LocalTypedActorRef<LocalLeaf, Query>),
}
impl Reference {
    fn ask(&self) -> BoxedFuture<'_, ActorResult<Receipt>> {
        match self {
            Self::Typed(actor) => Box::pin(actor.ask(Query)),
            Self::Local(actor) => Box::pin(actor.ask(Query)),
        }
    }
    async fn stop(&self) {
        match self {
            Self::Typed(actor) => actor.stop().await.unwrap(),
            Self::Local(actor) => actor.stop().await.unwrap(),
        }
    }
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
    async fn new(engine: usize) -> Self {
        if engine == 0 {
            Self::Thread(parrot::thread::ThreadActorSystem::shared(
                parrot::thread::ThreadActorSystemConfig {
                    shared_pool_size: 1,
                    shared_burst_workers_max: 0,
                    ..Default::default()
                },
            ))
        } else {
            Self::Actix(Arc::new(
                parrot::actix::ActixActorSystem::with_arbiter_count(1)
                    .await
                    .unwrap(),
            ))
        }
    }
    async fn parent(&self, engine: usize) -> Root {
        match self {
            Self::Thread(system) => {
                Root::Typed(system.spawn_typed(Parent, "/parent").await.unwrap())
            }
            Self::Actix(system) if engine == 1 => {
                Root::Typed(system.spawn_typed(Parent, "/parent").await.unwrap())
            }
            Self::Actix(system) => Root::Local(
                system
                    .spawn_local_typed(|| LocalParent(Rc::new(())), "/parent")
                    .await
                    .unwrap(),
            ),
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
fn policy(state: &Arc<State>) -> OneForOneStrategy {
    let state = state.clone();
    OneForOneStrategy {
        max_restarts: 2,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |_| {
            let epoch = state.policies.fetch_add(1, Ordering::SeqCst);
            state.note(epoch, "policy");
            SupervisionDecision::Restart
        }),
    }
}
async fn create(
    system: &Engine,
    engine: usize,
    context: Option<&TypedContext>,
    state: &Arc<State>,
    origin: Option<TraceContext>,
    ambient: Option<TraceContext>,
) -> Reference {
    let made = state.clone();
    let strategy = policy(state);
    if let Some(context) = context {
        if engine == 2 {
            let work = trace::in_scope(origin, || {
                context.spawn_local_typed_child_factory_with_strategy(
                    move || LocalLeaf {
                        actor: Leaf::new(made.clone()),
                        owner: Rc::new(()),
                    },
                    SupervisorStrategyType::OneForOne(strategy),
                )
            });
            Reference::Local(trace::scope(ambient, work).await.unwrap())
        } else {
            let work = trace::in_scope(origin, || {
                context.spawn_typed_child_factory_with_strategy(
                    move || Leaf::new(made.clone()),
                    SupervisorStrategyType::OneForOne(strategy),
                )
            });
            Reference::Typed(trace::scope(ambient, work).await.unwrap())
        }
    } else {
        match system {
            Engine::Thread(system) => {
                let work = trace::in_scope(origin, || {
                    system.spawn_typed_factory_with_strategy(
                        move || Leaf::new(made.clone()),
                        "/root",
                        strategy,
                    )
                });
                Reference::Typed(trace::scope(ambient, work).await.unwrap())
            }
            Engine::Actix(system) if engine == 1 => {
                let work = trace::in_scope(origin, || {
                    system.spawn_typed_factory_with_strategy(
                        move || Leaf::new(made.clone()),
                        "/root",
                        strategy,
                    )
                });
                Reference::Typed(trace::scope(ambient, work).await.unwrap())
            }
            Engine::Actix(system) => {
                let work = trace::in_scope(origin, || {
                    system.spawn_local_typed_factory_with_strategy(
                        move || LocalLeaf {
                            actor: Leaf::new(made.clone()),
                            owner: Rc::new(()),
                        },
                        "/root",
                        strategy,
                    )
                });
                Reference::Local(trace::scope(ambient, work).await.unwrap())
            }
        }
    }
}
async fn messages(actor: &Reference, ambient: Option<TraceContext>) {
    let work = trace::in_scope(None, || actor.ask());
    let receipt = trace::scope(ambient.clone(), work).await.unwrap();
    assert_eq!(
        receipt.0, 2,
        "original reference reaches the initialized third generation"
    );
    assert!(
        receipt.1.is_none(),
        "initialization cause must not leak into later messages"
    );
    let independent = TraceContext::root();
    let work = independent.in_scope(|| actor.ask());
    let receipt = trace::scope(ambient, work).await.unwrap();
    assert_eq!(receipt.0, 2);
    let observed = receipt.1.unwrap();
    assert_eq!(observed.trace_id(), independent.trace_id());
    assert_ne!(observed.span_id(), independent.span_id());
}
fn verify(
    state: &State,
    origin: Option<TraceContext>,
    engine: usize,
) -> (Vec<Option<TraceContext>>, Vec<Option<TraceContext>>) {
    assert!(TraceContext::current().is_none());
    assert_eq!(state.made.load(Ordering::SeqCst), 3);
    assert_eq!(state.dropped.load(Ordering::SeqCst), 3);
    assert_eq!(state.policies.load(Ordering::SeqCst), 2);
    let events = state.events.lock().unwrap();
    let phase = |generation, phase| {
        let found: Vec<_> = events
            .iter()
            .filter(|event| event.0 == generation && event.1 == phase)
            .collect();
        assert_eq!(found.len(), 1, "each actual lifecycle phase occurs once");
        found[0].2.clone()
    };
    assert_eq!(
        phase(0, "constructor"),
        origin,
        "original creation identity"
    );
    assert_eq!(
        phase(0, "init-call"),
        origin,
        "original initialization identity"
    );
    if engine == 2 {
        let owner = events
            .iter()
            .find(|event| event.1 == "constructor")
            .unwrap()
            .3;
        assert!(
            events
                .iter()
                .filter(|event| event.1 != "policy")
                .all(|event| event.3 == owner),
            "Local initialization, handlers and destruction stay on the owner"
        );
    }
    let mut actual = Vec::new();
    for generation in 0..2 {
        actual.push(phase(generation, "failure"));
        actual.push(phase(generation, "policy"));
        actual.push(phase(generation + 1, "constructor"));
        actual.push(phase(generation + 1, "init-call"));
        if state.point != 0 || generation == 1 {
            actual.push(phase(generation + 1, "init-poll"));
        }
        if state.point == 2 || generation == 1 {
            actual.push(phase(generation + 1, "init-resumed"));
        }
    }
    let expected = vec![origin; actual.len()];
    (actual, expected)
}
async fn scenario(
    engine: usize,
    child: bool,
    point: usize,
    panic: bool,
    traced: bool,
    poll_traced: bool,
) -> (Vec<Option<TraceContext>>, Vec<Option<TraceContext>>) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let state = State::new(point, panic);
        let system = Engine::new(engine).await;
        let parent = if child { Some(system.parent(engine).await) } else { None };
        let context = if let Some(parent) = &parent { Some(parent.context().await) } else { None };
        let origin = traced.then(TraceContext::root);
        let ambient = poll_traced.then(TraceContext::root);
        let actor = create(&system, engine, context.as_ref(), &state, origin.clone(), ambient.clone()).await;
        state.initialized().await;
        messages(&actor, ambient).await;
        if let Some(parent) = parent { parent.stop().await; assert!(context.as_ref().unwrap().children().is_empty()); }
        else { actor.stop().await; }
        assert_eq!(state.dropped.load(Ordering::SeqCst), 3, "stop joins actual initialization generations before system shutdown");
        system.shutdown().await;
        let result = verify(&state, origin, engine);
        println!("PARROT_TYPED_INIT_FAILURE_RETIRED engine={engine} child={child} point={point} panic={panic} traced={traced} poll_traced={poll_traced}");
        result
    }).await.expect("typed initialization causal independent liveness bound")
}
async fn matrix(engine: usize, child: bool, point: usize, panic: bool) {
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for traced in [false, true] {
        for poll_traced in [false, true] {
            let (observed, required) =
                scenario(engine, child, point, panic, traced, poll_traced).await;
            actual.extend(observed);
            expected.extend(required);
        }
    }
    assert_eq!(
        actual, expected,
        "typed initialization failure policy and replacements must retain the creation cause"
    );
}
macro_rules! case {
    ($name:ident, $engine:literal, $child:literal, $point:literal, $panic:literal) => {
        #[actix::test]
        async fn $name() {
            matrix($engine, $child, $point, $panic).await;
        }
    };
}
case!(thread_root_call_error, 0, false, 0, false);
case!(thread_root_call_panic, 0, false, 0, true);
case!(thread_root_poll_error, 0, false, 1, false);
case!(thread_root_poll_panic, 0, false, 1, true);
case!(thread_root_resume_error, 0, false, 2, false);
case!(thread_root_resume_panic, 0, false, 2, true);
case!(thread_child_call_error, 0, true, 0, false);
case!(thread_child_call_panic, 0, true, 0, true);
case!(thread_child_poll_error, 0, true, 1, false);
case!(thread_child_poll_panic, 0, true, 1, true);
case!(thread_child_resume_error, 0, true, 2, false);
case!(thread_child_resume_panic, 0, true, 2, true);
case!(actix_root_call_error, 1, false, 0, false);
case!(actix_root_call_panic, 1, false, 0, true);
case!(actix_root_poll_error, 1, false, 1, false);
case!(actix_root_poll_panic, 1, false, 1, true);
case!(actix_root_resume_error, 1, false, 2, false);
case!(actix_root_resume_panic, 1, false, 2, true);
case!(actix_child_call_error, 1, true, 0, false);
case!(actix_child_call_panic, 1, true, 0, true);
case!(actix_child_poll_error, 1, true, 1, false);
case!(actix_child_poll_panic, 1, true, 1, true);
case!(actix_child_resume_error, 1, true, 2, false);
case!(actix_child_resume_panic, 1, true, 2, true);
case!(local_root_call_error, 2, false, 0, false);
case!(local_root_call_panic, 2, false, 0, true);
case!(local_root_poll_error, 2, false, 1, false);
case!(local_root_poll_panic, 2, false, 1, true);
case!(local_root_resume_error, 2, false, 2, false);
case!(local_root_resume_panic, 2, false, 2, true);
case!(local_child_call_error, 2, true, 0, false);
case!(local_child_call_panic, 2, true, 0, true);
case!(local_child_poll_error, 2, true, 1, false);
case!(local_child_poll_panic, 2, true, 1, true);
case!(local_child_resume_error, 2, true, 2, false);
case!(local_child_resume_panic, 2, true, 2, true);
