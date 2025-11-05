use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    swim::{sync_to_node_table, Member, MemberEvent, MemberStatus, Membership, NodeAddrWire},
    LocalLookup, NodeAddr, NodeTable, RemoteActorSystem, RemoteConfig,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

fn member(id: &str, host: &str, port: u16) -> Member {
    Member {
        node_id: id.into(),
        addr: NodeAddrWire {
            node_id: id.into(),
            scheme: "tcp".into(),
            host: host.into(),
            port,
        },
        status: MemberStatus::Alive,
        incarnation: 1,
        status_until_ms: 0,
        metadata: Default::default(),
    }
}
#[tokio::test]
async fn membership_routes_preserve_ipv6_and_reject_nondialable_addresses() {
    let mut m = Membership::new(0);
    for entry in [
        member("v6", "::1", 9000),
        member("no-listener", "", 0),
        member("bad-host", "not an IP", 9001),
        member("wildcard", "0.0.0.0", 9002),
        member("zero", "127.0.0.1", 0),
    ] {
        m.merge_event(&MemberEvent::Upsert(entry));
    }
    let m = tokio::sync::Mutex::new(m);
    let nodes = NodeTable::new();
    sync_to_node_table(&m, &nodes).await;
    assert_eq!(
        nodes.get("v6").unwrap().0.addr,
        "[::1]:9000".parse().unwrap()
    );
    for id in ["no-listener", "bad-host", "wildcard", "zero"] {
        assert!(!nodes.contains(id), "invalid route published for {id}");
    }
}
#[tokio::test]
async fn membership_routes_update_and_retire_without_deleting_configured_seeds() {
    let m = tokio::sync::Mutex::new(Membership::new(0));
    let nodes = NodeTable::new();
    let configured = NodeAddr::tcp("configured", "127.0.0.1:8000".parse().unwrap());
    nodes.add_seed(configured.clone());
    m.lock()
        .await
        .merge_event(&MemberEvent::Upsert(member("dynamic", "127.0.0.1", 9000)));
    sync_to_node_table(&m, &nodes).await;
    let mut updated = member("dynamic", "127.0.0.1", 9001);
    updated.incarnation = 2;
    m.lock()
        .await
        .merge_event(&MemberEvent::Upsert(updated.clone()));
    sync_to_node_table(&m, &nodes).await;
    assert_eq!(
        nodes.get("dynamic").unwrap().0.addr.port(),
        9001,
        "new incarnation must replace old route"
    );
    updated.status = MemberStatus::Dead;
    m.lock().await.merge_event(&MemberEvent::Upsert(updated));
    sync_to_node_table(&m, &nodes).await;
    assert!(
        !nodes.contains("dynamic"),
        "dead membership route must be retired"
    );
    assert_eq!(nodes.get("configured").unwrap().0, configured);
    m.lock()
        .await
        .merge_event(&MemberEvent::Upsert(member("removed", "127.0.0.1", 9002)));
    sync_to_node_table(&m, &nodes).await;
    m.lock().await.members.clear();
    sync_to_node_table(&m, &nodes).await;
    assert!(!nodes.contains("removed"));
    assert!(nodes.contains("configured"));
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Ping(u64);
parrot_api::message::inventory::submit! { parrot_api::message::CodecRegistration {
    type_key:"bin:parrot.membership.route.Ping#v1", type_id: std::any::TypeId::of::<Ping>(),
    encode: |m:&BoxedMessage| parrot_api::message::serde_remote_serialize(m.downcast_ref::<Ping>().ok_or("Ping")?),
    decode: |bytes:&[u8]| { let m:Ping=parrot_api::message::serde_remote_deserialize(bytes)?;Ok(Box::new(m) as BoxedMessage) },
} }
#[derive(Clone, Debug)]
struct Echo(Arc<AtomicUsize>);
impl ActorRef for Echo {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(msg)
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(msg)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/echo".into()
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
struct Lookup(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        Some(Box::new(Echo(self.0.clone())))
    }
}
fn tcp(id: &str, counter: Arc<AtomicUsize>) -> Arc<RemoteActorSystem> {
    RemoteActorSystem::new(
        RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap())).with_insecure_development(),
        Arc::new(Lookup(counter)),
    )
    .unwrap()
}
async fn route_known(system: &RemoteActorSystem, node: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while system.nodes.get(node).is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("gossip must publish a dialable route without a manual connect to that node");
}
#[tokio::test]
async fn learned_tcp_route_serves_first_concurrent_asks_without_building_a_full_mesh() {
    let count = Arc::new(AtomicUsize::new(0));
    let a = tcp("route-a", Arc::new(AtomicUsize::new(0)));
    let b = tcp("route-b", Arc::new(AtomicUsize::new(0)));
    let c = tcp("route-c", count.clone());
    for node in [&a, &b, &c] {
        node.start().await.unwrap();
    }
    a.connect(&NodeAddr::tcp("route-b", b.local_addr().unwrap()))
        .await
        .unwrap();
    b.connect(&NodeAddr::tcp("route-c", c.local_addr().unwrap()))
        .await
        .unwrap();
    let outcome = async {
        route_known(&a, "route-c").await;
        assert_eq!(
            a.nodes.get("route-c").unwrap().0.addr,
            c.local_addr().unwrap()
        );
        assert_eq!(
            a.links_snapshot().await.len(),
            1,
            "learning a route must not dial every member"
        );
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let indirect = a.membership.lock().await.members["route-c"].clone();
        assert_eq!(indirect.status, MemberStatus::Alive);
        assert_eq!(
            indirect.incarnation, 1,
            "a route without a direct link must not trigger false suspicions"
        );
        let actor = a.remote_ref("parrot://route-c/echo").unwrap();
        let replies = futures::future::join_all((0..20).map(|n| {
            let actor = actor.clone();
            async move {
                actor
                    .send_with_timeout(Box::new(Ping(n)), Some(Duration::from_secs(2)))
                    .await
                    .map(|reply| reply.downcast_ref::<Ping>().unwrap().0)
            }
        }))
        .await;
        for (n, reply) in replies.into_iter().enumerate() {
            assert_eq!(reply.unwrap(), n as u64);
        }
        assert_eq!(count.load(Ordering::SeqCst), 20);
        assert_eq!(
            a.links_snapshot()
                .await
                .iter()
                .filter(|(id, _, _)| id == "route-c")
                .count(),
            1,
            "concurrent discovery must coalesce physical dials"
        );
        assert!(a.callbacks.is_empty());
        c.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while a.nodes.get("route-c").is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a discovery connection must not permanently promote its route into a seed");
        assert!(a.remote_ref("parrot://route-c/echo").is_err());
        assert!(
            a.nodes.contains("route-b"),
            "configured/manual seed ownership is independent"
        );
    };
    use futures::FutureExt;
    let result = std::panic::AssertUnwindSafe(outcome).catch_unwind().await;
    for node in [&a, &b, &c] {
        node.shutdown().await.unwrap();
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn membership_retirement_preserves_a_seed_with_the_same_identity() {
    let nodes = NodeTable::new();
    let seed = NodeAddr::tcp("shared", "127.0.0.1:8000".parse().unwrap());
    nodes.add_seed(seed.clone());
    let status = nodes.get("shared").unwrap().1;
    let m = tokio::sync::Mutex::new(Membership::new(0));
    m.lock()
        .await
        .merge_event(&MemberEvent::Upsert(member("shared", "127.0.0.1", 9000)));
    sync_to_node_table(&m, &nodes).await;
    assert_eq!(nodes.get("shared").unwrap().0.addr.port(), 9000);
    assert!(Arc::ptr_eq(&status, &nodes.get("shared").unwrap().1));
    m.lock().await.members.clear();
    sync_to_node_table(&m, &nodes).await;
    assert_eq!(nodes.get("shared").unwrap().0, seed);
    assert!(Arc::ptr_eq(&status, &nodes.get("shared").unwrap().1));
}

#[tokio::test]
async fn discovered_dial_obeys_ask_tell_and_completion_deadlines() {
    use tokio::io::AsyncReadExt;
    let source = tcp("dial-budget-source", Arc::new(AtomicUsize::new(0)));
    source.start().await.unwrap();
    for operation in 0..3 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let id = format!("silent-{operation}");
        let observer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            stream.read_to_end(&mut data).await.unwrap();
            data
        });
        source
            .membership
            .lock()
            .await
            .merge_event(&MemberEvent::Upsert(member(
                &id,
                "127.0.0.1",
                address.port(),
            )));
        route_known(&source, &id).await;
        let actor = source.remote_ref(&format!("parrot://{id}/echo")).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), async {
            match operation {
                0 => actor
                    .send_with_timeout(Box::new(Ping(1)), Some(Duration::from_millis(30)))
                    .await
                    .map(|_| ()),
                1 => {
                    actor
                        .deliver_with_options(
                            Box::new(Ping(1)),
                            parrot_api::message::MessageOptions {
                                timeout: Some(Duration::from_millis(30)),
                                ..Default::default()
                            },
                        )
                        .await
                }
                _ => {
                    actor
                        .deliver_with_ack(Box::new(Ping(1)), Duration::from_millis(30))
                        .await
                }
            }
        })
        .await
        .expect("caller deadline must include discovery and handshake");
        assert!(
            matches!(
                result,
                Err(parrot_api::errors::ActorError::Timeout
                    | parrot_api::errors::ActorError::TimeoutDetail(_))
            ),
            "{result:?}"
        );
        assert!(source.callbacks.is_empty());
        let hello = tokio::time::timeout(Duration::from_secs(1), observer)
            .await
            .expect("cancelled discovery must close its socket")
            .unwrap();
        assert!(
            !hello.is_empty(),
            "real handshake traffic must have reached the silent peer"
        );
    }
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn discovered_route_still_enforces_the_handshake_node_identity() {
    let calls = Arc::new(AtomicUsize::new(0));
    let source = tcp("identity-source", Arc::new(AtomicUsize::new(0)));
    let actual = tcp("actual-target", calls.clone());
    source.start().await.unwrap();
    actual.start().await.unwrap();
    source
        .membership
        .lock()
        .await
        .merge_event(&MemberEvent::Upsert(member(
            "claimed-target",
            "127.0.0.1",
            actual.local_addr().unwrap().port(),
        )));
    route_known(&source, "claimed-target").await;
    let actor = source.remote_ref("parrot://claimed-target/echo").unwrap();
    assert!(actor
        .send_with_timeout(Box::new(Ping(1)), Some(Duration::from_secs(1)))
        .await
        .is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(source.callbacks.is_empty());
    assert!(source.links_snapshot().await.is_empty());
    source.shutdown().await.unwrap();
    actual.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_the_system_cancels_a_discovered_dial_and_its_caller() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted, ready) = tokio::sync::oneshot::channel();
    let observer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = accepted.send(());
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
    });
    let system = tcp("drop-discovery", Arc::new(AtomicUsize::new(0)));
    system.start().await.unwrap();
    system
        .membership
        .lock()
        .await
        .merge_event(&MemberEvent::Upsert(member(
            "silent-drop",
            "127.0.0.1",
            address.port(),
        )));
    route_known(&system, "silent-drop").await;
    let actor = system.remote_ref("parrot://silent-drop/echo").unwrap();
    let pending = tokio::spawn(async move { actor.send(Box::new(Ping(1))).await });
    tokio::time::timeout(Duration::from_secs(1), ready)
        .await
        .unwrap()
        .unwrap();
    let weak = Arc::downgrade(&system);
    drop(system);
    assert!(
        weak.upgrade().is_none(),
        "discovery futures must not retain their system owner"
    );
    assert!(tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    tokio::time::timeout(Duration::from_secs(1), observer)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn changed_route_during_handshake_cannot_publish_a_stale_link_or_send_business() {
    use futures::{SinkExt, StreamExt};
    use parrot_remote::{
        frame::{frame_type, Frame},
        handshake::{HandshakeAckBody, TopologyRole},
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let old_address = listener.local_addr().unwrap();
    let (accepted, ready) = tokio::sync::oneshot::channel();
    let (release, proceed) = tokio::sync::oneshot::channel();
    let observer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut wire = tokio_util::codec::Framed::new(stream, parrot_remote::transport::FrameCodec);
        let hello = wire.next().await.unwrap().unwrap();
        assert_eq!(hello.header.frame_type, frame_type::HANDSHAKE);
        accepted.send(()).unwrap();
        proceed.await.unwrap();
        let mut body = bytes::BytesMut::new();
        HandshakeAckBody {
            node_id: "moving-target".into(),
            realm: None,
            cluster: None,
            capabilities: parrot_remote::handshake::caps::BIN,
            max_frame_len: parrot_remote::frame::MAX_FRAME_LEN,
            topology_role: TopologyRole::Normal,
            hop_limit: 8,
            direct_addr: None,
            chosen_codec: "bin".into(),
        }
        .encode_tlv(&mut body)
        .unwrap();
        wire.send(Frame::handshake_ack(body.freeze()))
            .await
            .unwrap();
        let mut business = 0;
        while let Some(Ok(frame)) = wire.next().await {
            if matches!(
                frame.header.frame_type,
                frame_type::ASK | frame_type::TELL | frame_type::STOP
            ) {
                business += 1;
            }
        }
        business
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let target = tcp("moving-target", calls.clone());
    let source = tcp("route-change-source", Arc::new(AtomicUsize::new(0)));
    source.start().await.unwrap();
    target.start().await.unwrap();
    source
        .membership
        .lock()
        .await
        .merge_event(&MemberEvent::Upsert(member(
            "moving-target",
            "127.0.0.1",
            old_address.port(),
        )));
    route_known(&source, "moving-target").await;
    let actor = source.remote_ref("parrot://moving-target/echo").unwrap();
    let first = actor.clone();
    let pending = tokio::spawn(async move {
        first
            .send_with_timeout(Box::new(Ping(1)), Some(Duration::from_secs(2)))
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), ready)
        .await
        .unwrap()
        .unwrap();
    let address = target.local_addr().unwrap();
    let mut update = member("moving-target", "127.0.0.1", address.port());
    update.incarnation = 2;
    source
        .membership
        .lock()
        .await
        .merge_event(&MemberEvent::Upsert(update));
    tokio::time::timeout(Duration::from_secs(1), async {
        while source.nodes.get("moving-target").unwrap().0.addr != address {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    release.send(()).unwrap();
    assert!(pending.await.unwrap().is_err());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), observer)
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(source.links_snapshot().await.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let reply = actor
        .send_with_timeout(Box::new(Ping(2)), Some(Duration::from_secs(1)))
        .await
        .unwrap();
    assert_eq!(reply.downcast_ref::<Ping>().unwrap().0, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}
