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
    /// Set of watcher paths that are watching this actor
    watchers: Option<HashSet<String>>,
    /// Whether `Actor::init` has already run successfully. The state machine
    /// stays in `Starting` until the `Start` control message arrives, so the
    /// state alone cannot distinguish "not yet initialized" from
    /// "initialized, awaiting start".
    initialized: bool,
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
            initialized: false,
        }
    }

    /// Creates a ThreadActor for unit tests with a placeholder path.
    pub fn new_for_test(actor: A) -> Self {
        Self::new(actor, ActorPath::placeholder("test://actor"))
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
        if self.initialized || self.state != ActorState::Starting {
            return Err(ActorError::Other(anyhow!("Actor is already initialized")));
        }

        debug!("Initializing actor at path: {:?}", self.path);

        // current ctx is ThreadContext<A>, not type cast
        match self.inner.init(ctx).await {
            Ok(_) => {
                // Mark initialized; the transition to Running is handled by
                // process_message for the Start control message.
                self.initialized = true;
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
    pub fn add_watcher(&mut self, watcher_path: String) {
        if self.watchers.is_none() {
            self.watchers = Some(HashSet::new());
        }
        self.watchers.as_mut().unwrap().insert(watcher_path);
    }

    /// Remove an actor from the watchers list
    pub fn remove_watcher(&mut self, watcher_path: &str) {
        if let Some(watchers) = self.watchers.as_mut() {
            watchers.remove(watcher_path);
        }
    }

    /// Get the watchers list (empty if none registered)
    pub fn watchers(&self) -> &HashSet<String> {
        static EMPTY: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
        self.watchers.as_ref().unwrap_or_else(|| EMPTY.get_or_init(HashSet::new))
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
        let (payload, reply) = envelope.into_parts();
        match self.inner.receive_message(payload, ctx).await {
            Ok(response) => {
                // Send the reply and return an empty response.
                // A dropped receiver (asker timed out) is not an error here.
                let _ = reply.send(Ok(response));
                Ok(Box::new(()))
            },
            Err(e) => {
                // Send error reply
                let _ = reply.send(Err(e));
                Ok(Box::new(()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::context::ThreadContext;
    use parrot_api::actor::EmptyConfig;

    /// Actor that records lifecycle callbacks.
    #[derive(Debug, Default)]
    struct LifecycleActor {
        inits: u32,
        messages: Vec<String>,
        before_stop_called: bool,
    }

    impl Actor for LifecycleActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(
            &'a mut self,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.inits += 1;
                Ok(())
            })
        }

        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if let Some(s) = msg.downcast_ref::<String>() {
                    self.messages.push(s.clone());
                }
                Ok(msg)
            })
        }

        fn receive_message_with_engine<'a>(
            &'a mut self,
            _msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
            _engine_ctx: parrot_api::actor::EngineContextHandle,
        ) -> Option<ActorResult<BoxedMessage>> {
            None
        }

        fn before_stop<'a>(
            &'a mut self,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.before_stop_called = true;
                Ok(())
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    fn make_actor() -> (ThreadActor<LifecycleActor>, ThreadContext<LifecycleActor>) {
        (
            ThreadActor::new_for_test(LifecycleActor::default()),
            ThreadContext::new_for_test("test/thread-actor"),
        )
    }

    #[tokio::test]
    async fn test_initialize_transitions_and_allows_start() {
        let (mut actor, mut ctx) = make_actor();
        assert_eq!(actor.state(), ActorState::Starting);

        actor.initialize(&mut ctx).await.expect("init ok");
        // Still Starting until the Start control message arrives.
        assert_eq!(actor.state(), ActorState::Starting);
    }

    #[tokio::test]
    async fn test_initialize_twice_errors() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();

        let second = actor.initialize(&mut ctx).await;
        assert!(second.is_err(), "double init must be rejected");
    }

    #[tokio::test]
    async fn test_start_control_message_transitions_to_running() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();

        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();
        assert_eq!(actor.state(), ActorState::Running);
    }

    #[tokio::test]
    async fn test_regular_message_rejected_before_running() {
        let (mut actor, mut ctx) = make_actor();
        // Not started yet: regular messages must be rejected.
        let result = actor
            .process_message(Box::new(String::from("hi")), &mut ctx)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_regular_message_processed_when_running() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();
        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();

        let reply = actor
            .process_message(Box::new(String::from("hello")), &mut ctx)
            .await
            .unwrap();
        let echoed = reply.downcast::<String>().expect("echo is String");
        assert_eq!(*echoed, "hello");
    }

    #[tokio::test]
    async fn test_stop_control_message_shuts_down_and_calls_before_stop() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();
        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();

        actor
            .process_message(Box::new(ControlMessage::Stop), &mut ctx)
            .await
            .unwrap();
        assert_eq!(actor.state(), ActorState::Stopped);
    }

    #[tokio::test]
    async fn test_shutdown_is_idempotent() {
        let (mut actor, mut ctx) = make_actor();
        actor.shutdown(&mut ctx).await.unwrap();
        // Second shutdown is a no-op.
        actor.shutdown(&mut ctx).await.unwrap();
        assert_eq!(actor.state(), ActorState::Stopped);
    }

    #[tokio::test]
    async fn test_health_check_returns_state() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();
        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();

        let reply = actor
            .process_message(Box::new(ControlMessage::HealthCheck), &mut ctx)
            .await
            .unwrap();
        let state = reply.downcast::<ActorState>().expect("state payload");
        assert_eq!(*state, ActorState::Running);
    }

    #[tokio::test]
    async fn test_watch_request_registers_watcher() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();
        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();

        actor
            .process_message(
                Box::new(crate::thread::system::WatchRequest {
                    watcher_path: "/user/watcher".into(),
                }),
                &mut ctx,
            )
            .await
            .unwrap();

        assert!(actor.watchers().contains("/user/watcher"));

        // Unwatch removes it again.
        actor
            .process_message(
                Box::new(crate::thread::system::UnwatchRequest {
                    watcher_path: "/user/watcher".into(),
                }),
                &mut ctx,
            )
            .await
            .unwrap();
        assert!(!actor.watchers().contains("/user/watcher"));
    }

    #[tokio::test]
    async fn test_watchers_empty_by_default() {
        let (actor, _ctx) = make_actor();
        assert!(actor.watchers().is_empty());
    }

    #[tokio::test]
    async fn test_ask_envelope_replies_through_channel() {
        let (mut actor, mut ctx) = make_actor();
        actor.initialize(&mut ctx).await.unwrap();
        actor
            .process_message(Box::new(ControlMessage::Start), &mut ctx)
            .await
            .unwrap();

        let (envelope, rx) = AskEnvelope::new(Box::new(String::from("ping")));
        let reply = actor
            .process_message(Box::new(envelope), &mut ctx)
            .await
            .unwrap();

        // The actor consumes the envelope and returns unit; the actual reply
        // arrives on the oneshot channel.
        let _ = reply.downcast::<()>().expect("unit ack");
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("ask reply arrives")
            .expect("channel open");
        let payload = response.expect("Ok reply");
        assert_eq!(*payload.downcast::<String>().unwrap(), "ping");
    }

    #[tokio::test]
    async fn test_ask_envelope_rejected_when_not_running() {
        let (mut actor, mut ctx) = make_actor();
        let (envelope, rx) = AskEnvelope::new(Box::new(String::from("ping")));

        let result = actor
            .process_message(Box::new(envelope), &mut ctx)
            .await;
        assert!(result.is_err(), "ask before running must fail");

        // Dropping rx here must not panic.
        drop(rx);
    }

    #[tokio::test]
    async fn test_failed_init_marks_actor_stopped() {
        #[derive(Debug, Default)]
        struct FailingActor;

        impl Actor for FailingActor {
            type Config = EmptyConfig;
            type Context = ThreadContext<Self>;

            fn init<'a>(
                &'a mut self,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async { Err(ActorError::MessageHandlingError("init boom".into())) })
            }

            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move { Ok(msg) })
            }

            fn receive_message_with_engine<'a>(
                &'a mut self,
                _msg: BoxedMessage,
                _ctx: &'a mut Self::Context,
                _engine_ctx: parrot_api::actor::EngineContextHandle,
            ) -> Option<ActorResult<BoxedMessage>> {
                None
            }

            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }

        let (mut actor, mut ctx) = (
            ThreadActor::new_for_test(FailingActor),
            ThreadContext::new_for_test("test/failing"),
        );
        let result = actor.initialize(&mut ctx).await;
        assert!(result.is_err());
        assert_eq!(actor.state(), ActorState::Stopped);
    }
} 