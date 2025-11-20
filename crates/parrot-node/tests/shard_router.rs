//! Real Node assembly, real children and durable ownership. Expected rejections
//! are checked alongside later healthy work, not merely an error return.
use parrot_api::{
    actor::{Actor, ActorState},
    address::{ActorPath, ActorRef},
    context::ActorContext,
    engine::{AsyncEngineRuntime, SpawnOptions},
    errors::ActorError,
    system::ActorSystem,
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_node::{
    configuration::NodeSettings,
    runtime::RunningNode,
    sharding::{ShardConfig, ShardEntity, ShardFactory, ShardRouter},
    NEcho, NEchoed, NGet, NGetTotal, NGot, NPut, NSlowEcho, NTotal,
};
use parrot_remote::{
    raft::Role,
    roles::directory::{
        network::{DirectoryTiming, NetworkDirectory},
        ownership::{OwnershipClusterClient, OwnershipOutcome, OwnershipRecord, OwnershipStatus},
    },
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig,
};
use std::{
    any::Any,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

const WAIT: Duration = Duration::from_secs(10);
#[path = "shard_router/concurrency.rs"]
mod concurrency;
#[cfg(unix)]
#[path = "shard_router/example.rs"]
mod example;
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}

struct Control {
    router: ShardRouter,
    members: Vec<NodeAddr>,
    events: Arc<Events>,
}
impl Actor for Control {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<NGetTotal>() {
                return Ok(
                    Box::new(NTotal(self.events.dropped.load(Ordering::SeqCst) as u64))
                        as BoxedMessage,
                );
            }
            let NPut(command, value) = *message.downcast::<NPut>().unwrap();
            let result = match command.as_str() {
                "members" => {
                    let selected = self
                        .members
                        .iter()
                        .filter(|m| value.split(',').any(|v| v == m.node_id))
                        .cloned()
                        .collect();
                    self.router.update_members(selected).await
                }
                "passivate" => self.router.passivate(&value).await,
                "close" => self.router.close().await,
                _ => panic!("unknown test control"),
            };
            Ok(Box::new(NGot(Some(
                result
                    .map(|()| "ok".into())
                    .unwrap_or_else(|e| e.to_string()),
            ))) as BoxedMessage)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "subprocess helper; invoked by the process contract"]
async fn shard_worker() {
    let config = std::env::var_os("SHARD_WORKER_CONFIG").expect("worker config");
    let settings = NodeSettings::from_environment(|key| match key {
        "PARROT_CONFIG" => Some(config.clone()),
        "PARROT_ACTORS" => Some("".into()),
        _ => None,
    })
    .unwrap();
    let members: Vec<NodeAddr> = std::env::var("SHARD_WORKER_MEMBERS")
        .unwrap()
        .split(',')
        .map(|v| v.parse().unwrap())
        .collect();
    let authority: NodeAddr = std::env::var("SHARD_WORKER_AUTHORITY")
        .unwrap()
        .parse()
        .unwrap();
    let node = RunningNode::start(&settings).await.unwrap();
    let events = new_events();
    let router = node
        .install_shards(
            ShardConfig::new(
                "process",
                members.clone(),
                "router-process",
                vec![authority],
            ),
            factory(&events),
        )
        .await
        .unwrap();
    node.thread
        .spawn(
            Box::new(Control {
                router,
                members,
                events: events.clone(),
            }),
            Box::new(()),
            SpawnOptions::at("/test/control"),
        )
        .await
        .unwrap();
    println!("SHARD_READY");
    use tokio::io::AsyncBufReadExt;
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    input.read_line(&mut line).await.unwrap();
    assert_eq!(line, "stop\n");
    node.shutdown().await.unwrap();
    assert_eq!(
        events.created.lock().unwrap().len(),
        events.dropped.load(Ordering::SeqCst)
    );
}

fn certificates(root: &std::path::Path, names: &[&str]) {
    let mut params = rcgen::CertificateParams::default();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    let ca = params.self_signed(&key).unwrap().pem();
    std::fs::write(root.join("ca.pem"), ca).unwrap();
    let issuer = rcgen::Issuer::new(params, key);
    for name in names {
        let mut params = rcgen::CertificateParams::new(vec![name.to_string()]).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, *name);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &issuer).unwrap();
        std::fs::write(root.join(format!("{name}.pem")), cert.pem()).unwrap();
        std::fs::write(root.join(format!("{name}.key")), key.serialize_pem()).unwrap();
    }
}
fn tls(root: &std::path::Path, name: &str) -> parrot_remote::tls::TlsConfig {
    parrot_remote::tls::TlsConfig {
        cert_pem: std::fs::read(root.join(format!("{name}.pem"))).unwrap(),
        key_pem: std::fs::read(root.join(format!("{name}.key"))).unwrap(),
        ca_pem: std::fs::read(root.join("ca.pem")).unwrap(),
    }
}
struct Process {
    child: tokio::process::Child,
    output: tokio::task::JoinHandle<String>,
}
async fn process(config: &std::path::Path, members: &[NodeAddr], authority: &NodeAddr) -> Process {
    let addresses = members
        .iter()
        .map(|m| format!("tcp://{}@{}", m.node_id, m.addr))
        .collect::<Vec<_>>()
        .join(",");
    let child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "shard_worker", "--nocapture"])
        .env("SHARD_WORKER_CONFIG", config)
        .env("SHARD_WORKER_MEMBERS", addresses)
        .env(
            "SHARD_WORKER_AUTHORITY",
            format!("tcp://{}@{}", authority.node_id, authority.addr),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    collect_process(child).await
}
async fn collect_process(mut child: tokio::process::Child) -> Process {
    let output = child.stdout.take().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let collector = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut output = tokio::io::BufReader::new(output);
        let mut text = String::new();
        let mut tx = Some(tx);
        loop {
            let mut line = String::new();
            if output.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            if line.contains("SHARD_READY") || line.contains("SHARDED_REGISTER_READY=") {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(());
                }
            }
            text.push_str(&line);
            print!("worker: {line}");
            assert!(text.len() < 65536, "unbounded worker output");
        }
        text
    });
    tokio::time::timeout(WAIT, rx)
        .await
        .expect("worker readiness deadline")
        .expect("worker exited before readiness");
    Process {
        child,
        output: collector,
    }
}
async fn finish(mut process: Process, killed: bool) {
    if killed {
        process.child.kill().await.unwrap();
    } else {
        use tokio::io::AsyncWriteExt;
        process
            .child
            .stdin
            .take()
            .unwrap()
            .write_all(b"stop\n")
            .await
            .unwrap();
    }
    let status = tokio::time::timeout(WAIT, process.child.wait())
        .await
        .unwrap()
        .unwrap();
    let text = process.output.await.unwrap();
    assert_eq!(status.success(), !killed, "worker status {status}: {text}");
}
async fn control(remote: &Arc<RemoteActorSystem>, node: &str, command: &str, value: &str) {
    let actor = remote
        .remote_ref(&format!("parrot://{node}/test/control"))
        .unwrap();
    let result = actor
        .send_with_timeout(Box::new(NPut(command.into(), value.into())), Some(WAIT))
        .await
        .unwrap();
    assert_eq!(
        result.downcast_ref::<NGot>().unwrap().0.as_deref(),
        Some("ok")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mtls_processes_route_both_ways_handoff_state_and_refuse_crash_takeover() {
    use parrot_remote::sharding::HashRing;
    use std::net::TcpListener;
    let root = tempfile::tempdir().unwrap();
    let names = [
        "router-a.test",
        "router-b.test",
        "router-dir.test",
        "router-client.test",
    ];
    certificates(root.path(), &names);
    let mut sockets: Vec<_> = (0..3)
        .map(|_| Some(TcpListener::bind("127.0.0.1:0").unwrap()))
        .collect();
    let addresses: Vec<_> = sockets
        .iter()
        .map(|s| s.as_ref().unwrap().local_addr().unwrap())
        .collect();
    let members = vec![
        NodeAddr::tcp(names[0], addresses[0]),
        NodeAddr::tcp(names[1], addresses[1]),
    ];
    let peer = NodeAddr::tcp(names[2], addresses[2]);
    let mut configs = Vec::new();
    for i in 0..2 {
        let path = root.path().join(format!("node{i}.toml"));
        let text = format!("[remote.node]\nnode_id={:?}\nbind={:?}\n[remote.security]\nmode='mtls'\ncert_path={:?}\nkey_path={:?}\nca_path={:?}\n", names[i], addresses[i].to_string(), root.path().join(format!("{}.pem", names[i])), root.path().join(format!("{}.key", names[i])), root.path().join("ca.pem"));
        std::fs::write(&path, text).unwrap();
        configs.push(path);
    }
    drop(sockets[2].take());
    std::fs::create_dir(root.path().join("directory")).unwrap();
    let mut directory = NetworkDirectory::start_with_timing(
        root.path().join("directory"),
        "router-process",
        RemoteConfig::tcp(names[2], Some(addresses[2])).with_mtls(
            tls(root.path(), names[2]),
            vec![names[0].into(), names[1].into(), names[3].into()],
        ),
        vec![],
        Arc::new(Empty),
        DirectoryTiming {
            election_ms: 100,
            heartbeat_ms: 20,
            rpc_timeout_ms: 500,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(WAIT, async {
        while directory.handle().observation().role != Role::Leader {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let remote = RemoteActorSystem::new(
        RemoteConfig::tcp(names[3], None).with_mtls(tls(root.path(), names[3]), vec![]),
        Arc::new(Empty),
    )
    .unwrap();
    remote.start().await.unwrap();
    let authority =
        OwnershipClusterClient::new(remote.clone(), "router-process", vec![peer.clone()]).unwrap();
    drop(sockets[0].take());
    let a = process(&configs[0], &members, &peer).await;
    drop(sockets[1].take());
    let b = process(&configs[1], &members, &peer).await;
    for member in &members {
        remote.connect(member).await.unwrap();
    }
    let ring = HashRing::new(
        &names[..2].iter().map(|n| n.to_string()).collect::<Vec<_>>(),
        256,
    );
    let key_a = (0..100)
        .map(|i| format!("key{i}"))
        .find(|k| ring.node(k) == Some(names[0]))
        .unwrap();
    let key_b = (0..100)
        .map(|i| format!("key{i}"))
        .find(|k| ring.node(k) == Some(names[1]))
        .unwrap();
    let to_b = remote
        .remote_ref(&format!("parrot://{}/shards/process/{key_b}", names[0]))
        .unwrap();
    let to_a = remote
        .remote_ref(&format!("parrot://{}/shards/process/{key_a}", names[1]))
        .unwrap();
    let (old_a, old_b) = tokio::join!(generation(&to_a), generation(&to_b));
    assert_ne!(old_a, old_b);
    assert_eq!(
        owner(&authority, &format!("/shards/process/{key_b}"))
            .await
            .unwrap()
            .holder
            .as_deref(),
        Some(names[1])
    );
    let revision = to_b
        .send_with_timeout(
            Box::new(NPut(
                "process-write:none".into(),
                "state-before-handoff".into(),
            )),
            Some(WAIT),
        )
        .await
        .unwrap()
        .downcast::<NGot>()
        .unwrap()
        .0
        .clone()
        .unwrap();
    let hidden = remote
        .remote_ref(&format!("parrot://{}/_shard_hosts/process", names[1]))
        .unwrap();
    assert!(hidden
        .send_with_timeout(Box::new(NEcho(999)), Some(WAIT))
        .await
        .is_err());
    echo(&to_b, 1).await;
    control(&remote, names[0], "members", names[1]).await;
    control(&remote, names[1], "members", names[0]).await;
    for actor in [&to_b, &to_a] {
        let failure = match actor
            .send_with_timeout(Box::new(NEcho(2)), Some(WAIT))
            .await
        {
            Err(failure) => failure,
            Ok(_) => panic!("wrong-host request succeeded"),
        };
        assert!(
            failure.to_string().contains("stale shard destination"),
            "wrong-host request did not terminate at the first receiver: {failure}"
        );
    }
    // Converge on A. B has actually retired/released its child; a new A
    // generation sees the state written before handoff.
    control(&remote, names[0], "members", names[0]).await;
    let next = generation(&to_b).await;
    assert!(next > old_b);
    assert_eq!(
        to_b.send_with_timeout(Box::new(NGet("".into())), Some(WAIT))
            .await
            .unwrap()
            .downcast::<NGot>()
            .unwrap()
            .0
            .as_deref(),
        Some("state-before-handoff")
    );
    assert!(to_b
        .send_with_timeout(
            Box::new(NPut(
                format!("process-write2:{revision}"),
                "state-after-handoff".into()
            )),
            Some(WAIT)
        )
        .await
        .unwrap()
        .downcast::<NGot>()
        .unwrap()
        .0
        .is_some());
    let stale = remote
        .remote_ref(&format!("parrot://{}/_shards/process/{key_b}", names[1]))
        .unwrap();
    assert!(stale
        .send_with_timeout(Box::new(NEcho(3)), Some(WAIT))
        .await
        .is_err());
    let stats = remote
        .remote_ref(&format!("parrot://{}/test/control", names[1]))
        .unwrap();
    assert!(generation(&stats).await > 0);
    finish(a, true).await;
    control(&remote, names[1], "members", names[1]).await;
    let surviving = remote
        .remote_ref(&format!("parrot://{}/shards/process/{key_b}", names[1]))
        .unwrap();
    assert!(surviving
        .send_with_timeout(Box::new(NEcho(4)), Some(WAIT))
        .await
        .is_err());
    assert_eq!(
        owner(&authority, &format!("/shards/process/{key_b}"))
            .await
            .unwrap()
            .holder
            .as_deref(),
        Some(names[0])
    );
    let restarted = process(&configs[0], &members, &peer).await;
    remote.connect(&members[0]).await.unwrap();
    // Same node identity is not proof that its previous generation retired.
    control(&remote, names[0], "members", names[0]).await;
    let restarted_ref = remote
        .remote_ref(&format!("parrot://{}/shards/process/{key_b}", names[0]))
        .unwrap();
    assert!(restarted_ref
        .send_with_timeout(Box::new(NEcho(5)), Some(WAIT))
        .await
        .is_err());
    finish(restarted, false).await;
    finish(b, false).await;
    remote.shutdown().await.unwrap();
    directory.close().await.unwrap();
}
type Creation = (String, u64, Option<Vec<u8>>);
struct Events {
    created: Mutex<Vec<Creation>>,
    processed: Mutex<Vec<(String, u64, Option<TraceContext>)>>,
    dropped: AtomicUsize,
    stopping: Notify,
    entered: Notify,
    dropping: Notify,
    drop_gate: (Mutex<bool>, Condvar),
    work: Semaphore,
    stop: Semaphore,
}
struct EntityActor {
    entity: ShardEntity,
    events: Arc<Events>,
}
impl Drop for EntityActor {
    fn drop(&mut self) {
        if self.entity.key == "late-drop" {
            self.events.dropping.notify_one();
            let (guard, timeout) = self
                .events
                .drop_gate
                .1
                .wait_timeout_while(self.events.drop_gate.0.lock().unwrap(), WAIT, |released| {
                    !*released
                })
                .unwrap();
            drop(guard);
            assert!(
                !timeout.timed_out(),
                "test owner did not release the destructor"
            );
        }
        self.events.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for EntityActor {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.entity.key == "late-init" {
                self.events.entered.notify_one();
                self.events.work.acquire().await.unwrap().forget();
            }
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(NEcho(value)) = message.downcast_ref::<NEcho>() {
                self.events.processed.lock().unwrap().push((
                    self.entity.key.clone(),
                    *value,
                    TraceContext::current(),
                ));
                return Ok(Box::new(NEchoed(*value)) as BoxedMessage);
            }
            if message.is::<NGetTotal>() {
                return Ok(Box::new(NTotal(self.entity.generation)) as BoxedMessage);
            }
            if let Some(NSlowEcho(millis)) = message.downcast_ref::<NSlowEcho>() {
                self.events.entered.notify_one();
                if self.entity.key == "blocked" {
                    self.events.work.acquire().await.unwrap().forget();
                } else {
                    tokio::time::sleep(Duration::from_millis(*millis)).await;
                }
                self.events.processed.lock().unwrap().push((
                    self.entity.key.clone(),
                    *millis,
                    TraceContext::current(),
                ));
                return Ok(Box::new(NEchoed(*millis)) as BoxedMessage);
            }
            if let Some(NPut(identity, value)) = message.downcast_ref::<NPut>() {
                // Tests supply the expected revision explicitly; never rebuild
                // a mutation from a newer value while retaining its old ID.
                let (id, revision) = identity.split_once(':').unwrap();
                let revision = (revision != "none").then(|| revision.parse().unwrap());
                let status = self
                    .entity
                    .write_value(id, revision, value.as_bytes().to_vec(), WAIT)
                    .await?;
                let reply = match status {
                    OwnershipStatus::Applied {
                        outcome: OwnershipOutcome::ValueChanged { value, .. },
                        ..
                    } => Some(value.revision.to_string()),
                    _ => None,
                };
                return Ok(Box::new(NGot(reply)) as BoxedMessage);
            }
            if message.is::<NGet>() {
                let status = self.entity.read_value(WAIT).await?;
                let reply = match status {
                    OwnershipStatus::Applied {
                        outcome: OwnershipOutcome::ValueObserved { value, .. },
                        ..
                    } => value.map(|v| String::from_utf8(v.value.to_vec()).unwrap()),
                    _ => panic!("unexpected state response"),
                };
                return Ok(Box::new(NGot(reply)) as BoxedMessage);
            }
            Err(ActorError::MessageHandlingError(
                "unsupported test message".into(),
            ))
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.entity.key == "blocked-stop" {
                self.events.stopping.notify_one();
                self.events.stop.acquire().await.unwrap().forget();
            }
            if self.entity.key == "failed-stop" {
                return Err(ActorError::InternalError(
                    "deliberate retirement failure".into(),
                ));
            }
            Ok(())
        })
    }
}
fn new_events() -> Arc<Events> {
    Arc::new(Events {
        created: Mutex::new(Vec::new()),
        processed: Mutex::new(Vec::new()),
        dropped: AtomicUsize::new(0),
        stopping: Notify::new(),
        entered: Notify::new(),
        dropping: Notify::new(),
        drop_gate: (Mutex::new(false), Condvar::new()),
        work: Semaphore::new(0),
        stop: Semaphore::new(0),
    })
}
fn factory(events: &Arc<Events>) -> ShardFactory {
    let events = events.clone();
    Arc::new(move |entity| {
        if entity.key == "bad-factory" {
            panic!("deliberate factory failure");
        }
        events.created.lock().unwrap().push((
            entity.key.clone(),
            entity.generation,
            entity.value.as_ref().map(|v| v.value.to_vec()),
        ));
        Ok(Box::new(EntityActor {
            entity,
            events: events.clone(),
        }))
    })
}
async fn reference(node: &RunningNode, router: &ShardRouter, key: &str) -> BoxedActorRef {
    ActorSystem::get_actor(
        node.facade.as_ref(),
        &ActorPath::placeholder(router.path(key).unwrap()),
    )
    .await
    .unwrap()
}
async fn generation(actor: &dyn ActorRef) -> u64 {
    actor
        .send_with_timeout(Box::new(NGetTotal), Some(WAIT))
        .await
        .unwrap()
        .downcast::<NTotal>()
        .unwrap()
        .0
}
async fn owner(client: &OwnershipClusterClient, resource: &str) -> Option<OwnershipRecord> {
    match client.observe(resource, WAIT).await.unwrap() {
        OwnershipStatus::Applied {
            outcome: OwnershipOutcome::Observed(record),
            ..
        } => record,
        other => panic!("unexpected ownership: {other:?}"),
    }
}
async fn echo(actor: &dyn ActorRef, value: u64) {
    assert_eq!(
        *actor
            .send_with_timeout(Box::new(NEcho(value)), Some(WAIT))
            .await
            .unwrap()
            .downcast::<NEchoed>()
            .unwrap(),
        NEchoed(value)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_shards_own_activation_drain_cancellation_and_actual_retirement() {
    let data = tempfile::tempdir().unwrap();
    let mut directory = NetworkDirectory::start_with_timing(
        data.path(),
        "router-test",
        RemoteConfig::mem("router-authority"),
        vec![],
        Arc::new(Empty),
        DirectoryTiming {
            election_ms: 100,
            heartbeat_ms: 20,
            rpc_timeout_ms: 500,
        },
    )
    .await
    .unwrap();
    tokio::time::timeout(WAIT, async {
        while directory.handle().observation().role != Role::Leader {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut settings = NodeSettings::from_environment(|key| match key {
        "PARROT_NODE_ID" => Some("router-local".into()),
        "PARROT_SCHEME" => Some("mem".into()),
        "PARROT_ACTORS" => Some("".into()),
        _ => None,
    })
    .unwrap();
    settings.resolved.thread.shutdown_timeout_ms = 1000;
    settings.resolved.thread.default_mailbox_capacity = 1;
    let node = RunningNode::start(&settings).await.unwrap();
    let routes = vec![NodeAddr::mem("router-authority")];
    let members = vec![NodeAddr::mem("router-local")];
    let authority =
        OwnershipClusterClient::new(node.remote.clone(), "router-test", routes.clone()).unwrap();
    let config = |name| ShardConfig::new(name, members.clone(), "router-test", routes.clone());
    concurrency::run(&node, &members, &routes, &authority).await;
    let events = new_events();
    let router = node
        .install_shards(config("contracts"), factory(&events))
        .await
        .unwrap();
    assert!(node
        .install_shards(config("contracts"), factory(&events))
        .await
        .is_err());
    for bad in ["", ".", "..", "a/b", "a?b", "a\nb"] {
        assert!(router.path(bad).is_err());
    }
    let a = reference(&node, &router, "alice").await;
    let b = reference(&node, &router, "bob").await;
    assert!(owner(&authority, &router.path("alice").unwrap())
        .await
        .is_none());
    let first = generation(a.as_ref()).await;
    let own_uri = ActorSystem::get_actor(
        node.facade.as_ref(),
        &ActorPath::placeholder("parrot://router-local/shards/contracts/alice"),
    )
    .await
    .unwrap();
    assert_eq!(generation(own_uri.as_ref()).await, first);
    assert_eq!(generation(a.as_ref()).await, first);
    assert_ne!(generation(b.as_ref()).await, first);
    let root = TraceContext::root();
    trace::scope(Some(root.clone()), async {
        echo(a.as_ref(), 10).await;
        a.deliver(Box::new(NEcho(11))).await.unwrap();
        a.deliver_with_options(Box::new(NEcho(12)), Default::default())
            .await
            .unwrap();
        echo(a.as_ref(), 13).await;
    })
    .await;
    assert_eq!(
        events
            .processed
            .lock()
            .unwrap()
            .iter()
            .map(|(_, id, _)| *id)
            .collect::<Vec<_>>(),
        [10, 11, 12, 13]
    );
    for (_, _, context) in events.processed.lock().unwrap().iter() {
        assert_eq!(context.as_ref().unwrap().trace_id(), root.trace_id());
    }
    let revision = a
        .send(Box::new(NPut("write1:none".into(), "persisted".into())))
        .await
        .unwrap()
        .downcast::<NGot>()
        .unwrap()
        .0
        .clone()
        .unwrap();
    let before = events.dropped.load(Ordering::SeqCst);
    router.passivate("alice").await.unwrap();
    assert_eq!(
        events.dropped.load(Ordering::SeqCst),
        before + 1,
        "release preceded actual instance retirement"
    );
    assert!(owner(&authority, &router.path("alice").unwrap())
        .await
        .unwrap()
        .holder
        .is_none());
    let next = generation(a.as_ref()).await;
    assert!(
        b.send(Box::new(NPut("write1:none".into(), "independent".into())))
            .await
            .unwrap()
            .downcast::<NGot>()
            .unwrap()
            .0
            .is_some(),
        "independent entities collided in the write identity namespace"
    );
    assert!(next > first);
    assert!(events
        .created
        .lock()
        .unwrap()
        .iter()
        .any(|(key, generation, value)| key == "alice"
            && *generation == next
            && value.as_deref() == Some(b"persisted")));
    assert!(a
        .send(Box::new(NPut(format!("write2:{revision}"), "next".into())))
        .await
        .unwrap()
        .downcast::<NGot>()
        .unwrap()
        .0
        .is_some());
    assert_eq!(
        a.send(Box::new(NGet("".into())))
            .await
            .unwrap()
            .downcast::<NGot>()
            .unwrap()
            .0
            .as_deref(),
        Some("next")
    );

    let blocked = reference(&node, &router, "blocked").await;
    blocked.deliver(Box::new(NSlowEcho(42))).await.unwrap();
    tokio::time::timeout(WAIT, events.entered.notified())
        .await
        .unwrap();
    blocked.deliver(Box::new(NEcho(43))).await.unwrap();
    let close = router.passivate("blocked");
    tokio::pin!(close);
    assert!(tokio::time::timeout(Duration::from_millis(80), &mut close)
        .await
        .is_err());
    assert_eq!(
        owner(&authority, &router.path("blocked").unwrap())
            .await
            .unwrap()
            .holder
            .as_deref(),
        Some("router-local")
    );
    events.work.add_permits(1);
    close.await.unwrap();
    assert_eq!(
        events
            .processed
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _, _)| key == "blocked")
            .map(|(_, id, _)| *id)
            .collect::<Vec<_>>(),
        [42, 43]
    );

    let blocked_stop = reference(&node, &router, "blocked-stop").await;
    let old = generation(blocked_stop.as_ref()).await;
    // Cancel the caller, not the already admitted retirement command.
    let mut retire = Box::pin(router.passivate("blocked-stop"));
    tokio::select! { _ = events.stopping.notified() => {}, result = &mut retire => panic!("retirement completed too soon: {result:?}") }
    assert_eq!(
        owner(&authority, &router.path("blocked-stop").unwrap())
            .await
            .unwrap()
            .generation,
        old
    );
    drop(retire);
    // This waiter is cancelled before admission while the accepted passivation
    // still owns control capacity. It must not silently change placement later.
    assert!(
        tokio::time::timeout(Duration::from_millis(80), router.update_members(vec![]))
            .await
            .is_err()
    );
    events.stop.add_permits(1);
    router.passivate("blocked-stop").await.unwrap();
    echo(b.as_ref(), 98).await;

    struct ReleaseDrop(Arc<Events>);
    impl Drop for ReleaseDrop {
        fn drop(&mut self) {
            *self.0.drop_gate.0.lock().unwrap() = true;
            self.0.drop_gate.1.notify_all();
        }
    }
    let release = ReleaseDrop(events.clone());
    let late_drop = reference(&node, &router, "late-drop").await;
    let late_generation = generation(late_drop.as_ref()).await;
    let mut retirement = Box::pin(router.passivate("late-drop"));
    tokio::select! { _ = events.dropping.notified() => {}, result = &mut retirement => panic!("retirement completed before instance disposal: {result:?}") }
    assert!(
        retirement.await.is_err(),
        "public timeout hid a pending actual destructor"
    );
    assert_eq!(
        owner(&authority, &router.path("late-drop").unwrap())
            .await
            .unwrap()
            .generation,
        late_generation
    );
    assert!(owner(&authority, &router.path("late-drop").unwrap())
        .await
        .unwrap()
        .holder
        .is_some());
    drop(release);
    tokio::time::timeout(WAIT, async {
        while router.passivate("late-drop").await.is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(owner(&authority, &router.path("late-drop").unwrap())
        .await
        .unwrap()
        .holder
        .is_none());
    assert!(owner(&authority, &router.path("blocked-stop").unwrap())
        .await
        .unwrap()
        .holder
        .is_none());

    let bad = reference(&node, &router, "bad-factory").await;
    assert!(bad.send(Box::new(NEcho(1))).await.is_err());
    assert!(owner(&authority, &router.path("bad-factory").unwrap())
        .await
        .unwrap()
        .holder
        .is_some());
    router.passivate("bad-factory").await.unwrap();
    echo(b.as_ref(), 99).await;
    assert!(router
        .update_members(vec![members[0].clone(), members[0].clone()])
        .await
        .is_err());
    echo(b.as_ref(), 100).await;
    router.update_members(vec![]).await.unwrap();
    assert!(a.send(Box::new(NEcho(1))).await.is_err());
    router.update_members(members.clone()).await.unwrap();
    assert!(generation(a.as_ref()).await > next);

    let mut bounded = config("bounded");
    bounded.max_inflight = 1;
    bounded.max_entities = 1;
    let bounded_events = new_events();
    let bounded = node
        .install_shards(bounded, factory(&bounded_events))
        .await
        .unwrap();
    let one = reference(&node, &bounded, "blocked").await;
    assert!(matches!(
        one.send_with_timeout(Box::new(NSlowEcho(1)), Some(Duration::from_millis(100)))
            .await,
        Err(ActorError::Timeout)
    ));
    assert!(
        matches!(one.send_with_timeout(Box::new(NEcho(2)), Some(Duration::from_millis(100))).await,
        Err(ActorError::MessageHandlingError(text)) if text.contains("admission capacity"))
    );
    bounded_events.work.add_permits(1);
    bounded.passivate("blocked").await.unwrap();
    echo(one.as_ref(), 3).await;
    let two = reference(&node, &bounded, "two").await;
    assert!(two.send(Box::new(NEcho(4))).await.is_err());
    bounded.passivate("blocked").await.unwrap();
    echo(two.as_ref(), 5).await;
    bounded.passivate("two").await.unwrap();
    let late = reference(&node, &bounded, "late-init").await;
    assert!(matches!(
        late.send_with_timeout(Box::new(NEcho(61)), Some(Duration::from_millis(100)))
            .await,
        Err(ActorError::Timeout)
    ));
    tokio::time::timeout(WAIT, bounded_events.entered.notified())
        .await
        .unwrap();
    assert!(owner(&authority, &bounded.path("late-init").unwrap())
        .await
        .unwrap()
        .holder
        .is_some());
    bounded_events.work.add_permits(1);
    bounded.passivate("late-init").await.unwrap();
    assert!(bounded_events
        .processed
        .lock()
        .unwrap()
        .iter()
        .any(|(key, id, _)| key == "late-init" && *id == 61));
    struct DropPanic(Arc<AtomicUsize>);
    impl Drop for DropPanic {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("deliberate input disposal failure");
        }
    }
    let input_drops = Arc::new(AtomicUsize::new(0));
    let future = a.send(Box::new(DropPanic(input_drops.clone())));
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok());
    assert_eq!(input_drops.load(Ordering::SeqCst), 1);
    let token = DropPanic(input_drops.clone());
    let unpolled = node.install_shards(
        config("unpolled"),
        Arc::new(move |_| {
            let _ = &token;
            panic!("unpolled factory was called")
        }),
    );
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(unpolled))).is_ok());
    assert_eq!(input_drops.load(Ordering::SeqCst), 2);
    router.close().await.unwrap();
    router.close().await.unwrap();
    assert!(a.send(Box::new(NEcho(1))).await.is_err());
    let failure_events = new_events();
    let failure_router = node
        .install_shards(config("retirement-errors"), factory(&failure_events))
        .await
        .unwrap();
    let failed = reference(&node, &failure_router, "failed-stop").await;
    let healthy = reference(&node, &failure_router, "healthy").await;
    let failed_generation = generation(failed.as_ref()).await;
    generation(healthy.as_ref()).await;
    assert!(failure_router.passivate("failed-stop").await.is_err());
    assert!(failed.send(Box::new(NEcho(1))).await.is_err());
    echo(healthy.as_ref(), 55).await;
    assert!(failure_router.close().await.is_err());
    let failed_owner = owner(&authority, &failure_router.path("failed-stop").unwrap())
        .await
        .unwrap();
    assert_eq!(failed_owner.generation, failed_generation);
    assert_eq!(failed_owner.holder.as_deref(), Some("router-local"));
    assert!(owner(&authority, &failure_router.path("healthy").unwrap())
        .await
        .unwrap()
        .holder
        .is_none());
    let token = DropPanic(input_drops.clone());
    let drop_router = node
        .install_shards(
            config("factory-disposal"),
            Arc::new(move |_| {
                let _ = &token;
                panic!("unused factory was called")
            }),
        )
        .await
        .unwrap();
    assert!(
        drop_router.close().await.is_err(),
        "final factory disposal failure was hidden"
    );
    assert_eq!(input_drops.load(Ordering::SeqCst), 3);
    let stalled_events = new_events();
    let stalled_router = node
        .install_shards(config("shutdown-deadline"), factory(&stalled_events))
        .await
        .unwrap();
    let stalled = reference(&node, &stalled_router, "blocked").await;
    stalled.deliver(Box::new(NSlowEcho(71))).await.unwrap();
    tokio::time::timeout(WAIT, stalled_events.entered.notified())
        .await
        .unwrap();
    assert!(tokio::time::timeout(WAIT, node.shutdown())
        .await
        .expect("Node shutdown abandoned its deadline")
        .is_err());
    let audit_remote = RemoteActorSystem::new(
        RemoteConfig::mem("router-after-shutdown-audit"),
        Arc::new(Empty),
    )
    .unwrap();
    audit_remote.start().await.unwrap();
    let audit =
        OwnershipClusterClient::new(audit_remote.clone(), "router-test", routes.clone()).unwrap();
    assert_eq!(
        owner(&audit, &stalled_router.path("blocked").unwrap())
            .await
            .unwrap()
            .holder
            .as_deref(),
        Some("router-local"),
        "aborted drain released its generation"
    );
    audit_remote.shutdown().await.unwrap();
    assert_eq!(
        stalled_events.created.lock().unwrap().len(),
        stalled_events.dropped.load(Ordering::SeqCst)
    );
    assert!(node
        .install_shards(config("after-close"), factory(&events))
        .await
        .is_err());
    directory.close().await.unwrap();
    assert_eq!(
        events.created.lock().unwrap().len(),
        events.dropped.load(Ordering::SeqCst)
    );
    assert_eq!(
        bounded_events.created.lock().unwrap().len(),
        bounded_events.dropped.load(Ordering::SeqCst)
    );
}
