use bytes::BytesMut;
use parrot_remote::{
    handshake::{caps, HandshakeAckBody, HandshakeBody, TopologyRole},
    transport::{run_connection, ConnParams, ConnSide},
};
use std::{sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;

fn hello() -> HandshakeBody {
    HandshakeBody {
        node_id: "local".into(),
        ..Default::default()
    }
}
fn ack() -> HandshakeAckBody {
    HandshakeAckBody {
        node_id: "local".into(),
        realm: None,
        cluster: None,
        capabilities: caps::BIN,
        max_frame_len: 1 << 20,
        topology_role: TopologyRole::Normal,
        hop_limit: 8,
        direct_addr: None,
        chosen_codec: "bin".into(),
    }
}
#[test]
fn oversized_hello_fields_preserve_output() {
    let mut corrupted = Vec::new();
    for field in 0..4 {
        let mut body = hello();
        let long = "x".repeat(65536);
        match field {
            0 => body.node_id = long,
            1 => body.realm = Some(long),
            2 => body.cluster = Some(long),
            _ => body.direct_addr = Some(long),
        }
        let mut out = BytesMut::from(&b"prior frame"[..]);
        let error = body.encode_tlv(&mut out).unwrap_err();
        assert!(matches!(
            error,
            parrot_remote::handshake::HandshakeError::BadLen(_, 65536)
        ));
        if &out[..] != b"prior frame" {
            corrupted.push(field);
        }
    }
    assert!(
        corrupted.is_empty(),
        "overflowing HELLO fields appended bytes: {corrupted:?}"
    );
}
#[test]
fn oversized_ack_fields_preserve_output() {
    let mut corrupted = Vec::new();
    for field in 0..5 {
        let mut body = ack();
        let long = "x".repeat(65536);
        match field {
            0 => body.node_id = long,
            1 => body.realm = Some(long),
            2 => body.cluster = Some(long),
            3 => body.direct_addr = Some(long),
            _ => body.chosen_codec = long,
        }
        let mut out = BytesMut::from(&b"prior frame"[..]);
        let error = body.encode_tlv(&mut out).unwrap_err();
        assert!(matches!(
            error,
            parrot_remote::handshake::HandshakeError::BadLen(_, 65536)
        ));
        if &out[..] != b"prior frame" {
            corrupted.push(field);
        }
    }
    assert!(
        corrupted.is_empty(),
        "overflowing ACK fields appended bytes: {corrupted:?}"
    );
}
#[test]
fn hello_rejects_ack_only_field() {
    let mut bytes = BytesMut::new();
    hello().encode_tlv(&mut bytes).unwrap();
    bytes.extend_from_slice(&[9, 3, 0, b'b', b'i', b'n']);
    assert!(HandshakeBody::decode_tlv(&bytes).is_err());
}
async fn rejected_local(side: ConnSide) {
    let (left, mut right) = tokio::io::duplex(4096);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let (inbound, _) = tokio::sync::mpsc::channel(1);
    let mut body = hello();
    body.realm = Some("x".repeat(65536));
    let params = ConnParams {
        side,
        local_addr: None,
        peer_addr: None,
        scheme: "mem",
        local_handshake: body,
        inbound,
        on_disconnect: Arc::new(|_| {}),
        shutdown: shutdown.subscribe(),
        knobs: None,
    };
    let mut received = Vec::new();
    let (connection, read) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            run_connection(left, params),
            right.read_to_end(&mut received)
        )
    })
    .await
    .expect("invalid local identity must fail before handshake I/O");
    assert!(connection.is_err());
    assert_eq!(read.unwrap(), 0);
    assert!(received.is_empty());
}
#[tokio::test]
async fn invalid_local_connect_writes_no_bytes() {
    rejected_local(ConnSide::Connect).await;
}
#[tokio::test]
async fn invalid_local_accept_does_not_wait_for_peer() {
    rejected_local(ConnSide::Accept).await;
}

#[test]
fn exact_u16_utf8_boundary_roundtrips_and_appends() {
    let maximum = "é".repeat(32767) + "x";
    assert_eq!(maximum.len(), 65535);
    let mut hello = hello();
    hello.node_id = maximum.clone();
    hello.realm = Some(maximum.clone());
    hello.cluster = Some(maximum.clone());
    hello.direct_addr = Some(maximum.clone());
    let mut output = BytesMut::from(&b"prefix"[..]);
    hello.encode_tlv(&mut output).unwrap();
    assert_eq!(&output[..6], b"prefix");
    assert_eq!(&output[6..9], &[1, 255, 255]);
    assert_eq!(HandshakeBody::decode_tlv(&output[6..]).unwrap(), hello);
    let mut ack = ack();
    ack.node_id = maximum.clone();
    ack.realm = Some(maximum.clone());
    ack.cluster = Some(maximum.clone());
    ack.direct_addr = Some(maximum);
    output.truncate(6);
    ack.encode_tlv(&mut output).unwrap();
    assert_eq!(&output[..6], b"prefix");
    assert_eq!(HandshakeAckBody::decode_tlv(&output[6..]).unwrap(), ack);
}
#[test]
fn invalid_local_values_do_not_modify_existing_frames() {
    for invalid in 0..4 {
        let mut hello = hello();
        let mut ack = ack();
        match invalid {
            0 => {
                hello.node_id.clear();
                ack.node_id.clear();
            }
            1 => {
                hello.node_id = "a\0b".into();
                ack.node_id = "a\0b".into();
            }
            2 => {
                hello.max_frame_len = 27;
                ack.max_frame_len = 27;
            }
            _ => {
                hello.hop_limit = 0;
                ack.hop_limit = 0;
            }
        }
        let mut bytes = BytesMut::from(&b"prefix"[..]);
        assert!(hello.encode_tlv(&mut bytes).is_err());
        assert_eq!(&bytes[..], b"prefix");
        assert!(ack.encode_tlv(&mut bytes).is_err());
        assert_eq!(&bytes[..], b"prefix");
    }
}
#[test]
fn ack_encoder_rejects_unknown_or_unshared_choice_atomically() {
    for (caps, codec) in [
        (caps::BIN, "pb"),
        (caps::PB, "bin"),
        (caps::BIN | caps::PB, "unknown"),
        (0, "bin"),
    ] {
        let mut ack = ack();
        ack.capabilities = caps;
        ack.chosen_codec = codec.into();
        let mut bytes = BytesMut::from(&b"prefix"[..]);
        assert!(ack.encode_tlv(&mut bytes).is_err());
        assert_eq!(&bytes[..], b"prefix");
    }
}
