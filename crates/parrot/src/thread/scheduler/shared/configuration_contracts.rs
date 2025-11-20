use super::*;
use crate::thread::{BackpressureStrategy, scheduler::configuration_contracts::stack};

#[tokio::test]
async fn invalid_direct_override_is_rejected_before_ready_registration() {
    let pool = SharedThreadPool::new(
        Some(SharedThreadPoolConfig {
            pool_size: 1,
            burst_workers_max: 0,
            ..Default::default()
        }),
        Handle::current(),
    );
    let (mailbox, processor) = stack("/invalid-shared");
    let result = pool
        .schedule(
            "/invalid-shared",
            mailbox,
            Some(ThreadActorConfig {
                mailbox_capacity: Some(0),
                ..Default::default()
            }),
        )
        .await;
    let published = pool.is_scheduled("/invalid-shared");
    let initialized = crate::thread::ProcessorInterface::is_initialized(&*processor);
    pool.shutdown(5000).await.unwrap();
    assert!(
        matches!(result, Err(SystemError::ConfigError(ref message)) if message.contains("actor.mailbox_capacity"))
    );
    assert!(!published);
    assert!(!initialized);
}

#[tokio::test]
async fn burst_worker_uses_configured_budget_and_yield_and_retires_after_shutdown() {
    for yielding in [false, true] {
        // No core worker can consume the mailbox: this targets the real burst path.
        let pool = SharedThreadPool::new_unchecked_for_test(
            Some(SharedThreadPoolConfig {
                pool_size: 0,
                burst_workers_max: 1,
                burst_backlog_threshold: Duration::ZERO,
                max_messages_per_batch: 3,
                yield_after_each_message: yielding,
                enable_detailed_logging: true,
                ..Default::default()
            }),
            Handle::current(),
        );
        let (mailbox, processor) = stack("/burst-config");
        for value in 0..5_u64 {
            mailbox
                .push(Box::new(value), BackpressureStrategy::Error)
                .await
                .unwrap();
        }
        pool.schedule_now("/burst-config", mailbox, None).unwrap();
        pool.elastic_controller().probe();
        pool.elastic_controller().probe();
        tokio::time::timeout(Duration::from_secs(5), async {
            while processor.values.lock().unwrap().len() < 5 {
                processor.changed.notified().await;
            }
        })
        .await
        .unwrap();
        pool.shutdown(5000).await.unwrap();
        assert_eq!(*processor.values.lock().unwrap(), vec![0, 1, 2, 3, 4]);
        let calls = processor.calls.lock().unwrap();
        assert!(calls.len() >= 2);
        assert!(
            calls
                .iter()
                .all(|(batch, flag)| *batch == 3 && *flag == yielding),
            "{calls:?}"
        );
        assert_eq!(pool.metrics().burst_workers_alive, 0);
        assert_eq!(pool.metrics().active_processors, 0);
    }
}
