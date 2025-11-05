//! Exercise public assembly/wiring against a real actor. Construction and polling
//! deliberately occur under different recorders, including an explicit None.
#![cfg(feature = "host")]
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    trace::{self, TraceContext, TraceRecorder},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
    host::{ThreadPropsFactory, ThreadPropsSpawner},
    manifest::{AppManifest, ComponentSpec},
    planner::{plan, LocalTopology},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Seen {
    handled: Mutex<Vec<Option<TraceContext>>>,
    retired: Mutex<Vec<Option<TraceContext>>>,
}
struct Probe(Arc<Seen>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.retired.lock().unwrap().push(TraceContext::current());
    }
}
struct Observe;
impl Actor for Observe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let input = message.downcast::<Probe>().unwrap();
            input
                .0
                .handled
                .lock()
                .unwrap()
                .push(TraceContext::current());
            drop(input);
            Ok(Box::new(42_u64) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
fn spawn(
    engine: Arc<ThreadActorSystem>,
    path: String,
    _: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let actor = engine
            .spawn_at(Observe, &path, None, Default::default())
            .await
            .map_err(|e| ActorError::InternalError(e.to_string()))?;
        Ok(Box::new(actor) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! {
    ThreadPropsFactory { name: "app.trace.observe", spawn }
}
struct NoGateway;
#[async_trait::async_trait]
impl GatewayFactory for NoGateway {
    async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        Ok(())
    }
    async fn start_gateway(
        &self,
        _: &ComponentSpec,
        _: &[String],
        _: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("unexpected gateway")
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        panic!("unexpected gateway")
    }
}
async fn assemble() -> (Arc<ThreadActorSystem>, AssemblingContext) {
    let manifest = AppManifest::from_toml(
        r#"
name = "wire-trace"
version = "1.0.0"
[[components]]
name = "sink"
engine = "parrot"
artifact = { Props = { factory = "app.trace.observe" } }
[[wiring]]
from = "sink:/user/input"
to = "sink:/user/sink"
qos = "lan"
"#,
    )
    .unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut context = AssemblingContext::new();
    context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &Default::default(),
        )
        .await
        .unwrap();
    (engine, context)
}
async fn teardown(engine: &Arc<ThreadActorSystem>, context: &mut AssemblingContext) {
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    context.teardown(&deployer).await.unwrap();
    engine.shutdown_internal().await.unwrap();
}
fn dispatch(
    actor: &dyn ActorRef,
    mode: u8,
    seen: Arc<Seen>,
) -> BoxedFuture<'_, ActorResult<Option<BoxedMessage>>> {
    let input = Box::new(Probe(seen)) as BoxedMessage;
    match mode {
        0 => {
            let f = actor.send(input);
            Box::pin(async move { f.await.map(Some) })
        }
        1 => {
            let f = actor.send_with_timeout(input, Some(Duration::from_secs(3)));
            Box::pin(async move { f.await.map(Some) })
        }
        2 => {
            let f = actor.send_with_options(input, Default::default());
            Box::pin(async move { f.await.map(Some) })
        }
        3 => {
            let f = actor.deliver(input);
            Box::pin(async move { f.await.map(|()| None) })
        }
        4 => {
            let f = actor.deliver_with_options(input, Default::default());
            Box::pin(async move { f.await.map(|()| None) })
        }
        _ => unreachable!(),
    }
}
async fn delivery(mode: u8, traced: bool, wired: bool) {
    let (engine, mut context) = assemble().await;
    let actor = if wired {
        context.wired_ref("sink:/user/input").unwrap()
    } else {
        context.component_ref("sink").unwrap().clone_boxed()
    };
    let seen = Arc::new(Seen::default());
    let original = TraceRecorder::new(16).unwrap();
    let unrelated = TraceRecorder::new(16).unwrap();
    let parent = traced.then(|| original.root());
    let f = trace::in_scope(parent.clone(), || {
        dispatch(actor.as_ref(), mode, seen.clone())
    });
    assert!(seen.handled.lock().unwrap().is_empty());
    let reply = unrelated.root().scope(f).await.unwrap();
    if mode < 3 {
        assert_eq!(*reply.unwrap().downcast::<u64>().unwrap(), 42);
    } else {
        assert!(reply.is_none());
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while seen.retired.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    teardown(&engine, &mut context).await;
    let actual = seen.handled.lock().unwrap();
    assert_eq!(actual.len(), 1);
    match (&parent, &actual[0]) {
        (Some(parent), Some(child)) => {
            assert_eq!(
                child.trace_id(),
                parent.trace_id(),
                "wire adopted polling trace"
            );
            assert_eq!(child.parent_span_id(), Some(parent.span_id()));
            assert_ne!(child.span_id(), parent.span_id());
        }
        (None, None) => {}
        _ => panic!("wire must preserve explicit None: {:?}", actual[0]),
    }
    assert_eq!(seen.retired.lock().unwrap().len(), 1);
    assert_eq!(original.snapshot().records.len(), usize::from(traced));
    assert!(unrelated.snapshot().records.is_empty());
    assert!(TraceContext::current().is_none());
}
async fn retirement(mode: u8, traced: bool, closed: bool) {
    let (engine, mut context) = assemble().await;
    let wire = context.wired_ref("sink:/user/input").unwrap();
    let seen = Arc::new(Seen::default());
    let original = TraceRecorder::new(16).unwrap();
    let unrelated = TraceRecorder::new(16).unwrap();
    let parent = traced.then(|| original.root());
    let f = trace::in_scope(parent.clone(), || {
        dispatch(wire.as_ref(), mode, seen.clone())
    });
    if closed {
        teardown(&engine, &mut context).await;
        assert!(matches!(
            unrelated.root().scope(f).await,
            Err(ActorError::Stopped)
        ));
    } else {
        trace::in_scope(Some(unrelated.root()), || drop(f));
        teardown(&engine, &mut context).await;
    }
    assert!(seen.handled.lock().unwrap().is_empty());
    assert_eq!(
        *seen.retired.lock().unwrap(),
        vec![parent],
        "message cleanup borrowed another caller's scope"
    );
    assert!(original.snapshot().records.is_empty());
    assert!(unrelated.snapshot().records.is_empty());
    assert!(TraceContext::current().is_none());
}
macro_rules! delivered {
    ($name:ident, $mode:expr, $traced:expr, $wired:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            delivery($mode, $traced, $wired).await;
        }
    };
}
macro_rules! retired {
    ($name:ident, $mode:expr, $traced:expr, $closed:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            retirement($mode, $traced, $closed).await;
        }
    };
}
delivered!(wire_ask_parent, 0, true, true);
delivered!(wire_timeout_parent, 1, true, true);
delivered!(wire_options_ask_parent, 2, true, true);
delivered!(wire_tell_parent, 3, true, true);
delivered!(wire_options_tell_parent, 4, true, true);
delivered!(wire_ask_none, 0, false, true);
delivered!(wire_timeout_none, 1, false, true);
delivered!(wire_options_ask_none, 2, false, true);
delivered!(wire_tell_none, 3, false, true);
delivered!(wire_options_tell_none, 4, false, true);
delivered!(direct_ask_parent, 0, true, false);
delivered!(direct_timeout_parent, 1, true, false);
delivered!(direct_options_ask_parent, 2, true, false);
delivered!(direct_tell_parent, 3, true, false);
delivered!(direct_options_tell_parent, 4, true, false);
delivered!(direct_ask_none, 0, false, false);
delivered!(direct_timeout_none, 1, false, false);
delivered!(direct_options_ask_none, 2, false, false);
delivered!(direct_tell_none, 3, false, false);
delivered!(direct_options_tell_none, 4, false, false);
retired!(unpolled_ask_parent, 0, true, false);
retired!(unpolled_timeout_parent, 1, true, false);
retired!(unpolled_options_ask_parent, 2, true, false);
retired!(unpolled_tell_parent, 3, true, false);
retired!(unpolled_options_tell_parent, 4, true, false);
retired!(unpolled_ask_none, 0, false, false);
retired!(unpolled_timeout_none, 1, false, false);
retired!(unpolled_options_ask_none, 2, false, false);
retired!(unpolled_tell_none, 3, false, false);
retired!(unpolled_options_tell_none, 4, false, false);
retired!(closed_ask_parent, 0, true, true);
retired!(closed_timeout_parent, 1, true, true);
retired!(closed_options_ask_parent, 2, true, true);
retired!(closed_tell_parent, 3, true, true);
retired!(closed_options_tell_parent, 4, true, true);
retired!(closed_ask_none, 0, false, true);
retired!(closed_timeout_none, 1, false, true);
retired!(closed_options_ask_none, 2, false, true);
retired!(closed_tell_none, 3, false, true);
retired!(closed_options_tell_none, 4, false, true);

// Rollback retains both a public RetainedActor and a host-owned CleanupRef.
// Exercise each surface independently, then the normal two-wrapper path.
#[derive(Debug)]
struct RejectFirstStop {
    actor: BoxedActorRef,
    stops: Arc<std::sync::atomic::AtomicUsize>,
}
impl ActorRef for RejectFirstStop {
    fn send<'a>(&'a self, m: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send(m)
    }
    fn send_with_timeout<'a>(
        &'a self,
        m: BoxedMessage,
        t: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send_with_timeout(m, t)
    }
    fn send_with_options<'a>(
        &'a self,
        m: BoxedMessage,
        o: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send_with_options(m, o)
    }
    fn deliver<'a>(&'a self, m: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.actor.deliver(m)
    }
    fn deliver_with_options<'a>(
        &'a self,
        m: BoxedMessage,
        o: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        self.actor.deliver_with_options(m, o)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Err(ActorError::InternalError(
                    "injected rollback refusal".into(),
                ));
            }
            self.actor.stop().await
        })
    }
    fn path(&self) -> String {
        self.actor.path()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.actor.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            actor: self.actor.clone_boxed(),
            stops: self.stops.clone(),
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn spawn_retained(
    engine: Arc<ThreadActorSystem>,
    path: String,
    config: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        if path.ends_with("-1") {
            return Err(ActorError::InternalError(
                "injected second instance failure".into(),
            ));
        }
        let actor = spawn(engine, path, config).await?;
        Ok(Box::new(RejectFirstStop {
            actor,
            stops: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! { ThreadPropsFactory { name: "app.trace.retained", spawn: spawn_retained } }
async fn cleanup_context(mode: u8, traced: bool, layer: u8, unpolled: bool) {
    use parrot_app::{assemble::ParrotSpawner, cleanup::RetainedActor};
    let engine = ThreadActorSystem::shared(Default::default());
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let error = spawner
        .spawn_props(
            "app.trace.retained",
            &["/cleanup-0".into(), "/cleanup-1".into()],
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.retained_actors().len(), 1);
    let mut pending = spawner.pending_cleanup();
    assert_eq!(pending.len(), 1);
    let retained = pending.pop().unwrap();
    let actor: Arc<dyn ActorRef> = match layer {
        0 => Arc::new(RetainedActor {
            path: retained.path.clone(),
            error: retained.error.clone(),
            actor: engine.get_actor_ref("/cleanup-0").unwrap(),
        }),
        1 => retained.actor.clone(),
        2 => Arc::new(retained),
        _ => unreachable!(),
    };
    let original = TraceRecorder::new(16).unwrap();
    let unrelated = TraceRecorder::new(16).unwrap();
    let parent = traced.then(|| original.root());
    let seen = Arc::new(Seen::default());
    let f = trace::in_scope(parent.clone(), || {
        dispatch(actor.as_ref(), mode, seen.clone())
    });
    if unpolled {
        trace::in_scope(Some(unrelated.root()), || drop(f));
    } else {
        let reply = unrelated.root().scope(f).await.unwrap();
        if mode == 2 {
            assert_eq!(*reply.unwrap().downcast::<u64>().unwrap(), 42);
        } else {
            assert!(reply.is_none());
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while seen.retired.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    spawner.retry_pending_cleanup().await.unwrap();
    assert!(spawner.pending_cleanup().is_empty());
    assert!(engine.get_actor_ref("/cleanup-0").is_none());
    engine.shutdown_internal().await.unwrap();
    let actual = seen.handled.lock().unwrap();
    assert_eq!(actual.len(), usize::from(!unpolled));
    if unpolled {
        assert_eq!(*seen.retired.lock().unwrap(), vec![parent]);
    } else {
        match (&parent, &actual[0]) {
            (Some(parent), Some(child)) => {
                assert_eq!(
                    child.trace_id(),
                    parent.trace_id(),
                    "cleanup reference adopted polling trace"
                );
                assert_eq!(child.parent_span_id(), Some(parent.span_id()));
                assert_ne!(child.span_id(), parent.span_id());
            }
            (None, None) => {}
            _ => panic!(
                "cleanup reference must preserve explicit None: {:?}",
                actual[0]
            ),
        }
        assert_eq!(seen.retired.lock().unwrap().len(), 1);
    }
    assert_eq!(
        original.snapshot().records.len(),
        usize::from(traced && !unpolled)
    );
    assert!(unrelated.snapshot().records.is_empty());
    assert!(TraceContext::current().is_none());
}
macro_rules! cleanup {
    ($name:ident, $mode:expr, $traced:expr, $layer:expr, $unpolled:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            cleanup_context($mode, $traced, $layer, $unpolled).await;
        }
    };
}
cleanup!(retained_ask_parent, 2, true, 0, false);
cleanup!(retained_tell_parent, 4, true, 0, false);
cleanup!(retained_ask_none, 2, false, 0, false);
cleanup!(retained_tell_none, 4, false, 0, false);
cleanup!(retained_unpolled_ask_parent, 2, true, 0, true);
cleanup!(retained_unpolled_tell_parent, 4, true, 0, true);
cleanup!(retained_unpolled_ask_none, 2, false, 0, true);
cleanup!(retained_unpolled_tell_none, 4, false, 0, true);
cleanup!(host_cleanup_ask_parent, 2, true, 1, false);
cleanup!(host_cleanup_tell_parent, 4, true, 1, false);
cleanup!(host_cleanup_ask_none, 2, false, 1, false);
cleanup!(host_cleanup_tell_none, 4, false, 1, false);
cleanup!(host_cleanup_unpolled_ask_parent, 2, true, 1, true);
cleanup!(host_cleanup_unpolled_tell_parent, 4, true, 1, true);
cleanup!(host_cleanup_unpolled_ask_none, 2, false, 1, true);
cleanup!(host_cleanup_unpolled_tell_none, 4, false, 1, true);
cleanup!(rollback_ask_parent, 2, true, 2, false);
cleanup!(rollback_tell_parent, 4, true, 2, false);
cleanup!(rollback_ask_none, 2, false, 2, false);
cleanup!(rollback_tell_none, 4, false, 2, false);
cleanup!(rollback_unpolled_ask_parent, 2, true, 2, true);
cleanup!(rollback_unpolled_tell_parent, 4, true, 2, true);
cleanup!(rollback_unpolled_ask_none, 2, false, 2, true);
cleanup!(rollback_unpolled_tell_none, 4, false, 2, true);
