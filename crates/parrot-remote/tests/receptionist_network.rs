//! Real TCP discovery and ingress contracts, with a controlled local message target.
use parrot_api::{
    address::ActorRef,
    errors::ActorError,
    receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey, ReceptionistStream},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    acl::AclRules,
    frame::{frame_type, Frame, FrameHeader, PROTOCOL_VERSION, SEQ_NONE},
    receptionist::{encode_sync, ReceptionistEntry, ReceptionistSync},
    swim::SwimConfig,
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc::error::TryRecvError;

const LIMIT: Duration = Duration::from_secs(5);
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
struct Ping(u64);
impl parrot_api::message::Message for Ping {
    type Result = ();
}
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:receptionist-network::Ping#v1",
        type_id: std::any::TypeId::of::<Ping>(),
        encode: |m: &BoxedMessage| parrot_api::message::serde_remote_serialize(m.downcast_ref::<Ping>().ok_or("Ping")?),
        decode: |b: &[u8]| { let p: Ping=parrot_api::message::serde_remote_deserialize(b)?; Ok(Box::new(p) as BoxedMessage) },
    }
}
#[derive(Clone, Debug, Default)]
struct Echo(Arc<Mutex<Vec<u64>>>);
impl ActorRef for Echo {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .push(msg.downcast_ref::<Ping>().unwrap().0);
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
    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move { self.send(msg).await.map(|_| ()) })
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
    ReceptionistKey::new("edge/echo").unwrap()
}
fn config(id: &str) -> RemoteConfig {
    RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap()))
        .with_insecure_development()
        .with_receptionist_acl(AclRules::permissive())
        .with_swim_config(SwimConfig {
            probe_interval: Duration::from_millis(50),
            probe_timeout: Duration::from_millis(200),
            suspect_timeout: Duration::from_millis(400),
            gossip_interval: Duration::from_millis(20),
            ..Default::default()
        })
}
async fn node(config: RemoteConfig) -> (Arc<RemoteActorSystem>, Echo) {
    let echo = Echo::default();
    let node = RemoteActorSystem::new(config, Arc::new(echo.clone())).unwrap();
    node.start().await.unwrap();
    (node, echo)
}
async fn connect(a: &Arc<RemoteActorSystem>, b: &Arc<RemoteActorSystem>) {
    a.connect(&NodeAddr {
        node_id: b.config.node_id.clone(),
        identity_is_explicit: true,
        scheme: b.config.scheme.into(),
        addr: b.local_addr().unwrap(),
    })
    .await
    .unwrap();
    tokio::time::timeout(LIMIT, async {
        loop {
            if a.membership
                .lock()
                .await
                .members
                .contains_key(&b.config.node_id)
                && b.membership
                    .lock()
                    .await
                    .members
                    .contains_key(&a.config.node_id)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn registered(node: &str) -> ReceptionistEvent {
    ReceptionistEvent::Registered {
        key: key(),
        remote_path: format!("parrot://{node}/user/echo"),
    }
}
async fn event(stream: &mut ReceptionistStream, expected: ReceptionistEvent) -> String {
    let actual = tokio::time::timeout(LIMIT, stream.recv())
        .await
        .unwrap()
        .expect("live discovery stream");
    assert_eq!(actual, expected);
    match actual {
        ReceptionistEvent::Registered { remote_path, .. }
        | ReceptionistEvent::Unregistered { remote_path, .. } => remote_path,
    }
}
fn empty(stream: &mut ReceptionistStream) {
    assert_eq!(stream.try_recv(), Err(TryRecvError::Empty));
}
fn sync(from: &str) -> ReceptionistSync {
    ReceptionistSync {
        from: from.into(),
        entries: vec![ReceptionistEntry {
            key: key(),
            remote_path: format!("parrot://{from}/user/echo"),
            node: from.into(),
        }],
    }
}
fn wire(payload: bytes::Bytes) -> Frame {
    Frame {
        header: FrameHeader {
            frame_len: 0,
            version: PROTOCOL_VERSION,
            frame_type: frame_type::SYSTEM_EVENT,
            flags: 0,
            correlation_id: 0,
            hop_count: 0,
            hop_limit: 1,
            seq: SEQ_NONE,
        },
        path: String::new(),
        type_key: String::new(),
        payload,
    }
}

#[tokio::test]
async fn tcp_snapshot_live_withdrawal_and_discovered_path_execute_real_message() {
    let (a, _) = node(config("discover-a")).await;
    let (b, echo) = node(config("discover-b")).await;
    let gateway = b.receptionist_gateway();
    gateway.register(key(), "/user/echo".into()).await.unwrap();
    let mut events = a.receptionist_gateway().subscribe(key()).await.unwrap();
    connect(&a, &b).await;
    let path = event(&mut events, registered("discover-b")).await;
    let response = tokio::time::timeout(
        LIMIT,
        a.remote_ref(&path).unwrap().send(Box::new(Ping(656))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(*response.downcast::<Ping>().unwrap(), Ping(656));
    assert_eq!(*echo.0.lock().unwrap(), vec![656]);
    let mut snapshot = a.receptionist_gateway().subscribe(key()).await.unwrap();
    event(&mut snapshot, registered("discover-b")).await;
    gateway
        .deregister(key(), "/user/echo".into())
        .await
        .unwrap();
    let removed = ReceptionistEvent::Unregistered {
        key: key(),
        remote_path: path,
    };
    event(&mut events, removed.clone()).await;
    event(&mut snapshot, removed).await;
    empty(&mut events);
    empty(&mut snapshot);
    empty(&mut a.receptionist_gateway().subscribe(key()).await.unwrap());
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn real_peer_death_withdraws_only_its_entries() {
    let (a, _) = node(config("death-a")).await;
    let (b, _) = node(config("death-b")).await;
    a.receptionist_gateway()
        .register(key(), "/user/echo".into())
        .await
        .unwrap();
    b.receptionist_gateway()
        .register(key(), "/user/echo".into())
        .await
        .unwrap();
    let mut events = a.receptionist_gateway().subscribe(key()).await.unwrap();
    event(&mut events, registered("death-a")).await;
    connect(&a, &b).await;
    let path = event(&mut events, registered("death-b")).await;
    b.shutdown().await.unwrap();
    event(
        &mut events,
        ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: path,
        },
    )
    .await;
    let mut snapshot = a.receptionist_gateway().subscribe(key()).await.unwrap();
    event(&mut snapshot, registered("death-a")).await;
    empty(&mut snapshot);
    empty(&mut events);
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn owned_gateway_is_default_deny_qualifies_paths_and_closes_on_shutdown() {
    let (denied, _) = node(config("denied").with_receptionist_acl(AclRules::default())).await;
    let gateway = denied.receptionist_gateway();
    let expected = "forbidden: receptionist key outside role scope (E5 ACL)";
    for result in [
        gateway.register(key(), "/user/echo".into()).await,
        gateway.deregister(key(), "/user/echo".into()).await,
    ] {
        assert!(matches!(result, Err(ActorError::InternalError(ref error)) if error == expected));
    }
    assert!(
        matches!(gateway.subscribe(key()).await, Err(ActorError::InternalError(ref error)) if error == expected)
    );
    denied.shutdown().await.unwrap();
    let (system, _) = node(config("owner")).await;
    let gateway = system.receptionist_gateway();
    let mut events = gateway.subscribe(key()).await.unwrap();
    for path in [
        "parrot://other/user/echo",
        "/",
        "/user/../echo",
        "relative",
        "/user/echo?query",
    ] {
        assert!(
            matches!(gateway.register(key(), path.into()).await, Err(ActorError::InternalError(ref error))
            if error == "receptionist path must belong to its owning remote node")
        );
        assert!(gateway.deregister(key(), path.into()).await.is_err());
        empty(&mut events);
    }
    gateway.register(key(), "/user/echo".into()).await.unwrap();
    event(&mut events, registered("owner")).await;
    system.shutdown().await.unwrap();
    assert!(events.recv().await.is_none());
    assert!(matches!(
        gateway.register(key(), "/user/echo".into()).await,
        Err(ActorError::Stopped)
    ));
    assert!(matches!(
        gateway.deregister(key(), "/user/echo".into()).await,
        Err(ActorError::Stopped)
    ));
    assert!(matches!(
        gateway.subscribe(key()).await,
        Err(ActorError::Stopped)
    ));
}

#[tokio::test]
async fn malformed_and_forged_snapshots_close_the_exact_peer_without_partial_merge() {
    for case in [
        "from",
        "path",
        "node",
        "acl",
        "trailing",
        "cid",
        "frame-path",
        "type-key",
        "seq",
    ] {
        let mut cfg = config("protected");
        cfg.swim.probe_interval = Duration::from_secs(60);
        cfg.receptionist_acl =
            AclRules::from_json(r#"{"roles":{"peer":["edge/echo"],"protected":["edge/echo"]}}"#)
                .unwrap();
        let (a, _) = node(cfg).await;
        let mut cfg = config("peer");
        cfg.swim.probe_interval = Duration::from_secs(60);
        let (b, _) = node(cfg).await;
        a.receptionist_gateway()
            .register(key(), "/user/echo".into())
            .await
            .unwrap();
        let mut events = a.receptionist_gateway().subscribe(key()).await.unwrap();
        event(&mut events, registered("protected")).await;
        connect(&b, &a).await;
        let mut update = sync("peer");
        match case {
            "from" => update.from = "protected".into(),
            "path" => update.entries.push(ReceptionistEntry {
                remote_path: "parrot://protected/user/injected".into(),
                ..update.entries[0].clone()
            }),
            "node" => update.entries.push(ReceptionistEntry {
                node: "other".into(),
                ..update.entries[0].clone()
            }),
            "acl" => update.entries.push(ReceptionistEntry {
                key: ReceptionistKey::new("admin/escape").unwrap(),
                ..update.entries[0].clone()
            }),
            _ => {}
        }
        let mut frame = wire(encode_sync(&update));
        match case {
            "trailing" => {
                let mut body = frame.payload.to_vec();
                body.push(0);
                frame.payload = body.into();
            }
            "cid" => frame.header.correlation_id = 1,
            "frame-path" => frame.path = "/ignored".into(),
            "type-key" => frame.type_key = "unexpected".into(),
            "seq" => frame.header.seq = 1,
            _ => {}
        }
        let sender = b.links_snapshot().await[0].1.clone();
        sender.send(frame).await.unwrap();
        tokio::time::timeout(LIMIT, sender.closed())
            .await
            .expect("invalid snapshot must close its connection");
        empty(&mut events);
        let mut snapshot = a.receptionist_gateway().subscribe(key()).await.unwrap();
        event(&mut snapshot, registered("protected")).await;
        empty(&mut snapshot);
        b.shutdown().await.unwrap();
        a.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn gateway_does_not_own_system_and_shutdown_cancels_public_membership_wait() {
    let (system, _) = node(config("owner-drop")).await;
    let weak = Arc::downgrade(&system);
    let gateway = system.receptionist_gateway();
    let mut events = gateway.subscribe(key()).await.unwrap();
    drop(system);
    tokio::time::timeout(LIMIT, async {
        while weak.strong_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("gateway and background tasks must not retain their system");
    assert!(tokio::time::timeout(LIMIT, events.recv())
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        gateway.subscribe(key()).await,
        Err(ActorError::Stopped)
    ));
    let (system, _) = node(config("locked-member")).await;
    let guard = system.membership.lock().await;
    tokio::time::timeout(LIMIT, system.shutdown())
        .await
        .unwrap()
        .unwrap();
    drop(guard);
}

#[tokio::test]
async fn disconnected_watcher_does_not_keep_system_alive_while_membership_is_locked() {
    let (a, _) = node(config("locked-drop-a")).await;
    let (b, _) = node(config("locked-drop-b")).await;
    connect(&a, &b).await;
    let membership = a.membership.clone();
    let guard = membership.lock().await;
    b.shutdown().await.unwrap();
    tokio::time::timeout(LIMIT, async {
        while !a.links_snapshot().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the disconnect watcher must remove its link before waiting for membership");
    let weak = Arc::downgrade(&a);
    drop(a);
    tokio::time::timeout(LIMIT, async {
        while weak.strong_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a disconnected watcher must release system ownership before a public-lock wait");
    drop(guard);
}

struct Pki {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    ca: Vec<u8>,
}
impl Pki {
    fn new() -> Self {
        let mut params = rcgen::CertificateParams::default();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().unwrap();
        let ca = params.self_signed(&key).unwrap().pem().into_bytes();
        Self {
            issuer: rcgen::Issuer::new(params, key),
            ca,
        }
    }
    fn node(&self, name: &str) -> parrot_remote::tls::TlsConfig {
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        parrot_remote::tls::TlsConfig {
            cert_pem: cert.pem().into_bytes(),
            key_pem: key.serialize_pem().into_bytes(),
            ca_pem: self.ca.clone(),
        }
    }
}

#[tokio::test]
async fn mtls_certificate_identity_binds_discovery_on_tcp_and_quic_without_admin_grants() {
    let pki = Pki::new();
    for scheme in ["tcp", "quic"] {
        let config = |id| {
            let mut cfg = config(id).with_mtls(pki.node(id), []);
            cfg.scheme = scheme;
            cfg
        };
        let (a, _) = node(config("secure-a")).await;
        let (b, echo) = node(config("secure-b")).await;
        let mut events = a.receptionist_gateway().subscribe(key()).await.unwrap();
        b.receptionist_gateway()
            .register(key(), "/user/echo".into())
            .await
            .unwrap();
        connect(&a, &b).await;
        assert_eq!(
            a.links_snapshot().await[0].1.authentication(),
            parrot_remote::PeerAuthentication::MutualTls
        );
        let path = event(&mut events, registered("secure-b")).await;
        let response =
            tokio::time::timeout(LIMIT, a.remote_ref(&path).unwrap().send(Box::new(Ping(91))))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(*response.downcast::<Ping>().unwrap(), Ping(91));
        assert_eq!(*echo.0.lock().unwrap(), vec![91]);
        b.shutdown().await.unwrap();
        a.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn untrusted_plain_tcp_cannot_publish_even_with_a_valid_claimed_node_and_key() {
    let mut cfg = config("plain-server");
    cfg.security = parrot_remote::RemoteSecurity::Local;
    let (a, _) = node(cfg).await;
    let (b, _) = node(config("plain-peer")).await;
    let mut events = a.receptionist_gateway().subscribe(key()).await.unwrap();
    connect(&b, &a).await;
    let sender = b.links_snapshot().await[0].1.clone();
    sender
        .send(wire(encode_sync(&sync("plain-peer"))))
        .await
        .unwrap();
    tokio::time::timeout(LIMIT, sender.closed()).await.unwrap();
    empty(&mut events);
    empty(&mut a.receptionist_gateway().subscribe(key()).await.unwrap());
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn old_peer_is_not_sent_snapshots_and_cannot_send_unnegotiated_sync() {
    use futures::{SinkExt, StreamExt};
    use parrot_remote::{
        handshake::{caps, HandshakeBody},
        transport::FrameCodec,
    };
    use tokio_util::codec::Framed;
    let (a, _) = node(config("legacy-server")).await;
    a.receptionist_gateway()
        .register(key(), "/user/echo".into())
        .await
        .unwrap();
    let stream = tokio::net::TcpStream::connect(a.local_addr().unwrap())
        .await
        .unwrap();
    let mut peer = Framed::new(stream, FrameCodec);
    let mut body = bytes::BytesMut::new();
    HandshakeBody {
        node_id: "legacy-peer".into(),
        capabilities: caps::BIN,
        ..Default::default()
    }
    .encode_tlv(&mut body)
    .unwrap();
    peer.send(Frame::handshake(body.freeze())).await.unwrap();
    assert_eq!(
        tokio::time::timeout(LIMIT, peer.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .header
            .frame_type,
        frame_type::HANDSHAKE_ACK
    );
    // Observe several actual synchronization periods; old peers must remain usable.
    let until = tokio::time::Instant::now() + Duration::from_millis(150);
    while let Ok(event) = tokio::time::timeout_at(until, peer.next()).await {
        let frame = event.expect("old peer remains connected").unwrap();
        assert_ne!(
            frame.payload.first(),
            Some(&parrot_remote::admin::sys_event_tag::RECEPTIONIST_SYNC)
        );
    }
    peer.send(wire(encode_sync(&sync("legacy-peer"))))
        .await
        .unwrap();
    tokio::time::timeout(LIMIT, async {
        while let Some(frame) = peer.next().await {
            if frame.is_err() {
                break;
            }
        }
    })
    .await
    .unwrap();
    let mut snapshot = a.receptionist_gateway().subscribe(key()).await.unwrap();
    event(&mut snapshot, registered("legacy-server")).await;
    empty(&mut snapshot);
    a.shutdown().await.unwrap();
}

type RawPeer =
    tokio_util::codec::Framed<tokio::net::TcpStream, parrot_remote::transport::FrameCodec>;

async fn snapshot_only_peer(server: &Arc<RemoteActorSystem>) -> RawPeer {
    use futures::{SinkExt, StreamExt};
    use parrot_remote::{
        handshake::{caps, HandshakeBody},
        transport::FrameCodec,
    };
    let stream = tokio::net::TcpStream::connect(server.local_addr().unwrap())
        .await
        .unwrap();
    let mut peer = tokio_util::codec::Framed::new(stream, FrameCodec);
    let mut body = bytes::BytesMut::new();
    HandshakeBody {
        node_id: "snapshot-peer".into(),
        capabilities: caps::BIN | caps::RECEPTIONIST_SYNC,
        ..Default::default()
    }
    .encode_tlv(&mut body)
    .unwrap();
    peer.send(Frame::handshake(body.freeze())).await.unwrap();
    let ack = tokio::time::timeout(LIMIT, peer.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(ack.header.frame_type, frame_type::HANDSHAKE_ACK);
    tokio::time::timeout(LIMIT, async {
        loop {
            if server
                .membership
                .lock()
                .await
                .members
                .contains_key("snapshot-peer")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    peer
}

async fn next_snapshot(peer: &mut RawPeer) -> ReceptionistSync {
    use futures::StreamExt;
    tokio::time::timeout(LIMIT, async {
        loop {
            let frame = peer
                .next()
                .await
                .expect("legacy service connection must stay open")
                .unwrap();
            if frame.header.frame_type == frame_type::SYSTEM_EVENT
                && frame.payload.first()
                    == Some(&parrot_remote::admin::sys_event_tag::RECEPTIONIST_SYNC)
            {
                return parrot_remote::receptionist::decode_sync(&frame.payload[1..]).unwrap();
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn snapshot_only_peer_keeps_canonical_services_without_receiving_native_addresses() {
    use futures::SinkExt;
    let (server, _) = node(config("compatible-server")).await;
    let gateway = server.receptionist_gateway();
    gateway.register(key(), "/user/echo".into()).await.unwrap();
    gateway
        .register(key(), "actix://Echo/instance".into())
        .await
        .unwrap();
    let mut peer = snapshot_only_peer(&server).await;
    let expected = ReceptionistEntry {
        key: key(),
        node: "compatible-server".into(),
        remote_path: "parrot://compatible-server/user/echo".into(),
    };
    for _ in 0..3 {
        let snapshot = next_snapshot(&mut peer).await;
        assert_eq!(snapshot.from, "compatible-server");
        assert_eq!(snapshot.entries.as_slice(), std::slice::from_ref(&expected));
    }
    let mut events = gateway.subscribe(key()).await.unwrap();
    event(&mut events, registered("compatible-server")).await;
    event(
        &mut events,
        ReceptionistEvent::Registered {
            key: key(),
            remote_path: "parrot://compatible-server/actix://Echo/instance".into(),
        },
    )
    .await;
    peer.send(wire(encode_sync(&sync("snapshot-peer"))))
        .await
        .unwrap();
    event(&mut events, registered("snapshot-peer")).await;
    gateway
        .deregister(key(), "/user/echo".into())
        .await
        .unwrap();
    tokio::time::timeout(LIMIT, async {
        loop {
            let snapshot = next_snapshot(&mut peer).await;
            assert!(snapshot.entries.iter().all(|entry| entry == &expected));
            if snapshot.entries.is_empty() {
                break;
            }
        }
    })
    .await
    .unwrap();
    // The final empty snapshot is a withdrawal barrier; native registration
    // must still exist locally and the old connection must still carry traffic.
    assert!(server
        .links_snapshot()
        .await
        .iter()
        .any(|(node, sender, _)| node == "snapshot-peer" && !sender.is_closed()));
    let mut current = gateway.subscribe(key()).await.unwrap();
    let mut paths = Vec::new();
    for _ in 0..2 {
        match tokio::time::timeout(LIMIT, current.recv())
            .await
            .unwrap()
            .unwrap()
        {
            ReceptionistEvent::Registered { remote_path, .. } => paths.push(remote_path),
            other => panic!("unexpected local state: {other:?}"),
        }
    }
    paths.sort();
    assert_eq!(
        paths,
        [
            "parrot://compatible-server/actix://Echo/instance",
            "parrot://snapshot-peer/user/echo"
        ]
    );
    empty(&mut current);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn snapshot_only_peer_cannot_publish_unnegotiated_native_addresses() {
    use futures::{SinkExt, StreamExt};
    let (server, _) = node(config("protected-server")).await;
    let gateway = server.receptionist_gateway();
    gateway.register(key(), "/user/echo".into()).await.unwrap();
    let mut live = gateway.subscribe(key()).await.unwrap();
    event(&mut live, registered("protected-server")).await;
    let mut peer = snapshot_only_peer(&server).await;
    let mut invalid = sync("snapshot-peer");
    invalid.entries.push(ReceptionistEntry {
        key: key(),
        node: "snapshot-peer".into(),
        remote_path: "parrot://snapshot-peer/actix://Echo/instance".into(),
    });
    peer.send(wire(encode_sync(&invalid))).await.unwrap();
    tokio::time::timeout(LIMIT, async {
        while let Some(frame) = peer.next().await {
            if frame.is_err() {
                break;
            }
        }
    })
    .await
    .unwrap();
    empty(&mut live);
    let mut snapshot = gateway.subscribe(key()).await.unwrap();
    event(&mut snapshot, registered("protected-server")).await;
    empty(&mut snapshot);
    server.shutdown().await.unwrap();
}
