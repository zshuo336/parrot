// SPDX-License-Identifier: AGPL-3.0-only
use parrot::thread::{
    actor::ThreadActor,
    config::{BackpressureStrategy, ThreadActorConfig},
    context::ThreadContext,
    mailbox::{Mailbox, mpsc::MpscMailbox},
    processor::ActorProcessor,
    scheduler::sharded::ShardedScheduler,
    scheduler::shared::{SharedThreadPool, SharedThreadPoolConfig},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tokio::sync::oneshot;

const PATH: &str = "/shared-registration";
const WAIT: Duration = Duration::from_secs(3);

struct Counter(u32);
struct Request {
    pause: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    reply: oneshot::Sender<u32>,
}
impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let Request { pause, reply } = *message.downcast::<Request>().unwrap();
            if let Some((entered, release)) = pause {
                entered.send(()).unwrap();
                release.await.unwrap();
            }
            self.0 += 1;
            let _ = reply.send(self.0);
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}

// Replacing a built-in mailbox's hook drops its old capture after publication.
// The new registration must remain usable if that old capture later fails.
struct HookRetirement {
    entered: Option<oneshot::Sender<()>>,
    release: Mutex<mpsc::Receiver<()>>,
}
impl Drop for HookRetirement {
    fn drop(&mut self) {
        self.entered.take().unwrap().send(()).unwrap();
        self.release.get_mut().unwrap().recv_timeout(WAIT).unwrap();
        panic!("old hook retirement");
    }
}

async fn submit(
    mailbox: &MpscMailbox,
    pause: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
) -> oneshot::Receiver<u32> {
    let (reply, received) = oneshot::channel::<u32>();
    mailbox
        .push(
            Box::new(Request { pause, reply }),
            BackpressureStrategy::Error,
        )
        .await
        .unwrap();
    received
}
async fn send(mailbox: &MpscMailbox) -> Result<u32, tokio::time::error::Elapsed> {
    let received = submit(mailbox, None).await;
    tokio::time::timeout(WAIT, async { received.await.unwrap() }).await
}

fn pool(work_stealing: bool) -> Arc<SharedThreadPool> {
    Arc::new(SharedThreadPool::new(
        Some(SharedThreadPoolConfig {
            pool_size: 2,
            work_stealing,
            max_queue_capacity: 1,
            max_messages_per_batch: 1,
            burst_workers_max: 0,
            ..Default::default()
        }),
        tokio::runtime::Handle::current(),
    ))
}
fn counter(path: &str) -> Arc<MpscMailbox> {
    let mailbox = Arc::new(MpscMailbox::new(4, ActorPath::placeholder(path)));
    mailbox.set_processor(Arc::new(ActorProcessor::new(
        ThreadActor::new_for_test(Counter(0)),
        ThreadContext::new_for_test(path),
        path.to_owned(),
        ThreadActorConfig::default(),
    )));
    mailbox
}

async fn replace_during_publication(work_stealing: bool) {
    let pool = pool(work_stealing);
    let mailbox = counter(PATH);
    let (entered, started) = oneshot::channel();
    let (release, held) = mpsc::channel();
    let retirement = HookRetirement {
        entered: Some(entered),
        release: Mutex::new(held),
    };
    mailbox.set_wake_hook(Arc::new(move || {
        std::hint::black_box(&retirement);
    }));
    let installing = {
        let pool = pool.clone();
        let mailbox = mailbox.clone();
        tokio::task::spawn_blocking(move || {
            futures::executor::block_on(pool.schedule(PATH, mailbox, None))
        })
    };
    tokio::time::timeout(WAIT, started).await.unwrap().unwrap();
    pool.deschedule(PATH).await.unwrap();
    pool.schedule(PATH, mailbox.clone(), None).await.unwrap();
    assert_eq!(send(&mailbox).await.unwrap(), 1);
    release.send(()).unwrap();
    let failure = installing.await.unwrap().unwrap_err();
    assert!(
        failure.to_string().contains("old hook retirement"),
        "{failure:?}"
    );
    let retained = pool.is_scheduled(PATH);
    let second = send(&mailbox).await;
    let removed = pool.deschedule(PATH).await;
    pool.shutdown(3000).await.unwrap();
    assert!(
        retained,
        "old rollback erased the replacement path; second reply: {second:?}"
    );
    assert_eq!(second.unwrap(), 2);
    removed.unwrap();
    assert!(mailbox.is_empty().await);
    assert_eq!(pool.metrics().queue_length, 0);
    assert_eq!(pool.metrics().deferred_ready, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fifo_retains_replacement_after_old_publication_fails() {
    replace_during_publication(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_stealing_retains_replacement_after_old_publication_fails() {
    replace_during_publication(true).await;
}

async fn replace_running(work_stealing: bool) {
    let pool = pool(work_stealing);
    let mailbox = counter(PATH);
    pool.schedule(PATH, mailbox.clone(), None).await.unwrap();
    let (entered, started) = oneshot::channel();
    let (release, held) = oneshot::channel();
    let first = submit(&mailbox, Some((entered, held))).await;
    tokio::time::timeout(WAIT, started).await.unwrap().unwrap();
    pool.deschedule(PATH).await.unwrap();
    let mut second = submit(&mailbox, None).await;
    pool.schedule(PATH, mailbox.clone(), None).await.unwrap();
    let other = counter("/other");
    pool.schedule("/other", other.clone(), None).await.unwrap();
    assert_eq!(send(&other).await.unwrap(), 1);
    assert_eq!(second.try_recv(), Err(oneshot::error::TryRecvError::Empty));
    release.send(()).unwrap();
    assert_eq!(tokio::time::timeout(WAIT, first).await.unwrap().unwrap(), 1);
    assert_eq!(
        tokio::time::timeout(WAIT, second).await.unwrap().unwrap(),
        2
    );
    pool.deschedule(PATH).await.unwrap();
    pool.deschedule("/other").await.unwrap();
    pool.shutdown(3000).await.unwrap();
    assert!(mailbox.is_empty().await);
    assert_eq!(pool.metrics().queue_length, 0);
    assert_eq!(pool.metrics().deferred_ready, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fifo_running_actor_hands_off_pending_work() {
    replace_running(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_stealing_running_actor_hands_off_pending_work() {
    replace_running(true).await;
}

enum Owner {
    Pool(Arc<SharedThreadPool>),
    Shard(Arc<ShardedScheduler>),
}

impl Owner {
    async fn schedule(&self, path: &str, mailbox: Arc<MpscMailbox>) {
        match self {
            Self::Pool(pool) => pool.schedule(path, mailbox, None).await.unwrap(),
            Self::Shard(shard) => shard.schedule_affinity(path, mailbox, "one").unwrap(),
        }
    }

    async fn remove(&self, path: &str) {
        match self {
            Self::Pool(pool) => pool.deschedule(path).await.unwrap(),
            Self::Shard(shard) => shard.deschedule(path).unwrap(),
        }
    }

    async fn stop(&self) {
        match self {
            Self::Pool(pool) => pool.shutdown(3000).await.unwrap(),
            Self::Shard(shard) => shard.shutdown_result().unwrap(),
        }
    }
}

async fn transfer(source: Owner, destination: Owner) {
    let mailbox = counter(PATH);
    source.schedule(PATH, mailbox.clone()).await;
    let (entered, started) = oneshot::channel();
    let (release, held) = oneshot::channel();
    let first = submit(&mailbox, Some((entered, held))).await;
    tokio::time::timeout(WAIT, started).await.unwrap().unwrap();
    source.remove(PATH).await;
    let mut second = submit(&mailbox, None).await;
    destination.schedule(PATH, mailbox.clone()).await;
    let other = counter("/destination");
    destination.schedule("/destination", other.clone()).await;
    assert_eq!(send(&other).await.unwrap(), 1);
    assert_eq!(second.try_recv(), Err(oneshot::error::TryRecvError::Empty));
    release.send(()).unwrap();
    assert_eq!(tokio::time::timeout(WAIT, first).await.unwrap().unwrap(), 1);
    let pending = tokio::time::timeout(WAIT, second).await;
    source.stop().await;
    let after_source_exit = send(&mailbox).await;
    destination.remove(PATH).await;
    destination.remove("/destination").await;
    destination.stop().await;
    assert_eq!(pending.expect("handoff lost the pending wake").unwrap(), 2);
    assert_eq!(after_source_exit.unwrap(), 3);
    assert!(mailbox.is_empty().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fifo_to_work_stealing_handoff() {
    transfer(Owner::Pool(pool(false)), Owner::Pool(pool(true))).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_stealing_to_fifo_handoff() {
    transfer(Owner::Pool(pool(true)), Owner::Pool(pool(false))).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_to_shard_handoff() {
    transfer(
        Owner::Pool(pool(false)),
        Owner::Shard(ShardedScheduler::start(1, 1)),
    )
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shard_to_shared_handoff() {
    transfer(
        Owner::Shard(ShardedScheduler::start(1, 1)),
        Owner::Pool(pool(true)),
    )
    .await;
}
