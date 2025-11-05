use remote_poc::*;
use std::time::Duration;

fn member(node: &str, incarnation: u64, status: MemberStatus) -> Member {
    Member {
        node_id: node.into(),
        addr: node.into(),
        incarnation,
        status,
    }
}

#[tokio::test]
async fn registered_but_unresponsive_target_is_not_alive() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let runtime = a.start_manual(&bus).unwrap();
    let _silent = bus.register("silent").unwrap();
    let result = a
        .probe(&bus, "silent", &[], Duration::from_millis(5))
        .await
        .unwrap();
    assert!(!result.reachable, "a queue registration is not an ACK");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn self_refutation_never_decreases_observed_incarnation() {
    let a = SwimNode::new("A");
    a.merge_membership(vec![member("A", 100, MemberStatus::Suspect)])
        .await
        .unwrap();
    a.refute().await.unwrap();
    let me = a
        .members()
        .await
        .into_iter()
        .find(|m| m.node_id == "A")
        .unwrap();
    assert!(me.incarnation > 100);
    assert_eq!(me.status, MemberStatus::Alive);
}

#[tokio::test]
async fn suspect_does_not_resurrect_a_dead_member() {
    let a = SwimNode::new("A");
    a.merge_membership(vec![member("B", 7, MemberStatus::Dead)])
        .await
        .unwrap();
    a.mark_suspect("B", 7).await;
    let b = a
        .members()
        .await
        .into_iter()
        .find(|m| m.node_id == "B")
        .unwrap();
    assert_eq!((b.incarnation, b.status), (7, MemberStatus::Dead));
}

#[tokio::test]
async fn late_subscriber_gets_current_service_snapshot() {
    let r = Receptionist::new();
    r.register("worker", "B", "/user/b").await.unwrap();
    let mut sub = r.subscribe("worker").await;
    let snapshot = tokio::time::timeout(Duration::from_millis(50), sub.recv())
        .await
        .expect("late subscriber needs a snapshot")
        .unwrap();
    assert_eq!(snapshot.entries, vec![("B".into(), "/user/b".into())]);
}

#[tokio::test]
async fn duplicate_service_registration_is_idempotent() {
    let r = Receptionist::new();
    let mut sub = r.subscribe("worker").await;
    assert!(r.register("worker", "B", "/user/b").await.unwrap());
    assert!(!r.register("worker", "B", "/user/b").await.unwrap());
    let mut snapshot = sub.recv().await.unwrap();
    while let Ok(value) = sub.try_recv() {
        snapshot = value;
    }
    assert_eq!(snapshot.entries, vec![("B".into(), "/user/b".into())]);
    assert_eq!(snapshot.revision, 1);
    assert!(sub.try_recv().is_err());
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("contract deadline")
}
fn ping_sequence(envelope: SwimEnvelope, from: &str) -> u64 {
    assert_eq!(envelope.from, from);
    match envelope.message {
        SwimMsg::Ping { seq } => seq,
        other => panic!("expected Ping, got {other:?}"),
    }
}
async fn barrier(endpoint: &mut BusEndpoint, to: &str) {
    assert!(endpoint.send(to, SwimMsg::Ping { seq: u64::MAX }));
    let response = bounded(endpoint.recv()).await.unwrap();
    assert_eq!(response.from, to);
    match response.message {
        SwimMsg::Ack { target, seq } => {
            assert_eq!(target, to);
            assert_eq!(seq, u64::MAX);
        }
        other => panic!("invalid barrier response: {other:?}"),
    }
}

#[tokio::test]
async fn indirect_probe_succeeds_only_via_a_live_helper_and_target() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    let c = SwimNode::new("C");
    let ar = a.start_manual(&bus).unwrap();
    let br = b.start_manual(&bus).unwrap();
    let cr = c.start_manual(&bus).unwrap();
    assert!(
        a.probe(&bus, "C", &[], Duration::from_millis(20))
            .await
            .unwrap()
            .reachable
    );
    bus.block_link("A", "C");
    bus.block_link("C", "A");
    let direct = a
        .probe(&bus, "C", &[], Duration::from_millis(20))
        .await
        .unwrap();
    assert_eq!((direct.reachable, direct.used_indirect), (false, false));
    let indirect = a
        .probe(&bus, "C", &["B".into()], Duration::from_millis(20))
        .await
        .unwrap();
    assert_eq!((indirect.reachable, indirect.used_indirect), (true, true));
    bus.block_link("C", "B");
    let lost = a
        .probe(&bus, "C", &["B".into()], Duration::from_millis(20))
        .await
        .unwrap();
    assert_eq!((lost.reachable, lost.used_indirect), (false, true));
    assert_eq!(a.pending_count(), 0);
    ar.shutdown().await.unwrap();
    br.shutdown().await.unwrap();
    cr.shutdown().await.unwrap();
    assert_eq!(bus.registered_count(), 0);
}

#[tokio::test]
async fn helper_does_not_invent_success_for_registered_but_silent_target() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    let ar = a.start_manual(&bus).unwrap();
    let br = b.start_manual(&bus).unwrap();
    let mut silent = bus.register("C").unwrap();
    bus.block_link("A", "C");
    let task = tokio::spawn({
        let a = a.clone();
        let bus = bus.clone();
        async move {
            a.probe(&bus, "C", &["B".into()], Duration::from_millis(200))
                .await
                .unwrap()
        }
    });
    let _helper_seq = ping_sequence(bounded(silent.recv()).await.unwrap(), "B");
    let result = bounded(task).await.unwrap();
    assert!(!result.reachable);
    assert!(result.used_indirect);
    ar.shutdown().await.unwrap();
    br.shutdown().await.unwrap();
    drop(silent);
    assert_eq!(b.pending_count(), 0);
    assert_eq!(bus.registered_count(), 0);
}

#[tokio::test]
async fn ack_requires_matching_sequence_target_and_source() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let runtime = a.start_manual(&bus).unwrap();
    let mut target = bus.register("T").unwrap();
    let mut attacker = bus.register("X").unwrap();
    let call = tokio::spawn({
        let a = a.clone();
        let bus = bus.clone();
        async move { a.probe(&bus, "T", &[], Duration::from_secs(2)).await }
    });
    let seq = ping_sequence(bounded(target.recv()).await.unwrap(), "A");
    assert!(attacker.send(
        "A",
        SwimMsg::Ack {
            target: "T".into(),
            seq
        }
    ));
    assert!(target.send(
        "A",
        SwimMsg::Ack {
            target: "wrong".into(),
            seq
        }
    ));
    assert!(target.send(
        "A",
        SwimMsg::Ack {
            target: "T".into(),
            seq: seq + 1
        }
    ));
    barrier(&mut attacker, "A").await;
    assert!(!call.is_finished());
    assert_eq!(a.pending_count(), 1);
    assert!(target.send(
        "A",
        SwimMsg::Ack {
            target: "T".into(),
            seq
        }
    ));
    assert!(bounded(call).await.unwrap().unwrap().reachable);
    assert_eq!(a.pending_count(), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_late_ack_and_runtime_shutdown_cleanup_pending() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let runtime = a.start_manual(&bus).unwrap();
    let mut target = bus.register("T").unwrap();
    let first = tokio::spawn({
        let a = a.clone();
        let bus = bus.clone();
        async move { a.probe(&bus, "T", &[], Duration::from_secs(30)).await }
    });
    let old = ping_sequence(bounded(target.recv()).await.unwrap(), "A");
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(a.pending_count(), 0);
    let second = tokio::spawn({
        let a = a.clone();
        let bus = bus.clone();
        async move { a.probe(&bus, "T", &[], Duration::from_secs(30)).await }
    });
    let new = ping_sequence(bounded(target.recv()).await.unwrap(), "A");
    assert_ne!(new, old);
    assert!(target.send(
        "A",
        SwimMsg::Ack {
            target: "T".into(),
            seq: old
        }
    ));
    barrier(&mut target, "A").await;
    assert!(!second.is_finished());
    assert_eq!(a.pending_count(), 1);
    runtime.shutdown().await.unwrap();
    assert_eq!(
        bounded(second).await.unwrap(),
        Err(ClusterError::NotRunning)
    );
    assert_eq!(a.pending_count(), 0);
    let restarted = a.start_manual(&bus).unwrap();
    assert!(target.send(
        "A",
        SwimMsg::Ack {
            target: "T".into(),
            seq: new
        }
    ));
    barrier(&mut target, "A").await;
    assert_eq!(a.pending_count(), 0);
    restarted.shutdown().await.unwrap();
    drop(target);
    assert_eq!(bus.registered_count(), 0);
}

#[tokio::test]
async fn probe_capacity_is_bounded_and_cancelled_requests_release_it() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let runtime = a.start_manual(&bus).unwrap();
    let _silent = bus.register("T").unwrap();
    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..128 {
        let a = a.clone();
        let bus = bus.clone();
        calls.spawn(async move { a.probe(&bus, "T", &[], Duration::from_secs(30)).await });
    }
    bounded(async {
        while a.pending_count() != 128 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(
        a.probe(&bus, "T", &[], Duration::from_secs(30)).await,
        Err(ClusterError::ProbeCapacity)
    );
    calls.abort_all();
    while let Some(result) = calls.join_next().await {
        assert!(result.unwrap_err().is_cancelled());
    }
    assert_eq!(a.pending_count(), 0);
    assert!(
        !a.probe(&bus, "T", &[], Duration::from_millis(1))
            .await
            .unwrap()
            .reachable
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn bounded_bus_rejects_duplicate_identity_and_releases_queue() {
    let bus = ClusterBus::new();
    let sender = bus.register("A").unwrap();
    let mut receiver = bus.register("B").unwrap();
    assert!(matches!(
        bus.register("B"),
        Err(ClusterError::AlreadyRegistered)
    ));
    let mut admitted = 0;
    while sender.send("B", SwimMsg::Ping { seq: admitted }) {
        admitted += 1;
        assert!(admitted <= 1024);
    }
    assert!(admitted > 0);
    for seq in 0..admitted {
        assert_eq!(ping_sequence(receiver.recv().await.unwrap(), "A"), seq);
    }
    assert!(sender.send("B", SwimMsg::Ping { seq: 1000 }));
    drop(receiver);
    assert!(!sender.send("B", SwimMsg::Ping { seq: 1001 }));
    let mut replacement = bus.register("B").unwrap();
    assert!(sender.send("B", SwimMsg::Ping { seq: 1002 }));
    assert_eq!(ping_sequence(replacement.recv().await.unwrap(), "A"), 1002);
    drop(sender);
    drop(replacement);
    assert_eq!(bus.registered_count(), 0);
}

#[tokio::test]
async fn stale_failure_observations_cannot_kill_new_incarnation() {
    let a = SwimNode::new("A");
    a.merge_membership(vec![member("B", 4, MemberStatus::Alive)])
        .await
        .unwrap();
    assert!(a.mark_suspect("B", 4).await);
    a.merge_membership(vec![member("B", 5, MemberStatus::Alive)])
        .await
        .unwrap();
    assert!(!a.mark_dead("B", 4).await);
    assert!(!a.mark_suspect("B", 4).await);
    assert!(
        !a.mark_dead("B", 5).await,
        "death requires current suspicion"
    );
    assert!(!a.mark_suspect("A", 0).await);
    assert_eq!(
        a.members()
            .await
            .into_iter()
            .find(|m| m.node_id == "B")
            .unwrap(),
        member("B", 5, MemberStatus::Alive)
    );
}

#[tokio::test]
async fn member_merge_order_and_incarnation_exhaustion_preserve_state() {
    for incoming in [
        vec![
            member("B", 2, MemberStatus::Suspect),
            member("B", 2, MemberStatus::Dead),
        ],
        vec![
            member("B", 2, MemberStatus::Dead),
            member("B", 2, MemberStatus::Suspect),
        ],
    ] {
        let a = SwimNode::new("A");
        a.merge_membership(incoming).await.unwrap();
        a.merge_membership(vec![
            member("B", 2, MemberStatus::Alive),
            member("B", 1, MemberStatus::Alive),
        ])
        .await
        .unwrap();
        assert_eq!(
            a.members()
                .await
                .into_iter()
                .find(|m| m.node_id == "B")
                .unwrap(),
            member("B", 2, MemberStatus::Dead)
        );
    }
    let a = SwimNode::new("A");
    let before = a.members().await;
    assert_eq!(
        a.merge_membership(vec![
            member("B", 10, MemberStatus::Alive),
            member("A", u64::MAX, MemberStatus::Suspect)
        ])
        .await,
        Err(ClusterError::IncarnationExhausted)
    );
    assert_eq!(a.members().await, before, "failed batch is atomic");
    a.merge_membership(vec![Member {
        addr: "foreign".into(),
        ..member("A", u64::MAX, MemberStatus::Alive)
    }])
    .await
    .unwrap();
    let before = a.members().await;
    assert_eq!(a.refute().await, Err(ClusterError::IncarnationExhausted));
    assert_eq!(a.members().await, before);
    assert_eq!(
        before[0].addr, "A",
        "gossip cannot replace our local address"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_registration_and_subscription_never_lose_current_snapshot() {
    let r = Receptionist::new();
    let mut writers = tokio::task::JoinSet::new();
    for i in 0..64 {
        let r = r.clone();
        writers.spawn(async move {
            r.register("workers", &format!("node-{i:02}"), "/worker")
                .await
                .unwrap();
            let mut sub = r.subscribe("workers").await;
            let first = sub.recv().await.unwrap();
            (sub, first)
        });
    }
    let mut subscribers = vec![];
    while let Some(value) = writers.join_next().await {
        subscribers.push(value.unwrap());
    }
    for (mut sub, mut snapshot) in subscribers {
        while let Ok(new) = sub.try_recv() {
            assert!(new.revision > snapshot.revision);
            snapshot = new;
        }
        assert_eq!(snapshot.entries.len(), 64);
        assert_eq!(snapshot.revision, 64);
        assert_eq!(snapshot.entries.first().unwrap().0, "node-00");
        assert_eq!(snapshot.entries.last().unwrap().0, "node-63");
    }
    assert_eq!(r.subscriber_count("workers"), 0);
}

#[tokio::test]
async fn slow_subscriber_coalesces_to_latest_and_node_removal_is_atomic() {
    let r = Receptionist::new();
    let mut sub = r.subscribe("workers").await;
    assert_eq!(sub.recv().await.unwrap().revision, 0);
    for i in 0..1000 {
        r.register("workers", "B", &format!("/{i}")).await.unwrap();
    }
    let latest = sub.recv().await.unwrap();
    assert_eq!(latest.revision, 1000);
    assert_eq!(latest.entries.len(), 1000);
    assert!(sub.try_recv().is_err(), "no stale event backlog");
    let mut other = r.subscribe("other").await;
    other.recv().await.unwrap();
    r.register("other", "B", "/b").await.unwrap();
    r.register("workers", "C", "/c").await.unwrap();
    assert_eq!(r.remove_node("B").await.unwrap(), 1001);
    let a = sub.recv().await.unwrap();
    let b = other.recv().await.unwrap();
    assert_eq!(a.entries, vec![("C".into(), "/c".into())]);
    assert!(b.entries.is_empty());
    assert_eq!(a.revision, b.revision);
    assert!(!r.unregister("workers", "B", "/missing").await.unwrap());
    assert!(sub.try_recv().is_err());
    assert!(r.unregister("workers", "C", "/c").await.unwrap());
    assert!(sub.recv().await.unwrap().entries.is_empty());
    drop(sub);
    drop(other);
    assert_eq!(r.retained_key_count(), 0);
    for _ in 0..1000 {
        drop(r.subscribe("ephemeral").await);
    }
    assert_eq!(r.retained_key_count(), 0);
}

#[tokio::test]
async fn subscription_delivers_final_unread_snapshot_after_publisher_drop() {
    let r = Receptionist::new();
    let mut sub = r.subscribe("key").await;
    assert!(sub.recv().await.unwrap().entries.is_empty());
    r.register("key", "A", "/a").await.unwrap();
    drop(r);
    assert_eq!(
        sub.try_recv().expect("final unseen snapshot").entries,
        vec![("A".into(), "/a".into())]
    );
    assert!(matches!(
        sub.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ));
    assert!(sub.recv().await.is_none());
}

#[tokio::test]
async fn cancelling_subscription_wait_does_not_consume_the_next_update() {
    let r = Receptionist::new();
    let mut sub = r.subscribe("key").await;
    sub.recv().await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(1), sub.recv())
        .await
        .is_err());
    r.register("key", "A", "/a").await.unwrap();
    assert_eq!(
        sub.recv().await.unwrap().entries,
        vec![("A".into(), "/a".into())]
    );
    assert_eq!(r.subscriber_count("key"), 1);
    drop(sub);
    assert_eq!(r.subscriber_count("key"), 0);
}

#[tokio::test]
async fn dropping_runtime_stops_ingress_and_helper_requests() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let runtime = a.start_manual(&bus).unwrap();
    let mut peer = bus.register("P").unwrap();
    let mut silent = bus.register("T").unwrap();
    assert!(peer.send(
        "A",
        SwimMsg::PingReq {
            target: "T".into(),
            seq: 1,
            timeout: Duration::from_secs(1),
        }
    ));
    ping_sequence(bounded(silent.recv()).await.unwrap(), "A");
    assert_eq!(a.pending_count(), 1);
    drop(runtime);
    bounded(async {
        while bus.registered_count() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(a.pending_count(), 0);
    assert_eq!(
        a.probe(&bus, "P", &[], Duration::from_millis(1)).await,
        Err(ClusterError::NotRunning)
    );
    assert!(!peer.send("A", SwimMsg::Ping { seq: 2 }));
    let restarted = a.start_manual(&bus).unwrap();
    barrier(&mut peer, "A").await;
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn helper_uses_requesters_probe_budget_instead_of_hidden_shorter_timeout() {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    let ar = a.start_manual(&bus).unwrap();
    let br = b.start_manual(&bus).unwrap();
    let mut target = bus.register("T").unwrap();
    bus.block_link("A", "T");
    let request = tokio::spawn({
        let a = a.clone();
        let bus = bus.clone();
        async move {
            a.probe(&bus, "T", &["B".into()], Duration::from_secs(1))
                .await
                .unwrap()
        }
    });
    let seq = ping_sequence(bounded(target.recv()).await.unwrap(), "B");
    // A deliberate response delay tests the advertised protocol budget. The
    // old helper silently imposed 100 ms despite the caller granting 1 second.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(target.send(
        "B",
        SwimMsg::Ack {
            target: "T".into(),
            seq
        }
    ));
    let result = bounded(request).await.unwrap();
    assert!(result.reachable);
    assert!(result.used_indirect);
    ar.shutdown().await.unwrap();
    br.shutdown().await.unwrap();
}
