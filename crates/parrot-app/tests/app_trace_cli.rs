#![cfg(all(feature = "host", unix))]
#[allow(dead_code)]
#[path = "../examples/support/replay_counter.rs"]
mod counter;
use parrot_api::{address::ActorRef, trace::TraceContext, types::BoxedActorRef};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

#[test]
fn fixture_command() {
    let Some(args) = std::env::var_os("PARROT_TRACE_TEST_ARGS") else {
        return;
    };
    let args: Vec<String> = serde_json::from_str(args.to_str().unwrap()).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(parrot_app::cli::execute(args.into_iter().map(Into::into)));
    if let Err(error) = &result {
        eprintln!("CLI_ERROR={error}");
    }
    drop(runtime);
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}
struct Case {
    dir: tempfile::TempDir,
}
impl Case {
    fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.path().join("app.toml"), format!("name = 'trace-case'\nversion = '1.0.0'\n[[components]]\nname = 'counter'\nengine = 'parrot'\nartifact = {{ Props = {{ factory = 'example.replay-counter' }} }}\n[components.config]\njournal = '{}'\n{config}\n[config_overlay.remote.node]\nbind = '127.0.0.1:0'\n[config_overlay.thread]\nshared_pool_size = 1\nshared_burst_workers_max = 0\n", dir.path().join("actor.log").display())).unwrap();
        Self { dir }
    }
    fn hook(behavior: &str) -> Self {
        let case = Self::new(&format!(
            "startup_behavior = '{behavior}'\n[components.hooks]\non_start = '.'"
        ));
        let manifest = case
            .text("app.toml")
            .replace("example.replay-counter", "test.trace-startup-hook");
        std::fs::write(
            case.path("app.toml"),
            format!("{manifest}shutdown_timeout_ms = 500\n"),
        )
        .unwrap();
        case
    }
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    fn text(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap_or_default()
    }
    fn start(&self, flags: &[&str]) -> tokio::process::Child {
        let mut args = vec![
            "run".to_owned(),
            "--manifest".to_owned(),
            self.path("app.toml").to_str().unwrap().to_owned(),
        ];
        args.extend(flags.iter().map(|s| s.to_string()));
        std::fs::write(
            self.path("arguments.json"),
            serde_json::to_vec(&args).unwrap(),
        )
        .unwrap();
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_command", "--nocapture"])
            .env(
                "PARROT_TRACE_TEST_ARGS",
                serde_json::to_string(&args).unwrap(),
            )
            .env("PARROT_APP_READY_FILE", self.path("ready"))
            .env_remove("PARROT_CONFIG")
            .env_remove("PARROT_REPO_ROOT")
            .current_dir(self.dir.path())
            .stdout(std::fs::File::create(self.path("stdout.log")).unwrap())
            .stderr(std::fs::File::create(self.path("stderr.log")).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }
    async fn ready(&self, child: &mut tokio::process::Child) -> String {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let endpoint = self.text("ready");
                if !endpoint.is_empty() {
                    return endpoint;
                }
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "premature exit: {}",
                    self.text("stderr.log")
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("readiness deadline")
    }
    async fn wait(&self, child: &mut tokio::process::Child) -> bool {
        tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .expect("child exit deadline")
            .unwrap()
            .success()
    }
    fn receipt(&self) -> serde_json::Value {
        let stderr = self.text("stderr.log");
        let records: Vec<_> = stderr
            .lines()
            .filter_map(|l| l.strip_prefix("PARROT_APP_TRACE="))
            .collect();
        assert_eq!(records.len(), 1, "{stderr}");
        serde_json::from_str(records[0]).unwrap()
    }
    fn signal(&self, child: &tokio::process::Child) {
        // Child is still owned and has not been reaped, so its PID cannot be reused.
        assert_eq!(
            unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGTERM) },
            0
        );
    }

    fn trace(&self, arguments: &[&str], log: &str) -> tokio::process::Child {
        let mut args = vec!["trace".to_string()];
        args.extend(arguments.iter().map(|s| s.to_string()));
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fixture_command", "--nocapture"])
            .env(
                "PARROT_TRACE_TEST_ARGS",
                serde_json::to_string(&args).unwrap(),
            )
            .current_dir(self.dir.path())
            .stdout(std::fs::File::create(self.path(log)).unwrap())
            .stderr(std::fs::File::create(self.path(&format!("{log}.stderr"))).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        if let Some(root) = std::env::var_os("PARROT_APP_TRACE_EVIDENCE") {
            let destination = Path::new(&root).join(self.dir.path().file_name().unwrap());
            std::fs::create_dir_all(&destination).unwrap();
            for entry in std::fs::read_dir(self.dir.path()).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_file() {
                    std::fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
                }
            }
        }
    }
}
struct Collector {
    url: String,
    documents: Arc<Mutex<Vec<serde_json::Value>>>,
    task: tokio::task::JoinHandle<()>,
    release: Arc<tokio::sync::Notify>,
    received: Arc<tokio::sync::Notify>,
}
impl Collector {
    async fn new(status: u16) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let documents = Arc::new(Mutex::new(Vec::new()));
        let saved = documents.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let released = release.clone();
        let received = Arc::new(tokio::sync::Notify::new());
        let incoming = received.clone();
        let mut first = true;
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = tokio::io::BufReader::new(stream);
                let mut length = None;
                let mut headers = 0;
                loop {
                    let mut line = String::new();
                    assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
                    headers += line.len();
                    assert!(headers <= 32768);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = Some(value.trim().parse::<usize>().unwrap());
                        }
                    }
                }
                let length = length.unwrap();
                assert!(length <= 8 * 1024 * 1024);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                saved
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap());
                incoming.notify_one();
                if status == 201 && first {
                    released.notified().await;
                    first = false;
                }
                let status = if status == 201 { 200 } else { status };
                if status == 0 {
                    std::future::pending::<()>().await;
                }
                stream.get_mut().write_all(format!("HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").as_bytes()).await.unwrap();
            }
        });
        Self {
            url,
            documents,
            task,
            release,
            received,
        }
    }
    fn spans(&self, case: &Case) -> Vec<serde_json::Value> {
        let documents = self.documents.lock().unwrap();
        std::fs::write(
            case.path("documents.json"),
            serde_json::to_vec_pretty(&*documents).unwrap(),
        )
        .unwrap();
        documents
            .iter()
            .flat_map(|d| d["resourceSpans"].as_array().unwrap())
            .flat_map(|r| {
                assert!(r["resource"]["attributes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(
                        |a| a["key"] == "service.name" && a["value"]["stringValue"] == "trace-case"
                    ));
                r["scopeSpans"].as_array().unwrap()
            })
            .flat_map(|s| s["spans"].as_array().unwrap())
            .cloned()
            .collect()
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct Empty;
#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}
async fn client(endpoint: &str) -> Arc<RemoteActorSystem> {
    let client =
        RemoteActorSystem::new(RemoteConfig::tcp("trace-client", None), Arc::new(Empty)).unwrap();
    client.start().await.unwrap();
    client
        .connect(&NodeAddr::tcp("app-host", endpoint.parse().unwrap()))
        .await
        .unwrap();
    client
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn startup_span(spans: &[serde_json::Value]) -> &serde_json::Value {
    let roots: Vec<_> = spans
        .iter()
        .filter(|span| span["name"] == "/application/startup")
        .collect();
    assert_eq!(
        roots.len(),
        1,
        "one executed startup must produce exactly one startup span"
    );
    let root = roots[0];
    assert!(
        root.get("parentSpanId").is_none(),
        "startup must have an independent root identity"
    );
    assert_eq!(attribute(root, "parrot.span.kind"), "application");
    root
}
fn attribute<'a>(span: &'a serde_json::Value, key: &str) -> &'a str {
    span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|attribute| attribute["key"] == key)
        .unwrap()["value"]["stringValue"]
        .as_str()
        .unwrap()
}

struct StartupHook {
    journal: PathBuf,
    behavior: String,
}
impl StartupHook {
    fn note(&self, line: &str) -> parrot_api::types::ActorResult<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal)
            .map_err(|error| parrot_api::errors::ActorError::InternalError(error.to_string()))?;
        writeln!(file, "{line}")
            .map_err(|error| parrot_api::errors::ActorError::InternalError(error.to_string()))
    }
}
impl Drop for StartupHook {
    fn drop(&mut self) {
        let _ = self.note("disposed");
    }
}
impl parrot_api::actor::Actor for StartupHook {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = parrot::thread::context::ThreadContext<Self>;
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: parrot_api::types::BoxedMessage,
        _: &'a mut Self::Context,
    ) -> parrot_api::types::BoxedFuture<
        'a,
        parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
    > {
        Box::pin(async move {
            assert!(message.is::<parrot_app::assemble::ParrotAppHook>());
            self.note("hook-entered")?;
            match self.behavior.as_str() {
                "hold" => {
                    // The host registered SIGTERM before assembly. Signal this
                    // actual child from inside its active start hook, so the
                    // parent test need not race a polling delay against hooks.
                    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
                    std::future::pending::<()>().await;
                }
                "fail" => {
                    return Err(parrot_api::errors::ActorError::MessageHandlingError(
                        "injected startup hook error".into(),
                    ))
                }
                "ok" => {}
                _ => panic!("unknown startup fixture behavior"),
            }
            self.note("hook-completed")?;
            Ok(message)
        })
    }
    fn before_stop<'a>(
        &'a mut self,
        _: &'a mut Self::Context,
    ) -> parrot_api::types::BoxedFuture<'a, parrot_api::types::ActorResult<()>> {
        Box::pin(async move { self.note("stopped") })
    }
}
fn spawn_startup_hook(
    engine: Arc<parrot::thread::system::ThreadActorSystem>,
    path: String,
    config: Option<toml::Table>,
) -> parrot_api::types::BoxedFuture<'static, parrot_api::types::ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let config = config.unwrap();
        let hook = StartupHook {
            journal: PathBuf::from(config["journal"].as_str().unwrap()),
            behavior: config["startup_behavior"].as_str().unwrap().to_owned(),
        };
        hook.note("started")?;
        let actor = engine
            .spawn_at(hook, &path, None, Default::default())
            .await
            .map_err(|error| {
                parrot_api::errors::ActorError::InitializationError(error.to_string())
            })?;
        Ok(Box::new(actor) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! {
    parrot_app::host::ThreadPropsFactory { name: "test.trace-startup-hook", spawn: spawn_startup_hook }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_records_real_untraced_and_traced_work_and_flushes_after_signal() {
    let collector = Collector::new(200).await;
    let case = Case::new("");
    let capture = case.path("frames.jsonl");
    let mut child = case.start(&[
        "--trace-otlp",
        &collector.url,
        "--trace-allow-http",
        "--record",
        capture.to_str().unwrap(),
    ]);
    let endpoint = case.ready(&mut child).await;
    let client = client(&endpoint).await;
    let actor = client.remote_ref("parrot://app-host/user/counter").unwrap();
    let first = actor
        .send(Box::new(counter::Count(2)))
        .await
        .unwrap()
        .downcast::<counter::Count>()
        .unwrap();
    assert_eq!(first.0, 2);
    actor
        .deliver_with_ack(Box::new(counter::Count(3)), Duration::from_secs(3))
        .await
        .unwrap();
    let parent = TraceContext::root();
    let result = parent
        .in_scope(|| actor.send(Box::new(counter::Count(4))))
        .await
        .unwrap()
        .downcast::<counter::Count>()
        .unwrap();
    assert_eq!(result.0, 9);
    client.shutdown().await.unwrap();
    case.signal(&child);
    assert!(case.wait(&mut child).await, "{}", case.text("stderr.log"));
    assert!(case.text("actor.log").ends_with("stopped\ndisposed\n"));
    let receipt = case.receipt();
    assert_eq!(receipt["complete"], true);
    assert_eq!(receipt["accepted_spans"], 4);
    assert_eq!(receipt["in_flight"], 0);
    assert!(std::fs::metadata(capture).unwrap().len() > 0);
    let spans = collector.spans(&case);
    assert_eq!(spans.len(), 4);
    assert_eq!(attribute(startup_span(&spans), "parrot.span.status"), "ok");
    assert!(spans.iter().all(|s| s["status"]["code"] == 1));
    let known = spans
        .iter()
        .find(|s| s["traceId"] == hex(&parent.trace_id()))
        .unwrap();
    assert_eq!(known["parentSpanId"], hex(&parent.span_id()));
    let ids: std::collections::HashSet<_> = spans
        .iter()
        .map(|s| s["traceId"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_trace_query_keeps_real_spans_after_collector_acceptance() {
    let collector = Collector::new(200).await;
    let case = Case::new("");
    let socket = case.path("trace.sock");
    let socket_text = socket.to_str().unwrap();
    let mut host = case.start(&[
        "--trace-otlp",
        &collector.url,
        "--trace-allow-http",
        "--trace-socket",
        socket_text,
    ]);
    let endpoint = case.ready(&mut host).await;
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    tokio::time::timeout(Duration::from_secs(5), collector.received.notified())
        .await
        .unwrap();
    let mut first = case.trace(&["trace-case", "--socket", socket_text], "query.log");
    assert!(
        case.wait(&mut first).await,
        "{}",
        case.text("query.log.stderr")
    );
    let before = case.text("query.log");
    assert!(
        before.contains("target=\"/application/startup\""),
        "{before}"
    );
    assert!(before.contains("history_evicted=0"), "{before}");
    assert!(
        before.contains("drained=1"),
        "collector consumed the actual startup batch: {before}"
    );

    let client = client(&endpoint).await;
    let actor = client.remote_ref("parrot://app-host/user/counter").unwrap();
    let parent = TraceContext::root();
    let result = parent
        .in_scope(|| actor.send(Box::new(counter::Count(9))))
        .await
        .unwrap()
        .downcast::<counter::Count>()
        .unwrap();
    assert_eq!(result.0, 9);
    tokio::time::timeout(Duration::from_secs(5), collector.received.notified())
        .await
        .unwrap();
    let mut next = case.trace(&["trace-case", "--socket", socket_text], "query-next.log");
    assert!(case.wait(&mut next).await);
    let text = case.text("query-next.log");
    assert!(text.contains(&hex(&parent.trace_id())), "{text}");
    assert!(
        text.contains(&format!("parent={}", hex(&parent.span_id()))),
        "{text}"
    );
    assert!(text.contains("target=\"/application/startup\""), "{text}");
    assert!(text.contains("completed=2"), "{text}");
    client.shutdown().await.unwrap();
    case.signal(&host);
    assert!(case.wait(&mut host).await, "{}", case.text("stderr.log"));
    assert!(
        !socket.exists(),
        "query service must remove only its own socket at shutdown"
    );
    assert_eq!(case.receipt()["complete"], true);
    assert_eq!(
        collector.spans(&case).len(),
        2,
        "queries must not create, consume or acknowledge spans"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_trace_follow_observes_later_work_and_rejects_wrong_application() {
    let collector = Collector::new(200).await;
    let case = Case::new("");
    let socket = case.path("trace.sock");
    let socket_text = socket.to_str().unwrap();
    let mut host = case.start(&[
        "--trace-otlp",
        &collector.url,
        "--trace-allow-http",
        "--trace-socket",
        socket_text,
    ]);
    let endpoint = case.ready(&mut host).await;
    let mut wrong = case.trace(&["other-app", "--socket", socket_text], "wrong.log");
    assert!(!case.wait(&mut wrong).await);
    assert!(!case.text("wrong.log").contains("target="));
    let mut follower = case.trace(
        &[
            "trace-case",
            "--socket",
            socket_text,
            "--follow",
            "--interval-ms",
            "100",
        ],
        "follow.log",
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !case
            .text("follow.log")
            .contains("target=\"/application/startup\"")
        {
            assert!(follower.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let client = client(&endpoint).await;
    let actor = client.remote_ref("parrot://app-host/user/counter").unwrap();
    let parent = TraceContext::root();
    let result = parent
        .in_scope(|| actor.send(Box::new(counter::Count(3))))
        .await
        .unwrap()
        .downcast::<counter::Count>()
        .unwrap();
    assert_eq!(result.0, 3);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !case.text("follow.log").contains(&hex(&parent.trace_id())) {
            assert!(follower.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    case.signal(&follower);
    assert!(case.wait(&mut follower).await);
    assert!(
        host.try_wait().unwrap().is_none(),
        "stopping the reader must not stop the host"
    );
    client.shutdown().await.unwrap();
    case.signal(&host);
    assert!(case.wait(&mut host).await);
    assert_eq!(case.receipt()["complete"], true);
    assert!(!socket.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_socket_preflight_preserves_existing_files_and_rejects_public_directory() {
    let collector = Collector::new(200).await;
    let case = Case::new("");
    let socket = case.path("trace.sock");
    std::fs::write(&socket, b"owned existing file").unwrap();
    let mut existing = case.start(&[
        "--trace-otlp",
        &collector.url,
        "--trace-allow-http",
        "--trace-socket",
        socket.to_str().unwrap(),
    ]);
    assert!(!case.wait(&mut existing).await);
    assert_eq!(std::fs::read(&socket).unwrap(), b"owned existing file");
    assert!(case.text("actor.log").is_empty());
    assert!(!case.path("ready").exists());
    use std::os::unix::fs::PermissionsExt;
    let public = case.path("public");
    std::fs::create_dir(&public).unwrap();
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
    let public_socket = public.join("trace.sock");
    let mut public_host = case.start(&[
        "--trace-otlp",
        &collector.url,
        "--trace-allow-http",
        "--trace-socket",
        public_socket.to_str().unwrap(),
    ]);
    assert!(!case.wait(&mut public_host).await);
    assert!(!public_socket.exists());
    assert!(case.text("actor.log").is_empty());
    assert!(collector.documents.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_query_shutdown_retires_stalled_client_and_preserves_replaced_path() {
    let collector = Collector::new(200).await;
    let case = Case::new("");
    let socket = case.path("trace.sock");
    let mut host = case.start(&[
        "--trace-otlp",
        &collector.url,
        "--trace-allow-http",
        "--trace-socket",
        socket.to_str().unwrap(),
    ]);
    case.ready(&mut host).await;
    let mut stalled = tokio::net::UnixStream::connect(&socket).await.unwrap();
    // Declare a legal frame and send only one byte: shutdown must cancel I/O,
    // join the service and never interpret a partial request as a snapshot.
    stalled.write_u32(100).await.unwrap();
    stalled.write_all(b"{").await.unwrap();
    std::fs::rename(&socket, case.path("original.sock")).unwrap();
    std::fs::write(&socket, b"replacement owner").unwrap();
    case.signal(&host);
    assert!(case.wait(&mut host).await, "{}", case.text("stderr.log"));
    assert_eq!(std::fs::read(&socket).unwrap(), b"replacement owner");
    let mut bytes = Vec::new();
    match tokio::time::timeout(Duration::from_secs(2), stalled.read_to_end(&mut bytes))
        .await
        .unwrap()
    {
        Ok(_) => {}
        Err(error) => assert!(matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
        )),
    }
    assert!(bytes.is_empty());
    assert_eq!(case.receipt()["complete"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_query_frames_preserve_service_history_and_export_receipt() {
    let collector = Collector::new(200).await;
    let case = Case::new("");
    let socket = case.path("trace.sock");
    let mut host = case.start(&[
        "--trace-otlp",
        &collector.url,
        "--trace-allow-http",
        "--trace-socket",
        socket.to_str().unwrap(),
    ]);
    case.ready(&mut host).await;
    for (length, body) in [
        (0, &b""[..]),
        (u32::MAX, &b""[..]),
        (1, &b"{"[..]),
        (0, &b"{\"version\":2,\"app\":\"trace-case\"}"[..]),
        (0, &b"{\"version\":1,\"app\":\"other-app\"}"[..]),
        (
            0,
            &b"{\"version\":1,\"app\":\"trace-case\",\"extra\":true}"[..],
        ),
        (0, &b"[1,\"trace-case\"]"[..]),
        (0, &br#"{"version":1,"version":1,"app":"trace-case"}"#[..]),
        (
            0,
            &br#"{"version":1,"app":"other-app","app":"trace-case"}"#[..],
        ),
        (
            0,
            &br#"{"version":1,"app":"trace-case","a\u0070p":"trace-case"}"#[..],
        ),
        (0, &br#"{"version":1,"app":"trace-case"} {}"#[..]),
    ] {
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        stream
            .write_u32(if body.is_empty() {
                length
            } else {
                body.len() as u32
            })
            .await
            .unwrap();
        stream.write_all(body).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(
            response.is_empty(),
            "malformed query must not receive history"
        );
        assert!(host.try_wait().unwrap().is_none());
    }
    let mut valid = case.trace(
        &["trace-case", "--socket", socket.to_str().unwrap()],
        "after-malformed.log",
    );
    assert!(case.wait(&mut valid).await);
    assert!(case
        .text("after-malformed.log")
        .contains("target=\"/application/startup\""));
    case.signal(&host);
    assert!(case.wait(&mut host).await, "{}", case.text("stderr.log"));
    assert_eq!(case.receipt()["complete"], true);
    assert_eq!(collector.spans(&case).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_query_expires_then_next_client_observes_real_history() {
    let collector = Collector::new(200).await;
    let case = Case::new("");
    let socket = case.path("trace.sock");
    let mut host = case.start(&[
        "--trace-otlp",
        &collector.url,
        "--trace-allow-http",
        "--trace-socket",
        socket.to_str().unwrap(),
    ]);
    case.ready(&mut host).await;
    let mut stalled = tokio::net::UnixStream::connect(&socket).await.unwrap();
    stalled.write_u32(100).await.unwrap();
    stalled.write_all(b"{").await.unwrap();
    let mut response = Vec::new();
    // Wait on the actual server EOF, not an assumed scheduling delay.
    let result = tokio::time::timeout(Duration::from_secs(10), stalled.read_to_end(&mut response))
        .await
        .unwrap();
    if let Err(error) = result {
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
        ));
    }
    assert!(response.is_empty());
    let mut valid = case.trace(
        &["trace-case", "--socket", socket.to_str().unwrap()],
        "after-stall.log",
    );
    assert!(case.wait(&mut valid).await);
    assert!(case
        .text("after-stall.log")
        .contains("target=\"/application/startup\""));
    case.signal(&host);
    assert!(case.wait(&mut host).await);
    assert_eq!(case.receipt()["complete"], true);
    assert_eq!(collector.spans(&case).len(), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collector_rejection_stops_host_and_reports_unconfirmed_spans() {
    let collector = Collector::new(400).await;
    let case = Case::new("");
    let mut child = case.start(&["--trace-otlp", &collector.url, "--trace-allow-http"]);
    // Startup itself creates the first real record. Rejection may stop the
    // host before an external client has time to connect.
    assert!(!case.wait(&mut child).await);
    let receipt = case.receipt();
    assert_eq!(receipt["complete"], false);
    assert_eq!(receipt["pending_spans"], 1);
    assert_eq!(receipt["accepted_spans"], 0);
    assert!(case.text("actor.log").ends_with("stopped\ndisposed\n"));
    let spans = collector.spans(&case);
    assert_eq!(spans.len(), 1);
    assert_eq!(attribute(startup_span(&spans), "parrot.span.status"), "ok");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_without_requests_records_real_assembly_and_no_handler_or_transport_span() {
    let collector = Collector::new(200).await;
    let case = Case::new("");
    let mut child = case.start(&["--trace-otlp", &collector.url, "--trace-allow-http"]);
    case.ready(&mut child).await;
    case.signal(&child);
    assert!(case.wait(&mut child).await);
    assert_eq!(
        case.receipt()["accepted_spans"],
        1,
        "actual startup without requests must produce its application record"
    );
    assert_eq!(case.receipt()["complete"], true);
    let spans = collector.spans(&case);
    assert_eq!(spans.len(), 1);
    assert_eq!(attribute(startup_span(&spans), "parrot.span.status"), "ok");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_run_does_not_enable_recording() {
    let case = Case::new("");
    let mut child = case.start(&[]);
    let endpoint = case.ready(&mut child).await;
    let client = client(&endpoint).await;
    let actor = client.remote_ref("parrot://app-host/user/counter").unwrap();
    actor.send(Box::new(counter::Count(2))).await.unwrap();
    client.shutdown().await.unwrap();
    case.signal(&child);
    assert!(case.wait(&mut child).await);
    assert!(!case.text("stderr.log").contains("PARROT_APP_TRACE="));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_trace_flags_fail_before_any_actor_or_recording_file() {
    for flags in [
        vec!["--trace-socket"],
        vec!["--trace-socket", "query.sock"],
        vec!["--trace-capacity", "8"],
        vec!["--trace-capacity", "0"],
        vec!["--trace-capacity", "65537"],
        vec!["--trace-capacity", "-1"],
        vec!["--trace-allow-http"],
        vec!["--trace-otlp"],
        vec!["--trace-otlp", "http://127.0.0.1:1/v1/traces"],
        vec!["--trace-otlp", "file:///tmp/trace"],
        vec!["--trace-otlp", "https://user:secret@localhost/v1/traces"],
        vec![
            "--trace-otlp",
            "https://localhost/v1/traces",
            "--trace-otlp",
            "https://localhost/v1/traces",
        ],
        vec![
            "--trace-otlp",
            "http://localhost:1",
            "--trace-allow-http",
            "--trace-allow-http",
        ],
    ] {
        let case = Case::new("");
        let capture = case.path("frames.jsonl");
        let mut full = vec!["--record", capture.to_str().unwrap()];
        full.extend(flags);
        let mut child = case.start(&full);
        assert!(!case.wait(&mut child).await);
        assert!(!case.path("actor.log").exists());
        assert!(!case.path("frames.jsonl").exists());
        assert!(!case.path("ready").exists());
        assert!(!case.text("stderr.log").contains("secret"));
    }
}

#[tokio::test]
async fn invalid_query_arguments_fail_before_connecting() {
    for arguments in [
        vec![],
        vec!["orders"],
        vec![""],
        vec!["orders\n"],
        vec!["orders", "--socket"],
        vec!["orders", "--socket", "missing.sock", "--follow", "--follow"],
        vec!["orders", "--socket", "missing.sock", "--interval-ms", "250"],
        vec![
            "orders",
            "--socket",
            "missing.sock",
            "--follow",
            "--interval-ms",
            "99",
        ],
        vec![
            "orders",
            "--socket",
            "missing.sock",
            "--follow",
            "--interval-ms",
            "60001",
        ],
        vec![
            "orders",
            "--socket",
            "missing.sock",
            "--socket",
            "other.sock",
        ],
    ] {
        let case = Case::new("");
        let mut child = case.trace(&arguments, "invalid-query.log");
        assert!(!case.wait(&mut child).await);
        let stderr = case.text("invalid-query.log.stderr");
        assert!(stderr.contains("CLI_ERROR="), "{stderr}");
        assert!(
            !stderr.contains("No such file"),
            "invalid arguments reached socket I/O: {stderr}"
        );
        assert!(!case.path("actor.log").exists());
    }
}

#[tokio::test]
async fn interrupted_one_shot_query_cancels_pending_io_without_claiming_a_snapshot() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let case = Case::new("");
        let socket = case.path("unresponsive.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mut child = case.trace(
            &["trace-case", "--socket", socket.to_str().unwrap()],
            "interrupted.log",
        );
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let length = stream.read_u32().await.unwrap();
        assert!(length > 0 && length <= 4096);
        let mut body = vec![0; length as usize];
        stream.read_exact(&mut body).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["app"],
            "trace-case"
        );
        // A complete request proves the CLI installed signal handlers and entered
        // its pending response read. The owned, unreaped child still owns its PID.
        assert_eq!(unsafe { libc::kill(child.id().unwrap() as i32, signal) }, 0);
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("signal must cancel pending response I/O")
            .unwrap();
        assert!(
            !status.success(),
            "an interrupted one-shot has no snapshot receipt"
        );
        assert!(case
            .text("interrupted.log.stderr")
            .contains("CLI_ERROR=trace query interrupted"));
        assert!(!case.text("interrupted.log").contains("application="));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), stream.read_u8())
                .await
                .unwrap()
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_startup_joins_trace_worker_and_keeps_startup_error() {
    for (case, message, expected) in [
        (Case::new("delay_ms = -1"), "delay_ms must be", 1),
        (Case::hook("fail"), "injected startup hook error", 2),
    ] {
        let collector = Collector::new(200).await;
        let mut child = case.start(&["--trace-otlp", &collector.url, "--trace-allow-http"]);
        assert!(!case.wait(&mut child).await);
        assert!(!case.path("ready").exists());
        assert!(case.text("stderr.log").contains(message));
        assert_eq!(case.receipt()["complete"], true);
        assert_eq!(
            case.receipt()["accepted_spans"],
            expected,
            "failed startup must retain its application span and any invoked hook"
        );
        let spans = collector.spans(&case);
        assert_eq!(spans.len(), expected);
        let root = startup_span(&spans);
        assert_eq!(
            attribute(root, "parrot.span.status"),
            "error",
            "startup failure must not be reported as success"
        );
        assert!(spans.iter().all(|span| span["status"]["code"] == 2));
        if expected == 2 {
            let hook = spans
                .iter()
                .find(|span| span["name"] == "/user/counter")
                .unwrap();
            assert_eq!(hook["traceId"], root["traceId"]);
            assert_eq!(hook["parentSpanId"], root["spanId"]);
            assert!(case.text("actor.log").contains("disposed"));
        }
    }
    // Assembly succeeded, but publishing readiness is still part of startup.
    let collector = Collector::new(200).await;
    let case = Case::new("");
    std::fs::create_dir(case.path("ready")).unwrap();
    let mut child = case.start(&["--trace-otlp", &collector.url, "--trace-allow-http"]);
    assert!(!case.wait(&mut child).await);
    assert!(case.path("ready").is_dir());
    assert!(!case.text("stdout.log").contains("PARROT_APP_READY="));
    assert!(case.text("stderr.log").contains("CLI_ERROR="));
    assert!(case.text("actor.log").ends_with("stopped\ndisposed\n"));
    assert_eq!(case.receipt()["complete"], true);
    assert_eq!(case.receipt()["accepted_spans"], 1);
    let spans = collector.spans(&case);
    assert_eq!(spans.len(), 1);
    assert_eq!(
        attribute(startup_span(&spans), "parrot.span.status"),
        "error",
        "readiness publication failure must be part of startup failure"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collector_hang_obeys_shutdown_deadline_without_claiming_delivery() {
    let collector = Collector::new(0).await;
    let case = Case::new("");
    let mut child = case.start(&["--trace-otlp", &collector.url, "--trace-allow-http"]);
    case.ready(&mut child).await;
    case.signal(&child);
    assert!(!case.wait(&mut child).await);
    let receipt = case.receipt();
    assert_eq!(receipt["complete"], false);
    assert_eq!(receipt["end"], "ShutdownDeadline");
    assert_eq!(receipt["pending_spans"], 1);
    assert_eq!(receipt["accepted_spans"], 0);
    assert_eq!(collector.spans(&case).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_http_does_not_hide_overwritten_records() {
    let collector = Collector::new(201).await;
    let case = Case::new("");
    let mut child = case.start(&[
        "--trace-otlp",
        &collector.url,
        "--trace-allow-http",
        "--trace-capacity",
        "2",
    ]);
    let endpoint = case.ready(&mut child).await;
    let client = client(&endpoint).await;
    let actor = client.remote_ref("parrot://app-host/user/counter").unwrap();
    tokio::time::timeout(Duration::from_secs(3), collector.received.notified())
        .await
        .unwrap();
    let first = collector.spans(&case);
    assert_eq!(first.len(), 1);
    startup_span(&first);
    // The startup batch is owned by the blocked HTTP request. Four real
    // handlers must finish into a two-entry ring, deterministically losing two.
    for _ in 0..4 {
        actor.send(Box::new(counter::Count(1))).await.unwrap();
    }
    collector.release.notify_one();
    client.shutdown().await.unwrap();
    case.signal(&child);
    assert!(!case.wait(&mut child).await);
    let receipt = case.receipt();
    assert_eq!(receipt["end"], "Complete");
    assert_eq!(receipt["complete"], false);
    assert_eq!(receipt["completed"], 5);
    assert_eq!(receipt["overwritten"], 2);
    assert_eq!(receipt["accepted_spans"], 3);
    assert_eq!(receipt["pending_spans"], 0);
    assert_eq!(collector.spans(&case).len(), 3);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_handler_error_is_not_relabelled_as_successful_ingress() {
    let collector = Collector::new(200).await;
    let case = Case::new("");
    let mut child = case.start(&["--trace-otlp", &collector.url, "--trace-allow-http"]);
    let endpoint = case.ready(&mut child).await;
    let client = client(&endpoint).await;
    let actor = client.remote_ref("parrot://app-host/user/counter").unwrap();
    assert!(actor.send(Box::new(counter::Count(-1))).await.is_err());
    client.shutdown().await.unwrap();
    case.signal(&child);
    // Actor retirement errors may independently make host shutdown fail. The
    // delivery receipt reports recording/export completeness, not business success.
    case.wait(&mut child).await;
    let receipt = case.receipt();
    assert_eq!(receipt["complete"], true);
    assert_eq!(receipt["accepted_spans"], 2);
    let spans = collector.spans(&case);
    assert_eq!(spans.len(), 2);
    assert_eq!(startup_span(&spans)["status"]["code"], 1);
    let handler = spans
        .iter()
        .find(|span| span["name"] == "/user/counter")
        .unwrap();
    assert_eq!(handler["status"]["code"], 2);
    assert!(case.text("actor.log").contains("disposed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_root_reaches_real_start_hook_and_completes_before_serving() {
    let collector = Collector::new(200).await;
    let case = Case::hook("ok");
    let mut child = case.start(&["--trace-otlp", &collector.url, "--trace-allow-http"]);
    case.ready(&mut child).await;
    assert!(case.text("actor.log").contains("hook-completed"));
    case.signal(&child);
    assert!(case.wait(&mut child).await, "{}", case.text("stderr.log"));
    assert_eq!(case.receipt()["complete"], true);
    assert_eq!(
        case.receipt()["accepted_spans"],
        2,
        "startup hook and application root must both be recorded"
    );
    let spans = collector.spans(&case);
    assert_eq!(spans.len(), 2);
    let root = startup_span(&spans);
    let hook = spans
        .iter()
        .find(|span| span["name"] == "/user/counter")
        .unwrap();
    assert_eq!(attribute(root, "parrot.span.status"), "ok");
    assert_eq!(attribute(hook, "parrot.span.status"), "ok");
    assert_eq!(attribute(hook, "parrot.span.kind"), "dynamic_handler");
    assert_eq!(hook["traceId"], root["traceId"]);
    assert_eq!(hook["parentSpanId"], root["spanId"]);
    let timestamp =
        |span: &serde_json::Value, name: &str| span[name].as_str().unwrap().parse::<u64>().unwrap();
    assert!(timestamp(root, "startTimeUnixNano") <= timestamp(hook, "startTimeUnixNano"));
    assert!(
        timestamp(hook, "endTimeUnixNano") <= timestamp(root, "endTimeUnixNano"),
        "startup root must include the completed hook lifetime"
    );
    assert!(case.text("actor.log").ends_with("stopped\ndisposed\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signal_during_start_hook_records_cancelled_startup_and_joins_owners() {
    let collector = Collector::new(200).await;
    let case = Case::hook("hold");
    let mut child = case.start(&["--trace-otlp", &collector.url, "--trace-allow-http"]);
    // A stop deadline may independently fail the host result. Recording must
    // still finish only after the suspended handler and actor have retired.
    case.wait(&mut child).await;
    assert!(!case.path("ready").exists());
    assert!(case.text("actor.log").contains("hook-entered\n"));
    assert!(case.text("actor.log").contains("disposed"));
    assert_eq!(case.receipt()["complete"], true);
    assert_eq!(case.receipt()["in_flight"], 0);
    assert_eq!(
        case.receipt()["accepted_spans"],
        2,
        "cancelled startup must retain root and active hook"
    );
    let spans = collector.spans(&case);
    assert_eq!(spans.len(), 2);
    let root = startup_span(&spans);
    assert_eq!(
        attribute(root, "parrot.span.status"),
        "cancelled",
        "cancelled startup must not be reported as successful or failed completion"
    );
    let hook = spans
        .iter()
        .find(|span| span["name"] == "/user/counter")
        .unwrap();
    assert_eq!(attribute(hook, "parrot.span.status"), "cancelled");
    assert_eq!(hook["traceId"], root["traceId"]);
    assert_eq!(hook["parentSpanId"], root["spanId"]);
}
