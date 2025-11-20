//! Typed and non-Send handlers resolve through their real generation and engine.
use parrot::{
    actix::ActixActorSystem,
    system::{ParrotActorSystem, RemoteGateway},
    thread::ThreadActorSystem,
};
use parrot_api::{
    engine::{AsyncLocalRuntime, FactoryOptions, LocalActor, SpawnOptions},
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalTypedActorRef, LocalTypedDispatch},
    message::Message,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::TypedActorRef,
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[path = "support/network.rs"]
mod network_support;

// This fallback lets the same contracts compile against the preceding public
// API. Native resolution must satisfy the assertions, rather than a new symbol
// causing a baseline compiler failure. Inherent methods take precedence.
#[allow(dead_code)]
trait PreviousResolution {
    fn resolve_actor(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
impl PreviousResolution for TypedContext {}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Ping(u64);
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Pong {
    sequence: u64,
    receiver: String,
    count: usize,
}
impl Message for Ping {
    type Result = Pong;
}
macro_rules! codec {
    ($ty:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key, type_id: std::any::TypeId::of::<$ty>(),
                encode: |message: &BoxedMessage| parrot_api::message::serde_remote_serialize(
                    message.downcast_ref::<$ty>().ok_or($key)?),
                decode: |bytes: &[u8]| {
                    let value: $ty = parrot_api::message::serde_remote_deserialize(bytes)?;
                    Ok(Box::new(value) as BoxedMessage)
                },
            }
        }
    };
}
codec!(Ping, "bin:typed-resolution::Ping#v1");
codec!(Pong, "bin:typed-resolution::Pong#v1");
struct Peer(Arc<Mutex<Vec<u64>>>);
impl ParrotTypedDispatch for Peer {
    type Msg = Ping;
    type Reply = Pong;
    fn dispatch(&mut self, _: Ping) -> BoxedFuture<'_, ActorResult<Pong>> {
        Box::pin(async { panic!("managed dispatch required") })
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        message: Ping,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<Pong>> {
        Box::pin(async move {
            let mut received = self.0.lock().unwrap();
            received.push(message.0);
            Ok(Pong {
                sequence: message.0,
                receiver: context.path().into(),
                count: received.len(),
            })
        })
    }
}
impl ParrotMsgVariant<Peer> for Ping {
    fn inject(value: Self) -> Ping {
        value
    }
    fn extract(value: Pong) -> ActorResult<Pong> {
        Ok(value)
    }
}
enum Op {
    Context,
    Call(String, u64),
    Fail,
}
enum Reply {
    Context(TypedContext),
    Called(Option<Pong>),
}
impl Message for Op {
    type Result = Reply;
}
async fn operate(context: &TypedContext, op: Op) -> ActorResult<Reply> {
    match op {
        Op::Context => Ok(Reply::Context(context.clone())),
        Op::Call(path, sequence) => {
            let Some(target) = context.resolve_actor(&path) else {
                return Ok(Reply::Called(None));
            };
            let value = target.send(Box::new(Ping(sequence))).await?;
            Ok(Reply::Called(Some(
                *value.downcast::<Pong>().expect("actual typed peer reply"),
            )))
        }
        Op::Fail => Err(ActorError::MessageHandlingError(
            "replace probe generation".into(),
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
    local: Rc<Cell<usize>>,
    owner: std::thread::ThreadId,
    drops: Arc<AtomicUsize>,
}
impl LocalProbe {
    fn new(drops: Arc<AtomicUsize>) -> Self {
        Self {
            local: Rc::new(Cell::new(0)),
            owner: std::thread::current().id(),
            drops,
        }
    }
    async fn handle(&mut self, op: Op, ctx: &TypedContext) -> ActorResult<Reply> {
        assert_eq!(self.owner, std::thread::current().id());
        let local = self.local.clone();
        local.set(local.get() + 1);
        let result = operate(ctx, op).await;
        assert_eq!(self.owner, std::thread::current().id());
        assert!(Rc::ptr_eq(&self.local, &local));
        result
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
            let op = *message.downcast::<Op>().expect("local probe operation");
            Ok(Box::new(self.handle(op, ctx).await?) as BoxedMessage)
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
            Self::Neutral(actor) => Ok(*actor
                .send(Box::new(op))
                .await?
                .downcast::<Reply>()
                .expect("local reply")),
        }
    }
    async fn context(&self) -> TypedContext {
        let Reply::Context(context) = self.ask(Op::Context).await.unwrap() else {
            panic!("context reply")
        };
        context
    }
    async fn call(&self, path: &str, sequence: u64) -> Option<Pong> {
        let Reply::Called(reply) = self.ask(Op::Call(path.into(), sequence)).await.unwrap() else {
            panic!("business reply")
        };
        reply
    }
    async fn stop(&self) {
        match self {
            Self::Typed(actor) => actor.stop().await.unwrap(),
            Self::Local(actor) => actor.stop().await.unwrap(),
            Self::Neutral(actor) => actor.stop().await.unwrap(),
        }
    }
}
struct Lookup(Weak<ThreadActorSystem>);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        self.0
            .upgrade()?
            .get_actor_ref(path)
            .map(|actor| actor.clone_boxed())
    }
}
struct Gateway(parrot_remote::system::RemoteGatewayImpl);
impl RemoteGateway for Gateway {
    fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        self.0.lookup(path)
    }
}
struct Missing;
impl RemoteGateway for Missing {
    fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
fn policy() -> OneForOneStrategy {
    OneForOneStrategy {
        max_restarts: 2,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    }
}
async fn contract(kind: usize, factory: bool) {
    let server_engine = ThreadActorSystem::shared(Default::default());
    let remote_received = Arc::new(Mutex::new(Vec::new()));
    let service = server_engine
        .spawn_typed::<_, Ping>(Peer(remote_received.clone()), "/user/remote")
        .await
        .unwrap();
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("typed-server", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Lookup(Arc::downgrade(&server_engine))),
    )
    .unwrap();
    server.start().await.unwrap();
    let remote = RemoteActorSystem::new(
        RemoteConfig::tcp("typed-client", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Lookup(Weak::new())),
    )
    .unwrap();
    remote.start().await.unwrap();
    remote
        .connect(&NodeAddr {
            node_id: "typed-server".into(),
            identity_is_explicit: true,
            scheme: "tcp".into(),
            addr: server.local_addr().unwrap(),
        })
        .await
        .unwrap();
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let local_received = Arc::new(Mutex::new(Vec::new()));
    let (client, local) = if kind == 0 {
        let engine = ThreadActorSystem::shared(Default::default());
        let local = engine
            .spawn_typed::<_, Ping>(Peer(local_received.clone()), "/user/local")
            .await
            .unwrap()
            .into_dyn();
        let actor = if factory {
            let drops = drops.clone();
            engine
                .spawn_typed_factory_with_strategy::<_, Op, _>(
                    move || Probe(drops.clone()),
                    "/user/probe",
                    policy(),
                )
                .await
                .unwrap()
        } else {
            engine
                .spawn_typed::<_, Op>(Probe(drops.clone()), "/user/probe")
                .await
                .unwrap()
        };
        facade
            .register_runtime("engine".into(), engine, true)
            .unwrap();
        (Client::Typed(actor), local)
    } else {
        let engine = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
        let local = engine
            .spawn_typed::<_, Ping>(Peer(local_received.clone()), "/user/local")
            .await
            .unwrap()
            .into_dyn();
        let actor = match kind {
            1 => {
                let actor = if factory {
                    let drops = drops.clone();
                    engine
                        .spawn_typed_factory_with_strategy::<_, Op, _>(
                            move || Probe(drops.clone()),
                            "/user/probe",
                            policy(),
                        )
                        .await
                        .unwrap()
                } else {
                    engine
                        .spawn_typed::<_, Op>(Probe(drops.clone()), "/user/probe")
                        .await
                        .unwrap()
                };
                Client::Typed(actor)
            }
            2 => {
                let made = drops.clone();
                let actor = if factory {
                    engine
                        .spawn_local_typed_factory_with_strategy::<_, Op, _>(
                            move || LocalProbe::new(made.clone()),
                            "/user/probe",
                            policy(),
                        )
                        .await
                        .unwrap()
                } else {
                    engine
                        .spawn_local_typed::<_, Op, _>(
                            move || LocalProbe::new(made.clone()),
                            "/user/probe",
                        )
                        .await
                        .unwrap()
                };
                Client::Local(actor)
            }
            3 => {
                let made = drops.clone();
                let actor = AsyncLocalRuntime::spawn_local_factory(
                    engine.as_ref(),
                    Box::new(move || Box::new(LocalProbe::new(made.clone()))),
                    FactoryOptions {
                        spawn: SpawnOptions::at("/user/probe"),
                        max_restarts: 2,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                Client::Neutral(actor)
            }
            _ => unreachable!(),
        };
        facade
            .register_runtime("engine".into(), engine, true)
            .unwrap();
        (actor, local)
    };
    facade
        .register_remote_gateway(Arc::new(Gateway(remote.gateway())))
        .await
        .unwrap();
    let context = client.context().await;
    assert_eq!(
        context
            .resolve_actor(context.path())
            .expect("typed context must preserve local registry identity")
            .path(),
        context.path()
    );
    assert!(context.resolve_actor("/user/absent").is_none());
    let local_reply = client
        .call("/user/local", 11)
        .await
        .expect("typed handler must resolve a local peer");
    assert_eq!(
        local_reply,
        Pong {
            sequence: 11,
            receiver: local.path(),
            count: 1
        }
    );
    let remote_path = format!("parrot://typed-server{}", service.path());
    let reply = client
        .call(&remote_path, 21)
        .await
        .expect("typed handler must resolve and call the TCP peer");
    assert_eq!(
        reply,
        Pong {
            sequence: 21,
            receiver: service.path().into(),
            count: 1
        }
    );
    facade
        .register_remote_gateway(Arc::new(Missing))
        .await
        .unwrap();
    assert!(
        client.call(&remote_path, 22).await.is_none(),
        "typed context must observe gateway replacement"
    );
    facade
        .register_remote_gateway(Arc::new(Gateway(remote.gateway())))
        .await
        .unwrap();
    let reply = client
        .call(&remote_path, 23)
        .await
        .expect("restored gateway must execute actual business");
    assert_eq!(
        reply,
        Pong {
            sequence: 23,
            receiver: service.path().into(),
            count: 2
        }
    );
    if factory {
        assert!(matches!(
            client.ask(Op::Fail).await,
            Err(ActorError::MessageHandlingError(_))
        ));
        let next = client.context().await;
        assert_eq!(next.path(), context.path());
        assert!(
            context.resolve_actor("/user/local").is_none(),
            "retired generation must not resolve a still-live local peer"
        );
        assert!(
            context.resolve_actor(&remote_path).is_none(),
            "retired generation must not resolve a still-live remote peer"
        );
        assert!(
            next.resolve_actor("/user/local").is_some(),
            "replacement generation must retain engine resolution"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    let current = client.context().await;
    client.stop().await;
    assert_eq!(drops.load(Ordering::SeqCst), if factory { 2 } else { 1 });
    assert!(
        current.resolve_actor("/user/local").is_none(),
        "stopped generation must reject lookup while the engine remains live"
    );
    assert!(
        facade
            .runtime("engine")
            .unwrap()
            .get_actor("/user/local")
            .await
            .is_some()
    );
    assert_eq!(*local_received.lock().unwrap(), [11]);
    assert_eq!(*remote_received.lock().unwrap(), [21, 23]);
    facade.shutdown_runtime().await.unwrap();
    assert!(current.resolve_actor(&remote_path).is_none());
    remote.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    server_engine.shutdown_internal().await.unwrap();
    let weak = Arc::downgrade(&server_engine);
    drop(server_engine);
    drop(server);
    assert!(
        weak.upgrade().is_none(),
        "retained contexts must not retain the peer engine"
    );
}
fn run(kind: usize) {
    if network_support::skip_tcp("typed TCP resolution") {
        return;
    }
    actix::System::new().block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            for factory in [false, true] {
                if kind == 3 && !factory {
                    continue;
                }
                contract(kind, factory).await;
            }
        })
        .await
        .expect("typed resolution contract deadline");
    });
}
#[test]
fn thread_typed_resolution() {
    run(0);
}
#[test]
fn actix_typed_resolution() {
    run(1);
}
#[test]
fn local_typed_resolution() {
    run(2);
}
#[test]
fn neutral_local_resolution() {
    run(3);
}
