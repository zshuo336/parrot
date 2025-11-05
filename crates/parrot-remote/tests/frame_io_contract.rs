//! Distinguish transport failures, clean EOF and truncated wire input.
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use parrot_remote::{
    error::{ErrCode, RemoteError},
    frame::{frame_type, Frame, FrameError, MAX_FRAME_LEN},
    transport::FrameCodec,
};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_util::codec::{Decoder, Framed, FramedWrite};

fn encoded() -> BytesMut {
    let mut bytes = BytesMut::new();
    Frame::ask(
        42,
        "/服务",
        "bin:contract",
        Bytes::from_static(b"payload"),
        None,
    )
    .encode(&mut bytes)
    .unwrap();
    bytes
}

fn check_io(error: &FrameError, expected: io::Error) {
    assert_eq!(error.to_errcode(), ErrCode::ConnectionLost);
    assert_eq!(error.clone(), *error);
    match error {
        FrameError::Io {
            kind,
            raw_os_error,
            message,
        } => {
            assert_eq!(*kind, expected.kind());
            assert_eq!(*raw_os_error, expected.raw_os_error());
            assert_eq!(*message, expected.to_string());
        }
        other => panic!("transport error misclassified: {other:?}"),
    }
    assert!(matches!(RemoteError::from(error.clone()).to_actor_error(),
        parrot_api::errors::ActorError::InternalError(message) if message == "remote connection lost"));
}

#[test]
fn io_conversion_preserves_kind_message_and_os_code() {
    for kind in [
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::UnexpectedEof,
        io::ErrorKind::InvalidData,
    ] {
        check_io(
            &FrameError::from(io::Error::new(kind, "boundary diagnostic")),
            io::Error::new(kind, "boundary diagnostic"),
        );
    }
    #[cfg(unix)]
    check_io(
        &FrameError::from(io::Error::from_raw_os_error(libc::ECONNRESET)),
        io::Error::from_raw_os_error(libc::ECONNRESET),
    );
    assert_eq!(ErrCode::ConnectionLost as u16, 8);
    assert_eq!(ErrCode::ProtocolViolation as u16, 12);
}

#[test]
fn eof_checks_every_split_without_consuming_incomplete_input() {
    let full = encoded();
    for split in 1..full.len() {
        let mut bytes = BytesMut::from(&full[..split]);
        let original = bytes.clone();
        assert!(FrameCodec.decode(&mut bytes).unwrap().is_none());
        let error = FrameCodec.decode_eof(&mut bytes).unwrap_err();
        assert_eq!(
            error,
            FrameError::Truncated {
                received: split,
                expected: (split >= 4).then_some(full.len()),
            }
        );
        assert_eq!(bytes, original);
        assert_eq!(error.to_errcode(), ErrCode::ProtocolViolation);
        assert!(matches!(RemoteError::from(error).to_actor_error(),
            parrot_api::errors::ActorError::InternalError(message) if message.contains("protocol violation")));
    }
    let mut empty = BytesMut::new();
    assert!(FrameCodec.decode_eof(&mut empty).unwrap().is_none());
}

#[test]
fn eof_drains_complete_pipeline_before_reporting_truncated_tail() {
    let full = encoded();
    let mut pipeline = full.clone();
    pipeline.extend_from_slice(&full);
    pipeline.extend_from_slice(&full[..7]);
    for _ in 0..2 {
        let frame = FrameCodec.decode_eof(&mut pipeline).unwrap().unwrap();
        assert_eq!(frame.header.frame_type, frame_type::ASK);
        assert_eq!(frame.header.correlation_id, 42);
        assert_eq!(
            frame.split_reply_to().unwrap().1,
            Bytes::from_static(b"payload")
        );
    }
    assert_eq!(
        FrameCodec.decode_eof(&mut pipeline).unwrap_err(),
        FrameError::Truncated {
            received: 7,
            expected: Some(full.len())
        }
    );
    let mut complete = full;
    assert!(FrameCodec.decode_eof(&mut complete).unwrap().is_some());
    assert!(FrameCodec.decode_eof(&mut complete).unwrap().is_none());
}

#[test]
fn eof_preserves_specific_length_and_header_rejections() {
    let mut too_large = BytesMut::from((MAX_FRAME_LEN + 1).to_le_bytes().as_slice());
    assert!(matches!(
        FrameCodec.decode_eof(&mut too_large),
        Err(FrameError::TooLarge { .. })
    ));
    let mut too_short = BytesMut::from(1u32.to_le_bytes().as_slice());
    assert!(matches!(
        FrameCodec.decode_eof(&mut too_short),
        Err(FrameError::MalformedLengths { flen: 1, .. })
    ));
    let mut bad_version = encoded();
    bad_version[4] = 99;
    assert!(matches!(
        FrameCodec.decode_eof(&mut bad_version),
        Err(FrameError::VersionMismatch { got: 99, .. })
    ));
}

#[tokio::test]
async fn real_tcp_half_close_distinguishes_clean_and_partial_eof() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let full = encoded();
        for split in [0, 1, 3, 4, full.len() - 1, full.len()] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = async {
                let (mut socket, _) = listener.accept().await.unwrap();
                socket.write_all(&full[..split]).await.unwrap();
                socket.shutdown().await.unwrap();
            };
            let reader = async {
                let socket = tokio::net::TcpStream::connect(address).await.unwrap();
                let mut framed = Framed::new(socket, FrameCodec);
                let next = framed.next().await;
                if split == 0 {
                    assert!(next.is_none());
                } else if split == full.len() {
                    assert_eq!(next.unwrap().unwrap().header.correlation_id, 42);
                    assert!(framed.next().await.is_none());
                } else {
                    assert_eq!(
                        next.unwrap().unwrap_err(),
                        FrameError::Truncated {
                            received: split,
                            expected: (split >= 4).then_some(full.len()),
                        }
                    );
                }
            };
            tokio::join!(server, reader);
        }
    })
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn real_tcp_reset_keeps_transport_diagnostics() {
    use std::os::fd::AsRawFd;
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (ready, connected) = tokio::sync::oneshot::channel();
        let reset = async {
            let (socket, _) = listener.accept().await.unwrap();
            connected.await.unwrap();
            let linger = libc::linger {
                l_onoff: 1,
                l_linger: 0,
            };
            // SAFETY: the socket owns a live descriptor and the pointer/length
            // describe the initialized platform linger value for setsockopt.
            let rc = unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    (&linger as *const libc::linger).cast(),
                    std::mem::size_of_val(&linger) as libc::socklen_t,
                )
            };
            assert_eq!(rc, 0, "{}", io::Error::last_os_error());
            drop(socket);
        };
        let read = async {
            let socket = tokio::net::TcpStream::connect(address).await.unwrap();
            let mut framed = Framed::new(socket, FrameCodec);
            ready.send(()).unwrap();
            let error = framed
                .next()
                .await
                .expect("reset is not clean EOF")
                .unwrap_err();
            check_io(&error, io::Error::from_raw_os_error(libc::ECONNRESET));
        };
        tokio::join!(reset, read);
    })
    .await
    .unwrap();
}

struct FailedWriter;
impl AsyncWrite for FailedWriter {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "writer retired",
        )))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn framed_writer_failure_is_not_a_protocol_violation() {
    let mut writer = FramedWrite::new(FailedWriter, FrameCodec);
    let error = writer.send(Frame::heartbeat()).await.unwrap_err();
    check_io(
        &error,
        io::Error::new(io::ErrorKind::BrokenPipe, "writer retired"),
    );
}

#[tokio::test]
async fn eof_and_bad_frames_retire_the_actual_connection_once() {
    use parrot_remote::{
        handshake::HandshakeBody,
        transport::{run_connection, ConnParams, ConnSide, RuntimeKnobs},
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(Duration::from_secs(5), async {
        for tail in [Vec::new(), vec![32], vec![32, 0, 0, 0], vec![1, 0, 0, 0]] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (shutdown, _) = tokio::sync::watch::channel(false);
            let (inbound, mut received) = tokio::sync::mpsc::channel(2);
            let closed = Arc::new(AtomicUsize::new(0));
            let close_count = closed.clone();
            let accepted = async {
                let (socket, _) = listener.accept().await.unwrap();
                let handle = run_connection(
                    socket,
                    ConnParams {
                        side: ConnSide::Accept,
                        local_addr: Some(address),
                        peer_addr: None,
                        scheme: "tcp",
                        local_handshake: HandshakeBody {
                            node_id: "io-boundary-server".into(),
                            ..Default::default()
                        },
                        inbound,
                        on_disconnect: Arc::new(move |sender| {
                            assert!(sender.is_closed());
                            close_count.fetch_add(1, Ordering::SeqCst);
                        }),
                        shutdown: shutdown.subscribe(),
                        knobs: Some(RuntimeKnobs {
                            heartbeat_enabled: false,
                            ..Default::default()
                        }),
                    },
                )
                .await
                .unwrap();
                assert_eq!(handle.node_id, "io-boundary-peer");
                handle.closed.await.unwrap();
                assert!(handle.sender.is_closed());
                assert_eq!(closed.load(Ordering::SeqCst), 1);
                assert!(received.recv().await.is_none());
            };
            let peer = async {
                let socket = tokio::net::TcpStream::connect(address).await.unwrap();
                let mut framed = Framed::new(socket, FrameCodec);
                let mut hello = BytesMut::new();
                HandshakeBody {
                    node_id: "io-boundary-peer".into(),
                    ..Default::default()
                }
                .encode_tlv(&mut hello)
                .unwrap();
                framed.send(Frame::handshake(hello.freeze())).await.unwrap();
                assert_eq!(
                    framed.next().await.unwrap().unwrap().header.frame_type,
                    frame_type::HANDSHAKE_ACK
                );
                let mut raw = framed.into_inner();
                raw.write_all(&tail).await.unwrap();
                raw.shutdown().await.unwrap();
                assert_eq!(raw.read(&mut [0; 1]).await.unwrap(), 0);
            };
            tokio::join!(accepted, peer);
        }
    })
    .await
    .unwrap();
}
