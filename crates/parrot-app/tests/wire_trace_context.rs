//! Actual OS processes and native actor handlers; no shared fixture context can cross this boundary.
use parrot::thread::{ThreadActorConfig, ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    trace::{TraceContext, TraceRecorder},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorRef, RemoteActorSystem, RemoteConfig};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[derive(Serialize, Deserialize)]
struct Work {
    value: i64,
    delay_ms: u64,
}
#[derive(Serialize, Deserialize, Debug)]
struct Observation {
    process: u32,
    value: i64,
    trace: Option<[u8; 16]>,
    span: Option<[u8; 8]>,
    parent: Option<[u8; 8]>,
}
#[derive(Serialize, Deserialize, Debug)]
struct Reply(Vec<Observation>);
macro_rules! codec {($ty:ty,$key:literal)=>{parrot_api::message::inventory::submit!{parrot_api::message::CodecRegistration{type_key:$key,type_id:std::any::TypeId::of::<$ty>(),encode:|m|parrot_api::message::serde_remote_serialize(m.downcast_ref::<$ty>().ok_or("wrong type")?),decode:|b|Ok(Box::new(parrot_api::message::serde_remote_deserialize::<$ty>(b)?) as BoxedMessage)}}};}
codec!(Work, "bin:wire-trace.Work#v1");
codec!(Reply, "bin:wire-trace.Reply#v1");
struct Counter {
    value: i64,
    next: Arc<Mutex<Option<RemoteActorRef>>>,
    journal: PathBuf,
}
fn append(path: &Path, value: impl Serialize) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(f, "{}", serde_json::to_string(&value).unwrap()).unwrap();
}
// Persist construction and disposal independently; assertions run in the parent
// process so a test assertion cannot turn a handler panic into a double panic.
struct Invocation {
    journal: PathBuf,
    expected: Option<TraceContext>,
}
fn identity(context: Option<&TraceContext>) -> serde_json::Value {
    serde_json::json!({"trace":context.map(TraceContext::trace_id),"span":context.map(TraceContext::span_id),"parent":context.and_then(TraceContext::parent_span_id)})
}
impl Drop for Invocation {
    fn drop(&mut self) {
        append(
            &self.journal,
            serde_json::json!({"event":"disposed","expected":identity(self.expected.as_ref()),"actual":identity(TraceContext::current().as_ref())}),
        );
    }
}
impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let constructed = TraceContext::current();
        let journal = self.journal.with_file_name("invocations.jsonl");
        append(
            &journal,
            serde_json::json!({"event":"constructed","actual":identity(constructed.as_ref())}),
        );
        let invocation = Invocation {
            journal,
            expected: constructed.clone(),
        };
        Box::pin(async move {
            let _invocation = invocation;
            let work = message.downcast::<Work>().unwrap();
            tokio::time::sleep(Duration::from_millis(work.delay_ms)).await;
            assert_eq!(TraceContext::current(), constructed);
            if work.value == i64::MIN {
                return Err(parrot_api::errors::ActorError::MessageHandlingError(
                    "intentional traced failure".into(),
                ));
            }
            assert_ne!(work.value, i64::MAX, "intentional traced panic");
            self.value += work.value;
            let observed = Observation {
                process: std::process::id(),
                value: self.value,
                trace: constructed.as_ref().map(TraceContext::trace_id),
                span: constructed.as_ref().map(TraceContext::span_id),
                parent: constructed.as_ref().and_then(TraceContext::parent_span_id),
            };
            append(&self.journal, &observed);
            let mut observations = vec![observed];
            let next = self.next.lock().unwrap().clone();
            if let Some(next) = next {
                observations.extend(
                    next.send(Box::new(*work))
                        .await?
                        .downcast::<Reply>()
                        .unwrap()
                        .0,
                );
            }
            Ok(Box::new(Reply(observations)) as BoxedMessage)
        })
    }
}
impl Drop for Counter {
    fn drop(&mut self) {
        append(&self.journal, "disposed");
    }
}
struct Lookup(BoxedActorRef);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/counter").then(|| self.0.clone_boxed())
    }
}
#[derive(Serialize, Deserialize)]
struct ServerConfig {
    node: String,
    directory: PathBuf,
    next: Option<(String, String)>,
    record: bool,
    hub: bool,
}
#[test]
fn server_process() {
    let Some(config) = std::env::var_os("PARROT_TRACE_SERVER_CONFIG") else {
        return;
    };
    let config: ServerConfig = serde_json::from_str(config.to_str().unwrap()).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move{
  let engine=ThreadActorSystem::shared(Default::default());let next=Arc::new(Mutex::new(None));
  let actor=engine.spawn_at(Counter{value:0,next:next.clone(),journal:config.directory.join("business.jsonl")},"/counter",None,ThreadActorConfig::default()).await.unwrap();
  let capture_path=config.directory.join("capture.jsonl");
  let recording=tokio::task::spawn_blocking(move||parrot_app::debug::Recording::start(&capture_path,Default::default(),Default::default())).await.unwrap().unwrap();
  let mut remote_config=RemoteConfig::tcp(&config.node,Some("127.0.0.1:0".parse().unwrap())).with_capture(recording.capture());remote_config.heartbeat_enabled=false;
  if config.hub{remote_config=remote_config.with_insecure_development().with_role(parrot_remote::handshake::TopologyRole::Hub);}
  let remote=RemoteActorSystem::new(remote_config,Arc::new(Lookup(Box::new(actor)))).unwrap();
  let recorder=TraceRecorder::new(128).unwrap();if config.record{remote.set_trace_recorder(Some(recorder.clone()));}
  remote.start().await.unwrap();
  if let Some((node,address))=&config.next{remote.connect(&NodeAddr::tcp(node,address.parse().unwrap())).await.unwrap();*next.lock().unwrap()=Some(remote.remote_ref(&format!("parrot://{node}/counter")).unwrap());}
  std::fs::write(config.directory.join("ready.json"),serde_json::to_vec(&serde_json::json!({"node":config.node,"hub":config.hub,"address":remote.local_addr().unwrap().to_string(),"pid":std::process::id()})).unwrap()).unwrap();
  let deadline=tokio::time::Instant::now()+Duration::from_secs(45);
  while !config.directory.join("stop").exists(){assert!(tokio::time::Instant::now()<deadline,"server watchdog");tokio::time::sleep(Duration::from_millis(10)).await;}
  remote.shutdown().await.unwrap();engine.shutdown_internal().await.unwrap();tokio::task::spawn_blocking(move||recording.finish()).await.unwrap().unwrap();recorder.close();let snapshot=recorder.drain();assert!(snapshot.terminal);
  std::fs::write(config.directory.join("spans.json"),parrot_app::debug::export_otlp_json(&snapshot,&config.node,1024*1024).unwrap()).unwrap();
  std::fs::write(config.directory.join("terminal.json"),serde_json::to_vec(&serde_json::json!({"completed":snapshot.counts.completed,"overwritten":snapshot.counts.overwritten,"terminal":snapshot.terminal})).unwrap()).unwrap();
 });
}
struct Server {
    child: Child,
    directory: tempfile::TempDir,
    ready: serde_json::Value,
}
impl Server {
    fn start(node: &str, next: Option<(String, String)>, record: bool, hub: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            node: node.into(),
            directory: directory.path().into(),
            next,
            record,
            hub,
        };
        std::fs::write(
            directory.path().join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "server_process", "--nocapture"])
            .env(
                "PARROT_TRACE_SERVER_CONFIG",
                serde_json::to_string(&config).unwrap(),
            )
            .stdout(std::fs::File::create(directory.path().join("stdout.log")).unwrap())
            .stderr(std::fs::File::create(directory.path().join("stderr.log")).unwrap())
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            directory,
            ready: serde_json::Value::Null,
        };
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(bytes) = std::fs::read(server.directory.path().join("ready.json")) {
                if let Ok(ready) = serde_json::from_slice(&bytes) {
                    server.ready = ready;
                    break;
                }
            }
            assert!(
                Instant::now() < end && server.child.try_wait().unwrap().is_none(),
                "server startup failed: {}",
                std::fs::read_to_string(server.directory.path().join("stderr.log")).unwrap()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        server
    }
    fn endpoint(&self) -> (String, String) {
        (
            self.ready["node"].as_str().unwrap().into(),
            self.ready["address"].as_str().unwrap().into(),
        )
    }
    fn stop(&mut self) {
        std::fs::write(self.directory.path().join("stop"), b"stop").unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "{}",
                    std::fs::read_to_string(self.directory.path().join("stderr.log")).unwrap()
                );
                break;
            }
            assert!(Instant::now() < end, "shutdown watchdog");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    async fn wait_value(&self, value: i64) {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let observed = std::fs::read_to_string(self.directory.path().join("business.jsonl"))
                .unwrap_or_default();
            if observed
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|entry| entry["value"] == value)
            {
                return;
            }
            assert!(
                Instant::now() < end,
                "business value {value} never observed: {observed}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    fn spans(&self) -> Vec<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_slice(
            &std::fs::read(self.directory.path().join("spans.json")).unwrap(),
        )
        .unwrap();
        v["resourceSpans"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|r| r["scopeSpans"].as_array().unwrap())
            .flat_map(|s| s["spans"].as_array().unwrap().iter().cloned())
            .collect()
    }
    fn invocations(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.directory.path().join("invocations.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect()
    }
    async fn wait_constructed(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self
                    .invocations()
                    .iter()
                    .any(|v| v["event"] == "constructed")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    async fn wait_disposed(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if std::fs::read_to_string(self.directory.path().join("business.jsonl"))
                    .unwrap_or_default()
                    .lines()
                    .any(|s| s == "\"disposed\"")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    fn observations(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.directory.path().join("business.jsonl"))
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(out) = std::env::var_os("PARROT_WIRE_TRACE_EVIDENCE") {
            let saved = PathBuf::from(out).join(self.directory.path().file_name().unwrap());
            if let Err(error) = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(&saved)?;
                for entry in std::fs::read_dir(self.directory.path())? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        std::fs::copy(entry.path(), saved.join(entry.file_name()))?;
                    }
                }
                Ok(())
            })() {
                eprintln!("evidence preservation: {error}");
            }
        }
    }
}
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
async fn client(server: &Server) -> Arc<RemoteActorSystem> {
    let mut config = RemoteConfig::tcp(format!("client-{}", server.child.id()), None);
    // This isolated plaintext fixture explicitly trusts its configured hub.
    if server.ready["hub"] == true {
        config = config.with_insecure_development();
    }
    let remote = RemoteActorSystem::new(config, Arc::new(Empty)).unwrap();
    remote.start().await.unwrap();
    let (node, address) = server.endpoint();
    remote
        .connect(&NodeAddr::tcp(node, address.parse().unwrap()))
        .await
        .unwrap();
    remote
}
fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_processes_preserve_parent_chain_and_isolate_untraced_requests() {
    let mut leaf = Server::start("leaf", None, true, false);
    let mut middle = Server::start("middle", Some(leaf.endpoint()), true, false);
    let remote = client(&middle).await;
    let target = remote.remote_ref("parrot://middle/counter").unwrap();
    let root = TraceContext::root();
    let operation = root.in_scope(|| {
        target.send(Box::new(Work {
            value: 2,
            delay_ms: 1,
        }))
    });
    let other = TraceContext::root();
    let reply = other
        .scope(operation)
        .await
        .unwrap()
        .downcast::<Reply>()
        .unwrap();
    assert_eq!(reply.0.len(), 2);
    let (a, b) = (&reply.0[0], &reply.0[1]);
    assert_eq!(a.process, middle.child.id());
    assert_eq!(b.process, leaf.child.id());
    assert_ne!(a.process, std::process::id());
    assert_ne!(a.process, b.process);
    assert_eq!(a.trace, Some(root.trace_id()));
    assert_eq!(b.trace, Some(root.trace_id()));
    assert_eq!(a.parent, Some(root.span_id()));
    assert_eq!(b.parent, a.span);
    assert_ne!(a.span, b.span);
    assert_eq!(a.value, 2);
    assert_eq!(b.value, 2);
    assert!(TraceContext::current().is_none());
    let plain = target
        .send(Box::new(Work {
            value: 3,
            delay_ms: 0,
        }))
        .await
        .unwrap()
        .downcast::<Reply>()
        .unwrap();
    assert!(plain
        .0
        .iter()
        .all(|o| o.trace.is_none() && o.span.is_none() && o.parent.is_none() && o.value == 5));
    remote.shutdown().await.unwrap();
    middle.stop();
    leaf.stop();
    for (server, observation) in [(&middle, a), (&leaf, b)] {
        let spans = server.spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0]["traceId"], hex(root.trace_id()));
        assert_eq!(spans[0]["spanId"], hex(observation.span.unwrap()));
        assert_eq!(spans[0]["parentSpanId"], hex(observation.parent.unwrap()));
        assert_eq!(spans[0]["status"]["code"], 1);
        assert_eq!(server.observations().last().unwrap(), "disposed");
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_and_completion_tell_execute_with_their_original_call_time_parent() {
    let mut server = Server::start("tell", None, true, false);
    let remote = client(&server).await;
    let target = remote.remote_ref("parrot://tell/counter").unwrap();
    let ordinary = TraceContext::root();
    let completion = TraceContext::root();
    let pending = ordinary.in_scope(|| {
        target.deliver(Box::new(Work {
            value: 2,
            delay_ms: 1,
        }))
    });
    completion.scope(pending).await.unwrap();
    server.wait_value(2).await;
    completion
        .in_scope(|| {
            target.deliver_with_ack(
                Box::new(Work {
                    value: 3,
                    delay_ms: 1,
                }),
                Duration::from_secs(5),
            )
        })
        .await
        .unwrap();
    let value = target
        .send(Box::new(Work {
            value: 0,
            delay_ms: 0,
        }))
        .await
        .unwrap()
        .downcast::<Reply>()
        .unwrap();
    assert_eq!(value.0[0].value, 5);
    remote.shutdown().await.unwrap();
    server.stop();
    let observations = server.observations();
    assert_eq!(observations.len(), 4);
    assert_eq!(
        observations[0]["trace"],
        serde_json::to_value(ordinary.trace_id()).unwrap()
    );
    assert_eq!(
        observations[0]["parent"],
        serde_json::to_value(ordinary.span_id()).unwrap()
    );
    assert_eq!(
        observations[1]["trace"],
        serde_json::to_value(completion.trace_id()).unwrap()
    );
    assert_eq!(
        observations[1]["parent"],
        serde_json::to_value(completion.span_id()).unwrap()
    );
    assert!(observations[2]["trace"].is_null());
    assert_eq!(server.spans().len(), 2);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hub_relay_preserves_original_parent_without_fabricating_local_handler_spans() {
    let mut leaf = Server::start("relay-leaf", None, true, false);
    let mut hub = Server::start("hub", Some(leaf.endpoint()), true, true);
    let remote = client(&hub).await;
    let target = remote.remote_ref("parrot://relay-leaf/counter").unwrap();
    let root = TraceContext::root();
    let reply = root
        .in_scope(|| {
            target.send(Box::new(Work {
                value: 4,
                delay_ms: 0,
            }))
        })
        .await
        .unwrap()
        .downcast::<Reply>()
        .unwrap();
    assert_eq!(reply.0.len(), 1);
    assert_eq!(reply.0[0].process, leaf.child.id());
    assert_eq!(reply.0[0].parent, Some(root.span_id()));
    assert_eq!(reply.0[0].trace, Some(root.trace_id()));
    root.in_scope(|| {
        target.deliver_with_ack(
            Box::new(Work {
                value: 3,
                delay_ms: 0,
            }),
            Duration::from_secs(5),
        )
    })
    .await
    .unwrap();
    root.in_scope(|| {
        target.deliver(Box::new(Work {
            value: 2,
            delay_ms: 0,
        }))
    })
    .await
    .unwrap();
    leaf.wait_value(9).await;
    let query = target
        .send(Box::new(Work {
            value: 0,
            delay_ms: 0,
        }))
        .await
        .unwrap()
        .downcast::<Reply>()
        .unwrap();
    assert_eq!(query.0[0].value, 9);
    remote.shutdown().await.unwrap();
    hub.stop();
    leaf.stop();
    assert!(hub.spans().is_empty());
    assert_eq!(hub.observations(), vec![serde_json::json!("disposed")]);
    let spans = leaf.spans();
    assert_eq!(spans.len(), 3);
    assert!(spans
        .iter()
        .all(|s| s["traceId"] == hex(root.trace_id()) && s["parentSpanId"] == hex(root.span_id())));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recorded_causal_requests_replay_business_bytes_with_fresh_local_identity() {
    let mut server = Server::start("recorded", None, true, false);
    let remote = client(&server).await;
    let target = remote.remote_ref("parrot://recorded/counter").unwrap();
    let original = TraceContext::root();
    for value in [2, 3] {
        original
            .in_scope(|| target.send(Box::new(Work { value, delay_ms: 0 })))
            .await
            .unwrap();
    }
    original
        .in_scope(|| {
            target.deliver_with_ack(
                Box::new(Work {
                    value: 4,
                    delay_ms: 0,
                }),
                Duration::from_secs(5),
            )
        })
        .await
        .unwrap();
    original
        .in_scope(|| {
            target.deliver(Box::new(Work {
                value: 5,
                delay_ms: 0,
            }))
        })
        .await
        .unwrap();
    server.wait_value(14).await;
    remote.shutdown().await.unwrap();
    server.stop();
    let log =
        parrot_app::debug::ReplayLog::load_capture(&server.directory.path().join("capture.jsonl"))
            .unwrap();
    assert_eq!(
        log.all()
            .iter()
            .filter(|r| r.dir == "rx"
                && matches!(
                    r.frame_type,
                    parrot_remote::frame::frame_type::ASK | parrot_remote::frame::frame_type::TELL
                )
                && r.wire.as_ref().unwrap().flags & parrot_remote::frame::flags::TRACE_CONTEXT != 0)
            .count(),
        4
    );
    for (index, fresh) in [None, Some(TraceContext::root())].into_iter().enumerate() {
        let engine = ThreadActorSystem::shared(Default::default());
        let journal = server
            .directory
            .path()
            .join(format!("replayed-{index}.jsonl"));
        let actor = engine
            .spawn_at(
                Counter {
                    value: 0,
                    next: Arc::new(Mutex::new(None)),
                    journal: journal.clone(),
                },
                "/counter",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        let lookup = Lookup(Box::new(actor));
        let report = parrot_api::trace::scope(
            fresh.clone(),
            log.replay_local(
                &lookup,
                parrot_app::debug::ReplayOptions {
                    target_node: Some("recorded".into()),
                    tell_mode: parrot_app::debug::ReplayTellMode::Completion,
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap();
        assert_eq!(report.delivered, 4);
        assert_eq!(report.replies.len(), 4);
        for (reply, expected) in report.replies.iter().zip([2, 5]) {
            let value: Reply =
                parrot_api::message::serde_remote_deserialize(&reply.payload).unwrap();
            assert_eq!(value.0[0].value, expected);
            assert_eq!(value.0[0].process, std::process::id());
        }
        for reply in &report.replies[2..] {
            assert_eq!(reply.type_key, parrot_remote::frame::TELL_COMPLETION_KEY);
            assert!(reply.payload.is_empty());
        }
        engine.shutdown_internal().await.unwrap();
        let observed: Vec<serde_json::Value> = std::fs::read_to_string(journal)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(observed.len(), 5);
        assert_eq!(observed[4], "disposed");
        for (entry, expected) in observed.iter().zip([2, 5, 9, 14]) {
            assert_eq!(entry["value"], expected);
            assert_eq!(
                entry["trace"],
                serde_json::to_value(fresh.as_ref().map(TraceContext::trace_id)).unwrap()
            );
            assert_eq!(
                entry["parent"],
                serde_json::to_value(fresh.as_ref().map(TraceContext::span_id)).unwrap()
            );
            assert_ne!(
                entry["trace"],
                serde_json::to_value(original.trace_id()).unwrap()
            );
        }
    }
}
async fn write_frame(stream: &mut tokio::net::TcpStream, frame: parrot_remote::frame::Frame) {
    use tokio::io::AsyncWriteExt;
    let mut bytes = bytes::BytesMut::new();
    frame.encode(&mut bytes).unwrap();
    stream.write_all(&bytes).await.unwrap();
}
async fn read_frame(stream: &mut tokio::net::TcpStream) -> Option<parrot_remote::frame::Frame> {
    use tokio::io::AsyncReadExt;
    let mut prefix = [0; 4];
    if stream.read_exact(&mut prefix).await.is_err() {
        return None;
    }
    let n = u32::from_le_bytes(prefix) as usize;
    assert!(n <= parrot_remote::frame::MAX_FRAME_LEN as usize);
    let mut bytes = bytes::BytesMut::from(&prefix[..]);
    bytes.resize(n + 4, 0);
    stream.read_exact(&mut bytes[4..]).await.unwrap();
    parrot_remote::frame::Frame::decode(&mut bytes).unwrap()
}
async fn raw_connection(server: &Server, causal: bool) -> tokio::net::TcpStream {
    use parrot_remote::{
        frame::Frame,
        handshake::{caps, HandshakeBody},
    };
    let mut stream = tokio::net::TcpStream::connect(server.endpoint().1)
        .await
        .unwrap();
    let mut bytes = bytes::BytesMut::new();
    HandshakeBody {
        node_id: "raw-peer".into(),
        capabilities: caps::BIN
            | caps::TELL_COMPLETION
            | if causal { caps::TRACE_CONTEXT } else { 0 },
        ..Default::default()
    }
    .encode_tlv(&mut bytes)
    .unwrap();
    write_frame(&mut stream, Frame::handshake(bytes.freeze())).await;
    assert_eq!(
        read_frame(&mut stream).await.unwrap().header.frame_type,
        parrot_remote::frame::frame_type::HANDSHAKE_ACK
    );
    stream
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unnegotiated_prefix_and_bad_last_batch_member_cannot_execute_business() {
    use parrot_remote::frame::{flags, frame_type, Frame};
    tokio::time::timeout(Duration::from_secs(20), async {
        for batch in [false, true] {
            let mut server = Server::start(
                if batch { "bad-batch" } else { "unnegotiated" },
                None,
                true,
                false,
            );
            let mut stream = raw_connection(&server, batch).await;
            let payload = parrot_api::message::serde_remote_serialize(&Work {
                value: 2,
                delay_ms: 0,
            })
            .unwrap();
            let root = TraceContext::root();
            let valid =
                Frame::tell_with_ack(1, "/counter", "bin:wire-trace.Work#v1", payload.into())
                    .with_trace_parent(root.as_parent())
                    .unwrap();
            let request = if batch {
                let mut second = valid.clone();
                second.header.correlation_id = 2;
                let mut frame = Frame::batch(&[valid.clone(), second]).unwrap();
                let mut bytes = frame.payload.to_vec();
                bytes[8 + 12 + valid.payload.len() + 12] = 2;
                frame.payload = bytes.into();
                assert_ne!(frame.header.flags & flags::BATCH, 0);
                frame
            } else {
                valid
            };
            write_frame(&mut stream, request).await;
            let failure = read_frame(&mut stream).await;
            if let Some(failure) = failure {
                assert_eq!(failure.header.frame_type, frame_type::ERROR);
                assert!(read_frame(&mut stream).await.is_none());
            }
            server.stop();
            assert_eq!(server.observations(), vec![serde_json::json!("disposed")]);
            assert!(server.spans().is_empty());
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_batch_assigns_each_real_handler_its_own_remote_parent() {
    use parrot_remote::frame::Frame;
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut server = Server::start("batch-parents", None, true, false);
        let mut stream = raw_connection(&server, true).await;
        let roots: Vec<_> = (0..3).map(|_| TraceContext::root()).collect();
        let frames: Vec<_> = roots
            .iter()
            .enumerate()
            .map(|(i, root)| {
                let payload = parrot_api::message::serde_remote_serialize(&Work {
                    value: 1,
                    delay_ms: 1,
                })
                .unwrap();
                Frame::tell_with_ack(
                    i as u64 + 1,
                    "/counter",
                    "bin:wire-trace.Work#v1",
                    payload.into(),
                )
                .with_trace_parent(root.as_parent())
                .unwrap()
            })
            .collect();
        write_frame(&mut stream, Frame::batch(&frames).unwrap()).await;
        let mut ids = std::collections::BTreeSet::new();
        for _ in 0..3 {
            let reply = read_frame(&mut stream).await.unwrap();
            assert_eq!(
                reply.header.frame_type,
                parrot_remote::frame::frame_type::REPLY
            );
            assert!(ids.insert(reply.header.correlation_id));
        }
        assert_eq!(ids, std::collections::BTreeSet::from([1, 2, 3]));
        drop(stream);
        server.stop();
        let observations = server.observations();
        assert_eq!(observations.len(), 4);
        let spans = server.spans();
        assert_eq!(spans.len(), 3);
        for root in roots {
            let observed: Vec<_> = observations
                .iter()
                .filter(|o| o["trace"] == serde_json::to_value(root.trace_id()).unwrap())
                .collect();
            assert_eq!(observed.len(), 1);
            assert_eq!(
                observed[0]["parent"],
                serde_json::to_value(root.span_id()).unwrap()
            );
            assert!(spans.iter().any(|s| s["traceId"] == hex(root.trace_id())
                && s["parentSpanId"] == hex(root.span_id())));
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reordered_tells_retain_the_buffered_messages_parent() {
    use parrot_remote::frame::Frame;
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut server = Server::start("reorder-parents", None, true, false);
        let mut stream = raw_connection(&server, true).await;
        let roots: Vec<_> = (0..3).map(|_| TraceContext::root()).collect();
        for i in [0, 2, 1] {
            let payload = parrot_api::message::serde_remote_serialize(&Work {
                value: i as i64 + 1,
                delay_ms: 0,
            })
            .unwrap();
            let frame = Frame::tell_with_seq(
                "/counter",
                "bin:wire-trace.Work#v1",
                payload.into(),
                i as u32 + 1,
            )
            .with_trace_parent(roots[i].as_parent())
            .unwrap();
            write_frame(&mut stream, frame).await;
            if i == 0 {
                server.wait_value(1).await;
            }
        }
        server.wait_value(6).await;
        drop(stream);
        server.stop();
        let observations = server.observations();
        assert_eq!(observations.len(), 4);
        for (i, value) in [1, 3, 6].iter().enumerate() {
            assert_eq!(observations[i]["value"], *value);
            assert_eq!(
                observations[i]["trace"],
                serde_json::to_value(roots[i].trace_id()).unwrap()
            );
            assert_eq!(
                observations[i]["parent"],
                serde_json::to_value(roots[i].span_id()).unwrap()
            );
        }
        assert_eq!(server.spans().len(), 3);
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn propagation_does_not_depend_on_receiver_recording() {
    let mut server = Server::start("no-recorder", None, false, false);
    let remote = client(&server).await;
    let target = remote.remote_ref("parrot://no-recorder/counter").unwrap();
    let root = TraceContext::root();
    let reply = root
        .in_scope(|| {
            target.send(Box::new(Work {
                value: 7,
                delay_ms: 1,
            }))
        })
        .await
        .unwrap()
        .downcast::<Reply>()
        .unwrap();
    assert_eq!(reply.0[0].trace, Some(root.trace_id()));
    assert_eq!(reply.0[0].parent, Some(root.span_id()));
    assert_eq!(reply.0[0].value, 7);
    remote.shutdown().await.unwrap();
    server.stop();
    assert!(server.spans().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_remote_ref_keeps_legacy_peers_business_payload_unchanged() {
    use parrot_remote::{
        frame::{flags, frame_type, Frame},
        handshake::{caps, HandshakeBody},
        transport::{run_connection, ConnParams, ConnSide, RuntimeKnobs},
    };
    tokio::time::timeout(Duration::from_secs(15), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let shutdown_rx = shutdown.subscribe();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::channel(4);
            let handle = run_connection(
                stream,
                ConnParams {
                    side: ConnSide::Accept,
                    local_addr: None,
                    peer_addr: None,
                    scheme: "tcp",
                    local_handshake: HandshakeBody {
                        node_id: "legacy".into(),
                        capabilities: caps::BIN | caps::TELL_COMPLETION,
                        ..Default::default()
                    },
                    inbound: tx,
                    on_disconnect: Arc::new(|_| {}),
                    shutdown: shutdown_rx,
                    knobs: Some(RuntimeKnobs {
                        heartbeat_enabled: false,
                        ..Default::default()
                    }),
                },
            )
            .await
            .unwrap();
            for (expected, value) in [
                (frame_type::ASK, 2),
                (frame_type::TELL, 3),
                (frame_type::TELL, 4),
            ] {
                let (frame, _, _) = rx.recv().await.unwrap();
                assert_eq!(frame.header.frame_type, expected);
                assert_eq!(frame.header.flags & flags::TRACE_CONTEXT, 0);
                let payload = if expected == frame_type::ASK {
                    frame.split_reply_to().unwrap().1
                } else {
                    frame.payload.clone()
                };
                let work: Work = parrot_api::message::serde_remote_deserialize(&payload).unwrap();
                assert_eq!(work.value, value);
                if expected == frame_type::ASK {
                    let payload =
                        parrot_api::message::serde_remote_serialize(&Reply(vec![])).unwrap();
                    handle
                        .sender
                        .send(Frame::reply(
                            frame.header.correlation_id,
                            &frame.path,
                            "bin:wire-trace.Reply#v1",
                            payload.into(),
                        ))
                        .await
                        .unwrap();
                } else if frame.header.flags & flags::TELL_ACK != 0 {
                    handle
                        .sender
                        .send(Frame::reply(
                            frame.header.correlation_id,
                            &frame.path,
                            parrot_remote::frame::TELL_COMPLETION_KEY,
                            bytes::Bytes::new(),
                        ))
                        .await
                        .unwrap();
                }
            }
            handle.closed.await.unwrap();
        });
        let remote =
            RemoteActorSystem::new(RemoteConfig::tcp("legacy-client", None), Arc::new(Empty))
                .unwrap();
        remote.start().await.unwrap();
        remote
            .connect(&NodeAddr::tcp("legacy", address))
            .await
            .unwrap();
        let target = remote.remote_ref("parrot://legacy/counter").unwrap();
        let root = TraceContext::root();
        root.in_scope(|| {
            target.send(Box::new(Work {
                value: 2,
                delay_ms: 0,
            }))
        })
        .await
        .unwrap();
        root.in_scope(|| {
            target.deliver(Box::new(Work {
                value: 3,
                delay_ms: 0,
            }))
        })
        .await
        .unwrap();
        root.in_scope(|| {
            target.deliver_with_ack(
                Box::new(Work {
                    value: 4,
                    delay_ms: 0,
                }),
                Duration::from_secs(5),
            )
        })
        .await
        .unwrap();
        remote.shutdown().await.unwrap();
        shutdown.send_replace(true);
        peer.await.unwrap();
        assert!(TraceContext::current().is_none());
    })
    .await
    .unwrap();
}

async fn transport_parent_contract(memory: bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        let directory = tempfile::tempdir().unwrap();
        let engine = ThreadActorSystem::shared(Default::default());
        let actor = engine
            .spawn_at(
                Counter {
                    value: 0,
                    next: Arc::new(Mutex::new(None)),
                    journal: directory.path().join("business.jsonl"),
                },
                "/counter",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        let config = |name| {
            if memory {
                RemoteConfig::mem(name)
            } else {
                RemoteConfig::quic(name, Some("127.0.0.1:0".parse().unwrap()))
                    .with_insecure_development()
            }
        };
        let server = RemoteActorSystem::new(
            config(if memory { "trace-mem" } else { "trace-quic" }),
            Arc::new(Lookup(Box::new(actor))),
        )
        .unwrap();
        let recorder = TraceRecorder::new(8).unwrap();
        server.set_trace_recorder(Some(recorder.clone()));
        server.start().await.unwrap();
        let remote = RemoteActorSystem::new(
            config(if memory {
                "trace-mem-client"
            } else {
                "trace-quic-client"
            }),
            Arc::new(Empty),
        )
        .unwrap();
        remote.start().await.unwrap();
        let endpoint = if memory {
            NodeAddr::mem("trace-mem")
        } else {
            NodeAddr::quic("trace-quic", server.local_addr().unwrap())
        };
        remote.connect(&endpoint).await.unwrap();
        let target = remote
            .remote_ref(if memory {
                "parrot://trace-mem/counter"
            } else {
                "parrot://trace-quic/counter"
            })
            .unwrap();
        let root = TraceContext::root();
        let reply = root
            .in_scope(|| {
                target.send(Box::new(Work {
                    value: 8,
                    delay_ms: 1,
                }))
            })
            .await
            .unwrap()
            .downcast::<Reply>()
            .unwrap();
        assert_eq!(reply.0[0].value, 8);
        assert_eq!(reply.0[0].trace, Some(root.trace_id()));
        assert_eq!(reply.0[0].parent, Some(root.span_id()));
        remote.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
        engine.shutdown_internal().await.unwrap();
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].span_id, reply.0[0].span.unwrap());
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quic_transports_causal_parent_to_actual_native_handler() {
    transport_parent_contract(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_transports_causal_parent_to_actual_native_handler() {
    transport_parent_contract(true).await;
}

async fn failing_handler_contract(value: i64) {
    tokio::time::timeout(Duration::from_secs(20), async {
        for mode in ["ask", "completion", "ordinary"] {
            let mut server = Server::start(&format!("failure-{mode}"), None, true, false);
            let remote = client(&server).await;
            let target = remote
                .remote_ref(&format!("parrot://failure-{mode}/counter"))
                .unwrap();
            let root = TraceContext::root();
            let work = Box::new(Work { value, delay_ms: 1 });
            match mode {
                "ask" => {
                    assert!(root.in_scope(|| target.send(work)).await.is_err());
                }
                "completion" => {
                    assert!(root
                        .in_scope(|| target.deliver_with_ack(work, Duration::from_secs(5)))
                        .await
                        .is_err());
                }
                _ => root.in_scope(|| target.deliver(work)).await.unwrap(),
            }
            server.wait_disposed().await;
            remote.shutdown().await.unwrap();
            server.stop();
            assert_eq!(server.observations(), vec![serde_json::json!("disposed")]);
            let invocations = server.invocations();
            assert_eq!(invocations.len(), 2);
            assert_eq!(invocations[0]["event"], "constructed");
            assert_eq!(invocations[1]["event"], "disposed");
            assert_eq!(invocations[0]["actual"], invocations[1]["actual"]);
            assert_eq!(invocations[1]["actual"], invocations[1]["expected"]);
            assert_eq!(
                invocations[1]["actual"]["trace"],
                serde_json::to_value(root.trace_id()).unwrap()
            );
            assert_eq!(
                invocations[1]["actual"]["parent"],
                serde_json::to_value(root.span_id()).unwrap()
            );
            let spans = server.spans();
            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0]["status"]["code"], 2);
            assert_eq!(spans[0]["traceId"], hex(root.trace_id()));
            assert_eq!(spans[0]["parentSpanId"], hex(root.span_id()));
            let expected = if value == i64::MAX {
                "panicked"
            } else {
                "error"
            };
            assert!(
                spans[0]["attributes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a["key"] == "parrot.span.status"
                        && a["value"]["stringValue"] == expected)
            );
            assert!(TraceContext::current().is_none());
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_handler_error_keeps_identity_through_disposal_and_records_failure() {
    failing_handler_contract(i64::MIN).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_handler_panic_keeps_identity_through_disposal_and_records_failure() {
    failing_handler_contract(i64::MAX).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_caller_does_not_change_identity_of_already_running_remote_business() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for disconnect in [false, true] {
            let mut server = Server::start("cancelled-caller", None, true, false);
            let remote = client(&server).await;
            let target = remote
                .remote_ref("parrot://cancelled-caller/counter")
                .unwrap();
            let root = TraceContext::root();
            let operation = root.scope(async move {
                target
                    .send(Box::new(Work {
                        value: 1,
                        delay_ms: 200,
                    }))
                    .await
            });
            let task = tokio::spawn(operation);
            server.wait_constructed().await;
            task.abort();
            match task.await {
                Err(error) => assert!(error.is_cancelled()),
                Ok(_) => panic!("caller completed before cancellation"),
            };
            if disconnect {
                remote.shutdown().await.unwrap();
            }
            // Dropping a caller future cannot undo a handler that has already started.
            server.wait_value(1).await;
            if !disconnect {
                let plain = remote
                    .remote_ref("parrot://cancelled-caller/counter")
                    .unwrap()
                    .send(Box::new(Work {
                        value: 0,
                        delay_ms: 0,
                    }))
                    .await
                    .unwrap()
                    .downcast::<Reply>()
                    .unwrap();
                assert!(plain.0[0].trace.is_none());
                remote.shutdown().await.unwrap();
            }
            server.stop();
            let invocations = server.invocations();
            assert_eq!(invocations.len(), if disconnect { 2 } else { 4 });
            assert_eq!(invocations[0]["actual"], invocations[1]["actual"]);
            assert_eq!(invocations[1]["expected"], invocations[1]["actual"]);
            assert_eq!(
                invocations[1]["actual"]["trace"],
                serde_json::to_value(root.trace_id()).unwrap()
            );
            assert_eq!(
                invocations[1]["actual"]["parent"],
                serde_json::to_value(root.span_id()).unwrap()
            );
            let spans = server.spans();
            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0]["status"]["code"], 1);
            assert_eq!(spans[0]["traceId"], hex(root.trace_id()));
            assert!(TraceContext::current().is_none());
        }
    })
    .await
    .unwrap();
}
