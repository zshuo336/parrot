use super::*;
use crate::thread::scheduler::configuration_contracts::stack;

#[tokio::test]
async fn configuration_defaults_reach_the_scheduled_worker_and_preserve_explicit_values() {
    for explicit in [false, true] {
        let scheduler = DedicatedThreadScheduler::new(Some(DedicatedThreadConfig {
            idle_sleep_duration: Duration::from_millis(73),
            yield_after_each_message: true,
            default_thread_stack_size: 4 * 1024 * 1024,
            max_messages_per_batch: 7,
            ..Default::default()
        }));
        let (mailbox, processor) = stack("/worker-defaults");
        scheduler
            .schedule_with_processor(
                "/worker-defaults",
                mailbox,
                processor,
                ThreadActorConfig {
                    idle_sleep_duration: explicit.then_some(Duration::ZERO),
                    yield_after_each_message: explicit.then_some(false),
                    thread_stack_size: explicit.then_some(5 * 1024 * 1024),
                    ..Default::default()
                },
            )
            .unwrap();
        let (idle, yielding, stack, batch) = {
            let workers = scheduler.workers.lock().unwrap();
            let worker = &workers["/worker-defaults"];
            (
                worker.config.idle_sleep_duration,
                worker.config.yield_after_each_message,
                worker.config.thread_stack_size,
                worker.batch_size,
            )
        };
        scheduler.shutdown().await.unwrap();
        assert_eq!(
            idle,
            Some(if explicit {
                Duration::ZERO
            } else {
                Duration::from_millis(73)
            })
        );
        assert_eq!(yielding, Some(!explicit));
        assert_eq!(stack, Some(if explicit { 5 } else { 4 } * 1024 * 1024));
        assert_eq!(batch, 7);
    }
}

#[tokio::test]
async fn direct_actor_override_validation_precedes_thread_creation_and_initialization() {
    let scheduler = DedicatedThreadScheduler::new(None);
    let (mailbox, processor) = stack("/bad-direct");
    let result = scheduler.schedule_with_processor(
        "/bad-direct",
        mailbox,
        processor.clone(),
        ThreadActorConfig {
            idle_sleep_duration: Some(Duration::MAX),
            ..Default::default()
        },
    );
    let workers = scheduler.worker_count();
    let initialized = processor.is_initialized();
    let shutdown = scheduler.shutdown().await;
    assert!(
        matches!(result, Err(SystemError::ConfigError(ref message)) if message.contains("actor.idle_sleep_duration"))
    );
    assert_eq!(workers, 0);
    assert!(!initialized);
    shutdown.unwrap();
}

struct Release(Option<std::sync::mpsc::Sender<()>>);
impl Drop for Release {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

async fn cumulative_budget(implicit: bool) {
    let budget = Arc::new(NativeWorkerBudget::with_capacity(1));
    let first = DedicatedThreadScheduler::try_new_with_budget(None, budget.clone()).unwrap();
    let second = DedicatedThreadScheduler::try_new_with_budget(None, budget.clone()).unwrap();
    let (started, ready) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let release = Release(Some(release));
    let expired = Arc::new(AtomicBool::new(false));
    let watchdog = expired.clone();
    let (mailbox, processor) = stack("/first");
    first
        .schedule_with_startup(
            "/first",
            mailbox,
            processor,
            Default::default(),
            move || {
                started.send(()).unwrap();
                if wait.recv_timeout(Duration::from_secs(10)).is_err() {
                    watchdog.store(true, Ordering::SeqCst);
                }
            },
        )
        .unwrap();
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    let (mailbox, processor) = stack("/second");
    let rejected = second
        .schedule_with_processor(
            "/second",
            mailbox.clone(),
            processor.clone(),
            Default::default(),
        )
        .unwrap_err();
    assert!(
        rejected
            .to_string()
            .contains("native worker budget exhausted"),
        "{rejected}"
    );
    assert!(!processor.is_initialized());
    if implicit {
        drop(first);
    } else {
        assert!(matches!(
            first
                .shutdown_until(tokio::time::Instant::now() + Duration::from_millis(20))
                .await,
            Err(SystemError::Timeout(_))
        ));
        assert!(budget.try_reserve().is_err());
        drop(release);
        first.shutdown().await.unwrap();
        return finish_recovered(second, mailbox, processor, expired).await;
    }
    assert!(budget.try_reserve().is_err());
    drop(release);
    finish_recovered(second, mailbox, processor, expired).await;
}

async fn finish_recovered(
    second: DedicatedThreadScheduler,
    mailbox: Arc<crate::thread::mailbox::mpsc::MpscMailbox>,
    processor: Arc<crate::thread::scheduler::configuration_contracts::Probe>,
    expired: Arc<AtomicBool>,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match second.schedule_with_processor(
                "/second",
                mailbox.clone(),
                processor.clone(),
                Default::default(),
            ) {
                Ok(()) => break,
                Err(error) => {
                    assert!(
                        error.to_string().contains("native worker budget exhausted"),
                        "{error}"
                    );
                    tokio::task::yield_now().await;
                }
            }
        }
    })
    .await
    .unwrap();
    mailbox
        .push(
            Box::new(73u64),
            crate::thread::config::BackpressureStrategy::Block,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), processor.changed.notified())
        .await
        .unwrap();
    assert_eq!(*processor.values.lock().unwrap(), vec![73]);
    second.shutdown().await.unwrap();
    assert!(!expired.load(Ordering::SeqCst));
}

#[tokio::test]
async fn native_budget_dedicated_timeout_retains_capacity_until_os_join() {
    cumulative_budget(false).await;
}

#[tokio::test]
async fn native_budget_dedicated_drop_retains_capacity_until_os_join() {
    cumulative_budget(true).await;
}
