use actix::Actor as _;
use parrot::actix::{ActixActor, ActixActorRef, ActixContext};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::{ActorRef, ActorRefExt},
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::time::Duration;
struct Echo;
impl ActixEngineExt for Echo {}
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}
#[derive(Debug, Clone)]
struct Request;
impl parrot_api::message::Message for Request {
    type Result = Request;
}
#[actix::test]
async fn raw_closed_reference_preserves_stopped_across_request_adapters() {
    let actor = ActixActorRef::new(ActixActor::new(Echo).start(), "/raw".into());
    actor.send(Box::new(Request)).await.unwrap();
    actor.stop().await.unwrap();
    assert!(!actor.is_alive().await);
    let cloned = actor.clone_boxed();
    let results = [
        actor.send(Box::new(Request)).await.map(|_| ()),
        actor
            .send_with_timeout(Box::new(Request), None)
            .await
            .map(|_| ()),
        actor
            .send_with_timeout(Box::new(Request), Some(Duration::from_millis(100)))
            .await
            .map(|_| ()),
        actor.ask(Request).await.map(|_| ()),
        actor.deliver(Box::new(Request)).await,
        cloned.send(Box::new(Request)).await.map(|_| ()),
    ];
    assert!(
        results
            .iter()
            .all(|result| matches!(result, Err(ActorError::Stopped))),
        "{results:?}"
    );
}

struct Hold {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
struct Counted {
    handled: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl ActixEngineExt for Counted {}
impl Actor for Counted {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let count = self
                .handled
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if message.is::<Hold>() {
                let hold = *message.downcast::<Hold>().unwrap();
                hold.entered.send(()).unwrap();
                let _ = hold.release.await;
            }
            Ok(Box::new(count) as BoxedMessage)
        })
    }
}
#[actix::test]
async fn raw_try_send_distinguishes_full_from_closed_and_preserves_rejected_payload() {
    let handled = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reference = ActixActorRef::new(
        ActixActor::new(Counted {
            handled: handled.clone(),
        })
        .start(),
        "/full".into(),
    );
    let (entered, observing) = tokio::sync::oneshot::channel();
    let (release, held) = tokio::sync::oneshot::channel();
    reference
        .deliver(Box::new(Hold {
            entered,
            release: held,
        }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), observing)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..1024 {
        reference.try_send(Box::new(Request)).unwrap();
    }
    match reference.try_send(Box::new(73u32)) {
        Err(actix::prelude::SendError::Full(payload)) => {
            assert_eq!(*payload.downcast::<u32>().unwrap(), 73)
        }
        result => panic!("full mailbox must reject with Full, got {result:?}"),
    }
    assert!(matches!(
        reference.deliver(Box::new(Request)).await,
        Err(ActorError::MessageHandlingError(_))
    ));
    release.send(()).unwrap();
    let count = reference.send(Box::new(Request)).await.unwrap();
    assert_eq!(
        *count.downcast::<usize>().unwrap(),
        1026,
        "rejected messages never execute"
    );
    reference.stop().await.unwrap();
    match reference.try_send(Box::new(79u32)) {
        Err(actix::prelude::SendError::Closed(payload)) => {
            assert_eq!(*payload.downcast::<u32>().unwrap(), 79)
        }
        result => panic!("stopped mailbox must reject with Closed, got {result:?}"),
    }
    assert_eq!(handled.load(std::sync::atomic::Ordering::SeqCst), 1026);
}
#[actix::test]
async fn raw_request_timeout_keeps_its_error_category_and_actor_can_continue() {
    let reference = ActixActorRef::new(
        ActixActor::new(Counted {
            handled: Default::default(),
        })
        .start(),
        "/timeout".into(),
    );
    let (entered, observing) = tokio::sync::oneshot::channel();
    let (release, held) = tokio::sync::oneshot::channel();
    let request = reference.send_with_timeout(
        Box::new(Hold {
            entered,
            release: held,
        }),
        Some(Duration::from_millis(30)),
    );
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    tokio::time::timeout(Duration::from_secs(2), observing)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(request.await, Err(ActorError::TimeoutDetail(_))));
    release.send(()).unwrap();
    let count = reference.send(Box::new(Request)).await.unwrap();
    assert_eq!(*count.downcast::<usize>().unwrap(), 2);
    reference.stop().await.unwrap();
}
