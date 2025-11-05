use parrot::thread::{
    config::ThreadActorSystemConfig,
    context::ThreadContext,
    error::SystemError,
    system::{Terminated, ThreadActorSystem},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

struct Watcher {
    entered: Arc<Notify>,
    observed: Arc<Notify>,
}
impl Actor for Watcher {
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
            if msg.is::<Terminated>() || msg.is::<parrot_api::context::ActorTerminated>() {
                self.observed.notify_one();
            }
            if msg.downcast_ref::<u64>() == Some(&0) {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(msg)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn common_full_mailbox_delivery_is_bounded_observable_and_cancellable() {
    for cancel in [false, true] {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            default_mailbox_capacity: 1,
            shared_pool_size: 2,
            shutdown_timeout: Duration::from_millis(200),
            ..Default::default()
        });
        let entered = Arc::new(Notify::new());
        let observed = Arc::new(Notify::new());
        for path in ["/watched", "/wedged", "/healthy"] {
            system
                .spawn_at(
                    Watcher {
                        entered: entered.clone(),
                        observed: observed.clone(),
                    },
                    path,
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
        }
        let target = system.get_actor_ref("/watched").unwrap();
        let wedged = system.get_actor_ref("/wedged").unwrap();
        let healthy = system.get_actor_ref("/healthy").unwrap();
        wedged.deliver(Box::new(0_u64)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        wedged.deliver(Box::new(1_u64)).await.unwrap();
        target.watch(wedged.clone_boxed()).await.unwrap();
        target.watch(healthy.clone_boxed()).await.unwrap();
        let handle = target.death_watch().unwrap();
        let stopping = target.clone_boxed();
        let stop = tokio::spawn(async move { stopping.stop().await });
        tokio::time::timeout(Duration::from_secs(1), observed.notified())
            .await
            .unwrap();
        // The healthy notification proves retirement has entered delivery.
        // The wedged mailbox still has no capacity, so its edge must be pending.
        assert_eq!(wedged.death_watch().unwrap().subscriptions().1, 1);
        if cancel {
            target.unwatch(wedged.clone_boxed()).await.unwrap();
        }
        let result = tokio::time::timeout(Duration::from_secs(1), stop)
            .await
            .unwrap()
            .unwrap();
        if cancel {
            result.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while system.get_actor_ref("/watched").is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(handle.delivery_failures(), u64::from(!cancel));
        assert_eq!(wedged.death_watch().unwrap().subscriptions().1, 0);
        let _ = system.shutdown_internal().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_watcher_cannot_block_other_watchers_or_hide_delivery_failure() {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        default_mailbox_capacity: 1,
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_millis(50),
        ..Default::default()
    });
    let entered = Arc::new(Notify::new());
    let observed = Arc::new(Notify::new());
    for path in ["/watched", "/wedged", "/healthy"] {
        system
            .spawn_at(
                Watcher {
                    entered: entered.clone(),
                    observed: observed.clone(),
                },
                path,
                None,
                Default::default(),
            )
            .await
            .unwrap();
    }
    let wedged = system.get_actor_ref("/wedged").unwrap();
    wedged.deliver(Box::new(0_u64)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    wedged.deliver(Box::new(1_u64)).await.unwrap();
    system
        .watch("/wedged".into(), "/watched".into())
        .await
        .unwrap();
    system
        .watch("/healthy".into(), "/watched".into())
        .await
        .unwrap();
    let (stopped, notified) = tokio::join!(
        tokio::time::timeout(Duration::from_millis(500), system.stop_actor("/watched")),
        tokio::time::timeout(Duration::from_millis(500), observed.notified())
    );
    let _ = system.shutdown_internal().await;
    assert!(
        matches!(stopped, Ok(Err(SystemError::Timeout(_)))),
        "stop must report notification deadline: {stopped:?}"
    );
    assert!(
        notified.is_ok(),
        "healthy watcher must receive its notification independently"
    );
}
