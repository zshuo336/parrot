//! Exercise input ownership through public references and native Actix requests.
use actix::Actor as _;
use parrot::actix::message::ActixMessageWrapper;
use parrot::actix::{ActixActor, ActixActorRef, ActixContext};
use parrot_api::actor::{ActixEngineExt, Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::errors::ActorError;
use parrot_api::message::{
    BackoffStrategy, MessageEnvelope, MessageOptions, MessagePriority, RetryPolicy,
};
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Input(Arc<AtomicUsize>);
impl Drop for Input {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("input destructor failed");
    }
}
#[derive(Debug)]
struct Sender(Arc<AtomicUsize>);
impl Drop for Sender {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("sender destructor failed");
    }
}
impl ActorRef for Sender {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("sender must not be invoked")
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<std::time::Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("sender must not be invoked")
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        panic!("sender must not be invoked")
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        panic!("sender must not be invoked")
    }
    fn path(&self) -> String {
        panic!("sender must not be formatted")
    }
    fn clone_boxed(&self) -> parrot_api::types::BoxedActorRef {
        panic!("sender must not be cloned")
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Business;
struct Gate {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl Actor for Business {
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
            match message.downcast::<Gate>() {
                Ok(gate) => {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                    Ok(Box::new(()) as BoxedMessage)
                }
                Err(message) => Ok(message),
            }
        })
    }
}
impl ActixEngineExt for Business {}

fn input(count: &Arc<AtomicUsize>) -> BoxedMessage {
    Box::new(Input(count.clone()))
}
fn cancel<T>(value: T) {
    let escaped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value)));
    assert!(escaped.is_ok(), "input destructor escaped cancellation");
}
async fn healthy(reference: &impl ActorRef) {
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reference.send(Box::new(41_u64)),
    )
    .await
    .expect("actor progress")
    .unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 41);
}

#[actix::test]
async fn unpolled_reference_inputs_are_owned_at_call_time() {
    let reference = ActixActorRef::new(ActixActor::new(Business).start(), "/input".into());
    for operation in 0..5 {
        let count = Arc::new(AtomicUsize::new(0));
        match operation {
            0 => cancel(reference.deliver(input(&count))),
            1 => cancel(reference.deliver_with_options(input(&count), MessageOptions::default())),
            2 => cancel(reference.send(input(&count))),
            3 => cancel(reference.send_with_options(input(&count), MessageOptions::default())),
            _ => cancel(reference.send_with_timeout(input(&count), None)),
        }
        assert_eq!(count.load(Ordering::SeqCst), 1, "operation {operation}");
        healthy(&reference).await;
    }
    reference.stop().await.unwrap();
}

#[actix::test]
async fn invalid_options_retire_inputs_without_entering_business_handler() {
    use futures::FutureExt;
    let reference = ActixActorRef::new(ActixActor::new(Business).start(), "/input".into());
    for ask in [false, true] {
        for options in [
            MessageOptions {
                priority: MessagePriority::HIGH,
                ..Default::default()
            },
            MessageOptions {
                retry_policy: Some(RetryPolicy {
                    max_attempts: 1,
                    retry_interval: std::time::Duration::from_millis(1),
                    backoff_strategy: BackoffStrategy::Fixed,
                }),
                ..Default::default()
            },
        ] {
            let count = Arc::new(AtomicUsize::new(0));
            let result = if ask {
                std::panic::AssertUnwindSafe(reference.send_with_options(input(&count), options))
                    .catch_unwind()
                    .await
                    .map(|r| r.map(|_| ()))
            } else {
                std::panic::AssertUnwindSafe(reference.deliver_with_options(input(&count), options))
                    .catch_unwind()
                    .await
            };
            assert!(matches!(
                result,
                Ok(Err(ActorError::MessageHandlingError(_)))
            ));
            assert_eq!(count.load(Ordering::SeqCst), 1);
            healthy(&reference).await;
        }
    }
    reference.stop().await.unwrap();
}

#[actix::test]
async fn native_unpolled_request_protects_input() {
    let address = ActixActor::new(Business).start();
    let count = Arc::new(AtomicUsize::new(0));
    cancel(address.send(ActixMessageWrapper {
        envelope: MessageEnvelope::from_boxed(input(&count), None, Default::default()),
    }));
    // Actix can enqueue before the Request is polled. A later barrier observes
    // cancellation and disposal in mailbox order.
    let reference = ActixActorRef::new(address, "/native-input".into());
    healthy(&reference).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    reference.stop().await.unwrap();
}

#[actix::test]
async fn closed_reference_retires_rejected_delivery() {
    use futures::FutureExt;
    let reference = ActixActorRef::new(ActixActor::new(Business).start(), "/closed-input".into());
    reference.stop().await.unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let result = std::panic::AssertUnwindSafe(reference.deliver(input(&count)))
        .catch_unwind()
        .await;
    assert!(matches!(result, Ok(Err(ActorError::Stopped))));
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn try_send_returns_original_input_to_caller() {
    let reference = ActixActorRef::new(ActixActor::new(Business).start(), "/return-input".into());
    reference.stop().await.unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let message = input(&count);
    let pointer = (&*message as *const dyn std::any::Any).cast::<()>();
    let returned = match reference.try_send(message) {
        Err(actix::prelude::SendError::Closed(message)) => message,
        _ => panic!("closed mailbox must return the input"),
    };
    assert_eq!(
        (&*returned as *const dyn std::any::Any).cast::<()>(),
        pointer
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(returned))).is_err());
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn full_mailbox_and_queued_cancellation_preserve_actor_progress() {
    use futures::FutureExt;
    let address = ActixActor::new(Business).start();
    let reference = ActixActorRef::new(address.clone(), "/full-input".into());
    healthy(&reference).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut blocked = reference.send(Box::new(Gate {
        entered: entered.clone(),
        release: release.clone(),
    }));
    assert!(futures::poll!(&mut blocked).is_pending());
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let queued = Arc::new(AtomicUsize::new(0));
    let request = address.send(ActixMessageWrapper {
        envelope: MessageEnvelope::from_boxed(input(&queued), None, Default::default()),
    });
    cancel(request);
    assert_eq!(
        queued.load(Ordering::SeqCst),
        0,
        "queued input disposed before dispatch"
    );
    let mut filled = 0;
    loop {
        match reference.try_send(Box::new(0_u64)) {
            Ok(()) => filled += 1,
            Err(actix::prelude::SendError::Full(_)) => break,
            Err(_) => panic!("healthy mailbox closed"),
        }
        assert!(filled < 1024, "mailbox admission was not bounded");
    }
    let waiting = Arc::new(AtomicUsize::new(0));
    // Use the already parked sender. Actix clones that sender inside a full
    // Request; polling the clone may admit one more message despite capacity.
    let request = reference.get_addr().send(ActixMessageWrapper {
        envelope: MessageEnvelope::from_boxed(input(&waiting), None, Default::default()),
    });
    assert_eq!(waiting.load(Ordering::SeqCst), 0);
    cancel(request);
    assert_eq!(
        waiting.load(Ordering::SeqCst),
        1,
        "capacity waiter retained cancelled input"
    );
    let rejected = Arc::new(AtomicUsize::new(0));
    let result = std::panic::AssertUnwindSafe(reference.deliver(input(&rejected)))
        .catch_unwind()
        .await;
    // Release the gate before assertions so a regression cannot strand cleanup.
    release.notify_one();
    blocked.await.unwrap();
    healthy(&reference).await;
    assert!(matches!(
        result,
        Ok(Err(ActorError::MessageHandlingError(_)))
    ));
    assert_eq!(rejected.load(Ordering::SeqCst), 1);
    assert_eq!(queued.load(Ordering::SeqCst), 1);
    reference.stop().await.unwrap();
}

#[test]
fn envelope_disposes_payload_and_sender_independently() {
    let payload = Arc::new(AtomicUsize::new(0));
    let sender = Arc::new(AtomicUsize::new(0));
    cancel(ActixMessageWrapper {
        envelope: MessageEnvelope::from_boxed(
            input(&payload),
            Some(Box::new(Sender(sender.clone()))),
            Default::default(),
        ),
    });
    assert_eq!(payload.load(Ordering::SeqCst), 1);
    assert_eq!(sender.load(Ordering::SeqCst), 1);
}

#[test]
fn into_envelope_transfers_original_values_without_disposal() {
    let payload = Arc::new(AtomicUsize::new(0));
    let sender = Arc::new(AtomicUsize::new(0));
    let envelope = MessageEnvelope::from_boxed(
        input(&payload),
        Some(Box::new(Sender(sender.clone()))),
        Default::default(),
    );
    let id = envelope.id;
    let transferred = ActixMessageWrapper { envelope }.into_envelope();
    assert_eq!(transferred.id, id);
    assert!(transferred.payload.is::<Input>());
    assert_eq!(payload.load(Ordering::SeqCst), 0);
    assert_eq!(sender.load(Ordering::SeqCst), 0);
    // The caller owns cleanup after explicit extraction, separately per value.
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(transferred.payload)))
            .is_err()
    );
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(transferred.sender)))
            .is_err()
    );
    assert_eq!(payload.load(Ordering::SeqCst), 1);
    assert_eq!(sender.load(Ordering::SeqCst), 1);
}
