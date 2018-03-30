use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use crate::errors::ActorError;
use crate::types::{BoxedActorRef, BoxedMessage, ActorResult, BoxedFuture, WeakActorTarget};
use crate::message::{Message, MessageEnvelope};
use std::sync::Arc;

/// Actor path
#[derive(Debug, Clone)]
pub struct ActorPath {
    pub target: WeakActorTarget,
    pub path: String,
}

impl PartialEq for ActorPath {
    fn eq(&self, other: &Self) -> bool {
        self.target.path() == other.target.path() && self.path == other.path
    }
}

impl Eq for ActorPath {}

impl Hash for ActorPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.path.hash(state);
    }
}

impl Display for ActorPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path)
    }
}

/// Actor reference trait
pub trait ActorRef: Send + Sync + Debug {
    /// Send a message to the actor
    fn send<'a>(&'a self, msg: MessageEnvelope) -> BoxedFuture<'a, ActorResult<BoxedMessage>>;

    /// Stop the actor
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>>;

    /// Get the actor's path
    fn path(&self) -> String;

    /// Check if actor is alive
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool>;

    /// Clone the actor reference into a boxed trait object
    fn clone_boxed(&self) -> BoxedActorRef;
}

/// Extension trait for ActorRef to provide type-safe message sending
pub trait ActorRefExt: ActorRef {
    /// Send message and wait for response
    fn ask<'a, M: Message>(&'a self, msg: M) -> BoxedFuture<'a, ActorResult<M::Result>> {
        let envelope = MessageEnvelope::new(msg, None, None);
        let this = self.clone_boxed();
        Box::pin(async move {
            let result = this.send(envelope).await?;
            M::extract_result(result)
        })
    }
    
    /// Send message without waiting for response
    fn tell<M: Message>(&self, msg: M) {
        let envelope = MessageEnvelope::new(msg, None, None);
        let actor_ref = self.clone_boxed();
        tokio::spawn(async move {
            let _ = actor_ref.send(envelope).await;
        });
    }
}

// Implement ActorRefExt for all types that implement ActorRef
impl<T: ActorRef + ?Sized> ActorRefExt for T {}

/// Weak Actor reference
#[derive(Clone, Debug)]
pub struct WeakActorRef {
    /// Actor path
    pub path: ActorPath,
}

impl WeakActorRef {
    /// Create a new weak reference
    pub fn new(path: ActorPath) -> Self {
        Self { path }
    }

    /// Try to upgrade to strong reference
    fn upgrade<'a>(&'a self) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async move { None })
    }
} 