use crate::thread::reply::{ReplyChannel, ThreadReplyChannel};
use parrot_api::types::BoxedMessage;
use std::fmt;
use std::fmt::Debug;
use tokio::sync::oneshot;

use parrot_api::errors::ActorError;
use parrot_api::types::ActorResult;

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

impl AskEnvelope {
    /// Create a new ask envelope from a message and a oneshot reply sender.
    pub fn new(payload: BoxedMessage) -> (Self, oneshot::Receiver<ActorResult<BoxedMessage>>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                payload,
                reply: Box::new(ThreadReplyChannel(tx)),
            },
            rx,
        )
    }
}

impl AskEnvelope {
    /// send success reply
    pub async fn reply_success(self, response: BoxedMessage) {
        self.reply.send_reply(Ok(response)).await;
    }

    /// send error reply
    pub async fn reply_error(self, error: ActorError) {
        self.reply.send_reply(Err(error)).await;
    }
}

/// Control messages used internally within the actor system.
#[derive(Debug, Clone)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ask_envelope_reply_success() {
        let (envelope, rx) = AskEnvelope::new(Box::new("ping") as BoxedMessage);

        // Responder side: send a successful reply.
        let responder = tokio::spawn(async move {
            envelope.reply_success(Box::new("pong") as BoxedMessage).await;
        });
        responder.await.unwrap();

        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("reply must arrive")
            .expect("channel must be open");
        let payload = reply.expect("reply is Ok");
        let pong = payload.downcast::<&str>().expect("payload is &str");
        assert_eq!(*pong, "pong");
    }

    #[tokio::test]
    async fn test_ask_envelope_reply_error() {
        let (envelope, rx) = AskEnvelope::new(Box::new("ping") as BoxedMessage);

        let responder = tokio::spawn(async move {
            envelope
                .reply_error(ActorError::MessageHandlingError("boom".into()))
                .await;
        });
        responder.await.unwrap();

        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("reply must arrive")
            .expect("channel must be open");
        match reply {
            Err(ActorError::MessageHandlingError(msg)) => assert!(msg.contains("boom")),
            other => panic!("expected MessageHandlingError, got {:?}", other.map(|_| ())),
        }
    }

    #[tokio::test]
    async fn test_dropped_receiver_reports_send_failure() {
        // When the asker drops the receiver (timeout / cancellation), the
        // reply channel must surface a send error rather than panic.
        let (envelope, rx) = AskEnvelope::new(Box::new("ping") as BoxedMessage);
        drop(rx);

        let result = envelope.reply.send_reply(Ok(Box::new(()))).await;
        // ThreadReplyChannel maps a dropped receiver to ReplyChannelError.
        assert!(matches!(result, Err(ActorError::ReplyChannelError(_))));
    }

    #[test]
    fn test_control_message_variants_are_cloneable() {
        let msgs = vec![
            ControlMessage::Start,
            ControlMessage::Stop,
            ControlMessage::ChildFailure {
                path: "/user/child".into(),
                reason: "panic".into(),
            },
            ControlMessage::SystemShutdown,
            ControlMessage::HealthCheck,
        ];
        for msg in &msgs {
            let cloned = msg.clone();
            // Debug formatting must work for all variants (used in logs).
            let repr = format!("{:?}", cloned);
            assert!(!repr.is_empty());
        }
    }

    #[test]
    fn test_envelope_debug_formatting() {
        let (envelope, _rx) = AskEnvelope::new(Box::new(42u64) as BoxedMessage);
        let repr = format!("{:?}", envelope);
        assert!(repr.contains("AskEnvelope"));
        assert!(repr.contains("payload"));
        assert!(repr.contains("reply"));
    }
}
