//! Regression tests exercising real in-memory transport, codec, ref and ingress.
use parrot_api::{
    address::{ActorRef, ActorRefExt},
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Ping(u64);
impl parrot_api::message::Message for Ping {
    type Result = ();
}
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:independent-review::Ping#v1",
        type_id: std::any::TypeId::of::<Ping>(),
        encode: |m: &BoxedMessage| parrot_api::message::serde_remote_serialize(m.downcast_ref::<Ping>().ok_or("Ping")?),
        decode: |b: &[u8]| { let p: Ping=parrot_api::message::serde_remote_deserialize(b)?; Ok(Box::new(p) as BoxedMessage) },
    }
}
#[derive(Debug, Clone)]
struct Sink {
    seen: Arc<Mutex<Vec<u64>>>,
    pending: bool,
    fail: bool,
    delay_first_tell: bool,
}
#[async_trait::async_trait]
impl ActorRef for Sink {
    fn send<'a>(&'a self, m: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if self.pending {
                std::future::pending::<()>().await;
            }
            if self.fail {
                return Err(ActorError::InternalError("hook failed".into()));
            }
            if let Some(ping) = m.downcast_ref::<Ping>() {
                self.seen.lock().unwrap().push(ping.0);
            }
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
    fn deliver<'a>(&'a self, m: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.delay_first_tell && m.downcast_ref::<Ping>().unwrap().0 == 0 {
                // Deliberately let the producer outrun the receiver once.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            self.seen
                .lock()
                .unwrap()
                .push(m.downcast_ref::<Ping>().unwrap().0);
            Ok(())
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/user/probe".into()
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
struct Lookup(Sink);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        Some(self.0.clone_boxed())
    }
}
async fn pair(
    name: &str,
    pending: bool,
) -> (
    Arc<RemoteActorSystem>,
    Arc<RemoteActorSystem>,
    Arc<Mutex<Vec<u64>>>,
) {
    let seen = Arc::new(Mutex::new(vec![]));
    let sink = Sink {
        seen: seen.clone(),
        pending,
        fail: false,
        delay_first_tell: false,
    };
    let a = RemoteActorSystem::new(
        RemoteConfig::mem(format!("{name}-a")),
        Arc::new(Lookup(sink.clone())),
    )
    .unwrap();
    let b = RemoteActorSystem::new(
        RemoteConfig::mem(format!("{name}-b")),
        Arc::new(Lookup(sink)),
    )
    .unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();
    (a, b, seen)
}
#[tokio::test]
async fn multiple_refs_deliver_every_sequence_in_order() {
    let (a, b, seen) = pair("refs", false).await;
    for n in 0..10 {
        a.remote_ref("parrot://refs-b/user/probe")
            .unwrap()
            .deliver(Box::new(Ping(n)))
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while seen.lock().unwrap().len() < 10 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all ten deliveries must complete");
    assert_eq!(*seen.lock().unwrap(), (0..10).collect::<Vec<_>>());
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}
#[tokio::test]
async fn externally_cancelled_ask_releases_callback() {
    let (a, b, _) = pair("cancel", true).await;
    let r = a.remote_ref("parrot://cancel-b/user/probe").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), r.send(Box::new(Ping(1))))
            .await
            .is_err()
    );
    assert_eq!(a.callbacks.len(), 0);
    let _ = r
        .send_with_timeout(Box::new(Ping(2)), Some(Duration::from_millis(30)))
        .await;
    assert_eq!(a.callbacks.len(), 0);
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

async fn node(id: &str, hub: bool) -> (Arc<RemoteActorSystem>, Arc<Mutex<Vec<u64>>>) {
    node_with_policy(id, hub, false).await
}

async fn node_with_policy(
    id: &str,
    hub: bool,
    strict_hub: bool,
) -> (Arc<RemoteActorSystem>, Arc<Mutex<Vec<u64>>>) {
    let seen = Arc::new(Mutex::new(vec![]));
    let mut config = RemoteConfig::mem(id);
    if strict_hub {
        config.topology.mode = parrot_remote::topology::TopologyMode::Hub;
        config.topology.relay_fallback = false;
    }
    if hub {
        config.topology_role = parrot_remote::handshake::TopologyRole::Hub;
    }
    let system = RemoteActorSystem::new(
        config,
        Arc::new(Lookup(Sink {
            seen: seen.clone(),
            pending: false,
            fail: false,
            delay_first_tell: false,
        })),
    )
    .unwrap();
    system.start().await.unwrap();
    (system, seen)
}

async fn await_count(seen: &Arc<Mutex<Vec<u64>>>, count: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while seen.lock().unwrap().len() < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all messages must arrive");
}

#[tokio::test]
async fn hub_routes_namesakes_and_preserves_messages_from_two_sources() {
    let (hub, hub_seen) = node("hub", true).await;
    let (a, a_seen) = node_with_policy("source-a", false, true).await;
    let (b, b_seen) = node_with_policy("source-b", false, true).await;
    let (target, received) = node("destination", false).await;
    a.connect_mem_pair(&hub).await.unwrap();
    b.connect_mem_pair(&hub).await.unwrap();
    target.connect_mem_pair(&hub).await.unwrap();
    for i in 0..10 {
        a.remote_ref("parrot://destination/user/probe")
            .unwrap()
            .deliver(Box::new(Ping(i)))
            .await
            .unwrap();
        b.remote_ref("parrot://destination/user/probe")
            .unwrap()
            .deliver(Box::new(Ping(100 + i)))
            .await
            .unwrap();
    }
    await_count(&received, 20).await;
    let mut messages = received.lock().unwrap().clone();
    messages.sort_unstable();
    assert_eq!(messages, (0..10).chain(100..110).collect::<Vec<_>>());
    for i in 200..220 {
        let reply = a
            .remote_ref("parrot://destination/user/probe")
            .unwrap()
            .send_with_timeout(Box::new(Ping(i)), Some(Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(reply.downcast_ref::<Ping>().unwrap().0, i);
    }
    assert!(
        hub_seen.lock().unwrap().is_empty(),
        "hub must not consume the destination's messages"
    );
    assert!(a_seen.lock().unwrap().is_empty());
    assert!(b_seen.lock().unwrap().is_empty());
    assert_eq!(received.lock().unwrap().len(), 40);
    assert!(hub.ingress.relay_table.is_empty());
    for system in [&a, &b, &target, &hub] {
        system.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn existing_ref_uses_replacement_connection() {
    let (a, b, _) = pair("reconnect", false).await;
    let reference = a.remote_ref("parrot://reconnect-b/user/probe").unwrap();
    reference.send(Box::new(Ping(1))).await.unwrap();
    b.shutdown().await.unwrap();
    let (replacement, received) = node("reconnect-b", false).await;
    a.connect_mem_pair(&replacement).await.unwrap();
    // Let old connection close notifications run; they must not remove the new link.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(reference.is_alive().await);
    reference
        .send_with_timeout(Box::new(Ping(2)), Some(Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(*received.lock().unwrap(), vec![2]);
    a.shutdown().await.unwrap();
    assert!(!reference.is_alive().await);
    assert!(reference.deliver(Box::new(Ping(3))).await.is_err());
    replacement.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_tell_over_tcp_preserves_admission_order_without_ask_callbacks() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Sink {
        seen: seen.clone(),
        pending: false,
        fail: false,
        delay_first_tell: true,
    };
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp("typed-tell-source", None),
        Arc::new(Lookup(sink.clone())),
    )
    .unwrap();
    // This checks ordering within admitted capacity, not unlimited delivery.
    // TELL acknowledges the source queue; remote dispatch overload may close
    // the connection. Keep a finite sender window below the receiver budget.
    const WINDOW: usize = 32;
    let mut config = RemoteConfig::tcp("typed-tell-target", Some("127.0.0.1:0".parse().unwrap()));
    config.knobs = Some(parrot_remote::transport::RuntimeKnobs {
        inbound_dispatch_capacity: WINDOW * 2,
        inbound_dispatch_per_connection: WINDOW * 2,
        inbound_dispatch_bytes: 64 * 1024,
        ..Default::default()
    });
    let target = RemoteActorSystem::new(config, Arc::new(Lookup(sink))).unwrap();
    source.start().await.unwrap();
    target.start().await.unwrap();
    source
        .connect(&parrot_remote::NodeAddr::tcp(
            "typed-tell-target",
            target.local_addr().unwrap(),
        ))
        .await
        .unwrap();
    let actor = source
        .remote_ref("parrot://typed-tell-target/user/probe")
        .unwrap();
    let clone = actor.clone_boxed();
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        for value in 0..20_000 {
            let reference: &dyn ActorRef = if value % 2 == 0 { &actor } else { &*clone };
            reference.tell(Ping(value)).await.unwrap();
            if ((value + 1) as usize).is_multiple_of(WINDOW) {
                while seen.lock().unwrap().len() < (value + 1) as usize {
                    tokio::task::yield_now().await;
                }
                assert!(
                    source.callbacks.is_empty(),
                    "ordering window must not use ASK callbacks"
                );
            }
        }
        while seen.lock().unwrap().len() < 20_000 {
            tokio::task::yield_now().await;
        }
        seen.lock().unwrap().clone()
    })
    .await;
    assert!(
        source.callbacks.is_empty(),
        "tell must not allocate ask callbacks"
    );
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
    assert_eq!(result.unwrap(), (0..20_000).collect::<Vec<_>>());
    assert!(
        actor.tell(Ping(20_001)).await.is_err(),
        "shutdown link must report admission failure"
    );
}

#[tokio::test]
async fn native_batch_tell_delivers_every_member_in_order() {
    use parrot_remote::{codec_registry::CodecRegistry, frame::Frame};
    let (a, b, seen) = pair("batch-delivery", false).await;
    let frames: Vec<_> = (0..20)
        .map(|n| {
            let msg = Box::new(Ping(n)) as BoxedMessage;
            let (key, payload) = CodecRegistry::global().encode_outgoing(&msg).unwrap();
            Frame::tell("/user/probe", &key, payload.into())
        })
        .collect();
    let sender = a.links_snapshot().await[0].1.clone();
    sender.send(Frame::batch(&frames).unwrap()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while seen.lock().unwrap().len() < 20 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*seen.lock().unwrap(), (0..20).collect::<Vec<_>>());
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}
#[tokio::test]
async fn malformed_batch_and_unimplemented_transforms_never_reach_business_actor() {
    use parrot_remote::{
        codec_registry::CodecRegistry,
        frame::{flags, Frame},
    };
    for (index, flag) in [
        flags::BATCH,
        flags::COMPRESSED_ZSTD,
        flags::APP_ENCRYPTED,
        0x8000,
    ]
    .into_iter()
    .enumerate()
    {
        let (a, b, seen) = pair(&format!("batch-reject-{index}"), false).await;
        let (key, payload) = CodecRegistry::global()
            .encode_outgoing(&(Box::new(Ping(7)) as BoxedMessage))
            .unwrap();
        let frame = Frame::tell("/user/probe", &key, payload.into());
        let mut frame = if flag == flags::BATCH {
            Frame::batch(&[frame]).unwrap()
        } else {
            frame
        };
        frame.header.flags |= flag;
        if flag == flags::BATCH {
            let mut payload = frame.payload.to_vec();
            payload.push(0);
            frame.payload = payload.into();
        }
        let sender = a.links_snapshot().await[0].1.clone();
        sender.send(frame).await.unwrap();
        tokio::time::timeout(Duration::from_secs(7), sender.closed())
            .await
            .unwrap();
        assert!(seen.lock().unwrap().is_empty());
        a.shutdown().await.unwrap();
        b.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn batch_ask_and_reply_preserve_connection_bound_callback_correlation() {
    use parrot_remote::{codec_registry::CodecRegistry, frame::Frame, registry::ReplyPayload};
    let (a, b, seen) = pair("batch-correlated", false).await;
    let outbound = a.links_snapshot().await[0].1.clone();
    let return_path = b.links_snapshot().await[0].1.clone();
    for reply_batch in [false, true] {
        let mut frames = Vec::new();
        let mut receivers = Vec::new();
        for value in [30, 10, 20] {
            let cid = a.callbacks.next_cid();
            let (tx, rx) = tokio::sync::oneshot::channel();
            a.callbacks
                .insert_on(cid, "batch-correlated-b", tx, &outbound)
                .unwrap();
            let (key, payload) = CodecRegistry::global()
                .encode_outgoing(&(Box::new(Ping(value)) as BoxedMessage))
                .unwrap();
            frames.push(if reply_batch {
                Frame::reply(cid, "/response", &key, payload.into())
            } else {
                Frame::ask(cid, "/user/probe", &key, payload.into(), Some("/response"))
            });
            receivers.push((value, rx));
        }
        frames.reverse();
        let sender = if reply_batch { &return_path } else { &outbound };
        sender.send(Frame::batch(&frames).unwrap()).await.unwrap();
        for (value, rx) in receivers {
            let result = tokio::time::timeout(Duration::from_secs(3), rx)
                .await
                .unwrap()
                .unwrap();
            let ReplyPayload::Ok(bytes, key) = result else {
                panic!("batch request failed")
            };
            let decoded = CodecRegistry::global()
                .decode_incoming(&key, &bytes)
                .unwrap()
                .downcast::<Ping>()
                .unwrap()
                .0;
            assert_eq!(decoded, value);
        }
        assert_eq!(a.callbacks.len(), 0);
    }
    assert_eq!(
        seen.lock().unwrap().len(),
        3,
        "only ASK members invoke the actor"
    );
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}
