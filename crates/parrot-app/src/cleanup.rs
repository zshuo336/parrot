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
impl RetainedActor {
    pub(crate) fn from_owned(path: String, error: String, actor: BoxedActorRef) -> Self {
        Self {
            path,
            error,
            actor: Arc::new(RetainedReference(Some(actor))),
        }
    }
}

// The error and the assembly share these originals without calling user clone
// code. Releasing the last error must isolate each reference, even after all
// actors have retired. This guard does not initiate asynchronous retirement.
struct RetainedReference(Option<BoxedActorRef>);
impl RetainedReference {
    fn target(&self) -> &dyn ActorRef {
        &**self.0.as_ref().expect("retained reference")
    }
}
impl std::fmt::Debug for RetainedReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RetainedReference")
    }
}
impl Drop for RetainedReference {
    fn drop(&mut self) {
        if !crate::disposal::drop_catching(self.0.take()) {
            tracing::warn!("retained host reference disposal panicked");
        }
    }
}
impl ActorRef for RetainedReference {
    fn retirement_status(&self) -> parrot_api::address::RetirementStatus {
        self.target().retirement_status()
    }
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.target().send(message)
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.target().send_with_timeout(message, timeout)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.target().deliver(message)
    }
    fn send_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.target().send_with_options(message, options)
    }
    fn deliver_with_options<'a>(
        &'a self,
        message: BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        self.target().deliver_with_options(message, options)
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        self.target().stop()
    }
    fn path(&self) -> String {
        self.target().path()
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        self.target().is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self(Some(self.target().clone_boxed())))
    }
    fn death_watch(&self) -> Option<parrot_api::death_watch::DeathWatch> {
        self.target().death_watch()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self.target().as_any()
    }
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
    fn retirement_status(&self) -> parrot_api::address::RetirementStatus {
        self.actor.retirement_status()
    }
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
