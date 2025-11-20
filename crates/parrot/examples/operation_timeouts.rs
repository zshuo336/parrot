//! Execution deadlines cancel actor work independently of caller ask timeouts.
use parrot::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, SpawnOptions},
    errors::ActorError,
    stream::StreamRegistryExt,
    supervisor::{DefaultStrategy, SupervisorStrategyType},
    system::{ActorSystemConfig, GuardianConfig, SystemTimeouts},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Cancellation(Arc<AtomicUsize>);
impl Drop for Cancellation {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct Counter {
    entered: usize,
    cancelled: Arc<AtomicUsize>,
    stream_error: Option<tokio::sync::oneshot::Sender<()>>,
}
enum Command {
    Wait,
    Read,
    Stream,
}
impl Actor for Counter {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match *message.downcast::<Command>().map_err(|_| {
                ActorError::MessageHandlingError("expected a counter command".into())
            })? {
                Command::Wait => {
                    self.entered += 1;
                    let _cancellation = Cancellation(self.cancelled.clone());
                    std::future::pending::<()>().await;
                    unreachable!()
                }
                Command::Read => Ok(Box::new(self.entered) as BoxedMessage),
                Command::Stream => {
                    context
                        .stream_registry()
                        .add_stream(futures::stream::iter([1_u32]))?;
                    Ok(Box::new(()) as BoxedMessage)
                }
            }
        })
    }
    fn handle_stream<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.entered += 1;
            let _cancellation = Cancellation(self.cancelled.clone());
            std::future::pending().await
        })
    }
    fn stream_error<'a>(
        &'a mut self,
        error: ActorError,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if !matches!(error, ActorError::Timeout) {
                return Err(error);
            }
            if let Some(completed) = self.stream_error.take() {
                let _ = completed.send(());
            }
            Ok(())
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let system = ParrotActorSystem::start(ActorSystemConfig {
        guardian_config: GuardianConfig {
            supervision_strategy: SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure),
            ..Default::default()
        },
        timeouts: SystemTimeouts {
            actor_creation: Duration::from_secs(2),
            message_handling: Duration::from_millis(50),
            system_shutdown: Duration::from_secs(2),
        },
        ..Default::default()
    })
    .await?;
    let cancelled = Arc::new(AtomicUsize::new(0));
    let (stream_error, stream_finished) = tokio::sync::oneshot::channel();
    let work = async {
        let actor = system
            .spawn(
                Box::new(Counter {
                    entered: 0,
                    cancelled: cancelled.clone(),
                    stream_error: Some(stream_error),
                }),
                Box::new(()),
                SpawnOptions::at("/counter"),
            )
            .await?;
        anyhow::ensure!(
            matches!(
                actor.send(Box::new(Command::Wait)).await,
                Err(ActorError::Timeout)
            ),
            "missing execution timeout"
        );
        anyhow::ensure!(
            cancelled.load(Ordering::SeqCst) == 1,
            "unfinished user future"
        );
        let count = actor.send(Box::new(Command::Read)).await?;
        anyhow::ensure!(
            *count
                .downcast::<usize>()
                .map_err(|_| anyhow::anyhow!("wrong reply type"))?
                == 1,
            "Resume lost state or replayed work"
        );
        actor.send(Box::new(Command::Stream)).await?;
        tokio::time::timeout(Duration::from_secs(5), stream_finished).await??;
        anyhow::ensure!(cancelled.load(Ordering::SeqCst) == 2, "stream error hook ran before cancellation");
        let count = actor.send(Box::new(Command::Read)).await?;
        anyhow::ensure!(*count.downcast::<usize>().map_err(|_| anyhow::anyhow!("wrong reply type"))? == 2, "stream timeout lost state or replayed work");
        println!(
            "Message and stream deadlines retired user futures; recovery preserved state without replay."
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let shutdown = system.shutdown_runtime().await;
    work?;
    shutdown?;
    Ok(())
}
