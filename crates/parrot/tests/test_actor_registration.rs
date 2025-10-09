use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use parrot::thread::{
    config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig},
    context::ThreadContext,
    system::ThreadActorSystem,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};

struct InitActor {
    starts: Arc<AtomicUsize>,
    panic_init: bool,
}
impl Actor for InitActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.starts.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic_init, "deliberate initialization panic");
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(msg) })
    }
}

fn modes() -> [SchedulingMode; 3] {
    [
        SchedulingMode::SharedPool {
            max_messages_per_run: 10,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "registration".into(),
        },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_duplicate_spawns_initialize_exactly_one_actor() {
    for mode in modes() {
        let system = ThreadActorSystem::shared(Default::default());
        let starts = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut attempts = vec![];
        for _ in 0..16 {
            let (system, starts, barrier, mode) = (
                system.clone(),
                starts.clone(),
                barrier.clone(),
                mode.clone(),
            );
            attempts.push(tokio::spawn(async move {
                barrier.wait().await;
                system
                    .spawn_at(
                        InitActor {
                            starts,
                            panic_init: false,
                        },
                        "/same",
                        None,
                        ThreadActorConfig {
                            scheduling_mode: Some(mode),
                            ..Default::default()
                        },
                    )
                    .await
            }));
        }
        let mut winners = vec![];
        for attempt in attempts {
            match attempt.await.unwrap() {
                Ok(actor) => winners.push(actor),
                Err(parrot::thread::error::SpawnError::ActorPathAlreadyExists(_)) => {}
                Err(error) => panic!("wrong duplicate-spawn error: {error}"),
            }
        }
        assert_eq!(winners.len(), 1);
        assert!(
            winners[0]
                .send_with_timeout(Box::new(()), Some(Duration::from_secs(2)))
                .await
                .is_ok()
        );
        assert_eq!(system.actor_count(), 1);
        system.shutdown_internal().await.unwrap();
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "losing spawns must never execute user initialization"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialization_panic_unregisters_actor_and_preserves_scheduler() {
    for mode in modes() {
        let system = ThreadActorSystem::shared(Default::default());
        let config = ThreadActorConfig {
            scheduling_mode: Some(mode),
            ..Default::default()
        };
        let actor = system
            .spawn_at(
                InitActor {
                    starts: Default::default(),
                    panic_init: true,
                },
                "/bad-init",
                None,
                config.clone(),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while system.actor_count() != 0 || actor.is_alive().await {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("initialization failure must reach system supervision");
        assert!(!actor.is_alive().await);
        let healthy = system
            .spawn_at(
                InitActor {
                    starts: Default::default(),
                    panic_init: false,
                },
                "/healthy",
                None,
                config,
            )
            .await
            .unwrap();
        let reply = healthy
            .send_with_timeout(Box::new(42_u64), Some(Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(*reply.downcast::<u64>().unwrap(), 42);
        system.shutdown_internal().await.unwrap();
    }
}

#[tokio::test]
async fn failed_scheduling_rolls_back_registration() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        max_dedicated_threads: 0,
        ..Default::default()
    });
    let starts = Arc::new(AtomicUsize::new(0));
    let result = system
        .spawn_at(
            InitActor {
                starts: starts.clone(),
                panic_init: false,
            },
            "/limit",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(SchedulingMode::DedicatedThread),
                ..Default::default()
            },
        )
        .await;
    assert!(result.is_err());
    assert_eq!(system.actor_count(), 0);
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    system.shutdown_internal().await.unwrap();
}
