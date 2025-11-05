use parrot_api::{
    address::ActorRef,
    message::RemoteMessage,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    frame::{flags, frame_type, Frame},
    ingress::{Ingress, LocalLookup},
    registry::CallbackRegistry,
    transport::FrameSender,
    NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    any::Any,
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

#[derive(Debug)]
struct State {
    started: AtomicUsize,
    completed: AtomicUsize,
    admitted: AtomicUsize,
    active: AtomicUsize,
    fail: AtomicUsize,
    release: tokio::sync::Semaphore,
}
impl Default for State {
    fn default() -> Self {
        Self {
            started: 0.into(),
            completed: 0.into(),
            admitted: 0.into(),
            active: 0.into(),
            fail: 0.into(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}
struct Active(Arc<State>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}
async fn wait_count(value: &AtomicUsize, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while value.load(Ordering::SeqCst) != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observable processing state must progress");
}
#[derive(Clone, Debug)]
struct Target(Arc<State>);
#[async_trait::async_trait]
impl ActorRef for Target {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert_ne!(self.0.fail.load(Ordering::SeqCst), 3, "constructor panic");
        Box::pin(async move {
            if message.downcast_ref::<Work>().is_some_and(|m| m.0 == 0) {
                return Ok(message);
            }
            self.0.active.fetch_add(1, Ordering::SeqCst);
            let _active = Active(self.0.clone());
            self.0.started.fetch_add(1, Ordering::SeqCst);
            self.0.release.acquire().await.unwrap().forget();
            match self.0.fail.load(Ordering::SeqCst) {
                1 => {
                    return Err(parrot_api::errors::ActorError::MessageHandlingError(
                        "business failed".into(),
                    ))
                }
                2 => panic!("future panic"),
                _ => {}
            }
            self.0.completed.fetch_add(1, Ordering::SeqCst);
            // Intentionally no RemoteMessage codec for the business result.
            Ok(Box::new(()) as BoxedMessage)
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
        Box::pin(async move {
            self.0.admitted.fetch_add(1, Ordering::SeqCst);
            if self.0.fail.load(Ordering::SeqCst) == 4 {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
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
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[async_trait::async_trait]
impl LocalLookup for Target {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/target").then(|| self.clone_boxed())
    }
}
fn completion(cid: u64) -> Frame {
    let mut f = Frame::tell(
        "/target",
        Work::TYPE_KEY,
        parrot_api::message::serde_remote_serialize(&Work(1))
            .unwrap()
            .into(),
    );
    f.header.flags |= flags::TELL_ACK;
    f.header.correlation_id = cid;
    f
}
fn setup() -> (
    Arc<State>,
    Arc<Ingress>,
    FrameSender,
    tokio::sync::mpsc::Receiver<Frame>,
) {
    let state = Arc::new(State::default());
    let ingress = Arc::new(Ingress::new(
        Arc::new(Target(state.clone())),
        Arc::new(CallbackRegistry::default()),
    ));
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    (state, ingress, FrameSender::anon(tx), rx)
}
#[tokio::test]
async fn completion_tell_waits_for_business_success_and_does_not_encode_its_result() {
    let (state, ingress, back, mut rx) = setup();
    ingress.dispatch(completion(71), &back, "peer").await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while state.started.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("TELL_ACK must invoke completion, not mere deliver admission");
    assert_eq!(state.completed.load(Ordering::SeqCst), 0);
    assert!(
        rx.try_recv().is_err(),
        "no success before handler completion"
    );
    state.release.add_permits(1);
    let ack = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ack.header.frame_type, frame_type::REPLY);
    assert_eq!(ack.header.correlation_id, 71);
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    assert_eq!(state.admitted.load(Ordering::SeqCst), 0);
    ingress.close();
    ingress.wait_closed().await;
}
#[tokio::test]
async fn completion_tell_decode_failure_returns_an_error_receipt() {
    let (state, ingress, back, mut rx) = setup();
    let mut f = completion(72);
    f.type_key = "unknown:completion".into();
    ingress.dispatch(f, &back, "peer").await;
    let ack = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ack.header.frame_type, frame_type::REPLY_ERR);
    assert_eq!(ack.header.correlation_id, 72);
    assert_eq!(state.started.load(Ordering::SeqCst), 0);
    ingress.close();
    ingress.wait_closed().await;
}
#[test]
fn completion_flag_requires_tell_a_request_id_and_no_ordinary_tell_sequence() {
    let mut f = completion(1);
    f.header.frame_type = frame_type::ASK;
    assert!(f.validate_payload_flags().is_err());
    f.header.frame_type = frame_type::TELL;
    f.header.correlation_id = 0;
    assert!(f.validate_payload_flags().is_err());
    f.header.correlation_id = 1;
    f.header.seq = 1;
    assert!(f.validate_payload_flags().is_err());
}

fn next_id() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "completion-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
async fn node(id: &str, kind: &str, state: Arc<State>) -> Arc<RemoteActorSystem> {
    let config = match kind {
        "tcp" => RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap())),
        "quic" => RemoteConfig::quic(id, Some("127.0.0.1:0".parse().unwrap())),
        _ => RemoteConfig::mem(id),
    }
    .with_insecure_development();
    let system = RemoteActorSystem::new(config, Arc::new(Target(state))).unwrap();
    system.start().await.unwrap();
    system
}

async fn hub_source(id: &str, state: Arc<State>) -> Arc<RemoteActorSystem> {
    let config = RemoteConfig::mem(id)
        .with_insecure_development()
        .with_topology(parrot_remote::topology::TopologyConfig {
            mode: parrot_remote::topology::TopologyMode::Hub,
            relay_fallback: false,
            ..Default::default()
        });
    let system = RemoteActorSystem::new(config, Arc::new(Target(state))).unwrap();
    system.start().await.unwrap();
    system
}
async fn pair(
    kind: &str,
) -> (
    Arc<RemoteActorSystem>,
    Arc<RemoteActorSystem>,
    Arc<State>,
    parrot_remote::RemoteActorRef,
) {
    let id = next_id();
    let state = Arc::new(State::default());
    let source = node(&format!("{id}-source"), kind, Arc::new(State::default())).await;
    let target_id = format!("{id}-target");
    let target = node(&target_id, kind, state.clone()).await;
    match kind {
        "direct" => source.connect_mem_pair(&target).await.unwrap(),
        "mem" => source.connect(&NodeAddr::mem(&target_id)).await.unwrap(),
        "tcp" => source
            .connect(&NodeAddr::tcp(&target_id, target.local_addr().unwrap()))
            .await
            .unwrap(),
        "quic" => source
            .connect(&NodeAddr::quic(&target_id, target.local_addr().unwrap()))
            .await
            .unwrap(),
        _ => unreachable!(),
    }
    let actor = source
        .remote_ref(&format!("parrot://{target_id}/target"))
        .unwrap();
    (source, target, state, actor)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_transports_wait_for_completion_without_blocking_other_requests() {
    for kind in ["direct", "mem", "tcp", "quic"] {
        let (source, target, state, actor) = pair(kind).await;
        let task = tokio::spawn({
            let actor = actor.clone();
            async move {
                actor
                    .deliver_with_ack(Box::new(Work(1)), Duration::from_secs(5))
                    .await
            }
        });
        wait_count(&state.started, 1).await;
        assert!(!task.is_finished());
        let reply = actor
            .send_with_timeout(Box::new(Work(0)), Some(Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(reply.downcast_ref::<Work>().unwrap().0, 0);
        assert_eq!(state.completed.load(Ordering::SeqCst), 0);
        state.release.add_permits(1);
        task.await.unwrap().unwrap();
        assert_eq!(state.completed.load(Ordering::SeqCst), 1);
        assert_eq!(source.callbacks.len(), 0);
        assert_eq!(state.admitted.load(Ordering::SeqCst), 0);
        source.shutdown().await.unwrap();
        target.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn handler_errors_panics_and_missing_actor_never_become_success_receipts() {
    let (source, target, state, actor) = pair("direct").await;
    for fail in [1, 2, 3] {
        state.fail.store(fail, Ordering::SeqCst);
        state.release.add_permits(1);
        assert!(actor
            .deliver_with_ack(Box::new(Work(1)), Duration::from_secs(2))
            .await
            .is_err());
        assert_eq!(source.callbacks.len(), 0);
    }
    state.fail.store(0, Ordering::SeqCst);
    let missing = source
        .remote_ref(&actor.path().replace("/target", "/missing"))
        .unwrap();
    assert!(missing
        .deliver_with_ack(Box::new(Work(1)), Duration::from_secs(2))
        .await
        .is_err());
    assert_eq!(state.completed.load(Ordering::SeqCst), 0);
    assert_eq!(state.active.load(Ordering::SeqCst), 0);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn timeout_and_cancellation_release_callbacks_without_claiming_business_rollback() {
    let (source, target, state, actor) = pair("direct").await;
    assert!(actor
        .deliver_with_ack(Box::new(Work(1)), Duration::from_millis(30))
        .await
        .is_err());
    assert_eq!(source.callbacks.len(), 0);
    wait_count(&state.started, 1).await;
    // Timed-out execution is still allowed to finish; its late receipt must not
    // complete another callback or report that retry would be safe.
    state.release.add_permits(1);
    wait_count(&state.completed, 1).await;
    let task = tokio::spawn({
        let actor = actor.clone();
        async move {
            actor
                .deliver_with_ack(Box::new(Work(1)), Duration::from_secs(60))
                .await
        }
    });
    wait_count(&state.started, 2).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(source.callbacks.len(), 0);
    state.release.add_permits(1);
    wait_count(&state.completed, 2).await;
    assert!(actor
        .deliver_with_ack(Box::new(Work(1)), Duration::ZERO)
        .await
        .is_err());
    assert_eq!(state.started.load(Ordering::SeqCst), 2);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn connection_close_cancels_owned_completion_waits() {
    let (source, target, state, actor) = pair("direct").await;
    let task = tokio::spawn(async move {
        actor
            .deliver_with_ack(Box::new(Work(1)), Duration::from_secs(60))
            .await
    });
    wait_count(&state.started, 1).await;
    source.shutdown().await.unwrap();
    assert!(task.await.unwrap().is_err());
    wait_count(&state.active, 0).await;
    assert_eq!(source.callbacks.len(), 0);
    assert_eq!(state.completed.load(Ordering::SeqCst), 0);
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn slow_ordinary_tell_lane_does_not_block_completion_requests() {
    let (source, target, state, actor) = pair("direct").await;
    state.fail.store(4, Ordering::SeqCst);
    actor.deliver(Box::new(Work(1))).await.unwrap();
    wait_count(&state.admitted, 1).await;
    let task = tokio::spawn(async move {
        actor
            .deliver_with_ack(Box::new(Work(1)), Duration::from_secs(5))
            .await
    });
    wait_count(&state.started, 1).await;
    state.release.add_permits(1);
    task.await.unwrap().unwrap();
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn completion_and_ask_share_admission_and_shutdown_budget() {
    let state = Arc::new(State::default());
    let ingress = Arc::new(
        Ingress::with_ask_limits(
            Arc::new(Target(state.clone())),
            Arc::new(CallbackRegistry::default()),
            1,
            4096,
        )
        .unwrap(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let back = FrameSender::anon(tx);
    ingress.dispatch(completion(1), &back, "peer").await;
    wait_count(&state.started, 1).await;
    ingress
        .dispatch(
            Frame::ask(
                2,
                "/target",
                Work::TYPE_KEY,
                parrot_api::message::serde_remote_serialize(&Work(1))
                    .unwrap()
                    .into(),
                None,
            ),
            &back,
            "peer",
        )
        .await;
    let rejected = rx.recv().await.unwrap();
    assert_eq!(rejected.header.correlation_id, 2);
    assert_eq!(
        &rejected.payload[..2],
        &(parrot_remote::ErrCode::Overloaded as u16).to_le_bytes()
    );
    assert_eq!(state.started.load(Ordering::SeqCst), 1);
    ingress.close();
    ingress.wait_closed().await;
    assert_eq!(state.active.load(Ordering::SeqCst), 0);
    assert_eq!(state.completed.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn batches_keep_independent_completion_ids_and_preflight_invalid_members() {
    let (state, ingress, back, mut rx) = setup();
    let frames = [completion(31), completion(32), completion(33)];
    let batch = Frame::batch(&frames).unwrap();
    ingress.dispatch(batch.clone(), &back, "peer").await;
    wait_count(&state.started, 3).await;
    assert!(rx.try_recv().is_err());
    state.release.add_permits(3);
    let mut ids = Vec::new();
    for _ in 0..3 {
        let reply = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.type_key, parrot_remote::frame::TELL_COMPLETION_KEY);
        assert!(reply.payload.is_empty());
        ids.push(reply.header.correlation_id);
    }
    ids.sort();
    assert_eq!(ids, [31, 32, 33]);
    let mut bad = batch;
    let mut bytes = bad.payload.to_vec();
    bytes[8..16].fill(0);
    bad.payload = bytes.into();
    assert!(bad.batch_frames().is_err());
    ingress.dispatch(bad, &back, "peer").await;
    assert!(back.is_closed());
    assert_eq!(state.started.load(Ordering::SeqCst), 3);
    ingress.close();
    ingress.wait_closed().await;
}

#[tokio::test]
async fn hub_relays_completion_and_restores_callback_id_without_local_delivery() {
    let id = next_id();
    let source = hub_source(&format!("{id}-source"), Arc::new(State::default())).await;
    let state = Arc::new(State::default());
    let target_id = format!("{id}-target");
    let target = node(&target_id, "mem", state.clone()).await;
    let hub_state = Arc::new(State::default());
    let hub = RemoteActorSystem::new(
        RemoteConfig::mem(format!("{id}-hub"))
            .with_insecure_development()
            .with_role(parrot_remote::handshake::TopologyRole::Hub),
        Arc::new(Target(hub_state.clone())),
    )
    .unwrap();
    hub.start().await.unwrap();
    source.connect_mem_pair(&hub).await.unwrap();
    target.connect_mem_pair(&hub).await.unwrap();
    let actor = source
        .remote_ref(&format!("parrot://{target_id}/target"))
        .unwrap();
    let task = tokio::spawn(async move {
        actor
            .deliver_with_ack(Box::new(Work(1)), Duration::from_secs(5))
            .await
    });
    wait_count(&state.started, 1).await;
    assert!(!task.is_finished());
    assert_eq!(hub_state.started.load(Ordering::SeqCst), 0);
    assert_eq!(hub.ingress.relay_table.len(), 1);
    state.release.add_permits(1);
    task.await.unwrap().unwrap();
    assert!(hub.ingress.relay_table.is_empty());
    assert_eq!(source.callbacks.len(), 0);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
    hub.shutdown().await.unwrap();
}

type RawInput = (Frame, FrameSender, String);
async fn raw_peer(
    capabilities: u32,
) -> (
    Arc<RemoteActorSystem>,
    parrot_remote::transport::memory::MemoryTransport,
    tokio::sync::mpsc::Receiver<RawInput>,
    parrot_remote::RemoteActorRef,
) {
    use parrot_remote::{
        handshake::HandshakeBody,
        transport::{memory::MemoryTransport, Transport},
    };
    let id = next_id();
    let raw_id = format!("{id}-raw");
    let source = node(&id, "mem", Arc::new(State::default())).await;
    let (inbound, rx) = tokio::sync::mpsc::channel(8);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let peer = MemoryTransport::new(
        HandshakeBody {
            node_id: raw_id.clone(),
            capabilities,
            ..Default::default()
        },
        inbound,
        Arc::new(|_| {}),
        shutdown,
        None,
    )
    .unwrap();
    let addr = NodeAddr::mem(&raw_id);
    let (_, handle) = tokio::try_join!(source.connect(&addr), peer.accept()).unwrap();
    assert_eq!(handle.sender.node_id(), id);
    let actor = source
        .remote_ref(&format!("parrot://{raw_id}/target"))
        .unwrap();
    (source, peer, rx, actor)
}

#[tokio::test]
async fn unsupported_peer_is_rejected_before_publishing_business_work() {
    use parrot_remote::{handshake::caps, transport::Transport};
    let (source, peer, mut rx, actor) = raw_peer(caps::BIN).await;
    let err = actor
        .deliver_with_ack(Box::new(Work(1)), Duration::from_secs(2))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("does not support"));
    assert!(rx.try_recv().is_err());
    assert_eq!(source.callbacks.len(), 0);
    source.shutdown().await.unwrap();
    peer.shutdown().await;
}

#[tokio::test]
async fn hub_refuses_to_forward_completion_to_a_legacy_destination() {
    use parrot_remote::{
        handshake::{caps, HandshakeBody, TopologyRole},
        transport::{memory::MemoryTransport, Transport},
    };
    let id = next_id();
    let raw_id = format!("{id}-old");
    let hub_id = format!("{id}-hub");
    let hub = RemoteActorSystem::new(
        RemoteConfig::mem(&hub_id)
            .with_insecure_development()
            .with_role(TopologyRole::Hub),
        Arc::new(Target(Arc::new(State::default()))),
    )
    .unwrap();
    hub.start().await.unwrap();
    let source = hub_source(&id, Arc::new(State::default())).await;
    source.connect_mem_pair(&hub).await.unwrap();
    let (inbound, mut rx) = tokio::sync::mpsc::channel(8);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let peer = MemoryTransport::new(
        HandshakeBody {
            node_id: raw_id.clone(),
            capabilities: caps::BIN,
            ..Default::default()
        },
        inbound,
        Arc::new(|_| {}),
        shutdown,
        None,
    )
    .unwrap();
    let addr = NodeAddr::mem(&raw_id);
    let (_, _handle) = tokio::try_join!(hub.connect(&addr), peer.accept()).unwrap();
    let actor = source
        .remote_ref(&format!("parrot://{raw_id}/target"))
        .unwrap();
    let error = actor
        .deliver_with_ack(Box::new(Work(1)), Duration::from_secs(3))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("next hop"));
    assert!(rx.try_recv().is_err());
    assert!(hub.ingress.relay_table.is_empty());
    assert_eq!(source.callbacks.len(), 0);
    source.shutdown().await.unwrap();
    hub.shutdown().await.unwrap();
    peer.shutdown().await;
}

#[tokio::test]
async fn malformed_or_wrong_connection_receipts_cannot_confirm_business_success() {
    use parrot_remote::{handshake::caps, transport::Transport};
    let (source, peer, mut rx, actor) = raw_peer(caps::BIN | caps::TELL_COMPLETION).await;
    let other = node(&next_id(), "mem", Arc::new(State::default())).await;
    source.connect_mem_pair(&other).await.unwrap();
    let wrong_back = source.links_snapshot().await.last().unwrap().1.clone();
    for malformed in [true, false] {
        let task = tokio::spawn({
            let actor = actor.clone();
            async move {
                actor
                    .deliver_with_ack(Box::new(Work(1)), Duration::from_secs(5))
                    .await
            }
        });
        let (request, back, _) = rx.recv().await.unwrap();
        assert_eq!(request.header.frame_type, frame_type::TELL);
        let cid = request.header.correlation_id;
        source
            .ingress
            .dispatch(
                Frame::tell_completed(cid),
                &wrong_back,
                wrong_back.node_id(),
            )
            .await;
        assert_eq!(source.callbacks.len(), 1);
        assert!(!task.is_finished());
        if malformed {
            back.send(Frame::reply(
                cid,
                "",
                "business-result",
                bytes::Bytes::new(),
            ))
            .await
            .unwrap();
        } else {
            let mut ack = Frame::tell_completed(cid);
            ack.payload = bytes::Bytes::from_static(b"extra");
            back.send(ack).await.unwrap();
        }
        assert!(task.await.unwrap().is_err());
        assert_eq!(source.callbacks.len(), 0);
    }
    source.shutdown().await.unwrap();
    other.shutdown().await.unwrap();
    peer.shutdown().await;
}
