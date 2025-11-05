//! Real actor contexts -> TCP discovery -> facade resolution -> codec/ASK/reply.
use parrot::{
    actix::{ActixActor, ActixActorSystem, ActixContext},
    system::{ParrotActorSystem, RemoteGateway},
    thread::{
        SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadActorSystemConfig,
        ThreadContext,
    },
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorPath,
    context::ActorContext,
    receptionist::{ReceptionistEvent, ReceptionistKey, ReceptionistStream},
    system::ActorSystem,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig, acl::AclRules, swim::SwimConfig,
    system::RemoteGatewayImpl,
};
use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const LIMIT: Duration = Duration::from_secs(5);
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Ping(u64);
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Pong {
    sequence: u64,
    receiver: String,
    received: usize,
}
macro_rules! codec {
    ($ty:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$ty>(),
                encode: |message: &BoxedMessage| parrot_api::message::serde_remote_serialize(
                    message.downcast_ref::<$ty>().ok_or($key)?),
                decode: |bytes: &[u8]| {
                    let value: $ty = parrot_api::message::serde_remote_deserialize(bytes)?;
                    Ok(Box::new(value) as BoxedMessage)
                },
            }
        }
    };
}
codec!(Ping, "bin:network-actor-receptionist::Ping#v1");
codec!(Pong, "bin:network-actor-receptionist::Pong#v1");

#[derive(Default)]
struct Counts {
    initialized: AtomicUsize,
    dropped: AtomicUsize,
    received: Mutex<Vec<u64>>,
}
enum Command {
    Ready,
    Register,
    Deregister,
    Subscribe,
}
fn key() -> ReceptionistKey {
    ReceptionistKey::new("edge/real-actor").unwrap()
}
async fn handle<C: ActorContext>(
    counts: &Counts,
    message: BoxedMessage,
    ctx: &mut C,
) -> ActorResult<BoxedMessage> {
    let value: BoxedMessage = match message.downcast::<Ping>() {
        Ok(ping) => {
            let mut received = counts.received.lock().unwrap();
            received.push(ping.0);
            Box::new(Pong {
                sequence: ping.0,
                receiver: ctx.get_self_ref().path(),
                received: received.len(),
            })
        }
        Err(message) => match *message.downcast::<Command>().expect("probe command") {
            Command::Ready => Box::new(()),
            Command::Register => Box::new(ctx.receptionist_register(key()).await),
            Command::Deregister => Box::new(ctx.receptionist_deregister(&key()).await),
            Command::Subscribe => Box::new(ctx.receptionist_subscribe(key()).await),
        },
    };
    Ok(value)
}
macro_rules! actor {
    ($name:ident, $context:ty) => {
        struct $name(Arc<Counts>);
        impl Drop for $name {
            fn drop(&mut self) {
                self.0.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
        impl Actor for $name {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn init<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                self.0.initialized.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(handle(&self.0, message, ctx))
            }
        }
    };
}
actor!(ThreadProbe, ThreadContext<Self>);
actor!(ActixProbe, ActixContext<ActixActor<Self>>);
impl ActixEngineExt for ActixProbe {}

struct Lookup(Weak<ParrotActorSystem>);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        self.0
            .upgrade()?
            .get_actor(&ActorPath::placeholder(path))
            .await
    }
}
struct Gateway(RemoteGatewayImpl);
impl RemoteGateway for Gateway {
    fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        self.0.lookup(path)
    }
}
struct Node {
    facade: Arc<ParrotActorSystem>,
    remote: Arc<RemoteActorSystem>,
    actor: BoxedActorRef,
    counts: Arc<Counts>,
}
const ENGINES: [&str; 4] = ["shared", "dedicated", "sharded", "actix"];
async fn node(id: &str, engine: usize, neutral: bool, gateway_first: bool) -> Node {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let remote = RemoteActorSystem::new(
        RemoteConfig::tcp(id, Some("127.0.0.1:0".parse().unwrap()))
            .with_insecure_development()
            .with_receptionist_acl(
                AclRules::from_json(
                    r#"{"roles":{"matrix-a":["edge/real-actor"],"matrix-b":["edge/real-actor"]}}"#,
                )
                .unwrap(),
            )
            .with_swim_config(SwimConfig {
                gossip_interval: Duration::from_millis(20),
                probe_interval: Duration::from_millis(50),
                probe_timeout: Duration::from_millis(200),
                suspect_timeout: Duration::from_millis(400),
                ..Default::default()
            }),
        Arc::new(Lookup(Arc::downgrade(&facade))),
    )
    .unwrap();
    remote.start().await.unwrap();
    let gateway = Arc::new(remote.receptionist_gateway());
    if gateway_first {
        facade
            .register_receptionist_gateway(gateway.clone())
            .await
            .unwrap();
    }
    let counts = Arc::new(Counts::default());
    let actor = if engine == 3 {
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let actor = system
            .spawn_root_typed(ActixProbe(counts.clone()), EmptyConfig)
            .await
            .unwrap();
        if neutral {
            facade
                .register_runtime("engine".into(), Arc::new(system), true)
                .unwrap();
        } else {
            facade
                .register_actix_system("engine".into(), system, true)
                .await
                .unwrap();
        }
        actor
    } else {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            max_dedicated_threads: 1,
            ..Default::default()
        });
        let mode = match engine {
            0 => SchedulingMode::SharedPool {
                max_messages_per_run: 4,
            },
            1 => SchedulingMode::DedicatedThread,
            2 => SchedulingMode::Sharded {
                affinity_key: "network-receptionist".into(),
            },
            _ => unreachable!(),
        };
        let actor = system
            .spawn_at(
                ThreadProbe(counts.clone()),
                "/user/probe",
                None,
                ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        if neutral {
            facade
                .register_runtime("engine".into(), system, true)
                .unwrap();
        } else {
            facade
                .register_thread_system("engine".into(), system, true)
                .await
                .unwrap();
        }
        Box::new(actor) as BoxedActorRef
    };
    if !gateway_first {
        facade.register_receptionist_gateway(gateway).await.unwrap();
    }
    facade
        .register_remote_gateway(Arc::new(Gateway(remote.gateway())))
        .await
        .unwrap();
    ask::<()>(&actor, Command::Ready).await;
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
    Node {
        facade,
        remote,
        actor,
        counts,
    }
}
async fn ask<T: Send + 'static>(actor: &BoxedActorRef, command: Command) -> T {
    *tokio::time::timeout(LIMIT, actor.send(Box::new(command)))
        .await
        .unwrap()
        .unwrap()
        .downcast::<T>()
        .expect("probe response")
}
async fn write(node: &Node, register: bool) {
    ask::<ActorResult<()>>(
        &node.actor,
        if register {
            Command::Register
        } else {
            Command::Deregister
        },
    )
    .await
    .unwrap();
}
async fn subscribe(node: &Node) -> ReceptionistStream {
    ask::<ActorResult<ReceptionistStream>>(&node.actor, Command::Subscribe)
        .await
        .unwrap()
}
fn path(node: &Node) -> String {
    let local = node.actor.path();
    format!(
        "parrot://{}{}{}",
        node.remote.config.node_id,
        if local.starts_with("actix://") {
            "/"
        } else {
            ""
        },
        local
    )
}
async fn event(stream: &mut ReceptionistStream, node: &Node, registered: bool) -> String {
    let path = path(node);
    let expected = if registered {
        ReceptionistEvent::Registered {
            key: key(),
            remote_path: path.clone(),
        }
    } else {
        ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: path.clone(),
        }
    };
    assert_eq!(
        tokio::time::timeout(LIMIT, stream.recv()).await.unwrap(),
        Some(expected)
    );
    path
}
async fn roundtrip(from: &Node, to: &Node, received_path: &str, sequence: u64, count: usize) {
    let actor = from
        .facade
        .get_actor(&ActorPath::placeholder(received_path))
        .await
        .expect("received discovery path must resolve through the facade remote gateway");
    let response = tokio::time::timeout(LIMIT, actor.send(Box::new(Ping(sequence))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        *response.downcast::<Pong>().unwrap(),
        Pong {
            sequence,
            receiver: to.actor.path(),
            received: count,
        }
    );
}
async fn stop(node: Node) {
    tokio::time::timeout(LIMIT, node.actor.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(!node.actor.is_alive().await);
    assert_eq!(node.counts.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(node.counts.dropped.load(Ordering::SeqCst), 1);
    tokio::time::timeout(LIMIT, node.remote.shutdown())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(LIMIT, node.facade.shutdown_runtime())
        .await
        .unwrap()
        .unwrap();
    let remote = Arc::downgrade(&node.remote);
    let facade = Arc::downgrade(&node.facade);
    drop(node);
    assert!(
        facade.upgrade().is_none(),
        "lookup must not retain the facade"
    );
    assert!(
        remote.upgrade().is_none(),
        "retired gateways/tasks must release the remote system"
    );
}
async fn matrix(sender: usize) {
    for (receiver, name) in ENGINES.iter().enumerate() {
        eprintln!("SCENARIO {} -> {}", ENGINES[sender], name);
        let id_a = "matrix-a";
        let id_b = "matrix-b";
        let a = node(
            id_a,
            sender,
            receiver.is_multiple_of(2),
            sender.is_multiple_of(2),
        )
        .await;
        let b = node(
            id_b,
            receiver,
            !sender.is_multiple_of(2),
            !receiver.is_multiple_of(2),
        )
        .await;
        let mut from_a = subscribe(&a).await;
        let mut from_b = subscribe(&b).await;
        let mut observer = a
            .facade
            .receptionist_gateway()
            .unwrap()
            .subscribe(key())
            .await
            .unwrap();
        write(&b, true).await;
        event(&mut from_b, &b, true).await;
        a.remote
            .connect(&NodeAddr {
                node_id: id_b.into(),
                identity_is_explicit: true,
                scheme: "tcp".into(),
                addr: b.remote.local_addr().unwrap(),
            })
            .await
            .unwrap();
        let received_b = event(&mut from_a, &b, true).await;
        event(&mut observer, &b, true).await;
        roundtrip(&a, &b, &received_b, 101, 1).await;
        write(&a, true).await;
        event(&mut from_a, &a, true).await;
        event(&mut observer, &a, true).await;
        let received_a = event(&mut from_b, &a, true).await;
        roundtrip(&b, &a, &received_a, 202, 1).await;
        let mut snapshot = subscribe(&a).await;
        // Snapshot order is deliberately unspecified. Compare the complete set.
        let mut paths = Vec::new();
        for _ in 0..2 {
            match tokio::time::timeout(LIMIT, snapshot.recv())
                .await
                .unwrap()
                .unwrap()
            {
                ReceptionistEvent::Registered {
                    key: found,
                    remote_path,
                } => {
                    assert_eq!(found, key());
                    paths.push(remote_path);
                }
                other => panic!("unexpected snapshot event: {other:?}"),
            }
        }
        paths.sort();
        let mut expected = vec![path(&a), path(&b)];
        expected.sort();
        assert_eq!(paths, expected);
        write(&b, false).await;
        event(&mut from_a, &b, false).await;
        event(&mut observer, &b, false).await;
        event(&mut snapshot, &b, false).await;
        event(&mut from_b, &b, false).await;
        write(&b, true).await;
        let received_b = event(&mut from_a, &b, true).await;
        event(&mut observer, &b, true).await;
        event(&mut snapshot, &b, true).await;
        event(&mut from_b, &b, true).await;
        roundtrip(&a, &b, &received_b, 303, 2).await;
        assert_eq!(*a.counts.received.lock().unwrap(), vec![202]);
        assert_eq!(*b.counts.received.lock().unwrap(), vec![101, 303]);
        write(&a, false).await;
        event(&mut from_a, &a, false).await;
        event(&mut observer, &a, false).await;
        event(&mut snapshot, &a, false).await;
        event(&mut from_b, &a, false).await;
        write(&b, false).await;
        event(&mut from_a, &b, false).await;
        event(&mut observer, &b, false).await;
        event(&mut snapshot, &b, false).await;
        event(&mut from_b, &b, false).await;
        for stream in [&mut from_a, &mut from_b, &mut observer, &mut snapshot] {
            assert_eq!(
                stream.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            );
        }
        let mut fresh_a = subscribe(&a).await;
        let mut fresh_b = subscribe(&b).await;
        for stream in [&mut fresh_a, &mut fresh_b] {
            assert_eq!(
                stream.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty),
                "complete withdrawal must remove stored entries, not only emit events"
            );
        }
        stop(b).await;
        stop(a).await;
        for stream in [
            &mut from_a,
            &mut from_b,
            &mut observer,
            &mut snapshot,
            &mut fresh_a,
            &mut fresh_b,
        ] {
            assert!(
                tokio::time::timeout(LIMIT, stream.recv())
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
fn isolated(name: &str, future: impl std::future::Future<Output = ()>) {
    if std::env::var("PARROT_NETWORK_ACTOR_CASE").ok().as_deref() == Some(name) {
        actix::System::new().block_on(future);
        return;
    }
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_NETWORK_ACTOR_CASE", name)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "actor network child failed: {name}: {status}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "actor network child watchdog: {name}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn shared_to_all_real_engines() {
    isolated("shared_to_all_real_engines", matrix(0));
}
#[test]
fn dedicated_to_all_real_engines() {
    isolated("dedicated_to_all_real_engines", matrix(1));
}
#[test]
fn sharded_to_all_real_engines() {
    isolated("sharded_to_all_real_engines", matrix(2));
}
#[test]
fn actix_to_all_real_engines() {
    isolated("actix_to_all_real_engines", matrix(3));
}

#[test]
fn actor_stop_with_live_nodes_withdraws_tcp_discovery() {
    isolated(
        "actor_stop_with_live_nodes_withdraws_tcp_discovery",
        async {
            for engine in 0..4 {
                let a = node("matrix-a", 0, false, true).await;
                let b = node("matrix-b", engine, true, false).await;
                let mut local = subscribe(&b).await;
                let mut remote = subscribe(&a).await;
                a.remote
                    .connect(&NodeAddr {
                        node_id: "matrix-b".into(),
                        identity_is_explicit: true,
                        scheme: "tcp".into(),
                        addr: b.remote.local_addr().unwrap(),
                    })
                    .await
                    .unwrap();
                write(&b, true).await;
                event(&mut local, &b, true).await;
                let discovered = event(&mut remote, &b, true).await;
                roundtrip(&a, &b, &discovered, 901, 1).await;
                b.actor.stop().await.unwrap();
                event(&mut local, &b, false).await;
                event(&mut remote, &b, false).await;
                assert!(!b.actor.is_alive().await);
                assert_eq!(b.counts.dropped.load(Ordering::SeqCst), 1);
                for node in [&a, &b] {
                    assert_eq!(
                        node.facade
                            .receptionist_gateway()
                            .unwrap()
                            .subscribe(key())
                            .await
                            .unwrap()
                            .try_recv(),
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                    );
                }
                // Both nodes and TCP synchronization remain functional after the
                // service actor dies. Node shutdown cannot supply the withdrawal.
                write(&a, true).await;
                event(&mut remote, &a, true).await;
                let discovered = event(&mut local, &a, true).await;
                roundtrip(&b, &a, &discovered, 902, 1).await;
                assert_eq!(*a.counts.received.lock().unwrap(), [902]);
                assert_eq!(*b.counts.received.lock().unwrap(), [901]);
                write(&a, false).await;
                event(&mut remote, &a, false).await;
                event(&mut local, &a, false).await;
                stop(b).await;
                stop(a).await;
            }
        },
    );
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Ready {
    address: std::net::SocketAddr,
    path: String,
}
struct NodeProcess {
    child: Child,
    directory: std::path::PathBuf,
}
impl NodeProcess {
    fn spawn(engine: usize) -> Self {
        let root = std::env::var_os("PARROT_NETWORK_ACTOR_ARTIFACTS")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let directory = root.join(format!("killed-{engine}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let log = std::fs::File::create(directory.join("child.log")).unwrap();
        let child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sigkill_real_actor_peers_withdraw_services",
                    "--nocapture",
                ])
                .env("PARROT_NETWORK_KILL_DIRECTORY", &directory)
                .env("PARROT_NETWORK_KILL_ENGINE", engine.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        Self { child, directory }
    }
    async fn ready(&mut self) -> Ready {
        let deadline = tokio::time::Instant::now() + LIMIT;
        loop {
            if let Ok(bytes) = std::fs::read(self.directory.join("ready.json"))
                && let Ok(ready) = serde_json::from_slice(&bytes)
            {
                return ready;
            }
            assert!(
                self.child.0.try_wait().unwrap().is_none(),
                "peer exited before readiness: {}",
                self.directory.display()
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "peer readiness deadline: {}",
                self.directory.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    fn kill_and_reap(&mut self) {
        self.child.0.kill().unwrap();
        let status = self.child.0.wait().unwrap();
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(9), "must execute actual SIGKILL");
            status.signal()
        };
        #[cfg(not(unix))]
        let signal: Option<i32> = None;
        assert!(!status.success());
        std::fs::write(
            self.directory.join("killed.json"),
            serde_json::to_vec(
                &serde_json::json!({"pid": self.child.0.id(), "status": status.to_string(),
                "signal": signal, "success": status.success(), "reaped": true}),
            )
            .unwrap(),
        )
        .unwrap();
    }
}
async fn raw_event(stream: &mut ReceptionistStream, path: &str, registered: bool) {
    let expected = if registered {
        ReceptionistEvent::Registered {
            key: key(),
            remote_path: path.into(),
        }
    } else {
        ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: path.into(),
        }
    };
    assert_eq!(
        tokio::time::timeout(LIMIT, stream.recv()).await.unwrap(),
        Some(expected)
    );
}
async fn killed_peers() {
    if let Some(directory) = std::env::var_os("PARROT_NETWORK_KILL_DIRECTORY") {
        let engine = std::env::var("PARROT_NETWORK_KILL_ENGINE")
            .unwrap()
            .parse()
            .unwrap();
        let peer = node("matrix-b", engine, false, true).await;
        write(&peer, true).await;
        let ready = Ready {
            address: peer.remote.local_addr().unwrap(),
            path: path(&peer),
        };
        std::fs::write(
            std::path::PathBuf::from(directory).join("ready.json"),
            serde_json::to_vec(&ready).unwrap(),
        )
        .unwrap();
        std::future::pending::<()>().await;
        drop(peer);
        return;
    }
    for (engine, name) in ENGINES.iter().enumerate() {
        eprintln!("SCENARIO SIGKILL {name}");
        let a = node("matrix-a", 0, true, false).await;
        let mut events = subscribe(&a).await;
        write(&a, true).await;
        event(&mut events, &a, true).await;
        let mut process = NodeProcess::spawn(engine);
        let ready = process.ready().await;
        a.remote
            .connect(&NodeAddr::tcp("matrix-b", ready.address))
            .await
            .unwrap();
        raw_event(&mut events, &ready.path, true).await;
        let discovered = a
            .facade
            .get_actor(&ActorPath::placeholder(&ready.path))
            .await
            .unwrap();
        let reply = tokio::time::timeout(LIMIT, discovered.send(Box::new(Ping(404))))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            *reply.downcast::<Pong>().unwrap(),
            Pong {
                sequence: 404,
                receiver: if engine == 3 {
                    ready.path.strip_prefix("parrot://matrix-b/").unwrap()
                } else {
                    ready.path.strip_prefix("parrot://matrix-b").unwrap()
                }
                .into(),
                received: 1,
            }
        );
        process.kill_and_reap();
        raw_event(&mut events, &ready.path, false).await;
        tokio::time::timeout(LIMIT, async {
            loop {
                if a.remote
                    .membership
                    .lock()
                    .await
                    .members
                    .get("matrix-b")
                    .is_some_and(|member| member.status == parrot_remote::swim::MemberStatus::Dead)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("actual killed peer must be observed Dead without state injection");
        let mut snapshot = subscribe(&a).await;
        event(&mut snapshot, &a, true).await;
        assert_eq!(
            snapshot.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
        let reply = tokio::time::timeout(LIMIT, a.actor.send(Box::new(Ping(505))))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            *reply.downcast::<Pong>().unwrap(),
            Pong {
                sequence: 505,
                receiver: a.actor.path(),
                received: 1,
            }
        );
        assert_eq!(*a.counts.received.lock().unwrap(), vec![505]);
        write(&a, false).await;
        event(&mut events, &a, false).await;
        event(&mut snapshot, &a, false).await;
        stop(a).await;
        assert!(
            tokio::time::timeout(LIMIT, events.recv())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            tokio::time::timeout(LIMIT, snapshot.recv())
                .await
                .unwrap()
                .is_none()
        );
    }
}
#[test]
fn sigkill_real_actor_peers_withdraw_services() {
    isolated("sigkill_real_actor_peers_withdraw_services", killed_peers());
}
