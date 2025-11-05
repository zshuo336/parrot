use parrot::typed_context::TypedContextExt;
use parrot_api::{
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalTypedActorRef, LocalTypedDispatch},
    message::Message,
    supervisor::{BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType},
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

type Event = (usize, usize, &'static str, Option<TraceContext>, ThreadId);
struct State {
    made: [AtomicUsize; 2],
    dropped: [AtomicUsize; 2],
    policies: AtomicUsize,
    events: Mutex<Vec<Event>>,
    ready: tokio::sync::watch::Sender<[Option<usize>; 2]>,
}
impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            made: [AtomicUsize::new(0), AtomicUsize::new(0)],
            dropped: [AtomicUsize::new(0), AtomicUsize::new(0)],
            policies: AtomicUsize::new(0),
            events: Mutex::default(),
            ready: tokio::sync::watch::channel([None, None]).0,
        })
    }
    fn note(&self, member: usize, generation: usize, phase: &'static str) {
        self.events.lock().unwrap().push((
            member,
            generation,
            phase,
            TraceContext::current(),
            std::thread::current().id(),
        ));
    }
    async fn wait(&self, generation: usize) {
        let mut ready = self.ready.subscribe();
        loop {
            let observed = *ready.borrow_and_update();
            if observed == [Some(generation); 2] {
                return;
            }
            assert!(
                observed
                    .iter()
                    .all(|item| item.is_none_or(|item| item <= generation))
            );
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
    member: usize,
    generation: usize,
}
impl Leaf {
    fn new(state: Arc<State>, member: usize) -> Self {
        let generation = state.made[member].fetch_add(1, Ordering::SeqCst);
        state.note(member, generation, "constructor");
        Self {
            state,
            member,
            generation,
        }
    }
}
impl Drop for Leaf {
    fn drop(&mut self) {
        self.state.dropped[self.member].fetch_add(1, Ordering::SeqCst);
        self.state.note(self.member, self.generation, "drop");
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
        self.state.note(self.member, self.generation, "init-call");
        Box::pin(async move {
            self.state.note(self.member, self.generation, "init-poll");
            tokio::task::yield_now().await;
            self.state
                .note(self.member, self.generation, "init-resumed");
            self.state
                .ready
                .send_modify(|ready| ready[self.member] = Some(self.generation));
            Ok(())
        })
    }
    fn dispatch(&mut self, request: Request) -> BoxedFuture<'_, ActorResult<Receipt>> {
        if request.0.is_some() {
            self.state
                .note(self.member, self.generation, "failure-call");
        }
        Box::pin(async move {
            tokio::task::yield_now().await;
            if let Some(panic) = request.0 {
                self.state
                    .note(self.member, self.generation, "failure-resumed");
                assert!(!panic, "deliberate typed cohort causal probe panic");
                return Err(ActorError::MessageHandlingError(
                    "deliberate typed cohort causal probe error".into(),
                ));
            }
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
enum Child {
    Typed(TypedActorRef<Leaf, Request>),
    Local(LocalTypedActorRef<LocalLeaf, Request>),
}
impl Child {
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
                    shared_pool_size: 2,
                    shared_burst_workers_max: 0,
                    ..Default::default()
                },
            );
            let root = system.spawn_typed(Parent, "/parent").await.unwrap();
            (Self::Thread(system), Root::Typed(root))
        } else {
            let system = Arc::new(
                parrot::actix::ActixActorSystem::with_arbiter_count(1)
                    .await
                    .unwrap(),
            );
            let root = if engine == 1 {
                Root::Typed(system.spawn_typed(Parent, "/parent").await.unwrap())
            } else {
                Root::Local(
                    system
                        .spawn_local_typed(|| LocalParent(Rc::new(())), "/parent")
                        .await
                        .unwrap(),
                )
            };
            (Self::Actix(system), root)
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

fn verify(state: &State, origins: &[TraceContext], causes: &[Option<TraceContext>], engine: usize) {
    assert!(TraceContext::current().is_none());
    assert_eq!(state.policies.load(Ordering::SeqCst), 2);
    for member in 0..2 {
        assert_eq!(state.made[member].load(Ordering::SeqCst), 3);
        assert_eq!(state.dropped[member].load(Ordering::SeqCst), 3);
    }
    let events = state.events.lock().unwrap();
    let phase = |member, generation, phase| {
        let found: Vec<_> = events
            .iter()
            .filter(|event| event.0 == member && event.1 == generation && event.2 == phase)
            .collect();
        assert_eq!(found.len(), 1, "each actual lifecycle phase occurs once");
        found[0].3.clone()
    };
    for (member, origin) in origins.iter().enumerate() {
        for phase_name in ["constructor", "init-call", "init-poll", "init-resumed"] {
            assert_eq!(
                phase(member, 0, phase_name).as_ref(),
                Some(origin),
                "initial typed child creation identity"
            );
        }
    }
    if engine == 2 {
        for member in 0..2 {
            let owner = events
                .iter()
                .find(|event| event.0 == member && event.2 == "constructor")
                .unwrap()
                .4;
            assert!(
                events
                    .iter()
                    .filter(|event| event.0 == member && event.2 != "policy")
                    .all(|event| event.4 == owner),
                "Local constructors, initialization, handlers and destruction stay on the owner"
            );
        }
    }
    let mut actual = Vec::new();
    let mut expected = Vec::new();
    for (epoch, cause) in causes.iter().enumerate() {
        let failure = phase(epoch, epoch, "failure-resumed");
        assert_eq!(phase(epoch, epoch, "failure-call"), failure);
        match (cause, &failure) {
            (None, None) => {}
            (Some(parent), Some(child)) => {
                assert_eq!(parent.trace_id(), child.trace_id());
                assert_ne!(parent.span_id(), child.span_id());
            }
            _ => panic!("typed failing handler ancestry differs"),
        }
        assert_eq!(
            phase(epoch, epoch, "policy"),
            failure,
            "typed cohort policy sees its actual cause"
        );
        for member in 0..2 {
            for phase_name in ["constructor", "init-call", "init-poll", "init-resumed"] {
                let observed = phase(member, epoch + 1, phase_name);
                if cause.is_none() {
                    assert!(
                        observed.is_none(),
                        "untraced typed cohort must erase earlier identities"
                    );
                }
                actual.push((epoch, member, phase_name, observed));
                expected.push((epoch, member, phase_name, failure.clone()));
            }
        }
        if cause.is_none() {
            println!("PARROT_TYPED_COHORT_UNTRACED_CONTROL_OK epoch={epoch}");
        }
    }
    assert_eq!(
        actual, expected,
        "typed cohort construction and initialization must retain the triggering failure identity"
    );
}
async fn scenario(engine: usize, tell: bool, panic: bool, traced: bool, poll_traced: bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let state = State::new();
        let (system, parent) = Engine::setup(engine).await;
        let context = parent.context().await;
        let policy = state.clone();
        context.set_child_strategy(SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 2, within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(move |_| {
                let epoch = policy.policies.fetch_add(1, Ordering::SeqCst);
                policy.note(epoch, epoch, "policy");
                SupervisionDecision::Restart
            }),
        }));
        let ambient = poll_traced.then(TraceContext::root);
        let mut children = Vec::new();
        let mut origins = Vec::new();
        for member in 0..2 {
            let origin = TraceContext::root();
            let made = state.clone();
            let child = if engine == 2 {
                let work = origin.in_scope(|| context.spawn_local_typed_child_factory(move || LocalLeaf {
                    actor: Leaf::new(made.clone(), member), owner: Rc::new(())
                }));
                Child::Local(trace::scope(Some(origin.clone()), work).await.unwrap())
            } else {
                let work = origin.in_scope(|| context.spawn_typed_child_factory(move || Leaf::new(made.clone(), member)));
                Child::Typed(trace::scope(Some(origin.clone()), work).await.unwrap())
            };
            origins.push(origin);
            children.push(child);
        }
        state.wait(0).await;
        let mut causes = Vec::new();
        for epoch in 0..2 {
            let cause = (if epoch == 0 { traced } else { !traced }).then(TraceContext::root);
            if tell {
                let work = trace::in_scope(cause.clone(), || children[epoch].tell(Request(Some(panic))));
                trace::scope(ambient.clone(), work).await.unwrap();
            } else {
                let work = trace::in_scope(cause.clone(), || children[epoch].ask(Request(Some(panic))));
                assert!(trace::scope(ambient.clone(), work).await.is_err());
            }
            state.wait(epoch + 1).await;
            for sibling in &children {
                let work = trace::in_scope(None, || sibling.ask(Request(None)));
                let receipt = trace::scope(ambient.clone(), work).await.unwrap();
                assert_eq!(receipt.0, epoch + 1, "stable typed refs reach the replacement generation");
                assert!(receipt.1.is_none(), "cohort cause must not leak into later typed messages");
                let independent = TraceContext::root();
                let work = independent.in_scope(|| sibling.ask(Request(None)));
                let receipt = trace::scope(ambient.clone(), work).await.unwrap();
                assert_eq!(receipt.0, epoch + 1);
                let observed = receipt.1.as_ref().unwrap();
                assert_eq!(observed.trace_id(), independent.trace_id());
                assert_ne!(observed.span_id(), independent.span_id());
            }
            causes.push(cause);
        }
        parent.stop().await;
        system.shutdown().await;
        verify(&state, &origins, &causes, engine);
        println!("PARROT_TYPED_COHORT_CAUSAL_CASE_OK engine={engine} tell={tell} panic={panic} traced={traced} poll_traced={poll_traced}");
    }).await.expect("typed cohort independent liveness bound");
}
async fn matrix(engine: usize, tell: bool, panic: bool) {
    for traced in [false, true] {
        for poll_traced in [false, true] {
            scenario(engine, tell, panic, traced, poll_traced).await;
        }
    }
}
macro_rules! case {
    ($name:ident, $engine:literal, $tell:literal, $panic:literal) => {
        #[actix::test]
        async fn $name() {
            matrix($engine, $tell, $panic).await;
        }
    };
}
case!(thread_ask_error, 0, false, false);
case!(thread_ask_panic, 0, false, true);
case!(thread_tell_error, 0, true, false);
case!(thread_tell_panic, 0, true, true);
case!(actix_ask_error, 1, false, false);
case!(actix_ask_panic, 1, false, true);
case!(actix_tell_error, 1, true, false);
case!(actix_tell_panic, 1, true, true);
case!(local_ask_error, 2, false, false);
case!(local_ask_panic, 2, false, true);
case!(local_tell_error, 2, true, false);
case!(local_tell_panic, 2, true, true);
