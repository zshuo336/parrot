//! This file intentionally uses only the pre-session public API so it can run
//! unchanged against a real prior library.
use parrot_api::{
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    handshake::TopologyRole,
    topology::{TopologyConfig, TopologyMode},
    LocalLookup, RemoteActorSystem, RemoteConfig,
};
use std::{sync::Arc, time::Duration};

#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Work(u64);
impl parrot_api::Message for Work {
    type Result = ();
}

#[derive(Debug)]
struct State {
    entered: tokio::sync::Notify,
    second: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    delivered: tokio::sync::mpsc::UnboundedSender<u64>,
}
#[derive(Clone, Debug)]
struct Target(Arc<State>);
#[async_trait::async_trait]
impl LocalLookup for Target {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        Some(Box::new(self.clone()))
    }
}
impl ActorRef for Target {
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let value = message.downcast_ref::<Work>().unwrap().0;
            if value == 1 {
                self.0.entered.notify_one();
                self.0.release.acquire().await.unwrap().forget();
            } else if value == 2 {
                self.0.second.notify_one();
            }
            self.0.delivered.send(value).unwrap();
            Ok(())
        })
    }
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_tell_keeps_fifo_when_a_blocked_hub_path_changes_to_direct() {
    let (delivered, mut received) = tokio::sync::mpsc::unbounded_channel();
    let state = Arc::new(State {
        entered: tokio::sync::Notify::new(),
        second: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        delivered,
    });
    let source = RemoteActorSystem::new(
        RemoteConfig::mem("route-source")
            .with_insecure_development()
            .with_topology(TopologyConfig {
                mode: TopologyMode::Hybrid,
                ..Default::default()
            }),
        Arc::new(Target(state.clone())),
    )
    .unwrap();
    let hub = RemoteActorSystem::new(
        RemoteConfig::mem("route-hub")
            .with_insecure_development()
            .with_role(TopologyRole::Hub),
        Arc::new(Target(state.clone())),
    )
    .unwrap();
    let target = RemoteActorSystem::new(
        RemoteConfig::mem("route-target").with_insecure_development(),
        Arc::new(Target(state.clone())),
    )
    .unwrap();
    target.ingress.set_reorder_knobs(Duration::ZERO, 1024);
    for system in [&source, &hub, &target] {
        system.start().await.unwrap();
    }
    source.connect_mem_pair(&hub).await.unwrap();
    target.connect_mem_pair(&hub).await.unwrap();
    let reference = source.remote_ref("parrot://route-target/target").unwrap();
    reference.deliver(Box::new(Work(1))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), state.entered.notified())
        .await
        .unwrap();
    source.connect_mem_pair(&target).await.unwrap();
    reference.deliver(Box::new(Work(2))).await.unwrap();
    // A real independent ASK proves the new physical link can make progress
    // while the first TELL's application future remains blocked.
    reference
        .send_with_timeout(Box::new(Work(7)), Some(Duration::from_secs(3)))
        .await
        .unwrap();
    let overtook = tokio::time::timeout(Duration::from_millis(300), state.second.notified())
        .await
        .is_ok();
    state.release.add_permits(1);
    let mut values = Vec::new();
    for _ in 0..2 {
        values.push(
            tokio::time::timeout(Duration::from_secs(3), received.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    for system in [&source, &hub, &target] {
        system.shutdown().await.unwrap();
    }
    assert!(
        !overtook,
        "new direct TELL overtook an admitted hub TELL from the same origin"
    );
    assert_eq!(
        values,
        vec![1, 2],
        "route selection split one source's ordering state"
    );
}
