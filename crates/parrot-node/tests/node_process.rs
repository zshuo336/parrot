//! Real executable contracts with owned children and bounded startup output.
use std::{
    path::PathBuf,
    process::Stdio,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, Command},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static SERIAL: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "parrot-node-process-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_parrot-node"));
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("PARROT_"))
        {
            cmd.env_remove(key);
        }
        cmd.current_dir(&self.0)
            .env("PARROT_BIND", "127.0.0.1:0")
            .env("PARROT_ACTORS", "")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        cmd
    }
    fn config(&self, text: &str) -> PathBuf {
        let path = self.0.join("node.toml");
        std::fs::write(&path, text).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn exited_or_ready(child: &mut Child) -> Option<String> {
    let stdout = child.stdout.take().unwrap();
    let read = async {
        let mut reader = BufReader::new(stdout.take(65537));
        let mut bytes = 0;
        loop {
            let mut line = String::new();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| e.to_string())?;
            bytes += n;
            if bytes > 65536 {
                return Err("startup output limit".to_string());
            }
            if n == 0 {
                return Ok(None);
            }
            if let Some(endpoint) = line.strip_prefix("PARROT_NODE_READY=") {
                return Ok(Some(endpoint.trim().to_string()));
            }
        }
    };
    match tokio::time::timeout(Duration::from_secs(180), read).await {
        Ok(Ok(endpoint)) => endpoint,
        error => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            panic!("node startup failed: {error:?}");
        }
    }
}
async fn finish(child: &mut Child) -> std::process::ExitStatus {
    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(result) => result.unwrap(),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            panic!("child exit deadline");
        }
    }
}
async fn rejects(mut cmd: Command) {
    let mut child = cmd.spawn().unwrap();
    let ready = exited_or_ready(&mut child).await;
    if ready.is_some() {
        child.kill().await.unwrap();
    }
    let status = finish(&mut child).await;
    assert!(ready.is_none(), "invalid configuration announced READY");
    assert!(
        !status.success(),
        "invalid configuration exited successfully"
    );
}
async fn probe(mut cmd: Command) -> bool {
    let mut child = cmd.env("PARROT_HEALTHCHECK", "1").spawn().unwrap();
    match tokio::time::timeout(Duration::from_secs(180), child.wait()).await {
        Ok(status) => status.unwrap().success(),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            panic!("healthcheck process deadline");
        }
    }
}
#[tokio::test]
async fn explicit_missing_config_is_fatal() {
    let f = Fixture::new();
    let mut cmd = f.command();
    cmd.env("PARROT_CONFIG", f.0.join("missing.toml"));
    rejects(cmd).await;
}
#[tokio::test]
async fn malformed_seed_is_fatal() {
    let f = Fixture::new();
    let mut cmd = f.command();
    cmd.env("PARROT_SEEDS", "not-an-endpoint");
    rejects(cmd).await;
}
#[tokio::test]
async fn tcp_backlog_is_not_a_healthy_node() {
    let f = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cmd = f.command();
    cmd.env("PARROT_BIND", listener.local_addr().unwrap().to_string());
    assert!(
        !probe(cmd).await,
        "bare TCP backlog passed protocol healthcheck"
    );
}
#[tokio::test]
async fn invalid_files_env_and_readiness_paths_fail_without_ready() {
    let f = Fixture::new();
    for file in [
        "[thread]\nshared_pool_size = 0",
        "[thread]\nshared_queue_capacity = 0",
        "[remote.node]\nunknown = true",
        "this isn't TOML",
    ] {
        let path = f.config(file);
        let mut cmd = f.command();
        cmd.env("PARROT_CONFIG", path);
        rejects(cmd).await;
    }
    for (key, value) in [
        ("PARROT_ACTORS", "echo,typo"),
        ("PARROT_ACTORS", "echo,"),
        ("PARROT_NODE_ID", ""),
        ("PARROT_BIND", ""),
        ("PARROT_SEEDS", "peer=127.0.0.1:1234,"),
        ("PARROT_SCHEME", "unknown"),
        ("PARROT_CONFIG", ""),
        ("PARROT_READY_FILE", ""),
    ] {
        let mut cmd = f.command();
        cmd.env(key, value);
        rejects(cmd).await;
    }
    let mut cmd = f.command();
    cmd.env("PARROT_READY_FILE", f.0.join("missing/ready"));
    rejects(cmd).await;
}
async fn handshake(endpoint: &str) -> parrot_remote::handshake::HandshakeAckBody {
    use futures::{SinkExt, StreamExt};
    use parrot_remote::{
        frame::Frame,
        handshake::{HandshakeAckBody, HandshakeBody},
        transport::FrameCodec,
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        let socket = tokio::net::TcpStream::connect(endpoint).await.unwrap();
        let mut peer = tokio_util::codec::Framed::new(socket, FrameCodec);
        let mut body = bytes::BytesMut::new();
        HandshakeBody {
            node_id: "wire-probe".into(),
            ..Default::default()
        }
        .encode_tlv(&mut body)
        .unwrap();
        peer.send(Frame::handshake(body.freeze())).await.unwrap();
        let ack = peer.next().await.unwrap().unwrap();
        assert_eq!(
            ack.header.frame_type,
            parrot_remote::frame::frame_type::HANDSHAKE_ACK
        );
        HandshakeAckBody::decode_tlv(&ack.payload).unwrap()
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn file_configuration_reaches_wire_healthcheck_and_env_override() {
    let f = Fixture::new();
    let path = f.config("[remote.node]\nnode_id = 'file-node'\nbind = '127.0.0.1:0'\n[remote.transport]\ndefault_hop_limit = 19");
    let ready = f.0.join("ready");
    for (override_id, expected) in [(None, "file-node"), (Some("env-node"), "env-node")] {
        let mut cmd = f.command();
        cmd.env_remove("PARROT_BIND")
            .env("PARROT_CONFIG", &path)
            .env("PARROT_READY_FILE", &ready);
        if let Some(id) = override_id {
            cmd.env("PARROT_NODE_ID", id);
        }
        let mut child = cmd.spawn().unwrap();
        let endpoint = exited_or_ready(&mut child)
            .await
            .expect("valid node must start");
        let ack = handshake(&endpoint).await;
        assert_eq!(ack.node_id, expected);
        assert_eq!(ack.hop_limit, 19);
        assert_eq!(std::fs::read_to_string(&ready).unwrap(), endpoint);
        let mut cmd = f.command();
        cmd.env_remove("PARROT_BIND")
            .env("PARROT_CONFIG", &path)
            .env("PARROT_NODE_ID", expected)
            .env("PARROT_READY_FILE", &ready);
        assert!(probe(cmd).await);
        let mut cmd = f.command();
        cmd.env("PARROT_BIND", &endpoint)
            .env("PARROT_NODE_ID", "wrong-node");
        assert!(!probe(cmd).await, "healthcheck must validate peer identity");
        #[cfg(unix)]
        {
            let status = Command::new("sh")
                .args([
                    "-c",
                    "kill -TERM \"$1\"",
                    "parrot-node-test",
                    &child.id().unwrap().to_string(),
                ])
                .status()
                .await
                .unwrap();
            assert!(status.success());
            assert!(
                finish(&mut child).await.success(),
                "SIGTERM must trigger graceful exit"
            );
        }
        #[cfg(not(unix))]
        {
            child.kill().await.unwrap();
            finish(&mut child).await;
        }
        assert!(tokio::net::TcpStream::connect(&endpoint).await.is_err());
    }
}

struct EmptyLookup;
#[async_trait::async_trait]
impl parrot_remote::LocalLookup for EmptyLookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
#[tokio::test]
async fn quic_node_executes_real_actor_calls_and_protocol_probe() {
    use parrot_api::address::ActorRef;
    use parrot_remote::{NodeAddr, RemoteActorSystem, RemoteConfig};
    use std::sync::Arc;
    let f = Fixture::new();
    let mut child = f
        .command()
        .env("PARROT_SCHEME", "quic")
        .env("PARROT_SECURITY_MODE", "insecure-development")
        .env("PARROT_ACTORS", "echo")
        .spawn()
        .unwrap();
    let endpoint = exited_or_ready(&mut child).await.unwrap();
    let client = RemoteActorSystem::new(
        RemoteConfig::quic("quic-probe", Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(EmptyLookup),
    )
    .unwrap();
    client.start().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        client.connect(&NodeAddr::quic("node-1", endpoint.parse().unwrap())),
    )
    .await
    .unwrap()
    .unwrap();
    let actor = client.remote_ref("parrot://node-1/user/echo").unwrap();
    let reply = actor
        .send_with_timeout(
            Box::new(parrot_node::NEcho(42)),
            Some(Duration::from_secs(3)),
        )
        .await
        .unwrap();
    assert_eq!(reply.downcast_ref::<parrot_node::NEchoed>().unwrap().0, 42);
    let mut cmd = f.command();
    cmd.env("PARROT_SCHEME", "quic")
        .env("PARROT_SECURITY_MODE", "insecure-development")
        .env("PARROT_BIND", &endpoint);
    assert!(probe(cmd).await);
    client.shutdown().await.unwrap();
    child.kill().await.unwrap();
    finish(&mut child).await;
}
#[tokio::test]
async fn executable_bootstraps_late_dns_seed() {
    use parrot_remote::{RemoteActorSystem, RemoteConfig};
    use std::sync::Arc;
    let f = Fixture::new();
    // Reserve the port without listening. Dropping a listener to discover a
    // "free" port allowed the client's own outbound socket to steal it.
    let reserve = tokio::net::TcpSocket::new_v4().unwrap();
    reserve.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let address = reserve.local_addr().unwrap();
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    let mut child = f
        .command()
        .env("PARROT_NODE_ID", "late-client")
        .env(
            "PARROT_SEEDS",
            format!("late-server=localhost:{}", address.port()),
        )
        .spawn()
        .unwrap();
    assert!(exited_or_ready(&mut child).await.is_some());
    // The seed is absent during startup, then appears without restarting the node.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("late-server", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(EmptyLookup),
    )
    .unwrap();
    server.start().await.unwrap();
    // Start listening on the same owned socket, then transparently forward to
    // the real RemoteActorSystem. There is no release/rebind race at the seed.
    let listener = reserve.listen(8).unwrap();
    let server_address = server.local_addr().unwrap();
    let proxy = tokio::spawn(async move {
        let (mut incoming, _) = listener.accept().await.unwrap();
        let mut outgoing = tokio::net::TcpStream::connect(server_address)
            .await
            .unwrap();
        let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
    });
    let connected = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if server
                .links_snapshot()
                .await
                .iter()
                .any(|(id, _, _)| id == "late-client")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    child.kill().await.unwrap();
    finish(&mut child).await;
    proxy.abort();
    let _ = proxy.await;
    server.shutdown().await.unwrap();
    connected.expect("seed retry must run in real executable");
}

#[tokio::test]
async fn optional_default_file_and_explicit_empty_seeds_are_honored() {
    let f = Fixture::new();
    std::fs::write(f.0.join("parrot.toml"), "[remote.node]\nnode_id = 'default-file'\nbind = '127.0.0.1:0'\nseeds = ['tcp://bad@does-not-exist.invalid:9801']").unwrap();
    let mut child = f
        .command()
        .env_remove("PARROT_BIND")
        .env("PARROT_SEEDS", "")
        .spawn()
        .unwrap();
    let endpoint = exited_or_ready(&mut child).await.unwrap();
    let ack = handshake(&endpoint).await;
    child.kill().await.unwrap();
    finish(&mut child).await;
    assert_eq!(ack.node_id, "default-file");
}
#[tokio::test]
async fn listen_failure_does_not_publish_readiness() {
    let f = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ready = f.0.join("ready");
    let mut cmd = f.command();
    cmd.env("PARROT_BIND", listener.local_addr().unwrap().to_string())
        .env("PARROT_ACTORS", "echo,counter")
        .env("PARROT_READY_FILE", &ready);
    rejects(cmd).await;
    assert!(!ready.exists());
}
#[tokio::test]
async fn memory_transport_is_explicit_and_cross_process_probe_rejects_it() {
    let f = Fixture::new();
    let mut child = f.command().env("PARROT_SCHEME", "mem").spawn().unwrap();
    let endpoint = exited_or_ready(&mut child).await;
    child.kill().await.unwrap();
    finish(&mut child).await;
    assert_eq!(endpoint.as_deref(), Some("mem://node-1"));
    let mut cmd = f.command();
    cmd.env("PARROT_SCHEME", "mem");
    assert!(!probe(cmd).await);
}
#[tokio::test]
async fn ipv6_unspecified_bind_probes_ipv6_loopback() {
    let f = Fixture::new();
    let ready = f.0.join("ready");
    let mut child = f
        .command()
        .env("PARROT_BIND", "[::]:0")
        .env("PARROT_SECURITY_MODE", "insecure-development")
        .env("PARROT_READY_FILE", &ready)
        .spawn()
        .unwrap();
    let endpoint = exited_or_ready(&mut child).await.unwrap();
    let mut cmd = f.command();
    cmd.env("PARROT_BIND", &endpoint);
    let healthy = probe(cmd).await;
    child.kill().await.unwrap();
    finish(&mut child).await;
    assert!(healthy, "IPv6 wildcard must probe ::1");
}

#[test]
fn node_defaults_and_overrides_are_strict_without_global_env_mutation() {
    use parrot_node::configuration::{seed_strings, NodeSettings};
    let f = Fixture::new();
    let path = f.config("");
    let settings = NodeSettings::from_environment(|key| {
        (key == "PARROT_CONFIG").then(|| path.clone().into_os_string())
    })
    .unwrap();
    assert_eq!(
        settings.resolved.remote.node.bind.as_deref(),
        Some("127.0.0.1:9801")
    );
    assert_eq!(
        settings.resolved.remote.node.node_id.as_deref(),
        Some("node-1")
    );
    assert_eq!(settings.actors, ["echo", "counter", "kv", "slow"]);
    assert_eq!(
        seed_strings("alpha=[::1]:9801, tcp://beta@localhost:9802", "tcp").unwrap(),
        ["tcp://alpha@[::1]:9801", "tcp://beta@localhost:9802"]
    );
    assert_eq!(seed_strings("mem://alpha", "mem").unwrap(), ["mem://alpha"]);
    for seed in [
        "alpha=127.0.0.1:0",
        "alpha=localhost",
        "=127.0.0.1:9801",
        ",alpha=localhost:9801",
        "quic://alpha@127.0.0.1:9801",
    ] {
        assert!(seed_strings(seed, "tcp").is_err(), "accepted {seed}");
    }
}
#[cfg(unix)]
#[tokio::test]
async fn non_unicode_environment_is_rejected() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let f = Fixture::new();
    let mut cmd = f.command();
    cmd.env("PARROT_NODE_ID", OsString::from_vec(vec![0xff]));
    rejects(cmd).await;
}

// macOS filesystems reject ill-formed UTF-8 filenames (EILSEQ); Linux permits
// these byte paths, so the filesystem contract belongs to its Linux CI target.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn non_unicode_file_paths_work() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let f = Fixture::new();
    let path = f.0.join(OsString::from_vec(vec![b'c', 0xff]));
    std::fs::write(&path, "[remote.node]\nnode_id = 'nonunicode-path'").unwrap();
    let mut child = f.command().env("PARROT_CONFIG", &path).spawn().unwrap();
    let endpoint = exited_or_ready(&mut child).await.unwrap();
    let ack = handshake(&endpoint).await;
    child.kill().await.unwrap();
    finish(&mut child).await;
    assert_eq!(ack.node_id, "nonunicode-path");
}

#[tokio::test]
async fn periodic_probe_processes_reuse_identity_and_retire_connection_routes() {
    use parrot_remote::{RemoteActorSystem, RemoteConfig};
    use std::sync::Arc;
    let f = Fixture::new();
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("probe-target", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(EmptyLookup),
    )
    .unwrap();
    server.start().await.unwrap();
    let address = server.local_addr().unwrap();
    // Observe the real HELLO before forwarding it to the real server. Checking
    // a retired route cannot establish which identity a short-lived probe used.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut identities = Vec::new();
    for _ in 0..3 {
        let mut cmd = f.command();
        cmd.env("PARROT_NODE_ID", "probe-target")
            .env("PARROT_BIND", listener.local_addr().unwrap().to_string());
        let observe = tokio::time::timeout(Duration::from_secs(5), async {
            let (socket, _) = listener.accept().await.unwrap();
            observe_probe(socket, address).await
        });
        let (healthy, identity) = tokio::join!(probe(cmd), observe);
        assert!(healthy);
        let (identity, relayed) = identity.expect("probe wire observation deadline");
        if let Err(error) = relayed {
            assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::NotConnected
                ),
                "unexpected probe relay failure: {error}"
            );
        }
        identities.push(identity);
        // Routes owned only by a connection must retire after it closes. A
        // permanent ghost entry is a leak, not evidence of identity reuse.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !server.nodes.is_empty() || !server.links_snapshot().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("probe connection and route must retire");
    }
    server.shutdown().await.unwrap();
    assert!(!identities[0].is_empty());
    assert!(
        identities.iter().all(|id| id == &identities[0]),
        "periodic process probes must reuse their actual wire identity: {identities:?}"
    );
}

/// Inspect HELLO, then relay bytes until close. A successful short-lived probe
/// may reset its socket with unread gossip; that is not malformed framing.
async fn observe_probe(
    socket: tokio::net::TcpStream,
    address: std::net::SocketAddr,
) -> (String, std::io::Result<(u64, u64)>) {
    observe_buffered_probe(
        tokio_util::codec::Framed::new(socket, parrot_remote::transport::FrameCodec),
        address,
    )
    .await
}

async fn observe_buffered_probe(
    mut peer: tokio_util::codec::Framed<
        tokio::net::TcpStream,
        parrot_remote::transport::FrameCodec,
    >,
    address: std::net::SocketAddr,
) -> (String, std::io::Result<(u64, u64)>) {
    use futures::StreamExt;
    use parrot_remote::handshake::HandshakeBody;
    use tokio::io::AsyncWriteExt;
    let hello = peer.next().await.unwrap().unwrap();
    assert_eq!(
        hello.header.frame_type,
        parrot_remote::frame::frame_type::HANDSHAKE
    );
    let identity = HandshakeBody::decode_tlv(&hello.payload).unwrap().node_id;
    let mut upstream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut encoded = bytes::BytesMut::new();
    hello.encode(&mut encoded).unwrap();
    upstream.write_all(&encoded).await.unwrap();
    // Preserve any bytes the framed reader already read past HELLO.
    let mut parts = peer.into_parts();
    upstream.write_all(&parts.read_buf).await.unwrap();
    let result = tokio::io::copy_bidirectional(&mut parts.io, &mut upstream).await;
    (identity, result)
}

#[cfg(unix)]
#[tokio::test]
async fn probe_observer_preserves_pipelined_bytes_and_identifies_reset() {
    use futures::{SinkExt, StreamExt};
    use parrot_remote::{frame::Frame, handshake::HandshakeBody, transport::FrameCodec};
    use std::os::fd::AsRawFd;
    use tokio::io::AsyncWriteExt;
    tokio::time::timeout(Duration::from_secs(5), async {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut body = bytes::BytesMut::new();
        HandshakeBody {
            node_id: "reset-probe".into(),
            ..Default::default()
        }
        .encode_tlv(&mut body)
        .unwrap();
        let mut pipeline = bytes::BytesMut::new();
        Frame::handshake(body.freeze())
            .encode(&mut pipeline)
            .unwrap();
        Frame::heartbeat().encode(&mut pipeline).unwrap();
        let forward = async {
            let (mut socket, _) = proxy.accept().await.unwrap();
            // Force the already-read case independently of OS packet segmentation.
            let mut buffered = vec![0; pipeline.len()];
            socket.read_exact(&mut buffered).await.unwrap();
            let mut parts = tokio_util::codec::Framed::new(socket, FrameCodec).into_parts();
            parts.read_buf = bytes::BytesMut::from(buffered.as_slice());
            let (identity, result) = observe_buffered_probe(
                tokio_util::codec::Framed::from_parts(parts),
                server.local_addr().unwrap(),
            )
            .await;
            assert_eq!(identity, "reset-probe");
            assert_eq!(
                result.unwrap_err().kind(),
                std::io::ErrorKind::ConnectionReset
            );
        };
        let upstream = async {
            let (socket, _) = server.accept().await.unwrap();
            let mut framed = tokio_util::codec::Framed::new(socket, FrameCodec);
            assert_eq!(
                framed.next().await.unwrap().unwrap().header.frame_type,
                parrot_remote::frame::frame_type::HANDSHAKE
            );
            assert_eq!(
                framed.next().await.unwrap().unwrap().header.frame_type,
                parrot_remote::frame::frame_type::HEARTBEAT
            );
            framed.send(Frame::heartbeat_ack()).await.unwrap();
            // Keep this half open until the proxy observes the downstream RST.
            assert!(framed.next().await.is_none());
        };
        let downstream = async {
            let mut socket = tokio::net::TcpStream::connect(proxy.local_addr().unwrap())
                .await
                .unwrap();
            socket.write_all(&pipeline).await.unwrap();
            let mut framed = tokio_util::codec::Framed::new(socket, FrameCodec);
            assert_eq!(
                framed.next().await.unwrap().unwrap().header.frame_type,
                parrot_remote::frame::frame_type::HEARTBEAT_ACK
            );
            let linger = libc::linger {
                l_onoff: 1,
                l_linger: 0,
            };
            // The connected socket and correctly sized linger value outlive the call.
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        framed.get_ref().as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_LINGER,
                        (&linger as *const libc::linger).cast(),
                        std::mem::size_of_val(&linger) as libc::socklen_t,
                    )
                },
                0
            );
            drop(framed);
        };
        tokio::join!(forward, upstream, downstream);
    })
    .await
    .expect("probe relay reset fixture stalled");
}

/// Real child process, Thread actors, TLS transport and the packaged probe CLI.
/// This complements remote's executor spies with executable-level side effects.
#[tokio::test]
async fn mtls_node_health_and_admin_probe_use_real_verified_connections() {
    use parrot_remote::{NodeAddr, RemoteActorSystem, RemoteConfig};
    use std::sync::Arc;
    let f = Fixture::new();
    let mut ca_params = rcgen::CertificateParams::default();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca = ca_params.self_signed(&ca_key).unwrap().pem();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);
    std::fs::write(f.0.join("ca.pem"), &ca).unwrap();
    let credentials = |name: &str| {
        let mut params = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &issuer).unwrap();
        let cert_path = f.0.join(format!("{name}.crt"));
        let key_path = f.0.join(format!("{name}.key"));
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key.serialize_pem()).unwrap();
        parrot_remote::tls::TlsConfig {
            cert_pem: cert.pem().into_bytes(),
            key_pem: key.serialize_pem().into_bytes(),
            ca_pem: ca.as_bytes().to_vec(),
        }
    };
    let _server = credentials("server.test");
    let operator = credentials("operator.test");
    let _ordinary = credentials("ordinary.test");
    let config_file = |name: &str, admins: &[&str]| {
        let path = f.0.join(format!("{name}.toml"));
        let text = format!("[remote.node]\nnode_id={name:?}\n[remote.security]\nmode='mtls'\ncert_path={:?}\nkey_path={:?}\nca_path={:?}\nadmin_nodes={admins:?}\n",
            f.0.join(format!("{name}.crt")).to_str().unwrap(), f.0.join(format!("{name}.key")).to_str().unwrap(), f.0.join("ca.pem").to_str().unwrap());
        std::fs::write(&path, text).unwrap();
        path
    };
    let server_config = config_file("server.test", &["operator.test"]);
    let operator_config = config_file("operator.test", &[]);
    let ordinary_config = config_file("ordinary.test", &[]);
    let mut child = f
        .command()
        .env("PARROT_CONFIG", &server_config)
        .env("PARROT_ACTORS", "echo,counter,kv,slow")
        .spawn()
        .unwrap();
    let endpoint = exited_or_ready(&mut child).await.unwrap();
    let mut health = f.command();
    health
        .env("PARROT_CONFIG", &server_config)
        .env("PARROT_BIND", &endpoint);
    assert!(probe(health).await, "mTLS healthcheck failed");
    for (path, success) in [(&operator_config, true), (&ordinary_config, false)] {
        let output = tokio::time::timeout(
            Duration::from_secs(180),
            Command::new(env!("CARGO_BIN_EXE_parrot-probe"))
                .arg(format!("--config={}", path.display()))
                .arg(format!("server.test={endpoint}"))
                .arg("--full")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.success(),
            success,
            "stdout={stdout}\nstderr={stderr}"
        );
        if success {
            assert!(stdout.contains("parrot-probe FULL PASS"));
        } else {
            assert!(
                stderr.contains("spawn failed: 13 "),
                "expected Forbidden, got {stderr}"
            );
        }
    }
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("operator.test", None).with_mtls(operator, []),
        Arc::new(EmptyLookup),
    )
    .unwrap();
    client.start().await.unwrap();
    client
        .connect(&NodeAddr::tcp("server.test", endpoint.parse().unwrap()))
        .await
        .unwrap();
    let sample = client.measured_metrics("server.test").await.unwrap();
    use parrot_remote::metrics::field;
    assert!(sample.has(field::TRANSPORT | field::PROCESSES));
    assert_eq!(sample.process_kind, "parrot_actor_entries");
    assert!(!sample.epoch.is_empty());
    assert_eq!(
        sample.processes, 4,
        "authorized probe leaked actors or rejected probe executed commands"
    );
    // Previously completed CLI probes may still be retiring their connections;
    // converge on the one actual operator connection, never accept a fabricated 0.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let sample = client.measured_metrics("server.test").await.unwrap();
            if sample.value(field::CONNECTIONS) == Some(1) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    client.shutdown().await.unwrap();
    child.kill().await.unwrap();
    finish(&mut child).await;
}

#[tokio::test]
async fn directory_role_without_a_persistent_group_must_not_publish_readiness() {
    let f = Fixture::new();
    let config = f.config("[remote.node]\ntopology_role = \"directory\"\n");
    let mut child = f.command().env("PARROT_CONFIG", config).spawn().unwrap();
    let ready = exited_or_ready(&mut child).await;
    if ready.is_some() {
        child.kill().await.unwrap();
    }
    let status = finish(&mut child).await;
    assert!(
        ready.is_none(),
        "node advertised Directory without a persistent group: {ready:?}"
    );
    assert!(!status.success());
}

#[tokio::test]
async fn directory_disk_failure_stops_the_actual_process_and_releases_its_owner() {
    let f = Fixture::new();
    let data = f.0.join("directory");
    std::fs::create_dir(&data).unwrap();
    let config=f.config(&format!("[remote.node]\nnode_id='disk-failure'\nscheme='mem'\ntopology_role='directory'\n[remote.directory]\nstorage_dir={:?}\ngroup='failure-group'\npeers=['mem://absent-voter']\nelection_ms=100\nheartbeat_ms=20\nrpc_timeout_ms=100\n",data.to_str().unwrap()));
    let mut child = f.command().env("PARROT_CONFIG", &config).spawn().unwrap();
    assert_eq!(
        exited_or_ready(&mut child).await.as_deref(),
        Some("mem://disk-failure")
    );
    // A missing second voter keeps elections changing persistent state. Inject
    // the failure only after readiness, without deleting an active state file.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match std::fs::create_dir(data.join("raft.next")) {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    tokio::task::yield_now().await
                }
                Err(e) => panic!("{e}"),
            }
        }
    })
    .await
    .unwrap();
    assert!(!finish(&mut child).await.success());
    std::fs::remove_dir(data.join("raft.next")).unwrap();
    let owner = parrot_remote::roles::directory::DurableDirectory::open(
        &data,
        "failure-group",
        "disk-failure",
        vec!["absent-voter".into()],
        std::sync::Arc::new(parrot_remote::raft::ManualClock::new()),
    )
    .unwrap();
    assert!(!owner.is_closed());
    drop(owner);
    let mut reopened = f.command().env("PARROT_CONFIG", &config).spawn().unwrap();
    assert_eq!(
        exited_or_ready(&mut reopened).await.as_deref(),
        Some("mem://disk-failure")
    );
    reopened.kill().await.unwrap();
    finish(&mut reopened).await;
}
