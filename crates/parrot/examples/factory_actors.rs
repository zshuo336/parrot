//! One logical Thread reference follows three fresh actor generations.
//! Intentional handler panics are caught by the runtime.
use parrot::thread::{SupervisorStrategy, ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Crash;
struct Counter {
    generation: usize,
    drops: Arc<AtomicUsize>,
}
impl Drop for Counter {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            assert!(!msg.is::<Crash>(), "intentional generation failure");
            Ok(Box::new(self.generation) as BoxedMessage)
        })
    }
}
struct Captures(Arc<AtomicUsize>);
impl Drop for Captures {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[tokio::main]
async fn main() {
    let system = ThreadActorSystem::shared(Default::default());
    let creations = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let factory_drops = Arc::new(AtomicUsize::new(0));
    let captures = Captures(factory_drops.clone());
    let count = creations.clone();
    let disposed = drops.clone();
    let actor = system
        .spawn_supervised(
            move || {
                let _ = &captures;
                Counter {
                    generation: count.fetch_add(1, Ordering::SeqCst),
                    drops: disposed.clone(),
                }
            },
            "/counter",
            SupervisorStrategy::Restart {
                max_retries: 2,
                within: Duration::from_secs(30),
            },
        )
        .await
        .unwrap();
    for expected in 0..3 {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(reply) = actor
                    .send_with_timeout(Box::new(()), Some(Duration::from_millis(100)))
                    .await
                    && *reply.downcast::<usize>().unwrap() == expected
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let _ = actor.send(Box::new(Crash)).await;
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while system.get_actor_ref("/counter").is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(creations.load(Ordering::SeqCst), 3);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    assert_eq!(factory_drops.load(Ordering::SeqCst), 1);
    system.shutdown_internal().await.unwrap();
    println!(
        "original reference followed three generations; all instances and factory captures disposed"
    );
}
