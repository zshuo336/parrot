use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Ping(u64);
parrot_api::message::inventory::submit! { parrot_api::message::CodecRegistration {
    type_key:"bin:parrot.dns.discovery.Ping#v1", type_id:std::any::TypeId::of::<Ping>(),
    encode:|m:&BoxedMessage|parrot_api::message::serde_remote_serialize(m.downcast_ref::<Ping>().ok_or("Ping")?),
    decode:|b:&[u8]|{let m:Ping=parrot_api::message::serde_remote_deserialize(b)?;Ok(Box::new(m) as BoxedMessage)},
} }
#[derive(Clone, Debug, Default)]
struct Calls(Arc<AtomicUsize>);
impl ActorRef for Calls {
    fn send<'a>(&'a self, m: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
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
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn path(&self) -> String {
        "/echo".into()
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
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        Some(Box::new(self.clone()))
    }
}
fn node(id: &str, calls: &Calls) -> Arc<RemoteActorSystem> {
    RemoteActorSystem::new(
        RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap())).with_insecure_development(),
        Arc::new(calls.clone()),
    )
    .unwrap()
}
async fn dns_first(admin: bool) {
    let count = Calls::default();
    // Choose a kernel-assigned port for the hostname announcement; the actual
    // transport must bind it successfully before this test proceeds.
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let c = RemoteActorSystem::new(
        RemoteConfig::tcp("dns-target", Some(address))
            .with_insecure_development()
            .with_direct_addr(format!("localhost:{}", address.port())),
        Arc::new(count.clone()),
    )
    .unwrap();
    let a = node("dns-source", &Calls::default());
    let b = node("dns-middle", &Calls::default());
    for s in [&a, &b, &c] {
        s.start().await.unwrap();
    }
    a.connect(&NodeAddr::tcp("dns-middle", b.local_addr().unwrap()))
        .await
        .unwrap();
    b.connect(&NodeAddr::tcp("dns-target", c.local_addr().unwrap()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if a.membership
                .lock()
                .await
                .members
                .get("dns-target")
                .is_some_and(|m| m.addr.host == "localhost")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        a.links_snapshot().await.len(),
        1,
        "hostname discovery must not eagerly build a full mesh"
    );
    let result = if admin {
        a.admin_stop("dns-target", "/echo")
            .await
            .map_err(|e| e.to_string())
    } else {
        match a.remote_ref("parrot://dns-target/echo") {
            Ok(actor) => actor
                .send_with_timeout(Box::new(Ping(42)), Some(Duration::from_secs(2)))
                .await
                .map(|reply| assert_eq!(reply.downcast_ref::<Ping>().unwrap().0, 42))
                .map_err(|e| e.to_string()),
            Err(error) => Err(error.to_string()),
        }
    };
    let calls = count.0.load(Ordering::SeqCst);
    for s in [&a, &b, &c] {
        s.shutdown().await.unwrap();
    }
    assert!(
        result.is_ok(),
        "first DNS discovery operation failed: {result:?}"
    );
    assert_eq!(calls, 1);
}
#[tokio::test]
async fn dns_gossip_serves_the_first_real_business_call() {
    dns_first(false).await;
}
#[tokio::test]
async fn dns_gossip_serves_the_first_real_admin_call() {
    dns_first(true).await;
}

async fn advertise(system: &RemoteActorSystem, id: &str, port: u16, incarnation: u64) {
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
                host: "localhost".into(),
                port,
            },
            status: MemberStatus::Alive,
            incarnation,
            status_until_ms: 0,
            metadata: Default::default(),
        }));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !system.nodes.contains(id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn silent_dns(drop_owner: bool) {
    use tokio::io::AsyncReadExt;
    let source = node("dns-silent-source", &Calls::default());
    source.start().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    advertise(
        &source,
        "dns-silent-target",
        listener.local_addr().unwrap().port(),
        1,
    )
    .await;
    let actor = source
        .remote_ref("parrot://dns-silent-target/echo")
        .unwrap();
    let timeout = if drop_owner {
        None
    } else {
        Some(Duration::from_millis(100))
    };
    let request =
        tokio::spawn(async move { actor.send_with_timeout(Box::new(Ping(1)), timeout).await });
    let (mut socket, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut first = [0u8; 1];
    socket.read_exact(&mut first).await.unwrap();
    let weak = Arc::downgrade(&source);
    if drop_owner {
        drop(source);
        assert!(weak.upgrade().is_none());
    }
    let result = tokio::time::timeout(Duration::from_secs(1), request)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    if !drop_owner {
        assert!(matches!(
            result,
            Err(parrot_api::errors::ActorError::Timeout
                | parrot_api::errors::ActorError::TimeoutDetail(_))
        ));
    }
    let mut remaining = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), socket.read_to_end(&mut remaining))
        .await
        .unwrap()
        .unwrap();
    assert!(!remaining.is_empty(), "actual HELLO must precede EOF");
    if let Some(source) = weak.upgrade() {
        assert!(source.callbacks.is_empty());
        assert!(source.links_snapshot().await.is_empty());
        source.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn dns_handshake_uses_the_callers_total_budget() {
    silent_dns(false).await;
}
#[tokio::test]
async fn dropping_system_cancels_dns_handshake_and_releases_socket() {
    silent_dns(true).await;
}
