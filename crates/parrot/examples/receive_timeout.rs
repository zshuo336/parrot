//! Run with: cargo run --locked -p parrot --example receive_timeout
//! Both dynamic engines emit idle events through ordinary actor dispatch.
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorContext, LifecycleEvent},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::time::Duration;
use tokio::sync::oneshot;

struct ThreadIdle(Option<oneshot::Sender<()>>);
struct ActixIdle(Option<oneshot::Sender<()>>);
impl ActixEngineExt for ActixIdle {}

macro_rules! idle_actor {
    ($actor:ident, $context:ty) => {
        impl Actor for $actor {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn init<'a>(
                &'a mut self,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    ctx.set_receive_timeout(Some(Duration::from_millis(100)));
                    Ok(())
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    if matches!(
                        message.downcast_ref::<LifecycleEvent>(),
                        Some(LifecycleEvent::ReceiveTimeout)
                    ) {
                        // Disable from inside the handler after the first idle period.
                        ctx.set_receive_timeout(None);
                        if let Some(sender) = self.0.take() {
                            let _ = sender.send(());
                        }
                    }
                    Ok(Box::new(()) as BoxedMessage)
                })
            }
        }
    };
}
idle_actor!(ThreadIdle, parrot::thread::ThreadContext<Self>);
idle_actor!(
    ActixIdle,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);

#[actix::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let thread = parrot::thread::ThreadActorSystem::shared(Default::default());
    let actix = parrot::actix::ActixActorSystem::new().await?;
    let (thread_send, thread_idle) = oneshot::channel();
    let (actix_send, actix_idle) = oneshot::channel();
    thread
        .spawn_at(
            ThreadIdle(Some(thread_send)),
            "/idle",
            None,
            Default::default(),
        )
        .await?;
    actix
        .spawn_root_typed(ActixIdle(Some(actix_send)), EmptyConfig)
        .await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        thread_idle.await?;
        actix_idle.await?;
        Ok::<_, oneshot::error::RecvError>(())
    })
    .await??;
    println!("Thread and Actix both received their idle event");
    thread.shutdown_internal().await?;
    actix.shutdown().await?;
    Ok(())
}
