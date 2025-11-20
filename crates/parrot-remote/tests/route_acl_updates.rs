//! Public policy updates must affect a live relay after alias resolution.
use parrot_api::{
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{acl::RouteAcl, LocalLookup, RemoteActorSystem, RemoteConfig};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Value(u64);
impl parrot_api::Message for Value {
    type Result = Self;
}

#[derive(Clone, Debug)]
struct Echo(Arc<AtomicUsize>);
impl ActorRef for Echo {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(message)
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("this probe expects request/reply delivery")
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/user/echo".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct Lookup {
    alias: Option<String>,
    echo: Option<Echo>,
}
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        self.echo
            .as_ref()
            .filter(|_| path == "/user/echo")
            .map(|echo| echo.clone_boxed())
    }
    fn forwarding_target(&self, path: &str) -> Option<String> {
        (path == "/user/alias")
            .then(|| self.alias.clone())
            .flatten()
    }
}

fn node(
    name: &str,
    hub: bool,
    alias: Option<String>,
    echo: Option<Echo>,
) -> Arc<RemoteActorSystem> {
    let mut config = RemoteConfig::mem(name);
    config.heartbeat_enabled = false;
    if hub {
        config.topology_role = parrot_remote::handshake::TopologyRole::Hub;
    }
    RemoteActorSystem::new(config, Arc::new(Lookup { alias, echo })).unwrap()
}

async fn ask(reference: &dyn ActorRef, value: u64, allowed: bool) {
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        reference.send(Box::new(Value(value))),
    )
    .await
    .expect("policy decisions must produce a receipt, not a timeout");
    if allowed {
        assert_eq!(result.unwrap().downcast_ref::<Value>().unwrap().0, value);
    } else {
        match result {
            Err(ActorError::MessageHandlingError(detail)) => {
                assert!(detail.starts_with("forbidden:"), "{detail}")
            }
            _ => panic!("denied relay must return the explicit Forbidden mapping"),
        }
    }
}

#[tokio::test]
async fn public_route_acl_updates_preserve_rejections_and_reach_live_alias_relays() {
    let calls = Arc::new(AtomicUsize::new(0));
    let target = node("acl-live-target", false, None, Some(Echo(calls.clone())));
    let hub = node(
        "acl-live-hub",
        true,
        Some("parrot://acl-live-target/user/echo".into()),
        None,
    );
    let client = node("acl-live-client", false, None, None);
    hub.set_route_acl(RouteAcl::default(), "source").unwrap();
    for system in [&target, &hub, &client] {
        system.start().await.unwrap();
    }
    hub.connect_mem_pair(&target).await.unwrap();
    client.connect_mem_pair(&hub).await.unwrap();
    let reference = client
        .remote_ref("parrot://acl-live-hub/user/alias")
        .unwrap();
    ask(&reference, 1, false).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let allow =
        RouteAcl::from_json(r#"{"routes":{"parrot://acl-live-target/user/":["source"]}}"#).unwrap();
    assert!(hub.set_route_acl(allow.clone(), "").is_err());
    ask(&reference, 2, false).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    hub.set_route_acl(allow, "source").unwrap();
    ask(&reference, 3, true).await;
    assert!(hub
        .set_route_acl(RouteAcl::default(), "bad//principal")
        .is_err());
    ask(&reference, 4, true).await;

    let mut invalid = RouteAcl::default();
    invalid
        .routes
        .insert("parrot://acl-live-target/../".into(), vec!["*".into()]);
    assert!(hub.set_route_acl(invalid, "another-source").is_err());
    ask(&reference, 5, true).await;
    hub.set_route_acl(RouteAcl::default(), "source").unwrap();
    ask(&reference, 60, false).await;
    for value in [6, 7] {
        hub.clear_route_acl().unwrap();
        ask(&reference, value, true).await;
        hub.set_route_acl(RouteAcl::default(), "source").unwrap();
        ask(&reference, value + 100, false).await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    hub.set_route_acl(RouteAcl::default(), "source").unwrap();
    ask(&reference, 8, false).await;
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    assert!(hub.ingress.relay_table.is_empty());
    for system in [&client, &hub, &target] {
        system.shutdown().await.unwrap();
    }
}
