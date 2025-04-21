#[cfg(test)]
mod tests {
    use std::sync::{Arc, Weak, Mutex};
    use std::time::Duration;
    use async_trait::async_trait;
    
    use tokio::runtime::Builder;
    
    use parrot_api::types::BoxedMessage;
    use parrot_api::address::ActorPath;
    use parrot::thread::mailbox::Mailbox;
    use parrot::thread::config::ThreadActorConfig;
    use parrot::thread::scheduler::dedicated::{DedicatedThreadPool, SystemRef};
    
    // Mock implementation of SystemRef for testing
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
    
    // 创建一个简单的Mock Mailbox实现
    struct MockMailbox {
        path: ActorPath,
        messages: Mutex<Vec<BoxedMessage>>,
        closed: Mutex<bool>,
    }
    
    impl MockMailbox {
        fn new(path: &str) -> Self {
            Self {
                path: ActorPath { path: path.to_string(), ..Default::default() },
                messages: Mutex::new(vec![]),
                closed: Mutex::new(false),
            }
        }
    }
    
    impl std::fmt::Debug for MockMailbox {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MockMailbox")
                .field("path", &self.path)
                .finish()
        }
    }
    
    #[async_trait]
    impl Mailbox for MockMailbox {
        async fn push(&self, msg: BoxedMessage, _strategy: parrot::thread::config::BackpressureStrategy) -> Result<(), parrot::thread::error::MailboxError> {
            let is_closed = *self.closed.lock().unwrap();
            if is_closed {
                return Err(parrot::thread::error::MailboxError::Closed);
            }
            
            let mut messages = self.messages.lock().unwrap();
            messages.push(msg);
            Ok(())
        }
        
        async fn pop(&self) -> Option<BoxedMessage> {
            let is_closed = *self.closed.lock().unwrap();
            if is_closed {
                return None;
            }
            
            let mut messages = self.messages.lock().unwrap();
            if messages.is_empty() {
                return None;
            }
            
            Some(messages.remove(0))
        }
        
        async fn is_empty(&self) -> bool {
            let messages = self.messages.lock().unwrap();
            messages.is_empty()
        }
        
        async fn signal_ready(&self) {
            // No-op for this test implementation
        }
        
        fn path(&self) -> &ActorPath {
            &self.path
        }
        
        fn capacity(&self) -> usize {
            100 // Arbitrary for testing
        }
        
        async fn len(&self) -> usize {
            let messages = self.messages.lock().unwrap();
            messages.len()
        }
        
        async fn close(&self) {
            let mut closed = self.closed.lock().unwrap();
            *closed = true;
        }
    }
    
    #[tokio::test]
    async fn test_dedicated_thread_creation() {
        // 创建一个测试运行时
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        
        let handle = runtime.handle().clone();
        
        // 创建一个系统引用
        let system_ref = Arc::new(MockSystemRef::default());
        let system_weak = Arc::downgrade(&system_ref) as Weak<dyn SystemRef + Send + Sync>;
        
        // 创建一个DedicatedThreadPool
        let pool = DedicatedThreadPool::new(
            handle.clone(),
            Some(system_weak),
            5, // 最多5个专用线程
        );
        
        // 创建一个Actor的Mailbox
        let mailbox = Arc::new(MockMailbox::new("test/actor1"));
        
        // 创建actor配置
        let config = ThreadActorConfig {
            scheduling_mode: Some(parrot::thread::config::SchedulingMode::DedicatedThread),
            ..Default::default()
        };
        
        // 将actor调度到专用线程
        pool.schedule(mailbox.clone(), Some(&config)).unwrap();
        
        // 验证线程已创建
        let count = pool.thread_count().unwrap();
        assert_eq!(count, 1, "应有一个专用线程被创建");
        
        // 检查actor是否有专用线程
        let has_thread = pool.has_dedicated_thread(&mailbox.path()).unwrap();
        assert!(has_thread, "actor应该有专用线程");
        
        // 向mailbox发送测试消息
        let test_message: BoxedMessage = Box::new(42);
        mailbox.push(test_message, parrot::thread::config::BackpressureStrategy::Block).await.unwrap();
        
        // 给一些时间让actor线程处理消息
        tokio::time::sleep(Duration::from_millis(100)).await;
        
        // 验证mailbox被处理(已被清空)
        let is_empty = mailbox.is_empty().await;
        assert!(is_empty, "消息应该被处理");
        
        // 取消actor线程调度
        pool.deschedule(&mailbox.path(), true, Some(1000)).unwrap();
        
        // 验证线程已移除
        let count = pool.thread_count().unwrap();
        assert_eq!(count, 0, "专用线程应该被移除");
        
        // 关闭池并停止运行时
        pool.shutdown(1000).await.unwrap();
        runtime.shutdown_timeout(Duration::from_millis(100));
    }
    
    #[tokio::test]
    async fn test_multiple_dedicated_threads() {
        // 创建一个测试运行时
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        
        let handle = runtime.handle().clone();
        
        // 创建一个DedicatedThreadPool，限制最多3个线程
        let pool = DedicatedThreadPool::new(handle.clone(), None, 3);
        
        // 创建多个Actor的Mailbox
        let mailbox1 = Arc::new(MockMailbox::new("test/actor1"));
        let mailbox2 = Arc::new(MockMailbox::new("test/actor2"));
        let mailbox3 = Arc::new(MockMailbox::new("test/actor3"));
        let mailbox4 = Arc::new(MockMailbox::new("test/actor4")); // 这个不会被创建线程
        
        let config = ThreadActorConfig {
            scheduling_mode: Some(parrot::thread::config::SchedulingMode::DedicatedThread),
            ..Default::default()
        };
        
        // 将3个actor调度到专用线程
        pool.schedule(mailbox1.clone(), Some(&config)).unwrap();
        pool.schedule(mailbox2.clone(), Some(&config)).unwrap();
        pool.schedule(mailbox3.clone(), Some(&config)).unwrap();
        
        // 验证3个线程已创建
        let count = pool.thread_count().unwrap();
        assert_eq!(count, 3, "应有三个专用线程被创建");
        
        // 尝试调度第4个actor，应该失败（超过最大线程数）
        let result = pool.schedule(mailbox4.clone(), Some(&config));
        assert!(result.is_err(), "超过最大线程数限制，应该调度失败");
        
        // 关闭其中一个actor的线程
        pool.deschedule(&mailbox1.path(), false, None).unwrap();
        
        // 验证线程数减少
        let count = pool.thread_count().unwrap();
        assert_eq!(count, 2, "取消调度后应有两个专用线程");
        
        // 现在应该可以调度第4个actor了
        let result = pool.schedule(mailbox4.clone(), Some(&config));
        assert!(result.is_ok(), "现在应该能够调度第4个actor");
        
        // 验证线程数恢复到3
        let count = pool.thread_count().unwrap();
        assert_eq!(count, 3, "再次调度后应有三个专用线程");
        
        // 关闭池并停止运行时
        pool.shutdown(1000).await.unwrap();
        runtime.shutdown_timeout(Duration::from_millis(100));
    }
} 