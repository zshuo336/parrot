//! A bounded, loopback-only HTTP view of the configured gateway snapshots.
use super::{sampling, MeasuredMetrics, NodeSpec, RemoteActorSystem};
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
const HEADER_LIMIT: usize = 8192;
const MAX_CONNECTIONS: usize = 64;
const REQUEST_DEADLINE: Duration = Duration::from_secs(15);
const HTML: &str = include_str!("dashboard.html");
const SCRIPT: &str = include_str!("dashboard.js");
const SECURITY_HEADERS:&str="Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'\r\n";

pub(crate) async fn serve(
    client: Arc<RemoteActorSystem>,
    nodes: Vec<NodeSpec>,
    port: u16,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    println!(
        "Web dashboard: http://127.0.0.1:{} (loopback only)",
        listener.local_addr()?.port()
    );
    serve_on(listener, client, nodes, MAX_CONNECTIONS, REQUEST_DEADLINE).await
}
async fn serve_on(
    listener: TcpListener,
    client: Arc<RemoteActorSystem>,
    nodes: Vec<NodeSpec>,
    capacity: usize,
    deadline: Duration,
) -> anyhow::Result<()> {
    let port = listener.local_addr()?.port();
    let nodes = Arc::new(nodes);
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            joined=tasks.join_next(),if !tasks.is_empty()=>{
                if let Some(Err(error))=joined { eprintln!("dashboard connection task failed: {error}"); }
            }
            incoming=listener.accept(),if tasks.len()<capacity=>{
                let (socket,_)=incoming?;let client=client.clone();let nodes=nodes.clone();
                tasks.spawn(async move {
                    // One deadline bounds header reading, remote collection and writes.
                    let _=tokio::time::timeout(deadline,handle(socket,port,&client,&nodes)).await;
                });
            }
        }
    }
}
fn request_path(header: &str, port: u16) -> Result<&str, &'static str> {
    let mut lines = header.split("\r\n");
    let words: Vec<_> = lines.next().ok_or("400 Bad Request")?.split(' ').collect();
    if words.len() != 3 || words[2] != "HTTP/1.1" {
        return Err("400 Bad Request");
    }
    if words[0] != "GET" {
        return Err("405 Method Not Allowed");
    }
    let mut host = None;
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or("400 Bad Request")?;
        if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err("400 Bad Request");
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") && host.replace(value).is_some() {
            return Err("400 Bad Request");
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value != "0")
        {
            return Err("400 Bad Request");
        }
    }
    let host = host.ok_or("400 Bad Request")?;
    if ![format!("127.0.0.1:{port}"), format!("localhost:{port}")]
        .iter()
        .any(|expected| host.eq_ignore_ascii_case(expected))
    {
        return Err("403 Forbidden");
    }
    Ok(words[1])
}
fn snapshot_json(metrics: &MeasuredMetrics) -> serde_json::Value {
    let mut value =
        serde_json::to_value(&metrics.snapshot).expect("metrics snapshot is JSON serializable");
    // Preserve u64 exactly in JavaScript, including counters above 2^53.
    for field in value.as_object_mut().expect("snapshot object").values_mut() {
        if let Some(n) = field.as_u64() {
            *field = serde_json::Value::String(n.to_string());
        }
    }
    let object = value.as_object_mut().unwrap();
    for (bit, name) in [
        "ts_ms",
        "runtime",
        "connections",
        "handshakes_ok",
        "handshakes_failed",
        "asks_rx",
        "tells_rx",
        "replies_tx",
        "reply_errs",
        "bytes_rx",
        "bytes_tx",
        "heartbeats_rx",
        "components",
        "component_states",
        "processes",
        "memory_rss",
        "uptime_start_ms",
    ]
    .iter()
    .enumerate()
    {
        if !metrics.has(1 << bit) {
            object.insert((*name).into(), serde_json::Value::Null);
        }
    }
    object.insert("available".into(), metrics.available.to_string().into());
    object.insert("epoch".into(), metrics.epoch.clone().into());
    object.insert("process_kind".into(), metrics.process_kind.clone().into());
    value
}

fn prometheus_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

fn prometheus_labels(node: &str, metrics: &MeasuredMetrics) -> String {
    format!(
        "node=\"{}\",runtime=\"{}\",epoch=\"{}\",process_kind=\"{}\"",
        prometheus_escape(node),
        prometheus_escape(if metrics.has(parrot_remote::metrics::field::RUNTIME) {
            &metrics.runtime
        } else {
            ""
        }),
        prometheus_escape(&metrics.epoch),
        prometheus_escape(&metrics.process_kind),
    )
}

fn prometheus_text(rows: &[(String, Result<MeasuredMetrics, String>)]) -> String {
    use parrot_remote::metrics::field;
    let mut out = String::new();
    out.push_str("# HELP parrot_node_up Whether the node metrics endpoint responded.\n# TYPE parrot_node_up gauge\n");
    out.push_str("# HELP parrot_metric_timestamp_ms Node supplied wall-clock timestamp in milliseconds.\n# TYPE parrot_metric_timestamp_ms gauge\n");
    out.push_str("# HELP parrot_metric_uptime_start_ms Node supplied process start timestamp in milliseconds.\n# TYPE parrot_metric_uptime_start_ms gauge\n");
    const METRICS: &[(u64, &str, &str)] = &[
        (field::CONNECTIONS, "parrot_connections", "gauge"),
        (
            field::HANDSHAKES_OK,
            "parrot_handshakes_ok_total",
            "counter",
        ),
        (
            field::HANDSHAKES_FAILED,
            "parrot_handshakes_failed_total",
            "counter",
        ),
        (field::ASKS_RX, "parrot_asks_received_total", "counter"),
        (field::TELLS_RX, "parrot_tells_received_total", "counter"),
        (field::REPLIES_TX, "parrot_replies_sent_total", "counter"),
        (field::REPLY_ERRS, "parrot_reply_errors_total", "counter"),
        (field::BYTES_RX, "parrot_bytes_received_total", "counter"),
        (field::BYTES_TX, "parrot_bytes_sent_total", "counter"),
        (
            field::HEARTBEATS_RX,
            "parrot_heartbeats_received_total",
            "counter",
        ),
        (field::COMPONENTS, "parrot_components", "gauge"),
        (field::PROCESSES, "parrot_processes", "gauge"),
        (field::MEMORY_RSS, "parrot_memory_rss_bytes", "gauge"),
    ];
    for (_, name, kind) in METRICS {
        out.push_str("# TYPE ");
        out.push_str(name);
        out.push(' ');
        out.push_str(kind);
        out.push('\n');
    }
    for (node, result) in rows {
        match result {
            Ok(metrics) => {
                let labels = prometheus_labels(node, metrics);
                out.push_str("parrot_node_up{");
                out.push_str(&labels);
                out.push_str("} 1\n");
                for (bit, name, _) in METRICS {
                    if let Some(value) = metrics.value(*bit) {
                        out.push_str(name);
                        out.push('{');
                        out.push_str(&labels);
                        out.push_str("} ");
                        out.push_str(&value.to_string());
                        out.push('\n');
                    }
                }
                for (bit, name) in [
                    (field::TS_MS, "parrot_metric_timestamp_ms"),
                    (field::UPTIME_START_MS, "parrot_metric_uptime_start_ms"),
                ] {
                    if let Some(value) = metrics.value(bit) {
                        out.push_str(name);
                        out.push('{');
                        out.push_str(&labels);
                        out.push_str("} ");
                        out.push_str(&value.to_string());
                        out.push('\n');
                    }
                }
            }
            Err(_) => {
                out.push_str("parrot_node_up{node=\"");
                out.push_str(&prometheus_escape(node));
                out.push_str("\"} 0\n");
            }
        }
    }
    out
}

// A clock domain for this collector process. Browser/network delay must not
// replace each node's actual receipt interval. This is not an authentication ID.
fn sample_clock() -> &'static (Instant, String) {
    static CLOCK: OnceLock<(Instant, String)> = OnceLock::new();
    CLOCK.get_or_init(|| {
        (
            Instant::now(),
            format!("{}:{:?}", std::process::id(), SystemTime::now()),
        )
    })
}
async fn response(
    socket: &mut TcpStream,
    status: &str,
    ctype: &str,
    body: &str,
) -> std::io::Result<()> {
    let header=format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n{SECURITY_HEADERS}\r\n",body.len());
    socket.write_all(header.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.shutdown().await
}
async fn handle(
    mut socket: TcpStream,
    port: u16,
    client: &Arc<RemoteActorSystem>,
    nodes: &[NodeSpec],
) -> std::io::Result<()> {
    let mut header = Vec::with_capacity(1024);
    let end = loop {
        if let Some(index) = header.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
        if header.len() == HEADER_LIMIT {
            return response(
                &mut socket,
                "431 Request Header Fields Too Large",
                "text/plain",
                "header too large",
            )
            .await;
        }
        let mut chunk = [0; 1024];
        let remaining = chunk.len().min(HEADER_LIMIT - header.len());
        let n = socket.read(&mut chunk[..remaining]).await?;
        if n == 0 {
            return Ok(());
        }
        header.extend_from_slice(&chunk[..n]);
    };
    let path = std::str::from_utf8(&header[..end])
        .map_err(|_| "400 Bad Request")
        .and_then(|s| request_path(s, port));
    let path = match path {
        Ok(path) => path,
        Err(status) => {
            return response(&mut socket, status, "text/plain", "request rejected").await
        }
    };
    match path {
        "/" | "/index.html" => {
            response(&mut socket, "200 OK", "text/html; charset=utf-8", HTML).await
        }
        "/dashboard.js" => {
            response(
                &mut socket,
                "200 OK",
                "text/javascript; charset=utf-8",
                SCRIPT,
            )
            .await
        }
        "/api/metrics" => {
            let mut data = serde_json::Map::new();
            let (origin, clock_id) = sample_clock();
            for (name, result) in sampling::collect(client, nodes).await {
                data.insert(
                    name,
                    match result {
                        Ok(sample) => {
                            let mut value = snapshot_json(&sample.metrics);
                            value["sample_clock"] = clock_id.clone().into();
                            value["sampled_at_ns"] = sample
                                .at
                                .duration_since(*origin)
                                .as_nanos()
                                .to_string()
                                .into();
                            value
                        }
                        Err(error) => serde_json::json!({"error":error}),
                    },
                );
            }
            response(
                &mut socket,
                "200 OK",
                "application/json",
                &serde_json::Value::Object(data).to_string(),
            )
            .await
        }
        "/metrics" => {
            let rows = sampling::collect(client, nodes)
                .await
                .into_iter()
                .map(|(name, result)| (name, result.map(|sample| sample.metrics)))
                .collect::<Vec<_>>();
            response(
                &mut socket,
                "200 OK",
                "text/plain; version=0.0.4; charset=utf-8",
                &prometheus_text(&rows),
            )
            .await
        }
        _ => response(&mut socket, "404 Not Found", "text/plain", "not found").await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_remote::admin_v2::MetricsSnapshot;
    use parrot_remote::metrics::field;

    #[test]
    fn prometheus_output_omits_unavailable_fields_and_escapes_labels() {
        let rows = vec![(
            "node\"\n".into(),
            Ok(MeasuredMetrics {
                snapshot: MetricsSnapshot {
                    ts_ms: 12,
                    asks_rx: 4,
                    ..Default::default()
                },
                available: field::TS_MS | field::ASKS_RX,
                epoch: "e\\\"\n".into(),
                process_kind: "actors".into(),
            }),
        )];
        let text = prometheus_text(&rows);
        assert!(text.contains("parrot_node_up{node=\"node\\\"\\n\",runtime=\"\",epoch=\"e\\\\\\\"\\n\",process_kind=\"actors\"} 1"));
        assert!(text.contains("parrot_asks_received_total{"));
        assert!(text.contains("} 4\n"));
        assert!(text.contains("parrot_metric_timestamp_ms{"));
        assert!(!text.contains("parrot_connections{"));
    }

    #[test]
    fn prometheus_output_reports_collection_failures_without_fake_values() {
        let text = prometheus_text(&[("offline".into(), Err("closed".into()))]);
        assert!(text.contains("parrot_node_up{node=\"offline\"} 0\n"));
        assert!(!text.contains("parrot_asks_received_total{node=\"offline\"}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actual_http_exposes_measured_fields_and_each_receipt_clock() {
        let target = RemoteActorSystem::new(
            parrot_remote::RemoteConfig::tcp("sparse", Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development(),
            Arc::new(crate::NoopLookup),
        )
        .unwrap();
        target.start().await.unwrap();
        let node = NodeSpec {
            name: "sparse".into(),
            dialect: "erl",
            addr: target.local_addr().unwrap(),
        };
        let client = crate::connect(std::slice::from_ref(&node), true)
            .await
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_on(
            listener,
            client.clone(),
            vec![node],
            2,
            Duration::from_secs(5),
        ));
        let result: anyhow::Result<Vec<serde_json::Value>> = async {
            let mut responses = Vec::new();
            for _ in 0..2 {
                let mut socket = TcpStream::connect(addr).await?;
                socket
                    .write_all(
                        format!(
                            "GET /api/metrics HTTP/1.1\r\nHost: localhost:{}\r\n\r\n",
                            addr.port()
                        )
                        .as_bytes(),
                    )
                    .await?;
                let mut bytes = Vec::new();
                tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut bytes))
                    .await??;
                let response = String::from_utf8(bytes)?;
                let (_, body) = response
                    .split_once("\r\n\r\n")
                    .ok_or_else(|| anyhow::anyhow!("missing HTTP body"))?;
                responses.push(serde_json::from_str(body)?);
            }
            let mut socket = TcpStream::connect(addr).await?;
            socket
                .write_all(
                    format!(
                        "GET /metrics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                        addr.port()
                    )
                    .as_bytes(),
                )
                .await?;
            let mut bytes = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut bytes)).await??;
            let response = String::from_utf8(bytes)?;
            assert!(response.contains("Content-Type: text/plain; version=0.0.4; charset=utf-8"));
            let (_, body) = response
                .split_once("\r\n\r\n")
                .ok_or_else(|| anyhow::anyhow!("missing Prometheus body"))?;
            assert!(body.contains("# TYPE parrot_node_up gauge\n"));
            assert!(body.contains("parrot_node_up{node=\"sparse\""));
            Ok(responses)
        }
        .await;
        server.abort();
        let _ = server.await;
        client.shutdown().await.unwrap();
        target.shutdown().await.unwrap();
        let responses = result.unwrap();
        let first = &responses[0]["sparse"];
        let second = &responses[1]["sparse"];
        assert_eq!(first["connections"], "1");
        assert!(first["components"].is_null());
        assert!(first["component_states"].is_null());
        assert!(first["processes"].is_null());
        assert!(!first["epoch"].as_str().unwrap().is_empty());
        assert_eq!(first["epoch"], second["epoch"]);
        assert_eq!(first["sample_clock"], second["sample_clock"]);
        assert!(
            second["sampled_at_ns"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > first["sampled_at_ns"]
                    .as_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
        );
    }
    #[test]
    fn http_requires_explicit_loopback_host_get_and_no_body() {
        assert_eq!(
            request_path(
                "GET /api/metrics HTTP/1.1\r\nHost: localhost:8190\r\n\r\n",
                8190
            ),
            Ok("/api/metrics")
        );
        assert_eq!(
            request_path(
                "GET /metrics HTTP/1.1\r\nHost: localhost:8190\r\n\r\n",
                8190
            ),
            Ok("/metrics")
        );
        for (request, status) in [
            (
                "GET / HTTP/1.1\r\nHost: evil.test:8190\r\n\r\n",
                "403 Forbidden",
            ),
            (
                "GET / HTTP/1.1\r\nHost: localhost:8190\r\nHost: evil\r\n\r\n",
                "400 Bad Request",
            ),
            (
                "POST / HTTP/1.1\r\nHost: localhost:8190\r\n\r\n",
                "405 Method Not Allowed",
            ),
            ("GET / HTTP/1.1\r\n\r\n", "400 Bad Request"),
            (
                "GET / HTTP/1.1\r\nHost: localhost:8190\r\nContent-Length: 1\r\n\r\n",
                "400 Bad Request",
            ),
            (
                "GET / HTTP/1.1\r\nHost: localhost:8190\r\nTransfer-Encoding: chunked\r\n\r\n",
                "400 Bad Request",
            ),
        ] {
            assert_eq!(request_path(request, 8190), Err(status));
        }
    }
    #[test]
    fn snapshot_preserves_unsigned64_counter_precision() {
        let m = MeasuredMetrics {
            snapshot: parrot_remote::admin_v2::MetricsSnapshot {
                asks_rx: u64::MAX,
                ..Default::default()
            },
            available: super::super::field::ALL,
            ..Default::default()
        };
        let value = snapshot_json(&m);
        assert_eq!(value["asks_rx"], "18446744073709551615");
        assert_eq!(value["connections"], "0");
        assert!(value["component_states"].is_array());
    }
    #[test]
    fn unavailable_fields_are_null_even_if_raw_storage_contains_nonzero_values() {
        let m = MeasuredMetrics {
            snapshot: parrot_remote::admin_v2::MetricsSnapshot {
                asks_rx: 99,
                runtime: "hidden".into(),
                ..Default::default()
            },
            available: super::super::field::MEMORY_RSS,
            epoch: "owner".into(),
            ..Default::default()
        };
        let value = snapshot_json(&m);
        assert!(value["asks_rx"].is_null());
        assert!(value["runtime"].is_null());
        assert!(value["component_states"].is_null());
        assert_eq!(value["memory_rss"], "0");
        assert_eq!(value["epoch"], "owner");
        assert_eq!(value["available"], "32768");
    }
    async fn server(
        deadline: Duration,
        capacity: usize,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<anyhow::Result<()>>,
        Arc<RemoteActorSystem>,
    ) {
        let client = RemoteActorSystem::new(
            parrot_remote::RemoteConfig::tcp("obs-web-test", None),
            Arc::new(crate::NoopLookup),
        )
        .unwrap();
        client.start().await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(serve_on(
            listener,
            client.clone(),
            vec![],
            capacity,
            deadline,
        ));
        (addr, task, client)
    }
    #[tokio::test]
    async fn fragmented_headers_are_completed_and_security_headers_are_sent() {
        let (addr, server, client) = server(Duration::from_secs(2), 2).await;
        let mut socket = TcpStream::connect(addr).await.unwrap();
        socket.write_all(b"GET /dashboard").await.unwrap();
        tokio::task::yield_now().await;
        socket
            .write_all(
                format!(".js HTTP/1.1\r\nHost: localhost:{}\r\n\r\n", addr.port()).as_bytes(),
            )
            .await
            .unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        let body = String::from_utf8(bytes).unwrap();
        server.abort();
        let _ = server.await;
        client.shutdown().await.unwrap();
        assert!(body.starts_with("HTTP/1.1 200 OK"));
        assert!(body.contains("Content-Security-Policy: default-src 'none'"));
        assert!(body.contains("X-Content-Type-Options: nosniff"));
        assert!(body.ends_with(SCRIPT));
    }
    #[tokio::test]
    async fn oversized_and_idle_connections_release_capacity() {
        let (addr, server, client) = server(Duration::from_millis(100), 1).await;
        let mut oversized = TcpStream::connect(addr).await.unwrap();
        oversized
            .write_all(&vec![b'x'; HEADER_LIMIT])
            .await
            .unwrap();
        let mut reply = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), oversized.read_to_end(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert!(reply.starts_with(b"HTTP/1.1 431"));
        let mut idle = TcpStream::connect(addr).await.unwrap();
        idle.write_all(b"GET /").await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), idle.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(bytes.is_empty());
        let mut healthy = TcpStream::connect(addr).await.unwrap();
        healthy
            .write_all(
                format!(
                    "GET /api/metrics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                    addr.port()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut reply = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), healthy.read_to_end(&mut reply))
            .await
            .unwrap()
            .unwrap();
        server.abort();
        let _ = server.await;
        client.shutdown().await.unwrap();
        assert!(reply.starts_with(b"HTTP/1.1 200"));
        assert!(reply.ends_with(b"{}"));
    }
    #[tokio::test]
    async fn cancelling_server_closes_owned_pending_connections() {
        let (addr, server, client) = server(Duration::from_secs(60), 2).await;
        let mut socket = TcpStream::connect(addr).await.unwrap();
        socket.write_all(b"GET /").await.unwrap();
        // A completed sibling request proves the accept loop made progress past the first.
        let mut healthy = TcpStream::connect(addr).await.unwrap();
        healthy
            .write_all(
                format!("GET / HTTP/1.1\r\nHost: localhost:{}\r\n\r\n", addr.port()).as_bytes(),
            )
            .await
            .unwrap();
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), healthy.read_to_end(&mut output))
            .await
            .unwrap()
            .unwrap();
        assert!(output.starts_with(b"HTTP/1.1 200"));
        server.abort();
        let _ = server.await;
        let mut output = Vec::new();
        let result = tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut output))
            .await
            .unwrap();
        assert!(
            result.is_ok()
                || matches!(result,Err(ref e) if e.kind()==std::io::ErrorKind::ConnectionReset)
        );
        client.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn full_connection_capacity_does_not_execute_a_queued_request() {
        let (addr, server, client) = server(Duration::from_secs(3), 1).await;
        let mut held = TcpStream::connect(addr).await.unwrap();
        held.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
        let mut queued = TcpStream::connect(addr).await.unwrap();
        queued
            .write_all(
                format!(
                    "GET /api/metrics HTTP/1.1\r\nHost: localhost:{}\r\n\r\n",
                    addr.port()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut byte = [0; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(150), queued.read(&mut byte))
                .await
                .is_err(),
            "queued HTTP request bypassed the sole occupied connection slot"
        );
        held.write_all(format!("Host: localhost:{}\r\n\r\n", addr.port()).as_bytes())
            .await
            .unwrap();
        let mut first = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), held.read_to_end(&mut first))
            .await
            .unwrap()
            .unwrap();
        let mut second = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), queued.read_to_end(&mut second))
            .await
            .unwrap()
            .unwrap();
        server.abort();
        let _ = server.await;
        client.shutdown().await.unwrap();
        assert!(first.starts_with(b"HTTP/1.1 200"));
        assert!(second.starts_with(b"HTTP/1.1 200"));
        assert!(second.ends_with(b"{}"));
    }
}
