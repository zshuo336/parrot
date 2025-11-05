//! Assert actual TCP next hops and destination execution, not configuration values.
use futures::FutureExt;
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_config::ParrotConfig;
use parrot_remote::{
    capture::{CaptureDirection, CaptureLimits, FrameCapture},
    frame::frame_type,
    swim::{sync_to_node_table, Member, MemberEvent, MemberStatus, NodeAddrWire},
    topology::TopologyMode,
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig, TopologyRole,
};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Ping(u64);
parrot_api::message::inventory::submit! { parrot_api::message::CodecRegistration {
    type_key: "bin:parrot.topology.routing.Ping#v1", type_id: std::any::TypeId::of::<Ping>(),
    encode: |m: &BoxedMessage| parrot_api::message::serde_remote_serialize(m.downcast_ref::<Ping>().ok_or("Ping")?),
    decode: |b: &[u8]| { let p: Ping = parrot_api::message::serde_remote_deserialize(b)?; Ok(Box::new(p) as BoxedMessage) },
} }

#[derive(Debug, Clone, Default)]
struct Echo(Arc<AtomicUsize>, Arc<Mutex<Vec<u64>>>);
impl ActorRef for Echo {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async {
            self.1.lock().unwrap().push(
                message
                    .downcast_ref::<Ping>()
                    .expect("routed ASK payload")
                    .0,
            );
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(message)
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        m: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(m)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.1.lock().unwrap().push(
                message
                    .downcast_ref::<Ping>()
                    .expect("routed TELL payload")
                    .0,
            );
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
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
impl LocalLookup for Echo {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/echo").then(|| Box::new(self.clone()) as BoxedActorRef)
    }
}

fn config(id: &str, listener: bool) -> RemoteConfig {
    let mut config = RemoteConfig::tcp(id, listener.then(|| "127.0.0.1:0".parse().unwrap()))
        .with_insecure_development();
    config.heartbeat_enabled = false;
    config
}

struct Network {
    source: Arc<RemoteActorSystem>,
    hub: Arc<RemoteActorSystem>,
    target: Arc<RemoteActorSystem>,
    calls: Echo,
    capture: FrameCapture,
    recording: std::thread::JoinHandle<Vec<String>>,
}

impl Network {
    async fn new(mode: TopologyMode, fallback: bool, target_listens: bool) -> Self {
        Self::with_source_role(mode, fallback, target_listens, TopologyRole::Normal).await
    }

    async fn with_source_role(
        mode: TopologyMode,
        fallback: bool,
        target_listens: bool,
        role: TopologyRole,
    ) -> Self {
        let (capture, mut receiver) = FrameCapture::channel(CaptureLimits::default()).unwrap();
        let recording = std::thread::spawn(move || {
            let mut peers = Vec::new();
            while let Some(event) = receiver.blocking_recv() {
                if event.direction == CaptureDirection::Tx
                    && matches!(
                        event.frame.header.frame_type,
                        frame_type::ASK | frame_type::TELL
                    )
                    && event.frame.path == "parrot://topology-target/echo"
                {
                    peers.push(
                        event
                            .wire
                            .peer_node
                            .expect("business writer has a verified peer"),
                    );
                }
            }
            peers
        });
        // Exercise the real config folding and runtime conversion for every mode.
        let mut resolved = ParrotConfig::builder()
            .remote_node_id("topology-source")
            .remote_bind("127.0.0.1:0")
            .topology_mode(mode)
            .topology_relay_fallback(fallback)
            .build()
            .unwrap();
        resolved.remote.security = parrot_config::RemoteSecurityResolved::InsecureDevelopment;
        let mut source_config = RemoteConfig::from_resolved(&resolved)
            .unwrap()
            .with_capture(capture.clone());
        source_config.heartbeat_enabled = false;
        let source =
            RemoteActorSystem::new(source_config.with_role(role), Arc::new(Echo::default()))
                .unwrap();
        let hub = RemoteActorSystem::new(
            config("topology-hub", true).with_role(TopologyRole::Hub),
            Arc::new(Echo::default()),
        )
        .unwrap();
        let calls = Echo::default();
        let target = RemoteActorSystem::new(
            config("topology-target", target_listens),
            Arc::new(calls.clone()),
        )
        .unwrap();
        for system in [&source, &hub, &target] {
            system.start().await.unwrap();
        }
        target
            .connect(&NodeAddr::tcp("topology-hub", hub.local_addr().unwrap()))
            .await
            .unwrap();
        source
            .connect(&NodeAddr::tcp("topology-hub", hub.local_addr().unwrap()))
            .await
            .unwrap();
        // The hub's accepted half must be registered before testing relay behavior.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !hub
                .links_snapshot()
                .await
                .iter()
                .any(|(id, _, _)| id == "topology-target")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        Self {
            source,
            hub,
            target,
            calls,
            capture,
            recording,
        }
    }

    async fn discover(&self) {
        let address = self.target.local_addr().unwrap();
        self.advertise(address).await;
    }

    async fn advertise(&self, address: std::net::SocketAddr) {
        let mut membership = self.source.membership.lock().await;
        let incarnation = membership
            .members
            .get("topology-target")
            .map_or(1, |member| member.incarnation + 1);
        membership.merge_event(&MemberEvent::Upsert(Member {
            node_id: "topology-target".into(),
            addr: NodeAddrWire {
                node_id: "topology-target".into(),
                scheme: "tcp".into(),
                host: "127.0.0.1".into(),
                port: address.port(),
            },
            status: MemberStatus::Alive,
            incarnation,
            status_until_ms: 0,
            metadata: Default::default(),
        }));
        drop(membership);
        sync_to_node_table(&self.source.membership, &self.source.nodes).await;
        assert_eq!(
            self.source.nodes.get("topology-target").unwrap().0.addr,
            address
        );
    }

    async fn exchange(&self, expected: bool) {
        let actor = self.source.remote_ref("parrot://topology-target/echo");
        if !expected && actor.is_err() {
            return;
        }
        let actor = actor.unwrap();
        let ask = actor
            .send_with_timeout(Box::new(Ping(71)), Some(Duration::from_secs(2)))
            .await;
        assert_eq!(ask.is_ok(), expected, "ASK error: {:?}", ask.as_ref().err());
        if let Ok(reply) = ask {
            assert_eq!(reply.downcast_ref::<Ping>().unwrap().0, 71);
        }
        let tell = actor
            .deliver_with_options(
                Box::new(Ping(72)),
                parrot_api::message::MessageOptions {
                    timeout: Some(Duration::from_secs(2)),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(tell.is_ok(), expected, "TELL: {tell:?}");
        let completion = actor
            .deliver_with_ack(Box::new(Ping(73)), Duration::from_secs(2))
            .await;
        assert_eq!(completion.is_ok(), expected, "completion: {completion:?}");
        if expected {
            tokio::time::timeout(Duration::from_secs(2), async {
                while self.calls.0.load(Ordering::SeqCst) != 3 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("three distinct business operations must execute exactly once");
        } else {
            assert_eq!(self.calls.0.load(Ordering::SeqCst), 0);
        }
    }

    async fn finish(self) -> Vec<String> {
        for system in [&self.source, &self.hub, &self.target] {
            system.shutdown().await.unwrap();
        }
        let mut messages = self.calls.1.lock().unwrap().clone();
        messages.sort_unstable();
        self.capture.close();
        let peers = tokio::task::spawn_blocking(move || self.recording.join().unwrap())
            .await
            .unwrap();
        if !messages.is_empty() {
            assert_eq!(
                messages,
                [71, 72, 73],
                "retired destination must preserve each business payload exactly once"
            );
        }
        assert_eq!(
            self.capture.stats().lost,
            0,
            "route assertions require complete capture"
        );
        peers
    }
}

#[tokio::test]
async fn existing_direct_link_obeys_every_mode_and_hub_loss_policy() {
    for mode in [TopologyMode::Hub, TopologyMode::Mesh, TopologyMode::Hybrid] {
        for fallback in [false, true] {
            for hub_alive in [false, true] {
                let network = Network::new(mode, fallback, true).await;
                network
                    .source
                    .connect(&NodeAddr::tcp(
                        "topology-target",
                        network.target.local_addr().unwrap(),
                    ))
                    .await
                    .unwrap();
                if !hub_alive {
                    network.hub.shutdown().await.unwrap();
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while network
                            .source
                            .missing_direct_connections(&["topology-hub"])
                            .is_empty()
                        {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("observe physical hub loss before testing fallback");
                }
                let expected = mode != TopologyMode::Hub || hub_alive || fallback;
                let result = AssertUnwindSafe(network.exchange(expected))
                    .catch_unwind()
                    .await;
                let peers = network.finish().await;
                if let Err(panic) = result {
                    std::panic::resume_unwind(panic);
                }
                let peer = if mode == TopologyMode::Hub && hub_alive {
                    "topology-hub"
                } else {
                    "topology-target"
                };
                assert_eq!(
                    peers,
                    if expected {
                        vec![peer.to_owned(); 3]
                    } else {
                        vec![]
                    },
                    "{mode:?}, fallback={fallback}, hub={hub_alive}"
                );
            }
        }
    }
}

#[tokio::test]
async fn first_discovered_call_prefers_direct_in_mesh_and_hybrid() {
    for mode in [TopologyMode::Mesh, TopologyMode::Hybrid] {
        let network = Network::new(mode, true, true).await;
        network.discover().await;
        assert!(!network
            .source
            .links_snapshot()
            .await
            .iter()
            .any(|(id, _, _)| id == "topology-target"));
        let result = AssertUnwindSafe(network.exchange(true))
            .catch_unwind()
            .await;
        let peers = network.finish().await;
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
        assert_eq!(peers, vec!["topology-target".to_owned(); 3], "{mode:?}");
    }
}

#[tokio::test]
async fn outbound_only_destination_requires_the_permitted_relay_policy() {
    for mode in [TopologyMode::Hub, TopologyMode::Mesh, TopologyMode::Hybrid] {
        for fallback in [false, true] {
            let network = Network::new(mode, fallback, false).await;
            let expected = mode == TopologyMode::Hub || mode == TopologyMode::Hybrid && fallback;
            let result = AssertUnwindSafe(network.exchange(expected))
                .catch_unwind()
                .await;
            let peers = network.finish().await;
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
            assert_eq!(
                peers,
                if expected {
                    vec!["topology-hub".to_owned(); 3]
                } else {
                    vec![]
                },
                "{mode:?}, fallback={fallback}"
            );
        }
    }
}

#[tokio::test]
async fn failed_direct_handshake_falls_back_only_when_enabled() {
    use tokio::io::AsyncReadExt;
    for fallback in [false, true] {
        let network = Network::new(TopologyMode::Hybrid, fallback, false).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        network.advertise(listener.local_addr().unwrap()).await;
        let mut attempts = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut byte = [0u8; 1];
                socket
                    .read_exact(&mut byte)
                    .await
                    .expect("direct attempt sent HELLO");
                // Reject the actual handshake before any business frame is admitted.
            }
        });
        let result = AssertUnwindSafe(network.exchange(fallback))
            .catch_unwind()
            .await;
        let attempts_result = tokio::time::timeout(Duration::from_secs(2), &mut attempts).await;
        if attempts_result.is_err() {
            attempts.abort();
            let _ = attempts.await;
        }
        let peers = network.finish().await;
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
        attempts_result
            .expect("three direct handshakes must actually be attempted")
            .unwrap();
        assert_eq!(
            peers,
            if fallback {
                vec!["topology-hub".to_owned(); 3]
            } else {
                vec![]
            }
        );
    }
}

#[tokio::test]
async fn caller_deadline_cancels_silent_direct_dial_without_late_relay() {
    use tokio::io::AsyncReadExt;
    let network = Network::new(TopologyMode::Hybrid, true, false).await;
    for operation in 0..3 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        network.advertise(listener.local_addr().unwrap()).await;
        let mut observer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut hello = Vec::new();
            socket.read_to_end(&mut hello).await.unwrap();
            hello
        });
        let actor = network
            .source
            .remote_ref("parrot://topology-target/echo")
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), async {
            let timeout = Duration::from_millis(100);
            match operation {
                0 => actor
                    .send_with_timeout(Box::new(Ping(1)), Some(timeout))
                    .await
                    .map(|_| ()),
                1 => {
                    actor
                        .deliver_with_options(
                            Box::new(Ping(2)),
                            parrot_api::message::MessageOptions {
                                timeout: Some(timeout),
                                ..Default::default()
                            },
                        )
                        .await
                }
                _ => actor.deliver_with_ack(Box::new(Ping(3)), timeout).await,
            }
        })
        .await;
        let observed = tokio::time::timeout(Duration::from_secs(1), &mut observer).await;
        if observed.is_err() {
            observer.abort();
            let _ = observer.await;
        }
        assert!(result
            .expect("caller deadline must bound the direct dial")
            .is_err());
        assert!(!observed
            .expect("cancelled handshake must close its socket")
            .unwrap()
            .is_empty());
    }
    let calls = network.calls.0.load(Ordering::SeqCst);
    let peers = network.finish().await;
    assert_eq!(
        calls, 0,
        "expired business calls must not execute later through the hub"
    );
    assert!(
        peers.is_empty(),
        "expired business calls published frames: {peers:?}"
    );
}

#[tokio::test]
async fn strict_hub_policy_does_not_redirect_management_to_the_hub() {
    let network = Network::new(TopologyMode::Hub, false, true).await;
    network.discover().await;
    let result = network.source.admin_stop("topology-target", "/echo").await;
    let calls = network.calls.0.load(Ordering::SeqCst);
    let direct = network
        .source
        .missing_direct_connections(&["topology-target"]);
    let peers = network.finish().await;
    assert!(
        result.is_ok(),
        "direct management failed under strict hub: {result:?}"
    );
    assert_eq!(calls, 1, "management must execute at the named destination");
    assert!(
        direct.is_empty(),
        "management requires the real target connection"
    );
    assert!(
        peers.is_empty(),
        "management must not masquerade as relayed business"
    );
}

#[tokio::test]
async fn relay_endpoint_originates_direct_business_in_strict_star_mode() {
    for role in [TopologyRole::Hub, TopologyRole::Border] {
        let network = Network::with_source_role(TopologyMode::Hub, false, true, role).await;
        network.discover().await;
        let result = AssertUnwindSafe(network.exchange(true))
            .catch_unwind()
            .await;
        let peers = network.finish().await;
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
        assert_eq!(peers, vec!["topology-target".to_owned(); 3], "{role:?}");
    }
}
