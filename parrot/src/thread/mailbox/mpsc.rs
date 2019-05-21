use async_trait::async_trait;
use flume::{Receiver, Sender};
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::types::{BoxedMessage, WeakActorTarget, ActorResult, BoxedFuture, BoxedActorRef};
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

use crate::thread::config::BackpressureStrategy;
use crate::thread::error::MailboxError;
use crate::thread::mailbox::Mailbox;

/// Mock implementation of ActorRef for testing
#[derive(Debug)]
struct MockActorRef {
    path_value: String,
}

impl MockActorRef {
    fn new(path: &str) -> Self {
        Self {
            path_value: path.to_string(),
        }
    }
}

#[async_trait]
impl ActorRef for MockActorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            Ok(msg)
        })
    }
    
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            Ok(())
        })
    }
    
    fn path(&self) -> String {
        self.path_value.clone()
    }
    
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move {
            true
        })
    }
    
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            path_value: self.path_value.clone(),
        })
    }
}

/// A multi-producer, single-consumer mailbox implementation using flume.
/// 
/// This mailbox allows multiple senders to send messages to a single consumer,
/// which is typically an actor. It provides FIFO ordering guarantees.
#[derive(Debug)]
pub struct MpscMailbox {
    /// The sending half of the channel
    sender: Sender<BoxedMessage>,
    /// The receiving half of the channel
    receiver: Receiver<BoxedMessage>,
    /// Path of the actor this mailbox belongs to
    path: ActorPath,
    /// Capacity of the mailbox
    capacity: usize,
    /// Flag to indicate if the mailbox has messages and is ready for processing
    is_ready: Arc<AtomicBool>,
    /// Notify mechanism to wake up processors when new messages arrive
    notify: Arc<Notify>,
    /// Flag indicating if this mailbox has been closed
    is_closed: Arc<AtomicBool>,
}

impl MpscMailbox {
    /// Creates a new MpscMailbox with the specified capacity and actor path.
    pub fn new(capacity: usize, path: ActorPath) -> Self {
        let (sender, receiver) = flume::bounded(capacity);
        let is_ready = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let is_closed = Arc::new(AtomicBool::new(false));
        
        Self {
            sender,
            receiver,
            path,
            capacity,
            is_ready,
            notify,
            is_closed,
        }
    }

    /// Creates a clone of the sender that can be used to send messages to this mailbox.
    pub fn sender(&self) -> Sender<BoxedMessage> {
        self.sender.clone()
    }

    /// Returns a reference to the notify mechanism
    pub fn notify_ref(&self) -> Arc<Notify> {
        self.notify.clone()
    }
    
    /// Check if this mailbox is closed
    fn is_closed(&self) -> bool {
        self.is_closed.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Mailbox for MpscMailbox {
    async fn push(&self, msg: BoxedMessage, strategy: BackpressureStrategy) -> Result<(), MailboxError> {
        // Check if mailbox is already closed
        if self.is_closed() {
            return Err(MailboxError::Closed);
        }
        
        match strategy {
            BackpressureStrategy::DropNewest => {
                // Try to send without waiting. If the mailbox is full, drop the message.
                match self.sender.try_send(msg) {
                    Ok(_) => {
                        // Signal that the mailbox has work
                        self.is_ready.store(true, Ordering::SeqCst);
                        self.notify.notify_one();
                        Ok(())
                    },
                    Err(flume::TrySendError::Full(_)) => {
                        // Mailbox is full, drop the message as per strategy
                        Ok(())
                    },
                    Err(flume::TrySendError::Disconnected(_)) => {
                        Err(MailboxError::Closed)
                    }
                }
            },
            BackpressureStrategy::Block => {
                // Block until the message can be sent
                match self.sender.send_async(msg).await {
                    Ok(_) => {
                        // Signal that the mailbox has work
                        self.is_ready.store(true, Ordering::SeqCst);
                        self.notify.notify_one();
                        Ok(())
                    },
                    Err(_) => Err(MailboxError::Closed),
                }
            },
            BackpressureStrategy::Error => {
                // Try to send without waiting. If the mailbox is full, return an error.
                match self.sender.try_send(msg) {
                    Ok(_) => {
                        // Signal that the mailbox has work
                        self.is_ready.store(true, Ordering::SeqCst);
                        self.notify.notify_one();
                        Ok(())
                    },
                    Err(flume::TrySendError::Full(_)) => {
                        Err(MailboxError::Full { capacity: self.capacity })
                    },
                    Err(flume::TrySendError::Disconnected(_)) => {
                        Err(MailboxError::Closed)
                    }
                }
            },
            BackpressureStrategy::DropOldest => {
                // Implementation of DropOldest would require more complex channel management
                // or a custom data structure. For now, we'll use a workaround:
                
                // If the mailbox is full, try to pop the oldest message first
                if self.len().await >= self.capacity {
                    // Try to remove an item from the queue
                    let _ = self.receiver.try_recv();
                }
                
                // Then try to send the new message
                match self.sender.try_send(msg) {
                    Ok(_) => {
                        // Signal that the mailbox has work
                        self.is_ready.store(true, Ordering::SeqCst);
                        self.notify.notify_one();
                        Ok(())
                    },
                    Err(flume::TrySendError::Full(_)) => {
                        // This shouldn't happen as we just made room, but handle just in case
                        Err(MailboxError::PushError("Failed to push message after dropping oldest".to_string()))
                    },
                    Err(flume::TrySendError::Disconnected(_)) => {
                        Err(MailboxError::Closed)
                    }
                }
            },
        }
    }

    async fn pop(&self) -> Option<BoxedMessage> {
        // Reset ready flag before attempting to receive
        self.is_ready.store(false, Ordering::SeqCst);
        
        // Try to receive a message
        match self.receiver.recv_async().await {
            Ok(msg) => {
                // If there are more messages, set ready flag again
                if !self.is_empty().await {
                    self.is_ready.store(true, Ordering::SeqCst);
                    self.notify.notify_one();
                }
                Some(msg)
            },
            Err(_) => None, // Channel is closed or empty
        }
    }

    async fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }

    async fn signal_ready(&self) {
        // Set the ready flag and notify any waiting processors
        self.is_ready.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn path(&self) -> &ActorPath {
        &self.path
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    async fn len(&self) -> usize {
        self.receiver.len()
    }

    async fn close(&self) {
        // Mark the mailbox as closed
        self.is_closed.store(true, Ordering::SeqCst);
        
        // Close the sender to prevent further message sends
        // For flume, simply dropping all senders will close the channel
        // We can create a clone and drop it to simulate closing
        // The real closure happens when all senders are dropped
        drop(self.sender.clone());
        
        // Drain any remaining messages to ensure proper cleanup
        while let Ok(_) = self.receiver.try_recv() {
            // Nothing to do, just drain
        }
        
        // Notify anyone waiting on this mailbox that it's now closed
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::test;
    
    // Helper to create a test ActorPath
    fn create_test_actor_path(path_str: &str) -> ActorPath {
        // Create a mock actor reference for the target
        let mock_ref = MockActorRef::new(path_str);
        
        ActorPath {
            target: Arc::new(mock_ref) as WeakActorTarget,
            path: path_str.to_string(),
        }
    }
    
    #[test]
    async fn test_push_and_pop() {
        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(10, path);
        
        // Push a message
        let message: BoxedMessage = Box::new("test message");
        mailbox.push(message, BackpressureStrategy::Block).await.unwrap();
        
        // Pop the message
        let received = mailbox.pop().await;
        assert!(received.is_some());
        
        if let Some(msg) = received {
            let msg_str = msg.downcast::<&str>().unwrap();
            assert_eq!(*msg_str, "test message");
        }
    }
    
    #[test]
    async fn test_backpressure_drop_newest() {
        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(1, path);
        
        // Fill the mailbox
        let message1: BoxedMessage = Box::new("message 1");
        mailbox.push(message1, BackpressureStrategy::Block).await.unwrap();
        
        // Try to push with drop strategy
        let message2: BoxedMessage = Box::new("message 2");
        let result = mailbox.push(message2, BackpressureStrategy::DropNewest).await;
        
        // Should succeed but the message is dropped
        assert!(result.is_ok());
        
        // Pop should only return the first message
        let received1 = mailbox.pop().await;
        assert!(received1.is_some());
        
        // Mailbox should be empty now
        let received2 = mailbox.pop().await;
        assert!(received2.is_none());
    }
    
    #[test]
    async fn test_backpressure_drop_oldest() {
        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(1, path);
        
        // Fill the mailbox with first message
        let message1: BoxedMessage = Box::new("message 1");
        mailbox.push(message1, BackpressureStrategy::Block).await.unwrap();
        
        // Try to push with DropOldest strategy
        let message2: BoxedMessage = Box::new("message 2");
        let result = mailbox.push(message2, BackpressureStrategy::DropOldest).await;
        
        // Should succeed
        assert!(result.is_ok());
        
        // Pop should return the second message, as the first was dropped
        let received = mailbox.pop().await;
        assert!(received.is_some());
        
        if let Some(msg) = received {
            let msg_str = msg.downcast::<&str>().unwrap();
            assert_eq!(*msg_str, "message 2");
        }
    }
    
    #[test]
    async fn test_backpressure_error() {
        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(1, path);
        
        // Fill the mailbox
        let message1: BoxedMessage = Box::new("message 1");
        mailbox.push(message1, BackpressureStrategy::Block).await.unwrap();
        
        // Try to push with error strategy
        let message2: BoxedMessage = Box::new("message 2");
        let result = mailbox.push(message2, BackpressureStrategy::Error).await;
        
        // Should fail with Full error
        assert!(matches!(result, Err(MailboxError::Full { .. })));
    }
    
    #[test]
    async fn test_signal_ready() {
        let path = create_test_actor_path("test-actor");
        let mailbox = MpscMailbox::new(10, path);
        
        // Check that it's initially not ready
        assert!(!mailbox.is_ready.load(Ordering::SeqCst));
        
        // Signal ready
        mailbox.signal_ready().await;
        
        // Verify the flag is set
        assert!(mailbox.is_ready.load(Ordering::SeqCst));
    }
} 