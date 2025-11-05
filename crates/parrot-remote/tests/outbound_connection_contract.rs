//! Request ownership is the physical connection, even when names/routes overlap.
use futures::{SinkExt, StreamExt};
use parrot_api::{
    address::ActorRef,
    message::RemoteMessage,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    frame::{frame_type, Frame},
    transport::FrameCodec,
    LocalLookup, RemoteActorSystem, RemoteConfig,
};
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
#[derive(Debug)]
struct Gate {
    entered: AtomicUsize,
    release: tokio::sync::Semaphore,
}
#[derive(Clone, Debug)]
struct Echo(Arc<Gate>);
impl Echo {
    fn new(blocked: bool) -> Self {
        let release = tokio::sync::Semaphore::new(0);
        if !blocked {
            release.close();
        }
        Self(Arc::new(Gate {
            entered: AtomicUsize::new(0),
            release,
        }))
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.0.entered.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
#[async_trait::async_trait]
impl ActorRef for Echo {
    fn send<'a>(&'a self, m: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.0.entered.fetch_add(1, Ordering::SeqCst);
            let _ = self.0.release.acquire().await;
            Ok(m)
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        m: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(m)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("unexpected TELL")
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
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        Some(self.clone_boxed())
    }
}
async fn node(name: &str, hub: bool, echo: Echo) -> Arc<RemoteActorSystem> {
    let mut cfg = RemoteConfig::mem(name);
    if hub {
        cfg.topology_role = parrot_remote::handshake::TopologyRole::Hub;
    }
    let sys = RemoteActorSystem::new(cfg, Arc::new(echo)).unwrap();
    sys.start().await.unwrap();
    sys
}
async fn links(sys: &RemoteActorSystem, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while sys.links_snapshot().await.len() != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn closing_old_connection_does_not_fail_request_on_new_connection() {
    let echo = Echo::new(true);
    let client = node("outbound-multi-client", false, Echo::new(false)).await;
    let server = node("outbound-multi-server", false, echo.clone()).await;
    client.connect_mem_pair(&server).await.unwrap();
    let old = client.links_snapshot().await[0].1.clone();
    client.connect_mem_pair(&server).await.unwrap();
    let reference = client
        .remote_ref("parrot://outbound-multi-server/user/echo")
        .unwrap();
    let request = tokio::spawn(async move { reference.send(Box::new(Value(42))).await });
    echo.entered().await;
    old.close();
    links(&client, 1).await;
    echo.0.release.close();
    let result = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .unwrap()
        .unwrap();
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    assert_eq!(
        result
            .expect("unrelated old connection must not fail this request")
            .downcast_ref::<Value>()
            .unwrap()
            .0,
        42
    );
}
#[tokio::test]
async fn hub_connection_loss_ends_request_addressed_to_another_node() {
    let echo = Echo::new(true);
    let client = node("outbound-route-client", false, Echo::new(false)).await;
    let hub = node("outbound-route-hub", true, Echo::new(false)).await;
    let server = node("outbound-route-server", false, echo.clone()).await;
    hub.connect_mem_pair(&server).await.unwrap();
    client.connect_mem_pair(&hub).await.unwrap();
    let reference = client
        .remote_ref("parrot://outbound-route-server/user/echo")
        .unwrap();
    let mut request = tokio::spawn(async move { reference.send(Box::new(Value(42))).await });
    echo.entered().await;
    client.links_snapshot().await[0].1.close();
    let result = tokio::time::timeout(Duration::from_millis(300), &mut request).await;
    let pending = client.callbacks.len();
    client.shutdown().await.unwrap();
    hub.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    if result.is_err() {
        let _ = request.await;
    }
    assert!(result
        .expect("physical hub loss must promptly finish a routed request")
        .unwrap()
        .is_err());
    assert_eq!(pending, 0);
}

type Peer = tokio_util::codec::Framed<tokio::net::TcpStream, FrameCodec>;
async fn peer(sys: &RemoteActorSystem, name: &str) -> Peer {
    let mut peer = tokio_util::codec::Framed::new(
        tokio::net::TcpStream::connect(sys.local_addr().unwrap())
            .await
            .unwrap(),
        FrameCodec,
    );
    let mut bytes = bytes::BytesMut::new();
    parrot_remote::handshake::HandshakeBody {
        node_id: name.into(),
        ..Default::default()
    }
    .encode_tlv(&mut bytes)
    .unwrap();
    peer.send(Frame::handshake(bytes.freeze())).await.unwrap();
    assert_eq!(
        peer.next().await.unwrap().unwrap().header.frame_type,
        frame_type::HANDSHAKE_ACK
    );
    peer
}
async fn business(peer: &mut Peer) -> Frame {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = peer.next().await.unwrap().unwrap();
            match frame.header.frame_type {
                frame_type::HEARTBEAT => peer.send(Frame::heartbeat_ack()).await.unwrap(),
                frame_type::HEARTBEAT_ACK => {}
                _ => return frame,
            }
        }
    })
    .await
    .unwrap()
}
fn encoded(v: u64) -> bytes::Bytes {
    parrot_api::message::serde_remote_serialize(&Value(v))
        .unwrap()
        .into()
}
#[tokio::test]
async fn malformed_error_payload_is_protocol_violation_without_consuming_other_callbacks() {
    use parrot_remote::{ErrCode, ReplyPayload};
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("error-contract", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Echo::new(false)),
    )
    .unwrap();
    client.start().await.unwrap();
    let mut remote = peer(&client, "error-peer").await;
    links(&client, 1).await;
    let sender = client.links_snapshot().await[0].1.clone();
    for payload in [vec![3, 0, 1, 0], vec![3, 0, 0, 1], vec![3, 0, 0, 0, 0xff]] {
        let cid = client.callbacks.next_cid();
        let other = client.callbacks.next_cid();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (other_tx, other_rx) = tokio::sync::oneshot::channel();
        client
            .callbacks
            .insert_on(cid, "error-peer", tx, &sender)
            .unwrap();
        client
            .callbacks
            .insert_on(other, "error-peer", other_tx, &sender)
            .unwrap();
        let mut response = Frame::reply_err(cid, "", ErrCode::Stopped, "placeholder");
        response.payload = payload.into();
        remote.send(response).await.unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(response, ReplyPayload::Err(ErrCode::ProtocolViolation, _)),
            "{response:?}"
        );
        assert_eq!(client.callbacks.len(), 1);
        remote
            .send(Frame::reply_err(other, "", ErrCode::Stopped, "valid"))
            .await
            .unwrap();
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(2), other_rx).await.unwrap().unwrap(), ReplyPayload::Err(ErrCode::Stopped, detail) if detail == "valid")
        );
        assert_eq!(client.callbacks.len(), 0);
        assert!(!sender.is_closed());
    }
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn same_named_wrong_connection_cannot_complete_a_known_correlation_id() {
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp(
            "outbound-reply-client",
            Some("127.0.0.1:0".parse().unwrap()),
        ),
        Arc::new(Echo::new(false)),
    )
    .unwrap();
    client.start().await.unwrap();
    let mut wrong = peer(&client, "same-service").await;
    links(&client, 1).await;
    let mut legitimate = peer(&client, "same-service").await;
    links(&client, 2).await;
    let reference = client
        .remote_ref("parrot://same-service/user/echo")
        .unwrap();
    let request = tokio::spawn(async move { reference.send(Box::new(Value(42))).await });
    let ask = business(&mut legitimate).await;
    assert_eq!(ask.header.frame_type, frame_type::ASK);
    let cid = ask.header.correlation_id;
    let mut observed_pending = Vec::new();
    for (i, forged) in [
        Frame::reply(cid, "", Value::TYPE_KEY, encoded(999)),
        Frame::reply_err(cid, "", parrot_remote::ErrCode::Stopped, "forged"),
    ]
    .into_iter()
    .enumerate()
    {
        wrong.send(forged).await.unwrap();
        // Same-socket roundtrip is an ordering barrier, not a timing guess.
        let probe = 9000 + i as u64;
        wrong
            .send(Frame::ask(
                probe,
                "/user/echo",
                Value::TYPE_KEY,
                encoded(7),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(business(&mut wrong).await.header.correlation_id, probe);
        observed_pending.push(client.callbacks.len());
    }
    legitimate
        .send(Frame::reply(
            ask.header.correlation_id,
            "",
            Value::TYPE_KEY,
            encoded(42),
        ))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    client.shutdown().await.unwrap();
    assert_eq!(
        observed_pending,
        vec![1, 1],
        "wrong source must not consume the slot"
    );
    assert_eq!(result.downcast_ref::<Value>().unwrap().0, 42);
}

async fn relay_empty(sys: &RemoteActorSystem) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !sys.ingress.relay_table.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnected relay ownership must be reclaimed without waiting for TTL");
}

#[tokio::test]
async fn relay_target_loss_fails_only_its_request_even_with_a_replacement_link() {
    let echo = Echo::new(true);
    let client = node("relay-loss-client", false, Echo::new(false)).await;
    let hub = node("relay-loss-hub", true, Echo::new(false)).await;
    let server = node("relay-loss-server", false, echo.clone()).await;
    hub.connect_mem_pair(&server).await.unwrap();
    hub.connect_mem_pair(&server).await.unwrap();
    let newest = hub.links_snapshot().await.last().unwrap().1.clone();
    client.connect_mem_pair(&hub).await.unwrap();
    let reference = client
        .remote_ref("parrot://relay-loss-server/user/echo")
        .unwrap();
    let mut request = tokio::spawn(async move { reference.send(Box::new(Value(42))).await });
    echo.entered().await;
    newest.close();
    let failed = tokio::time::timeout(Duration::from_secs(2), &mut request).await;
    let remaining = hub.ingress.relay_table.len();
    echo.0.release.close();
    let reference = client
        .remote_ref("parrot://relay-loss-server/user/echo")
        .unwrap();
    let recovered =
        tokio::time::timeout(Duration::from_secs(5), reference.send(Box::new(Value(43))))
            .await
            .unwrap();
    client.shutdown().await.unwrap();
    hub.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    if failed.is_err() {
        let _ = request.await;
    }
    assert!(failed
        .expect("closed target link must fail its relay despite another link")
        .unwrap()
        .is_err());
    assert_eq!(remaining, 0);
    assert_eq!(recovered.unwrap().downcast_ref::<Value>().unwrap().0, 43);
}

#[tokio::test]
async fn relay_source_loss_reclaims_mapping_without_closing_other_clients() {
    let echo = Echo::new(true);
    let source = node("relay-source-client", false, Echo::new(false)).await;
    let hub = node("relay-source-hub", true, Echo::new(false)).await;
    let target = node("relay-source-server", false, echo.clone()).await;
    hub.connect_mem_pair(&target).await.unwrap();
    source.connect_mem_pair(&hub).await.unwrap();
    let reference = source
        .remote_ref("parrot://relay-source-server/user/echo")
        .unwrap();
    let request = tokio::spawn(async move { reference.send(Box::new(Value(42))).await });
    echo.entered().await;
    source.links_snapshot().await[0].1.close();
    relay_empty(&hub).await;
    assert!(tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    let other = node("relay-source-other", false, Echo::new(false)).await;
    other.connect_mem_pair(&hub).await.unwrap();
    echo.0.release.close();
    let reference = other
        .remote_ref("parrot://relay-source-server/user/echo")
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), reference.send(Box::new(Value(43))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.downcast_ref::<Value>().unwrap().0, 43);
    source.shutdown().await.unwrap();
    other.shutdown().await.unwrap();
    hub.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn wrong_connection_cannot_consume_relay_mapping_with_success_or_error_reply() {
    let hub = RemoteActorSystem::new(
        RemoteConfig::tcp("relay-forgery-hub", Some("127.0.0.1:0".parse().unwrap()))
            .with_role(parrot_remote::handshake::TopologyRole::Hub),
        Arc::new(Echo::new(false)),
    )
    .unwrap();
    hub.start().await.unwrap();
    let mut wrong = peer(&hub, "relay-service").await;
    links(&hub, 1).await;
    let mut legitimate = peer(&hub, "relay-service").await;
    links(&hub, 2).await;
    let mut source = peer(&hub, "relay-source").await;
    links(&hub, 3).await;
    source
        .send(Frame::ask(
            55,
            "parrot://relay-service/user/echo",
            Value::TYPE_KEY,
            encoded(42),
            None,
        ))
        .await
        .unwrap();
    let forwarded = business(&mut legitimate).await;
    assert_eq!(forwarded.header.frame_type, frame_type::ASK);
    let cid = forwarded.header.correlation_id;
    for (i, forged) in [
        Frame::reply(cid, "", Value::TYPE_KEY, encoded(999)),
        Frame::reply_err(cid, "", parrot_remote::ErrCode::Stopped, "forged"),
    ]
    .into_iter()
    .enumerate()
    {
        wrong.send(forged).await.unwrap();
        let probe = 9000 + i as u64;
        wrong
            .send(Frame::ask(
                probe,
                "/user/echo",
                Value::TYPE_KEY,
                encoded(7),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(business(&mut wrong).await.header.correlation_id, probe);
        assert_eq!(
            hub.ingress.relay_table.len(),
            1,
            "wrong reply must leave mapping intact"
        );
    }
    legitimate
        .send(Frame::reply(cid, "", Value::TYPE_KEY, encoded(42)))
        .await
        .unwrap();
    let reply = business(&mut source).await;
    assert_eq!(reply.header.frame_type, frame_type::REPLY);
    assert_eq!(reply.header.correlation_id, 55);
    assert_eq!(
        parrot_api::message::serde_remote_deserialize::<Value>(&reply.payload)
            .unwrap()
            .0,
        42
    );
    assert!(hub.ingress.relay_table.is_empty());
    hub.shutdown().await.unwrap();
}
