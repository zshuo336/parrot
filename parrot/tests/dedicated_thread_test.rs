//! Integration tests for the dedicated-thread scheduler.
//!
//! The historical `dedicated` pool has been consolidated into
//! `parrot::thread::scheduler::dedicated_thread`.

mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
    use parrot::thread::actor::ThreadActor;
    use parrot::thread::config::{SchedulingMode, ThreadActorConfig};
    use parrot::thread::context::ThreadContext;
    use parrot::thread::mailbox::mpsc::MpscMailbox;
    use parrot::thread::mailbox::Mailbox;
    use parrot::thread::processor::{ActorProcessor, ProcessorInterface};
    use parrot::thread::scheduler::dedicated_thread::{
        DedicatedThreadConfig, DedicatedThreadScheduler, TypedThreadSchedulerExt,
    };
    use std::any::Any;

    /// A simple counting actor used by these tests.
    #[derive(Debug)]
    struct CountingActor {
        count: u64,
    }

    impl Actor for CountingActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(&'a mut self, msg: BoxedMessage, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if let Some(n) = msg.downcast_ref::<u64>() {
                    self.count += *n;
                    return Ok(Box::new(self.count) as BoxedMessage);
                }
                Ok(msg)
            })
        }

        fn receive_message_with_engine<'a>(&'a mut self, _msg: BoxedMessage, _ctx: &'a mut Self::Context, _engine_ctx: parrot_api::actor::EngineContextHandle) -> Option<ActorResult<BoxedMessage>> {
            None
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    fn build_stack(path: &str) -> Arc<MpscMailbox> {
        let actor_path = parrot_api::address::ActorPath::placeholder(path);
        let mailbox = Arc::new(MpscMailbox::new(64, actor_path));
        let context = ThreadContext::<CountingActor>::new_for_test(path);
        let processor = Arc::new(ActorProcessor::<CountingActor>::new(
            ThreadActor::new_for_test(CountingActor { count: 0 }),
            context,
            path.to_string(),
            ThreadActorConfig::default(),
        ));
        mailbox.set_processor(processor);
        mailbox
    }

    async fn wait_until_empty(mailbox: &Arc<MpscMailbox>) {
        for _ in 0..200 {
            if mailbox.is_empty().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_dedicated_thread_creation_and_message_processing() {
        let scheduler = DedicatedThreadScheduler::new(Some(DedicatedThreadConfig {
            max_threads: 2,
            idle_sleep_duration: Duration::from_millis(5),
            ..Default::default()
        }));

        let mailbox = build_stack("test/dedicated/actor1");

        scheduler
            .schedule_typed_by_processor::<CountingActor>(
                "test/dedicated/actor1",
                mailbox.clone(),
                mailbox.get_processor().unwrap(),
                ThreadActorConfig {
                    scheduling_mode: Some(SchedulingMode::DedicatedThread),
                    ..Default::default()
                },
            )
            .unwrap();

        // One thread created
        assert_eq!(scheduler.worker_count(), 1);
        assert!(scheduler.is_scheduled("test/dedicated/actor1"));

        // Push messages; the dedicated thread should drain them.
        for value in 1..=5u64 {
            mailbox
                .push(Box::new(value) as BoxedMessage, parrot::thread::config::BackpressureStrategy::Block)
                .await
                .unwrap();
        }

        wait_until_empty(&mailbox).await;
        assert!(mailbox.is_empty().await, "dedicated thread should drain the mailbox");

        // Deschedule: thread removed
        scheduler.deschedule("test/dedicated/actor1").await.unwrap();
        assert_eq!(scheduler.worker_count(), 0);

        scheduler.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_dedicated_threads_and_limit() {
        let scheduler = DedicatedThreadScheduler::new(Some(DedicatedThreadConfig {
            max_threads: 3,
            idle_sleep_duration: Duration::from_millis(5),
            ..Default::default()
        }));

        let m1 = build_stack("test/multi/a");
        let m2 = build_stack("test/multi/b");
        let m3 = build_stack("test/multi/c");
        let m4 = build_stack("test/multi/d");

        for (path, mailbox) in [
            ("test/multi/a", m1.clone()),
            ("test/mmulti/b", m2.clone()),
            ("test/multi/c", m3.clone()),
        ] {
            scheduler
                .schedule_typed_by_processor::<CountingActor>(
                    path,
                    mailbox.clone(),
                    mailbox.get_processor().unwrap(),
                    ThreadActorConfig::default(),
                )
                .unwrap();
        }
        assert_eq!(scheduler.worker_count(), 3);

        // Fourth actor exceeds the limit.
        let result = scheduler.schedule_typed_by_processor::<CountingActor>(
            "test/multi/d",
            m4.clone(),
            m4.get_processor().unwrap(),
            ThreadActorConfig::default(),
        );
        assert!(result.is_err(), "thread limit must be enforced");

        // Deschedule one; now the fourth fits.
        scheduler.deschedule("test/multi/a").await.unwrap();
        assert_eq!(scheduler.worker_count(), 2);

        scheduler
            .schedule_typed_by_processor::<CountingActor>(
                "test/multi/d",
                m4.clone(),
                m4.get_processor().unwrap(),
                ThreadActorConfig::default(),
            )
            .unwrap();
        assert_eq!(scheduler.worker_count(), 3);

        scheduler.shutdown().await.unwrap();
    }
}
