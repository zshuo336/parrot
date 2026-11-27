// SPDX-License-Identifier: AGPL-3.0-only
use parrot::thread::{
    ActorProcessor, BackpressureStrategy, ThreadActor, ThreadActorConfig, ThreadContext,
    error::SystemError,
    mailbox::{Mailbox, mpsc::MpscMailbox},
    scheduler::{
        ThreadScheduler,
        shared::{SharedThreadPool, SharedThreadPoolConfig},
    },
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};
use tokio::sync::oneshot;

#[path = "support/isolated.rs"]
mod isolated;

const WAIT: Duration = Duration::from_secs(8);
struct Held {
    disposing: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.disposing.send(());
        let _ = self.release.recv_timeout(WAIT);
    }
}
struct Probe;
struct Reentry(Arc<SharedThreadPool>, Option<oneshot::Sender<()>>);
impl Reentry {
    fn check(&self, error: Box<dyn std::error::Error + Send + Sync>) {
        assert!(matches!(error.downcast_ref::<SystemError>(),
            Some(SystemError::WorkerStateError(message)) if message.contains("own retirement")));
        assert!(!self.0.metrics().is_shutting_down);
    }
}
impl Drop for Reentry {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.check(ThreadScheduler::shutdown(self.0.as_ref()).unwrap_err());
            self.1.take().unwrap().send(()).unwrap();
        }
    }
}
impl Actor for Probe {
    type Context = ThreadContext<Self>;
    type Config = EmptyConfig;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let msg = match msg.downcast::<Reentry>() {
                Ok(input) => {
                    input.check(ThreadScheduler::shutdown(input.0.as_ref()).unwrap_err());
                    input.check(input.0.shutdown_async().await.unwrap_err());
                    return Ok(Box::new(()) as BoxedMessage);
                }
                Err(msg) => msg,
            };
            let (_held, started) = *msg.downcast::<(Held, oneshot::Sender<()>)>().unwrap();
            started.send(()).unwrap();
            std::future::pending().await
        })
    }
}

#[test]
fn shared_handler_and_input_destructor_cannot_join_their_own_pool() {
    isolated::run(
        "shared_handler_and_input_destructor_cannot_join_their_own_pool",
        "PARROT_SHARED_SELF_JOIN_CASE",
        WAIT,
        || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let pool = Arc::new(SharedThreadPool::new(
                    Some(SharedThreadPoolConfig {
                        pool_size: 1,
                        burst_workers_max: 0,
                        ..Default::default()
                    }),
                    runtime.handle().clone(),
                ));
                let path = "/self-join";
                let mailbox = Arc::new(MpscMailbox::new(2, ActorPath::placeholder(path)));
                mailbox.set_processor(Arc::new(ActorProcessor::new(
                    ThreadActor::new_for_test(Probe),
                    ThreadContext::new_for_test(path),
                    path.into(),
                    ThreadActorConfig::default(),
                )));
                ThreadScheduler::schedule(pool.as_ref(), path, mailbox.clone(), None).unwrap();
                for _ in 0..2 {
                    let (done, dropped) = oneshot::channel();
                    mailbox
                        .push(
                            Box::new(Reentry(pool.clone(), Some(done))),
                            BackpressureStrategy::Error,
                        )
                        .await
                        .unwrap();
                    dropped.await.unwrap();
                }
                pool.shutdown_async().await.unwrap();
            });
        },
    );
}

async fn running() -> (Arc<SharedThreadPool>, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let pool = Arc::new(SharedThreadPool::new(
        Some(SharedThreadPoolConfig {
            pool_size: 1,
            burst_workers_max: 0,
            ..Default::default()
        }),
        tokio::runtime::Handle::current(),
    ));
    let path = "/shared-shutdown";
    let mailbox = Arc::new(MpscMailbox::new(2, ActorPath::placeholder(path)));
    let processor = Arc::new(ActorProcessor::new(
        ThreadActor::new_for_test(Probe),
        ThreadContext::new_for_test(path),
        path.into(),
        ThreadActorConfig::default(),
    ));
    mailbox.set_processor(processor);
    ThreadScheduler::schedule(pool.as_ref(), path, mailbox.clone(), None).unwrap();
    let (entered, disposing) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let (started, started_rx) = oneshot::channel::<()>();
    mailbox
        .push(
            Box::new((
                Held {
                    disposing: entered,
                    release: wait,
                },
                started,
            )),
            BackpressureStrategy::Error,
        )
        .await
        .unwrap();
    tokio::time::timeout(WAIT, started_rx)
        .await
        .unwrap()
        .unwrap();
    (pool, disposing, release)
}

async fn join_shared(
    pool: Arc<SharedThreadPool>,
    blocking_task: bool,
    disposing: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
) {
    let (returned, observe) = mpsc::channel();
    let witness = std::thread::spawn(move || {
        disposing.recv_timeout(WAIT).unwrap();
        assert!(
            matches!(
                observe.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "shutdown returned before shared handler disposal"
        );
        release.send(()).unwrap();
    });
    let call = move || {
        ThreadScheduler::shutdown(pool.as_ref()).unwrap();
        assert!(!pool.is_scheduled("/shared-shutdown"));
        let _ = returned.send(());
    };
    let task = if blocking_task {
        tokio::task::spawn_blocking(call)
    } else {
        tokio::spawn(async move { call() })
    };
    task.await.unwrap();
    witness.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn sole_runtime_worker_can_synchronously_join_shared_retirement() {
    let (pool, disposing, release) = running().await;
    join_shared(pool, false, disposing, release).await;
}

#[tokio::test]
async fn external_blocking_caller_can_join_a_driven_current_thread_pool() {
    let (pool, disposing, release) = running().await;
    join_shared(pool, true, disposing, release).await;
}

#[test]
fn undriven_current_thread_host_can_resume_a_timed_out_sync_shutdown() {
    isolated::run(
        "undriven_current_thread_host_can_resume_a_timed_out_sync_shutdown",
        "PARROT_UNDRIVEN_SHARED_CASE",
        WAIT * 2,
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (pool, disposing, release) = runtime.block_on(running());
            let error = ThreadScheduler::shutdown(pool.as_ref()).unwrap_err();
            assert!(matches!(
                error.downcast_ref::<SystemError>(),
                Some(SystemError::Timeout(_))
            ));
            assert!(pool.metrics().is_shutting_down);
            runtime.block_on(async {
                let waiter = pool.shutdown_async();
                let witness = tokio::task::spawn_blocking(move || {
                    disposing.recv_timeout(WAIT).unwrap();
                    release.send(()).unwrap();
                });
                waiter.await.unwrap();
                witness.await.unwrap();
            });
            ThreadScheduler::shutdown(pool.as_ref()).unwrap();
        },
    );
}

async fn local_shutdown() {
    let (pool, disposing, release) = running().await;
    let scheduler: &dyn ThreadScheduler = pool.as_ref();
    let refused = scheduler.shutdown().unwrap_err();
    assert!(matches!(
        refused.downcast_ref::<SystemError>(),
        Some(SystemError::WorkerStateError(_))
    ));
    assert!(!pool.metrics().is_shutting_down);
    assert!(pool.is_scheduled("/shared-shutdown"));
    let mut first = scheduler.shutdown_async();
    assert!(futures::poll!(&mut first).is_pending());
    drop(first);
    tokio::task::spawn_blocking(move || disposing.recv_timeout(WAIT).unwrap())
        .await
        .unwrap();
    let mut second = scheduler.shutdown_async();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut second)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    tokio::time::timeout(WAIT, second).await.unwrap().unwrap();
    scheduler.shutdown().unwrap();
    assert!(!scheduler.is_scheduled("/shared-shutdown"));
}

#[tokio::test]
async fn current_thread_shutdown_waits_after_cancelling_the_first_waiter() {
    local_shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn local_set_shutdown_waits_after_cancelling_the_first_waiter() {
    tokio::task::LocalSet::new()
        .run_until(local_shutdown())
        .await;
}
