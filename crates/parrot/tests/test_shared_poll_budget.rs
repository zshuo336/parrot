//! Synchronous polls consume shared execution permits, not Tokio driver threads.
use futures::FutureExt;
use parrot::thread::{
    SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadActorSystemConfig, ThreadContext,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;
use tokio::sync::oneshot;

const DEADLINE: Duration = Duration::from_secs(2);
#[derive(Default)]
struct Counts {
    active: AtomicUsize,
    destroyed: AtomicUsize,
}
enum Input {
    Echo(u64),
    Block {
        entered: oneshot::Sender<std::thread::ThreadId>,
        release: mpsc::Receiver<()>,
    },
    Probe(oneshot::Sender<(u64, usize)>),
}
struct Worker(Arc<Counts>);
impl Drop for Worker {
    fn drop(&mut self) {
        self.0.destroyed.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Worker {
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
            match *msg.downcast::<Input>().unwrap() {
                Input::Echo(value) => Ok(Box::new(value) as BoxedMessage),
                Input::Block { entered, release } => {
                    self.0.active.fetch_add(1, Ordering::SeqCst);
                    let _ = entered.send(std::thread::current().id());
                    let released = release.recv_timeout(Duration::from_secs(15));
                    self.0.active.fetch_sub(1, Ordering::SeqCst);
                    released.expect("test must release every synchronous handler");
                    Ok(Box::new(42_u64) as BoxedMessage)
                }
                Input::Probe(reply) => {
                    let _ = reply.send((42, self.0.active.load(Ordering::SeqCst)));
                    Ok(Box::new(42_u64) as BoxedMessage)
                }
            }
        })
    }
}

#[test]
fn saturated_shared_polls_preserve_host_and_dedicated_progress_and_recover() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 1,
            shared_burst_backlog_threshold_ms: 10,
            ..Default::default()
        });
        let counts = Arc::new(Counts::default());
        let mut actors = Vec::new();
        for index in 0..4 {
            let actor = system
                .spawn_at(
                    Worker(counts.clone()),
                    &format!("/budget/{index}"),
                    None,
                    ThreadActorConfig {
                        scheduling_mode: (index == 3).then_some(SchedulingMode::DedicatedThread),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let reply = actor
                .send_with_timeout(Box::new(Input::Echo(42)), Some(DEADLINE))
                .await
                .unwrap();
            assert_eq!(*reply.downcast::<u64>().unwrap(), 42);
            actors.push(actor);
        }
        let host_thread = tokio::spawn(async { std::thread::current().id() })
            .await
            .unwrap();
        let (release_a, blocked_a) = mpsc::channel();
        let (release_b, blocked_b) = mpsc::channel();
        let (finish, finished) = mpsc::channel();
        let expired = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let releases = [release_a.clone(), release_b.clone()];
            let expired = expired.clone();
            std::thread::spawn(move || {
                if finished.recv_timeout(Duration::from_secs(5)).is_err() {
                    expired.store(true, Ordering::SeqCst);
                    for release in releases {
                        let _ = release.send(());
                    }
                }
            })
        };
        let mut jobs = tokio::task::JoinSet::new();
        let outcome = std::panic::AssertUnwindSafe(async {
            for (index, release) in [blocked_a, blocked_b].into_iter().enumerate() {
                let (entered, receipt) = oneshot::channel();
                let actor = actors[index].clone();
                jobs.spawn(async move {
                    actor
                        .send_with_timeout(
                            Box::new(Input::Block { entered, release }),
                            Some(Duration::from_secs(20)),
                        )
                        .await
                });
                let thread = tokio::time::timeout(DEADLINE, receipt)
                    .await
                    .expect("each admitted shared poll must enter, including elastic expansion")
                    .unwrap();
                assert_ne!(
                    thread, host_thread,
                    "synchronous poll must not execute on the async driver"
                );
            }
            assert_eq!(counts.active.load(Ordering::SeqCst), 2);
            assert_eq!(system.scheduler_metrics().unwrap().burst_workers_alive, 1);
            assert_eq!(
                tokio::time::timeout(
                    DEADLINE,
                    tokio::spawn(async {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        42
                    })
                )
                .await
                .unwrap()
                .unwrap(),
                42,
                "host timers and tasks must keep progressing"
            );

            let (reply, received) = oneshot::channel();
            actors[3]
                .deliver(Box::new(Input::Probe(reply)))
                .await
                .unwrap();
            assert_eq!(
                tokio::time::timeout(DEADLINE, received)
                    .await
                    .unwrap()
                    .unwrap(),
                (42, 2),
                "dedicated actor must progress while both shared polls remain blocked"
            );

            let (reply, mut received) = oneshot::channel();
            actors[2]
                .deliver(Box::new(Input::Probe(reply)))
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(25), &mut received)
                    .await
                    .is_err(),
                "shared probe must remain queued when the configured poll budget is occupied"
            );
            release_a.send(()).unwrap();
            assert_eq!(
                tokio::time::timeout(DEADLINE, received)
                    .await
                    .unwrap()
                    .unwrap(),
                (42, 1),
                "freeing one permit must restore shared progress while another poll is blocked"
            );
        })
        .catch_unwind()
        .await;
        let _ = release_a.send(());
        let _ = release_b.send(());
        let _ = finish.send(());
        watchdog.join().unwrap();
        let mut completed = Vec::new();
        while let Some(result) = jobs.join_next().await {
            completed.push(result);
        }
        let shutdown = system.shutdown_internal().await;
        assert!(
            !expired.load(Ordering::SeqCst),
            "OS watchdog released stalled work: host asynchronous drive was starved"
        );
        shutdown.unwrap();
        assert_eq!(counts.active.load(Ordering::SeqCst), 0);
        assert_eq!(counts.destroyed.load(Ordering::SeqCst), 4);
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
        assert_eq!(completed.len(), 2);
        for result in completed {
            assert_eq!(*result.unwrap().unwrap().downcast::<u64>().unwrap(), 42);
        }
    });
}
