//! Late stop calls must retain the final failed generation's cleanup receipt.
use parrot::thread::{SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
struct Probe {
    fails: bool,
    drop_fails: bool,
    drops: Arc<AtomicUsize>,
}
impl Actor for Probe {
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
            if self.fails {
                Err(ActorError::MessageHandlingError(
                    "terminal test failure".into(),
                ))
            } else {
                Ok(message)
            }
        })
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.drop_fails {
            panic!("terminal test destructor failed");
        }
    }
}
async fn check(mode: SchedulingMode, cleanup_fails: bool) {
    let system = ThreadActorSystem::shared(Default::default());
    let drops = Arc::new(AtomicUsize::new(0));
    let path = "/user/terminal-receipt";
    let config = ThreadActorConfig {
        scheduling_mode: Some(mode),
        ..Default::default()
    };
    let old = system
        .spawn_at(
            Probe {
                fails: true,
                drop_fails: cleanup_fails,
                drops: drops.clone(),
            },
            path,
            None,
            config.clone(),
        )
        .await
        .unwrap();
    let clone = old.clone_boxed();
    assert!(old.send(Box::new(())).await.is_err());
    tokio::time::timeout(Duration::from_secs(10), async {
        while system.get_actor_ref(path).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed registration must retire");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    // Reuse the text path before consulting old handles. No stop call was made
    // before registry removal, so this cannot rely on an earlier cached caller.
    let replacement = system
        .spawn_at(
            Probe {
                fails: false,
                drop_fails: false,
                drops: drops.clone(),
            },
            path,
            None,
            config,
        )
        .await
        .unwrap();
    let first = old.stop().await;
    let second = clone.stop().await;
    let third = old.stop().await;
    let replacement_reply = replacement.send(Box::new(42u64)).await;
    replacement.stop().await.unwrap();
    system.shutdown_internal().await.unwrap();
    for result in [first, second, third] {
        if cleanup_fails {
            assert!(
                matches!(&result, Err(ActorError::Panic(message)) if message.contains("terminal test destructor failed")),
                "{result:?}"
            );
        } else {
            result.unwrap();
        }
    }
    assert_eq!(*replacement_reply.unwrap().downcast::<u64>().unwrap(), 42);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn shared_clean_terminal_receipt_survives_registry_removal() {
    check(
        SchedulingMode::SharedPool {
            max_messages_per_run: 8,
        },
        false,
    )
    .await;
}
#[tokio::test]
async fn shared_failed_terminal_receipt_survives_registry_removal() {
    check(
        SchedulingMode::SharedPool {
            max_messages_per_run: 8,
        },
        true,
    )
    .await;
}
#[tokio::test]
async fn dedicated_clean_terminal_receipt_survives_registry_removal() {
    check(SchedulingMode::DedicatedThread, false).await;
}
#[tokio::test]
async fn dedicated_failed_terminal_receipt_survives_registry_removal() {
    check(SchedulingMode::DedicatedThread, true).await;
}
#[tokio::test]
async fn sharded_clean_terminal_receipt_survives_registry_removal() {
    check(
        SchedulingMode::Sharded {
            affinity_key: "terminal-receipt".into(),
        },
        false,
    )
    .await;
}
#[tokio::test]
async fn sharded_failed_terminal_receipt_survives_registry_removal() {
    check(
        SchedulingMode::Sharded {
            affinity_key: "terminal-receipt".into(),
        },
        true,
    )
    .await;
}
