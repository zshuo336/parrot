use parrot::thread::{
    ActorProcessor, BackpressureStrategy, ThreadActor, ThreadActorConfig, ThreadContext,
    mailbox::{Mailbox, mpsc::MpscMailbox},
    scheduler::{
        dedicated_thread::{DedicatedThreadConfig, DedicatedThreadScheduler},
        queue::SchedulingQueue,
        sharded::ShardedScheduler,
        shared::{SharedThreadPool, SharedThreadPoolConfig, Worker, WorkerConfig},
    },
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[test]
fn legacy_shared_constructor_rejects_zero_core_workers() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let shared = std::panic::catch_unwind(|| {
        SharedThreadPool::new(
            Some(SharedThreadPoolConfig {
                pool_size: 0,
                ..Default::default()
            }),
            runtime.handle().clone(),
        )
    });
    if let Ok(pool) = shared {
        runtime.block_on(pool.shutdown(0)).unwrap();
        panic!("zero core pool accepted");
    }
}

#[test]
fn legacy_dedicated_constructor_rejects_zero_batch() {
    assert!(
        std::panic::catch_unwind(
            || DedicatedThreadScheduler::new(Some(DedicatedThreadConfig {
                max_messages_per_batch: 0,
                ..Default::default()
            }))
        )
        .is_err()
    );
}

fn legacy_shard_input(n: usize, batch: usize) {
    let result = std::panic::catch_unwind(|| ShardedScheduler::start(n, batch));
    if let Ok(scheduler) = result {
        scheduler.shutdown();
        panic!("invalid shard input accepted");
    }
}

#[test]
fn legacy_sharded_constructor_rejects_zero_threads() {
    legacy_shard_input(0, 1);
}

#[test]
fn legacy_sharded_constructor_rejects_zero_batch() {
    legacy_shard_input(1, 0);
}

#[test]
fn legacy_worker_constructor_rejects_zero_batch() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        std::panic::catch_unwind(|| Worker::new(
            0,
            runtime.handle().clone(),
            Arc::new(SchedulingQueue::new(1)),
            Arc::new(AtomicBool::new(false)),
            WorkerConfig {
                batch_size: 0,
                ..Default::default()
            }
        ))
        .is_err()
    );
}

struct BatchProbe {
    batches: Arc<Mutex<Vec<usize>>>,
    seen: Arc<Mutex<Vec<u64>>>,
    ready: Arc<tokio::sync::Notify>,
    drops: Arc<AtomicUsize>,
    #[cfg(target_os = "linux")]
    stack: Arc<AtomicUsize>,
}
impl Drop for BatchProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for BatchProbe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        #[cfg(target_os = "linux")]
        {
            // Query this actual dedicated pthread; no stack recursion or allocation probe.
            let size = unsafe {
                let mut attributes = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
                assert_eq!(
                    libc::pthread_getattr_np(libc::pthread_self(), attributes.as_mut_ptr()),
                    0
                );
                let mut attributes = attributes.assume_init();
                let mut size = 0;
                let result = libc::pthread_attr_getstacksize(&attributes, &mut size);
                assert_eq!(libc::pthread_attr_destroy(&mut attributes), 0);
                assert_eq!(result, 0);
                size
            };
            self.stack.store(size, Ordering::SeqCst);
        }
        Box::pin(async { Ok(()) })
    }
    fn can_batch_message(&self, _: &BoxedMessage) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.seen
            .lock()
            .unwrap()
            .push(*message.downcast_ref::<u64>().unwrap());
        self.ready.notify_one();
        Box::pin(async { Ok(message) })
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        self.batches.lock().unwrap().push(messages.len());
        Box::pin(async move {
            let mut results = Vec::new();
            for message in messages {
                results.push(self.receive_message(message, context).await);
            }
            results
        })
    }
}

async fn dedicated_behavior(explicit: bool) {
    let batches = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(tokio::sync::Notify::new());
    #[cfg(target_os = "linux")]
    let stack = Arc::new(AtomicUsize::new(0));
    let scheduler = DedicatedThreadScheduler::new(Some(DedicatedThreadConfig {
        yield_after_each_message: true,
        default_thread_stack_size: 16 * 1024 * 1024,
        max_messages_per_batch: 4,
        idle_sleep_duration: Duration::ZERO,
        ..Default::default()
    }));
    let mailbox = Arc::new(MpscMailbox::new(8, ActorPath::placeholder("/defaults")));
    for value in 0..4_u64 {
        mailbox
            .push(Box::new(value), BackpressureStrategy::Error)
            .await
            .unwrap();
    }
    let actor = BatchProbe {
        batches: batches.clone(),
        seen: seen.clone(),
        drops: drops.clone(),
        ready: ready.clone(),
        #[cfg(target_os = "linux")]
        stack: stack.clone(),
    };
    let processor = Arc::new(ActorProcessor::new(
        ThreadActor::new_for_test(actor),
        ThreadContext::new_for_test("/defaults"),
        "/defaults".into(),
        ThreadActorConfig {
            message_batch_size: Some(4),
            ..Default::default()
        },
    ));
    mailbox.set_processor(processor.clone());
    scheduler
        .schedule_with_processor(
            "/defaults",
            mailbox.clone(),
            processor.clone(),
            ThreadActorConfig {
                yield_after_each_message: explicit.then_some(false),
                idle_sleep_duration: explicit.then_some(Duration::from_millis(1)),
                thread_stack_size: explicit.then_some(24 * 1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while seen.lock().unwrap().len() < 4 {
            ready.notified().await;
        }
    })
    .await
    .unwrap();
    scheduler.deschedule("/defaults").await.unwrap();
    scheduler.shutdown().await.unwrap();
    drop(mailbox);
    drop(processor);
    assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2, 3]);
    assert_eq!(
        *batches.lock().unwrap(),
        if explicit { vec![4] } else { vec![1, 1, 1, 1] }
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    #[cfg(target_os = "linux")]
    assert!(
        stack.load(Ordering::SeqCst) >= if explicit { 24 } else { 16 } * 1024 * 1024,
        "configured stack request was ignored: {}",
        stack.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn dedicated_defaults_reach_business_batches_and_the_actual_linux_stack() {
    dedicated_behavior(false).await;
}
#[tokio::test]
async fn explicit_dedicated_overrides_take_precedence_over_scheduler_defaults() {
    dedicated_behavior(true).await;
}
