//! Regression tests exercising real in-memory transport, codec, ref and ingress.
use parrot_api::{
    address::ActorRef,
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
    let seen = Arc::new(Mutex::new(vec![]));
    let mut config = RemoteConfig::mem(id);
    if hub {
        config.topology_role = parrot_remote::handshake::TopologyRole::Hub;
    }
    let system = RemoteActorSystem::new(
        config,
        Arc::new(Lookup(Sink {
            seen: seen.clone(),
            pending: false,
            fail: false,
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
    let (a, a_seen) = node("source-a", false).await;
    let (b, b_seen) = node("source-b", false).await;
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
