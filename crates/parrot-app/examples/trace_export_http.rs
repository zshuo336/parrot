//! Send real actor spans to a local HTTP collector and print its received JSON.
//! CARGO_BUILD_JOBS=2 cargo run --locked -p parrot-app --example trace_export_http
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    trace::{SpanGuard, SpanKind},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use parrot_app::debug::{TraceExportConfig, TraceExportEnd, TraceExporter};
use std::io::Write;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

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
                .map_err(|_| ActorError::MessageHandlingError("expected a u64 increment".into()))?;
            self.0 += *value;
            Ok(Box::new(self.0) as BoxedMessage)
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A loopback collector makes this example executable without an external
    // service. Production endpoints should use HTTPS (the default requirement).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1/traces", listener.local_addr()?);
    let collector = tokio::spawn(async move {
        let mut documents = Vec::new();
        let mut received = 0;
        while received < 4 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = tokio::io::BufReader::new(stream);
            let mut head = String::new();
            let mut length = None;
            loop {
                let mut line = String::new();
                assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
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
            assert!(head.starts_with("POST /v1/traces HTTP/1.1\r\n"));
            let length = length.unwrap();
            assert!(length <= 8 * 1024 * 1024);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            stream.get_mut().write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
            stream.get_mut().shutdown().await.unwrap();
            let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
            received += document["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .unwrap()
                .len();
            documents.push(body);
        }
        assert_eq!(received, 4);
        documents
    });
    let mut config = TraceExportConfig::new(endpoint, "parrot-http-sum-example");
    config.allow_insecure_http = true;
    config.interval = Duration::from_secs(60);
    let mut exporter = TraceExporter::start(config)?;
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let actor = system.spawn_root_typed_thread(Sum(0), EmptyConfig).await?;
    let root = exporter.root();
    let operation = root.in_scope(|| SpanGuard::current(SpanKind::Application, "/sum-operation"));
    let result: ActorResult<u64> = root
        .scope(async {
            let mut sum = 0;
            for value in [1_u64, 2, 3] {
                sum = *actor
                    .send(Box::new(value))
                    .await?
                    .downcast::<u64>()
                    .map_err(|_| ActorError::MessageHandlingError("expected a u64 sum".into()))?;
            }
            Ok(sum)
        })
        .await;
    operation.finish_result(&result);
    // Await actual retirement before publishing a terminal recording receipt.
    let stopped = actor.stop().await;
    let shutdown = system.shutdown_internal().await;
    stopped?;
    shutdown?;
    assert_eq!(result?, 6);
    let report = exporter.shutdown(Duration::from_secs(5)).await?;
    assert_eq!(report.end, TraceExportEnd::Complete);
    assert!((1..=4).contains(&report.status.accepted_batches));
    assert_eq!(report.status.accepted_spans, 4);
    assert_eq!(report.remaining.counts.overwritten, 0);
    assert_eq!(report.remaining.counts.rejected_after_close, 0);
    assert!(report.remaining.terminal && report.remaining.records.is_empty());
    assert!(report.pending.is_none());
    let documents = tokio::time::timeout(Duration::from_secs(5), collector).await??;
    let mut output = std::io::stdout().lock();
    for bytes in documents {
        output.write_all(&bytes)?;
        output.write_all(b"\n")?;
    }
    Ok(())
}
