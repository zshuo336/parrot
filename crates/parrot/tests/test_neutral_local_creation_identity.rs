use parrot::actix::ActixActorSystem;
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{
        AsyncEngineRuntime, AsyncFactoryRuntime, AsyncLocalRuntime, FactoryOptions, LocalActor,
        LocalRuntimeFactory, SpawnOptions,
    },
    errors::ActorError,
    local_typed::LocalBoxedFuture,
    message::Message,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    trace::{self, TraceContext},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::TypedActorRef,
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
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
    async fn initialized(&self) {
        let mut ready = self.ready.subscribe();
        while !*ready.borrow_and_update() {
            ready.changed().await.unwrap();
        }
    }
}
struct Inspect;
struct Probe {
    state: Arc<State>,
    owner: Rc<()>,
}
fn factory(state: Arc<State>) -> LocalRuntimeFactory {
    Box::new(move || {
        state.made.fetch_add(1, Ordering::SeqCst);
        state.note("constructor");
        Box::new(Probe {
            state: state.clone(),
            owner: Rc::new(()),
        })
    })
}
impl LocalActor for Probe {
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.state.note("init-call");
        Box::pin(async move {
            assert_eq!(Rc::strong_count(&self.owner), 1);
            self.state.note("init-poll");
            tokio::task::yield_now().await;
            self.state.note("init-resumed");
            self.state.ready.send_replace(true);
            Ok(())
        })
    }
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert!(message.is::<Inspect>());
        self.state.note("message-call");
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.state.note("message-resumed");
            assert_eq!(Rc::strong_count(&self.owner), 1);
            Ok(Box::new(TraceContext::current()) as BoxedMessage)
        })
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.state.note("drop");
        self.state.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
enum Cmd {
    Create(
        LocalRuntimeFactory,
        Option<SupervisorStrategyType>,
        Option<TraceContext>,
        Option<TraceContext>,
    ),
    Children,
}
impl Message for Cmd {
    type Result = BoxedMessage;
}
async fn typed_handle(message: Cmd, ctx: &TypedContext) -> ActorResult<BoxedMessage> {
    match message {
        Cmd::Create(factory, policy, origin, ambient) => {
            let spawner = ctx.local_spawner().unwrap();
            let work = trace::in_scope(origin, || spawner.spawn_local_factory(factory, policy));
            Ok(Box::new(trace::scope(ambient, work).await?) as BoxedMessage)
        }
        Cmd::Children => Ok(Box::new(ctx.children().len())),
    }
}
struct TypedParent;
impl ParrotTypedDispatch for TypedParent {
    type Msg = Cmd;
    type Reply = BoxedMessage;
    fn dispatch(&mut self, _: Cmd) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        Box::pin(async { Err(ActorError::InternalError("managed context required".into())) })
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        message: Cmd,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(typed_handle(message, ctx))
    }
}
impl ParrotMsgVariant<TypedParent> for Cmd {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: BoxedMessage) -> ActorResult<BoxedMessage> {
        Ok(reply)
    }
}
struct LocalParent(Rc<()>);
impl LocalActor for LocalParent {
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert_eq!(Rc::strong_count(&self.0), 1);
        Box::pin(typed_handle(*message.downcast::<Cmd>().unwrap(), ctx))
    }
}
struct NativeParent;
impl Actor for NativeParent {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match *message.downcast::<Cmd>().unwrap() {
                Cmd::Create(factory, policy, origin, ambient) => {
                    let spawner = ctx.local_spawner().unwrap();
                    let work =
                        trace::in_scope(origin, || spawner.spawn_local_factory(factory, policy));
                    Ok(Box::new(trace::scope(ambient, work).await?) as BoxedMessage)
                }
                Cmd::Children => Ok(
                    Box::new(ctx.children().map_or(0, |children| children.len())) as BoxedMessage,
                ),
            }
        })
    }
}
struct Parent {
    reference: BoxedActorRef,
    typed: Option<TypedActorRef<TypedParent, Cmd>>,
}
impl Parent {
    async fn ask(&self, message: Cmd) -> BoxedMessage {
        match &self.typed {
            Some(typed) => typed.ask(message).await.unwrap(),
            None => self.reference.send(Box::new(message)).await.unwrap(),
        }
    }
    async fn stop(&self) {
        self.reference.stop().await.unwrap();
    }
}
async fn parent(system: &Arc<ActixActorSystem>, flavor: usize) -> Parent {
    let options = FactoryOptions {
        spawn: SpawnOptions::at("/parent"),
        ..Default::default()
    };
    let mut typed = None;
    let reference = match flavor {
        1 => AsyncEngineRuntime::spawn(
            system.as_ref(),
            Box::new(NativeParent),
            Box::new(()),
            options.spawn,
        )
        .await
        .unwrap(),
        2 => AsyncFactoryRuntime::spawn_factory(
            system.as_ref(),
            Box::new(|| Box::new(NativeParent)),
            options,
        )
        .await
        .unwrap(),
        3 => {
            let actor = system.spawn_typed(TypedParent, "/parent").await.unwrap();
            let reference = actor.clone().into_dyn();
            typed = Some(actor);
            reference
        }
        4 => AsyncLocalRuntime::spawn_local_factory(
            system.as_ref(),
            Box::new(|| Box::new(LocalParent(Rc::new(())))),
            options,
        )
        .await
        .unwrap(),
        _ => unreachable!(),
    };
    Parent { reference, typed }
}
fn policy() -> SupervisorStrategyType {
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    })
}
async fn messages(actor: &BoxedActorRef, ambient: Option<TraceContext>) {
    let work = trace::in_scope(None, || actor.send(Box::new(Inspect)));
    let reply = trace::scope(ambient.clone(), work).await.unwrap();
    assert!(
        reply.downcast::<Option<TraceContext>>().unwrap().is_none(),
        "creation must not leak into later messages"
    );
    let followup = TraceContext::root();
    let work = followup.in_scope(|| actor.send(Box::new(Inspect)));
    let reply = trace::scope(ambient, work).await.unwrap();
    let observed = reply.downcast::<Option<TraceContext>>().unwrap().unwrap();
    assert_eq!(observed.trace_id(), followup.trace_id());
    assert_ne!(observed.span_id(), followup.span_id());
}
async fn scenario(
    flavor: usize,
    explicit: bool,
    traced: bool,
    poll_traced: bool,
) -> (Vec<Option<TraceContext>>, Vec<Option<TraceContext>>) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
        let state = State::new();
        let origin = traced.then(TraceContext::root);
        let ambient = poll_traced.then(TraceContext::root);
        let (actor, parent) = if flavor == 0 {
            let options = if explicit { FactoryOptions { spawn: SpawnOptions::at("/root"), max_restarts: 1, ..Default::default() } } else { FactoryOptions::default() };
            let work = trace::in_scope(origin.clone(), || AsyncLocalRuntime::spawn_local_factory(system.as_ref(), factory(state.clone()), options));
            (trace::scope(ambient.clone(), work).await.unwrap(), None)
        } else {
            let parent = parent(&system, flavor).await;
            let reply = parent.ask(Cmd::Create(factory(state.clone()), explicit.then(policy), origin.clone(), ambient.clone())).await;
            let actor = *reply.downcast::<BoxedActorRef>().unwrap();
            assert_eq!(*parent.ask(Cmd::Children).await.downcast::<usize>().unwrap(), 1);
            (actor, Some(parent))
        };
        state.initialized().await;
        messages(&actor, ambient).await;
        if let Some(parent) = &parent {
            parent.stop().await;
            assert!(!actor.is_alive().await, "parent stop joins child retirement");
        } else { actor.stop().await.unwrap(); }
        assert_eq!(state.dropped.load(Ordering::SeqCst), 1, "stop must join actual Local destruction before system shutdown");
        AsyncEngineRuntime::shutdown(system.as_ref()).await.unwrap();
        assert_eq!(system.status().active_actors, 0);
        assert_eq!(state.made.load(Ordering::SeqCst), 1);
        assert_eq!(state.dropped.load(Ordering::SeqCst), 1);
        assert!(TraceContext::current().is_none());
        let events = state.events.lock().unwrap();
        assert!(events.iter().all(|event| event.2 == events[0].2), "Local construction, lifecycle, messages and destruction stay on their owner");
        let lifecycle: Vec<_> = events.iter().filter(|event| matches!(event.0, "constructor" | "init-call" | "init-poll" | "init-resumed")).collect();
        assert_eq!(lifecycle.iter().map(|event| event.0).collect::<Vec<_>>(), ["constructor", "init-call", "init-poll", "init-resumed"]);
        println!("PARROT_NEUTRAL_LOCAL_CREATION_RETIRED flavor={flavor} explicit={explicit} traced={traced} poll_traced={poll_traced}");
        (lifecycle.iter().map(|event| event.1.clone()).collect(), vec![origin; 4])
    }).await.expect("neutral Local creation independent liveness bound")
}
async fn matrix(flavor: usize, explicit: bool) {
    let mut observed = Vec::new();
    let mut expected = Vec::new();
    for traced in [false, true] {
        for poll_traced in [false, true] {
            let (actual, required) = scenario(flavor, explicit, traced, poll_traced).await;
            observed.extend(actual);
            expected.extend(required);
        }
    }
    assert_eq!(
        observed, expected,
        "neutral Local construction and initialization must retain their call-time identity"
    );
}
macro_rules! case {
    ($name:ident, $flavor:literal, $explicit:literal) => {
        #[actix::test]
        async fn $name() {
            matrix($flavor, $explicit).await;
        }
    };
}
case!(root_default_options, 0, false);
case!(root_explicit_options, 0, true);
case!(native_instance_inherited_policy, 1, false);
case!(native_instance_explicit_policy, 1, true);
case!(native_factory_inherited_policy, 2, false);
case!(native_factory_explicit_policy, 2, true);
case!(typed_parent_inherited_policy, 3, false);
case!(typed_parent_explicit_policy, 3, true);
case!(local_parent_inherited_policy, 4, false);
case!(local_parent_explicit_policy, 4, true);
