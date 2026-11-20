//! Public business counterexample: pending initialization must bound root creation.
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext, ActixSystemSettings};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorConfig, ActorState},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::Duration,
};
use tokio::sync::Semaphore;

#[derive(Default)]
struct Counts {
    actors: AtomicUsize,
    configs: AtomicUsize,
    initialized: AtomicUsize,
}
struct Config(Arc<Counts>);
impl ActorConfig for Config {}
impl Drop for Config {
    fn drop(&mut self) {
        self.0.configs.fetch_add(1, Ordering::SeqCst);
    }
}
struct Probe {
    counts: Arc<Counts>,
    init: Arc<Semaphore>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActixEngineExt for Probe {}
impl Actor for Probe {
    type Config = Config;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.initialized.fetch_add(1, Ordering::SeqCst);
            self.init.acquire().await.unwrap().forget();
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}

#[actix::test]
async fn production_root_limit_bounds_initializing_instances_before_publication() {
    let system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 2,
        shutdown_timeout: Duration::from_secs(10),
        ..Default::default()
    })
    .await
    .unwrap();
    let counts = Arc::new(Counts::default());
    let init = Arc::new(Semaphore::new(0));
    let mut references = Vec::new();
    for _ in 0..1024 {
        references.push(
            system
                .spawn_root_typed(
                    Probe {
                        counts: counts.clone(),
                        init: init.clone(),
                    },
                    Config(counts.clone()),
                )
                .await
                .unwrap(),
        );
    }
    let mut pending = Box::pin(system.spawn_root_typed(
        Probe {
            counts: counts.clone(),
            init: init.clone(),
        },
        Config(counts.clone()),
    ));
    let first_poll = futures::poll!(&mut pending);
    let bounded = first_poll.is_pending();
    let registered = system.status().active_actors;
    // Release and retire everything before asserting, also on the old runtime
    // where this first poll has already published the overloaded root.
    init.add_permits(1025);
    let last = match first_poll {
        Poll::Ready(result) => result.unwrap(),
        Poll::Pending => pending.await.unwrap(),
    };
    last.send(Box::new(())).await.unwrap();
    references.push(last);
    for actor in references {
        actor.stop().await.unwrap();
    }
    system.clone().shutdown().await.unwrap();
    assert!(
        bounded,
        "1025th root bypassed the production initial-creation limit"
    );
    assert_eq!(
        registered, 1024,
        "overloaded root published before admission"
    );
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1025);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1025);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 1025);
}
