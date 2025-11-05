//! Runtime SWIM wiring: a real in-process connection must seed membership and
//! start the infrastructure task that emits bounded gossip without blocking
//! shutdown.

use parrot_api::types::BoxedActorRef;
use parrot_remote::{LocalLookup, RemoteActorSystem, RemoteConfig};
use std::sync::Arc;
use std::time::Duration;

struct Empty;

#[async_trait::async_trait]
impl LocalLookup for Empty {
    async fn lookup(&self, _: &str) -> Option<BoxedActorRef> {
        None
    }
}

#[tokio::test]
async fn connected_nodes_seed_swim_membership_and_shutdown_cleanly() {
    let a = RemoteActorSystem::new(RemoteConfig::mem("swim-runtime-a"), Arc::new(Empty)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("swim-runtime-b"), Arc::new(Empty)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let a_knows_b = a
                .membership
                .lock()
                .await
                .members
                .contains_key("swim-runtime-b");
            let b_knows_a = b
                .membership
                .lock()
                .await
                .members
                .contains_key("swim-runtime-a");
            if a_knows_b && b_knows_a {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real connection must seed both SWIM membership tables");

    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

async fn wait_for_member(system: &RemoteActorSystem, node: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if system.membership.lock().await.members.contains_key(node) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("membership update must not be lost");
}

#[tokio::test]
async fn connection_registration_survives_membership_lock_contention() {
    let a = RemoteActorSystem::new(RemoteConfig::mem("swim-lock-a"), Arc::new(Empty)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("swim-lock-b"), Arc::new(Empty)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    let guard_a = a.membership.lock().await;
    let guard_b = b.membership.lock().await;
    a.connect_mem_pair(&b).await.unwrap();
    drop(guard_a);
    drop(guard_b);
    wait_for_member(&a, "swim-lock-b").await;
    wait_for_member(&b, "swim-lock-a").await;
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn periodic_gossip_converges_across_three_real_connections() {
    let a = RemoteActorSystem::new(RemoteConfig::mem("swim-chain-a"), Arc::new(Empty)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("swim-chain-b"), Arc::new(Empty)).unwrap();
    let c = RemoteActorSystem::new(RemoteConfig::mem("swim-chain-c"), Arc::new(Empty)).unwrap();
    for system in [&a, &b, &c] {
        system.start().await.unwrap();
    }
    a.connect_mem_pair(&b).await.unwrap();
    b.connect_mem_pair(&c).await.unwrap();
    wait_for_member(&a, "swim-chain-c").await;
    wait_for_member(&c, "swim-chain-a").await;
    // No direct A-C connection exists: gossip receipt is required to learn it.
    assert_eq!(a.links_snapshot().await.len(), 1);
    assert_eq!(c.links_snapshot().await.len(), 1);
    tokio::time::sleep(Duration::from_millis(700)).await;
    for system in [&a, &b, &c] {
        let membership = system.membership.lock().await;
        assert_eq!(membership.members.len(), 3);
        assert!(membership
            .members
            .values()
            .all(|member| member.status == parrot_remote::swim::MemberStatus::Alive));
    }
    for system in [&a, &b, &c] {
        system.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn ordinary_tcp_connections_are_not_closed_by_unauthorized_gossip() {
    let a =
        RemoteActorSystem::new(RemoteConfig::tcp("swim-local-a", None), Arc::new(Empty)).unwrap();
    let b = RemoteActorSystem::new(
        RemoteConfig::tcp("swim-local-b", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Empty),
    )
    .unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect(&parrot_remote::NodeAddr::tcp(
        "swim-local-b",
        b.local_addr().unwrap(),
    ))
    .await
    .unwrap();
    wait_for_member(&a, "swim-local-b").await;
    wait_for_member(&b, "swim-local-a").await;
    tokio::time::sleep(Duration::from_millis(650)).await;
    for system in [&a, &b] {
        let links = system.links_snapshot().await;
        assert_eq!(
            links.len(),
            1,
            "local TCP permits business but not cluster control"
        );
        assert!(!links[0].1.is_closed());
        // Receiving the peer's gossip would also populate our own node entry.
        assert_eq!(system.membership.lock().await.members.len(), 1);
    }
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn connected_peer_refutes_retained_death_using_its_own_incarnation() {
    use parrot_remote::swim::MemberStatus;
    let a = RemoteActorSystem::new(RemoteConfig::mem("swim-refute-a"), Arc::new(Empty)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("swim-refute-b"), Arc::new(Empty)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();
    wait_for_member(&a, "swim-refute-b").await;
    wait_for_member(&b, "swim-refute-a").await;
    {
        let mut membership = a.membership.lock().await;
        let peer = membership.members.get_mut("swim-refute-b").unwrap();
        peer.incarnation = 7;
        peer.status = MemberStatus::Dead;
        peer.status_until_ms = u64::MAX;
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let membership = a.membership.lock().await;
            let peer = &membership.members["swim-refute-b"];
            if peer.status == MemberStatus::Alive && peer.incarnation == 8 {
                break;
            }
            drop(membership);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retained dead peers need reconciliation so the owner can refute");
    assert_eq!(
        b.membership.lock().await.members["swim-refute-b"].incarnation,
        8
    );
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn disconnection_progresses_from_suspect_to_dead() {
    use parrot_remote::swim::MemberStatus;
    let a = RemoteActorSystem::new(RemoteConfig::mem("swim-expiry-a"), Arc::new(Empty)).unwrap();
    let b = RemoteActorSystem::new(RemoteConfig::mem("swim-expiry-b"), Arc::new(Empty)).unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect_mem_pair(&b).await.unwrap();
    wait_for_member(&a, "swim-expiry-b").await;
    b.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if a.membership.lock().await.members["swim-expiry-b"].status == MemberStatus::Dead {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the real runtime must advance suspicion deadlines after disconnect");
    assert!(a.links_snapshot().await.is_empty());
    a.shutdown().await.unwrap();
}

#[tokio::test]
async fn gossip_capability_respects_the_receivers_local_tcp_policy() {
    let a = RemoteActorSystem::new(
        RemoteConfig::tcp("swim-mixed-a", None).with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap();
    let b = RemoteActorSystem::new(
        RemoteConfig::tcp("swim-mixed-b", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Empty),
    )
    .unwrap();
    a.start().await.unwrap();
    b.start().await.unwrap();
    a.connect(&parrot_remote::NodeAddr::tcp(
        "swim-mixed-b",
        b.local_addr().unwrap(),
    ))
    .await
    .unwrap();
    let links = a.links_snapshot().await;
    assert_eq!(
        links[0].1.peer_capabilities() & parrot_remote::handshake::caps::MEMBERSHIP_GOSSIP,
        0,
        "a local-only TCP receiver must not advertise cluster participation"
    );
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(!links[0].1.is_closed());
    assert_eq!(b.links_snapshot().await.len(), 1);
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}
