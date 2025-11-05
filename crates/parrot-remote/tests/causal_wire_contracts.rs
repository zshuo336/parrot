use bytes::{Bytes, BytesMut};
use parrot_api::trace::{
    SpanGuard, SpanKind, SpanStatus, TraceContext, TraceParent, TraceRecorder,
};
use parrot_remote::{
    frame::{
        flags, frame_type, Frame, FrameError, BODY_FIXED_OVERHEAD, MAX_FRAME_LEN,
        TRACE_CONTEXT_BYTES,
    },
    handshake::{caps, HandshakeBody},
    transport::{run_connection, ConnParams, ConnSide, RuntimeKnobs},
};
use std::{sync::Arc, time::Duration};
fn parent(n: u8) -> TraceParent {
    TraceParent::new([n; 16], [n; 8]).unwrap()
}
#[test]
fn remote_parent_requires_nonzero_identifiers_and_preserves_causality() {
    assert!(TraceParent::new([0; 16], [1; 8]).is_err());
    assert!(TraceParent::new([1; 16], [0; 8]).is_err());
    let parent = parent(3);
    let recorder = TraceRecorder::new(8).unwrap();
    let imported = recorder.remote_parent(parent);
    assert_eq!(imported.as_parent(), parent);
    assert_eq!(recorder.snapshot().counts.started, 0);
    let child = imported.child();
    assert_eq!(child.trace_id(), parent.trace_id());
    assert_eq!(child.parent_span_id(), Some(parent.span_id()));
    assert_ne!(child.span_id(), parent.span_id());
    child.in_scope(|| SpanGuard::current(SpanKind::Application, "/actual").finish(SpanStatus::Ok));
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(snapshot.records[0].parent_span_id, Some(parent.span_id()));
    assert_eq!(snapshot.records[0].span_id, child.span_id());
    assert_eq!(TraceContext::remote_parent(parent).as_parent(), parent);
    assert!(TraceContext::current().is_none());
}
#[test]
fn ask_parent_prefix_roundtrips_without_changing_reply_to_or_business_bytes() {
    let original = Frame::ask(
        77,
        "/actor",
        "bin:key",
        Bytes::from_static(b"business"),
        Some("parrot://caller/replies"),
    );
    let traced = original.clone().with_trace_parent(parent(4)).unwrap();
    assert_eq!(
        traced.payload.len(),
        original.payload.len() + TRACE_CONTEXT_BYTES
    );
    assert_eq!(&traced.payload[..1], &[1]);
    assert_eq!(&traced.payload[1..17], &[4; 16]);
    assert_eq!(&traced.payload[17..25], &[4; 8]);
    let mut wire = BytesMut::new();
    traced.encode(&mut wire).unwrap();
    let mut decoded = Frame::decode(&mut wire).unwrap().unwrap();
    assert!(wire.is_empty());
    assert_eq!(
        decoded.split_trace_parent().unwrap(),
        (Some(parent(4)), original.payload.clone())
    );
    assert_eq!(
        decoded.split_reply_to().unwrap(),
        (
            Some("parrot://caller/replies".into()),
            Bytes::from_static(b"business")
        )
    );
    assert_eq!(decoded.header.correlation_id, 77);
    assert_eq!(decoded.take_trace_parent().unwrap(), Some(parent(4)));
    assert_eq!(decoded.take_trace_parent().unwrap(), None);
    assert_eq!(decoded.payload, original.payload);
    assert_eq!(decoded.header.flags, original.header.flags);
}
#[test]
fn ordinary_and_completion_tell_keep_flags_and_raw_business_payload() {
    for completion in [false, true] {
        let mut frame = if completion {
            Frame::tell_with_ack(9, "/x", "key", Bytes::from_static(b"x"))
        } else {
            Frame::tell_with_seq("/x", "key", Bytes::from_static(b"x"), 4)
        };
        frame.header.flags |= flags::TRACING | flags::URGENT;
        let flags = frame.header.flags;
        let mut frame = frame.with_trace_parent(parent(9)).unwrap();
        frame.validate_payload_flags().unwrap();
        assert_eq!(frame.take_trace_parent().unwrap(), Some(parent(9)));
        assert_eq!(frame.header.flags, flags);
        assert_eq!(frame.payload, b"x"[..]);
    }
}
#[test]
fn invalid_trace_versions_lengths_and_zero_ids_are_rejected() {
    for bytes in [
        vec![],
        vec![1; 24],
        vec![2; 25],
        {
            let mut b = vec![1; 25];
            b[1..17].fill(0);
            b
        },
        {
            let mut b = vec![1; 25];
            b[17..25].fill(0);
            b
        },
    ] {
        let mut f = Frame::tell("/x", "key", bytes.into());
        f.header.flags |= flags::TRACE_CONTEXT;
        assert!(matches!(
            f.split_trace_parent(),
            Err(FrameError::InvalidTraceContext)
        ));
        assert!(matches!(
            f.validate_payload_flags(),
            Err(FrameError::InvalidTraceContext)
        ));
    }
}
#[test]
fn trace_extension_is_not_valid_on_control_or_reply_frames() {
    for kind in [
        frame_type::HANDSHAKE,
        frame_type::HEARTBEAT,
        frame_type::STOP,
        frame_type::REPLY,
        frame_type::REPLY_ERR,
        frame_type::SYSTEM_EVENT,
    ] {
        let mut frame = Frame::tell("/x", "key", Bytes::new());
        frame.header.frame_type = kind;
        assert!(frame.clone().with_trace_parent(parent(1)).is_err());
        frame.header.flags |= flags::TRACE_CONTEXT;
        frame.payload = vec![1; 25].into();
        assert!(frame.validate_payload_flags().is_err());
    }
    let traced = Frame::tell("/x", "key", Bytes::new())
        .with_trace_parent(parent(1))
        .unwrap();
    assert!(traced.with_trace_parent(parent(2)).is_err());
}
#[test]
fn batch_members_keep_distinct_parent_ids_and_validate_before_iteration() {
    let frames: Vec<_> = (1..=3)
        .map(|n| {
            Frame::tell_with_ack(n as u64, "/x", "key", Bytes::from(vec![n]))
                .with_trace_parent(parent(n))
                .unwrap()
        })
        .collect();
    let batch = Frame::batch(&frames).unwrap();
    assert!(batch.split_trace_parent().is_err());
    let expanded: Vec<_> = batch.batch_frames().unwrap().collect();
    assert_eq!(expanded.len(), 3);
    for (i, member) in expanded.iter().enumerate() {
        assert_eq!(
            member.split_trace_parent().unwrap(),
            (Some(parent(i as u8 + 1)), Bytes::from(vec![i as u8 + 1]))
        );
    }
    let mut invalid = batch;
    let mut payload = invalid.payload.to_vec();
    // Last member's version, after count and the first two length-delimited members.
    payload[8 + 2 * (12 + 26) + 12] = 2;
    invalid.payload = payload.into();
    assert!(invalid.batch_frames().is_err());
}
#[test]
fn trace_prefix_consumes_real_frame_budget() {
    let available = MAX_FRAME_LEN as usize - BODY_FIXED_OVERHEAD - TRACE_CONTEXT_BYTES;
    let frame = Frame::tell("", "", vec![0; available].into())
        .with_trace_parent(parent(1))
        .unwrap();
    let mut wire = BytesMut::new();
    frame.encode(&mut wire).unwrap();
    assert_eq!(wire.len(), MAX_FRAME_LEN as usize + 4);
    assert!(matches!(
        Frame::tell("", "", vec![0; available + 1].into()).with_trace_parent(parent(1)),
        Err(FrameError::TooLarge { .. })
    ));
}
#[tokio::test]
async fn causal_payload_requires_mutual_handshake_advertisement() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for (left, right) in [(false, false), (false, true), (true, false), (true, true)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let connecting = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (accepting, _) = listener.accept().await.unwrap();
            let (shutdown, _) = tokio::sync::watch::channel(false);
            let (tx_a, mut rx_a) = tokio::sync::mpsc::channel(8);
            let (tx_b, mut rx_b) = tokio::sync::mpsc::channel(8);
            let params = |side, name: &str, trace, inbound| ConnParams {
                side,
                local_addr: None,
                peer_addr: None,
                scheme: "tcp",
                local_handshake: HandshakeBody {
                    node_id: name.into(),
                    capabilities: caps::BIN | if trace { caps::TRACE_CONTEXT } else { 0 },
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
                run_connection(accepting, params(ConnSide::Accept, "b", right, tx_b))
            );
            let (a, b) = (a.unwrap(), b.unwrap());
            for (sender, receiver) in [(&a.sender, &mut rx_b), (&b.sender, &mut rx_a)] {
                assert_eq!(sender.supports_trace_context(), left && right);
                let frame = Frame::tell("/x", "key", Bytes::from_static(b"real"))
                    .with_trace_parent(parent(1))
                    .unwrap();
                let result = sender.send(frame).await;
                assert_eq!(result.is_ok(), left && right);
                if left && right {
                    let (received, _, _) = receiver.recv().await.unwrap();
                    assert_eq!(received.split_trace_parent().unwrap().0, Some(parent(1)));
                }
                sender
                    .send(Frame::tell("/x", "key", Bytes::from_static(b"legacy")))
                    .await
                    .unwrap();
                let (received, _, _) = receiver.recv().await.unwrap();
                assert_eq!(received.payload, b"legacy"[..]);
                assert!(!sender.is_closed());
            }
            shutdown.send_replace(true);
            a.closed.await.unwrap();
            b.closed.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn negotiated_limit_counts_prefix_and_rejection_leaves_link_usable() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connecting = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (accepting, _) = listener.accept().await.unwrap();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let (tx_a, mut rx_a) = tokio::sync::mpsc::channel(8);
        let (tx_b, mut rx_b) = tokio::sync::mpsc::channel(8);
        let limit = (BODY_FIXED_OVERHEAD + TRACE_CONTEXT_BYTES) as u32;
        let params = |side, name: &str, max, inbound| ConnParams {
            side,
            local_addr: None,
            peer_addr: None,
            scheme: "tcp",
            local_handshake: HandshakeBody {
                node_id: name.into(),
                max_frame_len: max,
                capabilities: caps::BIN | caps::TRACE_CONTEXT,
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
            run_connection(
                connecting,
                params(ConnSide::Connect, "budget-a", limit, tx_a)
            ),
            run_connection(accepting, params(ConnSide::Accept, "budget-b", 512, tx_b))
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        for (sender, rx) in [(&a.sender, &mut rx_b), (&b.sender, &mut rx_a)] {
            let too_large = Frame::tell("", "", Bytes::from_static(b"x"))
                .with_trace_parent(parent(1))
                .unwrap();
            assert!(matches!(
                sender.send(too_large.clone()).await,
                Err(parrot_remote::error::RemoteError::Frame(
                    FrameError::TooLarge { len: 54, max: 53 }
                ))
            ));
            assert!(sender.send_frame_sync(too_large).is_err());
            assert!(!sender.is_closed());
            sender
                .send(
                    Frame::tell("", "", Bytes::new())
                        .with_trace_parent(parent(1))
                        .unwrap(),
                )
                .await
                .unwrap();
            let (frame, _, _) = rx.recv().await.unwrap();
            assert_eq!(
                frame.split_trace_parent().unwrap(),
                (Some(parent(1)), Bytes::new())
            );
            sender
                .send(Frame::tell("", "", Bytes::from_static(b"legacy")))
                .await
                .unwrap();
            let (frame, _, _) = rx.recv().await.unwrap();
            assert_eq!(frame.payload, b"legacy"[..]);
        }
        shutdown.send_replace(true);
        a.closed.await.unwrap();
        b.closed.await.unwrap();
    })
    .await
    .unwrap();
}
