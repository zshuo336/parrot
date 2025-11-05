use parrot::actix::{ActixActor, ActixActorSystem, ActixContext, system::ActixSystemSettings};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (_guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(3), |open| {
                !*open
            })
            .unwrap();
        assert!(!timeout.timed_out());
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Probe {
    gate: Arc<Gate>,
    armed: Arc<AtomicBool>,
    handled: Arc<AtomicUsize>,
    hooks: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActixEngineExt for Probe {}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.gate.block();
        }
        true
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.hooks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.handled.fetch_add(1, Ordering::SeqCst);
            Ok(msg)
        })
    }
}
#[actix::test]
async fn stop_between_dispatch_transfer_and_first_poll_restores_actor_before_hook() {
    let sys = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        ..Default::default()
    })
    .await
    .unwrap();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let armed = Arc::new(AtomicBool::new(false));
    let handled = Arc::new(AtomicUsize::new(0));
    let hooks = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let actor = sys
        .spawn_root_typed(
            Probe {
                gate: gate.clone(),
                armed: armed.clone(),
                handled: handled.clone(),
                hooks: hooks.clone(),
                drops: drops.clone(),
            },
            EmptyConfig,
        )
        .await
        .unwrap();
    actor.send(Box::new(())).await.unwrap();
    armed.store(true, Ordering::SeqCst);
    let request = actor.send(Box::new(()));
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
        .await
        .unwrap();
    let stopped = actor.stop();
    tokio::pin!(stopped);
    assert!(futures::poll!(&mut stopped).is_pending());
    gate.release();
    let result = stopped.await;
    let reply = request.await;
    sys.shutdown().await.unwrap();
    assert!(result.is_ok(), "stop lost lifecycle completion: {result:?}");
    assert!(reply.is_err());
    assert_eq!(handled.load(Ordering::SeqCst), 1);
    assert_eq!(hooks.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
