use bytes::{Bytes, BytesMut};
use futures::FutureExt;
use parrot_api::{address::ActorRef, errors::ActorError};
use remote_poc::*;
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn invalid_frames() -> Vec<(Frame, &'static str)> {
    let mut unknown = Frame::tell("", "", vec![]);
    unknown.frame_type = 0xff;
    vec![
        (
            Frame::tell("", "", vec![0; MAX_FRAME_BODY - 27]),
            "size limit",
        ),
        (
            Frame::tell("p".repeat(MAX_FRAME_BODY - 27), "", vec![]),
            "size limit",
        ),
        (
            Frame::tell("", "k".repeat(MAX_FRAME_BODY - 27), vec![]),
            "size limit",
        ),
        (unknown, "unknown frame type"),
    ]
}

fn boundary_frame() -> Frame {
    // Limits count UTF-8 bytes of the entire body, not only the payload.
    Frame::tell(
        "/目录",
        "bin:界",
        vec![42; MAX_FRAME_BODY - 28 - "/目录".len() - "bin:界".len()],
    )
}

fn reject_immediately(sender: &FrameSender, frame: Frame, expected: &str) {
    let error = sender
        .send(frame)
        .now_or_never()
        .expect("invalid input must not wait for queue capacity")
        .expect_err("invalid local input must fail before admission");
    assert!(error.contains(expected), "wrong error: {error}");
    assert!(
        !sender.is_closed(),
        "caller error must not retire the shared link"
    );
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("contract deadline")
}

#[tokio::test]
async fn memory_rejects_invalid_frames_without_delivering_them_or_poisoning_link() {
    let (link, mut peer) = memory_pair();
    for (invalid, error) in invalid_frames() {
        reject_immediately(&link.sender, invalid, error);
        assert!(matches!(
            peer.incoming.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        let frame = boundary_frame();
        link.sender.send(frame.clone()).await.unwrap();
        assert_eq!(bounded(peer.incoming.recv()).await.unwrap(), frame);
    }
}

async fn tcp_contract(accepting: bool) {
    let (mut link, mut peer) = if accepting {
        let (addr, accepted) = tcp_listen().await.unwrap();
        let peer = tokio::net::TcpStream::connect(addr).await.unwrap();
        (bounded(accepted).await.unwrap(), peer)
    } else {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let link = tcp_connect(listener.local_addr().unwrap()).await.unwrap();
        (link, bounded(listener.accept()).await.unwrap().0)
    };
    for (invalid, error) in invalid_frames() {
        reject_immediately(&link.sender, invalid, error);
        let frame = boundary_frame();
        link.sender.send(frame.clone()).await.unwrap();
        let mut prefix = [0; 4];
        bounded(peer.read_exact(&mut prefix)).await.unwrap();
        assert_eq!(u32::from_le_bytes(prefix) as usize, MAX_FRAME_BODY);
        let mut body = vec![0; MAX_FRAME_BODY];
        bounded(peer.read_exact(&mut body)).await.unwrap();
        assert_eq!(&body[..4], &[1, 0x13, 0, 0]);
        assert_eq!(&body[24..31], "/目录".as_bytes());
        assert_eq!(&body[35..42], "bin:界".as_bytes());
        assert!(body[42..].iter().all(|byte| *byte == 42));
        // Echo the observed bytes, proving the inbound half is also usable.
        peer.write_all(&prefix).await.unwrap();
        bounded(peer.write_all(&body)).await.unwrap();
        assert_eq!(bounded(link.incoming.recv()).await.unwrap(), frame);
    }
    drop(peer);
    bounded(link.sender.closed()).await;
}

#[tokio::test]
async fn tcp_dial_rejection_preserves_exact_boundary_bidirectional_traffic() {
    tcp_contract(false).await;
}

#[tokio::test]
async fn tcp_accept_rejection_preserves_exact_boundary_bidirectional_traffic() {
    tcp_contract(true).await;
}

#[tokio::test]
async fn invalid_input_is_rejected_before_full_queue_and_does_not_consume_capacity() {
    let (link, mut peer) = memory_pair();
    for cid in 0..1024 {
        link.sender
            .send(Frame::ask(cid, "", "", vec![]))
            .await
            .unwrap();
    }
    for (invalid, error) in invalid_frames() {
        reject_immediately(&link.sender, invalid, error);
    }
    for cid in 0..1024 {
        assert_eq!(peer.incoming.recv().await.unwrap().correlation_id, cid);
    }
    assert!(peer.incoming.try_recv().is_err());
    link.sender
        .send(Frame::tell("", "", vec![7]))
        .await
        .unwrap();
    assert_eq!(
        peer.incoming.recv().await.unwrap().payload,
        Bytes::from_static(&[7])
    );
}

#[tokio::test]
async fn rejected_remote_ask_retires_callback_and_deliver_stop_do_not_enqueue() {
    install_poc_messages();
    let (link, mut peer) = memory_pair();
    let callbacks = Arc::new(CallbackRegistry::new());
    let remote = RemoteActorRef::new(
        "p".repeat(MAX_FRAME_BODY),
        link.sender.clone(),
        callbacks.clone(),
    );
    let result = remote
        .send(Box::new(Ping(1)))
        .now_or_never()
        .expect("oversized remote ASK must fail without awaiting a reply");
    assert!(
        matches!(result, Err(ActorError::InternalError(error)) if error.contains("size limit"))
    );
    assert_eq!(callbacks.pending_count(), 0);
    assert!(remote.deliver(Box::new(Ping(1))).await.is_err());
    assert!(remote.stop().await.is_err());
    assert!(matches!(
        peer.incoming.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    link.sender
        .send(Frame::tell("/ok", "", vec![]))
        .await
        .unwrap();
    assert_eq!(peer.incoming.recv().await.unwrap().path, "/ok");
}

#[test]
fn encoding_rejection_is_atomic_for_every_invalid_field() {
    for (frame, expected) in invalid_frames() {
        let mut buffer = BytesMut::from(&b"existing frame"[..]);
        let original = buffer.clone();
        assert!(frame.encode(&mut buffer).unwrap_err().contains(expected));
        assert_eq!(buffer, original);
    }
}
