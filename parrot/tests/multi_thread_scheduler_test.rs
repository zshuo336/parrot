#[cfg(test)]
mod tests {
    use std::fmt;
    use std::sync::{Arc, Weak, Mutex};
    use std::time::Duration;
    use std::collections::HashMap;
    
    use tokio::runtime::Builder;
    use tokio::sync::mpsc;
    use async_trait::async_trait;
    
    use parrot_api::types::BoxedMessage;
    use parrot_api::address::ActorPath;
    use parrot::thread::scheduler::multi_thread::{MultiThreadScheduler, SystemRef, WorkerConfig};
    use parrot::thread::mailbox::Mailbox;
    
    // Mock implementation of the SystemRef trait for testing
    #[derive(Default)]
    struct MockSystemRef {
        panic_reports: Mutex<Vec<(String, String)>>,
    }
    
    impl SystemRef for MockSystemRef {
        fn find_actor(&self, _path: &str) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
            None // In our test, we don't need to actually find actors
        }
        
        fn handle_worker_panic(&self, error: String, mailbox_path: String) {
            let mut reports = self.panic_reports.lock().unwrap();
            reports.push((error, mailbox_path));
        }
    }
    
    // Mock implementation of the Mailbox trait for testing
    struct MockMailbox {
        path: ActorPath,
        messages: Mutex<Vec<BoxedMessage>>,
        has_messages: bool,
    }
    
    impl MockMailbox {
        fn new(path: &str, has_messages: bool) -> Self {
            Self {
                path: ActorPath { path: path.to_string(), ..Default::default() },
                messages: Mutex::new(vec![]),
                has_messages,
            }
        }
    }
    
    impl fmt::Debug for MockMailbox {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("MockMailbox")
                .field("path", &self.path)
                .finish()
        }
    }
    
    #[async_trait]
    impl Mailbox for MockMailbox {
        async fn push(&self, msg: BoxedMessage, _strategy: parrot::thread::config::BackpressureStrategy) -> Result<(), parrot::thread::error::MailboxError> {
            let mut messages = self.messages.lock().unwrap();
            messages.push(msg);
            Ok(())
        }
        
        async fn pop(&self) -> Option<BoxedMessage> {
            let mut messages = self.messages.lock().unwrap();
            if self.has_messages && messages.len() < 5 {
                // Return a dummy message for testing
                Some(Box::new(()))
            } else {
                None
            }
        }
        
        async fn is_empty(&self) -> bool {
            // For testing, we'll use the has_messages flag to control behavior
            !self.has_messages
        }
        
        async fn signal_ready(&self) {
            // No-op for test
        }
        
        fn path(&self) -> &ActorPath {
            &self.path
        }
        
        fn capacity(&self) -> usize {
            100
        }
        
        async fn len(&self) -> usize {
            let messages = self.messages.lock().unwrap();
            messages.len()
        }
        
        async fn close(&self) {
            // No-op for test
        }
    }
    
    #[tokio::test]
    async fn test_scheduler_creation() {
        // Create a test runtime
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        
        let handle = runtime.handle().clone();
        
        // Create a scheduler with 2 worker threads
        let scheduler = MultiThreadScheduler::new(
            2,
            handle,
            100,
            None,
            None,
        );
        
        // Verify the scheduler was created with the correct pool size
        assert_eq!(scheduler.pool_size(), 2);
        
        // Allow the runtime to work briefly
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Shutdown the scheduler
        scheduler.shutdown(1000).await.unwrap();
        
        // Shutdown the runtime
        runtime.shutdown_timeout(Duration::from_millis(100));
    }
    
    #[tokio::test]
    async fn test_scheduler_mailbox_processing() {
        // Create a test runtime
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        
        let handle = runtime.handle().clone();
        
        // Create a system reference for panic handling
        let system_ref = Arc::new(MockSystemRef::default());
        let system_weak = Arc::downgrade(&system_ref) as Weak<dyn SystemRef + Send + Sync>;
        
        // Create worker config with fast idle sleep for quick testing
        let worker_config = WorkerConfig {
            idle_sleep_duration: Duration::from_millis(1),
            enable_detailed_logging: true,
            ..Default::default()
        };
        
        // Create a scheduler with 2 worker threads
        let scheduler = MultiThreadScheduler::new(
            2,
            handle,
            100,
            Some(system_weak),
            Some(worker_config),
        );
        
        // Create test mailboxes
        let mailbox1 = Arc::new(MockMailbox::new("actor/test1", true)); // Has messages
        let mailbox2 = Arc::new(MockMailbox::new("actor/test2", true)); // Has messages
        
        // Schedule the mailboxes
        scheduler.schedule(mailbox1.clone(), None);
        scheduler.schedule(mailbox2.clone(), None);
        
        // Allow time for processing
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // Shutdown the scheduler
        scheduler.shutdown(1000).await.unwrap();
        
        // Shutdown the runtime
        runtime.shutdown_timeout(Duration::from_millis(100));
    }
    
    #[tokio::test]
    async fn test_scheduler_metrics() {
        // Create a test runtime
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        
        let handle = runtime.handle().clone();
        
        // Create a scheduler
        let scheduler = MultiThreadScheduler::new(
            4, // 4 worker threads
            handle,
            100,
            None,
            None,
        );
        
        // Get metrics and verify values
        let metrics = scheduler.metrics();
        assert_eq!(metrics.pool_size, 4);
        assert_eq!(metrics.is_shutting_down, false);
        
        // Shutdown the scheduler
        scheduler.shutdown(1000).await.unwrap();
        
        // Check metrics again to verify shutdown was recorded
        let metrics = scheduler.metrics();
        assert_eq!(metrics.is_shutting_down, true);
        
        // Shutdown the runtime
        runtime.shutdown_timeout(Duration::from_millis(100));
    }
} 