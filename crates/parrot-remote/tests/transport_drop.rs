//! Transport ownership must not depend on dropping external signals or senders.
use parrot_remote::{
    frame::Frame,
    handshake::HandshakeBody,
    security::RemoteSecurity,
    transport::{
        memory::MemoryTransport, quic::QuicTransport, tcp::TcpTransport, FrameSender, RuntimeKnobs,
        Transport,
    },
    NodeAddr,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};

type Inbound = (Frame, FrameSender, String);

fn transport(
    scheme: &str,
    id: &str,
    inbound: mpsc::Sender<Inbound>,
    shutdown: watch::Sender<bool>,
    closed: Arc<AtomicUsize>,
) -> Box<dyn Transport> {
    let handshake = HandshakeBody {
        node_id: id.into(),
        ..Default::default()
    };
    let disconnect = Arc::new(move |sender: &FrameSender| {
        assert!(sender.is_closed());
        closed.fetch_add(1, Ordering::SeqCst);
    });
    // A timer-driven loss detector must not accidentally satisfy the test.
    let knobs = Some(RuntimeKnobs {
        heartbeat_enabled: false,
        ..Default::default()
    });
    match scheme {
        "mem" => {
            Box::new(MemoryTransport::new(handshake, inbound, disconnect, shutdown, knobs).unwrap())
        }
        "tcp" => Box::new(TcpTransport::new(
            handshake, inbound, disconnect, shutdown, knobs,
        )),
        "quic" => Box::new(
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
    }
}

async fn dropped_owner(scheme: &str, drop_server: bool) {
    let id = format!("drop-{scheme}-{drop_server}");
    let client_id = format!("{id}-client");
    let server_id = format!("{id}-server");
    let (client_tx, _client_rx) = mpsc::channel(8);
    let (server_tx, mut server_rx) = mpsc::channel(8);
    let (client_signal, _) = watch::channel(false);
    let (server_signal, _) = watch::channel(false);
    let client_closes = Arc::new(AtomicUsize::new(0));
    let server_closes = Arc::new(AtomicUsize::new(0));
    let mut client = Some(transport(
        scheme,
        &client_id,
        client_tx,
        client_signal.clone(),
        client_closes.clone(),
    ));
    let mut server = Some(transport(
        scheme,
        &server_id,
        server_tx,
        server_signal.clone(),
        server_closes.clone(),
    ));
    server
        .as_ref()
        .unwrap()
        .listen("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let server_address = server.as_ref().unwrap().local_addr();
    let client_address = client.as_ref().unwrap().local_addr();
    let target = match scheme {
        "mem" => NodeAddr::mem(&server_id),
        "tcp" => NodeAddr::tcp(&server_id, server_address.unwrap()),
        "quic" => NodeAddr::quic(&server_id, server_address.unwrap()),
        _ => unreachable!(),
    };
    let (mut incoming, mut outgoing) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(
            server.as_ref().unwrap().accept(),
            client.as_ref().unwrap().connect(&target)
        )
    })
    .await
    .unwrap()
    .unwrap();
    let probe = Frame::ask(
        1,
        "/probe",
        "probe",
        bytes::Bytes::from_static(b"alive"),
        None,
    );
    let expected_payload = probe.payload.clone(); // Includes ASK's budget prefix.
    outgoing.sender.send(probe).await.unwrap();
    let (frame, _, _) = tokio::time::timeout(Duration::from_secs(2), server_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.payload, expected_payload);
    assert_eq!(client_closes.load(Ordering::SeqCst), 0);
    assert_eq!(server_closes.load(Ordering::SeqCst), 0);

    // Only the owner disappears: both handles, peer and external signals survive.
    if drop_server {
        drop(server.take());
    } else {
        drop(client.take());
    }
    let ended = tokio::time::timeout(Duration::from_secs(2), async {
        (&mut incoming.closed).await.unwrap();
        (&mut outgoing.closed).await.unwrap();
    })
    .await
    .is_ok();
    let both_closed = incoming.sender.is_closed() && outgoing.sender.is_closed();
    let counts = (
        client_closes.load(Ordering::SeqCst),
        server_closes.load(Ordering::SeqCst),
    );
    // Even a failing old implementation must not leave a detached connection.
    incoming.sender.close();
    outgoing.sender.close();
    if let Some(owner) = client {
        owner.shutdown().await;
    }
    if let Some(owner) = server {
        owner.shutdown().await;
    }
    assert!(
        ended && both_closed,
        "{scheme} drop_server={drop_server}: drivers outlived their transport owner"
    );
    assert_eq!(counts, (1, 1), "each driver must notify exactly once");
    assert!(
        !*client_signal.borrow() && !*server_signal.borrow(),
        "Drop must not mutate caller-owned shutdown signals"
    );
    // Handles still exist; UDP runtime tasks must nevertheless release sockets.
    if scheme == "quic" {
        let mut addresses = vec![
            server_address.unwrap(),
            client_address.unwrap(),
            outgoing.info.local.unwrap(),
        ];
        addresses.sort_unstable();
        addresses.dedup();
        for addr in addresses {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Ok(socket) = tokio::net::UdpSocket::bind(addr).await {
                        break socket;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("QUIC runtime retained a UDP socket after owner destruction");
        }
    } else if scheme == "tcp" && drop_server {
        tokio::net::TcpListener::bind(server_address.unwrap())
            .await
            .expect("dropped TCP listener remained bound");
    }
}

macro_rules! drop_case {
    ($name:ident, $scheme:literal, $server:literal) => {
        #[tokio::test]
        async fn $name() {
            dropped_owner($scheme, $server).await;
        }
    };
}
drop_case!(memory_server_drop_cancels_retained_connection, "mem", true);
drop_case!(memory_client_drop_cancels_retained_connection, "mem", false);
drop_case!(tcp_server_drop_cancels_retained_connection, "tcp", true);
drop_case!(tcp_client_drop_cancels_retained_connection, "tcp", false);
drop_case!(quic_server_drop_cancels_retained_connection, "quic", true);
drop_case!(quic_client_drop_cancels_retained_connection, "quic", false);

#[test]
fn direct_transports_can_be_destroyed_after_their_runtime() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let owners = runtime.block_on(async {
        let mut owners = Vec::new();
        for scheme in ["mem", "tcp", "quic"] {
            let (inbound, _) = mpsc::channel(1);
            let (shutdown, _) = watch::channel(false);
            let owner = transport(
                scheme,
                &format!("drop-after-runtime-{scheme}"),
                inbound,
                shutdown,
                Arc::new(AtomicUsize::new(0)),
            );
            owner.listen("127.0.0.1:0".parse().unwrap()).await.unwrap();
            owners.push((scheme, owner.local_addr(), owner));
        }
        owners
    });
    drop(runtime);
    for (scheme, address, owner) in owners {
        drop(owner); // Must not spawn cleanup or require a current reactor.
        match scheme {
            "tcp" => {
                std::net::TcpListener::bind(address.unwrap()).unwrap();
            }
            "quic" => {
                std::net::UdpSocket::bind(address.unwrap()).unwrap();
            }
            "mem" => {
                let (inbound, _) = mpsc::channel(1);
                let (shutdown, _) = watch::channel(false);
                drop(transport(
                    "mem",
                    "drop-after-runtime-mem",
                    inbound,
                    shutdown,
                    Arc::new(AtomicUsize::new(0)),
                ));
            }
            _ => unreachable!(),
        }
    }
}
