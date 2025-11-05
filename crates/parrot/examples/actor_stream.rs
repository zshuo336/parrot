//! Run with: cargo run --locked -p parrot --example actor_stream
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::ActorContext,
    stream::StreamRegistryExt,
    trace::{SpanForest, SpanGuard, SpanKind, SpanStatus, TraceContext, TraceRecorder},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::time::Duration;
use tokio::sync::oneshot;

struct Sum {
    trace: TraceContext,
    value: u32,
    finished: Option<oneshot::Sender<u32>>,
}
struct ThreadSum(Sum);
struct ActixSum(Sum);
impl ActixEngineExt for ActixSum {}
macro_rules! stream_actor {
    ($actor:ident, $context:ty) => {
        impl Actor for $actor {
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
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    self.0.trace.in_scope(|| {
                        ctx.stream_registry()
                            .add_stream(futures::stream::iter([1_u32, 2, 3]))
                    })
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                item: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    // The default handle_stream forwards each item here.
                    self.0.value += *item.downcast::<u32>().expect("u32 stream protocol");
                    Ok(Box::new(()) as BoxedMessage)
                })
            }
            fn stream_finished<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    if let Some(finished) = self.0.finished.take() {
                        let _ = finished.send(self.0.value);
                    }
                    Ok(())
                })
            }
        }
    };
}
stream_actor!(ThreadSum, parrot::thread::ThreadContext<Self>);
stream_actor!(
    ActixSum,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
#[actix::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let recorder = TraceRecorder::new(32)?;
    let root = recorder.root();
    let operation = root.in_scope(|| SpanGuard::current(SpanKind::Application, "/stream-example"));
    let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
    let actix = parrot::actix::ActixActorSystem::new().await?;
    let (thread_send, thread_result) = oneshot::channel();
    let (actix_send, actix_result) = oneshot::channel();
    thread
        .spawn_at(
            ThreadSum(Sum {
                trace: root.clone(),
                value: 0,
                finished: Some(thread_send),
            }),
            "/sum",
            None,
            Default::default(),
        )
        .await?;
    actix
        .spawn_root_typed(
            ActixSum(Sum {
                trace: root.clone(),
                value: 0,
                finished: Some(actix_send),
            }),
            EmptyConfig,
        )
        .await?;
    let (thread_sum, actix_sum) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::try_join!(thread_result, actix_result)
    })
    .await??;
    assert_eq!((thread_sum, actix_sum), (6, 6));
    println!("Stream sums: Thread={thread_sum}, Actix={actix_sum}");
    // The business callback sends its result before worker ownership retires.
    // Wait for both admitted stream lifetimes before requesting shutdown.
    tokio::time::timeout(Duration::from_secs(2), async {
        while recorder
            .snapshot()
            .records
            .iter()
            .filter(|record| record.kind == SpanKind::ManagedTask)
            .count()
            != 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    thread.shutdown_internal().await?;
    actix.shutdown().await?;
    operation.finish(SpanStatus::Ok);
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.counts.completed, 13);
    assert_eq!(snapshot.counts.ok, 13);
    assert_eq!(snapshot.counts.overwritten, 0);
    assert_eq!(
        snapshot
            .records
            .iter()
            .filter(|r| r.kind == SpanKind::StreamStarted)
            .count(),
        2
    );
    assert_eq!(
        snapshot
            .records
            .iter()
            .filter(|r| r.kind == SpanKind::StreamItem)
            .count(),
        6
    );
    assert_eq!(
        snapshot
            .records
            .iter()
            .filter(|r| r.kind == SpanKind::StreamFinished)
            .count(),
        2
    );
    let lifetimes: Vec<_> = snapshot
        .records
        .iter()
        .filter(|record| record.kind == SpanKind::ManagedTask)
        .collect();
    assert_eq!(lifetimes.len(), 2);
    assert!(
        lifetimes
            .iter()
            .all(|record| record.parent_span_id == Some(root.span_id())
                && record.trace_id == root.trace_id()
                && record.status == SpanStatus::Ok)
    );
    let forest = SpanForest::new(&snapshot)?.render_text(16384)?;
    assert_eq!(forest.emitted_records, 13);
    assert_eq!(forest.omitted_records, 0);
    print!("{}", forest.text);
    Ok(())
}
