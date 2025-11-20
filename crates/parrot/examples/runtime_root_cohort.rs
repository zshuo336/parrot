//! Independent root recovery through the engine-neutral facade capability.
use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, SpawnOptions},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType},
    system::ActorSystemConfig,
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
            match *message
                .downcast::<Command>()
                .map_err(|_| ActorError::MessageHandlingError("expected a command".into()))?
            {
                Command::Read => Ok(Box::new(self.0) as BoxedMessage),
                Command::Fail => Err(ActorError::MessageHandlingError("example failure".into())),
            }
        })
    }
}
async fn read(actor: &BoxedActorRef) -> ActorResult<usize> {
    actor
        .send(Box::new(Command::Read))
        .await?
        .downcast::<usize>()
        .map(|value| *value)
        .map_err(|_| ActorError::MessageHandlingError("expected a generation".into()))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let system = ParrotActorSystem::start(ActorSystemConfig::default()).await?;
    let work = async {
        let group = system
            .cohort_runtime()
            .ok_or_else(|| anyhow::anyhow!("runtime has no root groups"))?
            .root_cohort(SupervisorStrategyType::OneForAll(OneForAllStrategy {
                max_restarts: 1,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
            }))
            .await?;
        let mut actors = Vec::new();
        let mut counts = Vec::new();
        for path in ["/workers/first", "/workers/second"] {
            let builds = Arc::new(AtomicUsize::new(0));
            let source = builds.clone();
            actors.push(
                group
                    .spawn_factory(
                        Box::new(move || Box::new(Counter(source.fetch_add(1, Ordering::SeqCst)))),
                        SpawnOptions::at(path),
                    )
                    .await?,
            );
            counts.push(builds);
        }
        for actor in &actors {
            anyhow::ensure!(read(actor).await? == 0);
        }
        anyhow::ensure!(matches!(actors[0].send(Box::new(Command::Fail)).await,
            Err(ActorError::MessageHandlingError(message)) if message == "example failure"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while counts.iter().any(|count| count.load(Ordering::SeqCst) != 2) {
                tokio::task::yield_now().await;
            }
            for actor in &actors {
                loop {
                    match read(actor).await {
                        Ok(1) => break,
                        Err(ActorError::Stopped) => tokio::task::yield_now().await,
                        Ok(value) => anyhow::bail!("unexpected generation {value}"),
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        anyhow::ensure!(group.members().len() == 2);
        group.stop().await?;
        println!("Both roots recovered through their original engine-neutral references.");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = system.shutdown_runtime().await;
    work?;
    stopped?;
    Ok(())
}
