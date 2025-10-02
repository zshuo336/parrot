use parrot_api::types::BoxedMessage;
use std::fmt;
use std::fmt::Debug;
use tokio::sync::oneshot;

use parrot_api::errors::ActorError;
use parrot_api::types::ActorResult;

/// Envelope for ask operations, containing the message payload and reply channel.
///
/// # Allocation profile (ADR-13)
/// An ask costs exactly **one** heap allocation: the `Box<AskEnvelope>` that
/// erases into `BoxedMessage`. The reply channel is stored **inline** as a
/// `oneshot::Sender` (no separate `Box<dyn ReplyChannel>`). Prefer
/// `new_typed` (moves the unboxed message in) over `with_boxed` when the
/// caller still owns the raw value — that avoids boxing the payload twice.
pub struct AskEnvelope {
    /// The actual message being sent
    pub payload: BoxedMessage,
    /// Channel to send the reply back to the requester
    reply: oneshot::Sender<ActorResult<BoxedMessage>>,
}

impl fmt::Debug for AskEnvelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AskEnvelope")
            .field("payload", &"<boxed-message>")
            .field("reply", &"<oneshot>")
            .finish()
    }
}

impl AskEnvelope {
    /// Create an ask envelope from an **unboxed** message (zero-redundancy
    /// constructor: the only allocation is the envelope box itself).
    pub fn new_typed<M: Send + 'static>(msg: M) -> (Self, oneshot::Receiver<ActorResult<BoxedMessage>>) {
        let (tx, rx) = oneshot::channel();
        (Self { payload: Box::new(msg), reply: tx }, rx)
    }

    /// Create an ask envelope from an already-boxed message.
    pub fn with_boxed(payload: BoxedMessage) -> (Self, oneshot::Receiver<ActorResult<BoxedMessage>>) {
        let (tx, rx) = oneshot::channel();
        (Self { payload, reply: tx }, rx)
    }

    /// Compatibility constructor accepting an already-boxed payload.
    pub fn new(payload: BoxedMessage) -> (Self, oneshot::Receiver<ActorResult<BoxedMessage>>) {
        Self::with_boxed(payload)
    }

    /// send success reply
    pub async fn reply_success(self, response: BoxedMessage) {
        let _ = self.reply.send(Ok(response));
    }

    /// send error reply
    pub async fn reply_error(self, error: ActorError) {
        let _ = self.reply.send(Err(error));
    }

    /// Synchronous reply for timeout/shutdown paths where a dropped receiver
    /// is expected and not an error.
    pub fn reply_sync(self, result: ActorResult<BoxedMessage>) {
        let _ = self.reply.send(result);
    }

    /// Deconstruct into (payload, reply sender) — used by the actor-side
    /// handler which consumes both parts.
    pub fn into_parts(self) -> (BoxedMessage, oneshot::Sender<ActorResult<BoxedMessage>>) {
        (self.payload, self.reply)
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
    async fn test_dropped_receiver_is_silently_ignored() {
        // When the asker drops the receiver (timeout / cancellation), the
        // inline oneshot sender must not panic; send failure is expected and
        // silently ignored (ADR-13: dropped receiver is not an actor error).
        let (envelope, rx) = AskEnvelope::new(Box::new("ping") as BoxedMessage);
        drop(rx);

        envelope.reply_sync(Ok(Box::new(())));
        // Reaching here without panic is the assertion.
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
