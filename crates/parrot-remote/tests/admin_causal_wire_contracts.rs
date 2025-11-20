use bytes::BytesMut;
use parrot_api::{
    trace::{SpanGuard, SpanKind, SpanStatus, TraceContext, TraceParent, TraceRecorder},
    types::BoxedActorRef,
};
use parrot_remote::{
    admin::{decode_sys_event, sys_event_tag, SysEvent},
    admin_v2::{encode_admin_cmd_v2, reply_frame_v2, AdminCommandV2, AdminReplyV2},
    frame::{flags, Frame, FrameError, BODY_FIXED_OVERHEAD, MAX_FRAME_LEN, TRACE_CONTEXT_BYTES},
    handshake::{caps, HandshakeBody},
    ingress::{Ingress, LocalLookup, SysEventHook},
    security::ControlPolicy,
    transport::{
        run_connection, ConnParams, ConnSide, ConnectionHandle, FrameSender, RuntimeKnobs,
    },
    CallbackRegistry, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, watch};

const ADMIN_TRACE: u32 = 1 << 20;
const REQUIRED: u32 = caps::TRACE_CONTEXT | ADMIN_TRACE;
type Incoming = mpsc::Receiver<(Frame, FrameSender, String)>;

fn parent() -> TraceParent {
    TraceParent::new([3; 16], [4; 8]).unwrap()
}
fn command() -> Frame {
    let mut frame = reply_frame_v2(7, "", &AdminReplyV2::Stopped { req_id: 7 });
    frame.payload = encode_admin_cmd_v2(&AdminCommandV2::StopComponent {
        req_id: 7,
        path_prefix: "/user/owned".into(),
    });
    frame
}
// Raw construction lets rejection tests exercise ingress without a safe encoder.
fn raw_prefix(mut frame: Frame) -> Frame {
    let mut payload = vec![1];
    payload.extend_from_slice(&parent().trace_id());
    payload.extend_from_slice(&parent().span_id());
    payload.extend_from_slice(&frame.payload);
    frame.payload = payload.into();
    frame.header.flags |= flags::TRACE_CONTEXT;
    frame
}
struct Link {
    a: ConnectionHandle,
    b: ConnectionHandle,
    rx_a: Incoming,
    rx_b: Incoming,
    shutdown: watch::Sender<bool>,
}
impl Link {
    async fn open(left: u32, right: u32, max: u32) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connecting = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (accepting, _) = listener.accept().await.unwrap();
        let (shutdown, _) = watch::channel(false);
        let (tx_a, rx_a) = mpsc::channel(8);
        let (tx_b, rx_b) = mpsc::channel(8);
        let params = |side, name: &str, capabilities, inbound| ConnParams {
            side,
            local_addr: None,
            peer_addr: None,
            scheme: "tcp",
            local_handshake: HandshakeBody {
                node_id: name.into(),
                capabilities: caps::BIN | capabilities,
                max_frame_len: max,
                ..Default::default()
            },
            inbound,
            on_disconnect: Arc::new(|_| {}),
            shutdown: shutdown.subscribe(),
            knobs: Some(RuntimeKnobs {
                heartbeat_enabled: false,
                ..Default::default()
            }),
        };
        let (a, b) = tokio::join!(
            run_connection(connecting, params(ConnSide::Connect, "a", left, tx_a)),
            run_connection(accepting, params(ConnSide::Accept, "b", right, tx_b)),
        );
        Self {
            a: a.unwrap(),
            b: b.unwrap(),
            rx_a,
            rx_b,
            shutdown,
        }
    }
    async fn close(self) {
        self.shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), async {
            self.a.closed.await.unwrap();
            self.b.closed.await.unwrap();
        })
        .await
        .unwrap();
    }
}

#[test]
fn admin_parent_prefix_roundtrips_without_changing_command_bytes() {
    assert_eq!(caps::ADMIN_TRACE_CONTEXT, ADMIN_TRACE);
    let original = command();
    let traced = original.clone().with_trace_parent(parent()).unwrap();
    assert_eq!(traced, raw_prefix(original.clone()));
    let mut bytes = BytesMut::new();
    traced.encode(&mut bytes).unwrap();
    let mut decoded = Frame::decode(&mut bytes).unwrap().unwrap();
    assert!(bytes.is_empty());
    assert_eq!(decoded.take_trace_parent().unwrap(), Some(parent()));
    assert_eq!(decoded, original);
    assert_eq!(decoded.take_trace_parent().unwrap(), None);
    assert!(matches!(
        decode_sys_event(&decoded.payload).unwrap(),
        SysEvent::AdminCommandV2(AdminCommandV2::StopComponent { req_id: 7, .. })
    ));
}

#[test]
fn admin_trace_does_not_extend_to_replies_legacy_or_cluster_events() {
    for tag in [
        sys_event_tag::ADMIN_CMD,
        sys_event_tag::ADMIN_REPLY,
        sys_event_tag::ADMIN_REPLY_V2,
        sys_event_tag::MEMBERSHIP_GOSSIP,
        sys_event_tag::MEMBERSHIP_PING,
        sys_event_tag::RECEPTIONIST_SYNC,
        sys_event_tag::RECEPTIONIST_SEGMENT,
        0xff,
    ] {
        let mut frame = command();
        frame.payload = vec![tag, 0].into();
        assert!(frame.clone().with_trace_parent(parent()).is_err());
        assert!(raw_prefix(frame).validate_payload_flags().is_err());
    }
}

#[test]
fn admin_trace_validates_prefix_and_counts_global_frame_budget() {
    for change in 0..4 {
        let mut frame = raw_prefix(command());
        let mut payload = frame.payload.to_vec();
        match change {
            0 => payload[0] = 2,
            1 => payload[1..17].fill(0),
            2 => payload[17..25].fill(0),
            3 => payload.truncate(24),
            _ => unreachable!(),
        }
        frame.payload = payload.into();
        assert!(frame.validate_payload_flags().is_err());
    }
    let available = MAX_FRAME_LEN as usize - BODY_FIXED_OVERHEAD - TRACE_CONTEXT_BYTES;
    for extra in [0, 1] {
        let mut frame = command();
        let mut payload = vec![0; available + extra];
        payload[0] = sys_event_tag::ADMIN_CMD_V2;
        frame.payload = payload.into();
        let result = frame.with_trace_parent(parent());
        if extra == 0 {
            let mut bytes = BytesMut::new();
            result.unwrap().encode(&mut bytes).unwrap();
            assert_eq!(bytes.len(), MAX_FRAME_LEN as usize + 4);
        } else {
            assert!(matches!(result, Err(FrameError::TooLarge { .. })));
        }
    }
}

#[tokio::test]
async fn admin_parent_requires_both_capabilities_on_both_handshake_sides() {
    tokio::time::timeout(Duration::from_secs(20), async {
        for left in [0, caps::TRACE_CONTEXT, ADMIN_TRACE, REQUIRED] {
            for right in [0, caps::TRACE_CONTEXT, ADMIN_TRACE, REQUIRED] {
                let mut link = Link::open(left, right, MAX_FRAME_LEN).await;
                let supported = left & right & REQUIRED == REQUIRED;
                for (sender, rx) in [
                    (&link.a.sender, &mut link.rx_b),
                    (&link.b.sender, &mut link.rx_a),
                ] {
                    let traced = command().with_trace_parent(parent()).unwrap();
                    assert_eq!(sender.send(traced.clone()).await.is_ok(), supported);
                    if supported {
                        let (frame, _, _) = rx.recv().await.unwrap();
                        assert_eq!(frame.split_trace_parent().unwrap().0, Some(parent()));
                    } else {
                        assert!(sender.send_frame_sync(traced).is_err());
                        assert!(rx.try_recv().is_err());
                    }
                    sender.send(command()).await.unwrap();
                    let (frame, _, _) = rx.recv().await.unwrap();
                    assert_eq!(frame.payload, command().payload);
                    assert_eq!(frame.header.flags, 0);
                    assert!(!sender.is_closed());
                }
                link.close().await;
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn admin_parent_counts_negotiated_budget_without_poisoning_connection() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let traced = command().with_trace_parent(parent()).unwrap();
        let max = (BODY_FIXED_OVERHEAD + traced.payload.len()) as u32;
        let mut link = Link::open(REQUIRED, REQUIRED, max).await;
        for (sender, rx) in [
            (&link.a.sender, &mut link.rx_b),
            (&link.b.sender, &mut link.rx_a),
        ] {
            let mut oversized = traced.clone();
            let mut bytes = oversized.payload.to_vec();
            bytes.push(0);
            oversized.payload = bytes.into();
            assert!(matches!(
                sender.send(oversized.clone()).await,
                Err(parrot_remote::error::RemoteError::Frame(
                    FrameError::TooLarge { .. }
                ))
            ));
            assert!(sender.send_frame_sync(oversized).is_err());
            assert!(!sender.is_closed());
            sender.send(traced.clone()).await.unwrap();
            let (mut frame, _, _) = rx.recv().await.unwrap();
            assert_eq!(frame.take_trace_parent().unwrap(), Some(parent()));
            assert_eq!(frame, command());
        }
        link.close().await;
    })
    .await
    .unwrap();
}

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
#[derive(Default)]
struct Observe(Mutex<Vec<Option<TraceContext>>>);
#[async_trait::async_trait]
impl SysEventHook for Observe {
    async fn on_event(&self, _: SysEvent, _: &FrameSender, _: &str) {
        self.0.lock().unwrap().push(TraceContext::current());
        SpanGuard::child(SpanKind::Application, "actual/admin").finish(SpanStatus::Ok);
    }
}
fn ingress(authorized: bool, recorder: Option<TraceRecorder>) -> (Arc<Ingress>, Arc<Observe>) {
    let ingress = Arc::new(
        Ingress::new(Arc::new(Empty), Arc::new(CallbackRegistry::new(8))).with_control_policy(
            if authorized {
                ControlPolicy::InsecureDevelopment
            } else {
                ControlPolicy::LocalOnly
            },
        ),
    );
    ingress.set_trace_recorder(recorder);
    let observe = Arc::new(Observe::default());
    *ingress.sys_event_slot() = Some(observe.clone());
    (ingress, observe)
}

#[tokio::test]
async fn admin_imports_validated_parent_only_after_request_id_and_authorization_checks() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for (attached, authorized, mismatch, bad_body) in [
            (false, true, false, false), (true, true, false, false),
            (true, false, false, false), (true, true, true, false),
            (true, true, false, true),
        ] {
            let mut link = Link::open(REQUIRED, REQUIRED, MAX_FRAME_LEN).await;
            let recorder = TraceRecorder::new(8).unwrap();
            let (ingress, observe) = ingress(authorized, attached.then(|| recorder.clone()));
            let mut original = command();
            if mismatch { original.header.correlation_id = 8; }
            if bad_body {
                let mut payload = original.payload.to_vec();
                payload.push(0);
                original.payload = payload.into();
            }
            link.a.sender.send(original.with_trace_parent(parent()).unwrap()).await.unwrap();
            let (frame, back, from) = link.rx_b.recv().await.unwrap();
            recorder.root().scope(ingress.dispatch(frame, &back, &from)).await;
            let seen = observe.0.lock().unwrap().clone();
            let records = recorder.snapshot().records;
            if mismatch || bad_body {
                assert!(seen.is_empty());
                assert!(records.is_empty());
                let (reply, _, _) = link.rx_a.recv().await.unwrap();
                let expected = if mismatch { 8 } else { 7 };
                assert_eq!(reply.header.correlation_id, expected);
                assert!(matches!(decode_sys_event(&reply.payload).unwrap(), SysEvent::AdminReplyV2(AdminReplyV2::Failed { req_id, .. }) if req_id == expected));
            } else {
                assert_eq!(seen.len(), 1);
                if authorized {
                    assert_eq!(seen[0].as_ref().unwrap().as_parent(), parent());
                    assert_eq!(records.len(), usize::from(attached));
                    if attached {
                        assert_eq!(records[0].trace_id, parent().trace_id());
                        assert_eq!(records[0].parent_span_id, Some(parent().span_id()));
                    }
                } else {
                    assert!(seen[0].is_none());
                    assert!(records.is_empty());
                }
            }
            let before = recorder.snapshot().counts.started;
            link.a.sender.send(command()).await.unwrap();
            let (frame, back, from) = link.rx_b.recv().await.unwrap();
            recorder.root().scope(ingress.dispatch(frame, &back, &from)).await;
            assert!(observe.0.lock().unwrap().last().unwrap().is_none());
            assert_eq!(recorder.snapshot().counts.started, before);
            link.close().await;
        }
    }).await.unwrap();
}

#[tokio::test]
async fn unnegotiated_admin_parent_closes_before_hook_or_recording() {
    for capabilities in [0, caps::TRACE_CONTEXT, ADMIN_TRACE] {
        let link = Link::open(capabilities, capabilities, MAX_FRAME_LEN).await;
        let recorder = TraceRecorder::new(8).unwrap();
        let (ingress, observe) = ingress(true, Some(recorder.clone()));
        recorder
            .root()
            .scope(ingress.dispatch(raw_prefix(command()), &link.b.sender, "a"))
            .await;
        assert!(link.b.sender.is_closed());
        assert!(observe.0.lock().unwrap().is_empty());
        assert_eq!(recorder.snapshot().counts.started, 0);
        link.close().await;
    }
}

#[tokio::test]
async fn actual_admin_client_preserves_legacy_payload_until_peer_negotiates_extension() {
    tokio::time::timeout(Duration::from_secs(20), async {
        for capabilities in [0, caps::TRACE_CONTEXT, ADMIN_TRACE, REQUIRED] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let remote = RemoteActorSystem::new(
                RemoteConfig::tcp("caller", None).with_insecure_development(),
                Arc::new(Empty),
            )
            .unwrap();
            remote.start().await.unwrap();
            let (shutdown, _) = watch::channel(false);
            let (tx, mut rx) = mpsc::channel(8);
            let node = NodeAddr::tcp("peer", listener.local_addr().unwrap());
            let accepting = async {
                let (io, _) = listener.accept().await.unwrap();
                run_connection(
                    io,
                    ConnParams {
                        side: ConnSide::Accept,
                        local_addr: None,
                        peer_addr: None,
                        scheme: "tcp",
                        local_handshake: HandshakeBody {
                            node_id: "peer".into(),
                            capabilities: caps::BIN | capabilities,
                            ..Default::default()
                        },
                        inbound: tx,
                        on_disconnect: Arc::new(|_| {}),
                        shutdown: shutdown.subscribe(),
                        knobs: Some(RuntimeKnobs {
                            heartbeat_enabled: false,
                            ..Default::default()
                        }),
                    },
                )
                .await
                .unwrap()
            };
            let (connected, peer) = tokio::join!(remote.connect(&node), accepting);
            connected.unwrap();
            let root = TraceContext::root();
            for legacy in [false, true] {
                let request = root.scope(async {
                    if legacy {
                        remote.admin_stop("peer", "/user/owned").await
                    } else {
                        remote.stop_component("peer", "/user/owned").await
                    }
                });
                let receiving = async {
                    let (mut frame, back, _) = rx.recv().await.unwrap();
                    let incoming = frame.take_trace_parent().unwrap();
                    assert_eq!(
                        incoming,
                        (!legacy && capabilities == REQUIRED).then(|| root.as_parent())
                    );
                    match decode_sys_event(&frame.payload).unwrap() {
                        SysEvent::AdminCommandV2(AdminCommandV2::StopComponent {
                            req_id, ..
                        }) if !legacy => {
                            back.send(reply_frame_v2(
                                req_id,
                                "",
                                &AdminReplyV2::Stopped { req_id },
                            ))
                            .await
                            .unwrap();
                        }
                        SysEvent::AdminCommand(parrot_remote::admin::AdminCommand::AdminStop {
                            req_id,
                            ..
                        }) if legacy => {
                            back.send(parrot_remote::admin::reply_frame(
                                req_id,
                                "",
                                &parrot_remote::admin::AdminReply::Stopped { req_id },
                            ))
                            .await
                            .unwrap();
                        }
                        other => panic!("unexpected management payload: {other:?}"),
                    }
                };
                let (result, ()) = tokio::join!(request, receiving);
                result.unwrap();
            }
            remote.shutdown().await.unwrap();
            shutdown.send_replace(true);
            peer.closed.await.unwrap();
        }
    })
    .await
    .unwrap();
}
