use remote_poc::*;
use std::sync::Arc;
use std::time::Duration;

fn member(id: &str) -> Member {
    Member {
        node_id: id.into(),
        addr: id.into(),
        incarnation: 0,
        status: MemberStatus::Alive,
    }
}
async fn elapse(duration: Duration) {
    for _ in 0..duration.as_millis().div_ceil(25) {
        tokio::time::advance(Duration::from_millis(25)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }
}
async fn observed(node: &SwimNode, id: &str) -> Option<Member> {
    node.members().await.into_iter().find(|m| m.node_id == id)
}

#[tokio::test(start_paused = true)]
async fn default_runtime_discovers_peers_without_caller_scheduled_gossip() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    a.merge_membership(vec![member("B")]).await.unwrap();
    let ar = a.start(&bus).unwrap();
    let br = b.start(&bus).unwrap();
    elapse(Duration::from_secs(3)).await;
    assert_eq!(observed(&b, "A").await, Some(member("A")));
    assert_eq!(a.members().await, b.members().await);
    ar.shutdown().await.unwrap();
    br.shutdown().await.unwrap();
    assert_eq!(bus.registered_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn default_runtime_expires_suspicion_for_a_registered_silent_peer() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    a.merge_membership(vec![member("silent")]).await.unwrap();
    let ar = a.start(&bus).unwrap();
    let silent = bus.register("silent").unwrap();
    elapse(Duration::from_secs(2)).await;
    assert_eq!(
        observed(&a, "silent").await.unwrap().status,
        MemberStatus::Suspect
    );
    elapse(Duration::from_secs(4)).await;
    assert_eq!(
        observed(&a, "silent").await.unwrap().status,
        MemberStatus::Dead
    );
    assert_eq!(a.pending_count(), 0);
    ar.shutdown().await.unwrap();
    drop(silent);
    assert_eq!(bus.registered_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn healed_partition_refutes_dead_incarnations_without_manual_intervention() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    a.merge_membership(vec![member("B")]).await.unwrap();
    b.merge_membership(vec![member("A")]).await.unwrap();
    let ar = a.start(&bus).unwrap();
    let br = b.start(&bus).unwrap();
    bus.partition("B");
    elapse(Duration::from_secs(7)).await;
    assert_eq!(observed(&a, "B").await.unwrap().status, MemberStatus::Dead);
    bus.heal("B");
    elapse(Duration::from_secs(3)).await;
    let recovered = observed(&a, "B").await.unwrap();
    assert_eq!(recovered.status, MemberStatus::Alive);
    assert!(recovered.incarnation > 0);
    assert_eq!(a.members().await, b.members().await);
    ar.shutdown().await.unwrap();
    br.shutdown().await.unwrap();
}

fn fast() -> SwimConfig {
    SwimConfig {
        probe_interval: Duration::from_millis(100),
        probe_timeout: Duration::from_millis(50),
        indirect_probes: 1,
        suspect_timeout: Duration::from_millis(300),
        gossip_interval: Duration::from_millis(100),
        gossip_fanout: 1,
    }
}

#[tokio::test(start_paused = true)]
async fn indirect_periodic_probes_keep_a_peer_alive_across_a_broken_direct_link() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    let c = SwimNode::new("C");
    a.merge_membership(vec![member("B"), member("C")])
        .await
        .unwrap();
    let br = b.start_manual(&bus).unwrap();
    let cr = c.start_manual(&bus).unwrap();
    bus.block_link("A", "C");
    bus.block_link("C", "A");
    let ar = a
        .start_with(&bus, fast(), ClusterBindings::default())
        .unwrap();
    elapse(Duration::from_secs(3)).await;
    assert_eq!(observed(&a, "C").await, Some(member("C")));
    assert_eq!(observed(&b, "A").await, Some(member("A")));
    ar.shutdown().await.unwrap();
    br.shutdown().await.unwrap();
    cr.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn gossip_and_probe_rotation_reach_members_beyond_the_fanout() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let mut peers: Vec<_> = (0..7)
        .map(|n| {
            (
                format!("peer{n}"),
                bus.register(&format!("peer{n}")).unwrap(),
            )
        })
        .collect();
    a.merge_membership(peers.iter().map(|(id, _)| member(id)).collect())
        .await
        .unwrap();
    let mut config = fast();
    config.indirect_probes = 0;
    let ar = a
        .start_with(&bus, config, ClusterBindings::default())
        .unwrap();
    let mut probed = std::collections::BTreeSet::new();
    let mut gossiped = std::collections::BTreeSet::new();
    for _ in 0..80 {
        elapse(Duration::from_millis(25)).await;
        for (id, peer) in &mut peers {
            loop {
                let packet = tokio::select! { biased; p = peer.recv() => p, _ = std::future::ready(()) => None };
                let Some(packet) = packet else {
                    break;
                };
                assert_eq!(packet.from, "A");
                match packet.message {
                    SwimMsg::Ping { seq } => {
                        probed.insert(id.clone());
                        assert!(peer.send(
                            "A",
                            SwimMsg::Ack {
                                target: id.clone(),
                                seq
                            }
                        ));
                    }
                    SwimMsg::Membership(members) => {
                        gossiped.insert(id.clone());
                        assert_eq!(members.len(), 8);
                    }
                    other => panic!("unexpected message: {other:?}"),
                }
            }
        }
    }
    let expected = peers.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(probed, expected);
    assert_eq!(gossiped, expected);
    assert!(a
        .members()
        .await
        .iter()
        .all(|m| m.status == MemberStatus::Alive));
    ar.shutdown().await.unwrap();
    drop(peers);
    assert_eq!(bus.registered_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn duplicate_suspicion_gossip_and_failed_probes_cannot_extend_the_deadline() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let accused = Member {
        status: MemberStatus::Suspect,
        ..member("B")
    };
    a.merge_membership(vec![accused.clone()]).await.unwrap();
    let peer = bus.register("P").unwrap();
    let _silent = bus.register("B").unwrap();
    let ar = a
        .start_with(&bus, fast(), ClusterBindings::default())
        .unwrap();
    for _ in 0..5 {
        assert!(peer.send("A", SwimMsg::Membership(vec![accused.clone()])));
        elapse(Duration::from_millis(50)).await;
    }
    assert_eq!(
        observed(&a, "B").await.unwrap().status,
        MemberStatus::Suspect
    );
    for _ in 0..3 {
        assert!(peer.send("A", SwimMsg::Membership(vec![accused.clone()])));
        elapse(Duration::from_millis(50)).await;
    }
    assert_eq!(observed(&a, "B").await.unwrap().status, MemberStatus::Dead);
    ar.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn previous_incarnation_probe_timeout_cannot_accuse_a_new_member() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    a.merge_membership(vec![member("B")]).await.unwrap();
    let _silent = bus.register("B").unwrap();
    let mut config = fast();
    config.probe_interval = Duration::from_secs(1);
    config.probe_timeout = Duration::from_millis(100);
    let ar = a
        .start_with(&bus, config, ClusterBindings::default())
        .unwrap();
    elapse(Duration::from_millis(1050)).await;
    assert_eq!(a.pending_count(), 1);
    let replacement = Member {
        incarnation: 1,
        ..member("B")
    };
    a.merge_membership(vec![replacement.clone()]).await.unwrap();
    elapse(Duration::from_millis(100)).await;
    assert_eq!(a.pending_count(), 0);
    assert_eq!(observed(&a, "B").await, Some(replacement));
    ar.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn new_incarnation_suspicion_has_its_own_deadline() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    a.merge_membership(vec![Member {
        status: MemberStatus::Suspect,
        ..member("B")
    }])
    .await
    .unwrap();
    let ar = a
        .start_with(&bus, fast(), ClusterBindings::default())
        .unwrap();
    elapse(Duration::from_millis(250)).await;
    a.merge_membership(vec![Member {
        status: MemberStatus::Suspect,
        incarnation: 1,
        ..member("B")
    }])
    .await
    .unwrap();
    elapse(Duration::from_millis(150)).await;
    assert_eq!(
        observed(&a, "B").await.unwrap().status,
        MemberStatus::Suspect
    );
    elapse(Duration::from_millis(250)).await;
    assert_eq!(
        observed(&a, "B").await.unwrap(),
        Member {
            incarnation: 1,
            status: MemberStatus::Dead,
            ..member("B")
        }
    );
    ar.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn dead_member_withdrawal_preserves_newer_route_owners_and_rejects_stale_registration() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    a.merge_membership(vec![member("B")]).await.unwrap();
    let services = Receptionist::new();
    services.register("worker", "B", "/old").await.unwrap();
    services
        .register("worker", "C", "/replacement")
        .await
        .unwrap();
    let mut sub = services.subscribe("worker").await;
    sub.recv().await.unwrap();
    let router = Arc::new(PrefixRouter::new());
    let (link, _peer) = memory_pair();
    let callbacks = Arc::new(CallbackRegistry::new());
    router
        .add_member("B", 0, "/old", link.sender.clone(), callbacks.clone())
        .unwrap();
    let old = router.resolve("/old/x").unwrap();
    router
        .add_member("C", 0, "/old", link.sender.clone(), callbacks.clone())
        .unwrap();
    assert!(old.is_retired());
    router
        .add_member(
            "B",
            1,
            "/new-generation",
            link.sender.clone(),
            callbacks.clone(),
        )
        .unwrap();
    router.add("/static", link.sender.clone(), callbacks.clone());
    let ar = a
        .start_with(
            &bus,
            fast(),
            ClusterBindings {
                receptionist: Some(services.clone()),
                router: Some(router.clone()),
            },
        )
        .unwrap();
    elapse(Duration::from_secs(1)).await;
    assert_eq!(observed(&a, "B").await.unwrap().status, MemberStatus::Dead);
    assert_eq!(
        sub.try_recv().unwrap().entries,
        vec![("C".into(), "/replacement".into())]
    );
    assert!(router.resolve("/old/x").is_some());
    assert!(router.resolve("/new-generation/x").is_some());
    assert!(router.resolve("/static/x").is_some());
    assert_eq!(
        services.register("worker", "B", "/stale").await,
        Err(ClusterError::UnavailableMember)
    );
    assert_eq!(
        router.add_member("B", 0, "/stale", link.sender.clone(), callbacks.clone()),
        Err(ClusterError::UnavailableMember)
    );
    a.merge_membership(vec![Member {
        incarnation: 1,
        ..member("B")
    }])
    .await
    .unwrap();
    // Let the membership notification run without advancing another probe.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(services.register("worker", "B", "/fresh").await.unwrap());
    assert!(sub
        .try_recv()
        .unwrap()
        .entries
        .contains(&("B".into(), "/fresh".into())));
    ar.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_and_drop_cancel_periodic_work_and_release_probe_ownership() {
    for explicit in [true, false] {
        let bus = ClusterBus::new();
        let a = SwimNode::new("A");
        a.merge_membership(vec![member("B")]).await.unwrap();
        let _silent = bus.register("B").unwrap();
        let ar = a.start(&bus).unwrap();
        elapse(Duration::from_millis(550)).await;
        assert_eq!(a.pending_count(), 1);
        if explicit {
            ar.shutdown().await.unwrap();
        } else {
            drop(ar);
        }
        for _ in 0..30 {
            tokio::task::yield_now().await;
        }
        assert_eq!(bus.registered_count(), 1);
        assert_eq!(a.pending_count(), 0);
        let before = a.members().await;
        elapse(Duration::from_secs(10)).await;
        assert_eq!(
            a.members().await,
            before,
            "retired timer changed membership"
        );
        let restarted = a.start_manual(&bus).unwrap();
        elapse(Duration::from_secs(2)).await;
        assert_eq!(a.members().await, before, "old generation survived restart");
        restarted.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn invalid_scheduler_config_and_oversized_membership_fail_without_registration_or_partial_merge(
) {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    for invalid in [
        SwimConfig {
            probe_interval: Duration::ZERO,
            ..fast()
        },
        SwimConfig {
            probe_timeout: Duration::MAX,
            ..fast()
        },
        SwimConfig {
            suspect_timeout: Duration::ZERO,
            ..fast()
        },
        SwimConfig {
            gossip_interval: Duration::ZERO,
            ..fast()
        },
        SwimConfig {
            indirect_probes: usize::MAX,
            ..fast()
        },
        SwimConfig {
            gossip_fanout: 0,
            ..fast()
        },
    ] {
        assert!(matches!(
            a.start_with(&bus, invalid, ClusterBindings::default()),
            Err(ClusterError::InvalidConfig)
        ));
        assert_eq!(bus.registered_count(), 0);
    }
    let before = a.members().await;
    assert_eq!(
        a.merge_membership((0..1024).map(|n| member(&format!("n{n}"))).collect())
            .await,
        Err(ClusterError::MembershipCapacity)
    );
    assert_eq!(a.members().await, before);
    assert_eq!(
        a.merge_membership(vec![member("")]).await,
        Err(ClusterError::InvalidMember)
    );
    assert_eq!(a.members().await, before);
}

#[tokio::test(start_paused = true)]
async fn local_probe_capacity_is_not_remote_failure_evidence() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    a.merge_membership(vec![member("B")]).await.unwrap();
    let ar = a
        .start_with(&bus, fast(), ClusterBindings::default())
        .unwrap();
    let br = b.start_manual(&bus).unwrap();
    let _silent = bus.register("silent").unwrap();
    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..128 {
        let a = a.clone();
        let bus = bus.clone();
        calls.spawn(async move { a.probe(&bus, "silent", &[], Duration::from_secs(30)).await });
    }
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(a.pending_count(), 128);
    elapse(Duration::from_secs(2)).await;
    assert_eq!(observed(&a, "B").await, Some(member("B")));
    calls.abort_all();
    while let Some(result) = calls.join_next().await {
        assert!(result.unwrap_err().is_cancelled());
    }
    elapse(Duration::from_secs(1)).await;
    assert_eq!(observed(&a, "B").await, Some(member("B")));
    ar.shutdown().await.unwrap();
    br.shutdown().await.unwrap();
    assert_eq!(a.pending_count(), 0);
}

async fn bounded<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("contract deadline")
}

#[tokio::test]
async fn automatic_death_cancels_only_owned_relay_requests_and_blocks_future_forwarding() {
    let router = Arc::new(PrefixRouter::new());
    let (mut caller, hub_link) = memory_pair();
    let hub = spawn_endpoint_routed(hub_link, router.clone()).await;
    let (mut other, other_hub_link) = memory_pair();
    let other_hub = spawn_endpoint_routed(other_hub_link, router.clone()).await;
    let (outbound_link, mut peer) = memory_pair();
    let outbound = spawn_endpoint(outbound_link).await;
    router
        .add_member(
            "B",
            0,
            "/bound",
            outbound.sender(),
            outbound.callbacks.clone(),
        )
        .unwrap();
    router.add("/static", outbound.sender(), outbound.callbacks.clone());
    let retained = router.resolve("/bound/x").unwrap();
    caller
        .sender
        .send(Frame::ask(1, "/bound/x", "test:raw", vec![7]))
        .await
        .unwrap();
    let request = bounded(peer.incoming.recv()).await.unwrap();
    peer.sender
        .send(Frame::reply(request.correlation_id, "test:raw", vec![7]))
        .await
        .unwrap();
    assert_eq!(
        bounded(caller.incoming.recv())
            .await
            .unwrap()
            .payload
            .as_ref(),
        &[7]
    );
    caller
        .sender
        .send(Frame::ask(2, "/bound/x", "test:raw", vec![8]))
        .await
        .unwrap();
    let pending = bounded(peer.incoming.recv()).await.unwrap();
    other
        .sender
        .send(Frame::ask(1, "/static/x", "test:raw", vec![9]))
        .await
        .unwrap();
    let unrelated = bounded(peer.incoming.recv()).await.unwrap();
    assert_ne!(pending.correlation_id, unrelated.correlation_id);
    assert_eq!(outbound.callbacks.pending_count(), 2);
    let services = Receptionist::new();
    services.register("worker", "B", "/bound/x").await.unwrap();
    let mut sub = services.subscribe("worker").await;
    sub.recv().await.unwrap();
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    a.merge_membership(vec![member("B")]).await.unwrap();
    let _silent = bus.register("B").unwrap();
    let ar = a
        .start_with(
            &bus,
            SwimConfig::default(),
            ClusterBindings {
                receptionist: Some(services),
                router: Some(router.clone()),
            },
        )
        .unwrap();
    assert!(bounded(sub.recv()).await.unwrap().entries.is_empty());
    let failed = bounded(caller.incoming.recv()).await.unwrap();
    assert_eq!(failed.frame_type, frame_type::REPLY_ERR);
    assert_eq!(failed.correlation_id, 2);
    assert!(String::from_utf8_lossy(&failed.payload).contains("route retired"));
    assert!(retained.is_retired());
    assert!(router.resolve("/bound/x").is_none());
    assert_eq!(outbound.callbacks.pending_count(), 1);
    peer.sender
        .send(Frame::reply(unrelated.correlation_id, "test:raw", vec![9]))
        .await
        .unwrap();
    assert_eq!(
        bounded(other.incoming.recv())
            .await
            .unwrap()
            .payload
            .as_ref(),
        &[9]
    );
    assert_eq!(outbound.callbacks.pending_count(), 0);
    // A fresh ASK after the withdrawal is rejected locally, not forwarded.
    caller
        .sender
        .send(Frame::ask(3, "/bound/x", "test:raw", vec![10]))
        .await
        .unwrap();
    let failed = bounded(caller.incoming.recv()).await.unwrap();
    assert_eq!(failed.frame_type, frame_type::REPLY_ERR);
    assert_eq!(failed.correlation_id, 3);
    assert!(matches!(
        peer.incoming.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    ar.shutdown().await.unwrap();
    drop((hub, other_hub, outbound));
}
