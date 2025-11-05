//! Two real TCP endpoints in one process; separate processes are covered by wire_trace_context.
//! CARGO_BUILD_JOBS=2 cargo run --locked -p parrot-app --example wire_trace
use parrot::thread::{ThreadActorConfig, ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    trace::{SpanGuard, SpanKind, SpanStatus, TraceRecorder},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::debug::{export_otlp_json, MAX_OTLP_JSON_BYTES};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use serde::{Deserialize, Serialize};
use std::{io::Write, sync::Arc, time::Duration};

#[derive(Serialize, Deserialize)]
struct Increment(u64);
#[derive(Serialize, Deserialize)]
struct Total(u64);
macro_rules! codec {
    ($ty:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$ty>(),
                encode: |m| parrot_api::message::serde_remote_serialize(m.downcast_ref::<$ty>().ok_or("wrong type")?),
                decode: |b| Ok(Box::new(parrot_api::message::serde_remote_deserialize::<$ty>(b)?) as BoxedMessage),
            }
        }
    };
}
codec!(Increment, "bin:wire-trace.Increment#v1");
codec!(Total, "bin:wire-trace.Total#v1");
struct Sum(u64);
impl Actor for Sum {
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
        Box::pin(async move {
            let increment = message
                .downcast::<Increment>()
                .map_err(|_| ActorError::MessageHandlingError("expected Increment".into()))?;
            self.0 += increment.0;
            Ok(Box::new(Total(self.0)) as BoxedMessage)
        })
    }
}
struct Lookup(Option<BoxedActorRef>);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/sum")
            .then(|| self.0.as_ref().map(|a| a.clone_boxed()))
            .flatten()
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = ThreadActorSystem::shared(Default::default());
    let actor = engine
        .spawn_at(Sum(0), "/sum", None, ThreadActorConfig::default())
        .await?;
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("sum-service", Some("127.0.0.1:0".parse()?)),
        Arc::new(Lookup(Some(Box::new(actor)))),
    )?;
    let server_recorder = TraceRecorder::new(16)?;
    server.set_trace_recorder(Some(server_recorder.clone()));
    server.start().await?;
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("sum-client", None),
        Arc::new(Lookup(None)),
    )?;
    client.start().await?;
    client
        .connect(&NodeAddr::tcp(
            "sum-service",
            server.local_addr().ok_or("server has no address")?,
        ))
        .await?;
    let target = client.remote_ref("parrot://sum-service/sum")?;
    let client_recorder = TraceRecorder::new(16)?;
    let root = client_recorder.root();
    let operation =
        root.in_scope(|| SpanGuard::current(SpanKind::Application, "/remote-sum-operation"));
    let result: ActorResult<u64> = root
        .scope(async {
            let mut sum = 0;
            for value in [1, 2, 3] {
                sum = target
                    .send_with_timeout(Box::new(Increment(value)), Some(Duration::from_secs(5)))
                    .await?
                    .downcast::<Total>()
                    .map_err(|_| ActorError::MessageHandlingError("expected Total".into()))?
                    .0;
            }
            Ok(sum)
        })
        .await;
    operation.finish_result(&result);
    let client_stopped = client.shutdown().await;
    let server_stopped = server.shutdown().await;
    let engine_stopped = engine.shutdown_internal().await;
    client_stopped?;
    server_stopped?;
    engine_stopped?;
    assert_eq!(result?, 6);
    let mut resources = Vec::new();
    for (recorder, name, count) in [
        (client_recorder, "sum-client", 1),
        (server_recorder, "sum-service", 3),
    ] {
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.records.len(), count);
        assert_eq!(snapshot.counts.overwritten, 0);
        assert_eq!(snapshot.counts.rejected_after_close, 0);
        for span in &snapshot.records {
            assert_eq!(span.trace_id, root.trace_id());
            assert_eq!(span.status, SpanStatus::Ok);
            if name == "sum-service" {
                assert_eq!(span.parent_span_id, Some(root.span_id()));
            }
        }
        let document: serde_json::Value =
            serde_json::from_slice(&export_otlp_json(&snapshot, name, MAX_OTLP_JSON_BYTES)?)?;
        resources.extend(
            document["resourceSpans"]
                .as_array()
                .ok_or("missing resourceSpans")?
                .iter()
                .cloned(),
        );
    }
    // One OTLP request with distinct client/server resources, retaining both recorder receipts.
    std::io::stdout().lock().write_all(&serde_json::to_vec(
        &serde_json::json!({"resourceSpans":resources}),
    )?)?;
    Ok(())
}
