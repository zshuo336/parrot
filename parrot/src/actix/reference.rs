use std::sync::Arc;
use std::any::Any;
use actix::Addr;
use async_trait::async_trait;
use anyhow::anyhow;
use parrot_api::address::{ActorRef, ActorRefExt};
use parrot_api::actor::Actor as ParrotActor;
use parrot_api::message::{Message, MessageEnvelope};
use parrot_api::errors::ActorError;
use parrot_api::types::{BoxedMessage, BoxedActorRef, BoxedFuture, ActorResult};
use crate::actix::actor::ActixActor;
use crate::actix::message::ActixMessageWrapper;
use crate::actix::context::ActixContext;

/// An internal message to stop the actor
#[derive(Debug)]
pub struct StopMessage;

// Implement actix::Message for StopMessage
impl actix::Message for StopMessage {
    type Result = ();
}

/// ActixActorRef implements the ActorRef trait for Actix addresses
/// 
/// # Overview
/// Provides a reference to an actor in the Actix system
/// 
/// # Key Responsibilities
/// - Send messages to the referenced actor
/// - Track actor address information
/// - Convert between Parrot and Actix message types
/// 
/// # Implementation Details
/// - Wraps an actix::Addr
/// - Handles message conversion and type safety
/// - Manages both sync and async message delivery
pub struct ActixActorRef<A>
where 
    A: ParrotActor<Context = ActixContext<ActixActor<A>>> + std::marker::Unpin + 'static,
{
    /// The underlying Actix address
    addr: Arc<Addr<ActixActor<A>>>,
    /// Path to the actor in the system
    path: String,
}

impl<A> ActixActorRef<A>
where
    A: ParrotActor<Context = ActixContext<ActixActor<A>>> + std::marker::Unpin + 'static,
{
    /// Create a new ActixActorRef from an Actix address
    pub fn new(addr: Addr<ActixActor<A>>, path: String) -> Self {
        Self {
            addr: Arc::new(addr),
            path,
        }
    }
}

impl<A> std::fmt::Debug for ActixActorRef<A>
where
    A: ParrotActor<Context = ActixContext<ActixActor<A>>> + std::marker::Unpin + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActixActorRef")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl<A> ActorRef for ActixActorRef<A>
where
    A: ParrotActor<Context = ActixContext<ActixActor<A>>> + std::marker::Unpin + 'static,
{
    /// Send a message to the actor and wait for a response
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        // Create message envelope
        let envelope = MessageEnvelope {
            id: uuid::Uuid::new_v4(),
            payload: msg,
            sender: None,
            options: Default::default(),
            message_type: "unknown",
        };
        
        // Create ActixMessageWrapper
        let wrapper = ActixMessageWrapper { envelope };
        let addr = self.addr.clone();
        
        // Send the message via Actix
        Box::pin(async move {
            match addr.send(wrapper).await {
                Ok(Some(result)) => result,
                Ok(None) => Err(ActorError::MessageHandlingError("No response from actor".to_string())),
                Err(e) => Err(ActorError::Other(anyhow!("Failed to deliver message: {}", e))),
            }
        })
    }
    
    /// Stop the actor
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        let addr = self.addr.clone();
        
        Box::pin(async move {
            // Wrap StopMessage in an ActixMessageWrapper
            let msg = Box::new(StopMessage) as BoxedMessage;
            let envelope = MessageEnvelope {
                id: uuid::Uuid::new_v4(),
                payload: msg,
                sender: None,
                options: Default::default(),
                message_type: "stop",
            };
            let wrapper = ActixMessageWrapper { envelope };
            addr.do_send(wrapper);
            Ok(())
        })
    }
    
    /// Get the actor's path
    fn path(&self) -> String {
        self.path.clone()
    }
    
    /// Check if the actor is alive
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        let addr = self.addr.clone();
        
        Box::pin(async move {
            // In Actix, we can check if the address is connected
            addr.connected()
        })
    }
    
    /// Clone this reference as a boxed ActorRef
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            addr: self.addr.clone(),
            path: self.path.clone(),
        })
    }
}
