//! Public send cancellation must own input before returning its lazy future.
use futures::FutureExt;
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot::thread::{BackpressureStrategy, ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    message::MessageOptions,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

struct Input(Arc<AtomicUsize>);
impl Drop for Input {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("cancelled message destructor failed");
    }
}
struct ThreadBusiness(Arc<AtomicUsize>);
struct ActixBusiness(Arc<AtomicUsize>);
impl Actor for ThreadBusiness {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(message) })
    }
}
impl Actor for ActixBusiness {
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
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(message) })
    }
}
impl ActixEngineExt for ActixBusiness {}
fn cancel<T>(future: T) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_err()
}
async fn retired(drops: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while drops.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled inputs must be retired");
    assert_eq!(drops.load(Ordering::SeqCst), expected);
}
async fn healthy(reference: &dyn ActorRef, handled: &AtomicUsize) {
    assert_eq!(
        handled.load(Ordering::SeqCst),
        0,
        "unpolled sends must stay lazy"
    );
    let reply = tokio::time::timeout(Duration::from_secs(5), reference.send(Box::new(41_u64)))
        .await
        .expect("actor progress")
        .unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 41);
    assert_eq!(handled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn thread_public_send_inputs_are_owned_before_first_poll() {
    let system = ThreadActorSystem::shared(Default::default());
    let handled = Arc::new(AtomicUsize::new(0));
    let reference = system
        .spawn_at(
            ThreadBusiness(handled.clone()),
            "/input",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut escaped = Vec::new();
    for operation in 0..13 {
        let input: BoxedMessage = Box::new(Input(drops.clone()));
        let panic = match operation {
            0 => cancel(reference.send(input)),
            1 => cancel(reference.send_with_timeout(input, None)),
            2 => cancel(reference.send_with_timeout(input, Some(Duration::from_secs(1)))),
            3 => cancel(reference.send_with_options(input, MessageOptions::default())),
            4 => cancel(reference.deliver(input)),
            5 => cancel(reference.deliver_with_options(input, MessageOptions::default())),
            6 => cancel(reference.send_msg(input)),
            7 => cancel(reference.send_with_strategy(input, BackpressureStrategy::Block)),
            8 => cancel(reference.send_with_priority(input, BackpressureStrategy::Block, false)),
            9 => cancel(reference.ask_with_strategy_and_timeout(
                input,
                BackpressureStrategy::Block,
                Duration::from_secs(1),
            )),
            10 => cancel(reference.ask(input)),
            11 => cancel(reference.ask_unbounded(input, BackpressureStrategy::Block)),
            _ => cancel(reference.ask_with_timeout(input, Duration::from_secs(1))),
        };
        if panic {
            escaped.push(operation);
        }
    }
    if cancel(reference.ask_inline(Input(drops.clone()), Duration::from_secs(1))) {
        escaped.push(13);
    }
    retired(&drops, 14).await;
    healthy(&reference, &handled).await;
    system.shutdown_internal().await.unwrap();
    assert!(
        escaped.is_empty(),
        "Thread input destructor escaped unpolled cancellation: {escaped:?}"
    );
}

impl parrot::thread::envelope::InlineMsg for Input {
    fn into_inline(self) -> parrot::thread::envelope::InlinePayload {
        parrot::thread::envelope::InlinePayload::Unit
    }
}

#[actix::test]
async fn actix_factory_send_inputs_are_owned_before_first_poll() {
    let system = ActixActorSystem::new().await.unwrap();
    let handled = Arc::new(AtomicUsize::new(0));
    let actor_count = handled.clone();
    let reference = system
        .spawn_root_factory(move || ActixBusiness(actor_count.clone()))
        .await
        .unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut escaped = Vec::new();
    for operation in 0..6 {
        let input: BoxedMessage = Box::new(Input(drops.clone()));
        let panic = match operation {
            0 => cancel(reference.send(input)),
            1 => cancel(reference.send_with_timeout(input, None)),
            2 => cancel(reference.send_with_timeout(input, Some(Duration::from_secs(1)))),
            3 => cancel(reference.send_with_options(input, MessageOptions::default())),
            4 => cancel(reference.deliver(input)),
            _ => cancel(reference.deliver_with_options(input, MessageOptions::default())),
        };
        if panic {
            escaped.push(operation);
        }
    }
    retired(&drops, 6).await;
    healthy(reference.as_ref(), &handled).await;
    system.shutdown().await.unwrap();
    assert!(
        escaped.is_empty(),
        "Actix factory input destructor escaped unpolled cancellation: {escaped:?}"
    );
}

async fn stopped(reference: &dyn ActorRef) {
    let drops = Arc::new(AtomicUsize::new(0));
    for operation in 0..6 {
        let input: BoxedMessage = Box::new(Input(drops.clone()));
        let future: BoxedFuture<'_, ActorResult<()>> = match operation {
            0 => reference
                .send(input)
                .map(|result| result.map(|_| ()))
                .boxed(),
            1 => reference
                .send_with_timeout(input, None)
                .map(|result| result.map(|_| ()))
                .boxed(),
            2 => reference
                .send_with_timeout(input, Some(Duration::from_secs(1)))
                .map(|result| result.map(|_| ()))
                .boxed(),
            3 => reference
                .send_with_options(input, MessageOptions::default())
                .map(|result| result.map(|_| ()))
                .boxed(),
            4 => reference.deliver(input),
            _ => reference.deliver_with_options(input, MessageOptions::default()),
        };
        let result = std::panic::AssertUnwindSafe(future).catch_unwind().await;
        assert!(
            matches!(result, Ok(Err(parrot_api::errors::ActorError::Stopped))),
            "stopped rejection escaped or changed its category at operation {operation}: {result:?}"
        );
    }
    retired(&drops, 6).await;
}

async fn invalid_options(reference: &dyn ActorRef) {
    let drops = Arc::new(AtomicUsize::new(0));
    for ask in [false, true] {
        let options = MessageOptions {
            retry_policy: Some(parrot_api::message::RetryPolicy {
                max_attempts: 1,
                retry_interval: Duration::from_millis(1),
                backoff_strategy: parrot_api::message::BackoffStrategy::Fixed,
            }),
            ..Default::default()
        };
        let input: BoxedMessage = Box::new(Input(drops.clone()));
        let future: BoxedFuture<'_, ActorResult<()>> = if ask {
            reference
                .send_with_options(input, options)
                .map(|result| result.map(|_| ()))
                .boxed()
        } else {
            reference.deliver_with_options(input, options)
        };
        let result = std::panic::AssertUnwindSafe(future).catch_unwind().await;
        assert!(
            matches!(
                result,
                Ok(Err(parrot_api::errors::ActorError::MessageHandlingError(_)))
            ),
            "invalid options escaped or changed their category: {result:?}"
        );
    }
    retired(&drops, 2).await;
}

#[tokio::test]
async fn stopped_thread_send_inputs_are_retired() {
    let system = ThreadActorSystem::shared(Default::default());
    let reference = system
        .spawn_at(
            ThreadBusiness(Arc::default()),
            "/closed",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    system.shutdown_internal().await.unwrap();
    stopped(&reference).await;
}

#[actix::test]
async fn stopped_actix_factory_send_inputs_are_retired() {
    let system = ActixActorSystem::new().await.unwrap();
    let reference = system
        .spawn_root_factory(|| ActixBusiness(Arc::default()))
        .await
        .unwrap();
    system.shutdown().await.unwrap();
    stopped(reference.as_ref()).await;
}

#[tokio::test]
async fn thread_invalid_options_retire_inputs_before_business_dispatch() {
    let system = ThreadActorSystem::shared(Default::default());
    let handled = Arc::new(AtomicUsize::new(0));
    let reference = system
        .spawn_at(
            ThreadBusiness(handled.clone()),
            "/options",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    invalid_options(&reference).await;
    healthy(&reference, &handled).await;
    system.shutdown_internal().await.unwrap();
}

#[actix::test]
async fn actix_factory_invalid_options_retire_inputs_before_business_dispatch() {
    let system = ActixActorSystem::new().await.unwrap();
    let handled = Arc::new(AtomicUsize::new(0));
    let count = handled.clone();
    let reference = system
        .spawn_root_factory(move || ActixBusiness(count.clone()))
        .await
        .unwrap();
    invalid_options(reference.as_ref()).await;
    healthy(reference.as_ref(), &handled).await;
    system.shutdown().await.unwrap();
}
