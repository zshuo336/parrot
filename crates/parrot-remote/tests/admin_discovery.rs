//! Management discovery must establish the named peer, never execute on a hub.
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    admin_v2::{AdminReplyV2, ComponentDeploy, ComponentExecutor},
    handshake::TopologyRole,
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig, RemoteError,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Clone, Debug, Default)]
struct Calls(Arc<AtomicUsize>);
impl ActorRef for Calls {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
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
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn path(&self) -> String {
        "/managed".into()
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
impl LocalLookup for Calls {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        assert_eq!(path, "/managed");
        Some(Box::new(self.clone()))
    }
}
#[async_trait::async_trait]
impl ComponentExecutor for Calls {
    async fn deploy(&self, _: u64, _: &ComponentDeploy) -> AdminReplyV2 {
        unreachable!()
    }
    async fn drain(&self, _: u64, _: &str, _: u64) -> AdminReplyV2 {
        unreachable!()
    }
    async fn status(&self, _: u64, _: &str) -> AdminReplyV2 {
        unreachable!()
    }
    async fn stop(&self, req_id: u64, prefix: &str) -> AdminReplyV2 {
        assert_eq!(prefix, "/managed");
        self.0.fetch_add(1, Ordering::SeqCst);
        AdminReplyV2::Stopped { req_id }
    }
}
fn node(config: RemoteConfig, calls: &Calls) -> Arc<RemoteActorSystem> {
    let system = RemoteActorSystem::new(config, Arc::new(calls.clone())).unwrap();
    system.set_component_executor(Some(Arc::new(calls.clone())));
    system
}
fn config(id: &str) -> RemoteConfig {
    RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap())).with_insecure_development()
}
async fn known(system: &RemoteActorSystem, id: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !system.nodes.contains(id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("gossip must publish target route");
}
async fn request(system: &Arc<RemoteActorSystem>, id: &str, v2: bool) -> Result<(), RemoteError> {
    if v2 {
        system.stop_component(id, "/managed").await
    } else {
        system.admin_stop(id, "/managed").await
    }
}
async fn first_admin(v2: bool) {
    let hub_calls = Calls::default();
    let target_calls = Calls::default();
    let a = node(config("admin-discovery-a"), &Calls::default());
    let hub = node(
        config("admin-discovery-hub").with_role(TopologyRole::Hub),
        &hub_calls,
    );
    let target = node(config("admin-discovery-target"), &target_calls);
    for system in [&a, &hub, &target] {
        system.start().await.unwrap();
    }
    a.connect(&NodeAddr::tcp(
        "admin-discovery-hub",
        hub.local_addr().unwrap(),
    ))
    .await
    .unwrap();
    hub.connect(&NodeAddr::tcp(
        "admin-discovery-target",
        target.local_addr().unwrap(),
    ))
    .await
    .unwrap();
    known(&a, "admin-discovery-target").await;
    assert_eq!(
        a.links_snapshot().await.len(),
        1,
        "no eager mesh before admin call"
    );
    let results =
        futures::future::join_all((0..12).map(|_| request(&a, "admin-discovery-target", v2))).await;
    let links = a.links_snapshot().await;
    for system in [&a, &hub, &target] {
        system.shutdown().await.unwrap();
    }
    assert!(
        results.iter().all(Result::is_ok),
        "first Admin v2={v2} failed: {results:?}"
    );
    assert_eq!(target_calls.0.load(Ordering::SeqCst), 12);
    assert_eq!(
        hub_calls.0.load(Ordering::SeqCst),
        0,
        "management must never execute on fallback hub"
    );
    assert_eq!(
        links
            .iter()
            .filter(|(id, _, _)| id == "admin-discovery-target")
            .count(),
        1
    );
}
#[tokio::test]
async fn first_v1_admin_uses_gossip_route_and_never_hub_fallback() {
    first_admin(false).await;
}
#[tokio::test]
async fn first_v2_admin_uses_gossip_route_and_never_hub_fallback() {
    first_admin(true).await;
}

#[tokio::test]
async fn missing_discovery_never_sends_management_to_an_existing_hub() {
    let calls = Calls::default();
    let a = node(config("admin-no-route-a"), &Calls::default());
    let hub = node(
        config("admin-no-route-hub").with_role(TopologyRole::Hub),
        &calls,
    );
    a.start().await.unwrap();
    hub.start().await.unwrap();
    a.connect(&NodeAddr::tcp(
        "admin-no-route-hub",
        hub.local_addr().unwrap(),
    ))
    .await
    .unwrap();
    for v2 in [false, true] {
        assert!(matches!(
            request(&a, "unknown", v2).await,
            Err(RemoteError::UnknownNode(_))
        ));
    }
    // Successful traffic on the same physical connection also proves it stayed usable.
    request(&a, "admin-no-route-hub", true).await.unwrap();
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    a.shutdown().await.unwrap();
    hub.shutdown().await.unwrap();
}

async fn advertise(system: &RemoteActorSystem, id: &str, address: std::net::SocketAddr) {
    use parrot_remote::swim::{Member, MemberEvent, MemberStatus, NodeAddrWire};
    system
        .membership
        .lock()
        .await
        .merge_event(&MemberEvent::Upsert(Member {
            node_id: id.into(),
            addr: NodeAddrWire {
                node_id: id.into(),
                scheme: "tcp".into(),
                host: address.ip().to_string(),
                port: address.port(),
            },
            status: MemberStatus::Alive,
            incarnation: 1,
            status_until_ms: 0,
            metadata: Default::default(),
        }));
    known(system, id).await;
}

#[tokio::test]
async fn cancelled_or_shutdown_admin_dial_closes_real_handshake_without_command() {
    use futures::StreamExt;
    use parrot_remote::{frame::frame_type, transport::FrameCodec};
    for v2 in [false, true] {
        for shutdown in [false, true] {
            let source = node(config("admin-cancel-source"), &Calls::default());
            source.start().await.unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            advertise(&source, "silent", listener.local_addr().unwrap()).await;
            let caller = source.clone();
            let task = tokio::spawn(async move { request(&caller, "silent", v2).await });
            let (socket, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut wire = tokio_util::codec::Framed::new(socket, FrameCodec);
            let hello = tokio::time::timeout(Duration::from_secs(1), wire.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(hello.header.frame_type, frame_type::HANDSHAKE);
            if shutdown {
                source.shutdown().await.unwrap();
                assert!(tokio::time::timeout(Duration::from_secs(1), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err());
            } else {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            }
            assert!(
                tokio::time::timeout(Duration::from_secs(1), wire.next())
                    .await
                    .unwrap()
                    .is_none(),
                "cancelled handshake must close before any command"
            );
            assert!(source.links_snapshot().await.is_empty());
            source.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn discovered_admin_rejects_a_different_handshake_identity_without_side_effects() {
    let calls = Calls::default();
    let source = node(config("admin-identity-source"), &Calls::default());
    let target = node(config("actual-admin-target"), &calls);
    source.start().await.unwrap();
    target.start().await.unwrap();
    advertise(
        &source,
        "claimed-admin-target",
        target.local_addr().unwrap(),
    )
    .await;
    for v2 in [false, true] {
        assert!(request(&source, "claimed-admin-target", v2).await.is_err());
    }
    assert!(source.links_snapshot().await.is_empty());
    assert_eq!(calls.0.load(Ordering::SeqCst), 0);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
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
async fn discovered_mtls_admin_preserves_target_allowlist_for_both_protocols() {
    let pki = Pki::new();
    let target_calls = Calls::default();
    let hub_calls = Calls::default();
    let secure = |id: &str| config(id).with_mtls(pki.node(id), ["secure-allowed".into()]);
    let denied = node(secure("secure-denied"), &Calls::default());
    let allowed = node(secure("secure-allowed"), &Calls::default());
    let hub = node(
        secure("secure-hub").with_role(TopologyRole::Hub),
        &hub_calls,
    );
    let target = node(secure("secure-target"), &target_calls);
    for s in [&denied, &allowed, &hub, &target] {
        s.start().await.unwrap();
    }
    for s in [&denied, &allowed] {
        s.connect(&NodeAddr::tcp("secure-hub", hub.local_addr().unwrap()))
            .await
            .unwrap();
    }
    hub.connect(&NodeAddr::tcp(
        "secure-target",
        target.local_addr().unwrap(),
    ))
    .await
    .unwrap();
    known(&denied, "secure-target").await;
    known(&allowed, "secure-target").await;
    for v2 in [false, true] {
        let error = request(&denied, "secure-target", v2).await.unwrap_err();
        if v2 {
            assert!(
                matches!(error, RemoteError::AdminRejected { code, .. } if code == parrot_remote::ErrCode::Forbidden as u16)
            );
        } else {
            assert!(
                error
                    .to_string()
                    .contains("administrative access requires an authorized connection identity"),
                "{error}"
            );
        }
        assert_eq!(target_calls.0.load(Ordering::SeqCst), 0);
    }
    for v2 in [false, true] {
        request(&allowed, "secure-target", v2).await.unwrap();
    }
    assert_eq!(target_calls.0.load(Ordering::SeqCst), 2);
    assert_eq!(hub_calls.0.load(Ordering::SeqCst), 0);
    for s in [&denied, &allowed] {
        let links = s.links_snapshot().await;
        let (_, sender, _) = links
            .iter()
            .find(|(id, _, _)| id == "secure-target")
            .unwrap();
        assert_eq!(
            sender.authentication(),
            parrot_remote::PeerAuthentication::MutualTls
        );
    }
    for s in [&denied, &allowed, &hub, &target] {
        s.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn discovered_metrics_requires_peer_capability_before_sending_command() {
    use futures::{SinkExt, StreamExt};
    use parrot_remote::{
        frame::{frame_type, Frame},
        handshake::HandshakeAckBody,
        transport::FrameCodec,
    };
    let source = node(config("metrics-discovery-source"), &Calls::default());
    source.start().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    advertise(&source, "legacy-metrics", listener.local_addr().unwrap()).await;
    let observer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut wire = tokio_util::codec::Framed::new(socket, FrameCodec);
        assert_eq!(
            wire.next().await.unwrap().unwrap().header.frame_type,
            frame_type::HANDSHAKE
        );
        let mut bytes = bytes::BytesMut::new();
        HandshakeAckBody {
            node_id: "legacy-metrics".into(),
            realm: None,
            cluster: None,
            capabilities: parrot_remote::handshake::caps::BIN,
            max_frame_len: parrot_remote::frame::MAX_FRAME_LEN,
            topology_role: TopologyRole::Normal,
            hop_limit: 8,
            direct_addr: None,
            chosen_codec: "bin".into(),
        }
        .encode_tlv(&mut bytes)
        .unwrap();
        wire.send(Frame::handshake_ack(bytes.freeze()))
            .await
            .unwrap();
        let mut commands = 0;
        while let Some(Ok(frame)) = wire.next().await {
            if frame.header.frame_type == frame_type::HEARTBEAT {
                wire.send(Frame::heartbeat_ack()).await.unwrap();
            }
            if frame.header.frame_type == frame_type::SYSTEM_EVENT {
                commands += 1;
            }
        }
        commands
    });
    let error = source.measured_metrics("legacy-metrics").await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not advertise measured metrics"),
        "{error}"
    );
    assert_eq!(
        source.links_snapshot().await.len(),
        1,
        "capability is checked on the real established peer"
    );
    source.shutdown().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), observer)
            .await
            .unwrap()
            .unwrap(),
        0
    );
}
