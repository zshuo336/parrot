// SPDX-License-Identifier: AGPL-3.0-only
use super::{seen, start, state, strict, Empty, HeldDrop, Release, State, Target, Work};
use parrot_api::errors::ActorError;
use parrot_api::types::BoxedActorRef;
use parrot_remote::{handshake::TopologyRole, transport::RuntimeKnobs, LocalLookup, RemoteConfig};
use std::sync::{Arc, RwLock};
use std::time::Duration;

struct Replaceable(RwLock<Target>);

#[async_trait::async_trait]
impl LocalLookup for Replaceable {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/target").then(|| Box::new(self.0.read().unwrap().clone()) as BoxedActorRef)
    }
}

async fn replace_after_admission(routed: bool) {
    let (old, mut old_messages) = state();
    let (new, mut new_messages) = state();
    let lookup = Arc::new(Replaceable(RwLock::new(Target(old.clone()))));
    let label = if routed { "routed" } else { "direct" };
    let endpoint = format!("pin-target-{label}");
    let source = start(
        RemoteConfig::mem(format!("pin-source-{label}")),
        Arc::new(Empty),
    )
    .await;
    let target = start(
        RemoteConfig::mem(&endpoint).with_ordered_relays(["pin-hub".into()]),
        lookup.clone(),
    )
    .await;
    let hub = if routed {
        let hub = start(
            RemoteConfig::mem("pin-hub").with_role(TopologyRole::Hub),
            Arc::new(Empty),
        )
        .await;
        target.connect_mem_pair(&hub).await.unwrap();
        source.connect_mem_pair(&hub).await.unwrap();
        Some(hub)
    } else {
        source.connect_mem_pair(&target).await.unwrap();
        None
    };
    let reference = source
        .remote_ref(&format!("parrot://{endpoint}/target"))
        .unwrap();
    strict(&reference, 701).await;
    tokio::time::timeout(Duration::from_secs(3), old.entered.notified())
        .await
        .unwrap();
    strict(&reference, 702).await;
    *lookup.0.write().unwrap() = Target(new);
    old.resume.notify_one();
    strict(&reference, 703).await;
    seen(&mut old_messages, 701).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), new_messages.recv())
            .await
            .unwrap(),
        Some(703),
        "a new actor received work admitted for its predecessor"
    );
    seen(&mut old_messages, 702).await;
    reference
        .close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    assert!(old_messages.try_recv().is_err() && new_messages.try_recv().is_err());
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
    if let Some(hub) = hub {
        hub.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn direct_admitted_work_stays_with_its_actor() {
    replace_after_admission(false).await;
}

#[tokio::test]
async fn routed_admitted_work_stays_with_its_actor() {
    replace_after_admission(true).await;
}

#[tokio::test]
async fn missing_actor_does_not_consume_the_next_sequence() {
    let (state, mut received) = state();
    let source = start(RemoteConfig::mem("pin-missing-source"), Arc::new(Empty)).await;
    let target = start(
        RemoteConfig::mem("pin-missing-target").with_knobs(RuntimeKnobs {
            reorder_gap_timeout_ms: 20_000,
            ..Default::default()
        }),
        Arc::new(Target(state)),
    )
    .await;
    source.connect_mem_pair(&target).await.unwrap();
    let reference = source
        .remote_ref("parrot://pin-missing-target/target")
        .unwrap();
    let absent = source
        .remote_ref("parrot://pin-missing-target/missing")
        .unwrap();
    strict(&reference, 11).await;
    seen(&mut received, 11).await;
    let rejected = absent
        .deliver_ordered(Box::new(Work(12)), Default::default())
        .await;
    assert!(
        matches!(rejected, Err(ActorError::ActorNotFound(_))),
        "{rejected:?}"
    );
    strict(&reference, 13).await;
    seen(&mut received, 13).await;
    reference
        .close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

struct HeldLookup(Arc<State>);
#[async_trait::async_trait]
impl LocalLookup for HeldLookup {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        HeldDrop(self.0.clone()).await.unwrap();
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_joins_a_cancelled_actor_lookup_destructor() {
    let (held, _received) = state();
    let release = Release(held.clone());
    let source = start(RemoteConfig::mem("pin-close-source"), Arc::new(Empty)).await;
    let target = start(
        RemoteConfig::mem("pin-close-target"),
        Arc::new(HeldLookup(held.clone())),
    )
    .await;
    source.connect_mem_pair(&target).await.unwrap();
    let reference = source
        .remote_ref("parrot://pin-close-target/target")
        .unwrap();
    let sending = reference.clone();
    let send = tokio::spawn(async move {
        sending
            .deliver_ordered(Box::new(Work(14)), Default::default())
            .await
    });
    let deadline = Duration::from_secs(4);
    tokio::time::timeout(deadline, held.entered.notified())
        .await
        .unwrap();
    send.abort();
    assert!(send.await.unwrap_err().is_cancelled());
    let mut close = tokio::spawn(async move { reference.close_ordered_session(deadline).await });
    tokio::time::timeout(deadline, held.dropping.notified())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(80), &mut close)
            .await
            .is_err(),
        "retirement returned while the admitted lookup destructor was blocked"
    );
    release.release();
    close.await.unwrap().unwrap();
    assert!(held.dropped.load(std::sync::atomic::Ordering::SeqCst));
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}
