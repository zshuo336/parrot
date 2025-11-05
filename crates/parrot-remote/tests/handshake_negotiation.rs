//! Negotiation must select a stack supported by both actual endpoints.
use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use parrot_remote::{
    frame::Frame,
    handshake::{caps, HandshakeAckBody, HandshakeBody, TopologyRole},
    transport::{run_connection, ConnParams, ConnSide, FrameCodec, RuntimeKnobs},
};
use std::{sync::Arc, time::Duration};
use tokio_util::codec::Framed;

fn hello(capabilities: u32) -> HandshakeBody {
    HandshakeBody {
        node_id: "peer".into(),
        capabilities,
        ..Default::default()
    }
}
fn ack(capabilities: u32, codec: &str) -> HandshakeAckBody {
    HandshakeAckBody {
        node_id: "peer".into(),
        realm: None,
        cluster: None,
        capabilities,
        max_frame_len: 1 << 20,
        topology_role: TopologyRole::Normal,
        hop_limit: 8,
        direct_addr: None,
        chosen_codec: codec.into(),
    }
}
// Deliberately independent wire fixture: malformed peer packets must not be
// blocked by the production encoder before reaching the decoder under test.
fn raw_hello(body: &HandshakeBody, chosen: Option<&str>) -> BytesMut {
    let mut out = BytesMut::new();
    for (tag, bytes) in [
        (1, body.node_id.as_bytes().to_vec()),
        (4, body.capabilities.to_le_bytes().to_vec()),
        (5, body.max_frame_len.to_le_bytes().to_vec()),
        (6, vec![body.topology_role as u8]),
        (7, vec![body.hop_limit]),
    ] {
        out.extend_from_slice(&[tag]);
        out.extend_from_slice(&u16::try_from(bytes.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&bytes);
    }
    if let Some(chosen) = chosen {
        out.extend_from_slice(&[9]);
        out.extend_from_slice(&u16::try_from(chosen.len()).unwrap().to_le_bytes());
        out.extend_from_slice(chosen.as_bytes());
    }
    out
}
fn params(
    side: ConnSide,
    capabilities: u32,
    shutdown: &tokio::sync::watch::Sender<bool>,
) -> ConnParams {
    let (inbound, _) = tokio::sync::mpsc::channel(8);
    ConnParams {
        side,
        local_addr: None,
        peer_addr: None,
        scheme: "mem",
        local_handshake: hello(capabilities),
        inbound,
        on_disconnect: Arc::new(|_| {}),
        shutdown: shutdown.subscribe(),
        knobs: Some(RuntimeKnobs {
            heartbeat_enabled: false,
            ..Default::default()
        }),
    }
}
#[tokio::test]
async fn accept_selects_a_mutually_supported_codec() {
    for (local, peer, expected) in [
        (caps::BIN | caps::PB, caps::PB, "pb"),
        (caps::PB, caps::BIN | caps::PB, "pb"),
        (caps::BIN | caps::PB, caps::BIN | caps::PB, "bin"),
    ] {
        let (left, right) = tokio::io::duplex(4096);
        let (shutdown, _) = tokio::sync::watch::channel(false);
        // Keep the independent peer and its reply alive until the owner exists.
        let mut bytes = BytesMut::new();
        hello(peer).encode_tlv(&mut bytes).unwrap();
        let exchange = async {
            let mut framed = Framed::new(right, FrameCodec);
            framed.send(Frame::handshake(bytes.freeze())).await.unwrap();
            let response = framed.next().await.unwrap().unwrap();
            (
                framed,
                HandshakeAckBody::decode_tlv(&response.payload).unwrap(),
            )
        };
        let (owner, (_peer, received)) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                run_connection(left, params(ConnSide::Accept, local, &shutdown)),
                exchange
            )
        })
        .await
        .unwrap();
        let owner = owner.unwrap();
        shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(2), owner.closed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.chosen_codec, expected, "local={local} peer={peer}");
    }
}
#[tokio::test]
async fn connect_rejects_unnegotiated_ack_choices() {
    for (local, remote, chosen) in [
        (caps::PB, caps::BIN | caps::PB, "bin"),
        (caps::BIN, caps::PB, "pb"),
        (caps::BIN | caps::PB, caps::BIN | caps::PB, "unknown"),
    ] {
        let (left, right) = tokio::io::duplex(4096);
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let exchange = async {
            let mut framed = Framed::new(right, FrameCodec);
            framed.next().await.unwrap().unwrap();
            let bytes = raw_hello(&hello(remote), Some(chosen));
            framed
                .send(Frame::handshake_ack(bytes.freeze()))
                .await
                .unwrap();
            framed
        };
        let (result, _peer) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                run_connection(left, params(ConnSide::Connect, local, &shutdown)),
                exchange
            )
        })
        .await
        .unwrap();
        shutdown.send_replace(true);
        let accepted = result.is_ok();
        if let Ok(connection) = result {
            tokio::time::timeout(Duration::from_secs(2), connection.closed)
                .await
                .unwrap()
                .unwrap();
        }
        assert!(!accepted, "local={local} remote={remote} chose={chosen}");
    }
}
#[test]
fn duplicate_chosen_codec_is_rejected() {
    let mut bytes = BytesMut::new();
    ack(caps::BIN | caps::PB, "pb")
        .encode_tlv(&mut bytes)
        .unwrap();
    bytes.extend_from_slice(&[9, 3, 0, b'b', b'i', b'n']);
    assert!(HandshakeAckBody::decode_tlv(&bytes).is_err());
}
#[test]
fn impossible_peer_bounds_and_identity_are_rejected() {
    for invalid in 0..4 {
        let mut body = hello(caps::BIN);
        match invalid {
            0 => body.node_id.clear(),
            1 => body.node_id = "a\0b".into(),
            2 => body.max_frame_len = 27,
            _ => body.hop_limit = 0,
        }
        let bytes = raw_hello(&body, None);
        assert!(
            HandshakeBody::decode_tlv(&bytes).is_err(),
            "invalid={invalid}"
        );
    }
}
