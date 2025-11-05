use parrot_api::trace::{SpanGuard, SpanKind, SpanStatus};
use parrot_app::debug::{TraceExportConfig, TraceExportEnd, TraceExportFailure, TraceExporter};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

struct Reply {
    status: u16,
    body: Vec<u8>,
    extra: String,
    content_type: &'static str,
    chunked: bool,
}
impl Reply {
    fn ok() -> Self {
        Self::new(200, b"{}")
    }
    fn new(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            body: body.to_vec(),
            extra: String::new(),
            content_type: "application/json",
            chunked: false,
        }
    }
}
struct Request {
    head: String,
    body: Vec<u8>,
    reply: tokio::sync::oneshot::Sender<Reply>,
    closed: Arc<tokio::sync::Notify>,
}
struct Server {
    url: String,
    requests: tokio::sync::mpsc::Receiver<Request>,
    task: tokio::task::JoinHandle<()>,
}
trait TestIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> TestIo for T {}
impl Server {
    async fn new() -> Self {
        Self::bind(None).await
    }
    async fn tls(names: &[&str]) -> (Self, Vec<u8>) {
        use tokio_rustls::rustls::{self, pki_types::PrivatePkcs8KeyDer};
        let cert = rcgen::generate_simple_self_signed(
            names.iter().map(|n| (*n).to_string()).collect::<Vec<_>>(),
        )
        .unwrap();
        let der = cert.cert.der().to_vec();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
        (
            Self::bind(Some(tokio_rustls::TlsAcceptor::from(Arc::new(config)))).await,
            der,
        )
    }
    async fn bind(tls: Option<tokio_rustls::TlsAcceptor>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "{}://{}/v1/traces",
            if tls.is_some() { "https" } else { "http" },
            listener.local_addr().unwrap()
        );
        let (tx, requests) = tokio::sync::mpsc::channel(8);
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let stream: Box<dyn TestIo> = match &tls {
                    Some(acceptor) => {
                        match tokio::time::timeout(Duration::from_secs(5), acceptor.accept(stream))
                            .await
                        {
                            Ok(Ok(stream)) => Box::new(stream),
                            _ => continue,
                        }
                    }
                    None => Box::new(stream),
                };
                let mut stream = tokio::io::BufReader::new(stream);
                let mut head = String::new();
                let mut length = None;
                loop {
                    let mut line = String::new();
                    if stream.read_line(&mut line).await.unwrap() == 0 {
                        return;
                    }
                    head.push_str(&line);
                    assert!(head.len() <= 32768);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = Some(value.trim().parse::<usize>().unwrap());
                        }
                    }
                }
                let length = length.unwrap();
                assert!(length <= 1024 * 1024);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let (reply, response) = tokio::sync::oneshot::channel();
                let closed = Arc::new(tokio::sync::Notify::new());
                if tx
                    .send(Request {
                        head,
                        body,
                        reply,
                        closed: closed.clone(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                let mut byte = [0];
                let response = tokio::select! {
                    response = response => response.ok(),
                    _ = stream.read(&mut byte) => { closed.notify_one(); None },
                };
                let Some(response) = response else {
                    continue;
                };
                let framing = if response.chunked {
                    "Transfer-Encoding: chunked\r\n".into()
                } else {
                    format!("Content-Length: {}\r\n", response.body.len())
                };
                let header = format!(
                    "HTTP/1.1 {} Result\r\nContent-Type: {}\r\n{}{}Connection: close\r\n\r\n",
                    response.status, response.content_type, response.extra, framing
                );
                let wire = stream.get_mut();
                if wire.write_all(header.as_bytes()).await.is_err() {
                    continue;
                }
                if response.chunked {
                    for chunk in response.body.chunks(32) {
                        if wire
                            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if wire.write_all(chunk).await.is_err() {
                            break;
                        }
                        if wire.write_all(b"\r\n").await.is_err() {
                            break;
                        }
                    }
                    let _ = wire.write_all(b"0\r\n\r\n").await;
                } else {
                    let _ = wire.write_all(&response.body).await;
                }
                let _ = wire.shutdown().await;
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
    async fn next(&mut self) -> Request {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .expect("collector request")
    }
    fn config(&self) -> TraceExportConfig {
        let mut c = TraceExportConfig::new(&self.url, "worker-test");
        c.allow_insecure_http = true;
        c.interval = Duration::from_millis(5);
        c.request_timeout = Duration::from_secs(2);
        c.retry_min = Duration::from_millis(10);
        c.retry_max = Duration::from_millis(50);
        c.max_attempts = 3;
        c
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn record(exporter: &TraceExporter, target: &str) {
    exporter
        .root()
        .in_scope(|| SpanGuard::current(SpanKind::Application, target).finish(SpanStatus::Ok));
}
async fn ended(exporter: &TraceExporter) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while exporter.status().end.is_none() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
fn spans(body: &[u8]) -> Vec<serde_json::Value> {
    serde_json::from_slice::<serde_json::Value>(body).unwrap()["resourceSpans"][0]["scopeSpans"][0]
        ["spans"]
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn continuously_exports_distinct_operations_before_shutdown() {
    let mut server = Server::new().await;
    let mut exporter = TraceExporter::start(server.config()).unwrap();
    record(&exporter, "/first");
    let first = server.next().await;
    assert!(first.head.starts_with("POST /v1/traces HTTP/1.1\r\n"));
    assert!(first
        .head
        .to_lowercase()
        .contains("content-type: application/json\r\n"));
    let one = spans(&first.body);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0]["name"], "/first");
    first.reply.send(Reply::ok()).ok().unwrap();
    record(&exporter, "/second");
    let second = server.next().await;
    let two = spans(&second.body);
    assert_eq!(two.len(), 1);
    assert_eq!(two[0]["name"], "/second");
    assert_ne!(one[0]["traceId"], two[0]["traceId"]);
    second.reply.send(Reply::ok()).ok().unwrap();
    let report = exporter.shutdown(Duration::from_secs(2)).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::Complete);
    assert_eq!(report.status.accepted_spans, 2);
    assert_eq!(report.status.accepted_batches, 2);
    assert!(report.pending.is_none());
    assert!(report.remaining.terminal);
    assert!(report.remaining.records.is_empty());
}
#[tokio::test]
async fn retry_keeps_exact_batch_and_does_not_drain_newer_records() {
    let mut server = Server::new().await;
    let mut exporter = TraceExporter::start(server.config()).unwrap();
    record(&exporter, "/first");
    let first = server.next().await;
    let original = first.body.clone();
    record(&exporter, "/later");
    first
        .reply
        .send(Reply::new(503, b"unavailable"))
        .ok()
        .unwrap();
    let retry = server.next().await;
    assert_eq!(retry.body, original);
    assert_eq!(exporter.snapshot().records.len(), 1);
    retry.reply.send(Reply::ok()).ok().unwrap();
    let next = server.next().await;
    assert_eq!(spans(&next.body)[0]["name"], "/later");
    next.reply.send(Reply::ok()).ok().unwrap();
    let report = exporter.shutdown(Duration::from_secs(2)).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::Complete);
    assert_eq!(report.status.attempts, 3);
    assert_eq!(report.status.retries, 1);
    assert_eq!(report.status.accepted_spans, 2);
}
async fn rejected(reply: Reply, expected: TraceExportFailure, response_limit: usize) {
    let mut server = Server::new().await;
    let mut config = server.config();
    config.max_response_bytes = response_limit;
    let mut exporter = TraceExporter::start(config).unwrap();
    record(&exporter, "/retained");
    let request = server.next().await;
    let bytes = request.body.clone();
    request.reply.send(reply).ok().unwrap();
    ended(&exporter).await;
    let report = exporter.shutdown(Duration::from_secs(1)).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::Failed(expected));
    assert_eq!(report.status.accepted_spans, 0);
    assert_eq!(report.status.attempts, 1);
    let pending = report.pending.as_ref().unwrap();
    assert_eq!(pending.snapshot.records.len(), 1);
    assert_eq!(pending.json.as_ref().unwrap().as_ref(), bytes);
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn partial_rejection_is_not_retried_or_reported_as_success() {
    rejected(
        Reply::new(
            200,
            br#"{"partialSuccess":{"rejectedSpans":"1","errorMessage":"partial"}}"#,
        ),
        TraceExportFailure::PartialRejected { rejected_spans: 1 },
        65536,
    )
    .await;
}
#[tokio::test]
async fn malformed_success_body_is_not_an_ack() {
    rejected(
        Reply::new(200, b"not-json"),
        TraceExportFailure::InvalidResponse,
        65536,
    )
    .await;
}
#[tokio::test]
async fn impossible_rejection_count_is_not_an_ack() {
    rejected(
        Reply::new(200, br#"{"partialSuccess":{"rejectedSpans":"2"}}"#),
        TraceExportFailure::InvalidResponse,
        65536,
    )
    .await;
}
#[tokio::test]
async fn wrong_content_type_is_not_an_ack() {
    let mut reply = Reply::ok();
    reply.content_type = "text/plain";
    rejected(reply, TraceExportFailure::InvalidResponse, 65536).await;
}
#[tokio::test]
async fn declared_response_limit_preserves_pending_batch() {
    rejected(
        Reply::new(200, &[b' '; 256]),
        TraceExportFailure::ResponseTooLarge,
        64,
    )
    .await;
}
#[tokio::test]
async fn chunked_response_limit_preserves_pending_batch() {
    let mut reply = Reply::new(200, &[b' '; 256]);
    reply.chunked = true;
    rejected(reply, TraceExportFailure::ResponseTooLarge, 64).await;
}
#[tokio::test]
async fn permanent_http_error_preserves_pending_batch() {
    rejected(
        Reply::new(400, b"invalid"),
        TraceExportFailure::Http(400),
        65536,
    )
    .await;
}
#[tokio::test]
async fn redirect_does_not_forward_trace_or_authorization() {
    let mut redirected = Server::new().await;
    let mut server = Server::new().await;
    let mut config = server.config();
    config.headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_static("Bearer test-only-secret"),
    );
    let mut exporter = TraceExporter::start(config).unwrap();
    record(&exporter, "/redirect");
    let original = server.next().await;
    assert!(original
        .head
        .to_lowercase()
        .contains("authorization: bearer test-only-secret\r\n"));
    let mut reply = Reply::new(307, b"");
    reply.extra = format!("Location: {}\r\n", redirected.url);
    original.reply.send(reply).ok().unwrap();
    ended(&exporter).await;
    let report = exporter.shutdown(Duration::from_secs(1)).await.unwrap();
    assert_eq!(
        report.end,
        TraceExportEnd::Failed(TraceExportFailure::Http(307))
    );
    assert_eq!(report.status.accepted_spans, 0);
    assert_eq!(report.status.attempts, 1);
    assert_eq!(
        report
            .pending
            .as_ref()
            .unwrap()
            .json
            .as_ref()
            .unwrap()
            .as_ref(),
        original.body
    );
    assert!(redirected.requests.try_recv().is_err());
    assert!(!format!("{report:?}").contains("test-only-secret"));
}
#[tokio::test]
async fn retry_after_beyond_budget_does_not_retry_early() {
    let mut reply = Reply::new(429, b"");
    reply.extra = "Retry-After: 3600\r\n".into();
    rejected(reply, TraceExportFailure::RetryDelay, 65536).await;
}
#[tokio::test]
async fn retry_after_http_date_is_respected() {
    let mut reply = Reply::new(503, b"");
    reply.extra = format!(
        "Retry-After: {}\r\n",
        httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(3600))
    );
    rejected(reply, TraceExportFailure::RetryDelay, 65536).await;
}
#[tokio::test]
async fn retry_budget_exhaustion_retains_identical_body() {
    let mut server = Server::new().await;
    let mut exporter = TraceExporter::start(server.config()).unwrap();
    record(&exporter, "/retry");
    let mut original = None;
    for _ in 0..3 {
        let request = server.next().await;
        if let Some(body) = &original {
            assert_eq!(&request.body, body);
        } else {
            original = Some(request.body.clone());
        }
        request.reply.send(Reply::new(503, b"")).ok().unwrap();
    }
    ended(&exporter).await;
    let report = exporter.shutdown(Duration::from_secs(1)).await.unwrap();
    assert_eq!(
        report.end,
        TraceExportEnd::AttemptsExhausted(TraceExportFailure::Http(503))
    );
    assert_eq!(report.status.attempts, 3);
    assert_eq!(report.status.retries, 2);
    assert!(report.pending.is_some());
}
#[tokio::test]
async fn cancelled_shutdown_waiter_keeps_join_and_repeatable_receipt() {
    let mut server = Server::new().await;
    let mut exporter = TraceExporter::start(server.config()).unwrap();
    record(&exporter, "/join");
    let request = server.next().await;
    {
        let mut wait = Box::pin(exporter.shutdown(Duration::from_secs(3)));
        assert!(futures::poll!(wait.as_mut()).is_pending());
    }
    request.reply.send(Reply::ok()).ok().unwrap();
    let first = exporter.shutdown(Duration::from_secs(2)).await.unwrap();
    let second = exporter.shutdown(Duration::ZERO).await.unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.end, TraceExportEnd::Complete);
}
#[tokio::test]
async fn shutdown_deadline_retains_unconfirmed_batch_and_late_completion() {
    let mut server = Server::new().await;
    let mut exporter = TraceExporter::start(server.config()).unwrap();
    record(&exporter, "/pending");
    let request = server.next().await;
    let bytes = request.body.clone();
    let guard = exporter
        .root()
        .in_scope(|| SpanGuard::current(SpanKind::Application, "/late"));
    let report = tokio::time::timeout(
        Duration::from_secs(1),
        exporter.shutdown(Duration::from_millis(20)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(report.end, TraceExportEnd::ShutdownDeadline);
    assert_eq!(
        report
            .pending
            .as_ref()
            .unwrap()
            .json
            .as_ref()
            .unwrap()
            .as_ref(),
        bytes
    );
    assert!(!report.remaining.terminal);
    assert_eq!(report.remaining.counts.in_flight, 1);
    guard.finish(SpanStatus::Ok);
    let late = exporter.snapshot();
    assert!(late.terminal);
    assert_eq!(late.records.len(), 1);
    assert_eq!(late.records[0].target, "/late");
}
#[tokio::test]
async fn pending_request_keeps_recording_capacity_bounded_and_loss_explicit() {
    let mut server = Server::new().await;
    let mut config = server.config();
    config.capacity = 2;
    let mut exporter = TraceExporter::start(config).unwrap();
    record(&exporter, "/pending");
    let _request = server.next().await;
    for _ in 0..10 {
        record(&exporter, "/bounded");
    }
    let report = exporter.shutdown(Duration::ZERO).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::ShutdownDeadline);
    assert_eq!(report.pending.as_ref().unwrap().snapshot.records.len(), 1);
    assert_eq!(report.remaining.records.len(), 2);
    assert_eq!(report.remaining.counts.overwritten, 8);
}
#[tokio::test]
async fn serialization_failure_preserves_snapshot_without_network_send() {
    let mut server = Server::new().await;
    let mut config = server.config();
    config.max_batch_bytes = 1;
    let mut exporter = TraceExporter::start(config).unwrap();
    record(&exporter, "/too-large");
    ended(&exporter).await;
    let report = exporter.shutdown(Duration::from_secs(1)).await.unwrap();
    assert_eq!(
        report.end,
        TraceExportEnd::Failed(TraceExportFailure::Serialization)
    );
    assert_eq!(report.status.attempts, 0);
    let pending = report.pending.as_ref().unwrap();
    assert!(pending.json.is_none());
    assert_eq!(pending.snapshot.records.len(), 1);
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn owner_drop_cancels_an_outstanding_http_response_wait() {
    let mut server = Server::new().await;
    let exporter = TraceExporter::start(server.config()).unwrap();
    record(&exporter, "/drop");
    let request = server.next().await;
    drop(exporter);
    tokio::time::timeout(Duration::from_secs(1), request.closed.notified())
        .await
        .unwrap();
}
#[tokio::test]
async fn shutdown_wakes_idle_collector_to_flush_before_short_grace() {
    let mut server = Server::new().await;
    let mut config = server.config();
    config.interval = Duration::from_secs(60);
    let mut exporter = TraceExporter::start(config).unwrap();
    tokio::task::yield_now().await;
    record(&exporter, "/flush");
    let wait = exporter.shutdown(Duration::from_secs(2));
    tokio::pin!(wait);
    assert!(futures::poll!(wait.as_mut()).is_pending());
    let request = server.next().await;
    request.reply.send(Reply::ok()).ok().unwrap();
    assert_eq!(wait.await.unwrap().end, TraceExportEnd::Complete);
}
#[tokio::test]
async fn zero_rejection_warning_is_counted_without_retry() {
    let mut server = Server::new().await;
    let mut exporter = TraceExporter::start(server.config()).unwrap();
    record(&exporter, "/warning");
    let request = server.next().await;
    request
        .reply
        .send(Reply::new(
            200,
            br#"{"partialSuccess":{"rejectedSpans":0,"errorMessage":"warning"}}"#,
        ))
        .ok()
        .unwrap();
    let report = exporter.shutdown(Duration::from_secs(2)).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::Complete);
    assert_eq!(report.status.accepted_with_warning, 1);
    assert_eq!(report.status.retries, 0);
}
#[test]
fn starting_without_runtime_returns_error() {
    assert!(matches!(
        TraceExporter::start(TraceExportConfig::new(
            "https://localhost/v1/traces",
            "test"
        )),
        Err(parrot_app::debug::TraceExportStartError::NoRuntime)
    ));
}
#[tokio::test]
async fn invalid_configuration_is_rejected_before_background_work() {
    let server = Server::new().await;
    let mut values = vec![
        TraceExportConfig::new(&server.url, "test"),
        TraceExportConfig::new("https://user:secret@localhost/v1/traces", "test"),
        TraceExportConfig::new("https://localhost/v1/traces#fragment", "test"),
    ];
    let mut c = server.config();
    c.capacity = 0;
    values.push(c);
    let mut c = server.config();
    c.max_attempts = 0;
    values.push(c);
    let mut c = server.config();
    c.interval = Duration::ZERO;
    values.push(c);
    let mut c = server.config();
    c.headers.insert(
        reqwest::header::HOST,
        reqwest::header::HeaderValue::from_static("other-host"),
    );
    values.push(c);
    for c in values {
        assert!(matches!(
            TraceExporter::start(c),
            Err(parrot_app::debug::TraceExportStartError::Configuration)
        ));
    }
}

#[tokio::test]
async fn trusted_https_exports_without_disabling_certificate_verification() {
    let (mut server, der) = Server::tls(&["127.0.0.1"]).await;
    let mut config = server.config();
    config.allow_insecure_http = false;
    config.additional_root_certificates.push(der);
    let mut exporter = TraceExporter::start(config).unwrap();
    record(&exporter, "/tls");
    let request = server.next().await;
    assert_eq!(spans(&request.body)[0]["name"], "/tls");
    request.reply.send(Reply::ok()).ok().unwrap();
    let report = exporter.shutdown(Duration::from_secs(2)).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::Complete);
    assert_eq!(report.status.accepted_spans, 1);
}
async fn tls_rejected(trust: bool, names: &[&str]) {
    let (mut server, der) = Server::tls(names).await;
    let mut config = server.config();
    config.allow_insecure_http = false;
    config.max_attempts = 1;
    if trust {
        config.additional_root_certificates.push(der);
    }
    let mut exporter = TraceExporter::start(config).unwrap();
    record(&exporter, "/tls-rejected");
    ended(&exporter).await;
    let report = exporter.shutdown(Duration::from_secs(1)).await.unwrap();
    assert_eq!(
        report.end,
        TraceExportEnd::AttemptsExhausted(TraceExportFailure::Transport)
    );
    assert_eq!(report.status.accepted_spans, 0);
    assert_eq!(report.pending.as_ref().unwrap().snapshot.records.len(), 1);
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn untrusted_https_does_not_transmit_trace() {
    tls_rejected(false, &["127.0.0.1"]).await;
}
#[tokio::test]
async fn trusted_certificate_with_wrong_hostname_does_not_transmit_trace() {
    tls_rejected(true, &["wrong.invalid"]).await;
}

#[tokio::test]
async fn request_timeout_retries_exact_bytes_and_can_recover() {
    let mut server = Server::new().await;
    let mut config = server.config();
    config.request_timeout = Duration::from_millis(100);
    let mut exporter = TraceExporter::start(config).unwrap();
    record(&exporter, "/timeout");
    let first = server.next().await;
    // Do not answer: the server observes the client cancelling the response wait.
    let retry = server.next().await;
    assert_eq!(retry.body, first.body);
    assert_eq!(
        exporter.status().last_failure,
        Some(TraceExportFailure::Timeout)
    );
    retry.reply.send(Reply::ok()).ok().unwrap();
    let report = exporter.shutdown(Duration::from_secs(2)).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::Complete);
    assert_eq!(report.status.attempts, 2);
    assert_eq!(report.status.accepted_spans, 1);
}
#[tokio::test]
async fn transport_disconnect_retries_exact_bytes_and_can_recover() {
    let mut server = Server::new().await;
    let mut exporter = TraceExporter::start(server.config()).unwrap();
    record(&exporter, "/disconnect");
    let first = server.next().await;
    let original = first.body.clone();
    drop(first); // Collector closes before sending any response.
    let retry = server.next().await;
    assert_eq!(retry.body, original);
    assert_eq!(
        exporter.status().last_failure,
        Some(TraceExportFailure::Transport)
    );
    retry.reply.send(Reply::ok()).ok().unwrap();
    let report = exporter.shutdown(Duration::from_secs(2)).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::Complete);
    assert_eq!(report.status.attempts, 2);
    assert_eq!(report.status.accepted_spans, 1);
}
#[tokio::test]
async fn empty_zero_grace_shutdown_completes_without_network_work() {
    let mut server = Server::new().await;
    let mut exporter = TraceExporter::start(server.config()).unwrap();
    let report = exporter.shutdown(Duration::ZERO).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::Complete);
    assert_eq!(report.status.attempts, 0);
    assert!(report.remaining.terminal);
    assert!(report.pending.is_none());
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn zero_grace_shutdown_retains_undrained_records_without_http_attempt() {
    let mut server = Server::new().await;
    let mut exporter = TraceExporter::start(server.config()).unwrap();
    record(&exporter, "/unsent");
    let report = exporter.shutdown(Duration::ZERO).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::ShutdownDeadline);
    assert_eq!(report.status.attempts, 0);
    assert_eq!(report.remaining.records.len(), 1);
    assert!(report.pending.is_none());
    assert!(server.requests.try_recv().is_err());
}
#[tokio::test]
async fn negative_and_fractional_partial_counts_are_invalid() {
    for body in [
        br#"{"partialSuccess":{"rejectedSpans":-1}}"#.as_slice(),
        br#"{"partialSuccess":{"rejectedSpans":0.5}}"#,
    ] {
        rejected(
            Reply::new(200, body),
            TraceExportFailure::InvalidResponse,
            65536,
        )
        .await;
    }
}
#[tokio::test]
async fn nonobject_responses_are_not_acks() {
    for body in [b"[]".as_slice(), b"null", br#"{"partialSuccess":[]}"#] {
        rejected(
            Reply::new(200, body),
            TraceExportFailure::InvalidResponse,
            65536,
        )
        .await;
    }
}
#[tokio::test]
async fn tls_material_and_protocol_override_budgets_are_enforced() {
    let server = Server::new().await;
    let mut values = Vec::new();
    let mut c = server.config();
    c.additional_root_certificates = vec![vec![0]; 33];
    values.push(c);
    let mut c = server.config();
    c.additional_root_certificates = vec![vec![0; 128 * 1024 + 1]];
    values.push(c);
    let mut c = server.config();
    c.additional_root_certificates = vec![vec![0; 128 * 1024]; 9];
    values.push(c);
    let mut c = server.config();
    c.additional_root_certificates = vec![vec![]];
    values.push(c);
    let mut c = server.config();
    c.endpoint = "http://127.0.0.1:0/v1/traces".into();
    values.push(c);
    for name in [
        reqwest::header::CONTENT_ENCODING,
        reqwest::header::ACCEPT,
        reqwest::header::TE,
        reqwest::header::TRAILER,
        reqwest::header::UPGRADE,
        reqwest::header::EXPECT,
    ] {
        let mut c = server.config();
        c.headers.insert(
            name,
            reqwest::header::HeaderValue::from_static("unsupported"),
        );
        values.push(c);
    }
    for c in values {
        assert!(matches!(
            TraceExporter::start(c),
            Err(parrot_app::debug::TraceExportStartError::Configuration)
        ));
    }
}

#[tokio::test]
async fn real_thread_actor_operations_reach_http_collector() {
    use parrot_api::{
        actor::{Actor, ActorState, EmptyConfig},
        address::ActorRef,
        errors::ActorError,
        types::{ActorResult, BoxedFuture, BoxedMessage},
    };
    struct Sum(u64);
    impl Actor for Sum {
        type Config = EmptyConfig;
        type Context = parrot::thread::ThreadContext<Self>;
        fn state(&self) -> ActorState {
            ActorState::Running
        }
        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            _: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                let value = message
                    .downcast::<u64>()
                    .map_err(|_| ActorError::MessageHandlingError("expected increment".into()))?;
                self.0 += *value;
                Ok(Box::new(self.0) as BoxedMessage)
            })
        }
    }
    let mut server = Server::new().await;
    let mut exporter = TraceExporter::start(server.config()).unwrap();
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_root_typed_thread(Sum(0), EmptyConfig)
        .await
        .unwrap();
    let root = exporter.root();
    let operation = root.in_scope(|| SpanGuard::current(SpanKind::Application, "/sum-operation"));
    let mut value = 0;
    for increment in [1_u64, 2, 3] {
        value = *root
            .scope(async { actor.send(Box::new(increment)).await })
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap();
    }
    assert_eq!(value, 6);
    operation.finish(SpanStatus::Ok);
    actor.stop().await.unwrap();
    system.shutdown_internal().await.unwrap();
    let mut all = Vec::new();
    let mut batch = 0;
    while all.len() < 4 {
        let request = server.next().await;
        all.extend(spans(&request.body));
        if let Some(path) = std::env::var_os("PARROT_EXPORT_WORKER_FIXTURES") {
            let path = std::path::PathBuf::from(path);
            std::fs::create_dir_all(&path).unwrap();
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path.join(format!("actor-{batch}.json")))
                .unwrap();
            std::io::Write::write_all(&mut file, &request.body).unwrap();
        }
        batch += 1;
        request.reply.send(Reply::ok()).ok().unwrap();
    }
    let report = exporter.shutdown(Duration::from_secs(2)).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::Complete);
    assert_eq!(report.status.accepted_spans, 4);
    assert_eq!(report.remaining.counts.overwritten, 0);
    assert_eq!(all.len(), 4);
    let operation = all.iter().find(|s| s["name"] == "/sum-operation").unwrap();
    for span in &all {
        assert_eq!(span["traceId"], operation["traceId"]);
        assert_eq!(span["status"]["code"], 1);
        if span["name"] != "/sum-operation" {
            assert_eq!(span["parentSpanId"], operation["spanId"]);
            assert!(span["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["key"] == "parrot.span.kind"
                    && a["value"]["stringValue"] == "dynamic_handler"));
        }
    }
}

#[tokio::test]
async fn shutdown_collects_inflight_completion_without_waiting_normal_interval() {
    let mut server = Server::new().await;
    let mut config = server.config();
    config.interval = Duration::from_secs(60);
    let mut exporter = TraceExporter::start(config).unwrap();
    let guard = exporter
        .root()
        .in_scope(|| SpanGuard::current(SpanKind::Application, "/finishes-during-close"));
    let wait = exporter.shutdown(Duration::from_secs(2));
    tokio::pin!(wait);
    assert!(futures::poll!(wait.as_mut()).is_pending());
    tokio::time::sleep(Duration::from_millis(20)).await;
    guard.finish(SpanStatus::Ok);
    let request = tokio::time::timeout(Duration::from_secs(1), server.next())
        .await
        .expect("shutdown must collect a completed admitted span promptly");
    assert_eq!(spans(&request.body)[0]["name"], "/finishes-during-close");
    request.reply.send(Reply::ok()).ok().unwrap();
    let report = wait.await.unwrap();
    assert_eq!(report.end, TraceExportEnd::Complete);
    assert_eq!(report.status.accepted_spans, 1);
}

#[tokio::test]
async fn protobuf_null_fields_mean_default_response_values() {
    for body in [
        br#"{"partialSuccess":null}"#.as_slice(),
        br#"{"partialSuccess":{"rejectedSpans":null,"errorMessage":null}}"#,
    ] {
        let mut server = Server::new().await;
        let mut exporter = TraceExporter::start(server.config()).unwrap();
        record(&exporter, "/null-default");
        server
            .next()
            .await
            .reply
            .send(Reply::new(200, body))
            .ok()
            .unwrap();
        let report = exporter.shutdown(Duration::from_secs(2)).await.unwrap();
        assert_eq!(report.end, TraceExportEnd::Complete);
        assert_eq!(report.status.accepted_spans, 1);
    }
}
