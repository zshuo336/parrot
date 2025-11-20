//! Real blocking handlers, scheduler task bounds, and observable idle retirement.
use parrot::thread::{
    config::ThreadActorSystemConfig, context::ThreadContext, system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::{sync::Notify, task::JoinHandle};

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap_or_else(|error| error.into_inner()) = true;
        self.changed.notify_all();
    }
    fn wait(&self) {
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(15), |open| {
                !*open
            })
            .unwrap();
        let opened = *open;
        drop(open);
        assert!(opened, "test did not release its blocking handler");
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Hold {
    entered: Arc<Notify>,
    gate: Arc<Gate>,
}
struct Echo;
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(hold) = message.downcast_ref::<Hold>() {
                hold.entered.notify_one();
                hold.gate.wait();
            }
            Ok(message)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
fn config() -> ThreadActorSystemConfig {
    ThreadActorSystemConfig {
        shared_pool_size: 2,
        shared_burst_workers_max: 2,
        shared_burst_backlog_threshold_ms: 40,
        shared_burst_idle_timeout_ms: 100,
        ..Default::default()
    }
}
async fn actor(system: &Arc<ThreadActorSystem>, path: &str) -> BoxedActorRef {
    let actor: BoxedActorRef = Box::new(
        system
            .spawn_at(Echo, path, None, Default::default())
            .await
            .unwrap(),
    );
    echo(&actor, 0).await;
    actor
}
async fn echo(actor: &BoxedActorRef, value: u64) {
    let reply = tokio::time::timeout(Duration::from_secs(5), actor.send(Box::new(value)))
        .await
        .expect("message did not progress")
        .unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), value);
}
async fn hold(actor: &BoxedActorRef, gate: &Arc<Gate>) -> JoinHandle<ActorResult<BoxedMessage>> {
    let entered = Arc::new(Notify::new());
    let message = Hold {
        entered: entered.clone(),
        gate: gate.clone(),
    };
    let actor = actor.clone_boxed();
    let task = tokio::spawn(async move { actor.send(Box::new(message)).await });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("blocking handler never entered");
    task
}
async fn join_hold(task: JoinHandle<ActorResult<BoxedMessage>>) {
    let reply = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(reply.is::<Hold>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn elastic_burst_rescues_starved_short_tasks() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_burst_idle_timeout_ms: 60_000,
        ..config()
    });
    let first = actor(&system, "/rescue/first").await;
    let second = actor(&system, "/rescue/second").await;
    let short = actor(&system, "/rescue/short").await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let first = hold(&first, &gate).await;
    let second = hold(&second, &gate).await;
    for value in 1..=200 {
        echo(&short, value).await;
        assert!(!first.is_finished() && !second.is_finished());
        let metrics = system.scheduler_metrics().unwrap();
        assert!((1..=2).contains(&metrics.burst_workers_alive));
    }
    gate.release();
    join_hold(first).await;
    join_hold(second).await;
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn elastic_burst_reaps_after_idle() {
    let system = ThreadActorSystem::shared(config());
    let first = actor(&system, "/reap/first").await;
    let second = actor(&system, "/reap/second").await;
    let short = actor(&system, "/reap/short").await;
    for _ in 0..2 {
        let gate = Arc::new(Gate::default());
        let _release = Release(gate.clone());
        let first = hold(&first, &gate).await;
        let second = hold(&second, &gate).await;
        let rescued = hold(&short, &gate).await;
        assert!(!first.is_finished() && !second.is_finished());
        assert!(system.scheduler_metrics().unwrap().burst_workers_alive > 0);
        gate.release();
        join_hold(first).await;
        join_hold(second).await;
        join_hold(rescued).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while system.scheduler_metrics().unwrap().burst_workers_alive != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("burst workers did not retire after pressure cleared");
    }
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn elastic_worker_task_count_bounded() {
    let settings = ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_backlog_threshold_ms: 0,
        shared_burst_idle_timeout_ms: 60_000,
        ..config()
    };
    let system = ThreadActorSystem::shared(settings);
    let mut actors = Vec::new();
    for index in 0..8 {
        actors.push(actor(&system, &format!("/bound/{index}")).await);
    }
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut held = Vec::new();
    for actor in &actors[..3] {
        held.push(hold(actor, &gate).await);
    }
    assert_eq!(system.scheduler_metrics().unwrap().burst_workers_alive, 2);
    let mut queued_entries = Vec::new();
    for actor in &actors[3..] {
        let entered = Arc::new(Notify::new());
        queued_entries.push(entered.clone());
        actor
            .deliver(Box::new(Hold {
                entered,
                gate: gate.clone(),
            }))
            .await
            .unwrap();
        let metrics = system.scheduler_metrics().unwrap();
        assert_eq!(metrics.pool_size, 1);
        assert_eq!(metrics.burst_workers_alive, 2);
        assert!(held.iter().all(|task| !task.is_finished()));
    }
    gate.release();
    for task in held {
        join_hold(task).await;
    }
    for entered in queued_entries {
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("queued blocking handler was never executed");
    }
    for (index, actor) in actors.iter().enumerate() {
        echo(actor, index as u64).await;
    }
    system.shutdown_internal().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn elastic_millisecond_config_does_not_wrap_to_immediate_scaling() {
    let mut builder = parrot_config::ParrotConfig::builder();
    builder.thread.shared_pool_size = Some(1);
    builder.thread.shared_burst_workers_max = Some(1);
    builder.thread.shared_burst_backlog_threshold_ms = Some(1 << 61);
    builder.thread.shared_burst_idle_timeout_ms = Some(60_000);
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(
        &builder.build().unwrap(),
    ));
    let blocker = actor(&system, "/duration/blocker").await;
    let queued = actor(&system, "/duration/queued").await;
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let held = hold(&blocker, &gate).await;
    queued.deliver(Box::new(7u64)).await.unwrap();
    let count = system.scheduler_metrics().unwrap().burst_workers_alive;
    gate.release();
    join_hold(held).await;
    echo(&queued, 8).await;
    system.shutdown_internal().await.unwrap();
    assert_eq!(
        count, 0,
        "configured threshold became an immediate expansion"
    );
}
