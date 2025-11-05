use parrot::thread::{SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

struct Gate;
struct Inspect;
struct Probe {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    groups: Arc<Mutex<Vec<Vec<u64>>>>,
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn can_batch_message(&self, message: &BoxedMessage) -> bool {
        message.is::<u64>()
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if message.is::<Gate>() {
                self.entered.notify_one();
                self.release.notified().await;
                return Ok(Box::new(()) as BoxedMessage);
            }
            if message.is::<Inspect>() {
                return Ok(Box::new(self.groups.lock().unwrap().clone()) as BoxedMessage);
            }
            let n = *message.downcast_ref::<u64>().unwrap();
            self.groups.lock().unwrap().push(vec![n]);
            Ok(message)
        })
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        Box::pin(async move {
            self.groups.lock().unwrap().push(
                messages
                    .iter()
                    .map(|m| *m.downcast_ref::<u64>().unwrap())
                    .collect(),
            );
            messages.into_iter().map(Ok).collect()
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_shared_dedicated_and_sharded_schedulers_coalesce_backlogs_with_independent_replies() {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 10,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "batch".into(),
        },
    ] {
        for batch_size in [1, 3] {
            let engine = ThreadActorSystem::shared(Default::default());
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let groups = Arc::new(Mutex::new(Vec::new()));
            let actor = engine
                .spawn_at(
                    Probe {
                        entered: entered.clone(),
                        release: release.clone(),
                        groups: groups.clone(),
                    },
                    "/batch",
                    None,
                    ThreadActorConfig {
                        scheduling_mode: Some(mode.clone()),
                        message_batch_size: Some(batch_size),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let mut gate = actor.send_with_timeout(Box::new(Gate), Some(Duration::from_secs(5)));
            assert!(futures::poll!(&mut gate).is_pending());
            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .unwrap();
            let mut calls: Vec<_> = (0..32u64)
                .map(|n| actor.send_with_timeout(Box::new(n), Some(Duration::from_secs(5))))
                .collect();
            for call in &mut calls {
                assert!(futures::poll!(call).is_pending());
            }
            let mut inspect =
                actor.send_with_timeout(Box::new(Inspect), Some(Duration::from_secs(5)));
            assert!(futures::poll!(&mut inspect).is_pending());
            release.notify_one();
            gate.await.unwrap();
            for (n, result) in futures::future::join_all(calls)
                .await
                .into_iter()
                .enumerate()
            {
                assert_eq!(*result.unwrap().downcast::<u64>().unwrap(), n as u64);
            }
            let actual = *inspect.await.unwrap().downcast::<Vec<Vec<u64>>>().unwrap();
            assert_eq!(
                actual.iter().flatten().copied().collect::<Vec<_>>(),
                (0..32).collect::<Vec<_>>()
            );
            assert!(actual.iter().all(|group| group.len() <= batch_size));
            if batch_size == 1 {
                assert_eq!(actual.len(), 32);
            } else {
                assert!(actual.iter().any(|group| group.len() == 3));
            }
            actor.stop().await.unwrap();
            engine.shutdown_internal().await.unwrap();
        }
    }
}

#[tokio::test]
async fn invalid_batch_sizes_are_rejected_before_registering_actor_paths() {
    let engine = ThreadActorSystem::shared(Default::default());
    for size in [0, 257, usize::MAX] {
        let actor = Probe {
            entered: Arc::default(),
            release: Arc::default(),
            groups: Arc::default(),
        };
        assert!(
            engine
                .spawn_at(
                    actor,
                    "/invalid",
                    None,
                    ThreadActorConfig {
                        message_batch_size: Some(size),
                        ..Default::default()
                    }
                )
                .await
                .is_err()
        );
    }
    let actor = engine
        .spawn_at(
            Probe {
                entered: Arc::default(),
                release: Arc::default(),
                groups: Arc::default(),
            },
            "/invalid",
            None,
            ThreadActorConfig {
                message_batch_size: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    actor.stop().await.unwrap();
    engine.shutdown_internal().await.unwrap();
}

#[derive(Debug)]
struct AddHundred;
impl parrot::thread::mailbox::admission::MessageAdmission for AddHundred {
    fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, parrot::thread::MailboxError> {
        match message.downcast::<u64>() {
            Ok(n) => Ok(Box::new(*n + 100)),
            Err(message) => Ok(message),
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_prepares_single_and_batch_messages_on_all_real_schedulers() {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 10,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "admission".into(),
        },
    ] {
        for limit in [1, 3] {
            let engine = ThreadActorSystem::shared(Default::default());
            let actor = engine
                .spawn_at(
                    Probe {
                        entered: Arc::default(),
                        release: Arc::default(),
                        groups: Arc::default(),
                    },
                    "/admission",
                    None,
                    ThreadActorConfig {
                        message_admission: Some(Arc::new(AddHundred)),
                        message_batch_size: Some(limit),
                        scheduling_mode: Some(mode.clone()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            actor.deliver(Box::new(1u64)).await.unwrap();
            let replies = futures::future::join_all(
                (2..=5u64)
                    .map(|n| actor.send_with_timeout(Box::new(n), Some(Duration::from_secs(5)))),
            )
            .await;
            for (index, reply) in replies.into_iter().enumerate() {
                assert_eq!(
                    *reply.unwrap().downcast::<u64>().unwrap(),
                    index as u64 + 102
                );
            }
            let events = actor
                .send_with_timeout(Box::new(Inspect), Some(Duration::from_secs(5)))
                .await
                .unwrap()
                .downcast::<Vec<Vec<u64>>>()
                .unwrap();
            assert_eq!(
                events.iter().flatten().copied().collect::<Vec<_>>(),
                vec![101, 102, 103, 104, 105]
            );
            actor.stop().await.unwrap();
            engine.shutdown_internal().await.unwrap();
        }
    }
}
