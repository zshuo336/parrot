//! Discovery must use the existing sparse graph before any business dial occurs.
use parrot_api::{
    address::ActorRef,
    receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{acl::AclRules, LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{sync::Arc, time::Duration};

const LIMIT: Duration = Duration::from_secs(5);

#[path = "receptionist_sparse/wire.rs"]
mod wire;

#[derive(Debug, serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Query(u64);
impl parrot_api::Message for Query {
    type Result = Self;
}

#[derive(Debug, Clone)]
struct Echo;
impl ActorRef for Echo {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
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
#[async_trait::async_trait]
impl LocalLookup for Echo {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/user/echo").then(|| self.clone_boxed())
    }
}

fn key() -> ReceptionistKey {
    ReceptionistKey::new("edge/sparse").unwrap()
}

async fn node(id: &str) -> Arc<RemoteActorSystem> {
    node_with(id, AclRules::permissive(), 1024).await
}

async fn node_with(id: &str, relay: AclRules, queue: usize) -> Arc<RemoteActorSystem> {
    start_node(
        RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap())),
        relay,
        queue,
    )
    .await
}

async fn start_node(
    mut config: RemoteConfig,
    relay: AclRules,
    queue: usize,
) -> Arc<RemoteActorSystem> {
    config = config
        .with_insecure_development()
        .with_receptionist_acl(AclRules::permissive())
        .with_receptionist_relay_acl(relay)
        .with_knobs(parrot_remote::transport::RuntimeKnobs {
            outbound_queue: queue,
            ..Default::default()
        });
    config.swim.gossip_interval = Duration::from_millis(20);
    config.swim.probe_interval = Duration::from_secs(3600);
    let system = RemoteActorSystem::new(config, Arc::new(Echo)).unwrap();
    system.start().await.unwrap();
    system
}

async fn connect(a: &Arc<RemoteActorSystem>, b: &Arc<RemoteActorSystem>) {
    a.connect(&NodeAddr::tcp(&b.config.node_id, b.local_addr().unwrap()))
        .await
        .unwrap();
}

#[tokio::test]
async fn outbound_only_owner_is_discovered_and_withdrawn_through_a_relay() {
    let observer = node("sparse-outbound-observer").await;
    let relay = node("sparse-outbound-relay").await;
    let owner = start_node(
        RemoteConfig::tcp("sparse-outbound-owner", None),
        AclRules::permissive(),
        1024,
    )
    .await;
    assert!(owner.local_addr().is_none());
    let mut events = observer
        .receptionist_gateway()
        .subscribe(key())
        .await
        .unwrap();
    owner
        .receptionist_gateway()
        .register(key(), "/user/echo".into())
        .await
        .unwrap();
    let outcome = tokio::time::timeout(LIMIT, async {
        connect(&observer, &relay).await;
        connect(&owner, &relay).await;
        registered(&mut events, &owner.config.node_id, key()).await;
        loop {
            let members = observer.membership.lock().await;
            if let Some(member) = members
                .members
                .get(&owner.config.node_id)
                .filter(|member| member.incarnation >= 1)
            {
                assert_eq!(member.addr.host, "");
                assert_eq!(member.addr.port, 0);
                break;
            }
            drop(members);
            tokio::task::yield_now().await;
        }
        assert_eq!(
            observer
                .missing_direct_connections(&[&owner.config.node_id])
                .len(),
            1
        );
        owner
            .receptionist_gateway()
            .deregister(key(), "/user/echo".into())
            .await
            .unwrap();
        assert_eq!(
            events.recv().await,
            Some(ReceptionistEvent::Unregistered {
                key: key(),
                remote_path: format!("parrot://{}/user/echo", owner.config.node_id)
            })
        );
    })
    .await;
    owner.shutdown().await.unwrap();
    relay.shutdown().await.unwrap();
    observer.shutdown().await.unwrap();
    outcome.expect("outbound-only owners must participate in sparse service discovery");
}

#[tokio::test]
async fn sparse_chain_discovers_and_withdraws_before_business_dial() {
    let a = node("sparse-chain-a").await;
    let b = node("sparse-chain-b").await;
    let c = node("sparse-chain-c").await;
    let mut events = a.receptionist_gateway().subscribe(key()).await.unwrap();
    let publisher = c.receptionist_gateway();
    publisher
        .register(key(), "/user/echo".into())
        .await
        .unwrap();
    connect(&a, &b).await;
    connect(&b, &c).await;
    let result = tokio::time::timeout(LIMIT, async {
        let path = format!("parrot://{}/user/echo", c.config.node_id);
        assert_eq!(
            events.recv().await,
            Some(ReceptionistEvent::Registered {
                key: key(),
                remote_path: path.clone()
            })
        );
        assert_eq!(a.missing_direct_connections(&[&c.config.node_id]).len(), 1);
        assert_eq!(c.missing_direct_connections(&[&a.config.node_id]).len(), 1);
        a.remote_ref(&path)
            .expect("the first discovery event must already have a route");
        publisher
            .deregister(key(), "/user/echo".into())
            .await
            .unwrap();
        assert_eq!(
            events.recv().await,
            Some(ReceptionistEvent::Unregistered {
                key: key(),
                remote_path: path.clone()
            })
        );
        assert_eq!(
            a.receptionist_gateway()
                .subscribe(key())
                .await
                .unwrap()
                .try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
        assert_eq!(a.missing_direct_connections(&[&c.config.node_id]).len(), 1);
        publisher
            .register(key(), "/user/echo".into())
            .await
            .unwrap();
        assert_eq!(
            events.recv().await,
            Some(ReceptionistEvent::Registered {
                key: key(),
                remote_path: path.clone()
            })
        );
        let reply = a
            .remote_ref(&path)
            .unwrap()
            .send(Box::new(Query(718)))
            .await
            .unwrap();
        assert_eq!(reply.downcast_ref::<Query>().unwrap().0, 718);
    })
    .await;
    c.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
    result.expect(
        "three-node discovery/withdrawal must converge without an endpoint-to-endpoint connection",
    );
}

fn service(value: &str) -> ReceptionistKey {
    ReceptionistKey::new(value).unwrap()
}
async fn registered(
    stream: &mut parrot_api::receptionist::ReceptionistStream,
    node: &str,
    service: ReceptionistKey,
) {
    assert_eq!(
        tokio::time::timeout(LIMIT, stream.recv()).await.unwrap(),
        Some(ReceptionistEvent::Registered {
            key: service,
            remote_path: format!("parrot://{node}/user/echo")
        })
    );
}
async fn echo(system: &Arc<RemoteActorSystem>, node: &str, value: u64) {
    let reply = tokio::time::timeout(
        LIMIT,
        system
            .remote_ref(&format!("parrot://{node}/user/echo"))
            .unwrap()
            .send(Box::new(Query(value))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(reply.downcast_ref::<Query>().unwrap().0, value);
}

#[tokio::test]
async fn missing_relay_grant_filters_foreign_keys_without_breaking_direct_service() {
    let a = node_with("sparse-deny-a", AclRules::default(), 1024).await;
    let b = node("sparse-deny-b").await;
    let c = node("sparse-deny-c").await;
    let mut forbidden = a.receptionist_gateway().subscribe(key()).await.unwrap();
    let mut learned = b.receptionist_gateway().subscribe(key()).await.unwrap();
    let marker = service("edge/relay-marker");
    let mut marker_events = a
        .receptionist_gateway()
        .subscribe(marker.clone())
        .await
        .unwrap();
    c.receptionist_gateway()
        .register(key(), "/user/echo".into())
        .await
        .unwrap();
    connect(&a, &b).await;
    connect(&b, &c).await;
    registered(&mut learned, &c.config.node_id, key()).await;
    b.receptionist_gateway()
        .register(marker.clone(), "/user/echo".into())
        .await
        .unwrap();
    // A marker published after B learns C proves A processed a complete B view.
    registered(&mut marker_events, &b.config.node_id, marker).await;
    assert_eq!(
        forbidden.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    assert_eq!(a.missing_direct_connections(&[&c.config.node_id]).len(), 1);
    echo(&a, &b.config.node_id, 719).await;
    c.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn diamond_caches_alternate_before_failure_and_withdraws_through_survivor() {
    let a = node_with(
        "sparse-diamond-a",
        AclRules {
            default_deny: true,
            roles: [
                ("sparse-diamond-b".into(), vec![key().as_str().into()]),
                ("sparse-diamond-c".into(), vec![key().as_str().into()]),
            ]
            .into(),
        },
        1024,
    )
    .await;
    let b = node("sparse-diamond-b").await;
    let c = node("sparse-diamond-c").await;
    let d = node("sparse-diamond-d").await;
    let mut observed = a.receptionist_gateway().subscribe(key()).await.unwrap();
    let mut c_learned = c.receptionist_gateway().subscribe(key()).await.unwrap();
    let marker = service("edge/alternate-ready");
    let after = service("edge/alternate-survived");
    let mut ready = a
        .receptionist_gateway()
        .subscribe(marker.clone())
        .await
        .unwrap();
    let mut survived = a
        .receptionist_gateway()
        .subscribe(after.clone())
        .await
        .unwrap();
    let local = service("edge/local");
    a.receptionist_gateway()
        .register(local.clone(), "/user/echo".into())
        .await
        .unwrap();
    d.receptionist_gateway()
        .register(key(), "/user/echo".into())
        .await
        .unwrap();
    connect(&a, &b).await;
    connect(&b, &d).await;
    registered(&mut observed, &d.config.node_id, key()).await;
    // Establish the alternate before A-C exists, so C can only learn D
    // from its direct publisher rather than through the primary A-B path.
    connect(&c, &d).await;
    registered(&mut c_learned, &d.config.node_id, key()).await;
    connect(&a, &c).await;
    // These marker keys have no relay grant at A: they must arrive in C's
    // direct source snapshot, alongside its already-learned D advertisement.
    c.receptionist_gateway()
        .register(marker.clone(), "/user/echo".into())
        .await
        .unwrap();
    registered(&mut ready, &c.config.node_id, marker).await;
    b.shutdown().await.unwrap();
    tokio::time::timeout(LIMIT, async {
        while a
            .missing_direct_connections(&[&b.config.node_id])
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    c.receptionist_gateway()
        .register(after.clone(), "/user/echo".into())
        .await
        .unwrap();
    registered(&mut survived, &c.config.node_id, after).await;
    let mut snapshot = a.receptionist_gateway().subscribe(key()).await.unwrap();
    registered(&mut snapshot, &d.config.node_id, key()).await;
    assert_eq!(
        observed.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty),
        "equal alternate route must not produce withdrawal/re-registration churn"
    );
    let mut own = a
        .receptionist_gateway()
        .subscribe(local.clone())
        .await
        .unwrap();
    registered(&mut own, &a.config.node_id, local).await;
    assert_eq!(a.missing_direct_connections(&[&d.config.node_id]).len(), 1);
    d.receptionist_gateway()
        .deregister(key(), "/user/echo".into())
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(LIMIT, observed.recv()).await.unwrap(),
        Some(ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: format!("parrot://{}/user/echo", d.config.node_id)
        })
    );
    assert_eq!(
        a.receptionist_gateway()
            .subscribe(key())
            .await
            .unwrap()
            .try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    d.shutdown().await.unwrap();
    c.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}
