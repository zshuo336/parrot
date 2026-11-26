//! Clone a custom payload before erasure, then deliver it through a real periodic timer.
use parrot::thread::{ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::ActorContext,
    errors::ActorError,
    message::CloneableMessage,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::time::Duration;
use tokio::sync::oneshot;

#[derive(Clone, Debug, PartialEq)]
struct Tick(String);

struct Collector {
    ticks: Vec<Tick>,
    done: Option<oneshot::Sender<Vec<Tick>>>,
}

impl Actor for Collector {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn state(&self) -> ActorState {
        ActorState::Running
    }

    fn use_async_handler(&self) -> bool {
        true
    }

    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            ctx.schedule_periodic(
                ctx.get_self_ref(),
                CloneableMessage::from_cloneable(Tick("heartbeat".into())),
                Duration::from_millis(10),
                Duration::from_millis(10),
            )
            .await
        })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let tick = msg.downcast::<Tick>().map_err(|_| {
                ActorError::MessageHandlingError("periodic payload must be a Tick".into())
            })?;
            if self.done.is_some() {
                self.ticks.push(*tick);
                if self.ticks.len() == 3
                    && let Some(done) = self.done.take()
                {
                    let _ = done.send(std::mem::take(&mut self.ticks));
                }
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Common erased types can still be recognized; an arbitrary erased Tick cannot.
    let text: BoxedMessage = Box::new(String::from("hello"));
    let copy = CloneableMessage::try_from_boxed(&text)
        .unwrap()
        .into_boxed();
    assert_eq!(*copy.downcast::<String>().unwrap(), "hello");
    let erased: BoxedMessage = Box::new(Tick("custom".into()));
    assert!(CloneableMessage::try_from_boxed(&erased).is_none());

    let system = ThreadActorSystem::shared(Default::default());
    let (done, received) = oneshot::channel();
    let work = async {
        let actor = system
            .spawn_root_typed_thread(
                Collector {
                    ticks: Vec::new(),
                    done: Some(done),
                },
                EmptyConfig,
            )
            .await?;
        let ticks = tokio::time::timeout(Duration::from_secs(3), received).await??;
        assert_eq!(ticks, vec![Tick("heartbeat".into()); 3]);
        actor.stop().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    system.shutdown_internal().await?;
    work?;
    assert_eq!(system.actor_count(), 0);
    println!("Three custom timer messages received; collector retired.");
    Ok(())
}
