use std::fmt::Debug;
use std::time::Duration;
use actix::{Actor as ActixActor, Addr, System, Handler, dev::ToEnvelope, ActorContext};
use async_trait::async_trait;
use parrot_api::{
    address::{ActorRef, ActorPath, ActorRefExt},
    errors::ActorError,
    types::{BoxedMessage, BoxedFuture, ActorResult, BoxedActorRef, WeakActorTarget},
    message::{MessageEnvelope, Message, MessageOptions},
};
use super::message::{ActixMessageWrapper, wrap_message, ActixMessageHandler};
use super::actor::StopMessage;
use std::any::Any;
use uuid;
use std::marker::PhantomData;

/// # ActixActorRef
///
/// ## Overview
/// A wrapper around Actix Address to implement the ActorRef trait for the Parrot actor system.
///
/// ## Key Responsibilities
/// - Encapsulates Actix actor address
/// - Provides message passing interface
/// - Manages actor lifecycle
///
/// ## Type Parameters
/// - `A`: The underlying Actix actor type
pub struct ActixActorRef<A>
where
    A: ActixActor + Handler<super::actor::StopMessage>
{
    /// The underlying Actix actor address
    pub addr: Addr<A>,
    /// The actor's path in the system hierarchy
    path: String,
}

impl<A: ActixActor> ActixActorRef<A> {
    /// Creates a new ActixActorRef instance
    ///
    /// ## Parameters
    /// - `addr`: The Actix actor address
    /// - `path`: The actor's path in the system
    ///
    /// ## Returns
    /// A new ActixActorRef instance
    pub fn new(addr: Addr<A>) -> Self {
        let path = ActorPath::new(&format!("/user/{}", addr.id()));
        Self { addr, path }
    }

    /// Creates a message envelope for internal message passing
    ///
    /// ## Parameters
    /// - `msg`: The message to be wrapped
    ///
    /// ## Returns
    /// A MessageEnvelope containing the wrapped message
    fn create_envelope(&self, msg: BoxedMessage) -> MessageEnvelope {
        // 先获取类型名，再移动值
        let message_type = std::any::type_name_of_val(&*msg);
        
        // Create a MessageEnvelope with all required fields
        MessageEnvelope {
            id: uuid::Uuid::new_v4(),
            payload: msg,
            sender: None,
            options: MessageOptions::default(),
            message_type,
        }
    }
    
    /// 发送自定义命令终止actor
    pub fn stop_actor(&self) {
        // 关闭与actor的连接
        self.addr.close();
    }
}

impl<A: ActixActor> Debug for ActixActorRef<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActixActorRef")
            .field("path", &self.path)
            .finish()
    }
}

/// Helper trait for converting between Parrot and Actix addresses
///
/// ## Type Parameters
/// - `A`: The Actix actor type
pub trait AddressConverter<A: ActixActor> {
    /// Converts to an Actix address
    fn to_actix(&self) -> Addr<A>;
    
    /// Creates a boxed actor reference from an Actix address
    fn from_actix(addr: Addr<A>, path: String) -> BoxedActorRef;
}

/// Implementation of ActorRef trait for ActixActorRef
///
/// ## Type Parameters
/// - `A`: The Actix actor type that must implement ActixMessageHandler
#[async_trait]
impl<A> ActorRef for ActixActorRef<A> 
where 
    A: ActixActor + ActixMessageHandler + Handler<ActixMessageWrapper> + Handler<StopMessage> + Send + Sync + 'static,
    <A as ActixActor>::Context: ToEnvelope<A, ActixMessageWrapper>
{
    /// Sends a message to the actor and waits for response
    ///
    /// ## Parameters
    /// - `msg`: The message to send
    ///
    /// ## Returns
    /// A future resolving to the response or error
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let envelope = self.create_envelope(msg);
            
            match self.addr.send(wrap_message(envelope)).await {
                Ok(result) => result,
                Err(e) => Err(ActorError::MessageHandlingError(
                    format!("Failed to send message: {}", e)
                )),
            }
        })
    }

    /// Stops the actor
    ///
    /// ## Returns
    /// A future resolving to success or error
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        // 关闭与actor的连接
        self.addr.close();
        
        Box::pin(async move {
            Ok(())
        })
    }

    /// Returns the actor's path in the system
    fn path(&self) -> String {
        self.path.clone()
    }

    /// Checks if the actor is still alive
    ///
    /// ## Returns
    /// A future resolving to true if the actor is alive
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move {
            !self.addr.connected()
        })
    }

    /// Creates a boxed clone of this actor reference
    ///
    /// ## Returns
    /// A new boxed actor reference
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(ActixActorRef {
            addr: self.addr.clone(),
            path: self.path.clone(),
        })
    }
} 