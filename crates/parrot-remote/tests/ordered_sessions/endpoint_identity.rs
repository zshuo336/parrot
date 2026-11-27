// SPDX-License-Identifier: AGPL-3.0-only
use super::{seen, start, state, strict, Empty, HeldDrop, Pki, Release, State, Target, Work};
use parrot_api::errors::ActorError;
use parrot_api::types::BoxedActorRef;
use parrot_remote::{
    handshake::TopologyRole, transport::RuntimeKnobs, LocalLookup, NodeAddr, RemoteConfig,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

#[derive(Default)]
struct AliasLookup(RwLock<Option<String>>);

#[async_trait::async_trait]
impl LocalLookup for AliasLookup {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
    fn forwarding_target(&self, path: &str) -> Option<String> {
        if path == "/alias" {
            self.0.read().unwrap().clone()
        } else {
            None
        }
    }
}

#[tokio::test]
async fn endpoint_lookup_follows_authorized_aliases_and_observes_remapping() {
    let pki = Pki::new();
    let config = |name: &str| {
        RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap())).with_mtls(pki.node(name), [])
    };
    let source = start(config("resolve-src"), Arc::new(Empty)).await;
    let hub = start(
        config("resolve-hub").with_role(TopologyRole::Hub),
        Arc::new(Empty),
    )
    .await;
    let aliases = Arc::new(AliasLookup::default());
    let host = start(
        config("resolve-host").with_ordered_relays(["resolve-hub".into()]),
        aliases.clone(),
    )
    .await;
    let (first_actor, mut first_messages) = state();
    let first = start(
        config("resolve-first").with_ordered_relays(["resolve-host".into()]),
        Arc::new(Target(first_actor)),
    )
    .await;
    let (second_actor, mut second_messages) = state();
    let second = start(
        config("resolve-second").with_ordered_relays(["resolve-host".into()]),
        Arc::new(Target(second_actor)),
    )
    .await;
    let hub_address = NodeAddr::tcp("resolve-hub", hub.local_addr().unwrap());
    source.connect(&hub_address).await.unwrap();
    host.connect(&hub_address).await.unwrap();
    host.connect(&NodeAddr::tcp("resolve-first", first.local_addr().unwrap()))
        .await
        .unwrap();
    host.connect(&NodeAddr::tcp(
        "resolve-second",
        second.local_addr().unwrap(),
    ))
    .await
    .unwrap();
    let deadline = Duration::from_secs(3);
    let alias = source.remote_ref("parrot://resolve-host/alias").unwrap();
    let mut observations = Vec::new();
    for (node, path) in [
        (&first, "parrot://resolve-first/target"),
        (&second, "parrot://resolve-second/target"),
    ] {
        *aliases.0.write().unwrap() = Some(path.into());
        let through_hub = alias.resolve_ordered_endpoint(deadline).await.unwrap();
        assert_eq!(through_hub.path, path);
        source
            .connect(&NodeAddr::tcp(
                &node.config.node_id,
                node.local_addr().unwrap(),
            ))
            .await
            .unwrap();
        let direct = source.remote_ref(path).unwrap();
        assert_eq!(
            direct.resolve_ordered_endpoint(deadline).await.unwrap(),
            through_hub
        );
        observations.push(through_hub.receiver);
    }
    assert_ne!(observations[0], observations[1]);
    assert!(matches!(
        first_messages.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        second_messages.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    *aliases.0.write().unwrap() = Some("parrot://resolve-second/missing".into());
    assert!(matches!(
        alias.resolve_ordered_endpoint(deadline).await,
        Err(ActorError::ActorNotFound(_))
    ));
    second
        .ingress
        .set_ordered_relays(Default::default())
        .unwrap();
    *aliases.0.write().unwrap() = Some("parrot://resolve-second/target".into());
    assert!(matches!(
        alias.resolve_ordered_endpoint(deadline).await,
        Err(ActorError::MessageHandlingError(detail)) if detail.starts_with("not remotable:")
    ));
    for node in [&source, &hub, &host, &first, &second] {
        node.shutdown().await.unwrap();
    }
}

struct Replaceable(RwLock<Target>);

#[tokio::test]
async fn mtls_hub_alias_close_cannot_cancel_a_reopened_direct_session() {
    let pki = Pki::new();
    let config = |name: &str| {
        RemoteConfig::tcp(name, Some("127.0.0.1:0".parse().unwrap())).with_mtls(pki.node(name), [])
    };
    let source = start(
        config("shared-tls-src").with_knobs(RuntimeKnobs {
            reorder_stream_capacity: 1,
            ..Default::default()
        }),
        Arc::new(Empty),
    )
    .await;
    let hub = start(
        config("shared-tls-hub").with_role(TopologyRole::Hub),
        Arc::new(Empty),
    )
    .await;
    let host = start(
        config("shared-tls-host").with_ordered_relays(["shared-tls-hub".into()]),
        Arc::new(AliasLookup(RwLock::new(Some(
            "parrot://shared-tls-end/target".into(),
        )))),
    )
    .await;
    let (actor, mut received) = state();
    let end = start(
        config("shared-tls-end")
            .with_ordered_relays(["shared-tls-hub".into(), "shared-tls-host".into()]),
        Arc::new(Target(actor.clone())),
    )
    .await;
    let hub_addr = NodeAddr::tcp("shared-tls-hub", hub.local_addr().unwrap());
    let end_addr = NodeAddr::tcp("shared-tls-end", end.local_addr().unwrap());
    source.connect(&hub_addr).await.unwrap();
    host.connect(&hub_addr).await.unwrap();
    host.connect(&end_addr).await.unwrap();
    end.connect(&hub_addr).await.unwrap();
    let alias = source.remote_ref("parrot://shared-tls-host/alias").unwrap();
    let direct = source.remote_ref("parrot://shared-tls-end/target").unwrap();
    strict(&direct, 40).await;
    seen(&mut received, 40).await;
    strict(&alias, 41).await;
    seen(&mut received, 41).await;
    source.connect(&end_addr).await.unwrap();
    let timeout = Duration::from_secs(3);
    direct.close_ordered_session(timeout).await.unwrap();
    strict(&direct, 701).await;
    tokio::time::timeout(timeout, actor.entered.notified())
        .await
        .unwrap();
    alias.close_ordered_session(timeout).await.unwrap();
    strict(&direct, 42).await;
    actor.resume.notify_one();
    for value in [701, 42] {
        seen(&mut received, value).await;
    }
    direct.close_ordered_session(timeout).await.unwrap();
    for node in [&source, &hub, &host, &end] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remapped_alias_retries_only_after_the_old_receiver_has_retired() {
    let routes = Arc::new(AliasLookup(RwLock::new(Some(
        "parrot://remap-old/target".into(),
    ))));
    let (held, _old_messages) = state();
    let release = Release(held.clone());
    let (next_actor, mut next_messages) = state();
    let source = start(RemoteConfig::mem("remap-src"), Arc::new(Empty)).await;
    let host = start(RemoteConfig::mem("remap-host"), routes.clone()).await;
    let old = start(
        RemoteConfig::mem("remap-old").with_ordered_relays(["remap-host".into()]),
        Arc::new(Target(held.clone())),
    )
    .await;
    let next = start(
        RemoteConfig::mem("remap-next").with_ordered_relays(["remap-host".into()]),
        Arc::new(Target(next_actor)),
    )
    .await;
    source.connect_mem_pair(&host).await.unwrap();
    host.connect_mem_pair(&old).await.unwrap();
    host.connect_mem_pair(&next).await.unwrap();
    let reference = source.remote_ref("parrot://remap-host/alias").unwrap();
    strict(&reference, 700).await;
    let timeout = Duration::from_secs(3);
    tokio::time::timeout(timeout, held.entered.notified())
        .await
        .unwrap();
    *routes.0.write().unwrap() = Some("parrot://remap-next/target".into());
    let send = |value| {
        let reference = reference.clone();
        tokio::spawn(async move {
            reference
                .deliver_ordered(Box::new(Work(value)), Default::default())
                .await
        })
    };
    let interrupted = send(801);
    tokio::time::timeout(timeout, held.dropping.notified())
        .await
        .unwrap();
    interrupted.abort();
    assert!(interrupted.await.unwrap_err().is_cancelled());
    let mut retry = send(802);
    assert!(tokio::time::timeout(Duration::from_millis(80), &mut retry)
        .await
        .is_err());
    assert!(matches!(
        next_messages.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    release.release();
    retry.await.unwrap().unwrap();
    assert!(held.dropped.load(Ordering::Acquire));
    seen(&mut next_messages, 802).await;
    reference.close_ordered_session(timeout).await.unwrap();
    for node in [&source, &host, &old, &next] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn strict_alias_and_direct_path_share_the_receiving_worker() {
    let routes = Arc::new(AliasLookup(RwLock::new(Some(
        "parrot://shared-end/target".into(),
    ))));
    let (actor, mut received) = state();
    let source = start(RemoteConfig::mem("shared-src"), Arc::new(Empty)).await;
    let host = start(RemoteConfig::mem("shared-host"), routes).await;
    let end = start(
        RemoteConfig::mem("shared-end").with_ordered_relays(["shared-host".into()]),
        Arc::new(Target(actor.clone())),
    )
    .await;
    source.connect_mem_pair(&host).await.unwrap();
    host.connect_mem_pair(&end).await.unwrap();
    let alias = source.remote_ref("parrot://shared-host/alias").unwrap();
    strict(&alias, 701).await;
    tokio::time::timeout(Duration::from_secs(3), actor.entered.notified())
        .await
        .unwrap();
    source.connect_mem_pair(&end).await.unwrap();
    let direct = source.remote_ref("parrot://shared-end/target").unwrap();
    strict(&direct, 702).await;
    strict(&alias, 703).await;
    actor.resume.notify_one();
    for value in [701, 702, 703] {
        seen(&mut received, value).await;
    }
    alias
        .close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    for node in [&source, &host, &end] {
        node.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn cyclic_alias_lookups_end_at_the_hop_limit_and_leave_the_link_usable() {
    let source = start(
        RemoteConfig::mem("cycle-src").with_knobs(RuntimeKnobs {
            default_hop_limit: 3,
            ..Default::default()
        }),
        Arc::new(Empty),
    )
    .await;
    let left_alias = Arc::new(AliasLookup(RwLock::new(Some(
        "parrot://cycle-right/alias".into(),
    ))));
    let left = start(
        RemoteConfig::mem("cycle-left").with_ordered_relays(["cycle-right".into()]),
        left_alias.clone(),
    )
    .await;
    let right = start(
        RemoteConfig::mem("cycle-right").with_ordered_relays(["cycle-left".into()]),
        Arc::new(AliasLookup(RwLock::new(Some(
            "parrot://cycle-left/alias".into(),
        )))),
    )
    .await;
    source.connect_mem_pair(&left).await.unwrap();
    left.connect_mem_pair(&right).await.unwrap();
    let reference = source.remote_ref("parrot://cycle-left/alias").unwrap();
    let timeout = Duration::from_secs(2);
    let error = reference
        .resolve_ordered_endpoint(timeout)
        .await
        .unwrap_err();
    assert!(
        matches!(error,ActorError::ActorNotFound(ref detail) if detail.contains("hop limit")),
        "{error:?}"
    );
    let (actor, mut messages) = state();
    let target = start(
        RemoteConfig::mem("cycle-end").with_ordered_relays(["cycle-left".into()]),
        Arc::new(Target(actor)),
    )
    .await;
    left.connect_mem_pair(&target).await.unwrap();
    *left_alias.0.write().unwrap() = Some("parrot://cycle-end/target".into());
    assert_eq!(
        reference
            .resolve_ordered_endpoint(timeout)
            .await
            .unwrap()
            .path,
        "parrot://cycle-end/target"
    );
    assert!(matches!(
        messages.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    for node in [&source, &left, &right, &target] {
        node.shutdown().await.unwrap();
    }
}

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

struct FallibleRoute {
    actor: Target,
    fail_next: AtomicBool,
}

#[async_trait::async_trait]
impl LocalLookup for FallibleRoute {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        self.actor.lookup(path).await
    }

    fn forwarding_target(&self, _: &str) -> Option<String> {
        assert!(
            !self.fail_next.swap(false, Ordering::AcqRel),
            "injected route lookup failure"
        );
        None
    }
}

#[tokio::test]
async fn failed_route_lookup_preserves_the_next_ordered_delivery() {
    let (actor, mut received) = state();
    let lookup = Arc::new(FallibleRoute {
        actor: Target(actor),
        fail_next: AtomicBool::new(false),
    });
    let source = start(RemoteConfig::mem("route-failure-source"), Arc::new(Empty)).await;
    let target = start(
        RemoteConfig::mem("route-failure-target").with_knobs(RuntimeKnobs {
            reorder_gap_timeout_ms: 20_000,
            ..Default::default()
        }),
        lookup.clone(),
    )
    .await;
    source.connect_mem_pair(&target).await.unwrap();
    let reference = source
        .remote_ref("parrot://route-failure-target/target")
        .unwrap();
    strict(&reference, 21).await;
    seen(&mut received, 21).await;
    lookup.fail_next.store(true, Ordering::Release);
    let error = reference
        .deliver_ordered(Box::new(Work(22)), Default::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, ActorError::InternalError(ref detail) if detail.contains("protocol violation"))
    );
    strict(&reference, 23).await;
    seen(&mut received, 23).await;
    reference
        .close_ordered_session(Duration::from_secs(3))
        .await
        .unwrap();
    assert!(received.try_recv().is_err());
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
