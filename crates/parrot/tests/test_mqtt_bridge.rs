//! Real bridge -> remote ingress -> actor/transactional business contracts.
//! The TCP broker fixture is owned by each test; missing services never pass.
#[path = "support/mqtt_broker.rs"]
mod broker;
use broker::{Broker, Session};
use parrot::thread::{ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    message::RemoteMessage,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::bytes::{Bytes, BytesMut};
use parrot_remote::mqtt::client as rumqttc;
use parrot_remote::{
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig, durable::*, frame::Frame, mqtt::*,
};
use rumqttc::{MqttOptions, Packet, Publish, QoS};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("parrot-mqtt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn unique() -> String {
    format!("mqtt-{}", uuid::Uuid::new_v4())
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
struct Lookup {
    actor: Option<BoxedActorRef>,
    durable: Option<Arc<dyn DurableReceiver>>,
}
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        if path == "/sink" {
            self.actor.as_ref().map(|a| a.clone_boxed())
        } else {
            None
        }
    }
    fn durable_receiver(&self, path: &str) -> Option<Arc<dyn DurableReceiver>> {
        if path == "/sink" {
            self.durable.clone()
        } else {
            None
        }
    }
}
async fn remote(
    id: &str,
    actor: Option<BoxedActorRef>,
    durable: Option<Arc<dyn DurableReceiver>>,
) -> Arc<RemoteActorSystem> {
    let r =
        RemoteActorSystem::new(RemoteConfig::mem(id), Arc::new(Lookup { actor, durable })).unwrap();
    r.start().await.unwrap();
    r
}
fn config(broker: &Broker, dir: &Scratch, target: &str) -> BridgeConfig {
    let mut mqtt = MqttOptions::new(unique(), "127.0.0.1", broker.port());
    mqtt.set_keep_alive(Duration::from_secs(5));
    BridgeConfig {
        mqtt,
        key: "edge/device-a".into(),
        producer: "device-a".into(),
        session: "boot-1".into(),
        receiver: NodeAddr::mem(target),
        path: "/sink".into(),
        directory: dir.0.join("outbox"),
        outbox: OutboxConfig {
            transactional: true,
            idempotent_submissions: true,
            attempt_timeout: Duration::from_millis(100),
            retry_min: Duration::from_millis(10),
            retry_max: Duration::from_millis(40),
            ..Default::default()
        },
        max_frame_bytes: 4096,
        queue_capacity: 4,
        queue_bytes: 65536,
        retry_min: Duration::from_millis(20),
        retry_max: Duration::from_millis(100),
        insecure_development: true,
    }
}
fn encode(frame: Frame) -> Bytes {
    let mut b = BytesMut::new();
    frame.encode(&mut b).unwrap();
    b.freeze()
}
fn durable(config: &BridgeConfig, seq: u64, cid: u64, value: u64) -> Bytes {
    let envelope = DurableEnvelope {
        source: config.producer.clone(),
        session: config.session.clone(),
        receiver: config.receiver.node_id.clone(),
        path: config.path.clone(),
        sequence: seq,
        type_key: "counter:add#v1".into(),
        payload: Bytes::copy_from_slice(&value.to_le_bytes()),
    };
    encode(Frame::tell_with_ack(
        cid,
        &envelope.target_path(),
        DURABLE_KEY,
        envelope.encode().unwrap(),
    ))
}
async fn inbox(dir: &Scratch, target: &str) -> TransactionalInbox {
    TransactionalInbox::open(
        dir.0.join("inbox"),
        target,
        "/sink",
        Bytes::copy_from_slice(&0u64.to_le_bytes()),
        InboxLimits::default(),
        |state, m| {
            if m.type_key != "counter:add#v1" || m.payload.len() != 8 {
                return Err("invalid counter input".into());
            }
            let n = u64::from_le_bytes(state.try_into().unwrap())
                .checked_add(u64::from_le_bytes(m.payload[..].try_into().unwrap()))
                .ok_or("overflow")?;
            Ok(Bytes::copy_from_slice(&n.to_le_bytes()))
        },
    )
    .await
    .unwrap()
}
fn value(inbox: &TransactionalInbox) -> u64 {
    u64::from_le_bytes(inbox.handle().snapshot().state[..].try_into().unwrap())
}
async fn receipt(session: &mut Session, config: &BridgeConfig, seq: u64, cid: u64) -> u16 {
    let Packet::Publish(p) = session.next().await else {
        panic!("expected Wire completion publication before subscriber PUBACK")
    };
    assert_eq!(p.topic, config.receipt_topic());
    assert_eq!(p.qos, QoS::AtLeastOnce);
    assert!(!p.retain);
    let mut bytes = BytesMut::from(p.payload.as_ref());
    let f = Frame::decode(&mut bytes).unwrap().unwrap();
    assert!(bytes.is_empty());
    assert_eq!(f.header.frame_type, parrot_remote::frame::frame_type::REPLY);
    assert_eq!(f.header.correlation_id, cid);
    assert_eq!(f.type_key, DURABLE_RECEIPT_KEY);
    assert_eq!(f.payload.as_ref(), seq.to_le_bytes());
    p.pkid
}
async fn finish(session: &mut Session, config: &BridgeConfig, seq: u64, cid: u64, pkid: u16) {
    let reply_id = receipt(session, config, seq, cid).await;
    session.ack(reply_id).await;
    session.incoming_ack(pkid).await;
}

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Add(u64);
impl parrot_api::Message for Add {
    type Result = ();
}
struct Query;
struct Sink(Vec<u64>);
impl Actor for Sink {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(Add(n)) = msg.downcast_ref::<Add>() {
                self.0.push(*n);
            }
            Ok(Box::new(self.0.clone()) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qos0_reaches_a_real_thread_actor_with_all_twenty_distinct_payloads() {
    let dir = Scratch::new();
    let broker = Broker::new().await;
    let target = unique();
    let source = remote(&unique(), None, None).await;
    let engine = ThreadActorSystem::shared(Default::default());
    let actor: BoxedActorRef = Box::new(
        engine
            .spawn_at(Sink(Vec::new()), "/sink", None, Default::default())
            .await
            .unwrap(),
    );
    let receiver = remote(&target, Some(actor.clone_boxed()), None).await;
    let cfg = config(&broker, &dir, &target);
    let bridge = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    let mut session = broker.accept(&cfg.topic()).await;
    bridge.handle().ready().await.unwrap();
    for n in 0..20 {
        session
            .publish(
                &cfg.topic(),
                QoS::AtMostOnce,
                0,
                encode(Frame::tell(
                    &format!("parrot://{target}/sink"),
                    Add::TYPE_KEY,
                    parrot_api::message::serde_remote_serialize(&Add(n))
                        .unwrap()
                        .into(),
                )),
            )
            .await;
        // QoS0 has no replay guarantee under overload. This delivery contract
        // admits one packet at a time; bounded overload is tested separately.
        until(|| bridge.handle().status().qos0_forwarded == n + 1).await;
    }
    until(|| bridge.handle().status().qos0_forwarded == 20).await;
    bridge.handle().drain(Duration::from_secs(5)).await.unwrap();
    let actual = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let values = *actor
                .send(Box::new(Query))
                .await
                .unwrap()
                .downcast::<Vec<u64>>()
                .unwrap();
            if values.len() == 20 {
                break values;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(actual, (0..20).collect::<Vec<_>>());
    bridge.close().await.unwrap();
    receiver.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    actor.stop().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}
struct Gate {
    open: AtomicBool,
    changed: tokio::sync::Notify,
    entered: AtomicBool,
    target: InboxHandle,
}
impl Gate {
    fn release(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
#[async_trait::async_trait]
impl DurableReceiver for Gate {
    async fn receive(&self, message: DurableEnvelope) -> Result<(), InboxError> {
        self.entered.store(true, Ordering::SeqCst);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.open.load(Ordering::SeqCst) {
                break;
            }
            changed.await;
        }
        self.target.apply(message).await
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qos1_ack_waits_for_business_commit_and_broker_acceptance_of_wire_receipt() {
    let dir = Scratch::new();
    let broker = Broker::new().await;
    let target = unique();
    let mut state = inbox(&dir, &target).await;
    let gate = Arc::new(Gate {
        open: AtomicBool::new(false),
        changed: Default::default(),
        entered: AtomicBool::new(false),
        target: state.handle(),
    });
    let _release = Release(gate.clone());
    let receiver = remote(&target, None, Some(gate.clone())).await;
    let source = remote(&unique(), None, None).await;
    let cfg = config(&broker, &dir, &target);
    let bridge = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    let mut session = broker.accept(&cfg.topic()).await;
    session
        .publish(&cfg.topic(), QoS::AtLeastOnce, 17, durable(&cfg, 1, 101, 7))
        .await;
    until(|| bridge.handle().status().last_enqueued == 1 && gate.entered.load(Ordering::SeqCst))
        .await;
    assert_eq!(state.handle().snapshot().revision, 0);
    session.quiet().await;
    gate.release();
    let response = receipt(&mut session, &cfg, 1, 101).await;
    assert_eq!(value(&state), 7);
    assert_eq!(bridge.handle().status().last_acked, 1);
    session.quiet().await;
    session.ack(response).await;
    session.incoming_ack(17).await;
    session
        .publish(&cfg.topic(), QoS::AtLeastOnce, 18, durable(&cfg, 1, 102, 7))
        .await;
    finish(&mut session, &cfg, 1, 102, 18).await;
    assert_eq!(state.handle().snapshot().revision, 1);
    assert_eq!(bridge.handle().status().last_enqueued, 1);
    bridge.handle().drain(Duration::from_secs(5)).await.unwrap();
    receiver.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    state.close().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_wal_reopens_and_conflicting_submission_identity_is_not_acknowledged() {
    let dir = Scratch::new();
    let broker = Broker::new().await;
    let target = unique();
    let source = remote(&unique(), None, None).await;
    let cfg = config(&broker, &dir, &target);
    let bridge = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    let mut session = broker.accept(&cfg.topic()).await;
    session
        .publish(&cfg.topic(), QoS::AtLeastOnce, 7, durable(&cfg, 1, 201, 9))
        .await;
    until(|| bridge.handle().status().last_enqueued == 1).await;
    assert_eq!(bridge.handle().status().last_acked, 0);
    bridge.close().await.unwrap();
    session.closed().await;
    drop(bridge);
    let mut state = inbox(&dir, &target).await;
    let receiver = remote(&target, None, Some(Arc::new(state.handle()))).await;
    let reopened = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    until(|| state.handle().snapshot().revision == 1).await;
    assert_eq!(value(&state), 9);
    let mut session = broker.accept(&cfg.topic()).await;
    session
        .publish(&cfg.topic(), QoS::AtLeastOnce, 8, durable(&cfg, 1, 202, 9))
        .await;
    finish(&mut session, &cfg, 1, 202, 8).await;
    assert_eq!(reopened.handle().status().last_enqueued, 1);
    session
        .publish(&cfg.topic(), QoS::AtLeastOnce, 9, durable(&cfg, 1, 203, 10))
        .await;
    until(|| reopened.handle().status().closed).await;
    assert!(
        matches!(reopened.close().await, Err(BridgeError::Storage(s)) if s.contains("conflicts"))
    );
    session.closed().await;
    assert_eq!(state.handle().snapshot().revision, 1);
    assert_eq!(value(&state), 9);
    receiver.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    state.close().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_connection_completion_cannot_ack_a_new_packet_reusing_the_same_id() {
    let dir = Scratch::new();
    let broker = Broker::new().await;
    let target = unique();
    let mut state = inbox(&dir, &target).await;
    let gate = Arc::new(Gate {
        open: AtomicBool::new(false),
        changed: Default::default(),
        entered: AtomicBool::new(false),
        target: state.handle(),
    });
    let _release = Release(gate.clone());
    let receiver = remote(&target, None, Some(gate.clone())).await;
    let source = remote(&unique(), None, None).await;
    let cfg = config(&broker, &dir, &target);
    let bridge = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    let mut old = broker.accept(&cfg.topic()).await;
    old.publish(&cfg.topic(), QoS::AtLeastOnce, 44, durable(&cfg, 1, 301, 7))
        .await;
    until(|| gate.entered.load(Ordering::SeqCst)).await;
    drop(old);
    let mut new = broker.accept(&cfg.topic()).await;
    new.publish(&cfg.topic(), QoS::AtLeastOnce, 44, durable(&cfg, 2, 302, 9))
        .await;
    new.quiet().await;
    gate.release();
    finish(&mut new, &cfg, 2, 302, 44).await;
    assert_eq!(value(&state), 16);
    assert_eq!(state.handle().snapshot().revision, 2);
    assert!(bridge.handle().status().generation >= 2);
    bridge.handle().drain(Duration::from_secs(5)).await.unwrap();
    receiver.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    state.close().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_and_unauthorized_input_fails_closed_without_ack_or_side_effects() {
    let dir = Scratch::new();
    let broker = Broker::new().await;
    let target = unique();
    let mut state = inbox(&dir, &target).await;
    let receiver = remote(&target, None, Some(Arc::new(state.handle()))).await;
    let source = remote(&unique(), None, None).await;
    let cfg = config(&broker, &dir, &target);
    for variant in 0..8 {
        let bridge = MqttBridge::start(cfg.clone(), source.clone())
            .await
            .unwrap();
        let mut session = broker.accept(&cfg.topic()).await;
        let mut publication = Publish {
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            topic: cfg.topic(),
            pkid: 1,
            payload: durable(&cfg, 1, 401, 7),
        };
        match variant {
            0 => publication.topic = "$parrot/foreign".into(),
            1 => publication.retain = true,
            2 => publication.payload = Bytes::from_static(b"not Wire"),
            3 => {
                let mut v = publication.payload.to_vec();
                v.extend_from_slice(&publication.payload);
                publication.payload = v.into();
            }
            4 => {
                let mut wrong = cfg.clone();
                wrong.producer = "forged".into();
                publication.payload = durable(&wrong, 1, 401, 7);
            }
            5 => {
                let mut wrong = cfg.clone();
                wrong.path = "/unauthorized".into();
                publication.payload = durable(&wrong, 1, 401, 7);
            }
            6 => publication.qos = QoS::ExactlyOnce,
            7 => {
                let mut wrong = cfg.clone();
                wrong.session = "other-session".into();
                publication.payload = durable(&wrong, 1, 401, 7);
            }
            _ => unreachable!(),
        }
        session.publication(publication).await;
        until(|| bridge.handle().status().closed).await;
        assert!(
            matches!(bridge.close().await, Err(BridgeError::Invalid(_))),
            "variant {variant}"
        );
        session.closed().await;
        assert_eq!(bridge.handle().status().last_enqueued, 0);
    }
    assert_eq!(state.handle().snapshot().revision, 0);
    receiver.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    state.close().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_owner_and_cancelled_drain_keep_durable_cleanup_owned() {
    let dir = Scratch::new();
    let broker = Broker::new().await;
    let target = unique();
    let source = remote(&unique(), None, None).await;
    let cfg = config(&broker, &dir, &target);
    let bridge = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    let h = bridge.handle();
    let mut session = broker.accept(&cfg.topic()).await;
    session
        .publish(&cfg.topic(), QoS::AtLeastOnce, 1, durable(&cfg, 1, 501, 7))
        .await;
    until(|| h.status().last_enqueued == 1).await;
    assert!(matches!(
        h.drain(Duration::from_millis(25)).await,
        Err(BridgeError::Deadline)
    ));
    assert!(h.status().draining);
    assert!(!h.status().closed);
    drop(bridge);
    h.close().await.unwrap();
    session.closed().await;
    assert!(h.status().closed);
    assert_eq!(h.status().pending_durable, 1);
    let reopened = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    assert_eq!(reopened.handle().status().last_enqueued, 1);
    reopened.close().await.unwrap();
    source.shutdown().await.unwrap();
}
#[tokio::test]
async fn configuration_is_secure_by_default_and_persistent_binding_rejects_route_reuse() {
    let dir = Scratch::new();
    let broker = Broker::new().await;
    let target = unique();
    let source = remote(&unique(), None, None).await;
    let cfg = config(&broker, &dir, &target);
    for variant in 0..6 {
        let mut bad = cfg.clone();
        match variant {
            0 => bad.insecure_development = false,
            1 => bad.mqtt = MqttOptions::new("outside", "192.0.2.1", broker.port()),
            2 => bad.key = "edge/#".into(),
            3 => bad.queue_bytes = 1,
            4 => bad.outbox.transactional = false,
            5 => bad.outbox.idempotent_submissions = false,
            _ => unreachable!(),
        }
        assert!(
            matches!(
                MqttBridge::start(bad, source.clone()).await,
                Err(BridgeError::Invalid(_))
            ),
            "variant {variant}"
        );
        assert!(!cfg.directory.exists());
    }
    let bridge = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    bridge.close().await.unwrap();
    let mut foreign = cfg;
    foreign.producer = "different-owner".into();
    assert!(matches!(
        MqttBridge::start(foreign, source.clone()).await,
        Err(BridgeError::Storage(_))
    ));
    source.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overload_disconnects_without_ack_and_redelivery_keeps_one_business_effect() {
    let dir = Scratch::new();
    let broker = Broker::new().await;
    let target = unique();
    let mut state = inbox(&dir, &target).await;
    let gate = Arc::new(Gate {
        open: AtomicBool::new(false),
        changed: Default::default(),
        entered: AtomicBool::new(false),
        target: state.handle(),
    });
    let _release = Release(gate.clone());
    let receiver = remote(&target, None, Some(gate.clone())).await;
    let source = remote(&unique(), None, None).await;
    let mut cfg = config(&broker, &dir, &target);
    cfg.queue_capacity = 1;
    let bridge = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    let mut session = broker.accept(&cfg.topic()).await;
    session
        .publish(&cfg.topic(), QoS::AtLeastOnce, 1, durable(&cfg, 1, 601, 1))
        .await;
    until(|| gate.entered.load(Ordering::SeqCst)).await;
    session
        .publish(&cfg.topic(), QoS::AtLeastOnce, 2, durable(&cfg, 2, 602, 2))
        .await;
    session
        .publish(&cfg.topic(), QoS::AtLeastOnce, 3, durable(&cfg, 3, 603, 3))
        .await;
    session.closed().await;
    assert_eq!(bridge.handle().status().last_enqueued, 1);
    assert_eq!(state.handle().snapshot().revision, 0);
    gate.release();
    until(|| state.handle().snapshot().revision == 2).await;
    let mut session = broker.accept(&cfg.topic()).await;
    for seq in 1..=3 {
        session
            .publish(
                &cfg.topic(),
                QoS::AtLeastOnce,
                seq as u16,
                durable(&cfg, seq, 700 + seq, seq),
            )
            .await;
        finish(&mut session, &cfg, seq, 700 + seq, seq as u16).await;
    }
    assert_eq!(state.handle().snapshot().revision, 3);
    assert_eq!(value(&state), 6);
    assert_eq!(bridge.handle().status().last_enqueued, 3);
    bridge.handle().drain(Duration::from_secs(5)).await.unwrap();
    receiver.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    state.close().await.unwrap();
}

#[tokio::test]
async fn missing_broker_never_reports_readiness_and_close_releases_the_owner() {
    let dir = Scratch::new();
    let broker = Broker::new().await;
    let source = remote(&unique(), None, None).await;
    let cfg = config(&broker, &dir, &unique());
    drop(broker);
    let bridge = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), bridge.handle().ready())
            .await
            .is_err()
    );
    assert!(!bridge.handle().status().ready);
    assert!(bridge.handle().status().reconnects > 0);
    bridge.close().await.unwrap();
    let reopened = MqttBridge::start(cfg, source.clone()).await.unwrap();
    reopened.close().await.unwrap();
    source.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_after_reopen_waits_for_preexisting_wal_without_new_mqtt_publications() {
    let dir = Scratch::new();
    let broker = Broker::new().await;
    let target = unique();
    let source = remote(&unique(), None, None).await;
    let cfg = config(&broker, &dir, &target);
    let original = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    let mut session = broker.accept(&cfg.topic()).await;
    session
        .publish(&cfg.topic(), QoS::AtLeastOnce, 1, durable(&cfg, 1, 801, 13))
        .await;
    until(|| original.handle().status().last_enqueued == 1).await;
    original.close().await.unwrap();
    session.closed().await;
    let reopened = MqttBridge::start(cfg.clone(), source.clone())
        .await
        .unwrap();
    let _resumed_session = broker.accept(&cfg.topic()).await;
    assert_eq!(reopened.handle().status().pending_durable, 1);
    assert!(
        matches!(
            reopened.handle().drain(Duration::from_millis(50)).await,
            Err(BridgeError::Deadline)
        ),
        "drain must not certify an unfinished recovered WAL"
    );
    assert!(!reopened.handle().status().closed);
    let mut state = inbox(&dir, &target).await;
    let receiver = remote(&target, None, Some(Arc::new(state.handle()))).await;
    reopened
        .handle()
        .drain(Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(value(&state), 13);
    assert_eq!(state.handle().snapshot().revision, 1);
    assert_eq!(reopened.handle().status().pending_durable, 0);
    receiver.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
    state.close().await.unwrap();
}
