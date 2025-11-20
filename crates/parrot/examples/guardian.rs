//! A finite automatic OneForAll recovery with stable root references.
use parrot::thread::{ThreadActorSystem, ThreadActorSystemConfig};
use parrot_api::{
    Message,
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType},
    system::GuardianConfig,
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    types::{ActorResult, BoxedFuture},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct Counter(usize);
enum Command {
    Generation,
    Fail,
}
impl Message for Command {
    type Result = usize;
}
impl ParrotMsgVariant<Counter> for Command {
    fn inject(value: Self) -> Self {
        value
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
impl ParrotTypedDispatch for Counter {
    type Msg = Command;
    type Reply = usize;
    fn dispatch(&mut self, command: Command) -> BoxedFuture<'_, ActorResult<usize>> {
        Box::pin(async move {
            match command {
                Command::Generation => Ok(self.0),
                Command::Fail => Err(ActorError::MessageHandlingError("example failure".into())),
            }
        })
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let system = ThreadActorSystem::with_guardian(
        ThreadActorSystemConfig::default(),
        GuardianConfig {
            max_restarts: 1,
            restart_window: Duration::from_secs(60),
            supervision_strategy: SupervisorStrategyType::OneForAll(OneForAllStrategy {
                max_restarts: 1,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
            }),
        },
    )
    .await?;
    let builds = Arc::new(AtomicUsize::new(0));
    let source = builds.clone();
    let first = system
        .spawn_typed_factory(
            move || Counter(source.fetch_add(1, Ordering::SeqCst)),
            "/first",
        )
        .await?;
    let other_builds = Arc::new(AtomicUsize::new(0));
    let source = other_builds.clone();
    let second = system
        .spawn_typed_factory(
            move || Counter(source.fetch_add(1, Ordering::SeqCst)),
            "/second",
        )
        .await?;
    assert_eq!(first.ask(Command::Generation).await?, 0);
    assert_eq!(second.ask(Command::Generation).await?, 0);
    assert!(first.ask(Command::Fail).await.is_err());
    tokio::time::timeout(Duration::from_secs(5), async {
        while builds.load(Ordering::SeqCst) < 2 || other_builds.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(first.ask(Command::Generation).await?, 1);
    assert_eq!(second.ask(Command::Generation).await?, 1);
    println!("Both roots recovered through their original references (generation 1).");
    system.shutdown_internal().await?;
    Ok(())
}
