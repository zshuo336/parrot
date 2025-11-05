use parrot::system::ParrotActorSystem;
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, SpawnOptions},
    system::SystemState,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{any::Any, sync::Arc, time::Duration};
struct Gate {
    index: usize,
    entered: tokio::sync::mpsc::UnboundedSender<usize>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl Actor for Gate {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(m) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.entered.send(self.index).unwrap();
            if let Some(release) = self.release.take() {
                let _ = release.await;
            }
            Ok(())
        })
    }
}
async fn cancelled(mixed: bool) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("first".into(), thread.clone(), true)
        .await
        .unwrap();
    let mut runtimes: Vec<Arc<dyn AsyncEngineRuntime>> = vec![thread];
    if mixed {
        let actix = parrot::actix::ActixActorSystem::with_arbiter_count(1)
            .await
            .unwrap();
        facade
            .register_actix_system("second".into(), actix.clone(), false)
            .await
            .unwrap();
        runtimes.push(Arc::new(actix));
    } else {
        let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
        facade
            .register_thread_system("second".into(), thread.clone(), false)
            .await
            .unwrap();
        runtimes.push(thread);
    }
    let (entered, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let mut releases = Vec::new();
    for (index, runtime) in runtimes.iter().enumerate() {
        let (release, held) = tokio::sync::oneshot::channel();
        releases.push(release);
        let reference = runtime
            .spawn(
                Box::new(Gate {
                    index,
                    entered: entered.clone(),
                    release: Some(held),
                }),
                Box::new(()),
                SpawnOptions::at("/owned"),
            )
            .await
            .unwrap();
        reference.send(Box::new(())).await.unwrap();
    }
    let mut shutdown = Box::pin(facade.internal_shutdown());
    tokio::select! {
        result = shutdown.as_mut() => panic!("shutdown completed before release: {result:?}"),
        first = tokio::time::timeout(Duration::from_secs(5), observed.recv()) => { first.unwrap().unwrap(); }
    }
    drop(shutdown);
    for release in releases {
        let _ = release.send(());
    }
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if runtimes
                .iter()
                .all(|runtime| runtime.status().state == SystemState::Stopped)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    // Clean up the old implementation's stranded engine before reporting failure.
    for runtime in runtimes {
        runtime.shutdown().await.unwrap();
    }
    assert!(
        completed,
        "cancelling facade shutdown abandoned a registered engine"
    );
}
#[tokio::test]
async fn cancelled_facade_shutdown_still_retires_all_thread_engines() {
    cancelled(false).await;
}
#[actix::test]
async fn cancelled_facade_shutdown_still_retires_both_production_engines() {
    cancelled(true).await;
}

#[derive(Clone)]
struct Increment(std::cell::Cell<u64>);
impl parrot_api::message::Message for Increment {
    type Result = ();
}
struct Counter(u64);
impl Actor for Counter {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(Increment(value)) = message.downcast_ref::<Increment>() {
                self.0 += value.get();
            }
            Ok(Box::new(self.0) as BoxedMessage)
        })
    }
}
#[tokio::test]
async fn facade_broadcast_visits_each_engine_once_across_aliases() {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
    facade
        .register_thread_system("a".into(), thread.clone(), true)
        .await
        .unwrap();
    facade
        .register_thread_system("b".into(), thread.clone(), false)
        .await
        .unwrap();
    let counter = AsyncEngineRuntime::spawn(
        thread.as_ref(),
        Box::new(Counter(0)),
        Box::new(()),
        Default::default(),
    )
    .await
    .unwrap();
    facade
        .internal_broadcast(Increment(std::cell::Cell::new(1)))
        .await
        .unwrap();
    let observed = *counter
        .send(Box::new(()))
        .await
        .unwrap()
        .downcast::<u64>()
        .unwrap();
    facade.internal_shutdown().await.unwrap();
    assert_eq!(observed, 1, "an engine alias duplicated broadcast delivery");
}
