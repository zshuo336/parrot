//! Exercise ActorContext through real handlers, including cross-engine calls.
use actix::Actor as _;
use parrot::actix::{ActixActor, ActixActorRef, ActixContext};
use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{ActixEngineExt, Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::context::ActorContext;
use parrot_api::errors::ActorError;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

enum Command {
    SendSelf,
    Increment,
    Read,
    Ask(BoxedActorRef),
    Wait(Arc<Notify>),
}

async fn handle(
    value: &mut u64,
    msg: BoxedMessage,
    ctx: &mut dyn ActorContext,
) -> ActorResult<BoxedMessage> {
    if let Some(n) = msg.downcast_ref::<u64>() {
        return Ok(Box::new(n + 1));
    }
    match *msg
        .downcast::<Command>()
        .map_err(|_| ActorError::MessageHandlingError("unexpected envelope".into()))?
    {
        Command::SendSelf => {
            ctx.send(ctx.get_self_ref(), Box::new(Command::Increment))
                .await?
        }
        Command::Increment => *value += 1,
        Command::Read => {}
        Command::Ask(target) => return ctx.ask(target, Box::new(41_u64)).await,
        Command::Wait(entered) => {
            entered.notify_one();
            std::future::pending::<()>().await;
        }
    }
    Ok(Box::new(*value))
}

struct ThreadDriver(u64);
struct ActixDriver(u64);
impl ActixEngineExt for ActixDriver {}
macro_rules! driver {
    ($actor:ty, $context:ty) => {
        impl Actor for $actor {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(handle(&mut self.0, msg, ctx))
            }
        }
    };
}
driver!(ThreadDriver, ThreadContext<Self>);
driver!(ActixDriver, ActixContext<ActixActor<Self>>);

fn system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        default_ask_timeout: Duration::from_millis(50),
        shutdown_timeout: Duration::from_millis(50),
        ..Default::default()
    })
}

async fn send_self(actor: &dyn ActorRef) -> ActorResult<BoxedMessage> {
    actor
        .send_with_timeout(
            Box::new(Command::SendSelf),
            Some(Duration::from_millis(500)),
        )
        .await?;
    actor
        .send_with_timeout(Box::new(Command::Read), Some(Duration::from_millis(500)))
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_context_send_to_self_does_not_wait_for_handler() {
    let system = system();
    let actor = system
        .spawn_at(ThreadDriver(0), "/self", None, Default::default())
        .await
        .unwrap();
    let result = send_self(&actor).await;
    let _ = system.shutdown_internal().await;
    assert_eq!(
        *result
            .expect("send must only enqueue")
            .downcast::<u64>()
            .unwrap(),
        1
    );
}

#[test]
fn actix_context_send_to_self_does_not_wait_for_handler() {
    actix::System::new().block_on(async {
        let actor = ActixActorRef::new(ActixActor::new(ActixDriver(0)).start(), "/self".into());
        let result = send_self(&actor).await;
        assert_eq!(
            *result
                .expect("send must only enqueue")
                .downcast::<u64>()
                .unwrap(),
            1
        );
        actor.stop().await.unwrap();
    });
}

#[test]
fn context_ask_crosses_engines_without_private_envelopes() {
    actix::System::new().block_on(async {
        let system = system();
        let thread = system
            .spawn_at(ThreadDriver(0), "/thread", None, Default::default())
            .await
            .unwrap();
        let actix = ActixActorRef::new(ActixActor::new(ActixDriver(0)).start(), "/actix".into());
        let forward = thread
            .send_with_timeout(
                Box::new(Command::Ask(actix.clone_boxed())),
                Some(Duration::from_secs(1)),
            )
            .await;
        let reverse = actix
            .send_with_timeout(
                Box::new(Command::Ask(Box::new(thread))),
                Some(Duration::from_secs(1)),
            )
            .await;
        system.shutdown_internal().await.unwrap();
        actix.stop().await.unwrap();
        assert_eq!(
            *forward
                .expect("thread context to Actix")
                .downcast::<u64>()
                .unwrap(),
            42
        );
        assert_eq!(
            *reverse
                .expect("Actix context to thread")
                .downcast::<u64>()
                .unwrap(),
            42
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_context_ask_applies_default_timeout_to_the_exchange() {
    let system = system();
    let caller = system
        .spawn_at(ThreadDriver(0), "/caller", None, Default::default())
        .await
        .unwrap();
    let target = system
        .spawn_at(ThreadDriver(0), "/target", None, Default::default())
        .await
        .unwrap();
    let entered = Arc::new(Notify::new());
    target
        .deliver(Box::new(Command::Wait(entered.clone())))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        caller.send(Box::new(Command::Ask(Box::new(target)))),
    )
    .await;
    let _ = system.shutdown_internal().await;
    assert!(
        matches!(result, Ok(Err(ActorError::TimeoutDetail(_)))),
        "context timeout must expire: {result:?}"
    );
}

async fn full_mailbox_timeout(inline: bool) {
    let system = system();
    let actor = system
        .spawn_at(
            ThreadDriver(0),
            "/full",
            None,
            ThreadActorConfig {
                mailbox_capacity: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let entered = Arc::new(Notify::new());
    actor
        .deliver(Box::new(Command::Wait(entered.clone())))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    actor.deliver(Box::new(Command::Increment)).await.unwrap();
    let ask = async {
        if inline {
            actor.ask_inline(41_u64, Duration::from_millis(50)).await
        } else {
            actor
                .send_with_timeout(Box::new(41_u64), Some(Duration::from_millis(50)))
                .await
        }
    };
    let result = tokio::time::timeout(Duration::from_millis(500), ask).await;
    let _ = system.shutdown_internal().await;
    assert!(
        matches!(result, Ok(Err(ActorError::TimeoutDetail(_)))),
        "mailbox enqueue must share the ask deadline: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boxed_ask_timeout_includes_full_mailbox_wait() {
    full_mailbox_timeout(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_ask_timeout_includes_full_mailbox_wait() {
    full_mailbox_timeout(true).await;
}

#[test]
fn actix_ask_timeout_with_full_mailbox_preserves_timeout_error() {
    actix::System::new().block_on(async {
        let actor = ActixActorRef::new(ActixActor::new(ActixDriver(0)).start(), "/full".into());
        let entered = Arc::new(Notify::new());
        actor
            .deliver(Box::new(Command::Wait(entered.clone())))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        // Actix defaults to 16; do_send can accept beyond that limit.
        for _ in 0..32 {
            actor.deliver(Box::new(Command::Increment)).await.unwrap();
        }
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            actor.send_with_timeout(Box::new(41_u64), Some(Duration::from_millis(50))),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(ActorError::TimeoutDetail(_)))),
            "Actix must return the unified timeout error: {result:?}"
        );
    });
}
