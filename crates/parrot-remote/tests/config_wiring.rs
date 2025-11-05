use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_config::ParrotConfig;
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};
use std::{sync::Arc, time::Duration};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Probe(u64);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot.config.Probe#v1",
        type_id: std::any::TypeId::of::<Probe>(),
        encode: |message: &BoxedMessage| parrot_api::message::serde_remote_serialize(message.downcast_ref::<Probe>().ok_or("Probe")?),
        decode: |bytes: &[u8]| { let probe: Probe = parrot_api::message::serde_remote_deserialize(bytes)?; Ok(Box::new(probe) as BoxedMessage) },
    }
}
#[derive(Debug)]
struct Echo;
impl ActorRef for Echo {
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(msg)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/echo".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Echo)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Lookup;
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/echo").then(|| Box::new(Echo) as BoxedActorRef)
    }
}
fn resolved(scheme: &str, id: &str) -> parrot_config::Resolved {
    let mut builder = ParrotConfig::builder().remote_node_id(id);
    builder.remote.node.scheme = Some(scheme.into());
    if scheme == "quic" {
        builder.remote.security.mode = Some("insecure-development".into());
    }
    builder.remote.node.bind = Some("127.0.0.1:0".into());
    builder.build().unwrap()
}
#[test]
fn configured_transport_is_preserved() {
    for scheme in ["tcp", "mem", "quic"] {
        assert_eq!(
            RemoteConfig::from_resolved(&resolved(scheme, "configured"))
                .unwrap()
                .scheme,
            scheme
        );
    }
}
#[test]
fn memory_seed_is_not_silently_discarded() {
    let mut cfg = resolved("mem", "memory-client");
    cfg.remote.node.seeds = vec!["mem://memory-server".into()];
    let cfg = RemoteConfig::from_resolved(&cfg).unwrap();
    assert_eq!(cfg.seeds.len(), 1);
    assert_eq!(cfg.seeds[0].node_id, "memory-server");
}
#[tokio::test]
async fn start_bootstraps_configured_seed_without_manual_connect() {
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("configured-server", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Lookup),
    )
    .unwrap();
    server.start().await.unwrap();
    let mut cfg = resolved("tcp", "configured-client");
    cfg.remote.node.seeds = vec![format!(
        "tcp://configured-server@{}",
        server.local_addr().unwrap()
    )];
    let client =
        RemoteActorSystem::new(RemoteConfig::from_resolved(&cfg).unwrap(), Arc::new(Lookup))
            .unwrap();
    client.start().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(actor) = client.remote_ref("parrot://configured-server/echo") {
                if let Ok(reply) = actor
                    .send_with_timeout(Box::new(Probe(42)), Some(Duration::from_millis(250)))
                    .await
                {
                    assert_eq!(reply.downcast_ref::<Probe>().unwrap().0, 42);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    result.expect("configured seed never became connected");
}

async fn assert_echo(system: &Arc<RemoteActorSystem>, node: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(actor) = system.remote_ref(&format!("parrot://{node}/echo")) {
                if let Ok(reply) = actor
                    .send_with_timeout(Box::new(Probe(73)), Some(Duration::from_millis(300)))
                    .await
                {
                    assert_eq!(reply.downcast_ref::<Probe>().unwrap().0, 73);
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("configured seed did not serve business traffic");
}

#[tokio::test]
async fn configured_memory_and_quic_seeds_serve_business_traffic() {
    for scheme in ["mem", "quic"] {
        let server_id = format!("{scheme}-bootstrap-server");
        let server_cfg = RemoteConfig::from_resolved(&resolved(scheme, &server_id)).unwrap();
        assert_eq!(server_cfg.bind.is_none(), scheme == "mem");
        let server = RemoteActorSystem::new(server_cfg, Arc::new(Lookup)).unwrap();
        server.start().await.unwrap();
        let mut cfg = resolved(scheme, &format!("{scheme}-bootstrap-client"));
        cfg.remote.node.seeds = vec![if scheme == "mem" {
            format!("mem://{server_id}")
        } else {
            format!("quic://{server_id}@{}", server.local_addr().unwrap())
        }];
        let client =
            RemoteActorSystem::new(RemoteConfig::from_resolved(&cfg).unwrap(), Arc::new(Lookup))
                .unwrap();
        client.start().await.unwrap();
        assert_echo(&client, &server_id).await;
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn dns_alternatives_connect_to_ipv4_listener_and_learn_actual_identity_once() {
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("dns-actual-server", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Lookup),
    )
    .unwrap();
    server.start().await.unwrap();
    let mut cfg = resolved("tcp", "dns-client");
    cfg.remote.node.seeds = vec![format!("localhost:{}", server.local_addr().unwrap().port())];
    assert!(
        RemoteConfig::from_resolved(&cfg).is_err(),
        "sync conversion must not discard DNS"
    );
    let cfg = RemoteConfig::from_resolved_async(&cfg, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(!cfg.seeds.is_empty());
    assert!(cfg
        .seeds
        .iter()
        .all(|seed| seed.node_id.starts_with("localhost:")));
    let client = RemoteActorSystem::new(cfg, Arc::new(Lookup)).unwrap();
    client.start().await.unwrap();
    assert_echo(&client, "dns-actual-server").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        client.links_snapshot().await.len(),
        1,
        "learned node identity must not cause repeated dialing"
    );
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn unavailable_seed_retries_until_late_server_starts() {
    let mut cfg = resolved("mem", "late-client");
    cfg.remote.node.seeds = vec!["mem://late-server".into()];
    let client =
        RemoteActorSystem::new(RemoteConfig::from_resolved(&cfg).unwrap(), Arc::new(Lookup))
            .unwrap();
    client.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(180)).await;
    assert!(client.links_snapshot().await.is_empty());
    let server =
        RemoteActorSystem::new(RemoteConfig::mem("late-server"), Arc::new(Lookup)).unwrap();
    server.start().await.unwrap();
    assert_echo(&client, "late-server").await;
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_joins_seed_retry_and_accept_tasks() {
    let client = RemoteActorSystem::new(
        RemoteConfig::mem("cancel-client")
            .with_seeds(vec![parrot_remote::NodeAddr::mem("cancel-server")]),
        Arc::new(Lookup),
    )
    .unwrap();
    client.start().await.unwrap();
    client.start().await.unwrap(); // idempotent, no second accept worker
    tokio::time::sleep(Duration::from_millis(80)).await;
    client.shutdown().await.unwrap();
    let weak = Arc::downgrade(&client);
    assert!(client.start().await.is_err());
    assert!(client
        .connect(&parrot_remote::NodeAddr::mem("cancel-server"))
        .await
        .is_err());
    drop(client);
    assert!(
        weak.upgrade().is_none(),
        "shutdown must release background Arc owners before returning"
    );
    let server =
        RemoteActorSystem::new(RemoteConfig::mem("cancel-server"), Arc::new(Lookup)).unwrap();
    server.start().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        server.links_snapshot().await.is_empty(),
        "stopped seed retry connected later"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_start_does_not_retain_system_and_prestart_shutdown_is_terminal() {
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let system = RemoteActorSystem::new(
        RemoteConfig::tcp("failed-bind", Some(occupied.local_addr().unwrap())),
        Arc::new(Lookup),
    )
    .unwrap();
    let weak = Arc::downgrade(&system);
    assert!(system.start().await.is_err());
    drop(system);
    assert!(
        weak.upgrade().is_none(),
        "failed bind leaked an ingress task"
    );
    let system =
        RemoteActorSystem::new(RemoteConfig::tcp("never-started", None), Arc::new(Lookup)).unwrap();
    system.shutdown().await.unwrap();
    assert!(
        system.start().await.is_err(),
        "shutdown must persist even without watch receivers"
    );
}

#[tokio::test]
async fn malformed_resolved_values_and_dns_failures_are_errors() {
    for (scheme, bind, seed) in [
        ("udp", "127.0.0.1:0", "127.0.0.1:1"),
        ("tcp", "not-a-bind", "127.0.0.1:1"),
        ("tcp", "127.0.0.1:0", "tcp://localhost:0"),
        ("tcp", "127.0.0.1:0", "mem://peer"),
    ] {
        let mut cfg = resolved("tcp", "invalid-resolved");
        cfg.remote.node.scheme = scheme.into();
        cfg.remote.node.bind = Some(bind.into());
        cfg.remote.node.seeds = vec![seed.into()];
        assert!(RemoteConfig::from_resolved(&cfg).is_err());
        assert!(
            RemoteConfig::from_resolved_async(&cfg, Duration::from_secs(1))
                .await
                .is_err()
        );
    }
    let mut cfg = resolved("tcp", "dns-failure");
    cfg.remote.node.seeds = vec!["tcp://nonexistent-parrot-seed.invalid:1234".into()];
    let error = match RemoteConfig::from_resolved_async(&cfg, Duration::from_secs(2)).await {
        Ok(_) => panic!("nonexistent DNS seed accepted"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("DNS") && error.contains("seeds[0]"),
        "{error}"
    );
    assert!(RemoteConfig::from_resolved_async(&cfg, Duration::ZERO)
        .await
        .is_err());
}

#[test]
fn direct_configs_reject_invalid_values_before_transport_side_effects() {
    let mut configs = Vec::new();
    let mut cfg = RemoteConfig::tcp("unknown-scheme", None);
    cfg.scheme = "udp";
    configs.push(cfg);
    configs.push(RemoteConfig::mem(""));
    configs.push(
        RemoteConfig::tcp("wrong-seed", None)
            .with_seeds(vec![parrot_remote::NodeAddr::mem("peer")]),
    );
    configs.push(RemoteConfig::tcp("zero-port", None).with_seeds(vec![
        parrot_remote::NodeAddr::tcp("peer", "127.0.0.1:0".parse().unwrap()),
    ]));
    for knobs in [
        parrot_remote::transport::RuntimeKnobs {
            inbound_ask_capacity: 0,
            ..Default::default()
        },
        parrot_remote::transport::RuntimeKnobs {
            inbound_ask_capacity: usize::MAX,
            ..Default::default()
        },
        parrot_remote::transport::RuntimeKnobs {
            inbound_ask_bytes: 0,
            ..Default::default()
        },
        parrot_remote::transport::RuntimeKnobs {
            inbound_ask_bytes: usize::MAX,
            ..Default::default()
        },
        parrot_remote::transport::RuntimeKnobs {
            outbound_queue: 0,
            ..Default::default()
        },
        parrot_remote::transport::RuntimeKnobs {
            outbound_queue: usize::MAX,
            ..Default::default()
        },
        parrot_remote::transport::RuntimeKnobs {
            heartbeat_interval: Duration::ZERO,
            ..Default::default()
        },
        parrot_remote::transport::RuntimeKnobs {
            heartbeat_interval: Duration::MAX,
            ..Default::default()
        },
        parrot_remote::transport::RuntimeKnobs {
            heartbeat_max_loss: 0,
            ..Default::default()
        },
        parrot_remote::transport::RuntimeKnobs {
            default_hop_limit: 0,
            ..Default::default()
        },
        parrot_remote::transport::RuntimeKnobs {
            default_hop_limit: 65,
            ..Default::default()
        },
    ] {
        configs.push(RemoteConfig::tcp("invalid-knob", None).with_knobs(knobs));
    }
    for config in configs {
        assert!(RemoteActorSystem::new(config, Arc::new(Lookup)).is_err());
    }
}

#[tokio::test]
async fn unavailable_seeds_do_not_starve_later_reachable_seed() {
    // TCP accepts but never completes Parrot's handshake, occupying the first
    // four connection attempts until their deadline. The fifth seed must run.
    let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("z-reachable", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Lookup),
    )
    .unwrap();
    server.start().await.unwrap();
    let mut seeds: Vec<_> = (0..4)
        .map(|i| {
            parrot_remote::NodeAddr::tcp(format!("a-blocked-{i}"), blackhole.local_addr().unwrap())
        })
        .collect();
    seeds.push(parrot_remote::NodeAddr::tcp(
        "z-reachable",
        server.local_addr().unwrap(),
    ));
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("fair-bootstrap", None).with_seeds(seeds),
        Arc::new(Lookup),
    )
    .unwrap();
    client.start().await.unwrap();
    assert_echo(&client, "z-reachable").await;
    tokio::time::timeout(Duration::from_secs(1), client.shutdown())
        .await
        .unwrap()
        .unwrap();
    server.shutdown().await.unwrap();
}

async fn accept_raw_peer(
    listener: tokio::net::TcpListener,
) -> (
    tokio_util::codec::Framed<tokio::net::TcpStream, parrot_remote::transport::FrameCodec>,
    parrot_remote::handshake::HandshakeBody,
) {
    use futures::{SinkExt, StreamExt};
    use parrot_remote::{
        frame::{frame_type, Frame},
        handshake::{HandshakeAckBody, HandshakeBody},
    };
    let (stream, _) = listener.accept().await.unwrap();
    let mut peer = tokio_util::codec::Framed::new(stream, parrot_remote::transport::FrameCodec);
    let hello = peer.next().await.unwrap().unwrap();
    assert_eq!(hello.header.frame_type, frame_type::HANDSHAKE);
    let hello = HandshakeBody::decode_tlv(&hello.payload).unwrap();
    let mut body = bytes::BytesMut::new();
    HandshakeAckBody {
        node_id: "raw-peer".into(),
        realm: None,
        cluster: None,
        capabilities: parrot_remote::handshake::caps::BIN,
        max_frame_len: parrot_remote::frame::MAX_FRAME_LEN,
        topology_role: parrot_remote::handshake::TopologyRole::Normal,
        hop_limit: 8,
        direct_addr: None,
        chosen_codec: "bin".into(),
    }
    .encode_tlv(&mut body)
    .unwrap();
    peer.send(Frame::handshake_ack(body.freeze()))
        .await
        .unwrap();
    (peer, hello)
}

#[tokio::test]
async fn configured_heartbeat_closes_a_live_but_silent_tcp_peer() {
    use futures::StreamExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let raw = tokio::spawn(async move {
        let (mut peer, _) = accept_raw_peer(listener).await;
        let start = std::time::Instant::now();
        let mut heartbeats = 0;
        while let Some(frame) = peer.next().await {
            assert_eq!(
                frame.unwrap().header.frame_type,
                parrot_remote::frame::frame_type::HEARTBEAT
            );
            heartbeats += 1;
            // Keep the socket open, draining frames without replying. EOF must
            // come from the configured detector, not from dropping our peer.
        }
        (start.elapsed(), heartbeats)
    });
    let cfg = ParrotConfig::builder()
        .remote_node_id("heartbeat-runtime")
        .remote_heartbeat_interval_ms(150)
        .remote_heartbeat_max_loss(2)
        .build()
        .unwrap();
    let client =
        RemoteActorSystem::new(RemoteConfig::from_resolved(&cfg).unwrap(), Arc::new(Lookup))
            .unwrap();
    client.start().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        client.connect(&parrot_remote::NodeAddr::tcp("raw-peer", address)),
    )
    .await
    .unwrap()
    .unwrap();
    let (elapsed, heartbeats) = tokio::time::timeout(Duration::from_secs(2), raw)
        .await
        .expect("configured half-open detection failed")
        .unwrap();
    assert!(
        elapsed >= Duration::from_millis(250),
        "premature disconnect: {elapsed:?}"
    );
    assert!(heartbeats >= 1, "peer closed without exercising heartbeat");
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn configured_hop_limit_is_on_the_handshake_and_actual_actor_frames() {
    use futures::{SinkExt, StreamExt};
    use parrot_remote::frame::{frame_type, Frame};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let raw = tokio::spawn(async move {
        let (mut peer, hello) = accept_raw_peer(listener).await;
        let mut observed = Vec::new();
        while observed.len() < 3 {
            let frame = peer.next().await.unwrap().unwrap();
            if frame.header.frame_type == frame_type::HEARTBEAT {
                peer.send(Frame::heartbeat_ack()).await.unwrap();
                continue;
            }
            observed.push((frame.header.frame_type, frame.header.hop_limit));
            if frame.header.frame_type == frame_type::ASK {
                let (_, payload) = frame.split_reply_to().unwrap();
                peer.send(Frame::reply(
                    frame.header.correlation_id,
                    &frame.path,
                    &frame.type_key,
                    payload,
                ))
                .await
                .unwrap();
            }
        }
        (hello.hop_limit, observed)
    });
    let cfg = ParrotConfig::builder()
        .remote_node_id("hop-runtime")
        .remote_default_hop_limit(16)
        .build()
        .unwrap();
    let client =
        RemoteActorSystem::new(RemoteConfig::from_resolved(&cfg).unwrap(), Arc::new(Lookup))
            .unwrap();
    client.start().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        client.connect(&parrot_remote::NodeAddr::tcp("raw-peer", address)),
    )
    .await
    .unwrap()
    .unwrap();
    let actor = client.remote_ref("parrot://raw-peer/echo").unwrap();
    let reply = actor
        .send_with_timeout(Box::new(Probe(11)), Some(Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(reply.downcast_ref::<Probe>().unwrap().0, 11);
    actor.deliver(Box::new(Probe(12))).await.unwrap();
    actor.stop().await.unwrap();
    let (hello_limit, frames) = tokio::time::timeout(Duration::from_secs(2), raw)
        .await
        .unwrap()
        .unwrap();
    client.shutdown().await.unwrap();
    assert_eq!(hello_limit, 16);
    assert_eq!(
        frames,
        vec![
            (frame_type::ASK, 16),
            (frame_type::TELL, 16),
            (frame_type::STOP, 16)
        ]
    );
}

#[tokio::test]
async fn retained_dns_sources_are_distinct_from_snapshot_and_explicit_override() {
    let mut input = resolved("tcp", "source-config");
    input.remote.node.seeds = vec!["tcp://peer@localhost:9000".into()];
    let config = RemoteConfig::from_resolved_async(&input, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(config.seed_endpoints.len(), 1);
    assert_eq!(config.seed_endpoints[0].host.as_deref(), Some("localhost"));
    assert!(!config.seeds.is_empty());
    let source = RemoteActorSystem::new(config.clone(), Arc::new(Lookup)).unwrap();
    assert!(source.nodes.contains("peer"));
    assert!(
        source.nodes.get("peer").is_none(),
        "initial DNS snapshot must not become a permanent static seed"
    );
    assert!(
        matches!(source.nodes.endpoint("peer"), Some(parrot_remote::NodeEndpoint::Dns { host, .. }) if host == "localhost")
    );
    source.shutdown().await.unwrap();
    let replacement =
        parrot_remote::NodeAddr::tcp("replacement", "127.0.0.1:9001".parse().unwrap());
    let config = config.with_seeds(vec![replacement.clone()]);
    assert!(config.seed_endpoints.is_empty());
    let source = RemoteActorSystem::new(config, Arc::new(Lookup)).unwrap();
    assert!(!source.nodes.contains("peer"));
    assert_eq!(source.nodes.get("replacement").unwrap().0, replacement);
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_retained_seed_source_is_rejected_before_binding() {
    let mut input = resolved("tcp", "invalid-source-config");
    input.remote.node.seeds = vec!["tcp://peer@localhost:9000".into()];
    let mut config = RemoteConfig::from_resolved_async(&input, Duration::from_secs(2))
        .await
        .unwrap();
    config.seed_endpoints[0].host = Some("bad/host".into());
    assert!(RemoteActorSystem::new(config, Arc::new(Lookup)).is_err());
}
