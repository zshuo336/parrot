use parrot::thread::{
    config::{ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot::thread::{
    mailbox::{Mailbox, mpsc::MpscMailbox},
    scheduler::queue::{Lane, SchedulingQueue},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorPath, ActorRef},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[test]
fn ready_queue_capacity_is_shared_by_both_lanes() {
    let queue = SchedulingQueue::new(2);
    for (name, lane) in [
        ("normal-1", Lane::Normal),
        ("high-1", Lane::High),
        ("normal-2", Lane::Normal),
        ("high-2", Lane::High),
    ] {
        let mailbox: Arc<dyn Mailbox> = Arc::new(MpscMailbox::new(1, ActorPath::placeholder(name)));
        let _ = queue.push_with_lane(mailbox, lane);
    }
    assert_eq!(
        queue.len(),
        2,
        "configured capacity must be a real shared upper bound"
    );
    assert_eq!(queue.high_len() + queue.normal_len(), 2);
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    fn wait(&self) {
        let (guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(8), |open| {
                !*open
            })
            .unwrap();
        assert!(
            *guard && !timeout.timed_out(),
            "test failed to release blocking actor"
        );
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct BlockingActor {
    entered: Arc<tokio::sync::Notify>,
    gate: Arc<Gate>,
}
impl Actor for BlockingActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.gate.wait();
            Ok(msg)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
struct Echo;
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_small_queue_bounds_backlog_without_limiting_actor_count_or_losing_work() {
    let mut config = parrot_config::ParrotConfig::builder();
    config.thread.shared_pool_size = Some(1);
    config.thread.shared_burst_workers_max = Some(0);
    config.thread.shared_queue_capacity = Some(1);
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(
        &config.build().unwrap(),
    ));
    let entered = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let blocker = system
        .spawn_at(
            BlockingActor {
                entered: entered.clone(),
                gate: gate.clone(),
            },
            "/blocker",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    let blocking = tokio::spawn(async move { blocker.send(Box::new(())).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let mut actors = Vec::new();
    for n in 0..12 {
        actors.push(
            system
                .spawn_at(
                    Echo,
                    &format!("/waiting/{n}"),
                    None,
                    ThreadActorConfig::default(),
                )
                .await
                .unwrap(),
        );
    }
    let metrics = system.scheduler_metrics().unwrap();
    let backlog = metrics.queue_length;
    let calls: Vec<_> = actors
        .into_iter()
        .enumerate()
        .map(|(n, actor)| {
            tokio::spawn(async move {
                actor
                    .send_with_timeout(Box::new(n as u64), Some(Duration::from_secs(3)))
                    .await
                    .map(|reply| *reply.downcast::<u64>().unwrap())
            })
        })
        .collect();
    gate.release();
    blocking.await.unwrap().unwrap();
    for (n, call) in calls.into_iter().enumerate() {
        assert_eq!(call.await.unwrap().unwrap(), n as u64);
    }
    system.shutdown_internal().await.unwrap();
    assert_eq!(metrics.queue_capacity, 1);
    assert_eq!(metrics.deferred_ready, 11);
    assert_eq!(
        backlog, 1,
        "ready queue must obey the resolved configuration even while CPU execution is blocked"
    );
}

struct Recorder {
    seen: Vec<u64>,
    initialized: Arc<std::sync::atomic::AtomicUsize>,
}
impl Actor for Recorder {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.initialized
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let n = *msg.downcast::<u64>().unwrap();
            tokio::task::yield_now().await;
            if n == u64::MAX {
                return Ok(Box::new(self.seen.clone()) as BoxedMessage);
            }
            self.seen.push(n);
            Ok(Box::new(n) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
fn recorder() -> Recorder {
    Recorder {
        seen: vec![],
        initialized: Arc::default(),
    }
}
async fn blocked_system() -> (
    Arc<ThreadActorSystem>,
    Release,
    tokio::task::JoinHandle<ActorResult<BoxedMessage>>,
) {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_queue_capacity: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    let entered = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let blocker = system
        .spawn_at(
            BlockingActor {
                entered: entered.clone(),
                gate,
            },
            "/cpu",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    let task = tokio::spawn(async move { blocker.send(Box::new(())).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    (system, release, task)
}

async fn exercise_deferred_backpressure(cancel: bool) {
    let (system, release, blocked) = blocked_system().await;
    let _occupant = system
        .spawn_at(Echo, "/resident", None, ThreadActorConfig::default())
        .await
        .unwrap();
    let actor = system
        .spawn_at(
            recorder(),
            "/deferred",
            None,
            ThreadActorConfig {
                mailbox_capacity: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    for n in 0..2_u64 {
        actor.deliver(Box::new(n)).await.unwrap();
    }
    let mut waiting = actor.deliver(Box::new(2_u64));
    assert!(
        tokio::time::timeout(Duration::from_millis(40), &mut waiting)
            .await
            .is_err(),
        "full deferred mailbox must apply backpressure"
    );
    assert_eq!(system.scheduler_metrics().unwrap().deferred_ready, 1);
    if cancel {
        drop(waiting);
    } else {
        release.0.release();
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
    }
    release.0.release();
    blocked.await.unwrap().unwrap();
    let reply = actor
        .send_with_timeout(Box::new(u64::MAX), Some(Duration::from_secs(2)))
        .await
        .unwrap();
    let expected: Vec<u64> = (0..if cancel { 2 } else { 3 }).collect();
    assert_eq!(*reply.downcast::<Vec<u64>>().unwrap(), expected);
    system.shutdown_internal().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_mailbox_backpressure_resumes_when_scheduler_progresses() {
    exercise_deferred_backpressure(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_full_deferred_send_preserves_already_accepted_messages() {
    exercise_deferred_backpressure(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_removes_deferred_generation_before_same_path_is_registered_again() {
    let (system, release, blocked) = blocked_system().await;
    let _occupant = system
        .spawn_at(Echo, "/resident", None, ThreadActorConfig::default())
        .await
        .unwrap();
    let old = recorder();
    let initialized = old.initialized.clone();
    let actor = system
        .spawn_at(old, "/generation", None, ThreadActorConfig::default())
        .await
        .unwrap();
    actor.deliver(Box::new(1_u64)).await.unwrap();
    assert_eq!(system.scheduler_metrics().unwrap().deferred_ready, 1);
    tokio::time::timeout(Duration::from_secs(2), actor.stop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(system.scheduler_metrics().unwrap().deferred_ready, 0);
    let new = system
        .spawn_at(
            recorder(),
            "/generation",
            None,
            ThreadActorConfig::default(),
        )
        .await
        .unwrap();
    new.deliver(Box::new(2_u64)).await.unwrap();
    release.0.release();
    blocked.await.unwrap().unwrap();
    let seen = new
        .send_with_timeout(Box::new(u64::MAX), Some(Duration::from_secs(2)))
        .await
        .unwrap();
    assert_eq!(*seen.downcast::<Vec<u64>>().unwrap(), vec![2]);
    assert_eq!(
        initialized.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "stopped deferred generation must never initialize later"
    );
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_saturation_preserves_every_actors_fifo_and_releases_all_markers() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 2,
        shared_queue_capacity: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    let mut tasks = Vec::new();
    for n in 0..32 {
        let actor = system
            .spawn_at(
                recorder(),
                &format!("/concurrent/{n}"),
                None,
                ThreadActorConfig {
                    mailbox_capacity: Some(4),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        tasks.push(tokio::spawn(async move {
            for n in 0..80_u64 {
                actor.deliver(Box::new(n)).await.unwrap();
            }
            let seen = actor
                .send_with_timeout(Box::new(u64::MAX), Some(Duration::from_secs(5)))
                .await
                .unwrap();
            assert_eq!(
                *seen.downcast::<Vec<u64>>().unwrap(),
                (0..80).collect::<Vec<_>>()
            );
        }));
    }
    tokio::time::timeout(Duration::from_secs(8), async {
        for task in tasks {
            task.await.unwrap();
        }
        while system.scheduler_metrics().unwrap().deferred_ready != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(system.scheduler_metrics().unwrap().queue_length <= 1);
    system.shutdown_internal().await.unwrap();
}
