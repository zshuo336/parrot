//! TCP deployment checks. --smoke runs P1/P2; --full (default) runs P1–P7.
//! P2 opens an isolated socket to each entry node and sends the destination URI
//! on that socket. Route hints never cause this probe to bypass the entry node.
//! These checks do not certify durable exactly-once delivery or cluster recovery.
use futures::{FutureExt, SinkExt, StreamExt};
use parrot_api::{address::ActorRef, types::BoxedMessage};
use parrot_config::endpoint::SeedEndpoint;
use parrot_node::{NEcho, NEchoed, NGet, NGetTotal, NGot, NInc, NPut, NSlowEcho, NTotal};
use parrot_remote::{
    codec_registry::CodecRegistry,
    frame::{frame_type, Frame},
    handshake::{HandshakeAckBody, HandshakeBody},
    transport::FrameCodec,
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio_util::codec::Framed;

type Error = Box<dyn std::error::Error + Send + Sync>;
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
struct NoopLookup;
#[async_trait::async_trait]
impl LocalLookup for NoopLookup {
    async fn lookup(&self, _: &str) -> Option<Box<dyn ActorRef>> {
        None
    }
}
#[derive(Debug)]
struct Options {
    seeds: Vec<String>,
    full: bool,
    config: Option<String>,
    insecure: bool,
}
impl Options {
    fn parse(args: &[String]) -> Result<Self, Error> {
        let mut seeds = Vec::new();
        let mut mode = None;
        let mut config = None;
        let mut insecure = false;
        let mut ids = HashSet::new();
        for arg in args {
            match arg.as_str() {
                "--insecure-development" => insecure = true,
                value if value.starts_with("--config=") => {
                    let path = value.trim_start_matches("--config=");
                    if path.is_empty() || config.replace(path.to_owned()).is_some() {
                        return Err("--config requires one nonempty path".into());
                    }
                }
                "--smoke" | "--full" => {
                    let full = arg == "--full";
                    if mode.is_some_and(|previous| previous != full) {
                        return Err("--smoke and --full conflict".into());
                    }
                    mode = Some(full);
                }
                _ => {
                    if arg.starts_with('-') {
                        return Err(format!("unknown option {arg}").into());
                    }
                    let (id, address) = arg.split_once('=').ok_or_else(|| {
                        format!("invalid argument {arg:?}; expected node-id=host:port")
                    })?;
                    parrot_config::endpoint::validate_node_id(id)?;
                    if !ids.insert(id.to_owned()) {
                        return Err(format!("duplicate target identity {id}").into());
                    }
                    let seed = format!("tcp://{id}@{address}");
                    SeedEndpoint::parse(&seed, "tcp")?;
                    seeds.push(seed);
                }
            }
        }
        if config.is_some() && insecure {
            return Err("--config and --insecure-development conflict".into());
        }
        if seeds.is_empty() {
            return Err("at least one target is required".into());
        }
        Ok(Self {
            seeds,
            full: mode.unwrap_or(true),
            config,
            insecure,
        })
    }
}
#[derive(Debug)]
struct Target {
    id: String,
    candidates: Vec<NodeAddr>,
}
async fn resolve(options: &Options) -> Result<Vec<Target>, Error> {
    let mut resolved = parrot_config::ParrotConfig::builder().build()?;
    resolved.remote.node.seeds = options.seeds.clone();
    // Library resolution preserves every DNS candidate under one total deadline.
    let cfg = RemoteConfig::from_resolved_async(&resolved, CONNECT_TIMEOUT).await?;
    let mut targets: Vec<Target> = Vec::new();
    for candidate in cfg.seeds {
        if let Some(target) = targets.iter_mut().find(|t| t.id == candidate.node_id) {
            target.candidates.push(candidate);
        } else {
            targets.push(Target {
                id: candidate.node_id.clone(),
                candidates: vec![candidate],
            });
        }
    }
    Ok(targets)
}
fn main() -> std::process::ExitCode {
    let options = match Options::parse(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!(
                "[probe] {error}\nusage: parrot-probe <node-id=host:port>... [--smoke | --full] [--config=path | --insecure-development]"
            );
            return 2.into();
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[probe] runtime: {e}");
            return 1.into();
        }
    };
    let result = rt.block_on(async {
        let targets = resolve(&options).await?;
        let cfg = if let Some(path) = &options.config {
            let resolved = parrot_config::ParrotConfig::builder()
                .load_file(path)?
                .build()?;
            if resolved.remote.node.node_id.is_none() {
                return Err("probe config requires remote.node.node_id".into());
            }
            let mut cfg = RemoteConfig::from_resolved(&resolved)?;
            if cfg.scheme != "tcp" {
                return Err("parrot-probe currently requires TCP".into());
            }
            cfg.bind = None;
            cfg.seeds.clear();
            cfg
        } else {
            let identity = probe_identity(
                "probe-client",
                targets.iter().map(|target| target.id.as_str()),
            );
            let cfg = RemoteConfig::tcp(identity, None);
            if options.insecure {
                cfg.with_insecure_development()
            } else {
                cfg
            }
        };
        run(&targets, options.full, cfg).await
    });
    // A timed-out OS DNS lookup cannot hold process exit indefinitely.
    rt.shutdown_timeout(Duration::from_secs(1));
    match result {
        Ok(()) => {
            println!(
                "\nparrot-probe {} PASS",
                if options.full { "FULL" } else { "SMOKE" }
            );
            0.into()
        }
        Err(error) => {
            eprintln!("[probe] FAIL: {error}");
            1.into()
        }
    }
}
async fn connect(client: &Arc<RemoteActorSystem>, target: &Target) -> Result<(), Error> {
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        let mut failures = Vec::new();
        let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
        for (index, addr) in target.candidates.iter().enumerate() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let budget = remaining / (target.candidates.len() - index) as u32;
            match tokio::time::timeout(budget, client.connect(addr)).await {
                Ok(Ok(())) => {
                    // TCP connect alone does not establish the requested identity.
                    if client
                        .links_snapshot()
                        .await
                        .iter()
                        .any(|(id, sender, _)| id == &target.id && !sender.is_closed())
                    {
                        return Ok(());
                    }
                    return Err(format!("peer identity mismatch for {}", target.id).into());
                }
                Ok(Err(error)) => failures.push(error.to_string()),
                Err(_) => failures.push(format!("{} attempt deadline", addr.addr)),
            }
        }
        Err(format!("connect {}: {}", target.id, failures.join("; ")).into())
    })
    .await
    .map_err(|_| format!("connect {} deadline", target.id))?
}

/// Owns exactly one TCP connection. No learned routes, implicit dialing, or
/// background tasks; dropping it closes the connection on every error path.
trait WireIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> WireIo for T {}
struct Wire {
    io: Framed<Box<dyn WireIo>, FrameCodec>,
}
impl Wire {
    async fn connect(target: &Target, config: &RemoteConfig) -> Result<Self, Error> {
        let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
        let mut failures = Vec::new();
        for (index, address) in target.candidates.iter().enumerate() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let budget = remaining / (target.candidates.len() - index) as u32;
            match tokio::time::timeout(budget, Self::connect_at(address, &target.id, config)).await
            {
                Ok(Ok(wire)) => return Ok(wire),
                Ok(Err(error)) => failures.push(error.to_string()),
                Err(_) => failures.push(format!("{} attempt deadline", address.addr)),
            }
        }
        Err(format!("wire connect {}: {}", target.id, failures.join("; ")).into())
    }
    async fn connect_at(
        address: &NodeAddr,
        expected_id: &str,
        config: &RemoteConfig,
    ) -> Result<Self, Error> {
        if matches!(config.security, parrot_remote::RemoteSecurity::Local)
            && !address.addr.ip().is_loopback()
        {
            return Err(
                "non-loopback probe requires mTLS config or explicit insecure development mode"
                    .into(),
            );
        }
        let socket = tokio::net::TcpStream::connect(address.addr).await?;
        socket.set_nodelay(true)?;
        let (io, certs): (Box<dyn WireIo>, _) = if let parrot_remote::RemoteSecurity::MutualTls {
            credentials,
            ..
        } = &config.security
        {
            let connector = parrot_remote::tls::client_connector(credentials)?;
            let stream = connector
                .connect(expected_id.to_owned().try_into()?, socket)
                .await?;
            let certs = stream
                .get_ref()
                .1
                .peer_certificates()
                .unwrap_or_default()
                .to_vec();
            (Box::new(stream), Some(certs))
        } else {
            (Box::new(socket), None)
        };
        let mut wire = Self {
            io: Framed::new(io, FrameCodec),
        };
        let mut body = bytes::BytesMut::new();
        HandshakeBody {
            node_id: config.node_id.clone(),
            ..Default::default()
        }
        .encode_tlv(&mut body)?;
        wire.io.send(Frame::handshake(body.freeze())).await?;
        let ack = wire
            .io
            .next()
            .await
            .ok_or("peer closed during handshake")??;
        if ack.header.frame_type != frame_type::HANDSHAKE_ACK {
            return Err("expected handshake ACK".into());
        }
        let ack = HandshakeAckBody::decode_tlv(&ack.payload)?;
        if ack.node_id != expected_id {
            return Err(format!(
                "peer identity mismatch: wanted {expected_id}, got {}",
                ack.node_id
            )
            .into());
        }
        if let Some(certs) = certs {
            parrot_remote::tls::verify_node_identity(&certs, &ack.node_id)?;
        }
        parrot_remote::handshake::negotiate_caps(
            parrot_remote::handshake::caps::BIN,
            ack.capabilities,
        )
        .map_err(|e| format!("codec negotiation: {e:?}"))?;
        Ok(wire)
    }
    async fn ask(&mut self, cid: u64, path: &str, message: BoxedMessage) -> Result<(), Error> {
        let (key, payload) = CodecRegistry::global()
            .encode_outgoing(&message)
            .map_err(|e| format!("encode: {e:?}"))?;
        self.io
            .send(Frame::ask(cid, path, &key, payload.into(), None))
            .await?;
        Ok(())
    }
    async fn response_frame(&mut self) -> Result<Frame, Error> {
        loop {
            let frame = self.io.next().await.ok_or("peer closed before reply")??;
            match frame.header.frame_type {
                frame_type::HEARTBEAT => self.io.send(Frame::heartbeat_ack()).await?,
                frame_type::HEARTBEAT_ACK | frame_type::ROUTE_HINT => {}
                frame_type::REPLY | frame_type::REPLY_ERR | frame_type::ERROR => return Ok(frame),
                kind => return Err(format!("unexpected frame {kind:#x}").into()),
            }
        }
    }
    async fn reply(&mut self) -> Result<(u64, BoxedMessage), Error> {
        let frame = self.response_frame().await?;
        if frame.header.frame_type != frame_type::REPLY {
            return Err(format!(
                "remote error: {:?}",
                parrot_remote::error::decode_err_payload(&frame.payload)
            )
            .into());
        }
        Ok((
            frame.header.correlation_id,
            CodecRegistry::global()
                .decode_incoming(&frame.type_key, &frame.payload)
                .map_err(|e| format!("decode: {e:?}"))?,
        ))
    }
    async fn expect_absent(&mut self, cid: u64) -> Result<(), Error> {
        let frame = self.response_frame().await?;
        ensure(
            frame.header.frame_type == frame_type::REPLY_ERR
                && frame.header.correlation_id == cid
                && matches!(
                    parrot_remote::error::decode_err_payload(&frame.payload),
                    Ok((parrot_remote::error::ErrCode::ActorNotFound, _))
                ),
            "stopped actor did not return wire ActorNotFound",
        )
    }
    async fn expect_echo(&mut self, cid: u64, value: u64) -> Result<(), Error> {
        let (actual_cid, reply) = self.reply().await?;
        if actual_cid != cid || reply.downcast_ref::<NEchoed>().map(|r| r.0) != Some(value) {
            return Err(format!(
                "wrong echo reply, expected cid={cid}, value={value}; received cid={actual_cid}"
            )
            .into());
        }
        Ok(())
    }
}
async fn cross_route(from: &Target, to: &Target, config: &RemoteConfig) -> Result<(), Error> {
    let mut wire = Wire::connect(from, config).await?;
    for (cid, value) in [(1, 99), (2, 107)] {
        wire.ask(
            cid,
            &format!("parrot://{}/user/echo", to.id),
            Box::new(NEcho(value)),
        )
        .await?;
        wire.expect_echo(cid, value).await?;
    }
    Ok(())
}
struct OwnedActors {
    paths: Vec<(String, String)>,
}
impl OwnedActors {
    async fn spawn(
        &mut self,
        client: &Arc<RemoteActorSystem>,
        node: &str,
        props: &str,
    ) -> Result<parrot_remote::RemoteActorRef, Error> {
        let path = format!("/user/{}", uniq("probe"));
        // Track before awaiting: a lost reply may still have created the actor.
        self.paths.push((node.to_owned(), path.clone()));
        Ok(client.spawn_named(node, &path, props).await?)
    }
    async fn cleanup(&mut self, client: &Arc<RemoteActorSystem>) -> Result<(), Error> {
        let mut errors = Vec::new();
        for (node, path) in self.paths.drain(..).rev() {
            match tokio::time::timeout(CHECK_TIMEOUT, client.admin_stop(&node, &path)).await {
                Ok(Ok(())) => {}
                error => errors.push(format!("cleanup {node}{path}: {error:?}")),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; ").into())
        }
    }
}
async fn run(targets: &[Target], full: bool, config: RemoteConfig) -> Result<(), Error> {
    let client = RemoteActorSystem::new(config, Arc::new(NoopLookup))?;
    let mut owned = OwnedActors { paths: Vec::new() };
    let result = std::panic::AssertUnwindSafe(async {
        client.start().await?;
        for target in targets {
            connect(&client, target).await?;
        }
        checks(&client, targets, full, &mut owned).await
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|_| Err("scenario panicked".into()));
    let cleanup = owned.cleanup(&client).await;
    let shutdown = tokio::time::timeout(CONNECT_TIMEOUT, client.shutdown()).await;
    let mut errors = Vec::new();
    if let Err(error) = result {
        errors.push(error.to_string());
    }
    if let Err(error) = cleanup {
        errors.push(error.to_string());
    }
    if !matches!(shutdown, Ok(Ok(()))) {
        errors.push(format!("client shutdown: {shutdown:?}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; ").into())
    }
}
async fn checks(
    client: &Arc<RemoteActorSystem>,
    targets: &[Target],
    full: bool,
    owned: &mut OwnedActors,
) -> Result<(), Error> {
    let primary = &targets[0];
    let pid = &primary.id;
    for target in targets {
        check(&format!("P1 echo on {}", target.id), async {
            let echo = client.remote_ref(&format!("parrot://{}/user/echo", target.id))?;
            let reply = echo.send(Box::new(NEcho(41))).await?;
            ensure(
                reply.downcast_ref::<NEchoed>().map(|r| r.0) == Some(41),
                "wrong echo",
            )
        })
        .await?;
    }
    check("P1 isolated counter", async {
        let counter = owned.spawn(client, pid, "deploy.counter").await?;
        counter.send(Box::new(NInc(7))).await?;
        counter.send(Box::new(NInc(3))).await?;
        let reply = counter.send(Box::new(NGetTotal)).await?;
        ensure(
            reply.downcast_ref::<NTotal>().map(|r| r.0) == Some(10),
            "wrong counter total",
        )
    })
    .await?;
    check("P1 isolated kv", async {
        let kv = owned.spawn(client, pid, "deploy.kv").await?;
        kv.send(Box::new(NPut("k1".into(), "v1".into()))).await?;
        let reply = kv.send(Box::new(NGet("k1".into()))).await?;
        ensure(
            reply.downcast_ref::<NGot>().and_then(|r| r.0.as_deref()) == Some("v1"),
            "wrong kv value",
        )
    })
    .await?;
    check("P1 slow response", async {
        let before = Instant::now();
        let slow = client.remote_ref(&format!("parrot://{pid}/user/slow"))?;
        let reply = slow.send(Box::new(NSlowEcho(80))).await?;
        ensure(
            reply.downcast_ref::<NEchoed>().map(|r| r.0) == Some(80)
                && before.elapsed() >= Duration::from_millis(80),
            "wrong slow response",
        )
    })
    .await?;
    // Adjacent pairs in both directions plus first/last, without duplicates.
    let mut pairs: Vec<(usize, usize)> = (0..targets.len().saturating_sub(1))
        .map(|i| (i, i + 1))
        .collect();
    if targets.len() > 2 {
        pairs.push((0, targets.len() - 1));
    }
    if pairs.is_empty() {
        println!("[probe] P2 NOT APPLICABLE: one target; no cross-node route was tested");
    }
    for (a, b) in pairs {
        for (from, to) in [(a, b), (b, a)] {
            check(
                &format!("P2 cross-route {} -> {}", targets[from].id, targets[to].id),
                cross_route(&targets[from], &targets[to], &client.config),
            )
            .await?;
        }
    }
    if !full {
        return Ok(());
    }
    check("P3 remote spawn", async {
        let echo = owned.spawn(client, pid, "deploy.echo").await?;
        let reply = echo.send(Box::new(NEcho(7))).await?;
        ensure(
            reply.downcast_ref::<NEchoed>().map(|r| r.0) == Some(7),
            "spawned actor reply",
        )
    })
    .await?;
    check("P4 acknowledged stop and actor absence", async {
        let counter = owned.spawn(client, pid, "deploy.counter").await?;
        let (node, path) = owned.paths.last().unwrap().clone();
        counter.send(Box::new(NGetTotal)).await?;
        client.admin_stop(&node, &path).await?;
        // The facade maps RouteUnreachable and ActorNotFound to the same Rust
        // variant. Inspect the actual wire code so missing routes cannot pass.
        let mut wire = Wire::connect(primary, &client.config).await?;
        wire.ask(1, &format!("parrot://{node}{path}"), Box::new(NGetTotal))
            .await?;
        wire.expect_absent(1).await?;
        owned.paths.pop();
        Ok(())
    })
    .await?;
    check(
        "P5 128 correlated replies and server-side increments",
        async {
            let echo = client.remote_ref(&format!("parrot://{pid}/user/echo"))?;
            let counter = owned.spawn(client, pid, "deploy.counter").await?;
            // Structured futures: a timeout drops every request, never detaches tasks.
            let requests = (0..128u64).map(|i| {
                let echo = &echo;
                let counter = &counter;
                async move {
                    let reply = echo.send(Box::new(NEcho(i))).await?;
                    ensure(
                        reply.downcast_ref::<NEchoed>().map(|r| r.0) == Some(i),
                        "wrong correlation",
                    )?;
                    counter.send(Box::new(NInc(1))).await?;
                    Ok::<_, Error>(())
                }
            });
            futures::future::try_join_all(requests).await?;
            let total = counter.send(Box::new(NGetTotal)).await?;
            ensure(
                total.downcast_ref::<NTotal>().map(|r| r.0) == Some(128),
                "server-side increment count is not 128",
            )
        },
    )
    .await?;
    check("P6 same-connection slow/echo reply isolation", async {
        let mut wire = Wire::connect(primary, &client.config).await?;
        // Slow ASK is written first, then echo on the very same socket. Require
        // echo's reply before slow's, and validate both cid and business values.
        wire.ask(
            1,
            &format!("parrot://{pid}/user/slow"),
            Box::new(NSlowEcho(600)),
        )
        .await?;
        wire.ask(2, &format!("parrot://{pid}/user/echo"), Box::new(NEcho(1)))
            .await?;
        wire.expect_echo(2, 1).await?;
        wire.expect_echo(1, 600).await
    })
    .await?;
    check("P7 echo latency sample", async {
        let echo = client.remote_ref(&format!("parrot://{pid}/user/echo"))?;
        let mut latencies = Vec::new();
        for i in 0..600u64 {
            let before = Instant::now();
            let reply = echo.send(Box::new(NEcho(i))).await?;
            ensure(
                reply.downcast_ref::<NEchoed>().map(|r| r.0) == Some(i),
                "latency sample wrong reply",
            )?;
            if i >= 100 {
                latencies.push(before.elapsed().as_micros());
            }
        }
        latencies.sort_unstable();
        println!(
            "[probe] P7 echo RTT: n={} p50={}µs p99={}µs (observational)",
            latencies.len(),
            latencies[latencies.len() / 2],
            latencies[latencies.len() * 99 / 100]
        );
        Ok(())
    })
    .await
}
fn ensure(condition: bool, detail: &str) -> Result<(), Error> {
    if condition {
        Ok(())
    } else {
        Err(detail.to_owned().into())
    }
}
fn uniq(base: &str) -> String {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    format!(
        "{base}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    )
}
// Stable transport identities keep repeated process probes from adding a new
// permanent NodeTable entry each time. Actor paths remain unique per attempt.
// Replies use their originating connection, so simultaneous probes do not share
// callbacks even when their handshake identity matches.
fn probe_identity<'a>(prefix: &str, ids: impl IntoIterator<Item = &'a str>) -> String {
    use sha2::{Digest, Sha256};
    let mut ids: Vec<_> = ids.into_iter().collect();
    ids.sort_unstable();
    let mut hash = Sha256::new();
    for id in ids {
        hash.update((id.len() as u64).to_le_bytes());
        hash.update(id.as_bytes());
    }
    format!("{prefix}-{}", hex::encode(hash.finalize()))
}
async fn check(
    name: &str,
    future: impl std::future::Future<Output = Result<(), Error>>,
) -> Result<(), Error> {
    let before = Instant::now();
    match tokio::time::timeout(CHECK_TIMEOUT, future).await {
        Ok(Ok(())) => {
            println!("[probe] PASS {name} ({:?})", before.elapsed());
            Ok(())
        }
        Ok(Err(error)) => Err(format!("{name}: {error}").into()),
        Err(_) => Err(format!("{name}: deadline {CHECK_TIMEOUT:?}").into()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn arguments_are_atomic_and_strict() {
        for values in [
            vec![],
            vec!["--typo"],
            vec!["--typo=localhost:42"],
            vec!["a=localhost:1", "bad"],
            vec!["a=localhost:1", "--smoke", "--full"],
            vec!["=localhost:1"],
            vec!["a=localhost:1", "a=localhost:2"],
            vec!["a=localhost:0"],
            vec!["a/b=localhost:1"],
            vec!["a=::1:42"],
            vec!["a=host:1 "],
        ] {
            assert!(
                Options::parse(&args(&values)).is_err(),
                "accepted {values:?}"
            );
        }
        let options = Options::parse(&args(&["a=localhost:42", "b=[::1]:43", "--smoke"])).unwrap();
        assert!(!options.full);
        assert_eq!(options.seeds, ["tcp://a@localhost:42", "tcp://b@[::1]:43"]);
    }

    async fn socket_pair() -> (Wire, Framed<tokio::net::TcpStream, FrameCodec>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, server) = tokio::join!(
            tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        (
            Wire {
                io: Framed::new(Box::new(client.unwrap()), FrameCodec),
            },
            Framed::new(server.unwrap().0, FrameCodec),
        )
    }

    #[tokio::test]
    async fn mismatched_reply_and_missing_route_never_count_as_success() {
        let message: BoxedMessage = Box::new(NEchoed(99));
        let (key, payload) = CodecRegistry::global().encode_outgoing(&message).unwrap();
        for (cid, value) in [(2, 99), (1, 100)] {
            let (mut wire, mut peer) = socket_pair().await;
            peer.send(Frame::reply(cid, "", &key, payload.clone().into()))
                .await
                .unwrap();
            assert!(wire.expect_echo(1, value).await.is_err());
        }
        for code in [
            parrot_remote::error::ErrCode::RouteUnreachable,
            parrot_remote::error::ErrCode::Timeout,
        ] {
            let (mut wire, mut peer) = socket_pair().await;
            peer.send(Frame::reply_err(1, "", code, "not an absence proof"))
                .await
                .unwrap();
            assert!(wire.expect_absent(1).await.is_err());
        }
        let (mut wire, mut peer) = socket_pair().await;
        peer.send(Frame::reply_err(
            1,
            "",
            parrot_remote::error::ErrCode::ActorNotFound,
            "missing",
        ))
        .await
        .unwrap();
        wire.expect_absent(1).await.unwrap();
    }

    #[tokio::test]
    async fn route_hints_cannot_bypass_the_selected_socket() {
        let (mut wire, mut peer) = socket_pair().await;
        let trap = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let exchange = async {
            peer.send(
                Frame::try_route_hint("destination", &trap.local_addr().unwrap().to_string())
                    .unwrap(),
            )
            .await
            .unwrap();
            peer.send(Frame::heartbeat()).await.unwrap();
            assert_eq!(
                peer.next().await.unwrap().unwrap().header.frame_type,
                frame_type::HEARTBEAT_ACK
            );
            let message: BoxedMessage = Box::new(NEchoed(99));
            let (key, payload) = CodecRegistry::global().encode_outgoing(&message).unwrap();
            peer.send(Frame::reply(1, "", &key, payload.into()))
                .await
                .unwrap();
            let next = peer.next().await.unwrap().unwrap();
            assert_eq!(next.path, "parrot://destination/user/echo");
            assert_eq!(next.header.correlation_id, 2);
        };
        let request = async {
            wire.expect_echo(1, 99).await.unwrap();
            wire.ask(2, "parrot://destination/user/echo", Box::new(NEcho(107)))
                .await
                .unwrap();
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(exchange, request);
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), trap.accept())
                .await
                .is_err(),
            "probe dialed the route hint"
        );
    }

    #[tokio::test]
    async fn unavailable_dns_candidate_does_not_hide_the_reachable_candidate() {
        let server = RemoteActorSystem::new(
            RemoteConfig::tcp("candidate-server", Some("127.0.0.1:0".parse().unwrap())),
            Arc::new(NoopLookup),
        )
        .unwrap();
        server.start().await.unwrap();
        let refused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refused_address = refused.local_addr().unwrap();
        drop(refused);
        let target = Target {
            id: "candidate-server".into(),
            candidates: vec![
                NodeAddr::tcp("candidate-server", refused_address),
                NodeAddr::tcp("candidate-server", server.local_addr().unwrap()),
            ],
        };
        let client = RemoteActorSystem::new(
            RemoteConfig::tcp("candidate-client", None),
            Arc::new(NoopLookup),
        )
        .unwrap();
        client.start().await.unwrap();
        let normal = connect(&client, &target).await;
        let wire = Wire::connect(&target, &RemoteConfig::tcp("probe-wire-test", None)).await;
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
        normal.unwrap();
        wire.unwrap();
    }

    #[tokio::test]
    async fn silent_first_candidate_cannot_consume_the_entire_connect_budget() {
        let server = RemoteActorSystem::new(
            RemoteConfig::tcp("fallback-server", Some("127.0.0.1:0".parse().unwrap())),
            Arc::new(NoopLookup),
        )
        .unwrap();
        server.start().await.unwrap();
        // Accepts TCP into the backlog but never acknowledges a handshake.
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target {
            id: "fallback-server".into(),
            candidates: vec![
                NodeAddr::tcp("fallback-server", silent.local_addr().unwrap()),
                NodeAddr::tcp("fallback-server", server.local_addr().unwrap()),
            ],
        };
        let client = RemoteActorSystem::new(
            RemoteConfig::tcp("fallback-client", None),
            Arc::new(NoopLookup),
        )
        .unwrap();
        client.start().await.unwrap();
        let results = tokio::time::timeout(CONNECT_TIMEOUT + Duration::from_secs(1), async {
            tokio::join!(
                connect(&client, &target),
                Wire::connect(&target, &client.config)
            )
        })
        .await;
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
        let (normal, wire) =
            results.expect("both paths must reach the second candidate within one budget");
        normal.unwrap();
        wire.unwrap();
    }

    #[tokio::test]
    async fn repeated_wire_connections_keep_one_peer_identity() {
        let server = RemoteActorSystem::new(
            RemoteConfig::tcp("identity-server", Some("127.0.0.1:0".parse().unwrap())),
            Arc::new(NoopLookup),
        )
        .unwrap();
        server.start().await.unwrap();
        let target = Target {
            id: "identity-server".into(),
            candidates: vec![NodeAddr::tcp(
                "identity-server",
                server.local_addr().unwrap(),
            )],
        };
        for _ in 0..4 {
            let mut wire = Wire::connect(&target, &RemoteConfig::tcp("probe-wire-test", None))
                .await
                .unwrap();
            wire.ask(
                1,
                "parrot://identity-server/user/missing",
                Box::new(NEcho(1)),
            )
            .await
            .unwrap();
            wire.expect_absent(1).await.unwrap();
        }
        let count = server.nodes.len();
        server.shutdown().await.unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            probe_identity("probe-client", ["a", "b"]),
            probe_identity("probe-client", ["b", "a"])
        );
        assert_ne!(
            probe_identity("probe-client", ["ab", "c"]),
            probe_identity("probe-client", ["a", "bc"])
        );
    }
}
