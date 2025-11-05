//! Run with: cargo run --locked -p parrot --example typed_death_watch
//! A Thread typed watcher observes an Actix typed actor through a Ping view.
use parrot_api::{
    Message, ParrotTypedActor,
    context::ActorTerminated,
    typed::TypedReceive,
    types::{ActorResult, BoxedFuture},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::oneshot;

pub struct Ping;
impl Message for Ping {
    type Result = ();
}

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Ping, ActorTerminated))]
struct Watcher(Option<oneshot::Sender<ActorTerminated>>);
impl TypedReceive<Ping> for Watcher {
    fn receive_typed(&mut self, _: Ping) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl TypedReceive<ActorTerminated> for Watcher {
    fn receive_typed(&mut self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(event);
            }
            Ok(())
        })
    }
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Ping))]
struct Service;
impl TypedReceive<Ping> for Service {
    fn receive_typed(&mut self, _: Ping) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[actix::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
    let actix = Arc::new(parrot::actix::ActixActorSystem::new().await?);
    let (sender, received) = oneshot::channel();
    let watcher = thread
        .spawn_typed::<Watcher, Ping>(Watcher(Some(sender)), "/watcher")
        .await?;
    watcher.ask(Ping).await?;
    let target = actix
        .spawn_typed::<Service, Ping>(Service, "/service")
        .await?;
    target.ask(Ping).await?;
    let target = target.into_dyn();
    let identity = target.death_watch().expect("managed target").id();
    // The watch endpoint uses the declared event protocol, even though this
    // reference's normal dynamic bridge only accepts Ping.
    target.watch(watcher.into_dyn()).await?;
    target.stop().await?;
    let event = tokio::time::timeout(Duration::from_secs(2), received).await??;
    assert_eq!(event.generation, identity);
    println!(
        "{} terminated: {:?} ({})",
        event.path, event.reason, event.generation
    );
    thread.shutdown_internal().await?;
    actix.as_ref().clone().shutdown().await?;
    Ok(())
}
