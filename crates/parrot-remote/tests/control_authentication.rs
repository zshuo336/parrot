//! Public TCP control entry points must not trust a self-declared node name.
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::admin_v2::{AdminReplyV2, ComponentDeploy, ComponentExecutor};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Debug)]
struct Victim {
    alive: AtomicBool,
    stopped: tokio::sync::Notify,
}
#[derive(Clone, Debug)]
struct VictimRef(Arc<Victim>);
#[async_trait::async_trait]
impl ActorRef for VictimRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(msg) })
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
            self.0.alive.store(false, Ordering::SeqCst);
            self.0.stopped.notify_one();
            Ok(())
        })
    }
    fn path(&self) -> String {
        "/user/victim".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { self.0.alive.load(Ordering::SeqCst) })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Lookup(Arc<Victim>);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
        (path == "/user/victim").then(|| Box::new(VictimRef(self.0.clone())) as Box<dyn ActorRef>)
    }
}
struct Executor(AtomicUsize);
#[async_trait::async_trait]
impl ComponentExecutor for Executor {
    async fn deploy(&self, _: u64, _: &ComponentDeploy) -> AdminReplyV2 {
        unreachable!()
    }
    async fn drain(&self, _: u64, _: &str, _: u64) -> AdminReplyV2 {
        unreachable!()
    }
    async fn status(&self, _: u64, _: &str) -> AdminReplyV2 {
        unreachable!()
    }
    async fn stop(&self, req_id: u64, _: &str) -> AdminReplyV2 {
        self.0.fetch_add(1, Ordering::SeqCst);
        AdminReplyV2::Stopped { req_id }
    }
}
async fn plain_pair() -> (Arc<RemoteActorSystem>, Arc<RemoteActorSystem>, Arc<Victim>) {
    let victim = Arc::new(Victim {
        alive: AtomicBool::new(true),
        stopped: tokio::sync::Notify::new(),
    });
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("protected", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Lookup(victim.clone())),
    )
    .unwrap();
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("self-claimed-admin", None),
        Arc::new(Lookup(victim.clone())),
    )
    .unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    client
        .connect(&NodeAddr::tcp("protected", server.local_addr().unwrap()))
        .await
        .unwrap();
    (client, server, victim)
}
#[tokio::test]
async fn unauthenticated_tcp_admin_v2_never_reaches_executor_by_default() {
    let (client, server, _) = plain_pair().await;
    let executor = Arc::new(Executor(AtomicUsize::new(0)));
    server.set_component_executor(Some(executor.clone()));
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.stop_component("protected", "/user"),
    )
    .await
    .unwrap();
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    assert_eq!(
        executor.0.load(Ordering::SeqCst),
        0,
        "untrusted peer executed a control command"
    );
    assert!(result.is_err(), "unauthenticated command succeeded");
    assert!(
        result.unwrap_err().to_string().contains("0x000d"),
        "denial must be Forbidden"
    );
}
#[tokio::test]
async fn unauthenticated_tcp_admin_v1_cannot_stop_a_local_actor() {
    let (client, server, victim) = plain_pair().await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.admin_stop("protected", "/user/victim"),
    )
    .await
    .unwrap();
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    assert!(
        victim.alive.load(Ordering::SeqCst),
        "untrusted peer stopped local actor"
    );
    assert!(result.is_err(), "unauthenticated command succeeded");
}
#[tokio::test]
async fn raw_stop_cannot_bypass_control_authorization() {
    let (client, server, victim) = plain_pair().await;
    let sender = client.links_snapshot().await[0].1.clone();
    sender
        .send(parrot_remote::frame::Frame::stop(
            "parrot://protected/user/victim",
        ))
        .await
        .unwrap();
    let rejected = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! { _=victim.stopped.notified()=>false, _=sender.closed()=>true }
    })
    .await
    .unwrap();
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    assert!(
        rejected && victim.alive.load(Ordering::SeqCst),
        "raw STOP executed without authorization"
    );
}
#[tokio::test]
async fn quic_requires_credentials_or_explicit_insecure_development_choice() {
    let victim = Arc::new(Victim {
        alive: AtomicBool::new(true),
        stopped: tokio::sync::Notify::new(),
    });
    let result = RemoteActorSystem::new(
        RemoteConfig::quic("unconfigured", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Lookup(victim)),
    );
    assert!(
        result.is_err(),
        "QUIC silently installed a skip-verification client"
    );
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
fn config(scheme: &str, name: &str) -> RemoteConfig {
    let bind = Some("127.0.0.1:0".parse().unwrap());
    if scheme == "tcp" {
        RemoteConfig::tcp(name, bind)
    } else {
        RemoteConfig::quic(name, bind)
    }
}
fn victim() -> Arc<Victim> {
    Arc::new(Victim {
        alive: AtomicBool::new(true),
        stopped: tokio::sync::Notify::new(),
    })
}
fn system(cfg: RemoteConfig, victim: &Arc<Victim>) -> Arc<RemoteActorSystem> {
    RemoteActorSystem::new(cfg, Arc::new(Lookup(victim.clone()))).unwrap()
}
fn address(system: &RemoteActorSystem, scheme: &str, name: &str) -> NodeAddr {
    NodeAddr {
        node_id: name.into(),
        identity_is_explicit: true,
        scheme: scheme.into(),
        addr: system.local_addr().unwrap(),
    }
}

async fn verified_control(scheme: &str, authorized: bool) {
    let pki = Pki::new();
    let victim = victim();
    let admins = if authorized {
        vec!["operator.test".into()]
    } else {
        Vec::new()
    };
    let server = system(
        config(scheme, "server.test").with_mtls(pki.node("server.test"), admins),
        &victim,
    );
    let client = system(
        config(scheme, "operator.test").with_mtls(pki.node("operator.test"), []),
        &victim,
    );
    let executor = Arc::new(Executor(AtomicUsize::new(0)));
    server.set_component_executor(Some(executor.clone()));
    server.start().await.unwrap();
    client.start().await.unwrap();
    client
        .connect(&address(&server, scheme, "server.test"))
        .await
        .unwrap();
    assert_eq!(
        client.links_snapshot().await[0].1.authentication(),
        parrot_remote::PeerAuthentication::MutualTls
    );
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.stop_component("server.test", "/user"),
    )
    .await
    .unwrap();
    assert_eq!(result.is_ok(), authorized, "{scheme}: {result:?}");
    assert_eq!(executor.0.load(Ordering::SeqCst), usize::from(authorized));
    let result = client.admin_stop("server.test", "/user/victim").await;
    assert_eq!(result.is_ok(), authorized, "{scheme}: {result:?}");
    assert_eq!(victim.alive.load(Ordering::SeqCst), !authorized);
    // Raw STOP must use the same privilege boundary, even after valid mTLS.
    victim.alive.store(true, Ordering::SeqCst);
    let sender = client.links_snapshot().await[0].1.clone();
    sender
        .send(parrot_remote::frame::Frame::stop(
            "parrot://server.test/user/victim",
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        if authorized {
            while victim.alive.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        } else {
            sender.closed().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(victim.alive.load(Ordering::SeqCst), !authorized);
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}
#[tokio::test]
async fn mtls_admin_allowlist_is_enforced_on_tcp() {
    verified_control("tcp", false).await;
    verified_control("tcp", true).await;
}
#[tokio::test]
async fn mtls_admin_allowlist_is_enforced_on_quic() {
    verified_control("quic", false).await;
    verified_control("quic", true).await;
}

async fn reject_untrusted(scheme: &str) {
    let pki = Pki::new();
    let victim = victim();
    let server = system(
        config(scheme, "server.test").with_mtls(pki.node("server.test"), ["operator.test".into()]),
        &victim,
    );
    server.start().await.unwrap();
    // Client trusts server CA, but offers a client certificate from an unrelated CA.
    let mut bad_client = Pki::new().node("operator.test");
    bad_client.ca_pem = pki.ca.clone();
    let client = system(
        config(scheme, "operator.test").with_mtls(bad_client, []),
        &victim,
    );
    client.start().await.unwrap();
    assert!(client
        .connect(&address(&server, scheme, "server.test"))
        .await
        .is_err());
    assert!(client.links_snapshot().await.is_empty());
    assert!(server.links_snapshot().await.is_empty());
    client.shutdown().await.unwrap();
    // Also reject the server when the client has an unrelated trust root.
    let mut bad_trust = pki.node("operator.test");
    bad_trust.ca_pem = Pki::new().ca;
    let client = system(
        config(scheme, "operator.test").with_mtls(bad_trust, []),
        &victim,
    );
    client.start().await.unwrap();
    assert!(client
        .connect(&address(&server, scheme, "server.test"))
        .await
        .is_err());
    assert!(client.links_snapshot().await.is_empty());
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}
#[tokio::test]
async fn tcp_rejects_untrusted_client_and_server_certificates() {
    reject_untrusted("tcp").await;
}
#[tokio::test]
async fn quic_rejects_untrusted_client_and_server_certificates() {
    reject_untrusted("quic").await;
}

#[tokio::test]
async fn tcp_verified_certificate_cannot_claim_an_admin_node_name() {
    use parrot_remote::{
        handshake::HandshakeBody,
        transport::{run_connection, ConnParams, ConnSide},
    };
    let pki = Pki::new();
    let victim = victim();
    let server = system(
        config("tcp", "server.test").with_mtls(pki.node("server.test"), ["operator.test".into()]),
        &victim,
    );
    server.start().await.unwrap();
    let stream = tokio::net::TcpStream::connect(server.local_addr().unwrap())
        .await
        .unwrap();
    let connector = parrot_remote::tls::client_connector(&pki.node("ordinary.test")).unwrap();
    let tls = connector
        .connect("server.test".try_into().unwrap(), stream)
        .await
        .unwrap();
    let (inbound, _rx) = tokio::sync::mpsc::channel(8);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let result = run_connection(
        tls,
        ConnParams {
            side: ConnSide::Connect,
            local_addr: None,
            peer_addr: server.local_addr(),
            scheme: "tcp",
            local_handshake: HandshakeBody {
                node_id: "operator.test".into(),
                ..Default::default()
            },
            inbound,
            on_disconnect: Arc::new(|_| {}),
            shutdown: shutdown.subscribe(),
            knobs: None,
        },
    )
    .await;
    assert!(
        result.is_err(),
        "certificate identity was replaced by HELLO claim"
    );
    assert!(server.links_snapshot().await.is_empty());
    assert!(victim.alive.load(Ordering::SeqCst));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn plaintext_cannot_connect_to_mtls_listener() {
    let pki = Pki::new();
    let victim = victim();
    let server = system(
        config("tcp", "server.test").with_mtls(pki.node("server.test"), []),
        &victim,
    );
    let client = system(config("tcp", "ordinary.test"), &victim);
    server.start().await.unwrap();
    client.start().await.unwrap();
    assert!(client
        .connect(&address(&server, "tcp", "server.test"))
        .await
        .is_err());
    assert!(server.links_snapshot().await.is_empty());
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn default_rejects_external_bind_and_dial_without_credentials() {
    let victim = victim();
    assert!(RemoteActorSystem::new(
        RemoteConfig::tcp("local", Some("0.0.0.0:0".parse().unwrap())),
        Arc::new(Lookup(victim.clone()))
    )
    .is_err());
    let client = system(RemoteConfig::tcp("local", None), &victim);
    let result = client
        .connect(&NodeAddr::tcp(
            "external",
            "192.0.2.1:9000".parse().unwrap(),
        ))
        .await;
    assert!(result.unwrap_err().to_string().contains("non-loopback"));
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn insecure_development_is_an_explicit_working_choice_for_both_transports() {
    for scheme in ["tcp", "quic"] {
        let victim = victim();
        let server = system(
            config(scheme, "dev-server").with_insecure_development(),
            &victim,
        );
        let client = system(
            config(scheme, "dev-client").with_insecure_development(),
            &victim,
        );
        server.start().await.unwrap();
        client.start().await.unwrap();
        client
            .connect(&address(&server, scheme, "dev-server"))
            .await
            .unwrap();
        client
            .admin_stop("dev-server", "/user/victim")
            .await
            .unwrap();
        assert!(!victim.alive.load(Ordering::SeqCst));
        assert_eq!(
            client.links_snapshot().await[0].1.authentication(),
            parrot_remote::PeerAuthentication::Unauthenticated
        );
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn quic_verified_certificate_cannot_claim_an_admin_node_name() {
    use parrot_remote::{
        handshake::HandshakeBody,
        transport::{
            quic::{QuicCrypto, QuicTransport, ALPN_PARROT_1},
            Transport,
        },
    };
    let pki = Pki::new();
    let victim = victim();
    let server = system(
        config("quic", "server.test").with_mtls(pki.node("server.test"), ["operator.test".into()]),
        &victim,
    );
    server.start().await.unwrap();
    // Direct custom crypto simulates a peer that bypasses local config validation.
    let cert = pki.node("ordinary.test");
    let mut client_tls = parrot_remote::tls::client_config(&cert).unwrap();
    // QuicTransport::new is a custom development transport using localhost SNI.
    // Issue the server a certificate with localhost SAN while retaining its real CN.
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".into(), "server.test".into()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "server.test");
    let key = rcgen::KeyPair::generate().unwrap();
    let server_cert = parrot_remote::tls::TlsConfig {
        cert_pem: params
            .signed_by(&key, &pki.issuer)
            .unwrap()
            .pem()
            .into_bytes(),
        key_pem: key.serialize_pem().into_bytes(),
        ca_pem: pki.ca.clone(),
    };
    server.shutdown().await.unwrap();
    let server = system(
        config("quic", "server.test").with_mtls(server_cert, ["operator.test".into()]),
        &victim,
    );
    server.start().await.unwrap();
    let mut server_tls = parrot_remote::tls::server_config(&cert).unwrap();
    client_tls.alpn_protocols = vec![ALPN_PARROT_1.to_vec()];
    server_tls.alpn_protocols = vec![ALPN_PARROT_1.to_vec()];
    let crypto = QuicCrypto {
        server: quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_tls).unwrap(),
        )),
        client: quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_tls).unwrap(),
        )),
    };
    let (inbound, _rx) = tokio::sync::mpsc::channel(8);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let attacker = QuicTransport::new(
        HandshakeBody {
            node_id: "operator.test".into(),
            ..Default::default()
        },
        inbound,
        Arc::new(|_| {}),
        shutdown,
        "127.0.0.1:0".parse().unwrap(),
        crypto,
        None,
    )
    .unwrap();
    assert!(attacker
        .connect(&address(&server, "quic", "server.test"))
        .await
        .is_err());
    assert!(server.links_snapshot().await.is_empty());
    assert!(victim.alive.load(Ordering::SeqCst));
    attacker.shutdown().await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn requested_node_identity_is_checked_before_publishing_a_plain_connection() {
    let victim = victim();
    let server = system(config("tcp", "actual-node"), &victim);
    let client = system(config("tcp", "client"), &victim);
    server.start().await.unwrap();
    client.start().await.unwrap();
    let result = client.connect(&address(&server, "tcp", "wrong-node")).await;
    assert!(result.unwrap_err().to_string().contains("expected node"));
    assert!(client.links_snapshot().await.is_empty());
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn unauthenticated_cluster_updates_are_rejected_without_mutating_routes() {
    for gossip in [false, true] {
        let (client, server, _) = plain_pair().await;
        let sender = client.links_snapshot().await[0].1.clone();
        let mut frame =
            parrot_remote::frame::Frame::try_route_hint("forged-node", "127.0.0.1:1").unwrap();
        if gossip {
            frame.header.frame_type = parrot_remote::frame::frame_type::SYSTEM_EVENT;
            frame.payload = bytes::Bytes::from_static(&[
                parrot_remote::admin::sys_event_tag::MEMBERSHIP_GOSSIP,
            ]);
        }
        sender.send(frame).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), sender.closed())
            .await
            .unwrap();
        assert!(server.ingress.learned.lock().unwrap().is_empty());
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn malformed_hello_identity_is_rejected_before_ack_and_registration() {
    use futures::{SinkExt, StreamExt};
    let victim = victim();
    let server = system(config("tcp", "server.test"), &victim);
    server.start().await.unwrap();
    for name in ["", "bad/name", "bad name", "bad@name"] {
        let socket = tokio::net::TcpStream::connect(server.local_addr().unwrap())
            .await
            .unwrap();
        let mut wire = tokio_util::codec::Framed::new(socket, parrot_remote::transport::FrameCodec);
        let mut body = bytes::BytesMut::new();
        // Construct hostile wire bytes independently: the production encoder
        // now rejects empty identities before they can reach the peer.
        body.extend_from_slice(&[1]);
        body.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        body.extend_from_slice(name.as_bytes());
        body.extend_from_slice(&[
            4, 4, 0, 1, 0, 0, 0, // capabilities = bin
            5, 4, 0, 0, 0, 16, 0, // max frame = 1 MiB
            6, 1, 0, 0, // normal topology
            7, 1, 0, 8, // hop limit
        ]);
        wire.send(parrot_remote::frame::Frame::handshake(body.freeze()))
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), wire.next())
            .await
            .unwrap();
        assert!(
            !matches!(response, Some(Ok(_))),
            "invalid identity {name:?} acknowledged"
        );
        assert!(server.links_snapshot().await.is_empty());
    }
    server.shutdown().await.unwrap();
}
