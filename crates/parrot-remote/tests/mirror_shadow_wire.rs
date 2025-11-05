//! Actual handshaken TCP peers: distinct mailbox receipts, bounded admission,
//! generation-independent loop prevention and strict destination boundaries.
use parrot_api::{
    address::ActorRef,
    message::RemoteMessage,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    frame::{flags, frame_type, Frame, MIRROR_ADMISSION_KEY},
    handshake::{caps, HandshakeBody},
    transport::{run_connection, ConnParams, ConnSide, ConnectionHandle, RuntimeKnobs},
    ErrCode, LocalLookup, RemoteActorSystem, RemoteConfig, RemoteSecurity,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Work(u64);
impl parrot_api::Message for Work {
    type Result = ();
}
#[derive(Debug, Default)]
struct State {
    mode: AtomicUsize,
    lookups: AtomicUsize,
    admissions: AtomicUsize,
    sends: AtomicUsize,
    active: AtomicUsize,
    dropped: AtomicUsize,
}
struct Active(Arc<State>);
impl Active {
    fn new(state: &Arc<State>) -> Self {
        state.active.fetch_add(1, Ordering::SeqCst);
        Self(state.clone())
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Debug, Clone)]
struct Target(Arc<State>);
impl ActorRef for Target {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.sends.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(parrot_api::errors::ActorError::Stopped) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        assert_eq!(message.downcast_ref::<Work>().unwrap().0, 41);
        Box::pin(async move {
            let _active = Active::new(&self.0);
            self.0.admissions.fetch_add(1, Ordering::SeqCst);
            match self.0.mode.load(Ordering::SeqCst) {
                1 => std::future::pending::<()>().await,
                2 => panic!("shadow admission panic"),
                _ => {}
            }
            Ok(())
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/shadow".into()
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
impl LocalLookup for Target {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        self.0.lookups.fetch_add(1, Ordering::SeqCst);
        if self.0.mode.load(Ordering::SeqCst) == 3 {
            let _active = Active::new(&self.0);
            std::future::pending::<()>().await;
        }
        (path == "/shadow").then(|| self.clone_boxed())
    }
    fn forwarding_target(&self, path: &str) -> Option<String> {
        (path == "/alias").then(|| "parrot://elsewhere/shadow".into())
    }
}
struct Peer {
    node: Arc<RemoteActorSystem>,
    state: Arc<State>,
    connection: ConnectionHandle,
    replies: tokio::sync::mpsc::Receiver<(Frame, parrot_remote::FrameSender, String)>,
    shutdown: tokio::sync::watch::Sender<bool>,
}
impl Peer {
    async fn new(authorized: bool, shadow_cap: bool) -> Self {
        let state = Arc::new(State::default());
        let mut config = RemoteConfig::tcp(
            format!("shadow-wire-{}", uuid::Uuid::new_v4()),
            Some("127.0.0.1:0".parse().unwrap()),
        );
        if authorized {
            config.security = RemoteSecurity::InsecureDevelopment;
        }
        let node = RemoteActorSystem::new(config, Arc::new(Target(state.clone()))).unwrap();
        node.start().await.unwrap();
        let stream = tokio::net::TcpStream::connect(node.local_addr().unwrap())
            .await
            .unwrap();
        let (inbound, replies) = tokio::sync::mpsc::channel(16);
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let connection = run_connection(
            stream,
            ConnParams {
                side: ConnSide::Connect,
                local_addr: None,
                peer_addr: None,
                scheme: "tcp",
                local_handshake: HandshakeBody {
                    node_id: format!("wire-peer-{}", uuid::Uuid::new_v4()),
                    capabilities: caps::BIN
                        | caps::TELL_COMPLETION
                        | if shadow_cap { caps::MIRROR_SHADOW } else { 0 },
                    ..Default::default()
                },
                inbound,
                on_disconnect: Arc::new(|_| {}),
                shutdown: shutdown.subscribe(),
                knobs: Some(RuntimeKnobs {
                    heartbeat_enabled: false,
                    ..Default::default()
                }),
            },
        )
        .await
        .unwrap();
        Self {
            node,
            state,
            connection,
            replies,
            shutdown,
        }
    }
    async fn reply(&mut self) -> Frame {
        tokio::time::timeout(Duration::from_secs(3), self.replies.recv())
            .await
            .unwrap()
            .unwrap()
            .0
    }
    async fn close(self) {
        self.shutdown.send_replace(true);
        self.connection.sender.close();
        tokio::time::timeout(Duration::from_secs(3), self.connection.closed)
            .await
            .unwrap()
            .unwrap();
        self.node.shutdown().await.unwrap();
        assert_eq!(self.state.active.load(Ordering::SeqCst), 0);
    }
}
fn frame(cid: u64, path: &str, budget: u32) -> Frame {
    let mut payload = budget.to_le_bytes().to_vec();
    payload.extend(parrot_api::message::serde_remote_serialize(&Work(41)).unwrap());
    let mut frame = Frame::tell_with_ack(cid, path, Work::TYPE_KEY, payload.into());
    frame.header.flags |= flags::MIRROR_SHADOW;
    frame
}
fn assert_error(frame: &Frame, code: ErrCode) {
    assert_eq!(frame.header.frame_type, frame_type::REPLY_ERR);
    assert_eq!(
        parrot_remote::error::decode_err_payload(&frame.payload)
            .unwrap()
            .0,
        code
    );
}
#[tokio::test]
async fn receipt_confirms_mailbox_admission_without_invoking_send() {
    let mut peer = Peer::new(true, true).await;
    peer.connection
        .sender
        .send(frame(11, "/shadow", 1000))
        .await
        .unwrap();
    let reply = peer.reply().await;
    assert_eq!(reply.header.correlation_id, 11);
    assert_eq!(reply.header.frame_type, frame_type::REPLY);
    assert_eq!(reply.type_key, MIRROR_ADMISSION_KEY);
    assert!(reply.payload.is_empty());
    assert_eq!(peer.state.admissions.load(Ordering::SeqCst), 1);
    assert_eq!(peer.state.sends.load(Ordering::SeqCst), 0);
    assert_eq!(peer.state.dropped.load(Ordering::SeqCst), 1);
    peer.close().await;
}
#[tokio::test]
async fn stalled_admission_and_lookup_have_receiver_owned_deadlines() {
    for mode in [1, 3] {
        let mut peer = Peer::new(true, true).await;
        peer.state.mode.store(mode, Ordering::SeqCst);
        peer.connection
            .sender
            .send(frame(12, "/shadow", 30))
            .await
            .unwrap();
        assert_error(&peer.reply().await, ErrCode::Timeout);
        assert_eq!(peer.state.active.load(Ordering::SeqCst), 0);
        assert_eq!(peer.state.dropped.load(Ordering::SeqCst), 1);
        peer.state.mode.store(0, Ordering::SeqCst);
        peer.connection
            .sender
            .send(frame(13, "/shadow", 1000))
            .await
            .unwrap();
        assert_eq!(peer.reply().await.type_key, MIRROR_ADMISSION_KEY);
        peer.close().await;
    }
}
#[tokio::test]
async fn shadow_admission_panic_is_isolated_and_never_acknowledged_as_success() {
    let mut peer = Peer::new(true, true).await;
    peer.state.mode.store(2, Ordering::SeqCst);
    peer.connection
        .sender
        .send(frame(14, "/shadow", 1000))
        .await
        .unwrap();
    assert_error(&peer.reply().await, ErrCode::ProtocolViolation);
    assert_eq!(peer.state.active.load(Ordering::SeqCst), 0);
    peer.state.mode.store(0, Ordering::SeqCst);
    peer.connection
        .sender
        .send(frame(15, "/shadow", 1000))
        .await
        .unwrap();
    assert_eq!(peer.reply().await.type_key, MIRROR_ADMISSION_KEY);
    peer.close().await;
}
#[tokio::test]
async fn qualified_foreign_destination_and_forwarding_alias_never_relay() {
    let mut peer = Peer::new(true, true).await;
    for path in [
        "/alias",
        "parrot://elsewhere/shadow",
        "/a/../shadow",
        "//shadow",
        "/",
    ] {
        peer.connection
            .sender
            .send(frame(20, path, 1000))
            .await
            .unwrap();
        assert_error(&peer.reply().await, ErrCode::ProtocolViolation);
    }
    assert_eq!(peer.state.lookups.load(Ordering::SeqCst), 0);
    assert_eq!(peer.state.admissions.load(Ordering::SeqCst), 0);
    peer.close().await;
}
#[tokio::test]
async fn unnegotiated_shadow_cannot_enter_the_writer_queue() {
    let peer = Peer::new(true, false).await;
    assert!(peer
        .connection
        .sender
        .send(frame(21, "/shadow", 1000))
        .await
        .is_err());
    assert_eq!(peer.state.admissions.load(Ordering::SeqCst), 0);
    peer.close().await;
}
#[tokio::test]
async fn negotiated_but_unauthorized_shadow_is_rejected_before_lookup() {
    let peer = Peer::new(false, true).await;
    peer.connection
        .sender
        .send(frame(22, "/shadow", 1000))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), peer.connection.sender.closed())
        .await
        .unwrap();
    assert_eq!(peer.state.lookups.load(Ordering::SeqCst), 0);
    peer.close().await;
}
#[test]
fn shadow_flag_rejects_ambiguous_envelopes_and_unbounded_receiver_budgets() {
    let good = frame(1, "/shadow", 1000);
    good.validate_payload_flags().unwrap();
    for extra in [flags::BATCH, flags::URGENT] {
        let mut bad = good.clone();
        bad.header.flags |= extra;
        assert!(bad.validate_payload_flags().is_err());
    }
    let mut bad = good.clone();
    bad.header.flags &= !flags::TELL_ACK;
    assert!(bad.validate_payload_flags().is_err());
    for kind in [frame_type::ASK, frame_type::REPLY, frame_type::STOP] {
        let mut bad = good.clone();
        bad.header.frame_type = kind;
        assert!(bad.validate_payload_flags().is_err());
    }
    for budget in [0, 60_001, u32::MAX] {
        assert!(frame(1, "/shadow", budget)
            .validate_payload_flags()
            .is_err());
    }
    let mut bad = good.clone();
    bad.payload = bytes::Bytes::from_static(&[1, 2, 3]);
    assert!(bad.validate_payload_flags().is_err());
    for key in [
        parrot_remote::durable::DURABLE_KEY,
        parrot_remote::roles::directory::client::READ_KEY,
        parrot_remote::roles::directory::client::CLIENT_KEY,
        parrot_remote::roles::directory::network::RAFT_KEY,
    ] {
        let mut bad = good.clone();
        bad.type_key = key.into();
        assert!(bad.validate_payload_flags().is_err());
    }
}
#[tokio::test]
async fn source_worker_rejects_old_peer_without_sending_an_unmarked_fallback() {
    let state = Arc::new(State::default());
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp(format!("shadow-source-{}", uuid::Uuid::new_v4()), None),
        Arc::new(Target(state)),
    )
    .unwrap();
    source.start().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let name = format!("old-shadow-{}", uuid::Uuid::new_v4());
    let (incoming, mut frames) = tokio::sync::mpsc::channel(8);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let watch = shutdown.subscribe();
    let old_name = name.clone();
    let accept = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        run_connection(
            stream,
            ConnParams {
                side: ConnSide::Accept,
                local_addr: None,
                peer_addr: None,
                scheme: "tcp",
                local_handshake: HandshakeBody {
                    node_id: old_name,
                    capabilities: caps::BIN | caps::TELL_COMPLETION,
                    ..Default::default()
                },
                inbound: incoming,
                on_disconnect: Arc::new(|_| {}),
                shutdown: watch,
                knobs: Some(RuntimeKnobs {
                    heartbeat_enabled: false,
                    ..Default::default()
                }),
            },
        )
        .await
        .unwrap()
    });
    source
        .connect(&parrot_remote::NodeAddr::tcp(&name, address))
        .await
        .unwrap();
    let old = accept.await.unwrap();
    let handle = source
        .register_wire_mirror(
            parrot_remote::mirror::WireMirrorPolicy::new(
                "/source",
                format!("parrot://{name}/shadow"),
            ),
            Default::default(),
        )
        .unwrap();
    let message: BoxedMessage = Box::new(Work(41));
    handle.offer_local("/source", &message);
    tokio::time::timeout(Duration::from_secs(3), async {
        while handle.stats().failed != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(handle.stats().delivered, 0);
    assert_eq!(source.callbacks.len(), 0);
    assert!(matches!(
        frames.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    handle.close();
    handle.wait_closed().await;
    shutdown.send_replace(true);
    old.sender.close();
    tokio::time::timeout(Duration::from_secs(3), old.closed)
        .await
        .unwrap()
        .unwrap();
    source.shutdown().await.unwrap();
}
#[tokio::test]
async fn shadow_worker_never_uses_an_available_hub_for_an_undiscovered_target() {
    let mut config = RemoteConfig::tcp(format!("direct-shadow-{}", uuid::Uuid::new_v4()), None);
    config.security = RemoteSecurity::InsecureDevelopment;
    let source =
        RemoteActorSystem::new(config, Arc::new(Target(Arc::new(State::default())))).unwrap();
    source.start().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let name = format!("shadow-hub-{}", uuid::Uuid::new_v4());
    let (incoming, mut frames) = tokio::sync::mpsc::channel(8);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let watch = shutdown.subscribe();
    let hub_name = name.clone();
    let accept = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        run_connection(
            stream,
            ConnParams {
                side: ConnSide::Accept,
                local_addr: None,
                peer_addr: None,
                scheme: "tcp",
                local_handshake: HandshakeBody {
                    node_id: hub_name,
                    topology_role: parrot_remote::handshake::TopologyRole::Hub,
                    capabilities: caps::BIN | caps::TELL_COMPLETION | caps::MIRROR_SHADOW,
                    ..Default::default()
                },
                inbound: incoming,
                on_disconnect: Arc::new(|_| {}),
                shutdown: watch,
                knobs: Some(RuntimeKnobs {
                    heartbeat_enabled: false,
                    ..Default::default()
                }),
            },
        )
        .await
        .unwrap()
    });
    source
        .connect(&parrot_remote::NodeAddr::tcp(&name, address))
        .await
        .unwrap();
    let hub = accept.await.unwrap();
    // A positive control proves that this physical connection is a usable uplink.
    let ordinary = source
        .remote_ref("parrot://undiscovered-shadow/shadow")
        .unwrap();
    ordinary.deliver(Box::new(Work(41))).await.unwrap();
    let ordinary_frame = tokio::time::timeout(Duration::from_secs(3), frames.recv())
        .await
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(ordinary_frame.header.frame_type, frame_type::TELL);
    assert_eq!(ordinary_frame.header.flags & flags::MIRROR_SHADOW, 0);
    let handle = source
        .register_wire_mirror(
            parrot_remote::mirror::WireMirrorPolicy::new(
                "/source",
                "parrot://undiscovered-shadow/shadow",
            ),
            Default::default(),
        )
        .unwrap();
    let message: BoxedMessage = Box::new(Work(41));
    handle.offer_local("/source", &message);
    tokio::time::timeout(Duration::from_secs(3), async {
        while handle.stats().failed != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(handle.stats().delivered, 0);
    assert_eq!(source.callbacks.len(), 0);
    assert!(
        matches!(
            frames.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "shadow escaped via the hub uplink"
    );
    handle.close();
    handle.wait_closed().await;
    shutdown.send_replace(true);
    hub.sender.close();
    tokio::time::timeout(Duration::from_secs(3), hub.closed)
        .await
        .unwrap()
        .unwrap();
    source.shutdown().await.unwrap();
}
async fn forged_shadow(capabilities: u32) {
    use futures::{SinkExt, StreamExt};
    use tokio::io::AsyncWriteExt;
    let state = Arc::new(State::default());
    let mut config = RemoteConfig::tcp(
        format!("shadow-forgery-{}", uuid::Uuid::new_v4()),
        Some("127.0.0.1:0".parse().unwrap()),
    );
    config.security = RemoteSecurity::InsecureDevelopment;
    config.knobs = Some(RuntimeKnobs {
        heartbeat_enabled: false,
        ..Default::default()
    });
    let node = RemoteActorSystem::new(config, Arc::new(Target(state.clone()))).unwrap();
    node.start().await.unwrap();
    let stream = tokio::net::TcpStream::connect(node.local_addr().unwrap())
        .await
        .unwrap();
    let mut framed = tokio_util::codec::Framed::new(stream, parrot_remote::transport::FrameCodec);
    let mut hello = bytes::BytesMut::new();
    HandshakeBody {
        node_id: "forged-shadow-peer".into(),
        capabilities,
        ..Default::default()
    }
    .encode_tlv(&mut hello)
    .unwrap();
    framed.send(Frame::handshake(hello.freeze())).await.unwrap();
    assert_eq!(
        framed.next().await.unwrap().unwrap().header.frame_type,
        frame_type::HANDSHAKE_ACK
    );
    let mut stream = framed.into_inner();
    let mut encoded = bytes::BytesMut::new();
    frame(71, "/shadow", 1000).encode(&mut encoded).unwrap();
    stream.write_all(&encoded).await.unwrap();
    let mut terminal = tokio_util::codec::Framed::new(stream, parrot_remote::transport::FrameCodec);
    let error = tokio::time::timeout(Duration::from_secs(3), terminal.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(error.header.frame_type, frame_type::ERROR);
    assert_eq!(
        parrot_remote::error::decode_err_payload(&error.payload)
            .unwrap()
            .0,
        ErrCode::ProtocolViolation
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(6), terminal.next())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(state.lookups.load(Ordering::SeqCst), 0);
    node.shutdown().await.unwrap();
}
#[tokio::test]
async fn physical_connection_close_retires_pending_shadow_admission() {
    let peer = Peer::new(true, true).await;
    peer.state.mode.store(1, Ordering::SeqCst);
    peer.connection
        .sender
        .send(frame(72, "/shadow", 60_000))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while peer.state.admissions.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(peer.state.active.load(Ordering::SeqCst), 1);
    let state = peer.state.clone();
    peer.close().await;
    assert_eq!(state.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn raw_peer_cannot_forge_an_unnegotiated_shadow_flag() {
    for capabilities in [
        caps::BIN | caps::TELL_COMPLETION,
        caps::BIN | caps::MIRROR_SHADOW,
    ] {
        forged_shadow(capabilities).await;
    }
}
