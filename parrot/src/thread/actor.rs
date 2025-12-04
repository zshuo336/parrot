use async_trait::async_trait;
use parrot_api::actor::{Actor, ActorState};
use parrot_api::address::ActorPath;
use parrot_api::types::{BoxedMessage, ActorResult, BoxedFuture, BoxedActorRef};
use parrot_api::errors::ActorError;
use std::fmt::Debug;
use std::collections::HashSet;
use anyhow::anyhow;
use tracing::{error, debug, info, warn};
use parrot_api::message::BoxedMessageClone;

use crate::thread::mailbox::Mailbox;
use crate::thread::context::ThreadContext;
use crate::thread::envelope::{AskEnvelope, ControlMessage};

/// Thread-based implementation of an Actor.
/// 
/// This is a wrapper that adapts the generic Actor trait to the
/// thread-based execution model, handling message dispatching and lifecycle.
#[derive(Debug)]
pub struct ThreadActor<A>
where
    A: Actor + Send + Sync + 'static,
    // remove the Deref constraint on A::Context, and force the constraint that A::Context must be ThreadContext<A>
    A::Context: Send + 'static,
{
    /// The wrapped actor implementation
    inner: A,
    /// Current actor state
    state: ActorState,
    /// Actor path for addressing
    path: ActorPath,
    /// Set of actor paths that are watching this actor
    watchers: Option<HashSet<ActorPath>>,
}

impl<A> ThreadActor<A> 
where
    A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
{
    /// Creates a new ThreadActor wrapping the provided actor implementation.
    pub fn new(actor: A, path: ActorPath) -> Self {
        Self {
            inner: actor,
            state: ActorState::Starting,
            path,
            watchers: None,
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
    pub async fn initialize<'a>(&'a mut self, ctx: &'a mut ThreadContext<A>) -> ActorResult<()> {
        if self.state != ActorState::Starting {
            return Err(ActorError::Other(anyhow!("Actor is already initialized")));
        }

        debug!("Initializing actor at path: {:?}", self.path);

        // current ctx is ThreadContext<A>, not type cast
        match self.inner.init(ctx).await {
            Ok(_) => {
                // Transition to Running state is handled by process_message for Start message
                Ok(())
            },
            Err(e) => {
                error!("Failed to initialize actor at {:?}: {}", self.path, e);
                self.state = ActorState::Stopped;
                Err(e)
            }
        }
    }

    /// Add an actor to the watchers list
    pub fn add_watcher(&mut self, watcher_path: ActorPath) {
        if self.watchers.is_none() {
            self.watchers = Some(HashSet::new());
        }
        self.watchers.as_mut().unwrap().insert(watcher_path);
    }

    /// Remove an actor from the watchers list
    pub fn remove_watcher(&mut self, watcher_path: &ActorPath) {
        if let Some(watchers) = self.watchers.as_mut() {
            watchers.remove(watcher_path);
        }
    }

    /// Get the watchers list
    pub fn watchers(&self) -> &HashSet<ActorPath> {
        self.watchers.as_ref().unwrap()
    }
    
    /// Process a message.
    pub async fn process_message<'a>(&'a mut self, msg: BoxedMessage, ctx: &'a mut ThreadContext<A>) -> ActorResult<BoxedMessage> {
        // Handle control messages specially
        if let Some(control_msg) = msg.downcast_ref::<ControlMessage>() {
            return self.handle_control_message(control_msg, ctx).await;
        }
        
        // Handle watch request
        if let Some(watch_req) = msg.downcast_ref::<crate::thread::system::WatchRequest>() {
            debug!("Actor at {:?} received watch request from {:?}", 
                   self.path, watch_req.watcher_path);
            self.add_watcher(watch_req.watcher_path.clone());
            return Ok(Box::new(()));
        }
        
        // Handle unwatch request
        if let Some(unwatch_req) = msg.downcast_ref::<crate::thread::system::UnwatchRequest>() {
            debug!("Actor at {:?} received unwatch request from {:?}", 
                   self.path, unwatch_req.watcher_path);
            self.remove_watcher(&unwatch_req.watcher_path);
            return Ok(Box::new(()));
        }
        
        // Only process regular messages if the actor is running
        if self.state != ActorState::Running {
            return Err(ActorError::Other(anyhow!("Actor is not running")));
        }
        
        // Handle Ask messages specially
        if msg.is::<AskEnvelope>() {
            return self.handle_ask_envelope(*msg.downcast::<AskEnvelope>().unwrap(), ctx).await;
        }

        // Handle Ask messages specially
        // call inner actor's receive_message
        self.inner.receive_message(msg, ctx).await
    }
    
    /// Handle internal control messages
    async fn handle_control_message<'a>(&'a mut self, control_msg: &ControlMessage, ctx: &'a mut ThreadContext<A>) -> ActorResult<BoxedMessage> {
        match control_msg {
            ControlMessage::Start => {
                debug!("Handling Start message for actor at {:?}", self.path);
                if self.state == ActorState::Starting {
                    // Transition to Running state
                    self.state = ActorState::Running;
                    info!("Actor at {:?} is now running", self.path);
                    Ok(Box::new(()))
                } else {
                    warn!("Ignoring Start message for actor at {:?} in state {:?}", 
                        self.path, self.state);
                    Ok(Box::new(()))
                }
            },
            
            ControlMessage::Stop => {
                debug!("Handling Stop message for actor at {:?}", self.path);
                // Shut down the actor
                self.shutdown(ctx).await?;
                Ok(Box::new(()))
            },
            
            ControlMessage::ChildFailure { path, reason } => {
                debug!("Handling ChildFailure message for child {:?} at parent {:?}: {}", 
                    path, self.path, reason);
                // get child_ref from context children
                let child_ref = ctx.children()
                    .and_then(|child| child.read_all().iter()
                        .find(|child| child.eq_path(path))
                        .map(|c| c.clone_boxed()))
                    .ok_or_else(|| ActorError::Other(anyhow!("Child actor reference not found")))?;
                
                self.inner.handle_child_terminated(child_ref, ctx).await?;
                Ok(Box::new(()))
            },
            
            ControlMessage::SystemShutdown => {
                debug!("Handling SystemShutdown message for actor at {:?}", self.path);
                // The system is shutting down, stop the actor
                self.shutdown(ctx).await?;
                Ok(Box::new(()))
            },
            
            ControlMessage::HealthCheck => {
                debug!("Handling HealthCheck message for actor at {:?}", self.path);
                // Just return the current state
                Ok(Box::new(self.state))
            }
        }
    }
    
    /// Handle ask envelopes
    async fn handle_ask_envelope<'a>(&'a mut self, envelope: AskEnvelope, ctx: &'a mut ThreadContext<A>) -> ActorResult<BoxedMessage> {
        // Only process messages if the actor is running
        if self.state != ActorState::Running {
            return Err(ActorError::Other(anyhow!("Actor is not running")));
        }
        
        // Process the message with the inner actor
        match self.inner.receive_message(envelope.payload, ctx).await {
            Ok(response) => {
                // Send the reply and return an empty response
                match envelope.reply.send_reply(Ok(response)).await {
                    Ok(_) => Ok(Box::new(())),
                    Err(e) => {
                        error!("Failed to send reply for ask operation: {}", e);
                        Err(e)
                    }
                }
            },
            Err(e) => {
                // Send error reply
                let err_clone = ActorError::Other(anyhow!(e.to_string()));
                match envelope.reply.send_reply(Err(e)).await {
                    Ok(_) => Ok(Box::new(())),
                    Err(send_err) => {
                        error!("Failed to send error reply for ask operation: {}", send_err);
                        Err(err_clone)
                    }
                }
            }
        }
    }
    
    /// Shut down the actor.
    pub async fn shutdown<'a>(&'a mut self, ctx: &'a mut ThreadContext<A>) -> ActorResult<()> {
        // Only attempt shutdown if not already stopped
        if self.state == ActorState::Stopped {
            return Ok(());
        }
        
        debug!("Shutting down actor at {:?}", self.path);
        
        // Transition to Stopping state
        self.state = ActorState::Stopping;
        
        // Call the inner actor's before_stop method
        let result = self.inner.before_stop(ctx).await;
        
        // Transition to Stopped state regardless of result
        self.state = ActorState::Stopped;
        
        // Notify all watchers
        self.notify_watchers_of_termination(ctx);
        
        info!("Actor at {:?} has stopped", self.path);
        
        result
    }
    
    /// Notify all watchers that this actor has terminated
    fn notify_watchers_of_termination(&self, _ctx: &ThreadContext<A>) {
        if let Some(watchers) = self.watchers.as_ref() {
            debug!("Notifying {} watchers of termination for actor at {:?}", 
                watchers.len(), self.path);
            
            // TODO: Implement actual notification logic
            // This would involve sending death notification messages to all watchers
            // For now this is just a placeholder
        }
    }
}

// Note: This is a stub implementation that will be expanded in future PRs
// to include more actor lifecycle management, supervision, etc. 