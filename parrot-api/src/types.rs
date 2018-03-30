use crate::address::ActorRef;
use crate::errors::ActorError;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// Type aliases for common types
pub type BoxedActorRef = Box<dyn ActorRef>;
pub type WeakActorTarget = Arc<dyn ActorRef>;
pub type BoxedMessage = Box<dyn Any + Send>;
pub type ActorResult<T> = Result<T, ActorError>;
pub type BoxedFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
