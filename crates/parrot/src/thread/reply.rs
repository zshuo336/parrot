use async_trait::async_trait;
use parrot_api::{
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::oneshot;

/// A type-erased, sendable channel for replying to an `ask` request.
#[async_trait]
pub trait ReplyChannel: Send + Sync + Debug {
    /// Send the reply message. Consumes the channel.
    /// The result indicates delivery, not business success: an error in `msg`
    /// is still delivered successfully while the receiver remains open.
    async fn send_reply(self: Box<Self>, msg: ActorResult<BoxedMessage>) -> ActorResult<()>;
}

/// Receiver-side ownership guard for ask replies.
///
/// A raw oneshot receiver drops an already-published user value when the
/// receiver future is cancelled. This wrapper first takes that value during
/// `Drop` and retires it through the framework's panic-isolating boundary.
#[derive(Debug)]
pub struct ReplyReceiver {
    receiver: Option<oneshot::Receiver<ActorResult<BoxedMessage>>>,
}

impl ReplyReceiver {
    /// Wrap a Tokio receiver while retaining framework-owned cleanup on drop.
    ///
    /// The receiver is intentionally not convertible back to a raw
    /// `oneshot::Receiver`: doing so would reintroduce the cancellation path
    /// that can drop a published user value outside Parrot's cleanup boundary.
    pub fn new(receiver: oneshot::Receiver<ActorResult<BoxedMessage>>) -> Self {
        Self {
            receiver: Some(receiver),
        }
    }

    /// Try to take a reply without waiting while retaining cancellation-safe
    /// ownership for the receiver's eventual drop.
    pub fn try_recv(&mut self) -> Result<ActorResult<BoxedMessage>, oneshot::error::TryRecvError> {
        match self.receiver.as_mut() {
            Some(receiver) => receiver.try_recv(),
            None => Err(oneshot::error::TryRecvError::Closed),
        }
    }

    /// Close the receiver so a later sender observes a failed delivery.
    pub fn close(&mut self) {
        if let Some(receiver) = self.receiver.as_mut() {
            receiver.close();
        }
    }

    /// Whether a reply has already been consumed or closure was observed.
    pub fn is_terminated(&self) -> bool {
        self.receiver
            .as_ref()
            .is_none_or(oneshot::Receiver::is_terminated)
    }

    /// Whether the channel currently contains no reply.
    pub fn is_empty(&self) -> bool {
        self.receiver
            .as_ref()
            .is_none_or(oneshot::Receiver::is_empty)
    }

    /// Block the current thread until a reply arrives.
    ///
    /// This mirrors Tokio's consuming `blocking_recv` operation. Consuming
    /// the wrapper is required because a successfully received value belongs
    /// to the caller and must not be retired again by `Drop`.
    pub fn blocking_recv(mut self) -> Result<ActorResult<BoxedMessage>, oneshot::error::RecvError> {
        self.receiver
            .as_mut()
            .expect("reply receiver already completed");
        // Check Tokio's blocking contract before moving any user-owned value.
        // A futures executor may already be entered, so do not nest block_on.
        let (sender, receiver) = oneshot::channel();
        let _ = sender.send(());
        receiver.blocking_recv().expect("blocking receiver probe");
        let waker =
            std::task::Waker::from(std::sync::Arc::new(BlockingWake(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(result) = Pin::new(&mut self).poll(&mut context) {
                return result;
            }
            // unpark retains a token when notification precedes park; spurious
            // wakes are harmless because the receiver is polled again.
            std::thread::park();
        }
    }
}

struct BlockingWake(std::thread::Thread);
impl std::task::Wake for BlockingWake {
    fn wake(self: std::sync::Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &std::sync::Arc<Self>) {
        self.0.unpark();
    }
}

impl Future for ReplyReceiver {
    type Output = Result<ActorResult<BoxedMessage>, oneshot::error::RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let receiver = self
            .receiver
            .as_mut()
            .expect("reply receiver polled after completion");
        match Pin::new(receiver).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.receiver.take();
                Poll::Ready(result)
            }
        }
    }
}

impl Drop for ReplyReceiver {
    fn drop(&mut self) {
        let Some(mut receiver) = self.receiver.take() else {
            return;
        };
        // Close before inspecting the slot: a concurrent sender must either
        // have published before close or retain its rejected value.
        let close = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| receiver.close()))
            .map_err(|payload| ActorError::Panic(crate::owned_payload::panic_message(payload)));
        let receive =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| receiver.try_recv().ok()))
                .map_err(|payload| ActorError::Panic(crate::owned_payload::panic_message(payload)));
        let result_cleanup = match receive {
            Ok(result) => crate::owned_payload::PublicationInput::new(result).retire(),
            Err(error) => Err(error),
        };
        let receiver_cleanup = crate::owned_payload::PublicationInput::new(receiver).retire();
        for error in [close, result_cleanup, receiver_cleanup]
            .into_iter()
            .filter_map(Result::err)
        {
            report_cleanup(&error);
        }
    }
}

/// Implementation of ReplyChannel using a Tokio oneshot channel.
#[derive(Debug)]
pub struct ThreadReplyChannel(pub oneshot::Sender<ActorResult<BoxedMessage>>);

// Match async_trait's existing boxed signature so ownership is protected before
// returning the lazy future, including cancellation without a first poll.
impl ReplyChannel for ThreadReplyChannel {
    fn send_reply<'async_trait>(
        self: Box<Self>,
        msg: ActorResult<BoxedMessage>,
    ) -> BoxedFuture<'async_trait, ActorResult<()>>
    where
        Self: 'async_trait,
    {
        let reply = PendingReply::new(Some(self.0), msg);
        Box::pin(async move {
            let (delivered, cleanup) = reply.send();
            if delivered {
                cleanup
            } else {
                let message = match cleanup {
                    Ok(()) => "Failed to send reply".into(),
                    Err(error) => format!(
                        "Failed to send reply; {}",
                        crate::error_receipt::actor_summary(&error)
                    ),
                };
                Err(ActorError::ReplyChannelError(message))
            }
        })
    }
}

pub(super) struct PendingReply {
    sender: ReplySender,
    result: ReplyValue<ActorResult<BoxedMessage>>,
}
impl PendingReply {
    pub(super) fn new(
        sender: Option<oneshot::Sender<ActorResult<BoxedMessage>>>,
        result: ActorResult<BoxedMessage>,
    ) -> Self {
        Self {
            sender: ReplySender::new(sender),
            result: ReplyValue::new(result),
        }
    }
    pub(super) fn send(mut self) -> (bool, ActorResult<()>) {
        let delivered = if let Some(sender) = self.sender.take() {
            match sender.send(self.result.take()) {
                Ok(()) => true,
                Err(result) => {
                    self.result = ReplyValue::new(result);
                    false
                }
            }
        } else {
            false
        };
        (delivered, self.result.retire())
    }
}

// Closing an unused sender runs the receiver's waker. Keep that callback out of
// a simultaneous payload unwind without changing the envelope's public fields.
pub(super) struct ReplySender(Option<oneshot::Sender<ActorResult<BoxedMessage>>>);
impl ReplySender {
    pub(super) fn new(sender: Option<oneshot::Sender<ActorResult<BoxedMessage>>>) -> Self {
        Self(sender)
    }
    pub(super) fn take(&mut self) -> Option<oneshot::Sender<ActorResult<BoxedMessage>>> {
        self.0.take()
    }
}
impl Drop for ReplySender {
    fn drop(&mut self) {
        if let Err(error) = crate::owned_payload::PublicationInput::new(self.0.take()).retire() {
            report_cleanup(&error);
        }
    }
}

pub(super) struct ReplyValue<T>(Option<crate::owned_payload::PublicationInput<T>>);
impl<T> ReplyValue<T> {
    pub(super) fn new(value: T) -> Self {
        Self(Some(crate::owned_payload::PublicationInput::new(value)))
    }
    fn take(&mut self) -> T {
        self.0.as_mut().expect("unpublished reply value").take()
    }
    pub(super) fn retire(mut self) -> ActorResult<()> {
        self.0.take().expect("unpublished reply value").retire()
    }
}
impl<T> Drop for ReplyValue<T> {
    fn drop(&mut self) {
        if let Some(value) = self.0.take()
            && let Err(error) = value.retire()
        {
            report_cleanup(&error);
        }
    }
}

pub(super) fn report_cleanup(error: &ActorError) {
    tracing::error!(
        error = %crate::error_receipt::actor_summary(error),
        "reply helper disposal failed"
    );
}

// This public compatibility channel is thread-engine-only. Actix ask/reply uses its
// managed actor envelope and lifecycle receipt directly; adding an Actix
// adapter here would create a second reply protocol without a production
// caller. It has no actor generation owner to record a retirement failure.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn test_send_reply_success() {
        let (tx, rx) = oneshot::channel();
        let channel = ThreadReplyChannel(tx);

        Box::new(channel)
            .send_reply(Ok(Box::new("ok") as BoxedMessage))
            .await
            .expect("send must succeed");

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("reply arrives")
            .expect("channel open");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_reply_error_payload() {
        let (tx, rx) = oneshot::channel();
        let channel = ThreadReplyChannel(tx);

        Box::new(channel)
            .send_reply(Err(ActorError::Timeout))
            .await
            .expect("send itself must succeed");

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("reply arrives")
            .expect("channel open");
        assert!(matches!(result, Err(ActorError::Timeout)));
    }

    #[tokio::test]
    async fn test_send_reply_to_dropped_receiver_errors() {
        let (tx, rx) = oneshot::channel();
        drop(rx);
        let channel = ThreadReplyChannel(tx);

        let result = Box::new(channel)
            .send_reply(Ok(Box::new(()) as BoxedMessage))
            .await;
        assert!(matches!(result, Err(ActorError::ReplyChannelError(_))));
    }

    #[test]
    fn receiver_keeps_oneshot_surface_without_exposing_raw_ownership() {
        let (tx, rx) = oneshot::channel();
        let mut receiver = ReplyReceiver::new(rx);
        assert!(matches!(
            receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        receiver.close();
        assert!(tx.send(Err(ActorError::Timeout)).is_err());
    }

    #[test]
    fn test_debug_formatting() {
        let (tx, _rx) = oneshot::channel();
        let repr = format!("{:?}", ThreadReplyChannel(tx));
        assert!(repr.contains("ThreadReplyChannel"));
    }

    #[test]
    fn unpolled_channel_reply_contains_notification_panic_and_closes_receiver() {
        struct HostileWake(Arc<AtomicUsize>);
        impl std::task::Wake for HostileWake {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("cancelled channel notification");
            }
        }
        let wakes = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = oneshot::channel();
        let mut receiver = ReplyReceiver::new(receiver);
        let waker = std::task::Waker::from(Arc::new(HostileWake(wakes.clone())));
        let mut context = Context::from_waker(&waker);
        assert!(Pin::new(&mut receiver).poll(&mut context).is_pending());
        let future =
            Box::new(ThreadReplyChannel(sender)).send_reply(Ok(Box::new(ReceiverPayload {
                drops: drops.clone(),
                panic_on_drop: true,
            })));
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok());
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(matches!(
            Pin::new(&mut receiver).poll(&mut context),
            Poll::Ready(Err(_))
        ));
    }

    struct ReceiverPayload {
        drops: Arc<AtomicUsize>,
        panic_on_drop: bool,
    }
    impl Drop for ReceiverPayload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            if self.panic_on_drop {
                panic!("receiver payload destructor");
            }
        }
    }

    #[tokio::test]
    async fn cancelled_receiver_retires_already_published_reply() {
        let drops = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = oneshot::channel();
        let mut receiver = Box::pin(ReplyReceiver::new(rx));
        assert!(futures::poll!(&mut receiver).is_pending());
        tx.send(Ok(Box::new(ReceiverPayload {
            drops: drops.clone(),
            panic_on_drop: false,
        }) as BoxedMessage))
            .unwrap();
        drop(receiver);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_receiver_contains_reply_destructor_panic() {
        let drops = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = oneshot::channel();
        let mut receiver = Box::pin(ReplyReceiver::new(rx));
        assert!(futures::poll!(&mut receiver).is_pending());
        tx.send(Ok(Box::new(ReceiverPayload {
            drops: drops.clone(),
            panic_on_drop: true,
        }) as BoxedMessage))
            .unwrap();
        let cancelled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(receiver)));
        assert!(cancelled.is_ok());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
