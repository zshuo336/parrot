//! Untrusted TLVs must not allocate storage proportional to their field count.
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use parrot_remote::{
    frame::Frame,
    handshake::{HandshakeAckBody, HandshakeBody, HandshakeError},
    transport::{run_connection, ConnParams, ConnSide, FrameCodec, RuntimeKnobs},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::Arc,
    time::Duration,
};
use tokio_util::codec::Framed;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static TOTAL: Cell<usize> = const { Cell::new(0) };
}
struct Meter;
fn record(size: usize) {
    if ENABLED.try_with(Cell::get).unwrap_or(false) {
        let _ = TOTAL.try_with(|n| n.set(n.get().saturating_add(size)));
    }
}
// SAFETY: System receives the original pointers/layouts. Thread-local accounting
// neither allocates nor dereferences pointers and tolerates TLS destruction.
unsafe impl GlobalAlloc for Meter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(pointer, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: Meter = Meter;

fn bounded<T>(run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ENABLED.set(false);
        }
    }
    TOTAL.set(0);
    ENABLED.set(true);
    let reset = Reset;
    let result = run();
    drop(reset);
    assert!(
        TOTAL.get() < 16 * 1024,
        "decoder allocated {} bytes",
        TOTAL.get()
    );
    result
}

// Literal fixtures do not depend on the production encoder. max_frame_len=1MiB.
fn wire(ack: bool) -> Vec<u8> {
    let mut out = vec![
        1, 4, 0, b'p', b'e', b'e', b'r', 4, 4, 0, 1, 0, 0, 0, 5, 4, 0, 0, 0, 16, 0, 6, 1, 0, 0,
    ];
    if ack {
        out.extend_from_slice(&[9, 3, 0, b'b', b'i', b'n']);
    }
    out
}
fn flood(out: &mut Vec<u8>, tag: u8, payload: &[u8]) {
    for _ in 0..32_768 {
        out.push(tag);
        out.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_le_bytes());
        out.extend_from_slice(payload);
    }
}

#[test]
fn repeated_unknown_fields_use_constant_decoder_storage() {
    for ack in [false, true] {
        for payload in [&b""[..], &b"future extension"[..]] {
            let plain = wire(ack);
            let mut extended = plain.clone();
            flood(&mut extended, 255, payload);
            // Include every other unknown tag, including reserved zero.
            for tag in std::iter::once(0).chain(10..255) {
                extended.extend_from_slice(&[tag, 0, 0]);
            }
            if ack {
                assert_eq!(
                    bounded(|| HandshakeAckBody::decode_tlv(&extended)),
                    HandshakeAckBody::decode_tlv(&plain)
                );
            } else {
                assert_eq!(
                    bounded(|| HandshakeBody::decode_tlv(&extended)),
                    HandshakeBody::decode_tlv(&plain)
                );
            }
        }
    }
}

#[test]
fn known_duplicate_flood_is_rejected_with_bounded_storage() {
    for tag in 1..=9 {
        let mut bytes = wire(true);
        flood(&mut bytes, tag, b"duplicate");
        assert_eq!(
            bounded(|| HandshakeAckBody::decode_tlv(&bytes)),
            Err(HandshakeError::DuplicateTag(tag))
        );
    }
}

#[test]
fn truncated_fields_after_extensions_preserve_error_priority() {
    for tag in [1, 9, 255] {
        for tail in [vec![tag], vec![tag, 1], vec![tag, 2, 0, 42]] {
            let mut bytes = wire(false);
            flood(&mut bytes, 255, b"extension");
            bytes.extend_from_slice(&tail);
            let expected = if tail.len() < 3 {
                HandshakeError::Truncated(tag, 0, tail.len())
            } else {
                HandshakeError::Truncated(tag, 2, 1)
            };
            assert_eq!(
                bounded(|| HandshakeBody::decode_tlv(&bytes)),
                Err(expected.clone())
            );
            assert_eq!(
                bounded(|| HandshakeAckBody::decode_tlv(&bytes)),
                Err(expected)
            );
        }
    }
    // A malformed tail still wins over an earlier known duplicate.
    let mut bytes = wire(true);
    flood(&mut bytes, 1, b"duplicate");
    bytes.push(255);
    assert_eq!(
        bounded(|| HandshakeAckBody::decode_tlv(&bytes)),
        Err(HandshakeError::Truncated(255, 0, 1))
    );
}

#[test]
fn chosen_codec_contract_survives_unknown_extensions() {
    let mut bytes = wire(false);
    flood(&mut bytes, 255, b"");
    assert_eq!(
        bounded(|| HandshakeAckBody::decode_tlv(&bytes))
            .unwrap()
            .chosen_codec,
        "bin"
    );
    bytes.extend_from_slice(&[9, 3, 0, b'b', b'i', b'n']);
    assert_eq!(
        bounded(|| HandshakeBody::decode_tlv(&bytes)),
        Err(HandshakeError::InvalidValue(9))
    );
    bytes.extend_from_slice(&[9, 3, 0, b'b', b'i', b'n']);
    assert_eq!(
        bounded(|| HandshakeBody::decode_tlv(&bytes)),
        Err(HandshakeError::DuplicateTag(9))
    );
    assert_eq!(
        bounded(|| HandshakeAckBody::decode_tlv(&bytes)),
        Err(HandshakeError::DuplicateTag(9))
    );
}

#[tokio::test]
async fn tcp_extensions_work_and_malformed_handshakes_do_not_admit_work() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        for side in [ConnSide::Connect, ConnSide::Accept] {
            // A valid connection after each bad input must still be usable.
            for malformed in [true, false] {
                let stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                    .await
                    .unwrap();
                let (socket, _) = listener.accept().await.unwrap();
                let (shutdown, _) = tokio::sync::watch::channel(false);
                let (inbound, mut received) = tokio::sync::mpsc::channel(8);
                let params = ConnParams {
                    side,
                    local_addr: None,
                    peer_addr: None,
                    scheme: "tcp",
                    local_handshake: HandshakeBody {
                        node_id: "local".into(),
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
                let exchange = async {
                    let mut peer = Framed::new(socket, FrameCodec);
                    let is_ack = matches!(side, ConnSide::Connect);
                    if is_ack {
                        peer.next().await.unwrap().unwrap();
                    }
                    let mut payload = wire(is_ack);
                    flood(&mut payload, 255, b"future");
                    if malformed {
                        payload.push(255);
                    }
                    let handshake = if is_ack {
                        Frame::handshake_ack(payload.into())
                    } else {
                        Frame::handshake(payload.into())
                    };
                    // Pipeline application traffic in the very same TCP write.
                    let mut packet = BytesMut::new();
                    handshake.encode(&mut packet).unwrap();
                    Frame::tell("/actor", "opaque", Bytes::from_static(b"work"))
                        .encode(&mut packet)
                        .unwrap();
                    use tokio::io::AsyncWriteExt;
                    peer.get_mut().write_all(&packet).await.unwrap();
                    peer
                };
                let (result, mut peer) = tokio::join!(run_connection(stream, params), exchange);
                if malformed {
                    assert!(result.is_err());
                    assert!(
                        received.recv().await.is_none(),
                        "work was admitted before handshake validation"
                    );
                    // Bad peer gets ERROR or EOF, never a successful ACK.
                    while let Some(Ok(frame)) = peer.next().await {
                        assert_eq!(
                            frame.header.frame_type,
                            parrot_remote::frame::frame_type::ERROR
                        );
                    }
                } else {
                    let owner = result.unwrap();
                    if matches!(side, ConnSide::Accept) {
                        let ack = peer.next().await.unwrap().unwrap();
                        assert_eq!(
                            HandshakeAckBody::decode_tlv(&ack.payload).unwrap().node_id,
                            "local"
                        );
                    }
                    let inbound = received.recv().await.unwrap();
                    assert_eq!(inbound.0.payload.as_ref(), b"work");
                    owner
                        .sender
                        .send(Frame::tell("/reply", "opaque", Bytes::from_static(b"ok")))
                        .await
                        .unwrap();
                    assert_eq!(peer.next().await.unwrap().unwrap().payload.as_ref(), b"ok");
                    shutdown.send_replace(true);
                    owner.closed.await.unwrap();
                }
            }
        }
    })
    .await
    .unwrap();
}
