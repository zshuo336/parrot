use async_trait::async_trait;
use parrot_api::{types::{BoxedMessage, ActorResult}};
use std::fmt::Debug;
use tokio::sync::oneshot;

/// A type-erased, sendable channel for replying to an `ask` request.
#[async_trait]
pub trait ReplyChannel: Send + Sync + Debug {
    /// Send the reply message. Consumes the channel.
    /// The result indicates success or failure of the original actor's processing.
    async fn send_reply(self: Box<Self>, msg: ActorResult<BoxedMessage>);
}

/// Implementation of ReplyChannel using a Tokio oneshot channel.
#[derive(Debug)]
pub struct ThreadReplyChannel(pub oneshot::Sender<ActorResult<BoxedMessage>>);

#[async_trait]
impl ReplyChannel for ThreadReplyChannel {
    async fn send_reply(self: Box<Self>, msg: ActorResult<BoxedMessage>) {
        // Ignore the result of send. If the receiver was dropped, it means the asker
        // is no longer waiting (e.g., due to timeout or shutdown), which is fine.
        let _ = self.0.send(msg);
    }
}

// TODO: Need an ActixReplyChannel implementation if enabling interop,
// potentially wrapping actix::prelude::Recipient<ReplyMessage> or similar. 