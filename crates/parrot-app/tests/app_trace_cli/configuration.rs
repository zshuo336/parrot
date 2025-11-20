use super::*;

fn overlay(case: &Case, text: &str) {
    let manifest = case.text("app.toml");
    std::fs::write(
        case.path("app.toml"),
        format!("{manifest}\n[config_overlay.tracing]\n{text}\n"),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_controls_real_export_and_query_without_trace_flags() {
    let collector = Collector::new(200).await;
    let case = Case::new("");
    let socket = case.path("trace.sock");
    overlay(
        &case,
        &format!(
            "otlp_endpoint = '{}'\nallow_insecure_http = true\ncapacity = 8\nsocket = '{}'\ninterval_ms = 10\nrequest_timeout_ms = 1000\nmax_attempts = 2",
            collector.url,
            socket.display()
        ),
    );
    let mut host = case.start(&[]);
    let endpoint = case.ready(&mut host).await;
    let client = client(&endpoint).await;
    let actor = client.remote_ref("parrot://app-host/user/counter").unwrap();
    let parent = TraceContext::root();
    let reply = parent
        .in_scope(|| actor.send(Box::new(counter::Count(17))))
        .await
        .unwrap()
        .downcast::<counter::Count>()
        .unwrap();
    assert_eq!(reply.0, 17);
    tokio::time::timeout(Duration::from_secs(5), async {
        while collector.spans(&case).len() < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut query = case.trace(
        &["trace-case", "--socket", socket.to_str().unwrap()],
        "configured-query.log",
    );
    assert!(
        case.wait(&mut query).await,
        "{}",
        case.text("configured-query.log.stderr")
    );
    let history = case.text("configured-query.log");
    assert!(history.contains(&hex(&parent.trace_id())), "{history}");
    assert!(history.contains("/application/startup"), "{history}");
    client.shutdown().await.unwrap();
    case.signal(&host);
    assert!(case.wait(&mut host).await, "{}", case.text("stderr.log"));
    assert_eq!(case.receipt()["complete"], true);
    assert_eq!(case.receipt()["accepted_spans"], 3);
    let spans = collector.spans(&case);
    assert_eq!(spans.len(), 3);
    startup_span(&spans);
    let received = spans
        .iter()
        .find(|span| span["traceId"] == hex(&parent.trace_id()))
        .unwrap();
    assert_eq!(received["parentSpanId"], hex(&parent.span_id()));
    assert!(!socket.exists());
    assert!(case.text("actor.log").ends_with("stopped\ndisposed\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_endpoint_accepts_partial_cli_configuration() {
    let collector = Collector::new(201).await;
    let case = Case::new("");
    std::fs::write(
        case.path("parrot.toml"),
        format!(
            "[tracing]\notlp_endpoint = '{}'\nallow_insecure_http = true\ncapacity = 1\n",
            collector.url
        ),
    )
    .unwrap();
    let socket = case.path("trace.sock");
    let mut host = case.start(&[
        "--trace-capacity",
        "8",
        "--trace-socket",
        socket.to_str().unwrap(),
    ]);
    let endpoint = case.ready(&mut host).await;
    tokio::time::timeout(Duration::from_secs(5), collector.received.notified())
        .await
        .unwrap();
    let client = client(&endpoint).await;
    let actor = client.remote_ref("parrot://app-host/user/counter").unwrap();
    for value in [1, 2, 3] {
        actor.send(Box::new(counter::Count(value))).await.unwrap();
    }
    let mut query = case.trace(
        &["trace-case", "--socket", socket.to_str().unwrap()],
        "capacity-query.log",
    );
    assert!(case.wait(&mut query).await);
    let history = case.text("capacity-query.log");
    assert!(history.contains("history_evicted=0"), "{history}");
    assert!(history.contains("completed=5"), "{history}");
    collector.release.notify_one();
    client.shutdown().await.unwrap();
    case.signal(&host);
    assert!(case.wait(&mut host).await, "{}", case.text("stderr.log"));
    assert_eq!(case.receipt()["complete"], true);
    assert_eq!(collector.spans(&case).len(), 5);
    assert!(!socket.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_endpoint_overrides_file_when_manifest_does_not_override_it() {
    let selected = Collector::new(200).await;
    let file = Collector::new(200).await;
    let case = Case::new("");
    std::fs::write(
        case.path("parrot.toml"),
        format!(
            "[tracing]\nenabled = false\notlp_endpoint = '{}'\nallow_insecure_http = true\n",
            file.url
        ),
    )
    .unwrap();
    let mut host = case.start(&["--trace-otlp", &selected.url]);
    case.ready(&mut host).await;
    case.signal(&host);
    assert!(case.wait(&mut host).await, "{}", case.text("stderr.log"));
    assert_eq!(case.receipt()["complete"], true);
    let spans = selected.spans(&case);
    assert_eq!(spans.len(), 2);
    startup_span(&spans);
    assert!(file.documents.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_endpoint_overrides_cli_and_file_collectors() {
    let selected = Collector::new(200).await;
    let cli = Collector::new(200).await;
    let file = Collector::new(200).await;
    let case = Case::new("");
    std::fs::write(
        case.path("parrot.toml"),
        format!(
            "[tracing]\notlp_endpoint = '{}'\nallow_insecure_http = true\n",
            file.url
        ),
    )
    .unwrap();
    overlay(&case, &format!("otlp_endpoint = '{}'", selected.url));
    let mut host = case.start(&["--trace-otlp", &cli.url]);
    case.ready(&mut host).await;
    case.signal(&host);
    assert!(case.wait(&mut host).await, "{}", case.text("stderr.log"));
    assert_eq!(case.receipt()["complete"], true);
    let spans = selected.spans(&case);
    assert_eq!(spans.len(), 2);
    startup_span(&spans);
    assert!(cli.documents.lock().unwrap().is_empty());
    assert!(file.documents.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_manifest_disable_prevents_inherited_export_and_socket() {
    let collector = Collector::new(200).await;
    let case = Case::new("");
    let socket = case.path("trace.sock");
    overlay(&case, "enabled = false");
    let mut host = case.start(&[
        "--trace-otlp",
        &collector.url,
        "--trace-allow-http",
        "--trace-socket",
        socket.to_str().unwrap(),
    ]);
    let endpoint = case.ready(&mut host).await;
    assert!(!socket.exists());
    let client = client(&endpoint).await;
    let actor = client.remote_ref("parrot://app-host/user/counter").unwrap();
    let reply = actor
        .send(Box::new(counter::Count(11)))
        .await
        .unwrap()
        .downcast::<counter::Count>()
        .unwrap();
    assert_eq!(reply.0, 11);
    client.shutdown().await.unwrap();
    case.signal(&host);
    assert!(case.wait(&mut host).await, "{}", case.text("stderr.log"));
    assert!(!case.text("stderr.log").contains("PARROT_APP_TRACE="));
    assert!(collector.documents.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_false_http_permission_overrides_cli_before_host_resources() {
    let case = Case::new("");
    overlay(&case, "allow_insecure_http = false");
    let capture = case.path("frames.jsonl");
    let mut host = case.start(&[
        "--trace-otlp",
        "http://127.0.0.1:1/v1/traces",
        "--trace-allow-http",
        "--record",
        capture.to_str().unwrap(),
    ]);
    assert!(!case.wait(&mut host).await);
    assert!(!capture.exists());
    assert!(!case.path("actor.log").exists());
    assert!(!case.path("ready").exists());
    assert!(case
        .text("stderr.log")
        .contains("invalid OTLP exporter configuration"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_retry_limit_controls_real_collector_failure() {
    let collector = Collector::new(503).await;
    let case = Case::new("");
    overlay(
        &case,
        &format!(
            "otlp_endpoint = '{}'\nallow_insecure_http = true\ninterval_ms = 1\nmax_attempts = 2",
            collector.url
        ),
    );
    let mut host = case.start(&[]);
    assert!(!case.wait(&mut host).await);
    let receipt = case.receipt();
    assert_eq!(receipt["complete"], false);
    unconfirmed_startup(&case, &collector, 2);
    assert!(receipt["end"]
        .as_str()
        .unwrap()
        .contains("AttemptsExhausted(Http(503))"));
    assert_eq!(collector.documents.lock().unwrap().len(), 2);
    assert!(case.text("actor.log").ends_with("stopped\ndisposed\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_request_timeout_bounds_an_unresponsive_collector() {
    let collector = Collector::new(0).await;
    let case = Case::new("");
    overlay(&case, &format!("otlp_endpoint = '{}'\nallow_insecure_http = true\ninterval_ms = 1\nrequest_timeout_ms = 50\nmax_attempts = 1", collector.url));
    let start = tokio::time::Instant::now();
    let mut host = case.start(&[]);
    assert!(!case.wait(&mut host).await);
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "configured request timeout was not applied"
    );
    let receipt = case.receipt();
    assert_eq!(receipt["complete"], false);
    unconfirmed_startup(&case, &collector, 1);
    assert!(receipt["end"]
        .as_str()
        .unwrap()
        .contains("AttemptsExhausted(Timeout)"));
    assert_eq!(collector.documents.lock().unwrap().len(), 1);
    assert!(case.text("actor.log").ends_with("stopped\ndisposed\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_effective_manifest_tracing_fails_before_actor_capture_or_readiness() {
    for settings in [
        "enabled = true",
        "capacity = 8",
        "otlp_endpoint = 'https://collector.example/v1/traces'\ncapacity = 0",
        "otlp_endpoint = 'https://collector.example/v1/traces'\ninterval_ms = 0",
        "otlp_endpoint = 'https://collector.example/v1/traces'\nrequest_timeout_ms = 3600001",
        "otlp_endpoint = 'https://collector.example/v1/traces'\nmax_attempts = 0",
        "otlp_endpoint = 'https://user:secret@localhost/v1/traces'",
        "otlp_endpoint = 'file:///tmp/secret'",
        "otlp_endpoint = 'http://127.0.0.1:1/v1/traces'",
        "otlp_endpoint = 'https://collector.example/v1/traces'\nsocket = ''",
    ] {
        let case = Case::new("");
        overlay(&case, settings);
        let capture = case.path("frames.jsonl");
        let mut host = case.start(&["--record", capture.to_str().unwrap()]);
        assert!(!case.wait(&mut host).await, "{settings}");
        assert!(!capture.exists(), "{settings}");
        assert!(!case.path("actor.log").exists(), "{settings}");
        assert!(!case.path("ready").exists(), "{settings}");
        let stderr = case.text("stderr.log");
        assert!(!stderr.contains("secret"), "{stderr}");
        assert!(stderr.contains("CLI_ERROR="), "{stderr}");
    }
}
