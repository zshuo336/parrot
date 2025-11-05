use parrot_api::address::ActorRef;
use remote_poc::*;
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("contract deadline")
}

#[tokio::test]
async fn ask_timeout_covers_full_admission_queue_and_cancellation_removes_callback() {
    install_poc_messages();
    let (a, mut b) = memory_pair();
    for _ in 0..1024 {
        a.sender
            .send(Frame::tell("/x", "bin:u:Ping", vec![0; 8]))
            .await
            .unwrap();
    }
    let callbacks = Arc::new(CallbackRegistry::new());
    let remote = RemoteActorRef::new("/x", a.sender.clone(), callbacks.clone());
    let result =
        bounded(remote.send_with_timeout(Box::new(Ping(1)), Some(Duration::from_millis(20)))).await;
    assert!(matches!(
        result,
        Err(parrot_api::errors::ActorError::TimeoutDetail(_))
    ));
    assert_eq!(callbacks.pending_count(), 0);
    for _ in 0..1024 {
        b.incoming.recv().await.unwrap();
    }
    assert!(
        b.incoming.try_recv().is_err(),
        "timed out admission must not enqueue a request"
    );
    let call = tokio::spawn(async move { remote.send(Box::new(Ping(2))).await });
    let sent = bounded(b.incoming.recv()).await.unwrap();
    assert_eq!(callbacks.pending_count(), 1);
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    assert_eq!(callbacks.pending_count(), 0);
    assert!(!callbacks.complete(Frame::reply(sent.correlation_id, "bin:u:Pong", vec![0; 8])));
}

#[tokio::test]
async fn actual_tcp_disconnect_fails_unbounded_ask_and_closes_writer() {
    install_poc_messages();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let link = tcp_connect(listener.local_addr().unwrap()).await.unwrap();
    let (mut peer, _) = listener.accept().await.unwrap();
    let ep = spawn_endpoint(link).await;
    let remote = ep.remote_ref("/x");
    let call = tokio::spawn(async move { remote.send(Box::new(Ping(1))).await });
    let mut prefix = [0u8; 4];
    bounded(peer.read_exact(&mut prefix)).await.unwrap();
    let mut body = vec![0; u32::from_le_bytes(prefix) as usize];
    bounded(peer.read_exact(&mut body)).await.unwrap();
    assert_eq!(body[1], 0x10);
    drop(peer);
    assert!(bounded(call).await.unwrap().is_err());
    bounded(ep.sender().closed()).await;
    assert_eq!(ep.callbacks.pending_count(), 0);
}

#[tokio::test]
async fn malformed_tcp_frame_closes_both_halves_without_consuming_following_valid_frame() {
    let (addr, accepted) = tcp_listen().await.unwrap();
    let mut peer = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut link = accepted.await.unwrap();
    let mut bytes = bytes::BytesMut::from(&[0u8; 4][..]);
    Frame::ask(1, "/x", "bin:u:Ping", vec![0; 8])
        .encode(&mut bytes)
        .unwrap();
    peer.write_all(&bytes).await.unwrap();
    assert!(bounded(link.incoming.recv()).await.is_none());
    bounded(link.sender.closed()).await;
    let mut byte = [0];
    assert_eq!(bounded(peer.read(&mut byte)).await.unwrap(), 0);
}

#[tokio::test]
async fn dropping_endpoint_retires_its_ingress_and_pending_requests() {
    install_poc_messages();
    let (link, mut peer) = memory_pair();
    let ep = spawn_endpoint(link).await;
    let remote = ep.remote_ref("/x");
    let cb = ep.callbacks.clone();
    let call = tokio::spawn(async move { remote.send(Box::new(Ping(1))).await });
    bounded(peer.incoming.recv()).await.unwrap();
    drop(ep);
    assert!(bounded(call).await.unwrap().is_err());
    bounded(peer.sender.closed()).await;
    assert_eq!(cb.pending_count(), 0);
}

#[tokio::test]
async fn cancelled_accept_releases_listener() {
    let (addr, accepted) = tcp_listen().await.unwrap();
    drop(accepted);
    bounded(async {
        loop {
            if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
                drop(listener);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

#[tokio::test]
async fn route_segment_boundaries_and_reply_namespaces_are_isolated() {
    let (a, _peer_a) = memory_pair();
    let (b, _peer_b) = memory_pair();
    let ca = Arc::new(CallbackRegistry::new());
    let cb = Arc::new(CallbackRegistry::new());
    let router = PrefixRouter::new();
    router.add("/erl", a.sender, ca.clone());
    router.add("/erl/nested", b.sender, cb.clone());
    assert!(router.resolve("/erlang").is_none());
    assert!(router.resolve("/erl-other/x").is_none());
    assert!(Arc::ptr_eq(&router.resolve("/erl").unwrap().callbacks, &ca));
    assert!(Arc::ptr_eq(
        &router.resolve("/erl/nested/x").unwrap().callbacks,
        &cb
    ));
    let (ta, mut ra) = tokio::sync::oneshot::channel();
    let (tb, rb) = tokio::sync::oneshot::channel();
    ca.insert(1, ta);
    cb.insert(1, tb);
    assert!(router
        .resolve("/erl/nested/x")
        .unwrap()
        .callbacks
        .complete(Frame::reply(1, "", vec![2])));
    assert_eq!(rb.await.unwrap().payload.as_ref(), &[2]);
    assert!(matches!(
        ra.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(ca.complete(Frame::reply(1, "", vec![1])));
    assert_eq!(ra.await.unwrap().payload.as_ref(), &[1]);
}
