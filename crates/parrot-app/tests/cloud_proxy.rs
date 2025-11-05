#![cfg(feature = "host")]
use bytes::Bytes;
use parrot::thread::ThreadActorSystem;
use parrot_api::{address::ActorRef, types::BoxedActorRef};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotSpawner},
    durable::*,
    host::{HostLookup, ThreadPropsSpawner},
    manifest::{AppManifest, ComponentSpec},
    planner::{plan, LocalTopology},
};
use parrot_remote::{
    durable::{DurableEnvelope, DurableReceiver, InboxLimits, TransactionalInbox, Wal},
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[path = "../../parrot-remote/tests/support/durable_timing.rs"]
mod diagnostic;

fn unique() -> String {
    diagnostic::optional_init();
    static N: AtomicUsize = AtomicUsize::new(0);
    format!(
        "proxy-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let p = std::env::temp_dir().join(unique());
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        diagnostic::optional_flush();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn config(s: &Scratch, receiver: String) -> CloudProxyConfig {
    CloudProxyConfig {
        directory: s.0.join("outbox"),
        receiver,
        path: "/counter".into(),
        session: "session".into(),
        attempt_timeout_ms: 100,
        retry_min_ms: 10,
        retry_max_ms: 30,
        drain_timeout_ms: 100,
        ..Default::default()
    }
}
fn manifest(config: &CloudProxyConfig) -> AppManifest {
    let mut m: AppManifest = toml::from_str("name='proxy-app'\nversion='1.0.0'\n[[components]]\nname='proxy'\nengine='parrot'\n[components.artifact.Props]\nfactory='parrot.cloud-proxy'\n[components.hooks]\non_start='.'\non_stop='.'\n").unwrap();
    m.components[0].config = Some(
        toml::Value::try_from(config)
            .unwrap()
            .as_table()
            .unwrap()
            .clone(),
    );
    m
}
struct NoGateway;
#[async_trait::async_trait]
impl GatewayFactory for NoGateway {
    async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        Ok(())
    }
    async fn start_gateway(
        &self,
        _: &ComponentSpec,
        _: &[String],
        _: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        panic!("no external gateway")
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        panic!("no external gateway")
    }
}
async fn source(id: &str) -> (Arc<ThreadActorSystem>, Arc<RemoteActorSystem>) {
    let engine = ThreadActorSystem::shared(Default::default());
    let remote = RemoteActorSystem::new(
        RemoteConfig::mem(id),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    remote.start().await.unwrap();
    (engine, remote)
}
fn submit(n: u64) -> CloudProxySubmit {
    CloudProxySubmit {
        type_key: "bin:counter.add#v1".into(),
        payload: n.to_le_bytes().to_vec().into(),
    }
}
async fn accepted(actor: &dyn ActorRef, n: u64) -> u64 {
    actor
        .send_with_timeout(Box::new(submit(n)), Some(Duration::from_secs(5)))
        .await
        .unwrap()
        .downcast::<CloudProxyAccepted>()
        .unwrap()
        .sequence
}
async fn status(actor: &dyn ActorRef) -> CloudProxyStatus {
    *actor
        .send_with_timeout(Box::new(CloudProxyInspect), Some(Duration::from_secs(5)))
        .await
        .unwrap()
        .downcast::<CloudProxyStatus>()
        .unwrap()
}
async fn acked(actor: &dyn ActorRef, seq: u64) {
    wait_ack_progress(actor, seq, Duration::from_secs(10))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
}
async fn wait_ack_progress(
    actor: &dyn ActorRef,
    seq: u64,
    budget: Duration,
) -> Result<CloudProxyStatus, String> {
    let mut previous = status(actor).await;
    if previous.last_enqueued < seq {
        return Err(format!("ACK {seq} was never accepted: {previous:?}"));
    }
    let remaining = u32::try_from(seq.saturating_sub(previous.last_acked))
        .map_err(|_| "test workload exceeds the bounded ACK watchdog".to_string())?;
    // Bound stalled delivery and total work separately; retries alone are not progress.
    let started = tokio::time::Instant::now();
    let total_deadline = started + budget.saturating_mul(remaining.max(1));
    let mut progress_deadline = started + budget;
    loop {
        let current =
            match tokio::time::timeout_at(progress_deadline.min(total_deadline), status(actor))
                .await
            {
                Ok(current) => current,
                Err(_) => {
                    let reason = if progress_deadline <= total_deadline {
                        "no durable progress"
                    } else {
                        "exhausted the total workload budget"
                    };
                    return Err(format!(
                        "ACK {seq}: {reason}; per-record budget {budget:?}: {previous:?}"
                    ));
                }
            };
        if current.last_acked < previous.last_acked
            || current.last_enqueued < previous.last_enqueued
            || current.last_enqueued.checked_sub(current.last_acked) != Some(current.pending)
            || current.terminal_error.is_some()
        {
            return Err(format!(
                "invalid ACK transition: {previous:?} -> {current:?}"
            ));
        }
        if current.last_acked >= seq {
            return Ok(current);
        }
        if current.closed {
            return Err(format!("outbox closed before ACK {seq}: {current:?}"));
        }
        if current.last_acked > previous.last_acked {
            progress_deadline = tokio::time::Instant::now() + budget;
        }
        previous = current;
        tokio::time::sleep_until(
            (tokio::time::Instant::now() + Duration::from_millis(10))
                .min(progress_deadline)
                .min(total_deadline),
        )
        .await;
    }
}
struct Receiver(parrot_remote::durable::InboxHandle);
#[async_trait::async_trait]
impl LocalLookup for Receiver {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
    fn durable_receiver(&self, p: &str) -> Option<Arc<dyn DurableReceiver>> {
        (p == "/counter").then(|| Arc::new(self.0.clone()) as Arc<dyn DurableReceiver>)
    }
}
struct HeldReceiver {
    inbox: parrot_remote::durable::InboxHandle,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::watch::Receiver<bool>,
}
#[async_trait::async_trait]
impl DurableReceiver for HeldReceiver {
    async fn receive(
        &self,
        message: DurableEnvelope,
    ) -> Result<(), parrot_remote::durable::InboxError> {
        self.entered.add_permits(1);
        self.release
            .clone()
            .wait_for(|released| *released)
            .await
            .unwrap();
        self.inbox.apply(message).await
    }
}
struct HeldLookup(Arc<HeldReceiver>);
#[async_trait::async_trait]
impl LocalLookup for HeldLookup {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
    fn durable_receiver(&self, path: &str) -> Option<Arc<dyn DurableReceiver>> {
        (path == "/counter").then(|| self.0.clone() as Arc<dyn DurableReceiver>)
    }
}

#[tokio::test]
async fn ack_watchdog_rejects_active_retries_without_progress_then_observes_exact_delivery() {
    let s = Scratch::new();
    let target_id = unique();
    let (engine, remote) = source(&unique()).await;
    let actor = parrot_app::host::cloud_proxy::CloudProxy::spawn(
        &engine,
        &remote,
        "/proxy",
        config(&s, format!("mem://{target_id}")),
    )
    .await
    .unwrap();
    assert!(
        wait_ack_progress(actor.as_ref(), 1, Duration::from_millis(250))
            .await
            .unwrap_err()
            .contains("never accepted")
    );
    assert_eq!(accepted(actor.as_ref(), 10).await, 1);
    assert_eq!(accepted(actor.as_ref(), 20).await, 2);
    let mut inbox = TransactionalInbox::open(
        s.0.join("inbox"),
        &target_id,
        "/counter",
        Bytes::new(),
        InboxLimits::default(),
        remember,
    )
    .await
    .unwrap();
    let (release, held) = tokio::sync::watch::channel(false);
    let receiver = Arc::new(HeldReceiver {
        inbox: inbox.handle(),
        entered: tokio::sync::Semaphore::new(0),
        release: held,
    });
    let edge = RemoteActorSystem::new(
        RemoteConfig::mem(&target_id),
        Arc::new(HeldLookup(receiver.clone())),
    )
    .unwrap();
    edge.start().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), receiver.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let before = status(actor.as_ref()).await;
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        wait_ack_progress(actor.as_ref(), 2, Duration::from_millis(250)),
    )
    .await
    .expect("retry activity incorrectly kept the ACK watchdog alive")
    .unwrap_err();
    assert!(error.contains("no durable progress"), "{error}");
    let stalled = status(actor.as_ref()).await;
    assert!(stalled.attempts > before.attempts);
    assert_eq!((stalled.last_acked, stalled.pending), (0, 2));
    assert_eq!(inbox.handle().snapshot().revision, 0);
    release.send(true).unwrap();
    acked(actor.as_ref(), 2).await;
    let final_state = inbox.handle().snapshot();
    assert_eq!(history(&final_state.state), vec![10, 20]);
    assert_eq!(final_state.revision, 2);
    assert_eq!(status(actor.as_ref()).await.pending, 0);
    actor.stop().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    edge.shutdown().await.unwrap();
    inbox.close().await.unwrap();
}
fn add(state: &[u8], message: &DurableEnvelope) -> Result<Bytes, String> {
    if message.type_key != "bin:counter.add#v1" || message.payload.len() != 8 {
        return Err("unknown business request".into());
    }
    let old = u64::from_le_bytes(state.try_into().unwrap());
    Ok(
        (old + u64::from_le_bytes(message.payload.as_ref().try_into().unwrap()))
            .to_le_bytes()
            .to_vec()
            .into(),
    )
}

fn remember(state: &[u8], message: &DurableEnvelope) -> Result<Bytes, String> {
    if message.type_key != "bin:counter.add#v1" || message.payload.len() != 8 {
        return Err("unknown business request".into());
    }
    let mut next = state.to_vec();
    next.extend_from_slice(&message.payload);
    Ok(next.into())
}
fn history(state: &[u8]) -> Vec<u64> {
    let (values, tail) = state.as_chunks::<8>();
    assert!(tail.is_empty());
    values.iter().copied().map(u64::from_le_bytes).collect()
}

#[test]
fn manifest_rejects_bad_proxy_config_and_ambiguous_stream_ownership() {
    let example: AppManifest =
        toml::from_str(include_str!("../examples/cloud-proxy.app.toml")).unwrap();
    assert!(parrot_app::validate(&example).is_ok());
    let s = Scratch::new();
    let c = config(&s, "mem://edge".into());
    assert!(parrot_app::validate(&manifest(&c)).is_ok());
    for field in [
        "mailbox_capacity",
        "max_inflight_bytes",
        "max_request_bytes",
        "max_submission_receipts",
        "max_submission_receipt_bytes",
        "max_pending_records",
        "max_pending_bytes",
        "max_disk_bytes",
        "group_commit_max_records",
        "attempt_timeout_ms",
        "retry_min_ms",
        "drain_timeout_ms",
    ] {
        let mut m = manifest(&c);
        m.components[0]
            .config
            .as_mut()
            .unwrap()
            .insert(field.into(), 0.into());
        assert!(parrot_app::validate(&m).is_err(), "{field}");
    }
    let mut m = manifest(&c);
    m.components[0].instances = parrot_app::InstancePolicy::Pool(2);
    assert!(parrot_app::validate(&m).is_err());
    let mut m = manifest(&c);
    m.components[0]
        .config
        .as_mut()
        .unwrap()
        .insert("typo".into(), true.into());
    assert!(parrot_app::validate(&m).is_err());
    for (field, value) in [
        ("directory", "relative"),
        ("receiver", "127.0.0.1:1234"),
        ("receiver", "mem://bad node"),
        ("session", ""),
        ("path", "relative"),
    ] {
        let mut m = manifest(&c);
        m.components[0]
            .config
            .as_mut()
            .unwrap()
            .insert(field.into(), value.into());
        assert!(parrot_app::validate(&m).is_err(), "{field}");
    }
    assert!(!c.directory.exists());
    let mut invalid = c.clone();
    invalid.group_commit_interval_ms = 1001;
    assert!(parrot_app::validate(&manifest(&invalid)).is_err());
    invalid.group_commit_interval_ms = 0;
    assert!(parrot_app::validate(&manifest(&invalid)).is_ok());
    for (request, total) in [
        (191, 1024),
        (1025, 1024),
        (32 * 1024 * 1024 + 1, 64 * 1024 * 1024),
        (512, 1024 * 1024 * 1024 + 1),
    ] {
        let mut bad = c.clone();
        bad.max_request_bytes = request;
        bad.max_inflight_bytes = total;
        assert!(parrot_app::validate(&manifest(&bad)).is_err());
    }
    invalid.group_commit_max_records = 65537;
    assert!(parrot_app::validate(&manifest(&invalid)).is_err());
    let mut bounded = c.clone();
    bounded.max_inflight_bytes = (1024 * 1024 * 1024).min(tokio::sync::Semaphore::MAX_PERMITS);
    assert!(bounded.validate().is_ok());
    bounded.max_inflight_bytes += 1;
    assert!(bounded.validate().is_err());
}

#[tokio::test]
async fn proxy_transport_mismatch_fails_assembly_before_storage_and_allows_corrected_retry() {
    let s = Scratch::new();
    let (engine, remote) = source(&unique()).await;
    let mut c = config(&s, "tcp://peer@127.0.0.1:12345".into());
    let spawner = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    let m = manifest(&c);
    let p = plan(&m, &LocalTopology).unwrap();
    let result = ctx.assemble(&p, &deployer, &m, &Default::default()).await;
    // Clean up even the old implementation, so the negative probe leaks no actor.
    ctx.teardown(&deployer).await.unwrap();
    assert!(
        result.is_err(),
        "mem host must reject a TCP proxy destination during assembly"
    );
    assert!(!c.directory.exists());
    c.receiver = "mem://peer".into();
    let m = manifest(&c);
    let p = plan(&m, &LocalTopology).unwrap();
    ctx.assemble(&p, &deployer, &m, &Default::default())
        .await
        .unwrap();
    assert_eq!(
        accepted(ctx.component_ref("proxy").unwrap().as_ref(), 7).await,
        1
    );
    ctx.teardown(&deployer).await.unwrap();
    assert_eq!(Wal::open(&c.directory).unwrap().pending_len().unwrap(), 1);
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn manifest_proxy_persists_offline_and_reassembles_into_transactional_delivery() {
    let s = Scratch::new();
    let id = unique();
    let target_id = unique();
    let c = config(&s, format!("mem://{target_id}"));
    let m = manifest(&c);
    let p = plan(&m, &LocalTopology).unwrap();
    let (engine, remote) = source(&id).await;
    let spawner = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    ctx.assemble(&p, &deployer, &m, &Default::default())
        .await
        .unwrap();
    let actor = ctx.component_ref("proxy").unwrap();
    for n in 1..=100 {
        assert_eq!(accepted(actor.as_ref(), n).await, n);
    }
    let view = status(actor.as_ref()).await;
    assert_eq!(view.pending, 100);
    assert_eq!(view.last_acked, 0);
    ctx.teardown(&deployer).await.unwrap();
    assert_eq!(Wal::open(&c.directory).unwrap().pending_len().unwrap(), 100);
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    let mut inbox = TransactionalInbox::open(
        s.0.join("inbox"),
        &target_id,
        "/counter",
        Bytes::new(),
        InboxLimits::default(),
        remember,
    )
    .await
    .unwrap();
    let edge = RemoteActorSystem::new(
        RemoteConfig::mem(&target_id),
        Arc::new(Receiver(inbox.handle())),
    )
    .unwrap();
    edge.start().await.unwrap();
    let (engine, remote) = source(&id).await;
    let spawner = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    ctx.assemble(&p, &deployer, &m, &Default::default())
        .await
        .unwrap();
    let actor = ctx.component_ref("proxy").unwrap();
    acked(actor.as_ref(), 100).await;
    let state = inbox.handle().snapshot();
    assert_eq!(state.revision, 100);
    assert_eq!(history(&state.state), (1..=100).collect::<Vec<_>>());
    assert_eq!(accepted(actor.as_ref(), 101).await, 101);
    acked(actor.as_ref(), 101).await;
    let drained = actor
        .send(Box::new(CloudProxyDrain))
        .await
        .unwrap()
        .downcast::<CloudProxyStatus>()
        .unwrap();
    assert!(drained.draining);
    assert_eq!(drained.pending, 0);
    assert!(actor.send(Box::new(submit(9))).await.is_err());
    ctx.teardown(&deployer).await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    edge.shutdown().await.unwrap();
    inbox.close().await.unwrap();
}

#[tokio::test]
async fn failed_drain_closes_admission_and_stop_preserves_pending_work() {
    let s = Scratch::new();
    let c = config(&s, format!("mem://{}", unique()));
    let (engine, remote) = source(&unique()).await;
    let actor =
        parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, "/proxy", c.clone())
            .await
            .unwrap();
    assert_eq!(accepted(actor.as_ref(), 7).await, 1);
    assert!(actor.send(Box::new(CloudProxyDrain)).await.is_err());
    let v = status(actor.as_ref()).await;
    assert!(v.draining);
    assert_eq!(v.last_acked, 0);
    assert_eq!(v.pending, 1);
    assert!(actor.send(Box::new(submit(8))).await.is_err());
    actor.stop().await.unwrap();
    assert_eq!(Wal::open(&c.directory).unwrap().pending_len().unwrap(), 1);
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn factory_requires_remote_and_rolls_back_started_proxy_on_later_failure() {
    let s = Scratch::new();
    let c = config(&s, format!("mem://{}", unique()));
    let mut m = manifest(&c);
    let (engine, remote) = source(&unique()).await;
    assert!(ThreadPropsSpawner::new(engine.clone())
        .spawn_props(
            CLOUD_PROXY_FACTORY,
            &["/proxy".into()],
            m.components[0].config.as_ref()
        )
        .await
        .is_err());
    assert!(!c.directory.exists());
    let mut broken = m.components[0].clone();
    broken.name = "broken".into();
    broken.deps = vec!["proxy".into()];
    broken.artifact = parrot_app::ArtifactRef::Props {
        factory: "missing.factory".into(),
    };
    m.components.push(broken);
    let p = plan(&m, &LocalTopology).unwrap();
    let spawner = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    assert!(ctx
        .assemble(&p, &deployer, &m, &Default::default())
        .await
        .is_err());
    assert!(ctx.started().is_empty());
    assert!(engine.get_actor_ref("/user/proxy").is_none());
    assert!(Wal::open(&c.directory).is_ok());
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[cfg(unix)]
async fn launch_cli(s: &Scratch, m: &AppManifest) -> (tokio::process::Child, std::net::SocketAddr) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let manifest_path = s.0.join("app.toml");
    std::fs::write(&manifest_path, toml::to_string(m).unwrap()).unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_parrot-app"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("PARROT_") {
            command.env_remove(key);
        }
    }
    let mut child = command
        .args(["run", "--bind", "127.0.0.1:0", "--manifest"])
        .arg(manifest_path)
        .current_dir(&s.0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let ready = async {
        let mut reader = tokio::io::BufReader::new(stdout.take(65537));
        loop {
            let mut line = String::new();
            assert_ne!(
                reader.read_line(&mut line).await.unwrap(),
                0,
                "child exited or exhausted readiness output budget"
            );
            if let Some(addr) = line.strip_prefix("PARROT_APP_READY=") {
                break addr.trim().parse().unwrap();
            }
        }
    };
    let address = match tokio::time::timeout(Duration::from_secs(120), ready).await {
        Ok(addr) => addr,
        Err(_) => {
            child.kill().await.unwrap();
            child.wait().await.unwrap();
            panic!("CLI readiness deadline");
        }
    };
    (child, address)
}

#[cfg(unix)]
#[tokio::test]
async fn stock_cli_sigkill_and_restart_replays_offline_work_and_graceful_stop_releases_storage() {
    let s = Scratch::new();
    let edge_id = unique();
    let cloud_id = unique();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    drop(socket);
    let c = config(&s, format!("tcp://{edge_id}@{address}"));
    let mut m = manifest(&c);
    m.config_overlay =
        Some(toml::from_str(&format!("[remote.node]\nnode_id='{cloud_id}'")).unwrap());
    let (mut child, address1) = launch_cli(&s, &m).await;
    let client =
        RemoteActorSystem::new(RemoteConfig::tcp(unique(), None), Arc::new(Empty)).unwrap();
    client.start().await.unwrap();
    client
        .connect(&NodeAddr::tcp(&cloud_id, address1))
        .await
        .unwrap();
    let actor = client
        .remote_ref(&format!("parrot://{cloud_id}/user/proxy"))
        .unwrap();
    assert_eq!(accepted(&actor, 7).await, 1);
    assert_eq!(status(&actor).await.pending, 1);
    child.kill().await.unwrap();
    let killed = child.wait().await.unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(killed.signal(), Some(libc::SIGKILL));
    client.shutdown().await.unwrap();
    assert_eq!(Wal::open(&c.directory).unwrap().pending_len().unwrap(), 1);
    let mut inbox = TransactionalInbox::open(
        s.0.join("inbox"),
        &edge_id,
        "/counter",
        0u64.to_le_bytes().to_vec().into(),
        InboxLimits::default(),
        add,
    )
    .await
    .unwrap();
    let edge = RemoteActorSystem::new(
        RemoteConfig::tcp(&edge_id, Some(address)).with_insecure_development(),
        Arc::new(Receiver(inbox.handle())),
    )
    .unwrap();
    edge.start().await.unwrap();
    let (mut child, address2) = launch_cli(&s, &m).await;
    let client =
        RemoteActorSystem::new(RemoteConfig::tcp(unique(), None), Arc::new(Empty)).unwrap();
    client.start().await.unwrap();
    client
        .connect(&NodeAddr::tcp(&cloud_id, address2))
        .await
        .unwrap();
    let actor = client
        .remote_ref(&format!("parrot://{cloud_id}/user/proxy"))
        .unwrap();
    acked(&actor, 1).await;
    assert_eq!(accepted(&actor, 8).await, 2);
    acked(&actor, 2).await;
    let view = inbox.handle().snapshot();
    assert_eq!(view.revision, 2);
    assert_eq!(
        u64::from_le_bytes(view.state.as_ref().try_into().unwrap()),
        15
    );
    assert!(tokio::process::Command::new("kill")
        .args(["-TERM", &child.id().unwrap().to_string()])
        .status()
        .await
        .unwrap()
        .success());
    let status = match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
        Ok(s) => s.unwrap(),
        Err(_) => {
            child.kill().await.unwrap();
            child.wait().await.unwrap();
            panic!("graceful shutdown deadline");
        }
    };
    assert!(status.success());
    assert_eq!(Wal::open(&c.directory).unwrap().pending_len().unwrap(), 0);
    client.shutdown().await.unwrap();
    edge.shutdown().await.unwrap();
    inbox.close().await.unwrap();
}

struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}

struct Business(u64);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:counter.add#v1", type_id: std::any::TypeId::of::<Business>(),
        encode: |m| Ok(m.downcast_ref::<Business>().ok_or("business type")?.0.to_le_bytes().to_vec()),
        decode: |b| Ok(Box::new(Business(u64::from_le_bytes(b.try_into().map_err(|_| "business length")?))) as parrot_api::types::BoxedMessage),
    }
}

#[tokio::test]
async fn proxy_rejections_preserve_order_and_registered_business_messages_use_original_codec() {
    let s = Scratch::new();
    let target_id = unique();
    let mut c = config(&s, format!("mem://{target_id}"));
    c.max_pending_records = 1;
    let (engine, remote) = source(&unique()).await;
    let actor =
        parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, "/proxy", c.clone())
            .await
            .unwrap();
    assert!(actor.send(Box::new(())).await.is_err());
    assert!(actor
        .send(Box::new(CloudProxySubmit {
            type_key: parrot_remote::durable::DURABLE_KEY.into(),
            payload: Bytes::new()
        }))
        .await
        .is_err());
    assert_eq!(status(actor.as_ref()).await.last_enqueued, 0);
    actor.deliver(Box::new(Business(4))).await.unwrap();
    assert_eq!(status(actor.as_ref()).await.last_enqueued, 1);
    assert!(actor.send(Box::new(Business(5))).await.is_err());
    let view = status(actor.as_ref()).await;
    assert_eq!(view.last_enqueued, 1);
    assert_eq!(view.pending, 1);
    let mut inbox = TransactionalInbox::open(
        s.0.join("inbox"),
        &target_id,
        "/counter",
        0u64.to_le_bytes().to_vec().into(),
        InboxLimits::default(),
        add,
    )
    .await
    .unwrap();
    let edge = RemoteActorSystem::new(
        RemoteConfig::mem(&target_id),
        Arc::new(Receiver(inbox.handle())),
    )
    .unwrap();
    edge.start().await.unwrap();
    acked(actor.as_ref(), 1).await;
    let accepted = actor
        .send(Box::new(Business(5)))
        .await
        .unwrap()
        .downcast::<CloudProxyAccepted>()
        .unwrap();
    assert_eq!(accepted.sequence, 2);
    acked(actor.as_ref(), 2).await;
    let reply = actor
        .send(Box::new(parrot_app::ParrotAppHook(
            parrot_app::hook_phase::DRAIN,
        )))
        .await
        .unwrap()
        .downcast::<parrot_app::ParrotAppHook>()
        .unwrap();
    assert_eq!(reply.0, parrot_app::hook_phase::DRAIN);
    assert!(actor.send(Box::new(Business(6))).await.is_err());
    assert_eq!(inbox.handle().snapshot().revision, 2);
    assert_eq!(
        u64::from_le_bytes(inbox.handle().snapshot().state.as_ref().try_into().unwrap()),
        9
    );
    actor.stop().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    edge.shutdown().await.unwrap();
    inbox.close().await.unwrap();
}

#[tokio::test]
#[ignore = "five-minute real TCP disconnect with continuous durable production; run explicitly with --ignored --nocapture"]
async fn cloud_proxy_offline_five_minutes_reconnects_without_loss_or_duplicate_state() {
    let s = Scratch::new();
    let target_id = unique();
    let inbox_dir = s.0.join("inbox");
    let mut inbox = TransactionalInbox::open(
        &inbox_dir,
        &target_id,
        "/counter",
        Bytes::new(),
        InboxLimits::default(),
        remember,
    )
    .await
    .unwrap();
    let edge = RemoteActorSystem::new(
        RemoteConfig::tcp(&target_id, Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development(),
        Arc::new(Receiver(inbox.handle())),
    )
    .unwrap();
    edge.start().await.unwrap();
    let address = edge.local_addr().unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    let remote = RemoteActorSystem::new(
        RemoteConfig::tcp(unique(), None),
        Arc::new(HostLookup { ts: engine.clone() }),
    )
    .unwrap();
    remote.start().await.unwrap();
    let c = config(&s, format!("tcp://{target_id}@{address}"));
    let m = manifest(&c);
    let p = plan(&m, &LocalTopology).unwrap();
    let spawner = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    ctx.assemble(&p, &deployer, &m, &Default::default())
        .await
        .unwrap();
    let actor = ctx.component_ref("proxy").unwrap();
    assert_eq!(accepted(actor.as_ref(), 0).await, 1);
    acked(actor.as_ref(), 1).await;
    edge.shutdown().await.unwrap();
    inbox.close().await.unwrap();
    let start = tokio::time::Instant::now();
    for n in 1..=100 {
        tokio::time::sleep_until(start + Duration::from_secs(n * 3)).await;
        assert_eq!(accepted(actor.as_ref(), n).await, n + 1);
        let v = status(actor.as_ref()).await;
        assert_eq!(v.last_acked, 1);
        assert_eq!(v.pending, n);
        if n % 20 == 0 {
            eprintln!("offline production: {n}/100, elapsed={:?}", start.elapsed());
        }
    }
    assert!(start.elapsed() >= Duration::from_secs(300));
    let mut inbox = TransactionalInbox::open(
        &inbox_dir,
        &target_id,
        "/counter",
        Bytes::new(),
        InboxLimits::default(),
        remember,
    )
    .await
    .unwrap();
    let edge = RemoteActorSystem::new(
        RemoteConfig::tcp(&target_id, Some(address)).with_insecure_development(),
        Arc::new(Receiver(inbox.handle())),
    )
    .unwrap();
    edge.start().await.unwrap();
    acked(actor.as_ref(), 101).await;
    let view = inbox.handle().snapshot();
    assert_eq!(view.revision, 101);
    assert_eq!(history(&view.state), (0..=100).collect::<Vec<_>>());
    assert_eq!(status(actor.as_ref()).await.pending, 0);
    ctx.teardown(&deployer).await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    edge.shutdown().await.unwrap();
    inbox.close().await.unwrap();
    let wal = Wal::open(&c.directory).unwrap();
    assert_eq!(wal.pending_len().unwrap(), 0);
}

#[tokio::test]
async fn wire_batch_reports_each_outcome_and_replays_only_accepted_items_in_order() {
    let s = Scratch::new();
    let target_id = unique();
    let mut c = config(&s, format!("mem://{target_id}"));
    c.max_pending_records = 2;
    c.group_commit_max_records = 8;
    c.group_commit_interval_ms = 20;
    let m = manifest(&c);
    parrot_app::validate(&m).unwrap();
    assert_eq!(
        CloudProxyConfig::from_table(m.components[0].config.as_ref())
            .unwrap()
            .outbox_config()
            .group_commit_interval,
        Duration::from_millis(20)
    );
    let (engine, remote) = source(&unique()).await;
    let actor =
        parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, "/proxy", c.clone())
            .await
            .unwrap();
    for messages in [
        vec![],
        vec![submit(1); CLOUD_PROXY_MAX_BATCH_RECORDS + 1],
        vec![CloudProxySubmit {
            type_key: "key".into(),
            payload: Bytes::from(vec![0; 16 * 1024 * 1024]),
        }],
    ] {
        assert!(actor
            .send(Box::new(CloudProxySubmitBatch { messages }))
            .await
            .is_err());
        assert_eq!(status(actor.as_ref()).await.last_enqueued, 0);
    }
    let (client_engine, client) = source(&unique()).await;
    client
        .connect(&NodeAddr::mem(&remote.config.node_id))
        .await
        .unwrap();
    let wire = client
        .remote_ref(&format!("parrot://{}/proxy", remote.config.node_id))
        .unwrap();
    let result = wire
        .send_with_timeout(
            Box::new(CloudProxySubmitBatch {
                messages: vec![
                    submit(10),
                    CloudProxySubmit {
                        type_key: "#ack".into(),
                        payload: Bytes::new(),
                    },
                    submit(20),
                    submit(30),
                ],
            }),
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap()
        .downcast::<CloudProxyBatchResult>()
        .unwrap();
    assert_eq!(result.outcomes.len(), 4);
    assert_eq!(result.outcomes[0].as_ref().unwrap().sequence, 1);
    assert!(result.outcomes[1].is_err());
    assert_eq!(result.outcomes[2].as_ref().unwrap().sequence, 2);
    assert!(result.outcomes[3].is_err());
    assert_eq!(status(actor.as_ref()).await.pending, 2);
    actor.stop().await.unwrap();
    let mut inbox = TransactionalInbox::open(
        s.0.join("inbox"),
        &target_id,
        "/counter",
        Bytes::new(),
        InboxLimits::default(),
        remember,
    )
    .await
    .unwrap();
    let edge = RemoteActorSystem::new(
        RemoteConfig::mem(&target_id),
        Arc::new(Receiver(inbox.handle())),
    )
    .unwrap();
    edge.start().await.unwrap();
    let actor = parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, "/proxy", c)
        .await
        .unwrap();
    acked(actor.as_ref(), 2).await;
    assert_eq!(history(&inbox.handle().snapshot().state), vec![10, 20]);
    assert_eq!(accepted(actor.as_ref(), 30).await, 3);
    acked(actor.as_ref(), 3).await;
    assert_eq!(history(&inbox.handle().snapshot().state), vec![10, 20, 30]);
    actor.send(Box::new(CloudProxyDrain)).await.unwrap();
    assert!(actor
        .send(Box::new(CloudProxySubmitBatch {
            messages: vec![submit(40)]
        }))
        .await
        .is_err());
    actor.stop().await.unwrap();
    client.shutdown().await.unwrap();
    client_engine.shutdown_internal().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    edge.shutdown().await.unwrap();
    inbox.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_local_and_wire_requests_automatically_share_syncs_and_recover_exact_history() {
    let s = Scratch::new();
    let (engine, remote) = source(&unique()).await;
    let (client_engine, client) = source(&unique()).await;
    client
        .connect(&NodeAddr::mem(&remote.config.node_id))
        .await
        .unwrap();
    for wire in [false, true] {
        for max_records in [1, 64] {
            let target_id = unique();
            let mut c = config(&s, format!("mem://{target_id}"));
            c.directory = s.0.join(format!("{wire}-{max_records}"));
            c.group_commit_max_records = max_records;
            c.group_commit_interval_ms = 100;
            let path = format!("/proxy-{wire}-{max_records}");
            let actor = parrot_app::host::cloud_proxy::CloudProxy::spawn(
                &engine,
                &remote,
                &path,
                c.clone(),
            )
            .await
            .unwrap();
            let endpoint: BoxedActorRef = if wire {
                Box::new(
                    client
                        .remote_ref(&format!("parrot://{}{path}", remote.config.node_id))
                        .unwrap(),
                )
            } else {
                actor.clone_boxed()
            };
            let mut values: Vec<_> = (1..=32u64).map(Some).collect();
            values.insert(16, None);
            let calls = values.iter().map(|value| {
                let message = value.map(submit).unwrap_or(CloudProxySubmit {
                    type_key: "#ack".into(),
                    payload: Bytes::new(),
                });
                endpoint.send_with_timeout(Box::new(message), Some(Duration::from_secs(10)))
            });
            let mut sequence = Vec::new();
            for (value, result) in values.iter().zip(futures::future::join_all(calls).await) {
                if let Some(value) = value {
                    let accepted = result.unwrap().downcast::<CloudProxyAccepted>().unwrap();
                    sequence.push((accepted.sequence, *value));
                } else {
                    assert!(result.is_err());
                }
            }
            sequence.sort_unstable();
            assert_eq!(
                sequence.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
                (1..=32).collect::<Vec<_>>()
            );
            let expected: Vec<_> = sequence.iter().map(|(_, value)| *value).collect();
            let stats = endpoint
                .send_with_timeout(
                    Box::new(CloudProxyInspectStorage),
                    Some(Duration::from_secs(5)),
                )
                .await
                .unwrap()
                .downcast::<CloudProxyStorageStats>()
                .unwrap();
            if max_records == 1 {
                assert_eq!(stats.acceptance_syncs, 32);
            } else {
                assert!(
                    stats.acceptance_syncs < 32,
                    "ordinary requests never coalesced: wire={wire}, syncs={}",
                    stats.acceptance_syncs
                );
            }
            assert_eq!(status(actor.as_ref()).await.last_acked, 0);
            actor.stop().await.unwrap();
            assert_eq!(Wal::open(&c.directory).unwrap().pending_len().unwrap(), 32);
            let mut inbox = TransactionalInbox::open(
                s.0.join(format!("inbox-{wire}-{max_records}")),
                &target_id,
                "/counter",
                Bytes::new(),
                InboxLimits::default(),
                remember,
            )
            .await
            .unwrap();
            let edge = RemoteActorSystem::new(
                RemoteConfig::mem(&target_id),
                Arc::new(Receiver(inbox.handle())),
            )
            .unwrap();
            edge.start().await.unwrap();
            let actor =
                parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, &path, c)
                    .await
                    .unwrap();
            acked(actor.as_ref(), 32).await;
            assert_eq!(history(&inbox.handle().snapshot().state), expected);
            assert_eq!(inbox.handle().snapshot().revision, 32);
            actor.stop().await.unwrap();
            edge.shutdown().await.unwrap();
            inbox.close().await.unwrap();
        }
    }
    client.shutdown().await.unwrap();
    client_engine.shutdown_internal().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn queued_drain_is_a_batch_barrier_and_cannot_acknowledge_later_business() {
    let s = Scratch::new();
    let mut c = config(&s, format!("mem://{}", unique()));
    c.group_commit_interval_ms = 100;
    let (engine, remote) = source(&unique()).await;
    let actor =
        parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, "/proxy", c.clone())
            .await
            .unwrap();
    let mut first = actor.send_with_timeout(Box::new(submit(10)), Some(Duration::from_secs(5)));
    let mut second = actor.send_with_timeout(Box::new(submit(20)), Some(Duration::from_secs(5)));
    let mut inspect =
        actor.send_with_timeout(Box::new(CloudProxyInspect), Some(Duration::from_secs(5)));
    let mut drain =
        actor.send_with_timeout(Box::new(CloudProxyDrain), Some(Duration::from_secs(5)));
    let mut after = actor.send_with_timeout(Box::new(submit(30)), Some(Duration::from_secs(5)));
    for future in [
        &mut first,
        &mut second,
        &mut inspect,
        &mut drain,
        &mut after,
    ] {
        assert!(futures::poll!(future).is_pending());
    }
    let (a, b, view, drain, after) = tokio::join!(first, second, inspect, drain, after);
    assert_eq!(
        a.unwrap()
            .downcast::<CloudProxyAccepted>()
            .unwrap()
            .sequence,
        1
    );
    assert_eq!(
        b.unwrap()
            .downcast::<CloudProxyAccepted>()
            .unwrap()
            .sequence,
        2
    );
    let view = view.unwrap().downcast::<CloudProxyStatus>().unwrap();
    assert_eq!(view.last_enqueued, 2);
    assert!(!view.draining);
    assert!(drain.is_err());
    assert!(after.is_err());
    let view = status(actor.as_ref()).await;
    assert!(view.draining);
    assert_eq!(view.last_enqueued, 2);
    assert_eq!(view.pending, 2);
    actor.stop().await.unwrap();
    let wal = Wal::open(&c.directory).unwrap();
    let target: NodeAddr = c.receiver.parse().unwrap();
    assert_eq!(
        wal.replay(&target.node_id)
            .unwrap()
            .iter()
            .map(|record| u64::from_le_bytes(record.payload.as_ref().try_into().unwrap()))
            .collect::<Vec<_>>(),
        vec![10, 20]
    );
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn oversized_proxy_delivery_is_rejected_before_mailbox_acceptance() {
    let s = Scratch::new();
    let (engine, remote) = source(&unique()).await;
    let c = config(&s, format!("mem://{}", unique()));
    let actor =
        parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, "/proxy", c.clone())
            .await
            .unwrap();
    let result = actor
        .deliver(Box::new(CloudProxySubmit {
            type_key: "bin:oversize#v1".into(),
            payload: Bytes::from(vec![7; 20 * 1024 * 1024]),
        }))
        .await;
    let view = status(actor.as_ref()).await;
    actor.stop().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    assert!(
        result.is_err(),
        "oversize TELL was acknowledged as admitted to the proxy mailbox"
    );
    assert_eq!(view.last_enqueued, 0);
    assert_eq!(Wal::open(&c.directory).unwrap().pending_len().unwrap(), 0);
}

#[tokio::test]
async fn aggregate_admission_bounds_a_real_proxy_backlog_and_drain_rejects_queued_business() {
    let s = Scratch::new();
    let (engine, remote) = source(&unique()).await;
    let mut c = config(&s, format!("mem://{}", unique()));
    let charge =
        CLOUD_PROXY_REQUEST_OVERHEAD + CLOUD_PROXY_RECORD_OVERHEAD + submit(0).type_key.len() + 8;
    c.max_request_bytes = charge;
    c.max_inflight_bytes = charge * 2;
    c.drain_timeout_ms = 60_000;
    let actor =
        parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, "/proxy", c.clone())
            .await
            .unwrap();
    assert_eq!(accepted(actor.as_ref(), 1).await, 1);
    // The offline tail holds Drain until the receiver starts, keeping later
    // business in the mailbox without any timing-based producer race.
    let mut drain =
        actor.send_with_timeout(Box::new(CloudProxyDrain), Some(Duration::from_secs(60)));
    assert!(futures::poll!(&mut drain).is_pending());
    for n in 0..16 {
        let outcome = actor.deliver(Box::new(submit(n + 10))).await;
        assert_eq!(
            outcome.is_ok(),
            n < 2,
            "unexpected admission for backlog item {n}: {outcome:?}"
        );
    }
    // Control inspection still enters when the byte budget is full.
    actor
        .deliver(Box::new(CloudProxyInspectAdmission))
        .await
        .unwrap();
    let target: NodeAddr = c.receiver.parse().unwrap();
    let mut inbox = TransactionalInbox::open(
        s.0.join("inbox"),
        &target.node_id,
        "/counter",
        Bytes::new(),
        InboxLimits::default(),
        remember,
    )
    .await
    .unwrap();
    let edge = RemoteActorSystem::new(
        RemoteConfig::mem(&target.node_id),
        Arc::new(Receiver(inbox.handle())),
    )
    .unwrap();
    edge.start().await.unwrap();
    drain.await.unwrap();
    let view = status(actor.as_ref()).await;
    assert_eq!(view.last_enqueued, 1);
    assert_eq!(view.last_acked, 1);
    assert_eq!(history(&inbox.handle().snapshot().state), vec![1]);
    let stats = actor
        .send_with_timeout(
            Box::new(CloudProxyInspectAdmission),
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap()
        .downcast::<CloudProxyAdmissionStats>()
        .unwrap();
    assert_eq!(stats.used_bytes, 0);
    actor.stop().await.unwrap();
    assert_eq!(Wal::open(&c.directory).unwrap().pending_len().unwrap(), 0);
    let actor = parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, "/proxy", c)
        .await
        .unwrap();
    assert_eq!(accepted(actor.as_ref(), 2).await, 2);
    let stats = actor
        .send_with_timeout(
            Box::new(CloudProxyInspectAdmission),
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap()
        .downcast::<CloudProxyAdmissionStats>()
        .unwrap();
    assert_eq!(stats.used_bytes, 0);
    assert_eq!(stats.max_inflight_bytes, (charge * 2) as u64);
    acked(actor.as_ref(), 2).await;
    assert_eq!(history(&inbox.handle().snapshot().state), vec![1, 2]);
    actor.stop().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    edge.shutdown().await.unwrap();
    inbox.close().await.unwrap();
}

#[tokio::test]
async fn wire_request_and_batch_limits_reject_before_wal_and_release_charge_for_next_requests() {
    let s = Scratch::new();
    let (engine, remote) = source(&unique()).await;
    let (client_engine, client) = source(&unique()).await;
    client
        .connect(&NodeAddr::mem(&remote.config.node_id))
        .await
        .unwrap();
    let mut c = config(&s, format!("mem://{}", unique()));
    c.max_request_bytes = 512;
    c.max_inflight_bytes = 1024;
    let actor =
        parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, "/proxy", c.clone())
            .await
            .unwrap();
    let endpoint = client
        .remote_ref(&format!("parrot://{}/proxy", remote.config.node_id))
        .unwrap();
    let oversized = CloudProxySubmit {
        type_key: "key".into(),
        payload: Bytes::from(vec![0; 321]),
    };
    for message in [
        Box::new(oversized) as parrot_api::types::BoxedMessage,
        Box::new(CloudProxySubmitBatch {
            messages: vec![submit(9); 5],
        }),
    ] {
        let result = endpoint
            .send_with_timeout(message, Some(Duration::from_secs(5)))
            .await;
        assert!(
            result.is_err(),
            "request exceeds the actor's configured charge"
        );
        assert_eq!(status(actor.as_ref()).await.last_enqueued, 0);
    }
    let result = endpoint
        .send_with_timeout(
            Box::new(CloudProxySubmitBatch {
                messages: vec![submit(10), submit(20)],
            }),
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap()
        .downcast::<CloudProxyBatchResult>()
        .unwrap();
    assert_eq!(
        result
            .outcomes
            .iter()
            .map(|x| x.as_ref().unwrap().sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    let stats = endpoint
        .send_with_timeout(
            Box::new(CloudProxyInspectAdmission),
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap()
        .downcast::<CloudProxyAdmissionStats>()
        .unwrap();
    assert_eq!(stats.used_bytes, 0);
    assert_eq!(stats.max_request_bytes, 512);
    assert_eq!(accepted(&endpoint, 30).await, 3);
    actor.stop().await.unwrap();
    let target: NodeAddr = c.receiver.parse().unwrap();
    let records = Wal::open(&c.directory)
        .unwrap()
        .replay(&target.node_id)
        .unwrap();
    assert_eq!(
        records
            .iter()
            .map(|r| u64::from_le_bytes(r.payload.as_ref().try_into().unwrap()))
            .collect::<Vec<_>>(),
        vec![10, 20, 30]
    );
    client.shutdown().await.unwrap();
    client_engine.shutdown_internal().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

fn keyed(id: &str, value: u64) -> CloudProxySubmitIdempotent {
    let plain = submit(value);
    CloudProxySubmitIdempotent {
        request_id: id.into(),
        type_key: plain.type_key,
        payload: plain.payload,
    }
}
#[tokio::test]
async fn keyed_manifest_wire_retries_survive_lost_reply_ack_gc_and_reassembly_without_duplicate_history(
) {
    let s = Scratch::new();
    let target_id = unique();
    let mut c = config(&s, format!("mem://{target_id}"));
    c.idempotent_submissions = true;
    c.max_submission_receipts = 2;
    let m = manifest(&c);
    let p = plan(&m, &LocalTopology).unwrap();
    let (engine, remote) = source(&unique()).await;
    let (client_engine, client) = source(&unique()).await;
    client
        .connect(&NodeAddr::mem(&remote.config.node_id))
        .await
        .unwrap();
    let spawner = ThreadPropsSpawner::with_remote(engine.clone(), remote.clone());
    let deployer = LocalDeployer {
        topology: &LocalTopology,
        gateway_factory: &NoGateway,
        parrot_spawner: &spawner,
    };
    let mut ctx = AssemblingContext::new();
    ctx.assemble(&p, &deployer, &m, &Default::default())
        .await
        .unwrap();
    let actor = ctx.component_ref("proxy").unwrap();
    // Drop the producer future/result after its first poll. A subsequent ordered
    // inspection proves the accepted work despite the caller retaining no receipt.
    let mut abandoned = actor.send_with_timeout(
        Box::new(keyed("producer:a", 10)),
        Some(Duration::from_secs(5)),
    );
    let _ = futures::poll!(&mut abandoned);
    drop(abandoned);
    assert_eq!(status(actor.as_ref()).await.last_enqueued, 1);
    let wire = client
        .remote_ref(&format!(
            "parrot://{}{}",
            remote.config.node_id,
            actor.path()
        ))
        .unwrap();
    let retries = futures::future::join_all((0..32).map(|_| {
        wire.send_with_timeout(
            Box::new(keyed("producer:a", 10)),
            Some(Duration::from_secs(5)),
        )
    }))
    .await;
    for reply in retries {
        assert_eq!(
            reply
                .unwrap()
                .downcast::<CloudProxyAccepted>()
                .unwrap()
                .sequence,
            1
        );
    }
    assert!(wire
        .send_with_timeout(
            Box::new(keyed("producer:a", 99)),
            Some(Duration::from_secs(5))
        )
        .await
        .is_err());
    assert_eq!(
        wire.send_with_timeout(
            Box::new(keyed("producer:b", 20)),
            Some(Duration::from_secs(5))
        )
        .await
        .unwrap()
        .downcast::<CloudProxyAccepted>()
        .unwrap()
        .sequence,
        2
    );
    assert!(wire
        .send_with_timeout(
            Box::new(keyed("producer:c", 30)),
            Some(Duration::from_secs(5))
        )
        .await
        .is_err());
    assert_eq!(status(actor.as_ref()).await.last_enqueued, 2);
    ctx.teardown(&deployer).await.unwrap();
    let mut inbox = TransactionalInbox::open(
        s.0.join("inbox"),
        &target_id,
        "/counter",
        Bytes::new(),
        InboxLimits::default(),
        remember,
    )
    .await
    .unwrap();
    let edge = RemoteActorSystem::new(
        RemoteConfig::mem(&target_id),
        Arc::new(Receiver(inbox.handle())),
    )
    .unwrap();
    edge.start().await.unwrap();
    ctx.assemble(&p, &deployer, &m, &Default::default())
        .await
        .unwrap();
    acked(ctx.component_ref("proxy").unwrap().as_ref(), 2).await;
    assert_eq!(history(&inbox.handle().snapshot().state), vec![10, 20]);
    assert_eq!(inbox.handle().snapshot().revision, 2);
    ctx.teardown(&deployer).await.unwrap();
    assert_eq!(Wal::open(&c.directory).unwrap().pending_len().unwrap(), 0);
    // Reopen again after the ACK checkpoint deleted the original business log.
    ctx.assemble(&p, &deployer, &m, &Default::default())
        .await
        .unwrap();
    assert_eq!(
        wire.send_with_timeout(
            Box::new(keyed("producer:a", 10)),
            Some(Duration::from_secs(5))
        )
        .await
        .unwrap()
        .downcast::<CloudProxyAccepted>()
        .unwrap()
        .sequence,
        1
    );
    let view = status(ctx.component_ref("proxy").unwrap().as_ref()).await;
    assert_eq!(view.last_enqueued, 2);
    assert_eq!(view.pending, 0);
    assert_eq!(history(&inbox.handle().snapshot().state), vec![10, 20]);
    assert_eq!(inbox.handle().snapshot().revision, 2);
    ctx.teardown(&deployer).await.unwrap();
    client.shutdown().await.unwrap();
    client_engine.shutdown_internal().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
    edge.shutdown().await.unwrap();
    inbox.close().await.unwrap();
}

#[tokio::test]
async fn keyed_request_ids_participate_in_admission_and_disabled_mode_fails_before_queueing() {
    let s = Scratch::new();
    let (engine, remote) = source(&unique()).await;
    let mut c = config(&s, format!("mem://{}", unique()));
    let actor =
        parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, "/plain", c.clone())
            .await
            .unwrap();
    assert!(actor.deliver(Box::new(keyed("id", 1))).await.is_err());
    assert_eq!(status(actor.as_ref()).await.last_enqueued, 0);
    actor.stop().await.unwrap();
    c.directory = s.0.join("keyed");
    c.idempotent_submissions = true;
    c.max_request_bytes = CLOUD_PROXY_REQUEST_OVERHEAD
        + CLOUD_PROXY_RECORD_OVERHEAD
        + submit(1).type_key.len()
        + 8
        + 2;
    c.max_inflight_bytes = c.max_request_bytes;
    let actor = parrot_app::host::cloud_proxy::CloudProxy::spawn(&engine, &remote, "/keyed", c)
        .await
        .unwrap();
    for id in ["", "bad\0id", "too-long"] {
        assert!(actor.deliver(Box::new(keyed(id, 1))).await.is_err());
    }
    assert_eq!(
        actor
            .send_with_timeout(Box::new(keyed("id", 1)), Some(Duration::from_secs(5)))
            .await
            .unwrap()
            .downcast::<CloudProxyAccepted>()
            .unwrap()
            .sequence,
        1
    );
    let stats = actor
        .send_with_timeout(
            Box::new(CloudProxyInspectAdmission),
            Some(Duration::from_secs(5)),
        )
        .await
        .unwrap()
        .downcast::<CloudProxyAdmissionStats>()
        .unwrap();
    assert_eq!(stats.used_bytes, 0);
    actor.stop().await.unwrap();
    remote.shutdown().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}
