use std::sync::{Arc, Weak};
use std::time::Duration;
use async_trait::async_trait;
use tokio::sync::oneshot;
use tokio::time::timeout;
use anyhow::Error as AnyhowError;

use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::errors::ActorError;
use parrot_api::types::{BoxedActorRef, BoxedMessage, ActorResult, BoxedFuture};
use crate::thread::error::MailboxError;
use crate::thread::mailbox::Mailbox;
use crate::thread::reply::ThreadReplyChannel;
use crate::thread::envelope::AskEnvelope;
use crate::thread::config::BackpressureStrategy;
use crate::thread::scheduler::WeakSchedulerRef;
use std::any::Any;
use crate::thread::scheduler::ThreadScheduler;
use parrot_api::actor::Actor;
use crate::thread::context::ThreadContext;
use std::marker::PhantomData;
/// Type alias for weak reference to a Mailbox
pub type WeakMailboxRef = Weak<dyn Mailbox + Send + Sync>;
/// Type alias for strong reference to a Mailbox
pub type StrongMailboxRef = Arc<dyn Mailbox + Send + Sync>;

/// Actor reference implementation for the thread-based actor system.
/// 
/// Provides methods to send messages to and interact with actors. Uses weak references
/// to mailboxes to prevent circular dependencies and support lifetime management.
#[derive(Debug)]
pub struct ThreadActorRef<A>
    where A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
 {
    /// The path that uniquely identifies this actor
    path: ActorPath,
    /// Weak reference to the actor's mailbox to avoid circular references
    mailbox: WeakMailboxRef,
    /// Default backpressure strategy to use when sending messages
    default_strategy: BackpressureStrategy,
    /// Default timeout for ask operations
    default_timeout: Duration,
    scheduler: WeakSchedulerRef,
    _marker: PhantomData<A>,
}

impl<A> ThreadActorRef<A>
    where A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
 {
    /// Creates a new ThreadActorRef.
    ///
    /// # Parameters
    /// * `path` - The actor path
    /// * `mailbox` - Weak reference to the actor's mailbox
    /// * `default_strategy` - Default backpressure strategy to use
    /// * `default_timeout` - Default timeout for ask operations
    pub fn new(
        path: ActorPath,
        mailbox: WeakMailboxRef,
        default_strategy: BackpressureStrategy,
        default_timeout: Duration,
        scheduler: Option<WeakSchedulerRef>,
    ) -> Self {
        Self {
            path,
            mailbox,
            default_strategy,
            default_timeout,
            scheduler: scheduler.unwrap_or_else(|| Weak::new()),
            _marker: PhantomData,
        }
    }

    /// Tries to upgrade the weak mailbox reference to a strong reference.
    ///
    /// Returns an error if the mailbox no longer exists (actor is dead).
    fn mailbox(&self) -> Result<StrongMailboxRef, ActorError> {
        self.mailbox.upgrade()
            .ok_or_else(|| {
                let err: ActorError = AnyhowError::msg(format!(
                    "Actor reference is dead (target: {:?})",
                    self.path
                )).into();
                err
            })
    }

    #[inline]
    async fn push_to_mailbox(&self, msg: BoxedMessage, strategy: BackpressureStrategy) -> Result<(), ActorError> {
        let mailbox = self.mailbox()?;
        mailbox.push(msg, strategy).await.map_err(|e| {
            let err: ActorError = AnyhowError::msg(format!(
                "Mailbox error for actor {:?}: {}",
                self.path, e
            )).into();
            err
        })
    }

    #[inline]
    async fn schedule_actor(&self) -> Result<(), ActorError> {
        let mailbox = self.mailbox()?;
        let scheduler = self.scheduler.upgrade();
        if let Some(scheduler) = scheduler {
            scheduler.dedicated_scheduler.schedule::<A>(&self.path.path, mailbox, None).await.unwrap();
        }
        Ok(())
    }

    #[inline]
    async fn push_and_schedule(&self, msg: BoxedMessage, strategy: BackpressureStrategy) -> Result<(), ActorError> {
        self.push_to_mailbox(msg, strategy).await?;
        self.schedule_actor().await?;
        Ok(())
    }

    /// Sends a message to the actor using the specified backpressure strategy.
    ///
    /// # Parameters
    /// * `msg` - The message to send
    /// * `strategy` - The backpressure strategy to use
    ///
    /// # Returns
    /// * `Ok(())` - The message was successfully sent
    /// * `Err(...)` - An error occurred (mailbox full, closed, etc.)
    pub async fn send_with_strategy(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
    ) -> ActorResult<BoxedMessage> {
        self.push_and_schedule(msg, strategy).await?;
        Ok(Box::new(()))
    }

    pub async fn send_with_timeout(
        &self,
        msg: BoxedMessage,
        timeout_duration: Duration,
    ) -> ActorResult<BoxedMessage> {        
        // Use timeout to wrap the mailbox push operation
        match timeout(timeout_duration, async {
            self.push_and_schedule(msg, self.default_strategy.clone()).await?;
            Ok::<(), ActorError>(())
        }).await {
            Ok(result) => {
                result?;
                Ok(Box::new(()))
            },
            Err(_) => {
                let err: ActorError = AnyhowError::msg(format!(
                    "Send timed out after {:?} for actor {}",
                    timeout_duration, self.path
                )).into();
                Err(err)
            }
        }
    }

    /// Sends a message to the actor and expects a reply, with custom strategy and timeout.
    ///
    /// # Parameters
    /// * `msg` - The message to send
    /// * `strategy` - The backpressure strategy to use
    /// * `timeout_duration` - How long to wait for a reply
    ///
    /// # Returns
    /// * `Ok(reply)` - The reply from the actor
    /// * `Err(...)` - An error occurred (timeout, mailbox error, etc.)
    pub async fn ask_with_strategy_and_timeout(
        &self,
        msg: BoxedMessage,
        strategy: BackpressureStrategy,
        timeout_duration: Duration,
    ) -> ActorResult<BoxedMessage> {
        // Create oneshot channel for reply
        let (tx, rx) = oneshot::channel();
        
        // Create reply channel and envelope
        let reply_channel = Box::new(ThreadReplyChannel(tx));
        let envelope = AskEnvelope {
            payload: msg,
            reply: reply_channel,
        };
        
        // Send the envelope as a message
        let mailbox = self.mailbox()?;
        mailbox.push(Box::new(envelope), strategy).await.map_err(|e| {
            let err: ActorError = AnyhowError::msg(format!(
                "Mailbox error for actor {:?}: {}",
                self.path, e
            )).into();
            err
        })?;
        
        // Wait for reply with timeout
        match timeout(timeout_duration, rx).await {
            Ok(reply_result) => {
                match reply_result {
                    Ok(reply) => reply,
                    Err(_) => {
                        let err: ActorError = AnyhowError::msg(format!(
                            "Reply channel closed for ask to {:?}",
                            self.path
                        )).into();
                        Err(err)
                    },
                }
            },
            Err(_) => {
                let err: ActorError = AnyhowError::msg(format!(
                    "Request to actor {:?} timed out after {}ms",
                    self.path, timeout_duration.as_millis()
                )).into();
                Err(err)
            },
        }
    }

    /// Sends a request to the actor and expects a reply, using default strategy and timeout.
    pub async fn ask(&self, msg: BoxedMessage) -> ActorResult<BoxedMessage> {
        self.ask_with_strategy_and_timeout(
            msg,
            self.default_strategy.clone(),
            self.default_timeout,
        ).await
    }

    /// Sends a request to the actor with a custom timeout, using default strategy.
    pub async fn ask_with_timeout(
        &self,
        msg: BoxedMessage,
        timeout_duration: Duration,
    ) -> ActorResult<BoxedMessage> {
        self.ask_with_strategy_and_timeout(
            msg,
            self.default_strategy.clone(),
            timeout_duration,
        ).await
    }
}

#[async_trait]
impl<A> ActorRef for ThreadActorRef<A>
    where A: Actor<Context = ThreadContext<A>> + Send + Sync + std::fmt::Debug + 'static,
 {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.send_with_strategy(msg, self.default_strategy.clone()).await
        })
    }
    
    fn send_with_timeout<'a>(&'a self, msg: BoxedMessage, timeout_duration: Option<Duration>) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        if let Some(duration) = timeout_duration {
            Box::pin(async move {
                self.send_with_timeout(msg, duration).await
            })
        } else {
            Box::pin(async move {
                self.send_with_strategy(msg, self.default_strategy.clone()).await
            })
        }
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            // Get mailbox reference
            let mailbox = self.mailbox()?;
            
            // Close the mailbox
            mailbox.close().await;
            
            // Return success
            Ok::<(), ActorError>(())
        })
    }
    
    fn path(&self) -> String {
        self.path.path.clone()
    }
    
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move {
            self.mailbox.upgrade().is_some()
        })
    }
    
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            path: self.path.clone(),
            mailbox: self.mailbox.clone(),
            default_strategy: self.default_strategy.clone(),
            default_timeout: self.default_timeout,
            scheduler: self.scheduler.clone(),
            _marker: PhantomData,
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl<A> Clone for ThreadActorRef<A>
    where A: Actor<Context = ThreadContext<A>> + Send + Sync + 'static,
 {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            mailbox: self.mailbox.clone(),
            default_strategy: self.default_strategy.clone(),
            default_timeout: self.default_timeout,
            scheduler: self.scheduler.clone(),
            _marker: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::mailbox::mpsc::MpscMailbox;
    use parrot_api::address::ActorPath;
    use std::time::Duration;
    use crate::thread::scheduler::ThreadScheduler;
    use parrot_api::actor::EmptyConfig;
    
    // Helper function to create a test mailbox
    fn create_test_mailbox() -> (Arc<dyn Mailbox + Send + Sync>, ActorPath) {
        // create a mock actor ref to replace ()
        #[derive(Debug)]
        struct MockActorRef;
        
        #[async_trait]
        impl ActorRef for MockActorRef {
            fn send<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async { Ok(Box::new(()) as Box<dyn std::any::Any + Send>) })
            }
            
            fn send_with_timeout<'a>(&'a self, _msg: BoxedMessage, _timeout_duration: Option<Duration>) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async { Ok(Box::new(()) as Box<dyn std::any::Any + Send>) })
            }

            fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async { Ok(()) })
            }
            
            fn path(&self) -> String {
                "mock-actor".to_string()
            }
            
            fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
                Box::pin(async { true })
            }
            
            fn clone_boxed(&self) -> BoxedActorRef {
                Box::new(Self)
            }

            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        
        let path = ActorPath {
            path: "test-actor".to_string(),
            target: Arc::new(MockActorRef) as Arc<dyn ActorRef + 'static>,
        };
        let mailbox = Arc::new(MpscMailbox::new(10, path.clone()));
        (mailbox, path)
    }

    // 添加此结构体作为测试用的Actor
    #[derive(Debug)]
    struct TestActor;
    
    impl Actor for TestActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;
        
        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        
        fn receive_message<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async { Ok(Box::new(()) as Box<dyn Any + Send>) })
        }
        
        fn receive_message_with_engine<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context, _engine_ctx: std::ptr::NonNull<dyn Any>) -> Option<ActorResult<BoxedMessage>> {
            None
        }
        
        fn state(&self) -> parrot_api::actor::ActorState {
            parrot_api::actor::ActorState::Running
        }
    }
    
    #[tokio::test]
    async fn test_send_message() {
        let (mailbox, path) = create_test_mailbox();
        let actor_ref = ThreadActorRef::<TestActor>::new(
            path,
            Arc::downgrade(&mailbox) as WeakMailboxRef,
            BackpressureStrategy::Block,
            Duration::from_secs(1),
            None,
        );

        // Send a simple message
        let message = Box::new("Hello, actor!") as BoxedMessage;
        let result = actor_ref.send(message).await;
        assert!(result.is_ok());

        // Verify message was received
        let received = mailbox.pop().await;
        assert!(received.is_some());

        // Note: In a real test, you'd check the message content
        // but that requires downcast which we'll simplify here
    }

    #[tokio::test]
    async fn test_dead_reference() {
        let (mailbox, path) = create_test_mailbox();
        let actor_ref = ThreadActorRef::<TestActor>::new(
            path,
            Arc::downgrade(&mailbox) as WeakMailboxRef,
            BackpressureStrategy::Block,
            Duration::from_secs(1),
            None,
        );

        // Drop the mailbox to simulate a dead actor
        drop(mailbox);

        // Try to send a message
        let message = Box::new("This should fail") as BoxedMessage;
        let result = actor_ref.send(message).await;
        
        // Verify it failed with error
        assert!(result.is_err());
    }

    // Additional tests for ask, timeout, etc. would be added here
} 