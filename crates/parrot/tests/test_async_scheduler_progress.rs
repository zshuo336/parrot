//! Waiting on another actor must release execution capacity, not mailbox ownership.
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use parrot::thread::config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};

struct Forwarder {
    next: Option<BoxedActorRef>,
    threads: Arc<Mutex<Vec<ThreadId>>>,
    forward_in_init: bool,
}

impl Actor for Forwarder {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.forward_in_init
                && let Some(next) = &self.next
            {
                next.send_with_timeout(Box::new(0_u64), Some(Duration::from_secs(2)))
                    .await?;
            }
            Ok(())
        })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.threads
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            let result = match &self.next {
                Some(next) => {
                    next.send_with_timeout(msg, Some(Duration::from_secs(2)))
                        .await
                }
                None => Ok(msg),
            };
            // Also verify the thread after resumption, not only on entry.
            self.threads
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            result
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

fn run_chain(sharded: bool, init: bool) {
    // A full-future spawn_blocking workaround would deadlock after two actors.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_queue_capacity: 1,
            shared_burst_workers_max: 0,
            ..Default::default()
        });
        let config = ThreadActorConfig {
            scheduling_mode: sharded.then(|| SchedulingMode::Sharded {
                affinity_key: "one-shard".into(),
            }),
            ..Default::default()
        };
        let threads = Arc::new(Mutex::new(Vec::new()));
        let mut next: Option<BoxedActorRef> = None;
        for n in 0..8 {
            let actor = Forwarder {
                next,
                threads: threads.clone(),
                forward_in_init: init,
            };
            next = Some(Box::new(
                system
                    .spawn_at(actor, &format!("/chain/{n}"), None, config.clone())
                    .await
                    .unwrap(),
            ));
        }
        let reply = next
            .unwrap()
            .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(3)))
            .await;
        // Always clean up before asserting, including on the broken implementation.
        system.shutdown_internal().await.unwrap();
        assert_eq!(
            *reply
                .expect("acyclic actor asks must make progress")
                .downcast::<u64>()
                .unwrap(),
            42
        );
        if sharded {
            let threads = threads.lock().unwrap();
            assert!(!threads.is_empty());
            assert!(
                threads.iter().all(|t| *t == threads[0]),
                "all polls must stay on the owning shard thread"
            );
        }
    });
}

#[test]
fn single_shared_worker_progresses_beyond_blocking_pool_capacity() {
    run_chain(false, false);
}

#[test]
fn shared_actor_init_can_await_another_actor() {
    run_chain(false, true);
}

#[test]
fn same_shard_asks_progress_without_losing_thread_affinity() {
    run_chain(true, false);
}

#[test]
fn shard_actor_init_can_await_another_actor() {
    run_chain(true, true);
}

struct GatedRecorder {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    seen: Vec<u64>,
}

impl Actor for GatedRecorder {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let n = *msg.downcast::<u64>().unwrap();
            if n == u64::MAX {
                return Ok(Box::new(self.seen.clone()) as BoxedMessage);
            }
            if n == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            // Async suspension inside every message exercises mailbox ownership.
            tokio::task::yield_now().await;
            self.seen.push(n);
            Ok(Box::new(n) as BoxedMessage)
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

async fn fifo_while_suspended(sharded: bool) {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    let config = ThreadActorConfig {
        scheduling_mode: sharded.then(|| SchedulingMode::Sharded {
            affinity_key: "one-shard".into(),
        }),
        ..Default::default()
    };
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let recorder = system
        .spawn_at(
            GatedRecorder {
                entered: entered.clone(),
                release: release.clone(),
                seen: vec![],
            },
            "/recorder",
            None,
            config.clone(),
        )
        .await
        .unwrap();
    recorder.deliver(Box::new(0_u64)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    for n in 1..64_u64 {
        recorder.deliver(Box::new(n)).await.unwrap();
    }

    let independent = system
        .spawn_at(
            Forwarder {
                next: None,
                threads: Default::default(),
                forward_in_init: false,
            },
            "/independent",
            None,
            config,
        )
        .await
        .unwrap();
    let reply = independent
        .send_with_timeout(Box::new(99_u64), Some(Duration::from_secs(2)))
        .await;
    // Release even if the independent actor starved, allowing failure cleanup.
    release.notify_one();
    let recorded = recorder
        .send_with_timeout(Box::new(u64::MAX), Some(Duration::from_secs(2)))
        .await;
    system.shutdown_internal().await.unwrap();
    assert_eq!(*reply.unwrap().downcast::<u64>().unwrap(), 99);
    assert_eq!(
        *recorded.unwrap().downcast::<Vec<u64>>().unwrap(),
        (0..64).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_wait_releases_capacity_but_preserves_actor_fifo() {
    fifo_while_suspended(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shard_wait_releases_capacity_but_preserves_actor_fifo() {
    fifo_while_suspended(true).await;
}

struct BlockingPoll {
    active: Arc<std::sync::atomic::AtomicUsize>,
    peak: Arc<std::sync::atomic::AtomicUsize>,
}

impl Actor for BlockingPoll {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            use std::sync::atomic::Ordering::SeqCst;
            let active = self.active.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(active, SeqCst);
            // Represents user code that cannot yield during a poll. The
            // assertion concerns concurrency, not a machine-specific latency.
            std::thread::sleep(Duration::from_millis(20));
            self.active.fetch_sub(1, SeqCst);
            Ok(msg)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiplexing_does_not_exceed_configured_cpu_capacity() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut actors = vec![];
    for n in 0..8 {
        actors.push(
            system
                .spawn_at(
                    BlockingPoll {
                        active: active.clone(),
                        peak: peak.clone(),
                    },
                    &format!("/blocking/{n}"),
                    None,
                    Default::default(),
                )
                .await
                .unwrap(),
        );
    }
    let results = futures::future::join_all(
        actors
            .iter()
            .map(|actor| actor.send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(2)))),
    )
    .await;
    system.shutdown_internal().await.unwrap();
    for result in results {
        assert_eq!(*result.unwrap().downcast::<u64>().unwrap(), 42);
    }
    assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 1);
}
