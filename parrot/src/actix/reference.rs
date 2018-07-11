use std::sync::Arc;
use std::any::Any;
use std::fmt::Debug;
use actix::Addr;
use async_trait::async_trait;
use anyhow::anyhow;
use parrot_api::address::{ActorRef, ActorRefExt};
use parrot_api::message::{Message, MessageEnvelope};
use parrot_api::errors::ActorError;
use parrot_api::types::{BoxedMessage, BoxedActorRef, BoxedFuture, ActorResult};
use crate::actix::actor::ActixActor;
use crate::actix::message::ActixMessageWrapper;

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
#[derive(Debug)]
pub struct ActixActorRef<A>
where 
    A: parrot_api::actor::Actor + std::marker::Unpin + std::fmt::Debug + 'static,
{
    /// The underlying Actix address
    addr: Arc<Addr<ActixActor<A>>>,
    /// Path to the actor in the system
    path: String,
}

impl<A> ActixActorRef<A>
where
    A: parrot_api::actor::Actor + std::marker::Unpin + std::fmt::Debug + 'static,
{
    /// Create a new ActixActorRef from an Actix address
    pub fn new(addr: Addr<ActixActor<A>>, path: String) -> Self {
        Self {
            addr: Arc::new(addr),
            path,
        }
    }
}

#[async_trait]
impl<A> ActorRef for ActixActorRef<A>
where
    A: parrot_api::actor::Actor + std::marker::Unpin + std::fmt::Debug + 'static,
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
                Ok(result) => result,
                Err(e) => Err(ActorError::Other(anyhow!("Failed to deliver message: {}", e))),
            }
        })
    }
    
    /// Stop the actor
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        let addr = self.addr.clone();
        
        Box::pin(async move {
            // Use our custom StopMessage
            addr.do_send(StopMessage);
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

/// Type-erased ActorRef for Actix actors
/// This type can hold any ActixActor reference
#[derive(Debug)]
pub struct DynamicActixActorRef {
    /// The underlying actor reference (dynamically typed)
    inner: BoxedActorRef,
}

impl DynamicActixActorRef {
    /// Create a new DynamicActixActorRef by wrapping a typed ActixActorRef
    pub fn new<A: parrot_api::actor::Actor + std::marker::Unpin + std::fmt::Debug + 'static>(actor_ref: ActixActorRef<A>) -> Self {
        Self {
            inner: Box::new(actor_ref),
        }
    }
}

#[async_trait]
impl ActorRef for DynamicActixActorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.inner.send(msg)
    }
    
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        self.inner.stop()
    }
    
    fn path(&self) -> String {
        self.inner.path()
    }
    
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.inner.is_alive()
    }
    
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            inner: self.inner.clone_boxed(),
        })
    }
} 