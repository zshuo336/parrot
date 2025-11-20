//! A configured facade supervises its own engine and keeps root references stable.
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, FactoryOptions, SpawnOptions},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType},
    system::{ActorSystemConfig, GuardianConfig},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Counter(usize);
enum Command {
    Read,
    Fail,
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
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match *message.downcast::<Command>().map_err(|_| {
                ActorError::MessageHandlingError("expected a counter command".into())
            })? {
                Command::Read => Ok(Box::new(self.0) as BoxedMessage),
                Command::Fail => Err(ActorError::MessageHandlingError("example failure".into())),
            }
        })
    }
}

async fn read(root: &BoxedActorRef) -> ActorResult<usize> {
    root.send(Box::new(Command::Read))
        .await?
        .downcast::<usize>()
        .map(|generation| *generation)
        .map_err(|_| ActorError::MessageHandlingError("expected a generation reply".into()))
}

async fn await_recovery(root: &BoxedActorRef) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match read(root).await {
                Ok(1) => return Ok(()),
                // A logical reference may be temporarily unavailable between
                // retiring its old generation and publishing the replacement.
                Err(ActorError::Stopped) => tokio::task::yield_now().await,
                Ok(generation) => anyhow::bail!("unexpected recovered generation {generation}"),
                Err(error) => return Err(error.into()),
            }
        }
    })
    .await?
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let facade = ParrotActorSystem::start(ActorSystemConfig {
        guardian_config: GuardianConfig {
            max_restarts: 1,
            restart_window: Duration::from_secs(60),
            supervision_strategy: SupervisorStrategyType::OneForAll(OneForAllStrategy {
                max_restarts: 1,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
            }),
        },
        ..Default::default()
    })
    .await?;
    let work = async {
        let mut roots = Vec::new();
        let mut counts = Vec::new();
        for path in ["/first", "/second"] {
            let builds = Arc::new(AtomicUsize::new(0));
            let source = builds.clone();
            roots.push(
                facade
                    .factory_runtime()
                    .ok_or_else(|| anyhow::anyhow!("missing factory runtime"))?
                    .spawn_factory(
                        Box::new(move || Box::new(Counter(source.fetch_add(1, Ordering::SeqCst)))),
                        FactoryOptions {
                            spawn: SpawnOptions::at(path),
                            ..Default::default()
                        },
                    )
                    .await?,
            );
            counts.push(builds);
        }
        for root in &roots {
            anyhow::ensure!(read(root).await? == 0, "wrong initial generation");
        }
        anyhow::ensure!(
            matches!(roots[0].send(Box::new(Command::Fail)).await,
                Err(ActorError::MessageHandlingError(message)) if message == "example failure"),
            "expected the original handler failure"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while counts.iter().any(|count| count.load(Ordering::SeqCst) != 2) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        for root in &roots {
            await_recovery(root).await?;
        }
        anyhow::ensure!(
            counts.iter().all(|count| count.load(Ordering::SeqCst) == 2),
            "unexpected extra reconstruction"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = facade.shutdown_runtime().await;
    work?;
    stopped?;
    println!("Facade guardian recovered both roots through their original references.");
    Ok(())
}
