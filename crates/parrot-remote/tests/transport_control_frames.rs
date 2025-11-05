//! Heartbeat acknowledgements belong to the connection, not application ingress.
use parrot_remote::{
    frame::{frame_type, Frame},
    handshake::HandshakeBody,
    transport::{run_connection, ConnParams, ConnSide, RuntimeKnobs},
};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn heartbeat_ack_does_not_occupy_application_ingress() {
    let (left, right) = tokio::io::duplex(4096);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let (in_a, _rx_a) = tokio::sync::mpsc::channel(8);
    let (in_b, mut rx_b) = tokio::sync::mpsc::channel(1);
    let params = |side, name: &str, inbound| ConnParams {
        side,
        local_addr: None,
        peer_addr: None,
        scheme: "mem",
        local_handshake: HandshakeBody {
            node_id: name.into(),
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
    let (a, b) = tokio::try_join!(
        run_connection(left, params(ConnSide::Connect, "ack-a", in_a)),
        run_connection(right, params(ConnSide::Accept, "ack-b", in_b))
    )
    .unwrap();
    a.sender.send(Frame::heartbeat_ack()).await.unwrap();
    a.sender
        .send(Frame::ask(
            42,
            "/business",
            "key",
            bytes::Bytes::from_static(b"payload"),
            None,
        ))
        .await
        .unwrap();
    // The later business frame is a same-stream barrier for the earlier ACK.
    let (received, _, _) = tokio::time::timeout(Duration::from_secs(2), rx_b.recv())
        .await
        .unwrap()
        .unwrap();
    shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), async {
        a.closed.await.unwrap();
        b.closed.await.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(received.header.frame_type, frame_type::ASK);
    assert_eq!(received.header.correlation_id, 42);
    let (reply_to, payload) = received.split_reply_to().unwrap();
    assert_eq!(reply_to, None);
    assert_eq!(payload.as_ref(), b"payload");
}
