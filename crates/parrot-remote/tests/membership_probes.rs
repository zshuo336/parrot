use futures::{SinkExt, StreamExt};
use parrot_api::types::BoxedActorRef;
use parrot_remote::{
    admin::{decode_sys_event, SysEvent},
    capture::{CaptureDirection, CaptureLimits, FrameCapture},
    frame::{frame_type, Frame},
    handshake::{caps, HandshakeBody},
    node::NodeAddr,
    swim::{ProbeEvent, ProbeOutcome, SwimConfig},
    transport::FrameCodec,
    LocalLookup, RemoteActorSystem, RemoteConfig,
};
use std::{sync::Arc, time::Duration};
use tokio_util::codec::Framed;
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
fn config(id: &str) -> RemoteConfig {
    RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap()))
        .with_insecure_development()
        .with_swim_config(SwimConfig {
            probe_interval: Duration::from_secs(60),
            gossip_interval: Duration::from_secs(60),
            probe_timeout: Duration::from_millis(300),
            ..Default::default()
        })
}
async fn node(id: &str) -> Arc<RemoteActorSystem> {
    let node = RemoteActorSystem::new(config(id), Arc::new(Empty)).unwrap();
    node.start().await.unwrap();
    node
}
async fn connect(a: &Arc<RemoteActorSystem>, b: &Arc<RemoteActorSystem>) {
    a.connect(&NodeAddr::tcp(&b.config.node_id, b.local_addr().unwrap()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !b
            .missing_direct_connections(&[&a.config.node_id])
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn real_tcp_helper_requires_target_pong_without_dialing_a_direct_link() {
    let (capture, mut receiver) = FrameCapture::channel(CaptureLimits::default()).unwrap();
    let collector = std::thread::spawn(move || {
        let mut events = Vec::new();
        while let Some(event) = receiver.blocking_recv() {
            if let Ok(SysEvent::MembershipProbe(probe)) = decode_sys_event(&event.frame.payload) {
                assert_eq!(event.frame.header.correlation_id, probe.request_id());
                events.push((event.direction, event.wire.peer_node, probe));
            }
        }
        assert_eq!(receiver.final_stats().lost, 0);
        events
    });
    let a = node("probe-a").await;
    let b = RemoteActorSystem::new(
        config("probe-b").with_capture(capture.clone()),
        Arc::new(Empty),
    )
    .unwrap();
    b.start().await.unwrap();
    let c = node("probe-c").await;
    connect(&a, &b).await;
    connect(&b, &c).await;
    assert!(!a.missing_direct_connections(&["probe-c"]).is_empty());
    assert_eq!(a.probe_member("probe-c").await, ProbeOutcome::Reachable);
    assert!(!a.missing_direct_connections(&["probe-c"]).is_empty());
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    c.shutdown().await.unwrap();
    capture.close();
    let events = collector.join().unwrap();
    let (index, request) = events
        .iter()
        .enumerate()
        .find_map(|(i, (dir, peer, event))| match event {
            ProbeEvent::PingReq(r)
                if *dir == CaptureDirection::Rx
                    && peer.as_deref() == Some("probe-a")
                    && r.target == "probe-c" =>
            {
                Some((i, r))
            }
            _ => None,
        })
        .expect("A's actual indirect request reached B");
    let ack_index = events
        .iter()
        .enumerate()
        .find_map(|(i, (dir, peer, event))| match event {
            ProbeEvent::PingReqAck(r)
                if *dir == CaptureDirection::Tx
                    && peer.as_deref() == Some("probe-a")
                    && r.request_id == request.request_id
                    && r.target == "probe-c"
                    && r.outcome == ProbeOutcome::Reachable =>
            {
                Some(i)
            }
            _ => None,
        })
        .expect("B returned A's correlated receipt");
    let ping = events[index + 1..ack_index]
        .iter()
        .find_map(|(dir, peer, event)| match event {
            ProbeEvent::Ping(r)
                if *dir == CaptureDirection::Tx && peer.as_deref() == Some("probe-c") =>
            {
                Some(r)
            }
            _ => None,
        })
        .expect("B really probed C before reporting success");
    assert!(events[index + 1..ack_index].iter().any(|(dir, peer, event)| matches!(event,
        ProbeEvent::Pong(r) if *dir == CaptureDirection::Rx && peer.as_deref() == Some("probe-c")
        && r.request_id == ping.request_id && r.target == "probe-c" && r.outcome == ProbeOutcome::Reachable)));
}
#[tokio::test]
async fn helper_without_a_target_route_does_not_invent_reachability_or_failure() {
    let a = node("unknown-a").await;
    let b = node("unknown-b").await;
    connect(&a, &b).await;
    assert_eq!(
        a.probe_member("unknown-target").await,
        ProbeOutcome::Unavailable
    );
    assert!(a
        .missing_direct_connections(&["unknown-target"])
        .contains(&"unknown-target"));
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}
#[tokio::test]
async fn a_connected_but_unresponsive_target_is_not_indirectly_reachable() {
    let a = node("silent-a").await;
    let b = node("silent-b").await;
    connect(&a, &b).await;
    let stream = tokio::net::TcpStream::connect(b.local_addr().unwrap())
        .await
        .unwrap();
    let mut peer = Framed::new(stream, FrameCodec);
    let mut body = bytes::BytesMut::new();
    HandshakeBody {
        node_id: "silent-target".into(),
        capabilities: caps::BIN | caps::MEMBERSHIP_PROBES,
        ..Default::default()
    }
    .encode_tlv(&mut body)
    .unwrap();
    peer.send(Frame::handshake(body.freeze())).await.unwrap();
    assert_eq!(
        peer.next().await.unwrap().unwrap().header.frame_type,
        frame_type::HANDSHAKE_ACK
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while !b.missing_direct_connections(&["silent-target"]).is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let request = tokio::spawn({
        let a = a.clone();
        async move { a.probe_member("silent-target").await }
    });
    let event = tokio::time::timeout(Duration::from_secs(1), next_probe(&mut peer))
        .await
        .unwrap();
    assert!(matches!(event, ProbeEvent::Ping(r) if r.target == "silent-target"));
    assert_eq!(request.await.unwrap(), ProbeOutcome::Failed);
    drop(peer);
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}
#[tokio::test]
async fn helper_count_zero_disables_indirect_queries() {
    let mut cfg = config("disabled-a");
    cfg.swim.indirect_probes = 0;
    let a = RemoteActorSystem::new(cfg, Arc::new(Empty)).unwrap();
    a.start().await.unwrap();
    let b = node("disabled-b").await;
    let c = node("disabled-c").await;
    connect(&a, &b).await;
    connect(&b, &c).await;
    assert_eq!(
        a.probe_member("disabled-c").await,
        ProbeOutcome::Unavailable
    );
    assert_eq!(a.probe_member("disabled-b").await, ProbeOutcome::Reachable);
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    c.shutdown().await.unwrap();
}

#[tokio::test]
async fn automatic_link_loss_uses_helpers_before_starting_suspicion() {
    use parrot_remote::swim::MemberStatus;
    for helpers in [0, 1] {
        let mut cfg = config("loss-a");
        cfg.swim.indirect_probes = helpers;
        let a = RemoteActorSystem::new(cfg, Arc::new(Empty)).unwrap();
        a.start().await.unwrap();
        let b = node("loss-b").await;
        let c = node("loss-c").await;
        connect(&a, &b).await;
        connect(&b, &c).await;
        connect(&a, &c).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if a.membership
                    .lock()
                    .await
                    .members
                    .get("loss-c")
                    .is_some_and(|m| m.status == MemberStatus::Alive)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        a.links_snapshot()
            .await
            .into_iter()
            .find(|(id, _, _)| id == "loss-c")
            .unwrap()
            .1
            .close();
        tokio::time::timeout(Duration::from_secs(2), async {
            while a.missing_direct_connections(&["loss-c"]).is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Observe beyond the entire configured failure observation window.
        // Gossip is disabled for this window, so it cannot repair a false suspicion.
        tokio::time::sleep(Duration::from_millis(650)).await;
        assert_eq!(
            a.membership.lock().await.members["loss-c"].status,
            if helpers == 0 {
                MemberStatus::Suspect
            } else {
                MemberStatus::Alive
            }
        );
        assert!(!a.missing_direct_connections(&["loss-c"]).is_empty());
        a.shutdown().await.unwrap();
        b.shutdown().await.unwrap();
        c.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn simultaneous_indirect_probes_do_not_deadlock_the_dispatch_lane() {
    let a = node("mutual-a").await;
    let b = node("mutual-b").await;
    let c = node("mutual-c").await;
    let d = node("mutual-d").await;
    // A needs B to reach C; B needs A to reach D, over the same A-B link.
    connect(&a, &b).await;
    connect(&b, &c).await;
    connect(&a, &d).await;
    tokio::join!(
        async {
            for _ in 0..8 {
                assert_eq!(a.probe_member("mutual-c").await, ProbeOutcome::Reachable);
            }
        },
        async {
            for _ in 0..8 {
                assert_eq!(b.probe_member("mutual-d").await, ProbeOutcome::Reachable);
            }
        },
    );
    for system in [a, b, c, d] {
        system.shutdown().await.unwrap();
    }
}

async fn raw_peer(
    system: &Arc<RemoteActorSystem>,
    id: &str,
    capabilities: u32,
) -> Framed<tokio::net::TcpStream, FrameCodec> {
    let stream = tokio::net::TcpStream::connect(system.local_addr().unwrap())
        .await
        .unwrap();
    let mut peer = Framed::new(stream, FrameCodec);
    let mut body = bytes::BytesMut::new();
    HandshakeBody {
        node_id: id.into(),
        capabilities: caps::BIN | capabilities,
        ..Default::default()
    }
    .encode_tlv(&mut body)
    .unwrap();
    peer.send(Frame::handshake(body.freeze())).await.unwrap();
    assert_eq!(
        peer.next().await.unwrap().unwrap().header.frame_type,
        frame_type::HANDSHAKE_ACK
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while !system.missing_direct_connections(&[id]).is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    peer
}

#[tokio::test]
async fn unavailable_helper_observations_never_mark_a_learned_target_suspect() {
    use parrot_remote::swim::{Member, MemberEvent, MemberStatus, NodeAddrWire};
    let mut cfg = config("observer-a");
    cfg.swim.probe_interval = Duration::from_millis(10);
    cfg.swim.probe_timeout = Duration::from_millis(40);
    let a = RemoteActorSystem::new(cfg, Arc::new(Empty)).unwrap();
    a.start().await.unwrap();
    let mut helper = raw_peer(&a, "silent-helper", caps::MEMBERSHIP_PROBES).await;
    a.membership
        .lock()
        .await
        .merge_event(&MemberEvent::Upsert(Member {
            node_id: "learned-target".into(),
            addr: NodeAddrWire {
                node_id: "learned-target".into(),
                scheme: "tcp".into(),
                host: "".into(),
                port: 0,
            },
            status: MemberStatus::Alive,
            incarnation: 1,
            status_until_ms: 0,
            metadata: bytes::Bytes::new(),
        }));
    // No helper replies at all. Wait for three independently issued observations,
    // so the first two have definitely expired in the actual production driver.
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut attempts = 0;
        while attempts < 3 {
            let frame = helper.next().await.unwrap().unwrap();
            if matches!(decode_sys_event(&frame.payload), Ok(SysEvent::MembershipProbe(ProbeEvent::PingReq(r))) if r.target == "learned-target") { attempts += 1; }
        }
    }).await.unwrap();
    assert_eq!(
        a.membership.lock().await.members["learned-target"].status,
        MemberStatus::Alive
    );
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_or_unauthorized_probes_close_only_the_offending_connection() {
    for case in 0..7 {
        let cfg = if case == 0 {
            RemoteConfig::tcp("guard", Some("127.0.0.1:0".parse().unwrap()))
        } else {
            config("guard")
        };
        let system = RemoteActorSystem::new(cfg, Arc::new(Empty)).unwrap();
        system.start().await.unwrap();
        let healthy = node("healthy").await;
        connect(&healthy, &system).await;
        let mut peer = raw_peer(
            &system,
            "bad-peer",
            if case == 1 {
                0
            } else {
                caps::MEMBERSHIP_PROBES
            },
        )
        .await;
        // A valid connection may already carry a probe before its peer sends
        // malformed input. Observe a real Ping and retain it in the unread
        // observation sequence so this ordering is exercised deterministically.
        let mut buffered = Vec::new();
        if case >= 2 {
            let observation = system.probe_member("bad-peer");
            tokio::pin!(observation);
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let frame = tokio::select! {
                        result = &mut observation => panic!("probe ended before a wire Ping: {result:?}"),
                        frame = peer.next() => frame.unwrap().unwrap(),
                    };
                    let is_ping = matches!(
                        decode_sys_event(&frame.payload),
                        Ok(SysEvent::MembershipProbe(ProbeEvent::Ping(ref request)))
                            if request.target == "bad-peer"
                    );
                    buffered.push(frame);
                    if is_ping {
                        break;
                    }
                }
            })
            .await
            .expect("authorized peer receives an actual Ping before malformed input");
        }
        let mut frame = Frame::tell("", "", bytes::Bytes::from_static(&[0x12, 1, 1, b'x', 50]));
        frame.header.frame_type = frame_type::SYSTEM_EVENT;
        frame.header.correlation_id = 1;
        match case {
            2 => frame.header.correlation_id = 2,
            3 => frame.path = "not-empty".into(),
            4 => frame.type_key = "not-empty".into(),
            5 => frame.header.flags = 1,
            6 => frame.payload = bytes::Bytes::from_static(&[0x12, 1, 1, b'x', 0]),
            _ => {}
        }
        peer.send(frame).await.unwrap();
        let mut incoming = futures::stream::iter(buffered.into_iter().map(Ok)).chain(peer);
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(frame) = incoming.next().await {
                let Ok(frame) = frame else {
                    return;
                };
                if let Ok(SysEvent::MembershipProbe(probe)) = decode_sys_event(&frame.payload) {
                    // Requests queued before the bad input remain legitimate
                    // only on authorized peers that negotiated this capability.
                    // No Pong/Ack is legitimate: this peer sent no valid probe.
                    assert!(
                        case >= 2 && matches!(probe, ProbeEvent::Ping(_) | ProbeEvent::PingReq(_)),
                        "case={case}: unexpected probe response/unauthorized request: {probe:?}"
                    );
                }
            }
        })
        .await
        .unwrap();
        // Rejecting an attacker must preserve an already established good link.
        assert!(healthy.missing_direct_connections(&["guard"]).is_empty());
        if case != 0 {
            assert_eq!(healthy.probe_member("guard").await, ProbeOutcome::Reachable);
        }
        healthy.shutdown().await.unwrap();
        system.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn legacy_peers_are_not_sent_new_probe_messages() {
    let a = node("legacy-observer").await;
    let mut peer = raw_peer(&a, "legacy", 0).await;
    assert_eq!(a.probe_member("legacy").await, ProbeOutcome::Unavailable);
    // A distinct business frame is a wire barrier after the observation. An
    // unrelated periodic heartbeat cannot satisfy this barrier prematurely.
    let sender = a
        .links_snapshot()
        .await
        .into_iter()
        .find(|(id, _, _)| id == "legacy")
        .unwrap()
        .1;
    let barrier = Frame::tell("barrier", "", bytes::Bytes::from_static(b"probe-finished"));
    sender.send(barrier).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let frame = peer.next().await.unwrap().unwrap();
            assert!(!matches!(
                decode_sys_event(&frame.payload),
                Ok(SysEvent::MembershipProbe(_))
            ));
            if frame.header.frame_type == frame_type::TELL {
                assert_eq!(frame.path, "barrier");
                assert_eq!(frame.payload.as_ref(), b"probe-finished");
                break;
            }
        }
    })
    .await
    .unwrap();
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_cancels_pending_network_observation_before_its_long_deadline() {
    let mut cfg = config("closing");
    cfg.swim.probe_timeout = Duration::from_secs(10);
    let a = RemoteActorSystem::new(cfg, Arc::new(Empty)).unwrap();
    a.start().await.unwrap();
    let mut peer = raw_peer(&a, "silent", caps::MEMBERSHIP_PROBES).await;
    let task = tokio::spawn({
        let a = a.clone();
        async move { a.probe_member("silent").await }
    });
    let event = tokio::time::timeout(Duration::from_secs(1), next_probe(&mut peer))
        .await
        .unwrap();
    assert!(matches!(event, ProbeEvent::Ping(r) if r.target == "silent"));
    tokio::time::timeout(Duration::from_secs(2), a.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.await.unwrap(), ProbeOutcome::Unavailable);
    assert_eq!(a.probe_member("silent").await, ProbeOutcome::Unavailable);
}

fn request_frame(id: u64, target: &str) -> Frame {
    let event = ProbeEvent::PingReq(parrot_remote::swim::ProbeRequest {
        request_id: id,
        target: target.into(),
        budget_ms: 5_000,
    });
    let mut frame = Frame::tell("", "", event.encode().unwrap());
    frame.header.frame_type = frame_type::SYSTEM_EVENT;
    frame.header.correlation_id = id;
    frame
}
async fn next_probe(peer: &mut Framed<tokio::net::TcpStream, FrameCodec>) -> ProbeEvent {
    loop {
        let frame = peer.next().await.unwrap().unwrap();
        if let Ok(SysEvent::MembershipProbe(probe)) = decode_sys_event(&frame.payload) {
            return probe;
        }
    }
}

// Losing the old requester also starts an automatic membership observation of
// that requester. Those unrelated PingReq messages are not slot-reuse receipts.
// The first periodic tick can race registration too: its budget is <=300ms,
// whereas these independently crafted helper requests give the target 5s.
// Count only the latter; otherwise a periodic Ping can hide one old helper Ping.
async fn capacity_probe(
    peer: &mut Framed<tokio::net::TcpStream, FrameCodec>,
    ack: bool,
) -> ProbeEvent {
    loop {
        let event = next_probe(peer).await;
        if match &event {
            ProbeEvent::Ping(r) => !ack && r.target == "busy-target" && r.budget_ms > 1_000,
            ProbeEvent::PingReqAck(r) => ack && r.target == "busy-target",
            _ => false,
        } {
            return event;
        }
    }
}

#[tokio::test]
async fn helper_capacity_is_bounded_and_requester_disconnect_releases_it() {
    let helper = node("bounded-helper").await;
    let mut target = raw_peer(&helper, "busy-target", caps::MEMBERSHIP_PROBES).await;
    let mut requester = raw_peer(&helper, "requester", caps::MEMBERSHIP_PROBES).await;
    for id in 1..=33 {
        requester
            .send(request_frame(id, "busy-target"))
            .await
            .unwrap();
    }
    // The target never replies, so each accepted helper occupies its real slot.
    tokio::time::timeout(Duration::from_secs(2), async {
        for _ in 0..32 {
            assert!(matches!(
                capacity_probe(&mut target, false).await,
                ProbeEvent::Ping(_)
            ));
        }
        assert!(
            matches!(capacity_probe(&mut requester, true).await, ProbeEvent::PingReqAck(r)
            if r.request_id == 33 && r.outcome == ProbeOutcome::Unavailable)
        );
    })
    .await
    .unwrap();
    drop(requester);
    let mut replacement = raw_peer(&helper, "next-requester", caps::MEMBERSHIP_PROBES).await;
    // Disconnect processing is asynchronous. Retry denied requests until a new
    // actual Ping proves a slot was released, long before the old 5s deadline.
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut id = 100;
        loop {
            replacement.send(request_frame(id, "busy-target")).await.unwrap();
            tokio::select! {
                event = capacity_probe(&mut target, false) => {
                    let ProbeEvent::Ping(request) = event else { panic!("wrong target request") };
                    let reply = ProbeEvent::Pong(parrot_remote::swim::ProbeReply { request_id: request.request_id,
                        target: request.target, outcome: ProbeOutcome::Reachable });
                    let mut frame = Frame::tell("", "", reply.encode().unwrap());
                    frame.header.frame_type = frame_type::SYSTEM_EVENT;
                    frame.header.correlation_id = request.request_id;
                    target.send(frame).await.unwrap();
                    assert!(matches!(capacity_probe(&mut replacement, true).await, ProbeEvent::PingReqAck(r)
                        if r.request_id == id && r.outcome == ProbeOutcome::Reachable));
                    break;
                }
                event = capacity_probe(&mut replacement, true) => {
                    assert!(matches!(event, ProbeEvent::PingReqAck(r)
                        if r.request_id == id && r.outcome == ProbeOutcome::Unavailable));
                    tokio::task::yield_now().await;
                    id += 1;
                }
            }
        }
    }).await.unwrap();
    helper.shutdown().await.unwrap();
}

#[test]
fn invalid_probe_configuration_is_rejected_before_system_startup() {
    for (count, timeout) in [
        (33, Duration::from_millis(10)),
        (1, Duration::ZERO),
        (1, Duration::from_millis(10_001)),
    ] {
        let mut cfg = config("invalid");
        cfg.swim.indirect_probes = count;
        cfg.swim.probe_timeout = timeout;
        assert!(RemoteActorSystem::new(cfg, Arc::new(Empty)).is_err());
    }
    let mut cfg = config("overflow");
    cfg.swim.probe_interval = Duration::MAX;
    assert!(RemoteActorSystem::new(cfg, Arc::new(Empty)).is_err());
}
