//! Inbound admission and task ownership, tested using actual ingress and systems.
use parrot_api::message::RemoteMessage;
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    frame::Frame,
    ingress::{Ingress, LocalLookup},
    registry::CallbackRegistry,
    transport::FrameSender,
    ErrCode, RemoteActorSystem, RemoteConfig,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Query(u64);
impl parrot_api::Message for Query {
    type Result = Self;
}

#[derive(Debug)]
struct State {
    active: AtomicUsize,
    started: AtomicUsize,
    release: tokio::sync::Semaphore,
    block_lookup: bool,
    panic_at: AtomicUsize,
}
impl State {
    fn new(block_lookup: bool) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            started: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
            block_lookup,
            panic_at: AtomicUsize::new(0),
        })
    }
    async fn entered(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.started.load(Ordering::SeqCst) < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("requests must reach the controlled future");
    }
    async fn drained(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.active.load(Ordering::SeqCst) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("controlled futures must be dropped");
    }
}
struct Active(Arc<State>);
impl Active {
    fn new(state: Arc<State>) -> Self {
        state.active.fetch_add(1, Ordering::SeqCst);
        state.started.fetch_add(1, Ordering::SeqCst);
        Self(state)
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}
#[derive(Debug, Clone)]
struct Controlled(Arc<State>);
#[async_trait::async_trait]
impl ActorRef for Controlled {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert_ne!(
            self.0.panic_at.load(Ordering::SeqCst),
            2,
            "synchronous send panic"
        );
        Box::pin(async move {
            assert_ne!(
                self.0.panic_at.load(Ordering::SeqCst),
                3,
                "send future panic"
            );
            if !self.0.block_lookup {
                let _active = Active::new(self.0.clone());
                let _release = self.0.release.acquire().await;
            }
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
        panic!("ASK must not use TELL")
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/user/controlled".into()
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
impl LocalLookup for Controlled {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        assert_ne!(self.0.panic_at.load(Ordering::SeqCst), 1, "lookup panic");
        if self.0.block_lookup {
            let _active = Active::new(self.0.clone());
            let _release = self.0.release.acquire().await;
        }
        Some(self.clone_boxed())
    }
}
fn query_frame(cid: u64) -> Frame {
    Frame::ask(
        cid,
        "/user/controlled",
        Query::TYPE_KEY,
        parrot_api::message::serde_remote_serialize(&Query(cid))
            .unwrap()
            .into(),
        None,
    )
}

#[tokio::test]
async fn default_inbound_capacity_rejects_excess_before_starting_more_work() {
    let state = State::new(false);
    let ingress = Arc::new(Ingress::new(
        Arc::new(Controlled(state.clone())),
        Arc::new(CallbackRegistry::default()),
    ));
    let (tx, mut rx) = tokio::sync::mpsc::channel(2048);
    let back = FrameSender::anon(tx);
    for cid in 1..=1025 {
        ingress.dispatch(query_frame(cid), &back, "test-peer").await;
    }
    state.entered(1024).await;
    let response = tokio::time::timeout(Duration::from_millis(250), rx.recv()).await;
    let started = state.started.load(Ordering::SeqCst);
    state.release.close();
    state.drained().await;
    let response = response
        .expect("the 1025th request must receive Overloaded, not create another task")
        .unwrap();
    assert_eq!(response.header.correlation_id, 1025);
    assert_eq!(
        parrot_remote::error::decode_err_payload(&response.payload)
            .unwrap()
            .0,
        ErrCode::Overloaded
    );
    assert_eq!(started, 1024);
}

async fn shutdown_drops_inbound_future(block_lookup: bool) {
    let state = State::new(block_lookup);
    let name = if block_lookup { "lookup" } else { "actor" };
    let server_id = format!("inbound-{name}-server");
    let server = RemoteActorSystem::new(
        RemoteConfig::mem(&server_id),
        Arc::new(Controlled(state.clone())),
    )
    .unwrap();
    let client = RemoteActorSystem::new(
        RemoteConfig::mem(format!("inbound-{name}-client")),
        Arc::new(Controlled(State::new(false))),
    )
    .unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    client.connect_mem_pair(&server).await.unwrap();
    let reference = client
        .remote_ref(&format!("parrot://{server_id}/user/controlled"))
        .unwrap();
    let request = tokio::spawn(async move { reference.send(Box::new(Query(1))).await });
    state.entered(1).await;
    tokio::time::timeout(Duration::from_secs(5), server.shutdown())
        .await
        .unwrap()
        .unwrap();
    let still_active = state.active.load(Ordering::SeqCst);
    client.shutdown().await.unwrap();
    state.release.close(); // Also clean up the old, broken implementation before asserting.
    state.drained().await;
    let result = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    assert_eq!(
        still_active, 0,
        "shutdown returned with an owned inbound future still alive"
    );
}
#[tokio::test]
async fn shutdown_joins_pending_actor_request() {
    shutdown_drops_inbound_future(false).await;
}
#[tokio::test]
async fn shutdown_joins_pending_lookup() {
    shutdown_drops_inbound_future(true).await;
}

fn limited_ingress(state: &Arc<State>, capacity: usize, bytes: usize) -> Arc<Ingress> {
    Arc::new(
        Ingress::with_ask_limits(
            Arc::new(Controlled(state.clone())),
            Arc::new(CallbackRegistry::default()),
            capacity,
            bytes,
        )
        .unwrap(),
    )
}
async fn receive(rx: &mut tokio::sync::mpsc::Receiver<Frame>) -> Frame {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
fn assert_error(frame: Frame, cid: u64, code: ErrCode) {
    assert_eq!(
        frame.header.frame_type,
        parrot_remote::frame::frame_type::REPLY_ERR
    );
    assert_eq!(frame.header.correlation_id, cid);
    assert_eq!(
        parrot_remote::error::decode_err_payload(&frame.payload)
            .unwrap()
            .0,
        code
    );
}
async fn active_count(state: &State, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.active.load(Ordering::SeqCst) != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn encoded_byte_budget_is_shared_and_recovered_after_reply() {
    let state = State::new(false);
    let first = query_frame(1);
    let bytes = first.path.len() + first.type_key.len() + first.payload.len();
    let ingress = limited_ingress(&state, 3, bytes);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let back = FrameSender::anon(tx);
    ingress.dispatch(first, &back, "peer").await;
    state.entered(1).await;
    ingress.dispatch(query_frame(2), &back, "peer").await;
    assert_error(receive(&mut rx).await, 2, ErrCode::Overloaded);
    assert_eq!(state.started.load(Ordering::SeqCst), 1);
    state.release.add_permits(1);
    let reply = receive(&mut rx).await;
    assert_eq!(reply.header.correlation_id, 1);
    assert_eq!(
        parrot_api::message::serde_remote_deserialize::<Query>(&reply.payload)
            .unwrap()
            .0,
        1
    );
    ingress.dispatch(query_frame(3), &back, "peer").await;
    state.entered(2).await;
    state.release.add_permits(1);
    assert_eq!(receive(&mut rx).await.header.correlation_id, 3);
    ingress.close_asks();
    ingress.wait_for_asks().await;
}

#[tokio::test]
async fn decode_failure_and_all_handler_panic_phases_release_admission() {
    let state = State::new(false);
    state.release.close();
    let ingress = limited_ingress(&state, 1, 4096);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let back = FrameSender::anon(tx);
    // Keep the ASK reply_to prefix valid; corrupt only the encoded message.
    let invalid = Frame::ask(
        1,
        "/user/controlled",
        Query::TYPE_KEY,
        bytes::Bytes::new(),
        None,
    );
    ingress.dispatch(invalid, &back, "peer").await;
    assert_error(receive(&mut rx).await, 1, ErrCode::CodecError);
    for phase in 1..=3 {
        state.panic_at.store(phase, Ordering::SeqCst);
        ingress
            .dispatch(query_frame(phase as u64 + 1), &back, "peer")
            .await;
        assert_error(
            receive(&mut rx).await,
            phase as u64 + 1,
            ErrCode::ProtocolViolation,
        );
    }
    state.panic_at.store(0, Ordering::SeqCst);
    ingress.dispatch(query_frame(5), &back, "peer").await;
    let reply = receive(&mut rx).await;
    assert_eq!(
        reply.header.frame_type,
        parrot_remote::frame::frame_type::REPLY
    );
    assert_eq!(
        parrot_api::message::serde_remote_deserialize::<Query>(&reply.payload)
            .unwrap()
            .0,
        5
    );
    ingress.close_asks();
    ingress.wait_for_asks().await;
}

#[tokio::test]
async fn disconnect_cancels_only_its_own_requests_even_with_same_node_id() {
    let state = State::new(false);
    let ingress = limited_ingress(&state, 2, 4096);
    let (tx1, rx1) = tokio::sync::mpsc::channel(8);
    let (tx2, mut rx2) = tokio::sync::mpsc::channel(8);
    let old = FrameSender::new(tx1, "same-peer");
    let replacement = FrameSender::new(tx2, "same-peer");
    ingress.dispatch(query_frame(1), &old, "same-peer").await;
    ingress
        .dispatch(query_frame(2), &replacement, "same-peer")
        .await;
    state.entered(2).await;
    drop(rx1); // Actual writer loss, not just the explicit close API.
    active_count(&state, 1).await;
    assert!(!replacement.is_closed());
    state.release.add_permits(1);
    assert_eq!(receive(&mut rx2).await.header.correlation_id, 2);
    ingress.close_asks();
    ingress.wait_for_asks().await;
}

#[tokio::test]
async fn overload_with_full_reply_queue_disconnects_without_blocking_dispatch() {
    let state = State::new(false);
    let ingress = limited_ingress(&state, 1, 4096);
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let back = FrameSender::new(tx, "saturated-peer");
    ingress.dispatch(query_frame(1), &back, "peer").await;
    state.entered(1).await;
    back.send(Frame::heartbeat()).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        ingress.dispatch(query_frame(2), &back, "peer"),
    )
    .await
    .unwrap();
    assert!(
        back.is_closed(),
        "an unreportable rejection must make the connection fail"
    );
    state.drained().await;
    ingress.close_asks();
    ingress.wait_for_asks().await;
}

#[tokio::test]
async fn shutdown_cancels_a_reply_waiting_for_writer_capacity() {
    let state = State::new(false);
    state.release.close();
    let ingress = limited_ingress(&state, 1, 4096);
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let back = FrameSender::anon(tx);
    back.send(Frame::heartbeat()).await.unwrap();
    ingress.dispatch(query_frame(1), &back, "peer").await;
    state.entered(1).await;
    ingress.close_asks();
    tokio::time::timeout(Duration::from_secs(1), ingress.wait_for_asks())
        .await
        .unwrap();
    assert_eq!(
        Arc::strong_count(&ingress),
        1,
        "tracked task must release its owner"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_admission_and_close_cannot_leave_late_tasks() {
    let state = State::new(false);
    let ingress = limited_ingress(&state, 1024, 65536);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    let back = FrameSender::anon(tx);
    let barrier = Arc::new(tokio::sync::Barrier::new(101));
    let mut dispatchers = Vec::new();
    for cid in 1..=100 {
        let (ingress, back, barrier) = (ingress.clone(), back.clone(), barrier.clone());
        dispatchers.push(tokio::spawn(async move {
            barrier.wait().await;
            ingress.dispatch(query_frame(cid), &back, "peer").await;
        }));
    }
    barrier.wait().await;
    ingress.close_asks();
    ingress.wait_for_asks().await;
    let started_at_close = state.started.load(Ordering::SeqCst);
    for task in dispatchers {
        task.await.unwrap();
    }
    assert_eq!(state.active.load(Ordering::SeqCst), 0);
    assert_eq!(state.started.load(Ordering::SeqCst), started_at_close);
    ingress.dispatch(query_frame(101), &back, "peer").await;
    loop {
        let reply = receive(&mut rx).await;
        let cid = reply.header.correlation_id;
        assert_error(reply, cid, ErrCode::Stopped);
        if cid == 101 {
            break;
        }
    }
    assert_eq!(Arc::strong_count(&ingress), 1);
}

#[tokio::test]
async fn cancelling_shutdown_caller_still_fences_and_cancels_inbound_work() {
    use futures::FutureExt;
    let state = State::new(false);
    let system = RemoteActorSystem::new(
        RemoteConfig::mem("cancel-shutdown-caller"),
        Arc::new(Controlled(state.clone())),
    )
    .unwrap();
    system.start().await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let back = FrameSender::anon(tx);
    system.ingress.dispatch(query_frame(1), &back, "peer").await;
    state.entered(1).await;
    // Poll once, then drop while background handles/ASK tasks are being joined.
    let _ = system.shutdown().now_or_never();
    state.drained().await;
    system.ingress.dispatch(query_frame(2), &back, "peer").await;
    assert_error(receive(&mut rx).await, 2, ErrCode::Stopped);
    let (one, two) = tokio::join!(system.shutdown(), system.shutdown());
    one.unwrap();
    two.unwrap();
    assert_eq!(state.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn resolved_limits_reach_actual_system_admission() {
    for byte_limit in [false, true] {
        let state = State::new(false);
        let first = query_frame(1);
        let bytes = first.path.len() + first.type_key.len() + first.payload.len();
        let mut config = parrot_config::ParrotConfig::builder()
            .remote_node_id(if byte_limit {
                "configured-bytes"
            } else {
                "configured-count"
            })
            .remote_inbound_ask_capacity(if byte_limit { 4 } else { 1 })
            .remote_inbound_ask_bytes(if byte_limit { bytes } else { 4096 });
        config.remote.node.scheme = Some("mem".into());
        let resolved = config.build().unwrap();
        let system = RemoteActorSystem::new(
            RemoteConfig::from_resolved(&resolved).unwrap(),
            Arc::new(Controlled(state.clone())),
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let back = FrameSender::anon(tx);
        system.ingress.dispatch(first, &back, "peer").await;
        state.entered(1).await;
        system.ingress.dispatch(query_frame(2), &back, "peer").await;
        assert_error(receive(&mut rx).await, 2, ErrCode::Overloaded);
        assert_eq!(state.started.load(Ordering::SeqCst), 1);
        system.shutdown().await.unwrap();
        assert_eq!(state.active.load(Ordering::SeqCst), 0);
    }
}

type TcpPeer =
    tokio_util::codec::Framed<tokio::net::TcpStream, parrot_remote::transport::FrameCodec>;
async fn tcp_peer(system: &RemoteActorSystem) -> TcpPeer {
    use futures::{SinkExt, StreamExt};
    let mut peer = tokio_util::codec::Framed::new(
        tokio::net::TcpStream::connect(system.local_addr().unwrap())
            .await
            .unwrap(),
        parrot_remote::transport::FrameCodec,
    );
    let mut body = bytes::BytesMut::new();
    parrot_remote::handshake::HandshakeBody {
        node_id: "same-tcp-peer".into(),
        ..Default::default()
    }
    .encode_tlv(&mut body)
    .unwrap();
    peer.send(Frame::handshake(body.freeze())).await.unwrap();
    let ack = peer.next().await.unwrap().unwrap();
    assert_eq!(
        ack.header.frame_type,
        parrot_remote::frame::frame_type::HANDSHAKE_ACK
    );
    peer
}
async fn tcp_reply(peer: &mut TcpPeer) -> Frame {
    use futures::{SinkExt, StreamExt};
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = peer.next().await.unwrap().unwrap();
            if frame.header.frame_type == parrot_remote::frame::frame_type::HEARTBEAT {
                peer.send(Frame::heartbeat_ack()).await.unwrap();
            } else {
                return frame;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn tcp_overload_reports_exact_request_then_recovers_on_same_socket() {
    use futures::SinkExt;
    let state = State::new(false);
    let system = RemoteActorSystem::new(
        RemoteConfig::tcp("tcp-admission-server", Some("127.0.0.1:0".parse().unwrap())).with_knobs(
            parrot_remote::transport::RuntimeKnobs {
                inbound_ask_capacity: 1,
                ..Default::default()
            },
        ),
        Arc::new(Controlled(state.clone())),
    )
    .unwrap();
    system.start().await.unwrap();
    let mut peer = tcp_peer(&system).await;
    peer.send(query_frame(1)).await.unwrap();
    state.entered(1).await;
    peer.send(query_frame(2)).await.unwrap();
    assert_error(tcp_reply(&mut peer).await, 2, ErrCode::Overloaded);
    assert_eq!(state.started.load(Ordering::SeqCst), 1);
    state.release.add_permits(1);
    assert_eq!(tcp_reply(&mut peer).await.header.correlation_id, 1);
    peer.send(query_frame(3)).await.unwrap();
    state.entered(2).await;
    state.release.add_permits(1);
    let reply = tcp_reply(&mut peer).await;
    assert_eq!(reply.header.correlation_id, 3);
    assert_eq!(
        parrot_api::message::serde_remote_deserialize::<Query>(&reply.payload)
            .unwrap()
            .0,
        3
    );
    system.shutdown().await.unwrap();
}

#[tokio::test]
async fn tcp_old_connection_loss_does_not_cancel_replacement_request() {
    use futures::SinkExt;
    let state = State::new(false);
    let system = RemoteActorSystem::new(
        RemoteConfig::tcp(
            "tcp-replacement-server",
            Some("127.0.0.1:0".parse().unwrap()),
        )
        .with_knobs(parrot_remote::transport::RuntimeKnobs {
            inbound_ask_capacity: 2,
            ..Default::default()
        }),
        Arc::new(Controlled(state.clone())),
    )
    .unwrap();
    system.start().await.unwrap();
    let mut old = tcp_peer(&system).await;
    let mut replacement = tcp_peer(&system).await;
    old.send(query_frame(1)).await.unwrap();
    replacement.send(query_frame(2)).await.unwrap();
    state.entered(2).await;
    drop(old);
    active_count(&state, 1).await;
    state.release.add_permits(1);
    let reply = tcp_reply(&mut replacement).await;
    assert_eq!(reply.header.correlation_id, 2);
    assert_eq!(
        parrot_api::message::serde_remote_deserialize::<Query>(&reply.payload)
            .unwrap()
            .0,
        2
    );
    system.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_request_is_rejected_before_any_codec_or_lookup() {
    // Any lookup attempt is observable and blocks forever until cancellation.
    let state = State::new(true);
    let ingress = limited_ingress(&state, 1, 1);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let back = FrameSender::anon(tx);
    let frame = Frame::ask(
        1,
        "/unknown",
        "bin:Unregistered#v1",
        bytes::Bytes::new(),
        None,
    );
    ingress.dispatch(frame, &back, "peer").await;
    assert_error(receive(&mut rx).await, 1, ErrCode::Overloaded);
    assert_eq!(state.started.load(Ordering::SeqCst), 0);
    ingress.close_asks();
    ingress.wait_for_asks().await;
}

#[derive(parrot_api_derive::RemoteMessage)]
struct CodecPanic(u8);
impl parrot_api::Message for CodecPanic {
    type Result = Self;
}
impl serde::Serialize for CodecPanic {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        assert_ne!(self.0, 1, "reply encoder panic");
        serializer.serialize_u8(self.0)
    }
}
impl<'de> serde::Deserialize<'de> for CodecPanic {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <u8 as serde::Deserialize>::deserialize(deserializer)?;
        assert_ne!(value, 0, "request decoder panic");
        Ok(Self(value))
    }
}

#[tokio::test]
async fn request_decoder_and_reply_encoder_panics_do_not_leak_capacity() {
    let state = State::new(false);
    state.release.close();
    let ingress = limited_ingress(&state, 1, 4096);
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let back = FrameSender::anon(tx);
    for (cid, wire) in [(1, 0u8), (2, 1u8)] {
        // The established bincode u8 representation avoids invoking the
        // intentionally panicking serializer in the test sender.
        let frame = Frame::ask(
            cid,
            "/user/controlled",
            CodecPanic::TYPE_KEY,
            bytes::Bytes::from(vec![wire]),
            None,
        );
        ingress.dispatch(frame, &back, "peer").await;
        assert_error(receive(&mut rx).await, cid, ErrCode::ProtocolViolation);
    }
    ingress.dispatch(query_frame(3), &back, "peer").await;
    let reply = receive(&mut rx).await;
    assert_eq!(
        parrot_api::message::serde_remote_deserialize::<Query>(&reply.payload)
            .unwrap()
            .0,
        3
    );
    ingress.close_asks();
    ingress.wait_for_asks().await;
}

#[tokio::test]
async fn disabling_heartbeat_is_effective_without_disabling_heartbeat_responses_or_business() {
    use futures::{SinkExt, StreamExt};
    let state = State::new(false);
    state.release.close();
    let mut config = RemoteConfig::tcp("disabled-heartbeat", Some("127.0.0.1:0".parse().unwrap()))
        .with_knobs(parrot_remote::transport::RuntimeKnobs {
            heartbeat_interval: Duration::from_millis(10),
            heartbeat_max_loss: 1,
            ..Default::default()
        });
    config.heartbeat_enabled = false;
    let system = RemoteActorSystem::new(config, Arc::new(Controlled(state))).unwrap();
    system.start().await.unwrap();
    let mut peer = tcp_peer(&system).await;
    // Several detection intervals elapse; neither heartbeat nor EOF is allowed.
    let unsolicited = tokio::time::timeout(Duration::from_millis(80), peer.next()).await;
    if unsolicited.is_ok() {
        system.shutdown().await.unwrap();
    }
    assert!(
        unsolicited.is_err(),
        "disabled heartbeat must not emit frames or declare a silent peer dead"
    );
    peer.send(Frame::heartbeat()).await.unwrap();
    let ack = tokio::time::timeout(Duration::from_secs(5), peer.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        ack.header.frame_type,
        parrot_remote::frame::frame_type::HEARTBEAT_ACK
    );
    peer.send(query_frame(1)).await.unwrap();
    let reply = tcp_reply(&mut peer).await;
    assert_eq!(reply.header.correlation_id, 1);
    assert_eq!(
        parrot_api::message::serde_remote_deserialize::<Query>(&reply.payload)
            .unwrap()
            .0,
        1
    );
    system.shutdown().await.unwrap();
}
