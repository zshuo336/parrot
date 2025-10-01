//! Integration tests for the shared (multi-thread) scheduler pool.
//!
//! The historical `multi_thread` scheduler module has been consolidated into
//! `parrot::thread::scheduler::shared`; these tests exercise the consolidated
//! pool behavior.

mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use parrot::thread::mailbox::mpsc::MpscMailbox;
    use parrot::thread::mailbox::Mailbox;
    use parrot::thread::scheduler::shared::SharedThreadPool;
    use parrot::thread::scheduler::shared::SharedThreadPoolConfig;
    use parrot_api::types::BoxedMessage;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_scheduler_creation() {
        let scheduler = SharedThreadPool::new(
            Some(SharedThreadPoolConfig {
                pool_size: 2,
                ..Default::default()
            }),
            tokio::runtime::Handle::current(),
        );

        assert_eq!(scheduler.pool_size(), 2);
        assert_eq!(scheduler.status(), parrot::thread::scheduler::shared::SchedulerStatus::Running);

        scheduler.shutdown(1000).await.unwrap();
        assert_eq!(scheduler.status(), parrot::thread::scheduler::shared::SchedulerStatus::Shutdown);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_scheduler_rejects_mailbox_without_processor() {
        let scheduler = SharedThreadPool::new(
            Some(SharedThreadPoolConfig::default()),
            tokio::runtime::Handle::current(),
        );

        let path = parrot_api::address::ActorPath::placeholder("test/no-processor");
        let mailbox: Arc<dyn Mailbox> = Arc::new(MpscMailbox::new(8, path));

        // Scheduling a mailbox without an attached processor must fail.
        let result = scheduler
            .schedule("test/no-processor", mailbox, None)
            .await;
        assert!(result.is_err());

        scheduler.shutdown(1000).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_deschedule_unknown_actor_errors() {
        let scheduler = SharedThreadPool::new(
            Some(SharedThreadPoolConfig::default()),
            tokio::runtime::Handle::current(),
        );

        let result = scheduler.deschedule("does/not/exist").await;
        assert!(matches!(
            result,
            Err(parrot::thread::error::SystemError::ActorNotFound(_))
        ));

        scheduler.shutdown(1000).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_broadcast_drop_push_works() {
        // Sanity check that the shared-pool mailbox handles DropNewest under
        // load without error (basic liveness of the queue path).
        let path = parrot_api::address::ActorPath::placeholder("test/drop");
        let mailbox = MpscMailbox::new(2, path);

        for i in 0..10 {
            let msg: BoxedMessage = Box::new(i);
            mailbox
                .push(msg, parrot::thread::config::BackpressureStrategy::DropNewest)
                .await
                .unwrap();
        }

        // Capacity bounded
        assert!(mailbox.len().await <= 2);
    }
}
