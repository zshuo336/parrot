use crate::thread::reply::ReplyChannel;
use parrot_api::types::BoxedMessage;
use std::fmt::Debug;

/// Wrapper for messages sent via 'ask' to carry a reply channel.
#[derive(Debug)]
pub struct AskEnvelope {
    pub payload: BoxedMessage,
    /// The channel to send the reply back on.
    pub reply: Box<dyn ReplyChannel>,
}

// Note: BoxedMessage already implements Debug, and Box<dyn ReplyChannel> will require ReplyChannel: Debug 