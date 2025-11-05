//! Export a real actor operation as an OTLP/JSON request body on stdout.
//! CARGO_BUILD_JOBS=2 cargo run --locked -p parrot-app --example trace_export
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    trace::{SpanGuard, SpanKind, SpanStatus, TraceRecorder},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use parrot_app::debug::{export_otlp_json, MAX_OTLP_JSON_BYTES};
use std::io::Write;

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
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let actor = system.spawn_root_typed_thread(Sum(0), EmptyConfig).await?;
    let recorder = TraceRecorder::new(16)?;
    let root = recorder.root();
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
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.counts.overwritten, 0);
    assert_eq!(snapshot.counts.rejected_after_close, 0);
    assert_eq!(snapshot.records.len(), 4);
    assert!(snapshot
        .records
        .iter()
        .all(|span| span.status == SpanStatus::Ok));
    // Export borrows this snapshot: retain it for retries if the consumer fails.
    let bytes = export_otlp_json(&snapshot, "parrot-sum-example", MAX_OTLP_JSON_BYTES)?;
    std::io::stdout().lock().write_all(&bytes)?;
    Ok(())
}
