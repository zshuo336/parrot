//! Run: cargo run --locked -p parrot --example mirror
use parrot::{
    mirror::{MirrorCopier, MirrorPolicy},
    system::ParrotActorSystem,
    thread::{config::ThreadActorConfig, context::ThreadContext, system::ThreadActorSystem},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    system::ActorSystem,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
struct Counter {
    total: Arc<AtomicU64>,
    changed: Arc<tokio::sync::Notify>,
}
impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let amount = *msg.downcast::<u64>().map_err(|_| {
                parrot_api::errors::ActorError::MessageHandlingError("expected u64".into())
            })?;
            let total = self.total.fetch_add(amount, Ordering::SeqCst) + amount;
            self.changed.notify_one();
            Ok(Box::new(total) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let facade = ParrotActorSystem::new(Default::default()).await?;
    let engine = ThreadActorSystem::shared(Default::default());
    let shadow = Arc::new(AtomicU64::new(0));
    let changed = Arc::new(tokio::sync::Notify::new());
    engine
        .spawn_at(
            Counter {
                total: Arc::new(AtomicU64::new(0)),
                changed: Arc::new(tokio::sync::Notify::new()),
            },
            "/source",
            None,
            ThreadActorConfig::default(),
        )
        .await?;
    engine
        .spawn_at(
            Counter {
                total: shadow.clone(),
                changed: changed.clone(),
            },
            "/shadow",
            None,
            ThreadActorConfig::default(),
        )
        .await?;
    facade
        .register_thread_system("thread".into(), engine, true)
        .await?;
    facade.register_mirror_copier(MirrorCopier::cloned::<u64>(|_| 8))?;
    let mirror = facade.register_mirror_policy(MirrorPolicy::new("/source", "/shadow"))?;
    let outcome = async {
        let actor = facade
            .get_actor(&ActorPath::placeholder("/source"))
            .await
            .ok_or("source missing")?;
        let reply = actor.send(Box::new(3u64)).await?;
        let total = *reply.downcast::<u64>().map_err(|_| "wrong reply type")?;
        // Admission is not execution: observe the actual shadow state.
        tokio::time::timeout(Duration::from_secs(2), async {
            while shadow.load(Ordering::SeqCst) != 3 {
                changed.notified().await;
            }
        })
        .await?;
        assert_eq!(total, 3);
        println!("primary={total}, shadow={}", shadow.load(Ordering::SeqCst));
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    facade.internal_shutdown().await?;
    assert!(mirror.is_finished());
    outcome
}
