#![cfg(unix)]
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_config::OrderedReceiverState;
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Work(u64);
impl parrot_api::Message for Work {
    type Result = ();
}

struct State {
    seen: tokio::sync::mpsc::UnboundedSender<u64>,
    entered: tokio::sync::Notify,
    dropping: tokio::sync::Notify,
    dropped: AtomicBool,
    gate: (Mutex<bool>, Condvar),
    marker: Option<PathBuf>,
}
fn state(marker: Option<PathBuf>) -> (Arc<State>, tokio::sync::mpsc::UnboundedReceiver<u64>) {
    let (seen, rx) = tokio::sync::mpsc::unbounded_channel();
    (
        Arc::new(State {
            seen,
            entered: Default::default(),
            dropping: Default::default(),
            dropped: AtomicBool::new(false),
            gate: (Mutex::new(false), Condvar::new()),
            marker,
        }),
        rx,
    )
}
struct Release(Arc<State>);
impl Drop for Release {
    fn drop(&mut self) {
        self.release();
    }
}
impl Release {
    fn release(&self) {
        *self.0.gate.0.lock().unwrap() = true;
        self.0.gate.1.notify_all();
    }
}
struct Held(Arc<State>);
impl Future for Held {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(path) = &self.0.marker {
            std::fs::write(path, b"entered").unwrap();
        }
        self.0.entered.notify_one();
        Poll::Pending
    }
}
impl Drop for Held {
    fn drop(&mut self) {
        self.0.dropping.notify_one();
        let mut gate = self.0.gate.0.lock().unwrap();
        while !*gate {
            gate = self.0.gate.1.wait(gate).unwrap();
        }
        self.0.dropped.store(true, Ordering::SeqCst);
    }
}
#[derive(Clone)]
struct Target(Arc<State>);
impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Target")
    }
}
#[async_trait::async_trait]
impl LocalLookup for Target {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/target").then(|| Box::new(self.clone()) as BoxedActorRef)
    }
}
impl ActorRef for Target {
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        let value = message.downcast_ref::<Work>().unwrap().0;
        if value == 700 {
            return Box::pin(Held(self.0.clone()));
        }
        Box::pin(async move {
            self.0.seen.send(value).unwrap();
            Ok(())
        })
    }
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn config(directory: &Path) -> RemoteConfig {
    RemoteConfig::tcp("recovery-target", Some("127.0.0.1:0".parse().unwrap()))
        .with_insecure_development()
        .with_knobs(parrot_remote::transport::RuntimeKnobs {
            heartbeat_enabled: false,
            ..Default::default()
        })
        .with_ordered_receiver_state(OrderedReceiverState {
            directory: directory.into(),
            realm: "recovery-contract".into(),
        })
}
async fn start(config: RemoteConfig, state: Arc<State>) -> Arc<RemoteActorSystem> {
    let node = RemoteActorSystem::new(config, Arc::new(Target(state))).unwrap();
    node.start().await.unwrap();
    node
}
async fn source() -> Arc<RemoteActorSystem> {
    start(
        RemoteConfig::tcp("recovery-source", None).with_insecure_development(),
        state(None).0,
    )
    .await
}
async fn connect(source: &Arc<RemoteActorSystem>, target: &RemoteActorSystem) {
    source
        .connect(&NodeAddr::tcp(
            "recovery-target",
            target.local_addr().unwrap(),
        ))
        .await
        .unwrap();
}
async fn send(reference: &parrot_remote::RemoteActorRef, value: u64) {
    reference
        .deliver_ordered(
            Box::new(Work(value)),
            parrot_api::message::MessageOptions {
                timeout: Some(Duration::from_secs(5)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
}
async fn seen(rx: &mut tokio::sync::mpsc::UnboundedReceiver<u64>, value: u64) {
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap(),
        Some(value)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopped_ingress_retains_disk_ownership_until_actual_old_delivery_drop_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let (old_state, _old_rx) = state(None);
    let release = Release(old_state.clone());
    let old = start(config(directory.path()), old_state.clone()).await;
    let retained_ingress = old.ingress.clone();
    let source = source().await;
    connect(&source, &old).await;
    let reference = source
        .remote_ref("parrot://recovery-target/target")
        .unwrap();
    send(&reference, 700).await;
    tokio::time::timeout(Duration::from_secs(5), old_state.entered.notified())
        .await
        .unwrap();
    old.ingress.close();
    let closing = old.clone();
    let shutdown = tokio::spawn(async move { closing.shutdown().await });
    tokio::time::timeout(Duration::from_secs(5), old_state.dropping.notified())
        .await
        .unwrap();
    let (new_state, mut rx) = state(None);
    let attempt = RemoteActorSystem::new(
        config(directory.path()),
        Arc::new(Target(new_state.clone())),
    );
    assert!(
        attempt.is_err(),
        "receiver state lock was released while the original Drop was blocked"
    );
    assert!(!old_state.dropped.load(Ordering::SeqCst));
    release.release();
    shutdown.await.unwrap().unwrap();
    assert!(old_state.dropped.load(Ordering::SeqCst));
    let replacement = start(config(directory.path()), new_state).await;
    connect(&source, &replacement).await;
    send(&reference, 1).await;
    seen(&mut rx, 1).await;
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    source.shutdown().await.unwrap();
    replacement.shutdown().await.unwrap();
    drop(retained_ingress);
}

struct Process(std::process::Child);
impl Process {
    fn kill_and_reap(&mut self) -> std::process::ExitStatus {
        self.0.kill().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "fixture process did not exit after kill"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        let _ = self.0.kill();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.0.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
async fn wait_file(path: &Path) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(bytes) = std::fs::read(path) {
                if !bytes.is_empty() {
                    break bytes;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_process_death_and_later_clean_restart_recover_the_original_source_lane() {
    recover_after_process_death(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_close_after_actual_process_death_uses_the_successor_retirement_proof() {
    recover_after_process_death(true).await;
}

async fn recover_after_process_death(close_first: bool) {
    let directory = tempfile::tempdir().unwrap();
    let control = tempfile::tempdir().unwrap();
    let ready = control.path().join("ready");
    let entered = control.path().join("entered");
    let mut child = Process(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "receiver_process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("PARROT_RECEIVER_FIXTURE_DIRECTORY", directory.path())
            .env("PARROT_RECEIVER_FIXTURE_READY", &ready)
            .env("PARROT_RECEIVER_FIXTURE_ENTERED", &entered)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let address: std::net::SocketAddr = String::from_utf8(wait_file(&ready).await)
        .unwrap()
        .parse()
        .unwrap();
    let source = source().await;
    source
        .connect(&NodeAddr::tcp("recovery-target", address))
        .await
        .unwrap();
    let reference = source
        .remote_ref("parrot://recovery-target/target")
        .unwrap();
    send(&reference, 700).await;
    assert_eq!(wait_file(&entered).await, b"entered");
    assert!(
        RemoteActorSystem::new(config(directory.path()), Arc::new(Target(state(None).0))).is_err()
    );
    let status = child.kill_and_reap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    let (new_state, mut rx) = state(None);
    let replacement = start(config(directory.path()), new_state.clone()).await;
    connect(&source, &replacement).await;
    if close_first {
        reference
            .close_ordered_session(Duration::from_secs(5))
            .await
            .unwrap();
    }
    send(&reference, 1).await;
    seen(&mut rx, 1).await;
    replacement.shutdown().await.unwrap();
    let next = start(config(directory.path()), new_state).await;
    connect(&source, &next).await;
    send(&reference, 2).await;
    seen(&mut rx, 2).await;
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    assert!(rx.try_recv().is_err());
    source.shutdown().await.unwrap();
    next.shutdown().await.unwrap();
}

#[tokio::test]
async fn unrelated_state_directory_cannot_erase_the_old_grant_and_the_correct_domain_can_recover() {
    let directory = tempfile::tempdir().unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    let (old_state, mut old_rx) = state(None);
    let old = start(config(directory.path()), old_state).await;
    let source = source().await;
    connect(&source, &old).await;
    let reference = source
        .remote_ref("parrot://recovery-target/target")
        .unwrap();
    send(&reference, 1).await;
    seen(&mut old_rx, 1).await;
    old.shutdown().await.unwrap();
    let (foreign_state, mut foreign_rx) = state(None);
    let foreign = start(config(unrelated.path()), foreign_state).await;
    connect(&source, &foreign).await;
    assert!(reference
        .deliver_ordered(Box::new(Work(99)), Default::default())
        .await
        .is_err());
    assert!(foreign_rx.try_recv().is_err());
    let (correct_state, mut correct_rx) = state(None);
    let correct = start(config(directory.path()), correct_state).await;
    connect(&source, &correct).await;
    send(&reference, 2).await;
    seen(&mut correct_rx, 2).await;
    assert!(foreign_rx.try_recv().is_err());
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    for node in [&source, &foreign, &correct] {
        node.shutdown().await.unwrap();
    }
}

#[test]
#[ignore = "isolated process fixture; executed by the process-death contract"]
fn receiver_process_fixture() {
    let directory = PathBuf::from(
        std::env::var_os("PARROT_RECEIVER_FIXTURE_DIRECTORY").expect("fixture-only directory"),
    );
    let ready = PathBuf::from(std::env::var_os("PARROT_RECEIVER_FIXTURE_READY").unwrap());
    let entered = PathBuf::from(std::env::var_os("PARROT_RECEIVER_FIXTURE_ENTERED").unwrap());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let node = start(config(&directory), state(Some(entered)).0).await;
            std::fs::write(ready, node.local_addr().unwrap().to_string()).unwrap();
            std::future::pending::<()>().await;
        });
}

struct Raw {
    handle: parrot_remote::transport::ConnectionHandle,
    incoming: tokio::sync::mpsc::Receiver<(
        parrot_remote::frame::Frame,
        parrot_remote::transport::FrameSender,
        String,
    )>,
    stop: tokio::sync::watch::Sender<bool>,
    wire: tokio::net::TcpStream,
    cid: u64,
}
impl Raw {
    async fn connect(target: &RemoteActorSystem, recovery: bool, matching_origin: bool) -> Self {
        use parrot_remote::{
            handshake::{caps, HandshakeBody},
            transport::{run_connection, ConnParams, ConnSide, RuntimeKnobs},
        };
        let socket = tokio::net::TcpStream::connect(target.local_addr().unwrap())
            .await
            .unwrap()
            .into_std()
            .unwrap();
        let wire = tokio::net::TcpStream::from_std(socket.try_clone().unwrap()).unwrap();
        let socket = tokio::net::TcpStream::from_std(socket).unwrap();
        let (tx, incoming) = tokio::sync::mpsc::channel(64);
        let (stop, _) = tokio::sync::watch::channel(false);
        let handle = run_connection(
            socket,
            ConnParams {
                side: ConnSide::Connect,
                local_addr: None,
                peer_addr: None,
                scheme: "tcp",
                local_handshake: HandshakeBody {
                    node_id: if matching_origin {
                        "wire-source"
                    } else {
                        "ungranted-source"
                    }
                    .into(),
                    capabilities: caps::BIN
                        | caps::ORDERED_TELL
                        | if recovery { caps::ORDERED_RECOVERY } else { 0 },
                    ..Default::default()
                },
                inbound: tx,
                on_disconnect: Arc::new(|_| {}),
                shutdown: stop.subscribe(),
                knobs: Some(RuntimeKnobs {
                    heartbeat_enabled: false,
                    ..Default::default()
                }),
            },
        )
        .await
        .unwrap();
        Self {
            handle,
            incoming,
            stop,
            wire,
            cid: 0,
        }
    }
    fn frame(
        &mut self,
        operation: u8,
        ticket: [u8; 16],
        receiver: Option<([u8; 16], u64)>,
    ) -> parrot_remote::frame::Frame {
        let source = "wire-source";
        let mut bytes = vec![2, operation];
        bytes.extend_from_slice(&[1; 16]);
        bytes.extend_from_slice(&[2; 16]);
        bytes.extend_from_slice(&ticket);
        bytes.extend_from_slice(&(source.len() as u16).to_le_bytes());
        bytes.extend_from_slice(source.as_bytes());
        if let Some((receiver, epoch)) = receiver {
            bytes.extend_from_slice(&receiver);
            bytes.extend_from_slice(&epoch.to_le_bytes());
        }
        self.cid += 1;
        parrot_remote::frame::Frame::ask(
            self.cid,
            "parrot://recovery-target/target",
            parrot_remote::ordered::CONTROL_KEY,
            bytes.into(),
            None,
        )
    }
    async fn response(&mut self) -> parrot_remote::frame::Frame {
        let (frame, _, _) = tokio::time::timeout(Duration::from_secs(5), self.incoming.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.header.correlation_id, self.cid);
        frame
    }
    async fn query(
        &mut self,
        operation: u8,
        ticket: [u8; 16],
        receiver: Option<([u8; 16], u64)>,
    ) -> parrot_remote::frame::Frame {
        let frame = self.frame(operation, ticket, receiver);
        self.handle.sender.send(frame).await.unwrap();
        self.response().await
    }
}
impl Drop for Raw {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
fn receipt(frame: &parrot_remote::frame::Frame, extended: bool) -> ([u8; 16], [u8; 16], u64) {
    assert_eq!(
        frame.header.frame_type,
        parrot_remote::frame::frame_type::REPLY
    );
    assert_eq!(frame.type_key, parrot_remote::ordered::RECEIPT_KEY);
    assert_eq!(frame.payload.len(), if extended { 54 } else { 46 });
    (
        frame.payload[2..18].try_into().unwrap(),
        frame.payload[30..46].try_into().unwrap(),
        if extended {
            u64::from_le_bytes(frame.payload[46..54].try_into().unwrap())
        } else {
            0
        },
    )
}
fn rejected(frame: &parrot_remote::frame::Frame, code: parrot_remote::error::ErrCode) {
    assert_eq!(
        frame.header.frame_type,
        parrot_remote::frame::frame_type::REPLY_ERR
    );
    assert_eq!(
        parrot_remote::error::decode_err_payload(&frame.payload)
            .unwrap()
            .0,
        code
    );
}

#[tokio::test]
async fn actual_wire_recovery_requires_negotiation_origin_and_an_exact_previous_instance() {
    use parrot_remote::error::ErrCode;
    use tokio::io::AsyncWriteExt;
    let directory = tempfile::tempdir().unwrap();
    let old = start(config(directory.path()), state(None).0).await;
    let mut peer = Raw::connect(&old, true, true).await;
    let (ticket, old_receiver, epoch) = receipt(&peer.query(0, [0; 16], None).await, true);
    assert_eq!(epoch, 1);
    old.shutdown().await.unwrap();
    drop(peer);
    let new = start(config(directory.path()), state(None).0).await;
    let mut peer = Raw::connect(&new, true, true).await;
    let (_, current, current_epoch) = receipt(&peer.query(0, [0; 16], None).await, true);
    assert_eq!(current_epoch, 2);
    let retired = peer.query(3, ticket, Some((old_receiver, epoch))).await;
    assert_eq!(receipt(&retired, true), (ticket, old_receiver, epoch));
    assert_eq!(retired.payload[1], 0);
    for (receiver, epoch) in [
        (current, current_epoch),
        (old_receiver, u64::MAX),
        ([42; 16], 1),
    ] {
        rejected(
            &peer.query(3, ticket, Some((receiver, epoch))).await,
            ErrCode::Forbidden,
        );
    }
    rejected(
        &peer.query(3, ticket, Some((old_receiver, 0))).await,
        ErrCode::ProtocolViolation,
    );
    let mut forged = Raw::connect(&new, true, false).await;
    rejected(
        &forged.query(3, ticket, Some((old_receiver, 1))).await,
        ErrCode::Forbidden,
    );
    let mut legacy = Raw::connect(&new, false, true).await;
    receipt(&legacy.query(0, [0; 16], None).await, false);
    let frame = legacy.frame(3, ticket, Some((old_receiver, 1)));
    assert!(
        legacy.handle.sender.send(frame.clone()).await.is_err(),
        "unnegotiated Recover entered the writer queue"
    );
    // Bypass the sender's guard to exercise actual receiver admission too.
    let mut bytes = bytes::BytesMut::new();
    frame.encode(&mut bytes).unwrap();
    legacy.wire.write_all(&bytes).await.unwrap();
    let (terminal, _, _) = tokio::time::timeout(Duration::from_secs(5), legacy.incoming.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        terminal.header.frame_type,
        parrot_remote::frame::frame_type::ERROR
    );
    assert_eq!(terminal.header.correlation_id, 0);
    assert_eq!(
        parrot_remote::error::decode_err_payload(&terminal.payload)
            .unwrap()
            .0,
        ErrCode::ProtocolViolation
    );
    legacy.handle.sender.close();
    drop(peer);
    drop(forged);
    drop(legacy);
    new.shutdown().await.unwrap();
}

#[tokio::test]
async fn actual_mtls_tcp_and_quic_recover_the_retired_receiver_without_an_old_socket() {
    let mut ca = rcgen::CertificateParams::default();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    let ca_pem = ca.self_signed(&key).unwrap().pem().into_bytes();
    let issuer = rcgen::Issuer::new(ca, key);
    let credentials = |name: &str| {
        let mut parameters = rcgen::CertificateParams::new(vec![name.into()]).unwrap();
        parameters
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().unwrap();
        parrot_remote::tls::TlsConfig {
            cert_pem: parameters
                .signed_by(&key, &issuer)
                .unwrap()
                .pem()
                .into_bytes(),
            key_pem: key.serialize_pem().into_bytes(),
            ca_pem: ca_pem.clone(),
        }
    };
    for scheme in ["tcp", "quic"] {
        let directory = tempfile::tempdir().unwrap();
        let (target_state, mut rx) = state(None);
        let mut target_config =
            config(directory.path()).with_mtls(credentials("recovery-target"), []);
        target_config.scheme = scheme;
        let source_config = if scheme == "tcp" {
            RemoteConfig::tcp("recovery-source", None)
        } else {
            RemoteConfig::quic("recovery-source", Some("127.0.0.1:0".parse().unwrap()))
        };
        let source = start(
            source_config.with_mtls(credentials("recovery-source"), []),
            state(None).0,
        )
        .await;
        let address = |node: &RemoteActorSystem| {
            if scheme == "tcp" {
                NodeAddr::tcp("recovery-target", node.local_addr().unwrap())
            } else {
                NodeAddr::quic("recovery-target", node.local_addr().unwrap())
            }
        };
        let old = start(target_config.clone(), target_state.clone()).await;
        source.connect(&address(&old)).await.unwrap();
        let reference = source
            .remote_ref("parrot://recovery-target/target")
            .unwrap();
        send(&reference, 1).await;
        seen(&mut rx, 1).await;
        old.shutdown().await.unwrap();
        let new = start(target_config, target_state).await;
        source.connect(&address(&new)).await.unwrap();
        send(&reference, 2).await;
        seen(&mut rx, 2).await;
        reference
            .close_ordered_session(Duration::from_secs(5))
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
        source.shutdown().await.unwrap();
        new.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn authorized_memory_relay_preserves_the_restart_proof_when_the_source_renews() {
    check_relay_restart(true).await;
}

#[tokio::test(start_paused = true)]
async fn routed_ordered_delivery_reaches_restarted_receiver_before_renewal() {
    check_relay_restart(false).await;
}

async fn check_relay_restart(wait_for_renewal: bool) {
    let directory = tempfile::tempdir().unwrap();
    let suffix = if wait_for_renewal {
        "renewed"
    } else {
        "cached"
    };
    let hub_id = format!("recovery-hub-{suffix}");
    let target_id = format!("recovery-target-{suffix}");
    let memory = |id: &str| {
        let mut config = RemoteConfig::mem(id);
        config.heartbeat_enabled = false;
        config
    };
    let target_config = memory(&target_id)
        .with_ordered_relays([hub_id.clone()])
        .with_ordered_receiver_state(OrderedReceiverState {
            directory: directory.path().into(),
            realm: "relay-recovery".into(),
        });
    let hub = start(
        memory(&hub_id).with_role(parrot_remote::handshake::TopologyRole::Hub),
        state(None).0,
    )
    .await;
    let source = start(memory(&format!("recovery-source-{suffix}")), state(None).0).await;
    source.connect_mem_pair(&hub).await.unwrap();
    let (target_state, mut rx) = state(None);
    let old = start(target_config.clone(), target_state.clone()).await;
    old.connect_mem_pair(&hub).await.unwrap();
    let reference = source
        .remote_ref(&format!("parrot://{target_id}/target"))
        .unwrap();
    send(&reference, 1).await;
    seen(&mut rx, 1).await;
    old.shutdown().await.unwrap();
    let new = start(target_config, target_state).await;
    new.connect_mem_pair(&hub).await.unwrap();
    if wait_for_renewal {
        tokio::time::advance(Duration::from_secs(16)).await;
    }
    send(&reference, 2).await;
    send(&reference, 3).await;
    seen(&mut rx, 2).await;
    seen(&mut rx, 3).await;
    reference
        .close_ordered_session(Duration::from_secs(5))
        .await
        .unwrap();
    for node in [&source, &hub, &new] {
        node.shutdown().await.unwrap();
    }
    assert!(rx.try_recv().is_err());
}
