//! Exercise both sides of Actix's response channel through real actor dispatch.

use actix::Actor as _;
use parrot::actix::message::{ActixMessageWrapper, ActixReply};
use parrot::actix::{ActixActor, ActixActorRef, ActixContext};
use parrot_api::actor::{ActixEngineExt, Actor, ActorState, EmptyConfig, EngineContextHandle};
use parrot_api::address::ActorRef;
use parrot_api::errors::ActorError;
use parrot_api::message::MessageEnvelope;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Clone, Copy)]
enum Command {
    Reply,
    Barrier,
    Timeout,
}

#[derive(Debug)]
struct ReplyPayload {
    drops: Arc<AtomicUsize>,
    panic_payloads: Arc<AtomicUsize>,
    hostile: bool,
}

struct RecursivePanic(Arc<AtomicUsize>);
impl Drop for RecursivePanic {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("Actix reply panic payload destructor");
    }
}
impl Drop for ReplyPayload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.hostile {
            std::panic::panic_any(RecursivePanic(self.panic_payloads.clone()));
        }
    }
}
impl std::fmt::Display for ReplyPayload {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        panic!("reply cleanup must not format a user error");
    }
}
impl std::error::Error for ReplyPayload {}

struct Business<const ASYNC: bool> {
    drops: Arc<AtomicUsize>,
    panic_payloads: Arc<AtomicUsize>,
    hostile: bool,
    entered: Arc<Notify>,
    release: Option<Arc<Notify>>,
}
impl<const ASYNC: bool> Business<ASYNC> {
    fn reply(&self, command: Command) -> ActorResult<BoxedMessage> {
        match command {
            Command::Reply => Ok(Box::new(ReplyPayload {
                drops: self.drops.clone(),
                panic_payloads: self.panic_payloads.clone(),
                hostile: self.hostile,
            })),
            Command::Barrier => Ok(Box::new(37_u64)),
            Command::Timeout => Err(ActorError::Timeout),
        }
    }
}
impl<const ASYNC: bool> Actor for Business<ASYNC> {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;

    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        ASYNC
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let command = *message.downcast::<Command>().unwrap();
            if matches!(command, Command::Reply) {
                self.entered.notify_one();
                if let Some(release) = &self.release {
                    release.notified().await;
                }
            }
            self.reply(command)
        })
    }
}
impl<const ASYNC: bool> ActixEngineExt for Business<ASYNC> {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
        _: EngineContextHandle<'_>,
    ) -> Option<ActorResult<BoxedMessage>> {
        assert!(!ASYNC, "asynchronous dispatch used the synchronous hook");
        Some(self.reply(*message.downcast::<Command>().unwrap()))
    }
}

fn wrapper(command: Command) -> ActixMessageWrapper {
    ActixMessageWrapper {
        envelope: MessageEnvelope::from_boxed(Box::new(command), None, Default::default()),
    }
}

async fn barrier(reference: &impl ActorRef) {
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        reference.send(Box::new(Command::Barrier)),
    )
    .await
    .expect("actor still processes requests")
    .expect("barrier reply");
    assert_eq!(*result.downcast::<u64>().unwrap(), 37);
}

async fn published<const ASYNC: bool>(native: bool, hostile: bool) {
    let drops = Arc::new(AtomicUsize::new(0));
    let panic_payloads = Arc::new(AtomicUsize::new(0));
    let address = ActixActor::new(Business::<ASYNC> {
        drops: drops.clone(),
        panic_payloads: panic_payloads.clone(),
        hostile,
        entered: Default::default(),
        release: None,
    })
    .start();
    let reference = ActixActorRef::new(address.clone(), "/reply-cancellation".into());
    let mut request: BoxedFuture<'_, ActorResult<BoxedMessage>> = if native {
        Box::pin(async {
            address
                .send(wrapper(Command::Reply))
                .await
                .unwrap()
                .into_inner()
                .unwrap()
        })
    } else {
        reference.send(Box::new(Command::Reply))
    };
    assert!(futures::poll!(&mut request).is_pending());
    // AtomicResponse's ctx.wait serializes the next handler after publication.
    barrier(&reference).await;
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let cancellation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(request)));
    if let Err(payload) = cancellation {
        // Preserve a normal assertion failure even under an unsafe regression.
        std::mem::forget(payload);
        panic!("Actix request cancellation leaked a destructor panic");
    }
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(panic_payloads.load(Ordering::SeqCst), usize::from(hostile));
    barrier(&reference).await;
    assert!(matches!(
        reference.send(Box::new(Command::Timeout)).await,
        Err(ActorError::Timeout)
    ));
    reference.stop().await.unwrap();
}

#[actix::test]
async fn native_sync_request_cancellation() {
    for hostile in [false, true] {
        published::<false>(true, hostile).await;
    }
}
#[actix::test]
async fn native_async_request_cancellation() {
    for hostile in [false, true] {
        published::<true>(true, hostile).await;
    }
}
#[actix::test]
async fn reference_sync_request_cancellation() {
    for hostile in [false, true] {
        published::<false>(false, hostile).await;
    }
}
#[actix::test]
async fn reference_async_request_cancellation() {
    for hostile in [false, true] {
        published::<true>(false, hostile).await;
    }
}

#[actix::test]
async fn cancelled_and_timed_out_receivers_reject_late_replies_safely() {
    for timeout in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let panic_payloads = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let address = ActixActor::new(Business::<true> {
            drops: drops.clone(),
            panic_payloads: panic_payloads.clone(),
            hostile: true,
            entered: entered.clone(),
            release: Some(release.clone()),
        })
        .start();
        let reference = ActixActorRef::new(address, "/late-reply".into());
        let mut request = reference.send_with_timeout(
            Box::new(Command::Reply),
            timeout.then_some(Duration::from_millis(20)),
        );
        assert!(futures::poll!(&mut request).is_pending());
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        if timeout {
            let error = tokio::time::timeout(Duration::from_secs(2), request)
                .await
                .unwrap();
            assert!(matches!(error, Err(ActorError::TimeoutDetail(_))));
        } else {
            drop(request);
        }
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        release.notify_one();
        barrier(&reference).await;
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(panic_payloads.load(Ordering::SeqCst), 1);
        reference.stop().await.unwrap();
    }
}

#[actix::test]
async fn fire_and_forget_retires_unobserved_replies() {
    let drops = Arc::new(AtomicUsize::new(0));
    let panic_payloads = Arc::new(AtomicUsize::new(0));
    let address = ActixActor::new(Business::<false> {
        drops: drops.clone(),
        panic_payloads: panic_payloads.clone(),
        hostile: true,
        entered: Default::default(),
        release: None,
    })
    .start();
    address.do_send(wrapper(Command::Reply));
    let reference = ActixActorRef::new(address, "/unobserved-reply".into());
    barrier(&reference).await;
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(panic_payloads.load(Ordering::SeqCst), 1);
    reference.stop().await.unwrap();
}

#[test]
fn error_reply_cleanup_is_protected_and_successful_receipt_transfers_ownership() {
    let drops = Arc::new(AtomicUsize::new(0));
    let panic_payloads = Arc::new(AtomicUsize::new(0));
    let payload = || ReplyPayload {
        drops: drops.clone(),
        panic_payloads: panic_payloads.clone(),
        hostile: true,
    };
    let reply = ActixReply::new(Some(Err(ActorError::Other(anyhow::Error::new(payload())))));
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(reply))).is_ok());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(panic_payloads.load(Ordering::SeqCst), 1);

    let reply = ActixReply::new(Some(Ok(Box::new(ReplyPayload {
        drops: drops.clone(),
        panic_payloads: panic_payloads.clone(),
        hostile: false,
    }))));
    let value = reply.into_inner().unwrap().unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(ActixReply::new(None).into_inner().is_none());
}
