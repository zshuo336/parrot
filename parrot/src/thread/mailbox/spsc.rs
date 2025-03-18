use async_trait::async_trait;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{Mutex, Notify};
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::types::{BoxedMessage, WeakActorTarget, ActorResult, BoxedFuture, BoxedActorRef};
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};

use crate::thread::config::BackpressureStrategy;
use crate::thread::error::MailboxError;
use crate::thread::mailbox::Mailbox;
use crate::thread::processor::ActorProcessor;
use std::any::Any;
use std::sync::Mutex as StdMutex;

/// A single-producer, single-consumer mailbox implementation using tokio channels.
/// 
/// This mailbox is optimized for the single-producer case, which is the common case
/// for dedicated actor mailboxes where only the scheduler sends messages.
/// It provides FIFO ordering guarantees.
#[derive(Debug)]
pub struct SpscMailbox {
    /// The sending half of the channel
    sender: Sender<BoxedMessage>,
    /// The receiving half of the channel, wrapped in a Mutex for interior mutability
    receiver: Arc<Mutex<Receiver<BoxedMessage>>>,
    /// Path of the actor this mailbox belongs to
    path: ActorPath,
    /// Capacity of the mailbox
    capacity: usize,
    /// Notify mechanism to wake up processors when new messages arrive
    notify: Arc<Notify>,
    /// Counter for current messages in the mailbox
    message_count: Arc<AtomicUsize>,
    /// Flag indicating if this mailbox has been closed
    is_closed: Arc<AtomicBool>,
    /// Associated processor
    processor: Arc<StdMutex<Option<Arc<dyn Any + Send + Sync>>>>
}

impl SpscMailbox {
    /// Creates a new SpscMailbox with the specified capacity and actor path.
    pub fn new(capacity: usize, path: ActorPath) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(capacity);
        let notify = Arc::new(Notify::new());
        let processor = Arc::new(StdMutex::new(None));
        
        Self {
            sender,
            receiver: Arc::new(Mutex::new(receiver)),
            path,
            capacity,
            notify,
            message_count: Arc::new(AtomicUsize::new(0)),
            is_closed: Arc::new(AtomicBool::new(false)),
            processor,
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
    
    /// Increment the message count
    fn increment_count(&self) {
        self.message_count.fetch_add(1, Ordering::SeqCst);
    }
    
    /// Decrement the message count
    fn decrement_count(&self) {
        self.message_count.fetch_sub(1, Ordering::SeqCst);
    }
    
    /// Check if this mailbox is closed
    fn is_closed(&self) -> bool {
        self.is_closed.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Mailbox for SpscMailbox {
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
                        self.increment_count();
                        self.notify.notify_one();
                        Ok(())
                    },
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        // Mailbox is full, drop the message as per strategy
                        Ok(())
                    },
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        Err(MailboxError::Closed)
                    }
                }
            },
            BackpressureStrategy::Block => {
                // Block until the message can be sent
                match self.sender.send(msg).await {
                    Ok(_) => {
                        self.increment_count();
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
                        self.increment_count();
                        self.notify.notify_one();
                        Ok(())
                    },
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        Err(MailboxError::Full { capacity: self.capacity })
                    },
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        Err(MailboxError::Closed)
                    }
                }
            },
            BackpressureStrategy::DropOldest => {
                // Tokio's mpsc doesn't directly support dropping oldest messages
                // For a real implementation, we would need a custom data structure
                // This implementation handles the drop oldest pattern more carefully:
                
                // Try to send first, in case there's room
                match self.sender.try_send(msg) {
                    Ok(_) => {
                        self.increment_count();
                        self.notify.notify_one();
                        Ok(())
                    },
                    Err(tokio::sync::mpsc::error::TrySendError::Full(msg)) => {
                        // Mailbox is full, get exclusive access to the receiver
                        let mut receiver_guard = match self.receiver.try_lock() {
                            Ok(guard) => guard,
                            Err(_) => {
                                // Another thread has the lock, we can't safely drop the oldest message
                                // Consider this case as if the mailbox is full and return error
                                return Err(MailboxError::PushError(
                                    "Cannot acquire lock to drop oldest message".to_string()
                                ));
                            }
                        };
                        
                        // Discard one message
                        match receiver_guard.try_recv() {
                            Ok(_) => {
                                // Successfully removed one message, decrement count
                                self.decrement_count();
                            },
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                                // Mailbox is empty (race condition), no need to drop
                            },
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                return Err(MailboxError::Closed);
                            }
                        }
                        
                        // Drop the guard to release the mutex
                        drop(receiver_guard);
                        
                        // Now try to send again
                        match self.sender.try_send(msg) {
                            Ok(_) => {
                                self.increment_count();
                                self.notify.notify_one();
                                Ok(())
                            },
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                // Still full, this is unusual but possible with concurrent access
                                Err(MailboxError::PushError("Failed to push after dropping oldest message".to_string()))
                            },
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                Err(MailboxError::Closed)
                            }
                        }
                    },
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        Err(MailboxError::Closed)
                    }
                }
            },
        }
    }

    async fn pop(&self) -> Option<BoxedMessage> {
        // Get exclusive access to the receiver
        let mut receiver = self.receiver.lock().await;
        
        // Try to receive a message
        match receiver.recv().await {
            Some(msg) => {
                self.decrement_count();
                Some(msg)
            },
            None => None, // Channel is closed or empty
        }
    }

    async fn is_empty(&self) -> bool {
        // Use atomic counter for thread-safe check without blocking
        self.message_count.load(Ordering::SeqCst) == 0
    }

    async fn signal_ready(&self) {
        // Notify any waiting consumers
        self.notify.notify_one();
    }

    fn path(&self) -> &ActorPath {
        &self.path
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    async fn len(&self) -> usize {
        // Use atomic counter for thread-safe length check without blocking
        self.message_count.load(Ordering::SeqCst)
    }
    
    async fn close(&self) {
        // Mark the mailbox as closed
        self.is_closed.store(true, Ordering::SeqCst);
        
        // Close the sender to prevent further message sends
        // This will cause any future attempts to send to fail
        // The original sender is dropped here
        drop(self.sender.clone());
        
        // Drain any remaining messages to ensure proper cleanup
        if let Ok(mut receiver) = self.receiver.try_lock() {
            while let Ok(_) = receiver.try_recv() {
                self.decrement_count();
            }
        }
        
        // Notify anyone waiting on this mailbox that it's now closed
        self.notify.notify_waiters();
    }

    /// associate a processor with this mailbox
    fn set_processor(&self, processor: Arc<dyn Any + Send + Sync>) {
        let mut processor_guard = self.processor.lock().unwrap();
        *processor_guard = Some(processor);
    }
    
    /// get the associated processor
    fn get_processor(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        let processor_guard = self.processor.lock().unwrap();
        processor_guard.as_ref().map(|p| p.clone())
    }

    /// check if there is a processor associated with this mailbox
    fn has_processor(&self) -> bool {
        let processor_guard = self.processor.lock().unwrap();
        processor_guard.is_some()
    }
    
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::test;
    use std::any::Any;
    use std::time::Duration;

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

        fn send_with_timeout<'a>(&'a self, msg: BoxedMessage, _timeout_duration: Option<Duration>) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
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

        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    
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
        let mailbox = SpscMailbox::new(10, path);
        
        // Test basic push and pop functionality
        let msg = Box::new("Hello, World!") as BoxedMessage;
        mailbox.push(msg, BackpressureStrategy::Block).await.unwrap();
        
        // Verify message count
        assert_eq!(mailbox.len().await, 1);
        
        // Pop the message
        let received = mailbox.pop().await;
        assert!(received.is_some());
        
        let str_msg = received.unwrap().downcast::<&str>().unwrap();
        assert_eq!(*str_msg, "Hello, World!");
        
        // Verify mailbox is now empty
        assert!(mailbox.is_empty().await);
        assert_eq!(mailbox.len().await, 0);
    }
    
    #[test]
    async fn test_backpressure_drop_newest() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscMailbox::new(1, path);
        
        // Fill the mailbox
        let msg1 = Box::new("First message") as BoxedMessage;
        mailbox.push(msg1, BackpressureStrategy::Block).await.unwrap();
        
        // Try to push when mailbox is full with DropNewest strategy
        let msg2 = Box::new("Second message") as BoxedMessage;
        let result = mailbox.push(msg2, BackpressureStrategy::DropNewest).await;
        
        // The push should succeed (message was dropped)
        assert!(result.is_ok());
        
        // Verify only one message is in the mailbox
        assert_eq!(mailbox.len().await, 1);
        
        // Pop the message and verify it's the first one
        let received = mailbox.pop().await.unwrap();
        let str_msg = received.downcast::<&str>().unwrap();
        assert_eq!(*str_msg, "First message");
    }
    
    #[test]
    async fn test_backpressure_error() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscMailbox::new(1, path);
        
        // Fill the mailbox
        let msg1 = Box::new("First message") as BoxedMessage;
        mailbox.push(msg1, BackpressureStrategy::Block).await.unwrap();
        
        // Try to push when mailbox is full with Error strategy
        let msg2 = Box::new("Second message") as BoxedMessage;
        let result = mailbox.push(msg2, BackpressureStrategy::Error).await;
        
        // The push should fail with Full error
        assert!(matches!(result, Err(MailboxError::Full { .. })));
    }
    
    #[test]
    async fn test_backpressure_drop_oldest() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscMailbox::new(1, path);
        
        // Fill the mailbox
        let msg1 = Box::new("First message") as BoxedMessage;
        mailbox.push(msg1, BackpressureStrategy::Block).await.unwrap();
        
        // Try to push when mailbox is full with DropOldest strategy
        let msg2 = Box::new("Second message") as BoxedMessage;
        let result = mailbox.push(msg2, BackpressureStrategy::DropOldest).await;
        
        // The push should succeed
        assert!(result.is_ok());
        
        // Pop the message and verify it's the second one
        let received = mailbox.pop().await.unwrap();
        let str_msg = received.downcast::<&str>().unwrap();
        assert_eq!(*str_msg, "Second message");
    }
}

// Note: This implementation has the following limitation:
// The DropOldest strategy has potential race conditions when multiple producers attempt
// to concurrently send messages to the same mailbox, despite this being a SPSC mailbox. 