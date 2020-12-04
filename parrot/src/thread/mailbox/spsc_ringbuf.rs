use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{Mutex as TokioMutex, Notify};
use async_trait::async_trait;
use ringbuf::{HeapRb, traits::Split, HeapProd, HeapCons};
use ringbuf::traits::{Observer, Producer, Consumer};
use parrot_api::address::{ActorPath, ActorRef};
use parrot_api::types::{BoxedMessage, WeakActorTarget, BoxedActorRef, ActorResult, BoxedFuture};

use crate::thread::config::BackpressureStrategy;
use crate::thread::error::MailboxError;
use crate::thread::mailbox::Mailbox;


/// A single-producer, single-consumer mailbox implementation using ringbuf and tokio::sync::Notify.
/// 
/// This is an implementation of the SPSC pattern optimized for the single-producer case,
/// which is common for dedicated actor mailboxes where only the scheduler sends messages.
/// 
/// # Key Implementation Details
/// - Uses lockless RingBuffer from the ringbuf crate (when accessed without locks)
/// - Provides efficient zero-copy access to the underlying buffer
/// - Uses tokio::sync::Notify for signaling when messages are available
/// - Maintains atomic message count for fast length checking
/// - Assumes strict Single-Producer Single-Consumer (SPSC) usage.
///   - Uses tokio::sync::Mutex for producer/consumer access to comply with the 
///     Mailbox trait's &self methods in an async context, preventing thread blocking.
///     This is a performance compromise compared to a true lock-free approach, 
///     which would require different ownership or trait signatures (&mut self).
pub struct SpscRingbufMailbox {
    /// The producer half of the ringbuf, protected by a Tokio Mutex
    producer: Arc<TokioMutex<HeapProd<BoxedMessage>>>,
    /// The consumer half of the ringbuf, protected by a Tokio Mutex
    consumer: Arc<TokioMutex<HeapCons<BoxedMessage>>>,
    /// Path of the actor this mailbox belongs to
    path: ActorPath,
    /// Capacity of the mailbox
    capacity: usize,
    /// Notify mechanism to wake up processors when new messages arrive
    notify: Arc<Notify>,
    /// Flag indicating if this mailbox is ready for scheduling
    is_ready: Arc<AtomicBool>,
    /// Flag indicating if this mailbox has been closed
    is_closed: Arc<AtomicBool>,
    /// Counter for current messages in the mailbox
    message_count: Arc<AtomicUsize>,
}

impl std::fmt::Debug for SpscRingbufMailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpscRingbufMailbox")
            .field("path", &self.path)
            .field("capacity", &self.capacity)
            .field("is_ready", &self.is_ready)
            .field("is_closed", &self.is_closed)
            .field("message_count", &self.message_count)
            .finish()
    }
}

impl SpscRingbufMailbox {
    /// Creates a new SpscRingbufMailbox with the specified capacity and actor path.
    pub fn new(capacity: usize, path: ActorPath) -> Self {
        // Create a ringbuffer with the specified capacity
        let rb = HeapRb::<BoxedMessage>::new(capacity);
        let (producer, consumer) = rb.split();
        
        Self {
            // Wrap producer and consumer in Tokio Mutexes
            producer: Arc::new(TokioMutex::new(producer)),
            consumer: Arc::new(TokioMutex::new(consumer)),
            path,
            capacity,
            notify: Arc::new(Notify::new()),
            is_ready: Arc::new(AtomicBool::new(false)),
            is_closed: Arc::new(AtomicBool::new(false)),
            message_count: Arc::new(AtomicUsize::new(0)),
        }
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
impl Mailbox for SpscRingbufMailbox {
    async fn push(&self, msg: BoxedMessage, strategy: BackpressureStrategy) -> Result<(), MailboxError> {
        // Check if mailbox is already closed
        if self.is_closed() {
            return Err(MailboxError::Closed);
        }
        
        // Lock the producer asynchronously
        // NOTE: In strict SPSC, this lock should ideally not contend, 
        // but it's needed for &self access in the trait.
        let mut producer = self.producer.lock().await;
        
        match strategy {
            BackpressureStrategy::DropNewest => {
                // Try to push the message, if full, just drop it
                if producer.is_full() {
                    Ok(()) // Drop newest message silently
                } else {
                    match producer.try_push(msg) {
                        Ok(()) => {
                            self.increment_count();
                            self.is_ready.store(true, Ordering::SeqCst);
                            self.notify.notify_one();
                            Ok(())
                        },
                        // Ringbuf push error itself is rare unless types are unpinnable, 
                        // treat as generic push error.
                        Err(_) => Err(MailboxError::PushError("Failed to push message (ringbuf error)".to_string())),
                    }
                }
            },
            BackpressureStrategy::Block => {
                // If buffer is full, we'll need to wait
                if producer.is_full() {
                    // Release the lock and wait for space
                    drop(producer);
                    
                    // Create an async loop to wait for space
                    let mut attempts = 0;
                    while attempts < 100 { // Limit retry attempts
                        // Wait a short time
                        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
                        
                        if self.is_closed() {
                            return Err(MailboxError::Closed);
                        }
                        
                        // Try acquiring lock again (async)
                        let mut p = self.producer.lock().await;
                        
                        if !p.is_full() {
                            match p.try_push(msg) {
                                Ok(()) => {
                                    self.increment_count();
                                    self.is_ready.store(true, Ordering::SeqCst);
                                    self.notify.notify_one();
                                    return Ok(());
                                },
                                Err(_) => return Err(MailboxError::PushError("Failed to push message (ringbuf error)".to_string())),
                            }
                        }
                        
                        // Release lock and try again
                        drop(p);
                        attempts += 1;
                    }
                    
                    // If we've reached max attempts, return an error
                    Err(MailboxError::PushError("Failed to push message after multiple attempts (ringbuf error)".to_string()))
                } else {
                    // There's space, push immediately
                    match producer.try_push(msg) {
                        Ok(()) => {
                            self.increment_count();
                            self.is_ready.store(true, Ordering::SeqCst);
                            self.notify.notify_one();
                            Ok(())
                        },
                        Err(_) => Err(MailboxError::PushError("Failed to push message (ringbuf error)".to_string())),
                    }
                }
            },
            BackpressureStrategy::Error => {
                // If the buffer is full, return an error
                if producer.is_full() {
                    Err(MailboxError::Full { capacity: self.capacity })
                } else {
                    match producer.try_push(msg) {
                        Ok(()) => {
                            self.increment_count();
                            self.is_ready.store(true, Ordering::SeqCst);
                            self.notify.notify_one();
                            Ok(())
                        },
                        Err(_) => Err(MailboxError::PushError("Failed to push message (ringbuf error)".to_string())),
                    }
                }
            },
            BackpressureStrategy::DropOldest => {
                // If buffer is full, pop the oldest item before pushing the new one
                if producer.is_full() {
                    // We need the consumer to pop the oldest item
                    drop(producer);
                    
                    // Acquire consumer lock asynchronously
                    let mut consumer = self.consumer.lock().await;
                    
                    // Try to pop oldest message
                    if !consumer.is_empty() {
                        let _ = consumer.try_pop();
                        self.decrement_count();
                    }
                    
                    // Release consumer lock
                    drop(consumer); // Explicitly drop Tokio MutexGuard
                    
                    // Re-acquire producer lock asynchronously
                    let mut producer = self.producer.lock().await;
                    
                    // Push the new message
                    match producer.try_push(msg) {
                        Ok(()) => {
                            self.increment_count();
                            self.is_ready.store(true, Ordering::SeqCst);
                            self.notify.notify_one();
                            Ok(())
                        },
                        Err(_) => Err(MailboxError::PushError("Failed to push message after dropping oldest (ringbuf error)".to_string())),
                    }
                } else {
                    // There's space, push immediately
                    match producer.try_push(msg) {
                        Ok(()) => {
                            self.increment_count();
                            self.is_ready.store(true, Ordering::SeqCst);
                            self.notify.notify_one();
                            Ok(())
                        },
                        Err(_) => Err(MailboxError::PushError("Failed to push message (ringbuf error)".to_string())),
                    }
                }
            },
        }
    }

    async fn pop(&self) -> Option<BoxedMessage> {
        // Reset ready flag before attempting to receive
        self.is_ready.store(false, Ordering::SeqCst);
        
        // Acquire consumer lock asynchronously
        let mut consumer = self.consumer.lock().await;
        
        // Try to pop a message
        if consumer.is_empty() {
            return None;
        }
        
        match consumer.try_pop() {
            Some(msg) => {
                self.decrement_count();
                
                // If there are more messages, set ready flag again
                if !consumer.is_empty() {
                    self.is_ready.store(true, Ordering::SeqCst);
                    self.notify.notify_one();
                }
                
                Some(msg)
            },
            None => None,
        }
    }

    async fn is_empty(&self) -> bool {
        // Use atomic counter to check if empty without locking
        self.message_count.load(Ordering::SeqCst) == 0
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
        // Use atomic counter for thread-safe length check without locking
        self.message_count.load(Ordering::SeqCst)
    }

    async fn close(&self) {
        // Mark the mailbox as closed
        self.is_closed.store(true, Ordering::SeqCst);
        
        // Drain any remaining messages to ensure proper cleanup
        // Lock consumer asynchronously
        let mut consumer = self.consumer.lock().await;
            
        while let Some(_) = consumer.try_pop() {
            self.decrement_count();
        }
        // Tokio MutexGuard dropped automatically here
        
        // Notify anyone waiting on this mailbox that it's now closed
        self.notify.notify_waiters();
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
        let mailbox = SpscRingbufMailbox::new(10, path);
        
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
        let mailbox = SpscRingbufMailbox::new(1, path);
        
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
        let mailbox = SpscRingbufMailbox::new(1, path);
        
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
        let mailbox = SpscRingbufMailbox::new(1, path);
        
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
    
    #[test]
    async fn test_close() {
        let path = create_test_actor_path("test-actor");
        let mailbox = SpscRingbufMailbox::new(10, path);
        
        // Push some messages
        for i in 0..5 {
            let msg = Box::new(format!("Message {}", i)) as BoxedMessage;
            mailbox.push(msg, BackpressureStrategy::Block).await.unwrap();
        }
        
        // Close the mailbox
        mailbox.close().await;
        
        // Verify the mailbox is closed
        assert!(mailbox.is_closed());
        
        // Try to push to a closed mailbox
        let msg = Box::new("This should fail") as BoxedMessage;
        let result = mailbox.push(msg, BackpressureStrategy::Block).await;
        assert!(matches!(result, Err(MailboxError::Closed)));
        
        // Verify all messages have been drained
        assert_eq!(mailbox.len().await, 0);
    }
} 