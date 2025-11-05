//! Original actor identities retained by an incomplete deployment rollback.
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{sync::Arc, time::Duration};

/// A retryable cleanup reference. Cloning never calls user `clone_boxed` code.
/// Keep the owning host or these references until cleanup has completed.
#[derive(Clone)]
pub struct RetainedActor {
    pub path: String,
    pub error: String,
    pub actor: Arc<dyn ActorRef>,
}
impl std::fmt::Debug for RetainedActor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedActor")
            .field("path", &self.path)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl PartialEq for RetainedActor {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.error == other.error
            && Arc::ptr_eq(&self.actor, &other.actor)
    }
}
impl ActorRef for RetainedActor {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send(message)
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send_with_timeout(message, timeout)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.actor.deliver(message)
    }
    fn send_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send_with_options(message, options)
    }
    fn deliver_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        self.actor.deliver_with_options(message, options)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        self.actor.stop()
    }
    fn path(&self) -> String {
        self.path.clone()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.actor.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn death_watch(&self) -> Option<parrot_api::death_watch::DeathWatch> {
        self.actor.death_watch()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
