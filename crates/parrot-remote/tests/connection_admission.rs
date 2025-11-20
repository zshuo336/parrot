use parrot_remote::{
    frame::Frame,
    handshake::HandshakeBody,
    security::RemoteSecurity,
    transport::{
        memory::MemoryTransport, quic::QuicTransport, tcp::TcpTransport, ConnectionHandle,
        FrameSender, RuntimeKnobs, Transport,
    },
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::AsyncReadExt,
    sync::{mpsc, watch},
};

type Inbound = (Frame, FrameSender, String);

struct Peer {
    transport: Arc<dyn Transport>,
    inbound: mpsc::Receiver<Inbound>,
    id: String,
}

impl Peer {
    async fn new(scheme: &str, id: &str, connections: usize, handshakes: usize) -> Self {
        let (inbound, receiver) = mpsc::channel(8);
        let (shutdown, _) = watch::channel(false);
        let handshake = HandshakeBody {
            node_id: id.into(),
            ..Default::default()
        };
        let knobs = Some(RuntimeKnobs {
            heartbeat_enabled: false,
            connection_capacity: connections,
            handshake_capacity: handshakes,
            ..Default::default()
        });
        let disconnect = Arc::new(|_: &FrameSender| {});
        let transport: Arc<dyn Transport> = match scheme {
            "mem" => Arc::new(
                MemoryTransport::new(handshake, inbound, disconnect, shutdown, knobs).unwrap(),
            ),
            "tcp" => Arc::new(TcpTransport::new(
                handshake, inbound, disconnect, shutdown, knobs,
            )),
            "quic" => Arc::new(
                QuicTransport::with_security(
                    handshake,
                    inbound,
                    disconnect,
                    shutdown,
                    "127.0.0.1:0".parse().unwrap(),
                    &RemoteSecurity::InsecureDevelopment,
                    knobs,
                )
                .unwrap(),
            ),
            _ => unreachable!(),
        };
        transport
            .listen("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        Self {
            transport,
            inbound: receiver,
            id: id.into(),
        }
    }

    fn address(&self) -> NodeAddr {
        match self.transport.scheme() {
            "mem" => NodeAddr::mem(&self.id),
            "tcp" => NodeAddr::tcp(&self.id, self.transport.local_addr().unwrap()),
            "quic" => NodeAddr::quic(&self.id, self.transport.local_addr().unwrap()),
            _ => unreachable!(),
        }
    }
}

async fn pair(client: &Peer, server: &Peer) -> (ConnectionHandle, ConnectionHandle) {
    let address = server.address();
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(
            client.transport.connect(&address),
            server.transport.accept()
        )
    })
    .await
    .expect("connection stalled")
    .unwrap()
}

async fn retire(outgoing: &mut ConnectionHandle, incoming: &mut ConnectionHandle) {
    outgoing.sender.close();
    tokio::time::timeout(Duration::from_secs(3), async {
        (&mut outgoing.closed).await.unwrap();
        (&mut incoming.closed).await.unwrap();
    })
    .await
    .expect("driver did not retire");
}

async fn established_capacity(scheme: &str) {
    let mut server = Peer::new(scheme, &format!("admit-{scheme}-server"), 1, 1).await;
    let first = Peer::new(scheme, &format!("admit-{scheme}-first"), 1, 1).await;
    let second = Peer::new(scheme, &format!("admit-{scheme}-second"), 1, 1).await;
    let (mut outgoing, mut incoming) = pair(&first, &server).await;

    let error = first
        .transport
        .connect(&server.address())
        .await
        .err()
        .expect("outbound limit bypassed");
    assert!(error.to_string().contains("connection capacity"), "{error}");
    let address = server.address();
    let (rejected, denied) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            server.transport.accept(),
            second.transport.connect(&address)
        )
    })
    .await
    .expect("saturated accept did not reject");
    let error = rejected.err().expect("inbound limit bypassed");
    assert!(error.to_string().contains("connection capacity"), "{error}");
    assert!(denied.is_err(), "rejected peer completed handshake");

    outgoing
        .sender
        .send(Frame::tell(
            "/probe",
            "bin:probe",
            bytes::Bytes::from_static(b"still-healthy"),
        ))
        .await
        .unwrap();
    let (frame, _, peer) = tokio::time::timeout(Duration::from_secs(2), server.inbound.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.payload.as_ref(), b"still-healthy");
    assert_eq!(peer, first.id);

    // The handles remain alive: only actual driver/IO retirement returns space.
    retire(&mut outgoing, &mut incoming).await;
    let (mut next_out, mut next_in) = pair(&second, &server).await;
    let probe = Frame::tell(
        "/probe",
        "bin:probe",
        bytes::Bytes::from_static(b"after-recovery"),
    );
    next_out.sender.send(probe).await.unwrap();
    let mut receiver = server.inbound;
    let (frame, _, peer) = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.payload.as_ref(), b"after-recovery");
    assert_eq!(peer, second.id);
    retire(&mut next_out, &mut next_in).await;
    first.transport.shutdown().await;
    second.transport.shutdown().await;
    server.transport.shutdown().await;
}

macro_rules! capacity_case {
    ($name:ident, $scheme:literal) => {
        #[tokio::test]
        async fn $name() {
            established_capacity($scheme).await;
        }
    };
}
capacity_case!(
    tcp_capacity_rejects_and_recovers_after_driver_retirement,
    "tcp"
);
capacity_case!(
    quic_capacity_rejects_and_recovers_after_driver_retirement,
    "quic"
);
capacity_case!(
    memory_capacity_rejects_and_recovers_after_driver_retirement,
    "mem"
);

#[tokio::test]
async fn completed_handshakes_release_slots_while_connections_remain_alive() {
    for scheme in ["mem", "tcp", "quic"] {
        let server = Peer::new(scheme, &format!("handshake-release-{scheme}"), 2, 1).await;
        let first = Peer::new(scheme, &format!("handshake-release-first-{scheme}"), 1, 1).await;
        let second = Peer::new(scheme, &format!("handshake-release-second-{scheme}"), 1, 1).await;
        let (mut a, mut b) = pair(&first, &server).await;
        let (mut c, mut d) = pair(&second, &server).await;
        assert!(!a.sender.is_closed() && !b.sender.is_closed());
        retire(&mut a, &mut b).await;
        retire(&mut c, &mut d).await;
        first.transport.shutdown().await;
        second.transport.shutdown().await;
        server.transport.shutdown().await;
    }
}

#[tokio::test]
async fn outbound_stalled_handshake_rejects_before_dial_and_cancellation_recovers() {
    let client = Peer::new("tcp", "outbound-handshake-budget", 2, 1).await;
    let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = NodeAddr::tcp("blackhole", blackhole.local_addr().unwrap());
    let owner = client.transport.clone();
    let attempt = tokio::spawn(async move { owner.connect(&address).await });
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), blackhole.accept())
        .await
        .unwrap()
        .unwrap();
    let mut byte = [0];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut byte))
        .await
        .unwrap()
        .unwrap();
    let error = client
        .transport
        .connect(&NodeAddr::tcp("absent", "127.0.0.1:0".parse().unwrap()))
        .await
        .err()
        .expect("handshake limit bypassed");
    assert!(error.to_string().contains("handshake capacity"), "{error}");
    attempt.abort();
    assert!(attempt
        .await
        .err()
        .expect("attempt was not cancelled")
        .is_cancelled());
    let mut remaining = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut remaining))
        .await
        .unwrap()
        .unwrap();
    let server = Peer::new("tcp", "outbound-handshake-recovery", 1, 1).await;
    let (mut outgoing, mut incoming) = pair(&client, &server).await;
    let another = Peer::new("tcp", "outbound-handshake-recovery-second", 1, 1).await;
    let (mut next_out, mut next_in) = pair(&client, &another).await;
    retire(&mut outgoing, &mut incoming).await;
    retire(&mut next_out, &mut next_in).await;
    client.transport.shutdown().await;
    server.transport.shutdown().await;
    another.transport.shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_stalled_dial_and_closes_its_socket() {
    let client = Peer::new("tcp", "shutdown-handshake-budget", 1, 1).await;
    let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = NodeAddr::tcp("blackhole", blackhole.local_addr().unwrap());
    let owner = client.transport.clone();
    let attempt = tokio::spawn(async move { owner.connect(&address).await });
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), blackhole.accept())
        .await
        .unwrap()
        .unwrap();
    let mut byte = [0];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut byte))
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), client.transport.shutdown())
        .await
        .expect("shutdown stalled");
    assert!(attempt.await.unwrap().is_err());
    let mut remaining = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut remaining))
        .await
        .unwrap()
        .unwrap();
    let error = client
        .transport
        .connect(&NodeAddr::tcp("absent", "127.0.0.1:0".parse().unwrap()))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("shut down"), "{error}");
}

struct EmptyLookup;
#[async_trait::async_trait]
impl LocalLookup for EmptyLookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}

#[tokio::test]
async fn one_silent_inbound_peer_does_not_block_another_system_handshake() {
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp(
            "parallel-accept-server",
            Some("127.0.0.1:0".parse().unwrap()),
        )
        .with_knobs(RuntimeKnobs {
            connection_capacity: 2,
            handshake_capacity: 2,
            ..Default::default()
        }),
        Arc::new(EmptyLookup),
    )
    .unwrap();
    server.start().await.unwrap();
    let address = server.local_addr().unwrap();
    let silent = tokio::net::TcpStream::connect(address).await.unwrap();
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("parallel-accept-client", None),
        Arc::new(EmptyLookup),
    )
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        client.connect(&NodeAddr::tcp("parallel-accept-server", address)),
    )
    .await;
    client.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), server.shutdown())
        .await
        .expect("stalled inbound peer delayed shutdown")
        .unwrap();
    drop(silent);
    result
        .expect("silent peer blocked the next handshake")
        .unwrap();
}

#[tokio::test]
async fn direct_memory_pair_shares_budget_with_transport_connections() {
    let config = |id| {
        RemoteConfig::mem(id).with_knobs(RuntimeKnobs {
            connection_capacity: 1,
            handshake_capacity: 1,
            ..Default::default()
        })
    };
    let a = RemoteActorSystem::new(config("shared-budget-a"), Arc::new(EmptyLookup)).unwrap();
    let b = RemoteActorSystem::new(config("shared-budget-b"), Arc::new(EmptyLookup)).unwrap();
    let c = RemoteActorSystem::new(config("shared-budget-c"), Arc::new(EmptyLookup)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    c.start().await.unwrap();
    a.connect(&NodeAddr::mem("shared-budget-b")).await.unwrap();
    let error = a.connect_mem_pair(&c).await.unwrap_err();
    assert!(error.to_string().contains("connection capacity"), "{error}");
    // An unsuccessful admission on A must not consume C's capacity.
    let d = RemoteActorSystem::new(config("shared-budget-d"), Arc::new(EmptyLookup)).unwrap();
    c.connect_mem_pair(&d).await.unwrap();
    let error = c
        .connect(&NodeAddr::mem("shared-budget-a"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("connection capacity"), "{error}");
    let e = RemoteActorSystem::new(config("shared-budget-e"), Arc::new(EmptyLookup)).unwrap();
    let f = RemoteActorSystem::new(config("shared-budget-f"), Arc::new(EmptyLookup)).unwrap();
    let error = e.connect_mem_pair(&b).await.unwrap_err();
    assert!(error.to_string().contains("connection capacity"), "{error}");
    e.connect_mem_pair(&f)
        .await
        .expect("peer rejection leaked caller capacity");
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    c.shutdown().await.unwrap();
    d.shutdown().await.unwrap();
    e.shutdown().await.unwrap();
    f.shutdown().await.unwrap();
}

#[test]
fn config_layers_validate_and_preserve_admission_limits() {
    use parrot_config::ParrotConfig;
    let load = |source: &str| {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), source).unwrap();
        ParrotConfig::builder().load_file(file.path())
    };
    let fallback =
        load("[remote.transport]\nconnection_capacity = 7\nhandshake_capacity = 3").unwrap();
    let primary = load("[remote.transport]\nconnection_capacity = 9").unwrap();
    let resolved = primary.with_fallback(fallback).build().unwrap();
    assert_eq!(resolved.remote.transport.connection_capacity, 9);
    assert_eq!(resolved.remote.transport.handshake_capacity, 3);
    let runtime = RemoteConfig::from_resolved(&resolved)
        .unwrap()
        .knobs
        .unwrap();
    assert_eq!(runtime.connection_capacity, 9);
    assert_eq!(runtime.handshake_capacity, 3);
    let defaults = ParrotConfig::builder().build().unwrap();
    assert_eq!(defaults.remote.transport.connection_capacity, 1024);
    assert_eq!(defaults.remote.transport.handshake_capacity, 64);
    let max = tokio::sync::Semaphore::MAX_PERMITS.min(u32::MAX as usize);
    for key in ["connection_capacity", "handshake_capacity"] {
        for value in [0, max + 1, usize::MAX] {
            let cfg = load(&format!("[remote.transport]\n{key} = {value}"));
            assert!(cfg.and_then(|cfg| cfg.build()).is_err());
        }
        assert!(load(&format!("[remote.transport]\n{key} = {max}"))
            .unwrap()
            .build()
            .is_ok());
    }
}
