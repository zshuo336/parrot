//! Shutdown is observable at the listener and handshake boundary, not just a flag.
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{sync::Arc, time::Duration};
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        None
    }
}

#[tokio::test]
async fn tcp_shutdown_releases_listener_while_system_handle_is_retained() {
    let system = RemoteActorSystem::new(
        RemoteConfig::tcp("release-tcp", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Empty),
    )
    .unwrap();
    system.start().await.unwrap();
    let address = system.local_addr().unwrap();
    system.shutdown().await.unwrap();
    let rebound = tokio::net::TcpListener::bind(address).await;
    assert!(
        rebound.is_ok(),
        "shutdown retained TCP listener: {:?}",
        rebound.err()
    );
    assert!(system.start().await.is_err());
}

#[tokio::test]
async fn memory_shutdown_removes_endpoint_while_system_handle_is_retained() {
    let system = RemoteActorSystem::new(RemoteConfig::mem("release-mem"), Arc::new(Empty)).unwrap();
    let client =
        RemoteActorSystem::new(RemoteConfig::mem("release-mem-client"), Arc::new(Empty)).unwrap();
    system.start().await.unwrap();
    client.start().await.unwrap();
    system.shutdown().await.unwrap();
    let attempt = tokio::time::timeout(
        Duration::from_millis(300),
        client.connect(&NodeAddr::mem("release-mem")),
    )
    .await;
    client.shutdown().await.unwrap();
    assert!(attempt
        .expect("closed endpoint must fail immediately, not stall in handshake")
        .is_err());
}

#[tokio::test]
async fn quic_shutdown_releases_udp_listener_while_system_handle_is_retained() {
    let system = RemoteActorSystem::new(
        RemoteConfig::quic("release-quic", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    system.start().await.unwrap();
    let address = system.local_addr().unwrap();
    system.shutdown().await.unwrap();
    let rebound = tokio::net::UdpSocket::bind(address).await;
    assert!(
        rebound.is_ok(),
        "shutdown retained QUIC UDP socket: {:?}",
        rebound.err()
    );
}

fn params(shutdown: tokio::sync::watch::Receiver<bool>) -> parrot_remote::transport::ConnParams {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    parrot_remote::transport::ConnParams {
        side: parrot_remote::transport::ConnSide::Connect,
        local_addr: None,
        peer_addr: None,
        scheme: "mem",
        local_handshake: parrot_remote::handshake::HandshakeBody {
            node_id: "cancel-handshake".into(),
            ..Default::default()
        },
        inbound: tx,
        on_disconnect: Arc::new(|_| {}),
        shutdown,
        knobs: None,
    }
}
#[tokio::test]
async fn shutdown_interrupts_blocked_handshake_write() {
    use tokio::io::AsyncReadExt;
    let (local, mut peer) = tokio::io::duplex(1);
    let (shutdown, rx) = tokio::sync::watch::channel(false);
    let mut task = tokio::spawn(parrot_remote::transport::run_connection(local, params(rx)));
    let mut first = [0];
    tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut first))
        .await
        .unwrap()
        .unwrap();
    shutdown.send_replace(true);
    let result = tokio::time::timeout(Duration::from_millis(300), &mut task).await;
    if result.is_err() {
        task.abort();
        let _ = task.await;
    }
    assert!(result
        .expect("shutdown must interrupt the handshake write")
        .unwrap()
        .is_err());
}
#[tokio::test]
async fn shutdown_interrupts_waiting_for_handshake_ack() {
    use futures::StreamExt;
    let (local, peer) = tokio::io::duplex(4096);
    let (shutdown, rx) = tokio::sync::watch::channel(false);
    let mut task = tokio::spawn(parrot_remote::transport::run_connection(local, params(rx)));
    let mut peer = tokio_util::codec::Framed::new(peer, parrot_remote::transport::FrameCodec);
    let hello = tokio::time::timeout(Duration::from_secs(5), peer.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        hello.header.frame_type,
        parrot_remote::frame::frame_type::HANDSHAKE
    );
    shutdown.send_replace(true);
    let result = tokio::time::timeout(Duration::from_millis(300), &mut task).await;
    if result.is_err() {
        task.abort();
        let _ = task.await;
    }
    assert!(result
        .expect("shutdown must interrupt waiting for handshake ACK")
        .unwrap()
        .is_err());
}

#[tokio::test(start_paused = true)]
async fn handshake_deadline_includes_blocked_write() {
    use tokio::io::AsyncReadExt;
    let (local, mut peer) = tokio::io::duplex(1);
    let (_shutdown, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(parrot_remote::transport::run_connection(local, params(rx)));
    peer.read_exact(&mut [0]).await.unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    let result = tokio::time::timeout(Duration::from_millis(1), task)
        .await
        .expect("whole handshake deadline must include write")
        .unwrap();
    assert!(
        matches!(result, Err(parrot_remote::error::RemoteError::Handshake(message)) if message.contains("timeout"))
    );
}

#[tokio::test]
async fn active_quic_shutdown_releases_both_endpoints_with_senders_retained() {
    let server = RemoteActorSystem::new(
        RemoteConfig::quic("active-quic-server", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    let client = RemoteActorSystem::new(
        RemoteConfig::quic("active-quic-client", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    server.start().await.unwrap();
    client.start().await.unwrap();
    let server_addr = server.local_addr().unwrap();
    let client_addr = client.local_addr().unwrap();
    client
        .connect(&NodeAddr::quic("active-quic-server", server_addr))
        .await
        .unwrap();
    let senders = client.links_snapshot().await;
    assert_eq!(senders.len(), 1);
    tokio::time::timeout(Duration::from_secs(10), async {
        let (a, b) = tokio::join!(client.shutdown(), server.shutdown());
        a.unwrap();
        b.unwrap();
    })
    .await
    .expect("QUIC drivers must exit");
    assert!(senders[0].1.is_closed());
    let _client_rebound = tokio::net::UdpSocket::bind(client_addr)
        .await
        .expect("client socket released");
    let _server_rebound = tokio::net::UdpSocket::bind(server_addr)
        .await
        .expect("server socket released");
    assert!(client
        .connect(&NodeAddr::quic("active-quic-server", server_addr))
        .await
        .is_err());
}

#[tokio::test]
async fn explicit_memory_pair_is_closed_and_cannot_publish_after_shutdown() {
    let a = RemoteActorSystem::new(RemoteConfig::mem("direct-close-a"), Arc::new(Empty)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("direct-close-b"), Arc::new(Empty)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();
    let senders = a.links_snapshot().await;
    a.shutdown().await.unwrap();
    assert!(senders[0].1.is_closed());
    assert!(a.links_snapshot().await.is_empty());
    assert!(a.connect_mem_pair(&b).await.is_err());
    assert!(b.connect_mem_pair(&a).await.is_err());
    assert!(a.links_snapshot().await.is_empty());
    b.shutdown().await.unwrap();
}

fn tcp_transport() -> parrot_remote::transport::tcp::TcpTransport {
    let (inbound, _) = tokio::sync::mpsc::channel(1);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    parrot_remote::transport::tcp::TcpTransport::new(
        parrot_remote::handshake::HandshakeBody {
            node_id: "tcp-peer".into(),
            ..Default::default()
        },
        inbound,
        Arc::new(|_| {}),
        shutdown,
        None,
    )
}

#[tokio::test]
async fn transport_shutdown_joins_outbound_handshake_and_rejects_reopen() {
    use futures::StreamExt;
    use parrot_remote::transport::{FrameCodec, Transport};
    let transport = Arc::new(tcp_transport());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let dialer = transport.clone();
    let connect =
        tokio::spawn(async move { dialer.connect(&NodeAddr::tcp("silent", address)).await });
    let (peer, _) = listener.accept().await.unwrap();
    let mut peer = tokio_util::codec::Framed::new(peer, FrameCodec);
    assert!(peer.next().await.unwrap().is_ok()); // HELLO proves the dial is inside handshake.
    tokio::time::timeout(Duration::from_secs(1), transport.shutdown())
        .await
        .unwrap();
    assert!(connect.await.unwrap().is_err());
    assert!(
        peer.next().await.is_none(),
        "shutdown returned before client IO dropped"
    );
    assert!(transport
        .connect(&NodeAddr::tcp("silent", address))
        .await
        .is_err());
    assert!(transport
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .is_err());
    assert!(transport.accept().await.is_err());
}

#[tokio::test]
async fn cancelled_shutdown_can_be_joined_again_without_losing_tasks() {
    use futures::poll;
    let system = RemoteActorSystem::new(
        RemoteConfig::quic("cancel-close-quic", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    system.start().await.unwrap();
    let address = system.local_addr().unwrap();
    {
        let mut first = Box::pin(system.shutdown());
        // First poll fences and cancels synchronously. Drop even if drivers
        // have not been scheduled yet, then join using independent callers.
        let _ = poll!(first.as_mut());
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        let (a, b) = tokio::join!(system.shutdown(), system.shutdown());
        a.unwrap();
        b.unwrap();
    })
    .await
    .unwrap();
    let _rebound = tokio::net::UdpSocket::bind(address).await.unwrap();
}

#[tokio::test]
async fn closing_old_memory_endpoint_preserves_same_name_replacement() {
    let old =
        RemoteActorSystem::new(RemoteConfig::mem("replace-listener"), Arc::new(Empty)).unwrap();
    assert!(
        RemoteActorSystem::new(RemoteConfig::mem("replace-listener"), Arc::new(Empty)).is_err()
    );
    old.shutdown().await.unwrap();
    let new =
        RemoteActorSystem::new(RemoteConfig::mem("replace-listener"), Arc::new(Empty)).unwrap();
    let client =
        RemoteActorSystem::new(RemoteConfig::mem("replace-client"), Arc::new(Empty)).unwrap();
    new.start().await.unwrap();
    client.start().await.unwrap();
    old.shutdown().await.unwrap(); // Repeated cleanup cannot remove the new owner.
    client
        .connect(&NodeAddr::mem("replace-listener"))
        .await
        .unwrap();
    assert_eq!(client.links_snapshot().await.len(), 1);
    client.shutdown().await.unwrap();
    new.shutdown().await.unwrap();
}

#[tokio::test]
async fn tcp_active_driver_finishes_before_shutdown_with_handle_retained() {
    use parrot_remote::transport::Transport;
    use tokio::io::AsyncReadExt;
    let server = tcp_transport();
    let client = tcp_transport();
    server.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let address = server.local_addr().unwrap();
    let node = NodeAddr::tcp("tcp-peer", address);
    let (accepted, connected) = tokio::join!(server.accept(), client.connect(&node));
    let mut accepted = accepted.unwrap();
    let mut connected = connected.unwrap();
    let (a, b) = tokio::join!(server.shutdown(), client.shutdown());
    let _ = (a, b);
    assert!(accepted.sender.is_closed());
    assert!(connected.sender.is_closed());
    // No timeout or scheduler turn: the completion signal must already exist.
    assert!(accepted.closed.try_recv().is_ok());
    assert!(connected.closed.try_recv().is_ok());
    let rebound = tokio::net::TcpListener::bind(address).await.unwrap();
    drop(rebound);
    // Also exercise accept-side handshake cancellation with a silent raw peer.
    let server = Arc::new(tcp_transport());
    server.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let mut peer = tokio::net::TcpStream::connect(server.local_addr().unwrap())
        .await
        .unwrap();
    let accepting = server.clone();
    let task = tokio::spawn(async move { accepting.accept().await });
    server.shutdown().await;
    assert!(task.await.unwrap().is_err());
    // If not yet accepted, close may reset the raw socket; EOF and reset both
    // prove the listener/handshake no longer retains a live connection.
    match peer.read(&mut [0]).await {
        Ok(0) | Err(_) => {}
        other => panic!("unexpected read: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_fences_racing_memory_pair_registration() {
    for i in 0..16 {
        let a = RemoteActorSystem::new(RemoteConfig::mem(format!("race-a-{i}")), Arc::new(Empty))
            .unwrap();
        let b = RemoteActorSystem::new(RemoteConfig::mem(format!("race-b-{i}")), Arc::new(Empty))
            .unwrap();
        let connector = a.clone();
        let peer = b.clone();
        let task = tokio::spawn(async move { connector.connect_mem_pair(&peer).await });
        a.shutdown().await.unwrap();
        let _ = task.await.unwrap();
        // Whether connection or closure wins, no link may be published afterward.
        assert!(a.links_snapshot().await.is_empty());
        b.shutdown().await.unwrap();
    }
}
