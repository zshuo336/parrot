use async_trait::async_trait;
use parrot_api::actor::{Actor, ActorState};
use parrot_api::address::ActorPath;
use parrot_api::context::ActorContext;
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture};
use parrot_api::errors::ActorError;
use std::fmt::Debug;
use anyhow::anyhow;

use crate::thread::mailbox::Mailbox;
use crate::thread::context::ThreadContext;

/// Thread-based implementation of an Actor.
/// 
/// This is a wrapper that adapts the generic Actor trait to the
/// thread-based execution model, handling message dispatching and lifecycle.
#[derive(Debug)]
pub struct ThreadActor<A: Actor> {
    /// The wrapped actor implementation
    inner: A,
    /// Current actor state
    state: ActorState,
    /// Actor path for addressing
    path: ActorPath,
}

impl<A: Actor> ThreadActor<A> {
    /// Creates a new ThreadActor wrapping the provided actor implementation.
    pub fn new(actor: A, path: ActorPath) -> Self {
        Self {
            inner: actor,
            state: ActorState::Starting,
            path,
        }
    }
    
    /// Gets the current state of the actor.
    pub fn state(&self) -> ActorState {
        self.state
    }
    
    /// Gets the actor's path.
    pub fn path(&self) -> &ActorPath {
        &self.path
    }
    
    /// Initialize the actor.
    pub async fn initialize<'a>(&'a mut self, ctx: &'a mut ThreadContext) -> ActorResult<()> {
        // Call init on the inner actor
        self.inner.init(ctx).await?;
        
        // Transition to Running state
        self.state = ActorState::Running;
        
        Ok(())
    }
    
    /// Process a message.
    pub async fn process_message<'a>(&'a mut self, msg: BoxedMessage, ctx: &'a mut ThreadContext) -> ActorResult<BoxedMessage> {
        // Only process messages if the actor is running
        if self.state != ActorState::Running {
            return Err(ActorError::Other(anyhow!("Actor is not running")));
        }
        
        // Delegate to the inner actor's receive_message method
        self.inner.receive_message(msg, ctx).await
    }
    
    /// Shut down the actor.
    pub async fn shutdown<'a>(&'a mut self, ctx: &'a mut ThreadContext) -> ActorResult<()> {
        // Only attempt shutdown if not already stopped
        if self.state == ActorState::Stopped {
            return Ok(());
        }
        
        // Transition to Stopping state
        self.state = ActorState::Stopping;
        
        // Call the inner actor's before_stop method
        let result = self.inner.before_stop(ctx).await;
        
        // Transition to Stopped state regardless of result
        self.state = ActorState::Stopped;
        
        result
    }
}

// Note: This is a stub implementation that will be expanded in future PRs
// to include more actor lifecycle management, supervision, etc. 