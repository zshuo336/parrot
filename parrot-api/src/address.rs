use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::any::Any;
use async_trait::async_trait;
use crate::errors::ActorError;
use crate::message::{Message, MessageEnvelope};
use std::sync::Arc;

/// Actor path
#[derive(Debug, Clone)]
pub struct ActorPath {
    pub target: Arc<dyn ActorRef>,
    pub segments: Vec<String>,
}

impl PartialEq for ActorPath {
    fn eq(&self, other: &Self) -> bool {
        self.target.eq_ref(&*other.target) && self.segments == other.segments
    }
}

impl Eq for ActorPath {}

impl Hash for ActorPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.segments.hash(state);
    }
}

impl Display for ActorPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.segments.join("/"))
    }
}

/// Actor reference trait
#[async_trait]
pub trait ActorRef: Send + Sync + Debug + 'static {
    /// Get Actor path
    fn path(&self) -> &ActorPath;
    
    /// Stop Actor
    async fn stop(&self) -> Result<(), ActorError>;
    
    /// Check if Actor is alive
    async fn is_alive(&self) -> bool;
    
    /// Send message to Actor
    async fn send(&self, envelope: MessageEnvelope) -> Result<Box<dyn Any + Send>, ActorError>;
    
    /// Clone boxed reference
    fn clone_box(&self) -> Box<dyn ActorRef>;
    
    /// Compare references for equality
    fn eq_ref(&self, other: &dyn ActorRef) -> bool;
}

impl Clone for Box<dyn ActorRef> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

impl PartialEq for Box<dyn ActorRef> {
    fn eq(&self, other: &Self) -> bool {
        self.eq_ref(&**other)
    }
}

impl Eq for Box<dyn ActorRef> {}

impl Hash for Box<dyn ActorRef> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.path().segments.hash(state)
    }
}

/// Extension trait for ActorRef to provide type-safe message sending
#[async_trait]
pub trait ActorRefExt: ActorRef {
    /// Send message and wait for response
    async fn ask<M: Message>(&self, msg: M) -> Result<M::Result, ActorError> {
        let envelope = MessageEnvelope::new(msg, None, None);
        let result = self.send(envelope).await?;
        M::extract_result(result)
    }
    
    /// Send message without waiting for response
    fn tell<M: Message>(&self, msg: M) -> Result<(), ActorError> {
        let envelope = MessageEnvelope::new(msg, None, None);
        // Fire and forget
        let actor_ref = self.clone_box();
        tokio::spawn(async move {
            let _ = actor_ref.send(envelope).await;
        });
        Ok(())
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
    pub async fn upgrade(&self) -> Option<Box<dyn ActorRef>> {
        // Concrete implementation to be provided by Actor system
        None
    }
} 