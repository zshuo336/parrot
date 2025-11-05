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
    ready: tokio::sync::watch::Sender<Option<usize>>,
}
impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            made: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            policies: AtomicUsize::new(0),
            events: Mutex::default(),
            ready: tokio::sync::watch::channel(None).0,
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
    async fn initialized(&self, generation: usize) {
        let mut ready = self.ready.subscribe();
        loop {
            let observed = *ready.borrow_and_update();
            if observed == Some(generation) {
                return;
            }
            assert!(observed.is_none_or(|observed| observed <= generation));
            ready.changed().await.unwrap();
        }
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
struct Request(Option<bool>);
type Receipt = (usize, Option<TraceContext>);
impl Message for Request {
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
            "replacement must follow actual retirement"
        );
        state.note(generation, "constructor");
        Self { state, generation }
    }
}
impl Drop for Leaf {
    fn drop(&mut self) {
        self.state.note(self.generation, "drop");
        self.state.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl ParrotMsgVariant<Leaf> for Request {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: Receipt) -> ActorResult<Receipt> {
        Ok(reply)
    }
}
impl ParrotTypedDispatch for Leaf {
    type Msg = Request;
    type Reply = Receipt;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.state.note(self.generation, "init-call");
        Box::pin(async move {
            self.state.note(self.generation, "init-poll");
            tokio::task::yield_now().await;
            self.state.note(self.generation, "init-resumed");
            self.state.ready.send_replace(Some(self.generation));
            Ok(())
        })
    }
    fn dispatch(&mut self, request: Request) -> BoxedFuture<'_, ActorResult<Receipt>> {
        self.state.note(
            self.generation,
            if request.0.is_some() {
                "failure-call"
            } else {
                "message-call"
            },
        );
        Box::pin(async move {
            tokio::task::yield_now().await;
            if let Some(panic) = request.0 {
                self.state.note(self.generation, "failure-resumed");
                assert!(!panic, "deliberate typed restart causal probe panic");
                return Err(ActorError::MessageHandlingError(
                    "deliberate typed restart causal probe error".into(),
                ));
            }
            self.state.note(self.generation, "message-resumed");
            Ok((self.generation, TraceContext::current()))
        })
    }
}
struct LocalLeaf {
    actor: Leaf,
    owner: Rc<()>,
}
impl ParrotMsgVariant<LocalProtocol<LocalLeaf>> for Request {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: Receipt) -> ActorResult<Receipt> {
        Ok(reply)
    }
}
impl LocalTypedDispatch for LocalLeaf {
    type Msg = Request;
    type Reply = Receipt;
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        assert_eq!(Rc::strong_count(&self.owner), 1);
        ParrotTypedDispatch::init(&mut self.actor)
    }
    fn dispatch<'a>(
        &'a mut self,
        request: Request,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<Receipt>> {
        assert_eq!(Rc::strong_count(&self.owner), 1);
        ParrotTypedDispatch::dispatch(&mut self.actor, request)
    }
}
enum Reference {
    Typed(TypedActorRef<Leaf, Request>),
    Local(LocalTypedActorRef<LocalLeaf, Request>),
}
impl Reference {
    fn ask(&self, request: Request) -> BoxedFuture<'_, ActorResult<Receipt>> {
        match self {
            Self::Typed(actor) => Box::pin(actor.ask(request)),
            Self::Local(actor) => Box::pin(actor.ask(request)),
        }
    }
    fn tell(&self, request: Request) -> BoxedFuture<'_, ActorResult<()>> {
        match self {
            Self::Typed(actor) => Box::pin(actor.tell(request)),
            Self::Local(actor) => Box::pin(actor.tell(request)),
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
    origin: &TraceContext,
    ambient: Option<TraceContext>,
) -> Reference {
    let made = state.clone();
    let strategy = policy(state);
    if let Some(context) = context {
        if engine == 2 {
            let work = origin.in_scope(|| {
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
            let work = origin.in_scope(|| {
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
                let work = origin.in_scope(|| {
                    system.spawn_typed_factory_with_strategy(
                        move || Leaf::new(made.clone()),
                        "/root",
                        strategy,
                    )
                });
                Reference::Typed(trace::scope(ambient, work).await.unwrap())
            }
            Engine::Actix(system) if engine == 1 => {
                let work = origin.in_scope(|| {
                    system.spawn_typed_factory_with_strategy(
                        move || Leaf::new(made.clone()),
                        "/root",
                        strategy,
                    )
                });
                Reference::Typed(trace::scope(ambient, work).await.unwrap())
            }
            Engine::Actix(system) => {
                let work = origin.in_scope(|| {
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
async fn messages(actor: &Reference, generation: usize, ambient: Option<TraceContext>) {
    let work = trace::in_scope(None, || actor.ask(Request(None)));
    let receipt = trace::scope(ambient.clone(), work).await.unwrap();
    assert_eq!(
        receipt.0, generation,
        "stable references reach the replacement generation"
    );
    assert!(
        receipt.1.is_none(),
        "restart cause must not leak into later messages"
    );
    let independent = TraceContext::root();
    let work = independent.in_scope(|| actor.ask(Request(None)));
    let receipt = trace::scope(ambient, work).await.unwrap();
    assert_eq!(receipt.0, generation);
    let observed = receipt.1.unwrap();
    assert_eq!(observed.trace_id(), independent.trace_id());
    assert_ne!(observed.span_id(), independent.span_id());
}
fn verify(
    state: &State,
    origin: &TraceContext,
    causes: &[Option<TraceContext>],
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
    for phase_name in ["constructor", "init-call", "init-poll", "init-resumed"] {
        assert_eq!(
            phase(0, phase_name).as_ref(),
            Some(origin),
            "initial creation identity"
        );
    }
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
            "Local lifecycle, handlers and destruction stay on the owner"
        );
    }
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for (epoch, cause) in causes.iter().enumerate() {
        let failure = phase(epoch, "failure-resumed");
        assert_eq!(phase(epoch, "failure-call"), failure);
        match (cause, &failure) {
            (None, None) => {}
            (Some(parent), Some(child)) => {
                assert_eq!(parent.trace_id(), child.trace_id());
                assert_ne!(parent.span_id(), child.span_id());
            }
            _ => panic!("typed failing handler ancestry differs"),
        }
        assert_eq!(
            phase(epoch, "policy"),
            failure,
            "OneForOne policy sees its accepted failing handler"
        );
        for phase_name in ["constructor", "init-call", "init-poll", "init-resumed"] {
            actual.push(phase(epoch + 1, phase_name));
            expected.push(failure.clone());
        }
    }
    (actual, expected)
}
async fn scenario(
    engine: usize,
    child: bool,
    tell: bool,
    panic: bool,
    traced: bool,
    poll_traced: bool,
) -> (Vec<Option<TraceContext>>, Vec<Option<TraceContext>>) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let state = State::new();
        let system = Engine::new(engine).await;
        let parent = if child { Some(system.parent(engine).await) } else { None };
        let context = if let Some(parent) = &parent { Some(parent.context().await) } else { None };
        let origin = TraceContext::root();
        let ambient = poll_traced.then(TraceContext::root);
        let actor = create(&system, engine, context.as_ref(), &state, &origin, ambient.clone()).await;
        state.initialized(0).await;
        let mut causes = Vec::new();
        for epoch in 0..2 {
            let cause = (if epoch == 0 { traced } else { !traced }).then(TraceContext::root);
            if tell {
                let work = trace::in_scope(cause.clone(), || actor.tell(Request(Some(panic))));
                trace::scope(ambient.clone(), work).await.unwrap();
            } else {
                let work = trace::in_scope(cause.clone(), || actor.ask(Request(Some(panic))));
                assert!(trace::scope(ambient.clone(), work).await.is_err());
            }
            state.initialized(epoch + 1).await;
            messages(&actor, epoch + 1, ambient.clone()).await;
            causes.push(cause);
        }
        if let Some(parent) = parent { parent.stop().await; assert!(context.as_ref().unwrap().children().is_empty()); }
        else { actor.stop().await; }
        assert_eq!(state.dropped.load(Ordering::SeqCst), 3, "stop joins final Local/typed destruction before system shutdown");
        system.shutdown().await;
        let result = verify(&state, &origin, &causes, engine);
        println!("PARROT_TYPED_RESTART_RETIRED engine={engine} child={child} tell={tell} panic={panic} traced={traced} poll_traced={poll_traced}");
        result
    }).await.expect("typed restart independent liveness bound")
}
async fn matrix(engine: usize, child: bool, tell: bool, panic: bool) {
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for traced in [false, true] {
        for poll_traced in [false, true] {
            let (observed, required) =
                scenario(engine, child, tell, panic, traced, poll_traced).await;
            actual.extend(observed);
            expected.extend(required);
        }
    }
    assert_eq!(
        actual, expected,
        "typed OneForOne construction and initialization must retain the triggering failure identity"
    );
}
macro_rules! case {
    ($name:ident, $engine:literal, $child:literal, $tell:literal, $panic:literal) => {
        #[actix::test]
        async fn $name() {
            matrix($engine, $child, $tell, $panic).await;
        }
    };
}
case!(thread_root_ask_error, 0, false, false, false);
case!(thread_root_ask_panic, 0, false, false, true);
case!(thread_root_tell_error, 0, false, true, false);
case!(thread_root_tell_panic, 0, false, true, true);
case!(actix_root_ask_error, 1, false, false, false);
case!(actix_root_ask_panic, 1, false, false, true);
case!(actix_root_tell_error, 1, false, true, false);
case!(actix_root_tell_panic, 1, false, true, true);
case!(local_root_ask_error, 2, false, false, false);
case!(local_root_ask_panic, 2, false, false, true);
case!(local_root_tell_error, 2, false, true, false);
case!(local_root_tell_panic, 2, false, true, true);
case!(thread_child_ask_error, 0, true, false, false);
case!(thread_child_ask_panic, 0, true, false, true);
case!(thread_child_tell_error, 0, true, true, false);
case!(thread_child_tell_panic, 0, true, true, true);
case!(actix_child_ask_error, 1, true, false, false);
case!(actix_child_ask_panic, 1, true, false, true);
case!(actix_child_tell_error, 1, true, true, false);
case!(actix_child_tell_panic, 1, true, true, true);
case!(local_child_ask_error, 2, true, false, false);
case!(local_child_ask_panic, 2, true, false, true);
case!(local_child_tell_error, 2, true, true, false);
case!(local_child_tell_panic, 2, true, true, true);
