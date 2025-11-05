//! Scripted loopback peer uses independently framed bytes. No app codec runs
//! on the server, so a bad reply cannot be rejected before reaching the client.
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedMessage},
};
use parrot_remote::{
    system::{RemoteActorSystem, RemoteConfig},
    NodeAddr,
};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn frame(ft: u8, cid: u64, key: &str, payload: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&(28u32 + key.len() as u32 + payload.len() as u32).to_le_bytes());
    data.extend_from_slice(&[1, ft, 0, 0]);
    data.extend_from_slice(&cid.to_le_bytes());
    data.extend_from_slice(&[0, 8, 0, 0, 0, 0, 0, 0]);
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&(key.len() as u32).to_le_bytes());
    data.extend_from_slice(key.as_bytes());
    data.extend_from_slice(payload);
    data
}
async fn read(stream: &mut tokio::net::TcpStream) -> (u8, u64) {
    maybe_read(stream)
        .await
        .expect("peer closed before expected frame")
}
async fn maybe_read(stream: &mut tokio::net::TcpStream) -> Option<(u8, u64)> {
    let size = match stream.read_u32_le().await {
        Ok(size) => size,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return None
        }
        Err(error) => panic!("peer frame read failed: {error}"),
    };
    assert!((28..=1048576).contains(&size));
    let mut body = vec![0; size as usize];
    stream.read_exact(&mut body).await.unwrap();
    Some((body[1], u64::from_le_bytes(body[4..12].try_into().unwrap())))
}
async fn ask(stream: &mut tokio::net::TcpStream) -> u64 {
    loop {
        let (ft, cid) = read(stream).await;
        match ft {
            0x10 => return cid,
            3 => stream.write_all(&frame(4, cid, "", &[])).await.unwrap(),
            _ => panic!("unexpected client frame {ft}"),
        }
    }
}

pub async fn malformed_then_valid(
    request: impl Fn() -> BoxedMessage,
    key: &'static str,
    malformed: Vec<u8>,
    valid: Vec<u8>,
) -> (ActorResult<BoxedMessage>, ActorResult<BoxedMessage>) {
    with_replies(key, malformed, valid, |_, target| async move {
        let bad = target
            .send_with_timeout(request(), Some(Duration::from_secs(3)))
            .await;
        let good = target
            .send_with_timeout(request(), Some(Duration::from_secs(3)))
            .await;
        (bad, good)
    })
    .await
}

pub async fn with_replies<R, F: std::future::Future<Output = R>>(
    key: &'static str,
    malformed: Vec<u8>,
    valid: Vec<u8>,
    run: impl FnOnce(Arc<RemoteActorSystem>, parrot_remote::RemoteActorRef) -> F,
) -> R {
    with_script(key, vec![malformed, valid], run).await
}

pub async fn with_script<R, F: std::future::Future<Output = R>>(
    key: &'static str,
    replies: Vec<Vec<u8>>,
    run: impl FnOnce(Arc<RemoteActorSystem>, parrot_remote::RemoteActorRef) -> F,
) -> R {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (ft, cid) = read(&mut stream).await;
            assert_eq!(ft, 1);
            // node_id, caps bin|pb, max_frame, topology, hop and chosen codec.
            let mut hs = Vec::new();
            for (tag, bytes) in [
                (1, b"app-codec-peer".as_slice()),
                (4, &3u32.to_le_bytes()),
                (5, &1048576u32.to_le_bytes()),
                (6, &[0]),
                (7, &[8]),
                (9, b"bin"),
            ] {
                hs.push(tag);
                hs.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                hs.extend_from_slice(bytes);
            }
            stream.write_all(&frame(2, cid, "", &hs)).await.unwrap();
            for bytes in replies {
                let cid = ask(&mut stream).await;
                stream
                    .write_all(&frame(0x11, cid, key, &bytes))
                    .await
                    .unwrap();
            }
            // Keep the link alive until client retirement, but never silently
            // swallow an unexpected business mutation after the scripted replies.
            while let Some((ft, cid)) = maybe_read(&mut stream).await {
                assert_eq!(ft, 3, "unexpected business request after scripted replies");
                stream.write_all(&frame(4, cid, "", &[])).await.unwrap();
            }
        })
        .await
        .unwrap();
    });
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("app-codec-client", None).with_insecure_development(),
        Arc::new(crate::NoopLookup),
    )
    .unwrap();
    client.start().await.unwrap();
    client
        .connect(&NodeAddr::tcp("app-codec-peer", address))
        .await
        .unwrap();
    let target = client
        .remote_ref("parrot://app-codec-peer/service")
        .unwrap();
    let result = run(client.clone(), target).await;
    client.shutdown().await.unwrap();
    peer.await.unwrap();
    result
}
