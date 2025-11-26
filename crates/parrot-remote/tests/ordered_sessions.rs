use bytes::Bytes;
use parrot_api::{
    address::ActorRef,
    message::{MessageOptions, RemoteMessage},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    frame::{frame_type, Frame},
    handshake::{caps, HandshakeBody, TopologyRole},
    ordered::{CONTROL_KEY, DATA_KEY, RECEIPT_KEY},
    transport::{run_connection, ConnParams, ConnSide, ConnectionHandle, RuntimeKnobs},
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Work(u64);
impl parrot_api::Message for Work {
    type Result = ();
}

#[derive(Debug)]
struct State {
    seen: tokio::sync::mpsc::UnboundedSender<u64>,
    entered: tokio::sync::Notify,
    dropping: tokio::sync::Notify,
    dropped: AtomicBool,
    gate: (Mutex<bool>, Condvar),
}
fn state() -> (Arc<State>, tokio::sync::mpsc::UnboundedReceiver<u64>) {
    let (seen, receiver) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(State {
            seen,
            entered: tokio::sync::Notify::new(),
            dropping: tokio::sync::Notify::new(),
            dropped: AtomicBool::new(false),
            gate: (Mutex::new(false), Condvar::new()),
        }),
        receiver,
    )
}
struct Release(Arc<State>);
impl Release {
    fn release(&self) {
        *self.0.gate.0.lock().unwrap() = true;
        self.0.gate.1.notify_all();
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        self.release();
    }
}
struct HeldDrop(Arc<State>);
impl Future for HeldDrop {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.entered.notify_one();
        Poll::Pending
    }
}
impl Drop for HeldDrop {
    fn drop(&mut self) {
        self.0.dropping.notify_one();
        let mut released = self.0.gate.0.lock().unwrap();
        while !*released {
            released = self.0.gate.1.wait(released).unwrap();
        }
        self.0.dropped.store(true, Ordering::SeqCst);
    }
}
#[derive(Clone, Debug)]
struct Target(Arc<State>);
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
#[async_trait::async_trait]
impl LocalLookup for Target {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/target").then(|| Box::new(self.clone()) as BoxedActorRef)
    }
}
impl ActorRef for Target {
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        let value = message.downcast_ref::<Work>().unwrap().0;
        if value == 700 {
            return Box::pin(HeldDrop(self.0.clone()));
        }
        Box::pin(async move {
            self.0.seen.send(value).unwrap();
            Ok(())
        })
    }
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/target".into()
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
async fn start(mut config: RemoteConfig, lookup: Arc<dyn LocalLookup>) -> Arc<RemoteActorSystem> {
    config.heartbeat_enabled = false;
    let system = RemoteActorSystem::new(config, lookup).unwrap();
    system.start().await.unwrap();
    system
}
async fn seen(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<u64>, value: u64) {
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .unwrap(),
        Some(value)
    );
}
async fn strict(reference: &parrot_remote::RemoteActorRef, value: u64) {
    reference
        .deliver_ordered(
            Box::new(Work(value)),
            MessageOptions {
                timeout: Some(Duration::from_secs(4)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn authenticated_memory_hub_keeps_sources_independent_and_closes_sessions() {
    let (state, mut receiver) = state();
    let hub = start(
        RemoteConfig::mem("ordered-hub").with_role(TopologyRole::Hub),
        Arc::new(Empty),
    )
    .await;
    let target = start(
        RemoteConfig::mem("ordered-target").with_ordered_relays(["ordered-hub".into()]),
        Arc::new(Target(state)),
    )
    .await;
    let first = start(RemoteConfig::mem("ordered-first"), Arc::new(Empty)).await;
    let second = start(RemoteConfig::mem("ordered-second"), Arc::new(Empty)).await;
    for source in [&first, &second, &target] {
        source.connect_mem_pair(&hub).await.unwrap();
    }
    let a = first.remote_ref("parrot://ordered-target/target").unwrap();
    let b = second.remote_ref("parrot://ordered-target/target").unwrap();
    for value in 1..=5 {
        strict(&a, value).await;
        strict(&b, value + 10).await;
    }
    let mut actual = Vec::new();
    for _ in 0..10 {
        actual.push(
            tokio::time::timeout(Duration::from_secs(3), receiver.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert_eq!(
        actual
            .iter()
            .copied()
            .filter(|&value| value < 10)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(
        actual
            .iter()
            .copied()
            .filter(|&value| value > 10)
            .collect::<Vec<_>>(),
        vec![11, 12, 13, 14, 15]
    );
    a.close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    strict(&a, 20).await;
    seen(&mut receiver, 20).await;
    for node in [&first, &second, &hub, &target] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn strict_delivery_rejects_missing_relay_grants_but_ordinary_delivery_can_use_legacy() {
    let (state, mut receiver) = state();
    let hub = start(
        RemoteConfig::mem("ungranted-hub").with_role(TopologyRole::Hub),
        Arc::new(Empty),
    )
    .await;
    let target = start(
        RemoteConfig::mem("ungranted-target"),
        Arc::new(Target(state)),
    )
    .await;
    let source = start(RemoteConfig::mem("ungranted-source"), Arc::new(Empty)).await;
    source.connect_mem_pair(&hub).await.unwrap();
    target.connect_mem_pair(&hub).await.unwrap();
    let reference = source
        .remote_ref("parrot://ungranted-target/target")
        .unwrap();
    assert!(reference
        .deliver_ordered(Box::new(Work(1)), Default::default())
        .await
        .is_err());
    reference.deliver(Box::new(Work(2))).await.unwrap();
    seen(&mut receiver, 2).await;
    assert!(receiver.try_recv().is_err());
    for node in [&source, &hub, &target] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn plain_tcp_falls_back_without_claiming_authenticated_ordering() {
    let (state, mut receiver) = state();
    let target = start(
        RemoteConfig::tcp("plain-order-target", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Target(state)),
    )
    .await;
    let source = start(
        RemoteConfig::tcp("plain-order-source", None),
        Arc::new(Empty),
    )
    .await;
    source
        .connect(&NodeAddr::tcp(
            "plain-order-target",
            target.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let reference = source
        .remote_ref("parrot://plain-order-target/target")
        .unwrap();
    assert!(reference
        .deliver_ordered(Box::new(Work(1)), Default::default())
        .await
        .is_err());
    assert!(source.callbacks.is_empty());
    reference.deliver(Box::new(Work(2))).await.unwrap();
    seen(&mut receiver, 2).await;
    assert!(receiver.try_recv().is_err());
    for node in [&source, &target] {
        node.shutdown().await.unwrap();
    }
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
async fn strict_sessions_execute_over_real_mtls_tcp_and_quic() {
    let pki = Pki::new();
    for scheme in ["tcp", "quic"] {
        let (state, mut receiver) = state();
        let source_name = format!("ordered-source-{scheme}");
        let target_name = format!("ordered-target-{scheme}");
        let config = |name: &str| {
            let bind = Some("127.0.0.1:0".parse().unwrap());
            let config = if scheme == "tcp" {
                RemoteConfig::tcp(name, bind)
            } else {
                RemoteConfig::quic(name, bind)
            };
            config.with_mtls(pki.node(name), [])
        };
        let source = start(config(&source_name), Arc::new(Empty)).await;
        let target = start(config(&target_name), Arc::new(Target(state))).await;
        let address = if scheme == "tcp" {
            NodeAddr::tcp(&target_name, target.local_addr().unwrap())
        } else {
            NodeAddr::quic(&target_name, target.local_addr().unwrap())
        };
        source.connect(&address).await.unwrap();
        let reference = source
            .remote_ref(&format!("parrot://{target_name}/target"))
            .unwrap();
        for value in 1..=3 {
            strict(&reference, value).await;
            seen(&mut receiver, value).await;
        }
        reference
            .close_ordered_session(Duration::from_secs(3))
            .await
            .unwrap();
        source.shutdown().await.unwrap();
        target.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn idle_expiry_and_receiver_recreation_get_new_leases_instead_of_reusing_sequence_state() {
    let (state, mut receiver) = state();
    let source = start(RemoteConfig::mem("expiry-source"), Arc::new(Empty)).await;
    let target = start(
        RemoteConfig::mem("expiry-target"),
        Arc::new(Target(state.clone())),
    )
    .await;
    source.connect_mem_pair(&target).await.unwrap();
    let reference = source.remote_ref("parrot://expiry-target/target").unwrap();
    strict(&reference, 1).await;
    seen(&mut receiver, 1).await;
    tokio::time::advance(Duration::from_secs(31)).await;
    strict(&reference, 2).await;
    seen(&mut receiver, 2).await;
    target.shutdown().await.unwrap();
    drop(target);
    let target = start(RemoteConfig::mem("expiry-target"), Arc::new(Target(state))).await;
    source.connect_mem_pair(&target).await.unwrap();
    strict(&reference, 3).await;
    seen(&mut receiver, 3).await;
    reference
        .close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_recreation_and_cancelled_intermediate_handoff_cannot_bypass_actual_old_drop() {
    let (state, mut receiver) = state();
    let release = Release(state.clone());
    let target = start(
        RemoteConfig::mem("handoff-target"),
        Arc::new(Target(state.clone())),
    )
    .await;
    let first = start(RemoteConfig::mem("handoff-source"), Arc::new(Empty)).await;
    first.connect_mem_pair(&target).await.unwrap();
    let reference = first.remote_ref("parrot://handoff-target/target").unwrap();
    strict(&reference, 700).await;
    tokio::time::timeout(Duration::from_secs(3), state.entered.notified())
        .await
        .unwrap();
    first.shutdown().await.unwrap();
    drop(first);
    let second = start(RemoteConfig::mem("handoff-source"), Arc::new(Empty)).await;
    second.connect_mem_pair(&target).await.unwrap();
    let next = second.remote_ref("parrot://handoff-target/target").unwrap();
    let mut attempt = tokio::spawn(async move {
        next.deliver_ordered(Box::new(Work(1)), Default::default())
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), state.dropping.notified())
        .await
        .unwrap();
    assert!(
        !attempt.is_finished(),
        "COMMIT acknowledged while actual old future Drop was blocked"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut attempt)
            .await
            .is_err(),
        "COMMIT completed before the old future's real destructor finished"
    );
    second.shutdown().await.unwrap();
    assert!(attempt.await.unwrap().is_err());
    drop(second);
    let third = start(RemoteConfig::mem("handoff-source"), Arc::new(Empty)).await;
    third.connect_mem_pair(&target).await.unwrap();
    let next = third.remote_ref("parrot://handoff-target/target").unwrap();
    assert!(
        next.deliver_ordered(Box::new(Work(2)), Default::default())
            .await
            .is_err(),
        "a third incarnation bypassed old cleanup"
    );
    assert!(!state.dropped.load(Ordering::SeqCst));
    assert!(receiver.try_recv().is_err());
    release.release();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state.dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match next
                .deliver_ordered(Box::new(Work(3)), Default::default())
                .await
            {
                Ok(()) => break,
                Err(error) if error.to_string().contains("remote overloaded") => {
                    tokio::task::yield_now().await
                }
                Err(error) => panic!("unexpected handoff recovery failure: {error}"),
            }
        }
    })
    .await
    .unwrap();
    seen(&mut receiver, 3).await;
    assert!(receiver.try_recv().is_err());
    third.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

struct Raw {
    handle: ConnectionHandle,
    incoming: tokio::sync::mpsc::Receiver<(Frame, parrot_remote::transport::FrameSender, String)>,
    shutdown: tokio::sync::watch::Sender<bool>,
    cid: u64,
}
impl Raw {
    async fn connect(target: &RemoteActorSystem) -> Self {
        let io = tokio::net::TcpStream::connect(target.local_addr().unwrap())
            .await
            .unwrap();
        let (inbound, incoming) = tokio::sync::mpsc::channel(64);
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let handle = run_connection(
            io,
            ConnParams {
                side: ConnSide::Connect,
                local_addr: None,
                peer_addr: None,
                scheme: "tcp",
                local_handshake: HandshakeBody {
                    node_id: "wire-source".into(),
                    capabilities: caps::BIN | caps::ORDERED_TELL,
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
            handle,
            incoming,
            shutdown,
            cid: 0,
        }
    }
    async fn control(
        &mut self,
        operation: u8,
        incarnation: u8,
        stream: u8,
        ticket: [u8; 16],
        source: &str,
    ) -> Frame {
        let mut payload = vec![1, operation];
        payload.extend_from_slice(&[incarnation; 16]);
        payload.extend_from_slice(&[stream; 16]);
        payload.extend_from_slice(&ticket);
        payload.extend_from_slice(&(source.len() as u16).to_le_bytes());
        payload.extend_from_slice(source.as_bytes());
        self.cid += 1;
        self.handle
            .sender
            .send(Frame::ask(
                self.cid,
                "parrot://wire-target/target",
                CONTROL_KEY,
                payload.into(),
                None,
            ))
            .await
            .unwrap();
        let (reply, _, _) = tokio::time::timeout(Duration::from_secs(3), self.incoming.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.header.correlation_id, self.cid);
        reply
    }
    async fn data(&self, incarnation: u8, stream: u8, ticket: [u8; 16], sequence: u64, value: u64) {
        let source = "wire-source";
        let key = Work::TYPE_KEY;
        let mut payload = vec![1];
        payload.extend_from_slice(&[incarnation; 16]);
        payload.extend_from_slice(&[stream; 16]);
        payload.extend_from_slice(&ticket);
        payload.extend_from_slice(&sequence.to_le_bytes());
        payload.extend_from_slice(&(source.len() as u16).to_le_bytes());
        payload.extend_from_slice(&(key.len() as u16).to_le_bytes());
        payload.extend_from_slice(source.as_bytes());
        payload.extend_from_slice(key.as_bytes());
        payload
            .extend_from_slice(&parrot_api::message::serde_remote_serialize(&Work(value)).unwrap());
        self.handle
            .sender
            .send(Frame::tell(
                "parrot://wire-target/target",
                DATA_KEY,
                Bytes::from(payload),
            ))
            .await
            .unwrap();
    }
}
fn ticket(reply: &Frame, active: bool) -> [u8; 16] {
    assert_eq!(reply.header.frame_type, frame_type::REPLY);
    assert_eq!(reply.type_key, RECEIPT_KEY);
    assert_eq!(reply.payload.len(), 30);
    assert_eq!(&reply.payload[..2], &[1, u8::from(active)]);
    reply.payload[2..18].try_into().unwrap()
}
#[tokio::test]
async fn actual_wire_old_open_commit_close_and_data_cannot_retire_or_overwrite_the_new_ticket() {
    let (state, mut receiver) = state();
    let target = start(
        RemoteConfig::tcp("wire-target", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Target(state)),
    )
    .await;
    let mut raw = Raw::connect(&target).await;
    let forged = raw.control(0, 1, 2, [0; 16], "forged-origin").await;
    assert_eq!(forged.header.frame_type, frame_type::REPLY_ERR);
    assert_eq!(
        parrot_remote::error::decode_err_payload(&forged.payload)
            .unwrap()
            .0,
        parrot_remote::error::ErrCode::Forbidden
    );
    let old = ticket(&raw.control(0, 1, 2, [0; 16], "wire-source").await, false);
    assert_eq!(
        ticket(&raw.control(1, 1, 2, old, "wire-source").await, true),
        old
    );
    raw.data(1, 2, old, 2, 2).await;
    raw.data(1, 2, old, 2, 99).await;
    raw.data(1, 2, old, 1, 1).await;
    seen(&mut receiver, 1).await;
    seen(&mut receiver, 2).await;
    let new = ticket(&raw.control(0, 3, 4, [0; 16], "wire-source").await, false);
    assert_ne!(new, old);
    raw.data(1, 2, old, 3, 3).await;
    seen(&mut receiver, 3).await;
    assert_eq!(
        ticket(&raw.control(1, 3, 4, new, "wire-source").await, true),
        new
    );
    raw.data(1, 2, old, 4, 99).await;
    for operation in [1, 2] {
        assert_eq!(
            raw.control(operation, 1, 2, old, "wire-source")
                .await
                .header
                .frame_type,
            frame_type::REPLY_ERR
        );
    }
    raw.data(3, 4, new, 1, 5).await;
    seen(&mut receiver, 5).await;
    let stale_open = raw.control(0, 1, 2, [0; 16], "wire-source").await;
    let _ = ticket(&stale_open, false);
    raw.data(3, 4, new, 2, 6).await;
    seen(&mut receiver, 6).await;
    assert_eq!(
        ticket(&raw.control(2, 3, 4, new, "wire-source").await, false),
        new
    );
    raw.data(3, 4, new, 3, 99).await;
    let fresh = ticket(&raw.control(0, 5, 6, [0; 16], "wire-source").await, false);
    assert_ne!(fresh, new);
    let _ = ticket(&raw.control(1, 5, 6, fresh, "wire-source").await, true);
    raw.data(5, 6, fresh, 1, 7).await;
    seen(&mut receiver, 7).await;
    assert!(receiver.try_recv().is_err());
    raw.shutdown.send_replace(true);
    target.shutdown().await.unwrap();
}
