use std::any::Any;
use async_trait::async_trait;
use crate::context::ActorContext;
use crate::address::ActorRef;
use crate::errors::ActorError;


/// Actor lifecycle state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorState {
    Starting,
    Running,
    Stopping,
    Stopped,
}

/// Actor configuration trait
pub trait ActorConfig: Send + Sync + 'static {}

/// Actor factory trait
pub trait ActorFactory<A: Actor>: Send + 'static {
    fn create(&self, config: A::Config) -> A;
}


/// Core Actor trait
#[async_trait]
pub trait Actor: Send + 'static {
    /// Actor configuration type
    type Config: ActorConfig;

    /// Initialize the Actor
    async fn init(&mut self, _ctx: &mut dyn ActorContext) -> Result<(), ActorError> {
        Ok(())
    }

    /// Handle incoming message
    async fn receive_message(&mut self, msg: Box<dyn Any + Send>, ctx: &mut dyn ActorContext) -> Result<Box<dyn Any + Send>, ActorError>;

    /// Cleanup work before Actor stops
    async fn before_stop(&mut self, _ctx: &mut dyn ActorContext) -> Result<(), ActorError> {
        Ok(())
    }

    /// Handle child Actor termination
    async fn handle_child_terminated(&mut self, _child: Box<dyn ActorRef>, _ctx: &mut dyn ActorContext) -> Result<(), ActorError> {
        Ok(())
    }

    /// Get Actor state
    fn state(&self) -> ActorState;
} 
