use async_trait::async_trait;
use parrot_api::{
    errors::ActorError,
    types::{ActorResult, BoxedMessage},
};
use std::fmt::Debug;
use tokio::sync::oneshot;

/// A type-erased, sendable channel for replying to an `ask` request.
#[async_trait]
pub trait ReplyChannel: Send + Sync + Debug {
    /// Send the reply message. Consumes the channel.
    /// The result indicates success or failure of the original actor's processing.
    async fn send_reply(self: Box<Self>, msg: ActorResult<BoxedMessage>) -> ActorResult<()>;
}

/// Implementation of ReplyChannel using a Tokio oneshot channel.
#[derive(Debug)]
pub struct ThreadReplyChannel(pub oneshot::Sender<ActorResult<BoxedMessage>>);

#[async_trait]
impl ReplyChannel for ThreadReplyChannel {
    async fn send_reply(self: Box<Self>, msg: ActorResult<BoxedMessage>) -> ActorResult<()> {
        // A dropped receiver means the asker timed out or shut down. Preserve
        // that fact as a reply-channel error so the caller can distinguish a
        // delivered reply from a late one; the original business result is
        // still returned by the failed send and is retired with the channel.
        let result = self.0.send(msg);
        match result {
            Ok(_) => Ok(()),
            Err(_) => Err(ActorError::ReplyChannelError("Failed to send reply".into())),
        }
    }
}

// This channel is intentionally thread-engine-only. Actix ask/reply uses its
// managed actor envelope and lifecycle receipt directly; adding an Actix
// adapter here would create a second reply protocol without a production
// caller. Keep the trait private to the thread implementation until a shared
// cross-engine reply contract exists.

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_debug_formatting() {
        let (tx, _rx) = oneshot::channel();
        let repr = format!("{:?}", ThreadReplyChannel(tx));
        assert!(repr.contains("ThreadReplyChannel"));
    }
}
