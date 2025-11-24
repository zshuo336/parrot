//! Failed control submission must still await the actual disposal receipt.
use super::*;
use crate::thread::processor::{ProcessorStats, ProcessorStatsTrait};
use parrot_api::types::BoxedFuture;
use std::{
    any::Any,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

struct Processor {
    cancelled: AtomicUsize,
    waited: AtomicUsize,
    constructed: AtomicUsize,
    retired: tokio::sync::watch::Sender<bool>,
    fails: bool,
}
impl ProcessorStatsTrait for Processor {
    fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
        None
    }
}
impl ProcessorInterface for Processor {
    fn is_initialized(&self) -> bool {
        true
    }
    fn initialize_and_start_erased(
        self: Arc<Self>,
    ) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
    fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
        self.constructed.fetch_add(1, Ordering::SeqCst);
        panic!("rejected stop must never construct user work");
    }
    fn retire_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
        self.constructed.fetch_add(1, Ordering::SeqCst);
        panic!("rejected retirement must never construct user work");
    }
    fn retirement(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async move {
            self.waited.fetch_add(1, Ordering::SeqCst);
            let mut retired = self.retired.subscribe();
            while !*retired.borrow_and_update() {
                retired.changed().await.unwrap();
            }
            if self.fails {
                Err(SystemError::Panic(
                    "original storage disposal failure".into(),
                ))
            } else {
                Ok(())
            }
        })
    }
    fn cancel_processing(&self) {
        self.cancelled.fetch_add(1, Ordering::SeqCst);
    }
    fn process_batch_erased(
        self: Arc<Self>,
        _: Arc<dyn Mailbox>,
        _: usize,
        _: bool,
    ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
        Box::pin(async { Ok((0, 0)) })
    }
    fn as_any(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

async fn exercise(sharded: bool) {
    let group = ThreadSchedulerFactory::new(Handle::current()).create_scheduler_group(
        Some(shared::SharedThreadPoolConfig {
            pool_size: 1,
            burst_workers_max: 0,
            ..Default::default()
        }),
        None,
    );
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let mode = if sharded {
        let scheduler = ShardedScheduler::try_start(1, 32).unwrap();
        scheduler
            .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(3))
            .await
            .unwrap();
        *group.sharded_scheduler.lock().unwrap() = Some(scheduler);
        SchedulingMode::Sharded {
            affinity_key: "retirement-diagnostic".into(),
        }
    } else {
        slots.close();
        SchedulingMode::SharedPool {
            max_messages_per_run: 1,
        }
    };
    for fails in [false, true] {
        let processor = Arc::new(Processor {
            cancelled: AtomicUsize::new(0),
            waited: AtomicUsize::new(0),
            constructed: AtomicUsize::new(0),
            retired: tokio::sync::watch::channel(false).0,
            fails,
        });
        let retirement = group.retire_actor(
            "/retirement-diagnostic",
            &mode,
            processor.clone(),
            slots.clone(),
            tokio::time::Instant::now() + Duration::from_secs(3),
        );
        tokio::pin!(retirement);
        assert!(
            futures::poll!(retirement.as_mut()).is_pending(),
            "submission failure is not a disposal receipt"
        );
        assert_eq!(processor.waited.load(Ordering::SeqCst), 1);
        assert_eq!(processor.cancelled.load(Ordering::SeqCst), 1);
        assert_eq!(processor.constructed.load(Ordering::SeqCst), 0);
        processor.retired.send_replace(true);
        let result = retirement.await;
        if fails {
            assert!(matches!(result, Err(SystemError::Panic(ref text))
                if text == "original storage disposal failure"));
        } else {
            result.unwrap();
        }
        assert_eq!(processor.constructed.load(Ordering::SeqCst), 0);
    }
    group.shared_scheduler.shutdown(3000).await.unwrap();
}

async fn check(name: &str, sharded: bool) {
    if std::env::var_os("PARROT_NOTIFICATION_LOGGING_CHILD").is_none() {
        exercise(sharded).await;
        eprintln!("retirement normal control complete: {name}");
    }
    super::notification_contracts::isolated_async(name, exercise(sharded), 2).await;
}

#[tokio::test]
async fn retirement_shared_submission_diagnostic_keeps_actual_receipt() {
    check(
        "retirement_shared_submission_diagnostic_keeps_actual_receipt",
        false,
    )
    .await;
}

#[tokio::test]
async fn retirement_sharded_submission_diagnostic_keeps_actual_receipt() {
    check(
        "retirement_sharded_submission_diagnostic_keeps_actual_receipt",
        true,
    )
    .await;
}

struct ActorDrop(Arc<AtomicUsize>);
impl Drop for ActorDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl parrot_api::actor::Actor for ActorDrop {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = crate::thread::ThreadContext<Self>;
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: parrot_api::types::BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}

async fn public_stop() {
    use parrot_api::address::{ActorRef, RetirementStatus};
    let system = crate::thread::ThreadActorSystem::shared(crate::thread::ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        default_scheduling_mode: SchedulingMode::Sharded {
            affinity_key: "retirement-diagnostic".into(),
        },
        shutdown_timeout: Duration::from_secs(3),
        ..Default::default()
    });
    let scheduler = ShardedScheduler::try_start(1, 32).unwrap();
    *system.scheduler_group().sharded_scheduler.lock().unwrap() = Some(scheduler.clone());
    let drops = Arc::new(AtomicUsize::new(0));
    let actor = system
        .spawn_at(
            ActorDrop(drops.clone()),
            "/retirement-diagnostic",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    actor.send(Box::new(31_u32)).await.unwrap();
    let processor = system
        .get_mailbox("/retirement-diagnostic")
        .unwrap()
        .get_processor()
        .unwrap();
    // Retire actual storage on its affinity thread before closing the shard.
    // An idle shard's shutdown alone does not retire registered actor storage.
    let (completion, result) = tokio::sync::oneshot::channel();
    scheduler
        .enqueue_stop_hook_until(
            "retirement-diagnostic",
            StopHookRequest {
                trace_context: None,
                path: "/retirement-diagnostic".into(),
                processor,
                deadline: tokio::time::Instant::now() + Duration::from_secs(3),
                completion,
                retire_only: true,
            },
        )
        .await
        .unwrap();
    result.await.unwrap().unwrap();
    scheduler
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let result = actor.stop().await;
    assert!(
        matches!(&result, Err(parrot_api::errors::ActorError::Other(error))
        if error.to_string() == "Actor system is already shutting down"),
        "{result:?}"
    );
    assert!(matches!(
        actor.retirement_status(),
        RetirementStatus::Retired(Err(_))
    ));
    assert_eq!(format!("{result:?}"), format!("{:?}", actor.stop().await));
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retirement_public_sharded_stop_preserves_terminal_receipt() {
    let name = "retirement_public_sharded_stop_preserves_terminal_receipt";
    if std::env::var_os("PARROT_NOTIFICATION_LOGGING_CHILD").is_none() {
        public_stop().await;
        eprintln!("retirement normal control complete: {name}");
    }
    super::notification_contracts::isolated_async(name, public_stop(), 1).await;
}
