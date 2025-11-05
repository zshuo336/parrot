//! Administrative commands must create, conflict with and stop actual Thread actors.
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Value(u64);
impl parrot_api::Message for Value {
    type Result = Self;
}
struct Echo;
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(msg) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
// Inventory factories are process-global. Each test registers only its unique
// requested path with a weak engine reference; no fixture owns another's actors.
fn engines() -> &'static Mutex<HashMap<String, Weak<ThreadActorSystem>>> {
    static ENGINES: OnceLock<Mutex<HashMap<String, Weak<ThreadActorSystem>>>> = OnceLock::new();
    ENGINES.get_or_init(Default::default)
}
fn spawn_echo(path: &str) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    let engine = engines().lock().unwrap().get(path).and_then(Weak::upgrade);
    let path = path.to_owned();
    Box::pin(async move {
        let engine = engine.ok_or_else(|| {
            parrot_api::errors::ActorError::InternalError("test engine missing".into())
        })?;
        let actor = engine
            .spawn_at(Echo, &path, None, Default::default())
            .await
            .map_err(|e| parrot_api::errors::ActorError::InternalError(e.to_string()))?;
        Ok(Box::new(actor) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! {
    parrot_remote::admin::PropsFactory { name: "test.real.echo", spawn: spawn_echo }
}
struct Lookup(Arc<ThreadActorSystem>);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        self.0.get_actor_ref(path).map(|actor| actor.clone_boxed())
    }
}
struct Fixture {
    client: Arc<RemoteActorSystem>,
    server: Arc<RemoteActorSystem>,
    engine: Arc<ThreadActorSystem>,
    path: String,
    target: String,
}
impl Fixture {
    async fn new(name: &str) -> Self {
        let engine = ThreadActorSystem::shared(Default::default());
        let path = format!("/user/{name}");
        let target = format!("admin-{name}-server");
        engines()
            .lock()
            .unwrap()
            .insert(path.clone(), Arc::downgrade(&engine));
        let server =
            RemoteActorSystem::new(RemoteConfig::mem(&target), Arc::new(Lookup(engine.clone())))
                .unwrap();
        let client = RemoteActorSystem::new(
            RemoteConfig::mem(format!("admin-{name}-client")),
            Arc::new(Lookup(engine.clone())),
        )
        .unwrap();
        server.start().await.unwrap();
        client.start().await.unwrap();
        client.connect_mem_pair(&server).await.unwrap();
        Self {
            client,
            server,
            engine,
            path,
            target,
        }
    }
    async fn spawn(&self) -> Result<parrot_remote::RemoteActorRef, parrot_remote::RemoteError> {
        self.client
            .spawn_named(&self.target, &self.path, "test.real.echo")
            .await
    }
    async fn close(&self) {
        self.client.shutdown().await.unwrap();
        self.server.shutdown().await.unwrap();
        self.engine.shutdown_internal().await.unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        engines().lock().unwrap().remove(&self.path);
    }
}

#[tokio::test]
async fn spawn_named_e2e() {
    let fixture = Fixture::new("roundtrip").await;
    let actor = fixture.spawn().await.unwrap();
    assert_eq!(fixture.engine.actor_count(), 1);
    let result = actor.send(Box::new(Value(42))).await.unwrap();
    assert_eq!(result.downcast_ref::<Value>().unwrap().0, 42);
    fixture.close().await;
}
#[tokio::test]
async fn spawn_named_unknown_props_has_no_side_effects() {
    let fixture = Fixture::new("unknown").await;
    let error = fixture
        .client
        .spawn_named(&fixture.target, &fixture.path, "unregistered")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not registered"));
    assert_eq!(fixture.engine.actor_count(), 0);
    fixture.close().await;
}
#[tokio::test]
async fn admin_stop_with_receipt_closes_the_actual_actor_mailbox() {
    let fixture = Fixture::new("stop").await;
    let actor = fixture.spawn().await.unwrap();
    actor.send(Box::new(Value(1))).await.unwrap();
    fixture
        .client
        .admin_stop(&fixture.target, &fixture.path)
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), actor.send(Box::new(Value(2))))
        .await
        .unwrap();
    assert!(result.is_err(), "stopped actor still executed a request");
    fixture.close().await;
}
#[tokio::test]
async fn concurrent_spawn_path_conflict_preserves_one_live_actor() {
    let fixture = Fixture::new("conflict").await;
    let (first, second) = tokio::join!(fixture.spawn(), fixture.spawn());
    assert_ne!(first.is_ok(), second.is_ok(), "exactly one spawn must win");
    assert_eq!(fixture.engine.actor_count(), 1);
    let actor = first.or(second).unwrap();
    let reply = actor.send(Box::new(Value(7))).await.unwrap();
    assert_eq!(reply.downcast_ref::<Value>().unwrap().0, 7);
    fixture.close().await;
}
#[tokio::test]
async fn in_process_admin_is_authorized() {
    let fixture = Fixture::new("inprocess").await;
    let sender = fixture.client.links_snapshot().await[0].1.clone();
    assert!(parrot_remote::admin::admin_allowed(
        &parrot_remote::ControlPolicy::LocalOnly,
        &sender
    ));
    fixture.spawn().await.unwrap();
    assert_eq!(fixture.engine.actor_count(), 1);
    fixture.close().await;
}
#[test]
fn claimed_admin_name_is_not_a_credential() {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let peer = parrot_remote::FrameSender::new(tx, "operator.test");
    let policy = parrot_remote::ControlPolicy::AuthenticatedNodes(["operator.test".into()].into());
    assert!(!parrot_remote::admin::admin_allowed(&policy, &peer));
    assert!(!parrot_remote::admin::admin_allowed(
        &parrot_remote::ControlPolicy::LocalOnly,
        &peer
    ));
}
