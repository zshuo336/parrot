//! Guest observations cross a real TCP connection and an assembled Wire in a
//! separate process. Client and server cannot share a Rust trace scope.
#![cfg(all(feature = "host", feature = "wasm"))]

use parrot::thread::ThreadActorSystem;
use parrot_api::{
    address::ActorRef,
    trace::{self, SpanKind, SpanStatus, TraceContext, TraceRecorder},
    types::BoxedActorRef,
};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer},
    host::{
        wasm::{WasmAsk, WasmReply, WasmTell},
        ThreadPropsSpawner,
    },
    manifest::{AppManifest, ComponentSpec},
    planner::{plan, LocalTopology},
};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorRef, RemoteActorSystem, RemoteConfig};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

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
        panic!("unexpected gateway")
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        panic!("unexpected gateway")
    }
}

struct Lookup(Vec<BoxedActorRef>);

#[derive(serde::Serialize, serde::Deserialize)]
struct RecordedSpan {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    parent_span_id: Option<[u8; 8]>,
    target: String,
}

#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        match path {
            "/wire-left" => self.0.first().map(|actor| actor.clone_boxed()),
            "/wire-right" => self.0.get(1).map(|actor| actor.clone_boxed()),
            _ => None,
        }
    }
}

#[test]
fn wasm_wire_server_process() {
    let Some(directory) = std::env::var_os("PARROT_WASM_WIRE_SERVER") else {
        return;
    };
    let directory = PathBuf::from(directory);
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
        let guest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../parrot-wasm/tests/fixtures/trace-component.wasm").canonicalize().unwrap();
        let digest = format!("{:x}", Sha256::digest(std::fs::read(&guest).unwrap()));
        let uri = url::Url::from_file_path(guest).unwrap();
        let mut source = "name = 'wasm-wire'\nversion = '1.0.0'\n".to_owned();
        for name in ["left", "right"] {
            source.push_str(&format!("[[components]]\nname = '{name}'\nengine = 'parrot'\nartifact = {{ Wasm = {{ digest = '{digest}', uri = '{uri}' }} }}\n"));
        }
        for name in ["left", "right"] {
            source.push_str(&format!("[[wiring]]\nfrom = '{name}:/user/input'\nto = '{name}:/user/{name}'\nqos = 'lan'\n"));
        }
        let manifest = AppManifest::from_toml(&source).unwrap();
        std::fs::write(directory.join("application.toml"), source).unwrap();
        let engine = ThreadActorSystem::shared(Default::default());
        let spawner = ThreadPropsSpawner::new(engine.clone()).with_wasm_limits(
            parrot_wasm::WasmConfig { epoch_deadline: 1000, ..Default::default() }, Default::default());
        let deployer = LocalDeployer { topology: &LocalTopology, gateway_factory: &NoGateway, parrot_spawner: &spawner };
        let mut assembly = AssemblingContext::new();
        assembly.assemble(&plan(&manifest, &LocalTopology).unwrap(), &deployer, &manifest, &Default::default()).await.unwrap();
        let recorder = TraceRecorder::new(128).unwrap();
        let remote = RemoteActorSystem::new(
            RemoteConfig::tcp("wasm-wire-server", Some("127.0.0.1:0".parse().unwrap())),
            Arc::new(Lookup(vec![assembly.wired_ref("left:/user/input").unwrap(), assembly.wired_ref("right:/user/input").unwrap()])),
        ).unwrap();
        remote.set_trace_recorder(Some(recorder.clone()));
        remote.start().await.unwrap();
        std::fs::write(directory.join("ready.json"), serde_json::to_vec(&serde_json::json!({
            "pid": std::process::id(), "address": remote.local_addr().unwrap().to_string(),
        })).unwrap()).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        while !directory.join("stop").exists() {
            assert!(tokio::time::Instant::now() < deadline, "Wasm Wire server watchdog");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        remote.shutdown().await.unwrap();
        assembly.teardown(&deployer).await.unwrap();
        engine.shutdown_internal().await.unwrap();
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal && snapshot.counts.overwritten == 0);
        let records: Vec<_> = snapshot.records.into_iter().map(|record| {
            assert_eq!(record.kind, SpanKind::DynamicHandler);
            assert_eq!(record.status, SpanStatus::Ok);
            RecordedSpan { trace_id: record.trace_id, span_id: record.span_id,
                parent_span_id: record.parent_span_id, target: record.target }
        }).collect();
        std::fs::write(directory.join("spans.json"), serde_json::to_vec(&records).unwrap()).unwrap();
    });
}

struct Server {
    child: Child,
    directory: tempfile::TempDir,
    address: std::net::SocketAddr,
}
impl Server {
    fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "wasm_wire_server_process", "--nocapture"])
            .env("PARROT_WASM_WIRE_SERVER", directory.path())
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(directory.path().join("stdout.log")).unwrap())
            .stderr(std::fs::File::create(directory.path().join("stderr.log")).unwrap())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            directory,
            address: "127.0.0.1:0".parse().unwrap(),
        };
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(bytes) = std::fs::read(server.directory.path().join("ready.json")) {
                if let Ok(ready) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    assert_eq!(ready["pid"].as_u64().unwrap(), u64::from(server.child.id()));
                    assert_ne!(server.child.id(), std::process::id());
                    server.address = ready["address"].as_str().unwrap().parse().unwrap();
                    return server;
                }
            }
            assert!(
                Instant::now() < end && server.child.try_wait().unwrap().is_none(),
                "server startup failed: {}",
                server.stderr()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn stderr(&self) -> String {
        std::fs::read_to_string(self.directory.path().join("stderr.log")).unwrap()
    }
    fn stop(&mut self) -> Vec<RecordedSpan> {
        std::fs::write(self.directory.path().join("stop"), b"stop").unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "server failed: {}", self.stderr());
                return serde_json::from_slice(
                    &std::fs::read(self.directory.path().join("spans.json")).unwrap(),
                )
                .unwrap();
            }
            assert!(
                Instant::now() < end,
                "server shutdown watchdog: {}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn save(&self, name: &str, observations: &impl serde::Serialize) {
        std::fs::write(
            self.directory.path().join(name),
            serde_json::to_vec_pretty(observations).unwrap(),
        )
        .unwrap();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(root) = std::env::var_os("PARROT_WASM_WIRE_EVIDENCE") {
            let destination = PathBuf::from(root).join(self.directory.path().file_name().unwrap());
            if let Err(error) = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(&destination)?;
                for entry in std::fs::read_dir(self.directory.path())? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        std::fs::copy(entry.path(), destination.join(entry.file_name()))?;
                    }
                }
                Ok(())
            })() {
                eprintln!("Wasm Wire evidence preservation: {error}");
            }
        }
    }
}

async fn client(server: &Server) -> Arc<RemoteActorSystem> {
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp(format!("wasm-wire-client-{}", server.child.id()), None),
        Arc::new(Lookup(vec![])),
    )
    .unwrap();
    client.start().await.unwrap();
    client
        .connect(&NodeAddr::tcp("wasm-wire-server", server.address))
        .await
        .unwrap();
    client
}
async fn ask(
    actor: &RemoteActorRef,
    key: &str,
    parent: Option<&TraceContext>,
    unrelated: &TraceRecorder,
) -> Vec<u8> {
    let operation = trace::in_scope(parent.cloned(), || {
        actor.send(Box::new(WasmAsk {
            type_key: key.into(),
            payload: vec![],
        }))
    });
    unrelated
        .root()
        .scope(operation)
        .await
        .unwrap()
        .downcast::<WasmReply>()
        .unwrap()
        .0
}
fn assert_parent(record: &RecordedSpan, root: &TraceContext, observed: &[u8], target: &str) {
    assert_eq!(record.target, target);
    assert_eq!(record.trace_id, root.trace_id());
    assert_eq!(record.parent_span_id, Some(root.span_id()));
    assert_ne!(record.span_id, root.span_id());
    assert_eq!(
        observed,
        [record.trace_id.as_slice(), record.span_id.as_slice()].concat(),
        "TCP/Wire Wasm guest must observe the actual server handler as parent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_wire_wasm_asks_preserve_call_identity_and_explicit_none() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut server = Server::start();
        let client = client(&server).await;
        let actor = client.remote_ref("parrot://wasm-wire-server/wire-left").unwrap();
        let original = TraceRecorder::new(16).unwrap();
        let unrelated = TraceRecorder::new(16).unwrap();
        let roots = [original.root(), original.root()];
        let mut observations = Vec::new();
        for parent in [Some(&roots[0]), None, Some(&roots[1]), Some(&roots[0]), None] {
            let observed = ask(&actor, "parent", parent, &unrelated).await;
            if let Some(parent) = parent { observations.push((parent.clone(), observed)); }
            else { assert!(observed.is_empty(), "TCP/Wire Wasm inherited a previous or polling trace"); }
            assert!(ask(&actor, "start-parent", parent, &unrelated).await.is_empty());
        }
        assert!(original.snapshot().records.is_empty() && unrelated.snapshot().records.is_empty());
        assert!(TraceContext::current().is_none());
        client.shutdown().await.unwrap();
        let records = server.stop();
        assert_eq!(records.len(), 6, "only the three traced ASK pairs should record spans");
        for (index, (root, observed)) in observations.iter().enumerate() {
            assert_parent(&records[index * 2], root, observed, "/user/left");
            let lifecycle_query = &records[index * 2 + 1];
            assert_eq!(lifecycle_query.parent_span_id, Some(root.span_id()));
            assert_ne!(lifecycle_query.span_id, records[index * 2].span_id);
        }
        server.save("ask-observations.json", &observations.iter().map(|(root, bytes)| serde_json::json!({"root_trace":root.trace_id(),"root_span":root.span_id(),"guest_parent":bytes})).collect::<Vec<_>>());
    }).await.expect("TCP/Wire Wasm ASK liveness deadline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_wire_wasm_tells_execute_under_independent_sibling_parents() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut server = Server::start();
        let client = client(&server).await;
        let actors = [
            client
                .remote_ref("parrot://wasm-wire-server/wire-left")
                .unwrap(),
            client
                .remote_ref("parrot://wasm-wire-server/wire-right")
                .unwrap(),
        ];
        let original = TraceRecorder::new(16).unwrap();
        let unrelated = TraceRecorder::new(16).unwrap();
        let roots = [original.root(), original.root()];
        let operations = [0, 1].map(|index| {
            roots[index].in_scope(|| {
                actors[index].deliver(Box::new(WasmTell {
                    type_key: "remember".into(),
                    payload: vec![],
                }))
            })
        });
        let [a, b] = operations;
        let (a, b) = unrelated.root().scope(async { tokio::join!(a, b) }).await;
        a.unwrap();
        b.unwrap();
        let mut observations = Vec::new();
        for actor in &actors {
            // Delivery only acknowledges admission. Read the guest's saved
            // identity to prove the TELL effect, without assuming wire ordering.
            let observed = loop {
                let saved = ask(actor, "saved-parent", None, &unrelated).await;
                if !saved.is_empty() {
                    break saved;
                }
                tokio::task::yield_now().await;
            };
            observations.push(observed);
            assert!(ask(actor, "parent", None, &unrelated).await.is_empty());
            trace::in_scope(None, || {
                actor.deliver(Box::new(WasmTell {
                    type_key: "remember".into(),
                    payload: vec![],
                }))
            })
            .await
            .unwrap();
            loop {
                if ask(actor, "saved-parent", None, &unrelated)
                    .await
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
        assert!(original.snapshot().records.is_empty() && unrelated.snapshot().records.is_empty());
        client.shutdown().await.unwrap();
        let records = server.stop();
        assert_eq!(
            records.len(),
            2,
            "untraced effects/barriers must not create handler spans"
        );
        for index in [0, 1] {
            let matches: Vec<_> = records
                .iter()
                .filter(|record| record.trace_id == roots[index].trace_id())
                .collect();
            assert_eq!(matches.len(), 1);
            assert_parent(
                matches[0],
                &roots[index],
                &observations[index],
                if index == 0 {
                    "/user/left"
                } else {
                    "/user/right"
                },
            );
        }
        assert!(TraceContext::current().is_none());
        server.save("tell-observations.json", &observations);
    })
    .await
    .expect("TCP/Wire Wasm TELL liveness deadline");
}
