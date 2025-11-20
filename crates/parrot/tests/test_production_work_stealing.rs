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
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

enum Input {
    Gate,
    Value(u64),
    Snapshot,
}
struct Recorder {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    active: Arc<AtomicUsize>,
    seen: Vec<u64>,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        assert_eq!(self.0.fetch_sub(1, Ordering::SeqCst), 1);
    }
}
impl Actor for Recorder {
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
            assert_eq!(
                self.active.fetch_add(1, Ordering::SeqCst),
                0,
                "actor must have one executing batch"
            );
            let _active = Active(self.active.clone());
            match *message.downcast::<Input>().unwrap() {
                Input::Gate => {
                    self.entered.notify_one();
                    self.release.notified().await;
                    Ok(Box::new(91u64) as BoxedMessage)
                }
                Input::Value(value) => {
                    tokio::task::yield_now().await;
                    self.seen.push(value);
                    Ok(Box::new(value) as BoxedMessage)
                }
                Input::Snapshot => Ok(Box::new(self.seen.clone()) as BoxedMessage),
            }
        })
    }
}
struct Release(Arc<Notify>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[tokio::test]
async fn resolved_configuration_selects_real_local_requeues_and_preserves_fifo() {
    for enabled in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("parrot.toml");
        std::fs::write(&path, format!("[thread]\nshared_pool_size = 1\nshared_burst_workers_max = 0\nshared_queue_capacity = 1\nshared_work_stealing = {enabled}\n")).unwrap();
        let resolved = parrot_config::ParrotConfig::builder()
            .load_file(path)
            .unwrap()
            .build()
            .unwrap();
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig::from_resolved(&resolved));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let _release = Release(release.clone());
        let actor = system
            .spawn_at(
                Recorder {
                    entered: entered.clone(),
                    release: release.clone(),
                    active: Arc::default(),
                    seen: vec![],
                },
                "/production-steal",
                None,
                ThreadActorConfig {
                    scheduling_mode: Some(SchedulingMode::SharedPool {
                        max_messages_per_run: 1,
                    }),
                    mailbox_capacity: Some(128),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let gate_actor = actor.clone();
        let gate = tokio::spawn(async move { gate_actor.send(Box::new(Input::Gate)).await });
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        for value in 0..80 {
            actor.deliver(Box::new(Input::Value(value))).await.unwrap();
        }
        release.notify_one();
        assert_eq!(*gate.await.unwrap().unwrap().downcast::<u64>().unwrap(), 91);
        let seen = actor
            .send_with_timeout(Box::new(Input::Snapshot), Some(Duration::from_secs(5)))
            .await
            .unwrap();
        assert_eq!(
            *seen.downcast::<Vec<u64>>().unwrap(),
            (0..80).collect::<Vec<_>>()
        );
        let metrics = system.scheduler_metrics().unwrap();
        system.shutdown_internal().await.unwrap();
        assert_eq!(
            metrics.local_selections > 0,
            enabled,
            "config must reach the actual worker selection path"
        );
        assert!(metrics.queue_length <= 1);
        assert_eq!(
            metrics.work_steals, 0,
            "single-worker execution must not invent steals"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_local_and_deferred_actors_retain_each_fifo_until_shutdown() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 3,
        shared_burst_workers_max: 0,
        shared_queue_capacity: 1,
        shared_work_stealing: true,
        ..Default::default()
    });
    let mut tasks = Vec::new();
    for n in 0..16 {
        let actor = system
            .spawn_at(
                Recorder {
                    entered: Arc::default(),
                    release: Arc::default(),
                    active: Arc::default(),
                    seen: vec![],
                },
                &format!("/steal-fifo/{n}"),
                None,
                ThreadActorConfig {
                    scheduling_mode: Some(SchedulingMode::SharedPool {
                        max_messages_per_run: 1,
                    }),
                    mailbox_capacity: Some(4),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        tasks.push(tokio::spawn(async move {
            for value in 0..80 {
                actor.deliver(Box::new(Input::Value(value))).await.unwrap();
            }
            let seen = actor
                .send_with_timeout(Box::new(Input::Snapshot), Some(Duration::from_secs(5)))
                .await
                .unwrap();
            assert_eq!(
                *seen.downcast::<Vec<u64>>().unwrap(),
                (0..80).collect::<Vec<_>>()
            );
        }));
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        for task in tasks {
            task.await.unwrap();
        }
    })
    .await
    .unwrap();
    system.shutdown_internal().await.unwrap();
    let metrics = system.scheduler_metrics().unwrap();
    assert_eq!(
        (
            metrics.queue_length,
            metrics.local_ready,
            metrics.deferred_ready
        ),
        (0, 0, 0)
    );
}
