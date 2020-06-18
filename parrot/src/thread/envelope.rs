use crate::thread::reply::ReplyChannel;
use parrot_api::types::BoxedMessage;
use std::fmt::Debug;
use std::fmt;
use parrot_api::errors::ActorError;

/// Envelope for ask operations, containing the message payload and reply channel.
pub struct AskEnvelope {
    /// The actual message being sent
    pub payload: BoxedMessage,
    /// Channel to send the reply back to the requester
    pub reply: Box<dyn ReplyChannel>,
}

impl fmt::Debug for AskEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AskEnvelope")
            .field("payload", &"<boxed-message>")
            .field("reply", &"<reply-channel>")
            .finish()
    }
}

/// Control messages used internally within the actor system.
#[derive(Debug)]
pub enum ControlMessage {
    /// Start processing messages (transition from Starting to Running state)
    Start,
    /// Stop the actor (graceful shutdown)
    Stop,
    /// Notify of a child actor failure
    ChildFailure {
        /// Path of the failed child actor
        path: String,
        /// Reason for failure
        reason: String,
    },
    /// System is shutting down, stop gracefully
    SystemShutdown,
    /// Check if actor is healthy (for supervision)
    HealthCheck,
}

// Note: BoxedMessage already implements Debug, and Box<dyn ReplyChannel> will require ReplyChannel: Debug 

// 实现AskEnvelope的方法，用于发送回复
impl AskEnvelope {
    /// 发送成功回复
    pub async fn reply_success(self, response: BoxedMessage) {
        self.reply.send_reply(Ok(response)).await;
    }

    /// 发送错误回复
    pub async fn reply_error(self, error: ActorError) {
        self.reply.send_reply(Err(error)).await;
    }
} 