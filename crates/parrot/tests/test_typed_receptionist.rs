//! Service bindings belong to actual typed/Local generations, not retained contexts.
use parrot::{actix::ActixActorSystem, system::ParrotActorSystem, thread::ThreadActorSystem};
use parrot_api::{
    engine::{AsyncLocalRuntime, FactoryOptions, LocalActor, SpawnOptions},
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalTypedActorRef, LocalTypedDispatch},
    message::Message,
    receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey, ReceptionistStream},
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::TypedActorRef,
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{acl::AclRules, receptionist::ReceptionistFacade};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

// Keep the baseline buildable: failures must reach a service assertion.
#[allow(dead_code)]
trait PreviousReceptionist {
    fn receptionist_register(&self, _: ReceptionistKey) -> BoxedFuture<'static, ActorResult<()>> {
        Box::pin(async { Err(ActorError::InternalError("receptionist not enabled".into())) })
    }
    fn receptionist_deregister(
        &self,
        _: &ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<()>> {
        Box::pin(async { Err(ActorError::InternalError("receptionist not enabled".into())) })
    }
    fn receptionist_subscribe(
        &self,
        _: ReceptionistKey,
    ) -> BoxedFuture<'static, ActorResult<ReceptionistStream>> {
        Box::pin(async { Err(ActorError::InternalError("receptionist not enabled".into())) })
    }
}
impl PreviousReceptionist for TypedContext {}

fn key() -> ReceptionistKey {
    ReceptionistKey::new("edge/typed-service").unwrap()
}
fn late_key() -> ReceptionistKey {
    ReceptionistKey::new("edge/delayed-service").unwrap()
}
enum Op {
    Context,
    Register,
    Deregister,
    Subscribe,
    Fail,
}
enum Reply {
    Context(TypedContext),
    Done,
    Stream(ReceptionistStream),
}
impl Message for Op {
    type Result = Reply;
}
async fn operate(ctx: &TypedContext, op: Op) -> ActorResult<Reply> {
    match op {
        Op::Context => Ok(Reply::Context(ctx.clone())),
        Op::Register => {
            ctx.receptionist_register(key()).await?;
            Ok(Reply::Done)
        }
        Op::Deregister => {
            ctx.receptionist_deregister(&key()).await?;
            Ok(Reply::Done)
        }
        Op::Subscribe => Ok(Reply::Stream(ctx.receptionist_subscribe(key()).await?)),
        Op::Fail => Err(ActorError::MessageHandlingError(
            "replace publication owner".into(),
        )),
    }
}
struct Probe(Arc<AtomicUsize>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Op;
    type Reply = Reply;
    fn dispatch(&mut self, _: Op) -> BoxedFuture<'_, ActorResult<Reply>> {
        Box::pin(async { panic!("managed dispatch required") })
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        op: Op,
        ctx: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<Reply>> {
        Box::pin(operate(ctx, op))
    }
}
impl ParrotMsgVariant<Probe> for Op {
    fn inject(value: Self) -> Op {
        value
    }
    fn extract(value: Reply) -> ActorResult<Reply> {
        Ok(value)
    }
}
struct LocalProbe {
    state: Rc<Cell<usize>>,
    owner: std::thread::ThreadId,
    drops: Arc<AtomicUsize>,
}
impl LocalProbe {
    fn new(drops: Arc<AtomicUsize>) -> Self {
        Self {
            state: Rc::new(Cell::new(0)),
            owner: std::thread::current().id(),
            drops,
        }
    }
    async fn handle(&mut self, op: Op, ctx: &TypedContext) -> ActorResult<Reply> {
        assert_eq!(self.owner, std::thread::current().id());
        let state = self.state.clone();
        state.set(state.get() + 1);
        let reply = operate(ctx, op).await;
        assert_eq!(self.owner, std::thread::current().id());
        assert!(Rc::ptr_eq(&state, &self.state));
        reply
    }
}
impl Drop for LocalProbe {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl LocalTypedDispatch for LocalProbe {
    type Msg = Op;
    type Reply = Reply;
    fn dispatch<'a>(
        &'a mut self,
        op: Op,
        ctx: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<Reply>> {
        Box::pin(self.handle(op, ctx))
    }
}
impl ParrotMsgVariant<LocalProtocol<LocalProbe>> for Op {
    fn inject(value: Self) -> Op {
        value
    }
    fn extract(value: Reply) -> ActorResult<Reply> {
        Ok(value)
    }
}
impl LocalActor for LocalProbe {
    fn receive<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            Ok(
                Box::new(self.handle(*message.downcast::<Op>().unwrap(), ctx).await?)
                    as BoxedMessage,
            )
        })
    }
}
enum Client {
    Typed(TypedActorRef<Probe, Op>),
    Local(LocalTypedActorRef<LocalProbe, Op>),
    Neutral(BoxedActorRef),
}
impl Client {
    async fn ask(&self, op: Op) -> ActorResult<Reply> {
        match self {
            Self::Typed(actor) => actor.ask(op).await,
            Self::Local(actor) => actor.ask(op).await,
            Self::Neutral(actor) => {
                Ok(*actor.send(Box::new(op)).await?.downcast::<Reply>().unwrap())
            }
        }
    }
    async fn context(&self) -> TypedContext {
        match self.ask(Op::Context).await.unwrap() {
            Reply::Context(ctx) => ctx,
            _ => panic!("context reply"),
        }
    }
    async fn stop(&self) {
        match self {
            Self::Typed(actor) => actor.stop().await.unwrap(),
            Self::Local(actor) => actor.stop().await.unwrap(),
            Self::Neutral(actor) => actor.stop().await.unwrap(),
        }
    }
}
fn policy(resume: bool) -> OneForOneStrategy {
    OneForOneStrategy {
        max_restarts: 2,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |_| {
            if resume {
                SupervisionDecision::Resume
            } else {
                SupervisionDecision::Restart
            }
        }),
    }
}
async fn spawn(
    facade: &ParrotActorSystem,
    kind: usize,
    factory: bool,
    resume: bool,
    drops: Arc<AtomicUsize>,
) -> Client {
    if kind == 0 {
        let system = ThreadActorSystem::shared(Default::default());
        let actor = if factory {
            system
                .spawn_typed_factory_with_strategy::<_, Op, _>(
                    move || Probe(drops.clone()),
                    "/user/service",
                    policy(resume),
                )
                .await
                .unwrap()
        } else {
            system
                .spawn_typed::<_, Op>(Probe(drops), "/user/service")
                .await
                .unwrap()
        };
        facade
            .register_runtime("engine".into(), system, true)
            .unwrap();
        return Client::Typed(actor);
    }
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let actor = match kind {
        1 => Client::Typed(if factory {
            system
                .spawn_typed_factory_with_strategy::<_, Op, _>(
                    move || Probe(drops.clone()),
                    "/user/service",
                    policy(resume),
                )
                .await
                .unwrap()
        } else {
            system
                .spawn_typed::<_, Op>(Probe(drops), "/user/service")
                .await
                .unwrap()
        }),
        2 => Client::Local(if factory {
            system
                .spawn_local_typed_factory_with_strategy::<_, Op, _>(
                    move || LocalProbe::new(drops.clone()),
                    "/user/service",
                    policy(resume),
                )
                .await
                .unwrap()
        } else {
            system
                .spawn_local_typed::<_, Op, _>(
                    move || LocalProbe::new(drops.clone()),
                    "/user/service",
                )
                .await
                .unwrap()
        }),
        3 => Client::Neutral(
            AsyncLocalRuntime::spawn_local_factory(
                system.as_ref(),
                Box::new(move || Box::new(LocalProbe::new(drops.clone()))),
                FactoryOptions {
                    spawn: SpawnOptions::at("/user/service"),
                    max_restarts: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
        ),
        _ => unreachable!(),
    };
    facade
        .register_runtime("engine".into(), system, true)
        .unwrap();
    actor
}
fn gateway() -> Arc<ReceptionistFacade> {
    Arc::new(ReceptionistFacade::new("typed-owner").with_acl(AclRules::permissive()))
}
async fn event(
    stream: &mut ReceptionistStream,
    key: ReceptionistKey,
    path: &str,
    registered: bool,
) {
    let expected = if registered {
        ReceptionistEvent::Registered {
            key,
            remote_path: path.into(),
        }
    } else {
        ReceptionistEvent::Unregistered {
            key,
            remote_path: path.into(),
        }
    };
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), stream.recv())
            .await
            .expect("generation service event deadline"),
        Some(expected)
    );
}
fn empty(stream: &mut ReceptionistStream) {
    assert!(
        matches!(
            stream.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "no duplicate, replayed, or misdirected publication"
    );
}
async fn snapshot(gateway: &ReceptionistFacade, expected: Option<&str>) {
    let mut stream = gateway.subscribe(key()).await.unwrap();
    if let Some(path) = expected {
        event(&mut stream, key(), path, true).await;
    }
    empty(&mut stream);
}
async fn contract(kind: usize, factory: bool, resume: bool) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let first = gateway();
    if factory {
        facade
            .register_receptionist_gateway(first.clone())
            .await
            .unwrap();
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let client = spawn(&facade, kind, factory, resume, drops.clone()).await;
    let context = client.context().await;
    if !factory {
        assert!(
            matches!(context.receptionist_register(key()).await, Err(ActorError::InternalError(message)) if message == "receptionist not enabled")
        );
        facade
            .register_receptionist_gateway(first.clone())
            .await
            .unwrap();
    }
    assert!(
        matches!(client.ask(Op::Register).await, Ok(Reply::Done)),
        "typed handler must publish its actual generation"
    );
    let Reply::Stream(mut old_stream) = client.ask(Op::Subscribe).await.unwrap() else {
        panic!("subscription reply")
    };
    event(&mut old_stream, key(), context.path(), true).await;
    let mut late_stream = first.subscribe(late_key()).await.unwrap();
    let captured_first = context.receptionist_register(late_key());
    let captured_subscribe = context.receptionist_subscribe(key());
    let second = gateway();
    facade
        .register_receptionist_gateway(second.clone())
        .await
        .unwrap();
    captured_first.await.unwrap();
    event(&mut late_stream, late_key(), context.path(), true).await;
    let mut captured_stream = captured_subscribe.await.unwrap();
    event(&mut captured_stream, key(), context.path(), true).await;
    snapshot(&second, None).await;
    assert!(matches!(client.ask(Op::Register).await, Ok(Reply::Done)));
    let Reply::Stream(mut new_stream) = client.ask(Op::Subscribe).await.unwrap() else {
        panic!("new subscription reply")
    };
    event(&mut new_stream, key(), context.path(), true).await;
    client.ask(Op::Deregister).await.unwrap();
    event(&mut new_stream, key(), context.path(), false).await;
    snapshot(&first, Some(context.path())).await;
    client.ask(Op::Register).await.unwrap();
    event(&mut new_stream, key(), context.path(), true).await;
    let delayed_register = context.receptionist_register(key());
    let delayed_remove = context.receptionist_deregister(&key());
    let delayed_subscribe = context.receptionist_subscribe(key());
    if factory {
        assert!(matches!(
            client.ask(Op::Fail).await,
            Err(ActorError::MessageHandlingError(_))
        ));
        let next = client.context().await;
        assert_eq!(next.path(), context.path());
        if resume {
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            snapshot(&first, Some(context.path())).await;
            snapshot(&second, Some(context.path())).await;
            empty(&mut old_stream);
            empty(&mut new_stream);
        } else {
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            event(&mut old_stream, key(), context.path(), false).await;
            event(&mut new_stream, key(), context.path(), false).await;
            event(&mut late_stream, late_key(), context.path(), false).await;
            snapshot(&first, None).await;
            snapshot(&second, None).await;
            client
                .ask(Op::Register)
                .await
                .expect("replacement needs a new publication scope");
            event(&mut new_stream, key(), next.path(), true).await;
            assert!(
                matches!(delayed_register.await, Err(ActorError::Stopped)),
                "old unpolled registration must reject retired generation"
            );
            assert!(
                matches!(delayed_remove.await, Err(ActorError::Stopped)),
                "old removal must not erase replacement publication"
            );
            assert!(matches!(delayed_subscribe.await, Err(ActorError::Stopped)));
            snapshot(&second, Some(next.path())).await;
        }
    }
    let current = client.context().await;
    let pending = current.receptionist_register(late_key());
    client.stop().await;
    assert_eq!(
        drops.load(Ordering::SeqCst),
        if factory && !resume { 2 } else { 1 }
    );
    event(&mut new_stream, key(), current.path(), false).await;
    if !factory || resume {
        event(&mut old_stream, key(), current.path(), false).await;
        event(&mut late_stream, late_key(), current.path(), false).await;
    }
    assert!(
        matches!(pending.await, Err(ActorError::Stopped)),
        "stopped context must fence calls created before stop"
    );
    assert!(matches!(
        current.receptionist_register(key()).await,
        Err(ActorError::Stopped)
    ));
    assert!(matches!(
        current.receptionist_deregister(&key()).await,
        Err(ActorError::Stopped)
    ));
    assert!(matches!(
        current.receptionist_subscribe(key()).await,
        Err(ActorError::Stopped)
    ));
    snapshot(&first, None).await;
    snapshot(&second, None).await;
    empty(&mut new_stream);
    facade.shutdown_runtime().await.unwrap();
}
fn run(kind: usize) {
    actix::System::new().block_on(async {
        tokio::time::timeout(Duration::from_secs(45), async {
            if kind != 3 {
                contract(kind, false, false).await;
            }
            contract(kind, true, false).await;
            if kind != 3 {
                contract(kind, true, true).await;
            }
        })
        .await
        .expect("typed receptionist contract deadline");
    });
}
#[test]
fn thread_typed_receptionist() {
    run(0);
}
#[test]
fn actix_typed_receptionist() {
    run(1);
}
#[test]
fn local_typed_receptionist() {
    run(2);
}
#[test]
fn neutral_local_receptionist() {
    run(3);
}

struct DynamicProbe(Arc<AtomicUsize>);
impl Drop for DynamicProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl parrot_api::actor::ActixEngineExt for DynamicProbe {}
impl parrot_api::actor::Actor for DynamicProbe {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        use parrot_api::context::ActorContext;
        Box::pin(async move {
            let reply = match *message.downcast::<Op>().unwrap() {
                Op::Register => {
                    context.receptionist_register(key()).await?;
                    Reply::Done
                }
                Op::Deregister => {
                    context.receptionist_deregister(&key()).await?;
                    Reply::Done
                }
                Op::Subscribe => Reply::Stream(context.receptionist_subscribe(key()).await?),
                _ => panic!("unexpected host-exit operation"),
            };
            Ok(Box::new(reply) as BoxedMessage)
        })
    }
}

fn host_exit_contract(kind: usize, published: bool) {
    for (factory, system_shutdown) in [(false, false), (false, true), (true, false), (true, true)] {
        let observer = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let facade = observer
            .block_on(ParrotActorSystem::new(Default::default()))
            .unwrap();
        let host = actix::System::new();
        let (gateway, client, drops, mut events, path) = host.block_on(async {
            let gateway = gateway();
            facade
                .register_receptionist_gateway(gateway.clone())
                .await
                .unwrap();
            let drops = Arc::new(AtomicUsize::new(0));
            let client = if kind < 4 {
                spawn(&facade, kind, factory, false, drops.clone()).await
            } else {
                let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
                let actor = if factory {
                    let drops = drops.clone();
                    system
                        .spawn_root_factory(move || DynamicProbe(drops.clone()))
                        .await
                        .unwrap()
                } else {
                    system
                        .spawn_root_typed(
                            DynamicProbe(drops.clone()),
                            parrot_api::actor::EmptyConfig,
                        )
                        .await
                        .unwrap()
                };
                facade
                    .register_runtime("engine".into(), system, true)
                    .unwrap();
                Client::Neutral(actor)
            };
            let path = match &client {
                Client::Neutral(actor) => actor.path(),
                _ => "/user/service".into(),
            };
            let mut events = gateway.subscribe(key()).await.unwrap();
            let Reply::Stream(mut ready) = client.ask(Op::Subscribe).await.unwrap() else {
                panic!("generation is serving before host exit");
            };
            empty(&mut ready);
            if published {
                assert!(matches!(client.ask(Op::Register).await, Ok(Reply::Done)));
                event(&mut events, key(), &path, true).await;
            }
            (gateway, client, drops, events, path)
        });
        drop(host);
        observer.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                assert_eq!(
                    drops.load(Ordering::SeqCst),
                    0,
                    "business actor survives host"
                );
                snapshot(&gateway, published.then_some(path.as_str())).await;
                empty(&mut events);
                if !published {
                    let reply = client
                        .ask(Op::Register)
                        .await
                        .expect("live generation can first publish after host exit");
                    assert!(matches!(reply, Reply::Done));
                    event(&mut events, key(), &path, true).await;
                }
                let Reply::Stream(mut subscribed) = client.ask(Op::Subscribe).await.unwrap() else {
                    panic!("actual actor subscription");
                };
                event(&mut subscribed, key(), &path, true).await;
                client.ask(Op::Deregister).await.unwrap();
                event(&mut events, key(), &path, false).await;
                event(&mut subscribed, key(), &path, false).await;
                snapshot(&gateway, None).await;
                client.ask(Op::Register).await.unwrap();
                event(&mut events, key(), &path, true).await;
                event(&mut subscribed, key(), &path, true).await;
                if system_shutdown {
                    facade.shutdown_runtime().await.unwrap();
                }
                client.stop().await;
                event(&mut events, key(), &path, false).await;
                event(&mut subscribed, key(), &path, false).await;
                snapshot(&gateway, None).await;
                empty(&mut events);
                empty(&mut subscribed);
                assert_eq!(
                    drops.load(Ordering::SeqCst),
                    1,
                    "actual generation retired once"
                );
                client.stop().await;
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                facade.shutdown_runtime().await.unwrap();
            })
            .await
            .expect("host-exit registration lifecycle deadline");
        });
    }
}

macro_rules! host_case {
    ($name:ident, $kind:expr, $published:expr) => {
        #[test]
        fn $name() {
            host_exit_contract($kind, $published);
        }
    };
}
host_case!(actix_typed_registration_survives_host, 1, true);
host_case!(actix_typed_first_registration_after_host, 1, false);
host_case!(local_typed_registration_survives_host, 2, true);
host_case!(local_typed_first_registration_after_host, 2, false);
host_case!(neutral_local_registration_survives_host, 3, true);
host_case!(neutral_local_first_registration_after_host, 3, false);
host_case!(actix_dynamic_registration_survives_host, 4, true);
host_case!(actix_dynamic_first_registration_after_host, 4, false);

struct BlockingGateway {
    started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}
#[async_trait::async_trait]
impl ReceptionistGateway for BlockingGateway {
    async fn register_scoped(
        &self,
        key: ReceptionistKey,
        path: String,
        scope: Arc<parrot_api::receptionist::RegistrationScope>,
    ) -> ActorResult<()> {
        let mut started = self.started.lock().unwrap().take();
        let release = self.release.lock().unwrap().take().unwrap();
        scope.publish(
            (1, key, path),
            move || {
                started.take().unwrap().send(()).unwrap();
                release.recv().unwrap();
                Ok(())
            },
            || (),
        )
    }
    async fn register(&self, _: ReceptionistKey, _: String) -> ActorResult<()> {
        panic!("managed publication must use scoped gateway")
    }
    async fn deregister(&self, _: ReceptionistKey, _: String) -> ActorResult<()> {
        panic!("managed removal must use scoped gateway")
    }
    async fn subscribe(&self, _: ReceptionistKey) -> ActorResult<ReceptionistStream> {
        panic!("receipt contract does not subscribe")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_stop_joins_actual_service_cleanup() {
    use futures::FutureExt;
    tokio::time::timeout(Duration::from_secs(10), async {
        let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
        let (started, arrived) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        facade
            .register_receptionist_gateway(Arc::new(BlockingGateway {
                started: std::sync::Mutex::new(Some(started)),
                release: std::sync::Mutex::new(Some(wait)),
            }))
            .await
            .unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let client = spawn(&facade, 0, false, false, drops.clone()).await;
        assert!(
            matches!(client.ask(Op::Register).await, Ok(Reply::Done)),
            "typed handler must publish its actual generation"
        );
        let mut stopped = Box::pin(client.stop());
        tokio::select! {
            _ = &mut stopped => panic!("typed stop completed before registration cleanup"),
            result = arrived => result.unwrap(),
        }
        let pending = stopped.as_mut().now_or_never().is_none();
        // Always unblock the worker before asserting to preserve a failing run.
        release.send(()).unwrap();
        assert!(pending, "typed stop must join actual service cleanup");
        stopped.await;
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        facade.shutdown_runtime().await.unwrap();
    })
    .await
    .expect("typed cleanup receipt deadline");
}
