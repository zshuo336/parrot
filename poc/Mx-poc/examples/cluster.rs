//! Run: cargo run --manifest-path poc/Mx-poc/Cargo.toml --example cluster
use remote_poc::{
    ClusterBindings, ClusterBus, ClusterError, MemberStatus, Receptionist, SwimConfig, SwimNode,
};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bus = ClusterBus::new();
    let a = SwimNode::new("A");
    let b = SwimNode::new("B");
    let c = SwimNode::new("C");
    // Bootstrap only A. Periodic gossip must teach B and C the remaining peers.
    a.merge_membership(b.members().await).await?;
    a.merge_membership(c.members().await).await?;
    let discovery = Receptionist::new();
    discovery.register("worker", "C", "/user/worker").await?;
    let mut services = discovery.subscribe("worker").await;
    let initial = services.recv().await.unwrap();
    let ar = a.start_with(
        &bus,
        SwimConfig::default(),
        ClusterBindings {
            receptionist: Some(discovery.clone()),
            router: None,
        },
    )?;
    let br = b.start(&bus)?;
    let cr = c.start(&bus)?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if a.members().await.len() == 3
                && a.members().await == b.members().await
                && a.members().await == c.members().await
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    println!("periodic gossip: all three membership views converged");

    bus.block_link("A", "C");
    bus.block_link("C", "A");
    let indirect = a
        .probe(&bus, "C", &["B".into()], Duration::from_millis(500))
        .await?;
    assert!(indirect.reachable && indirect.used_indirect);
    println!("A -> B -> C correlated probe: reachable despite direct-link failure");
    bus.heal_link("A", "C");
    bus.heal_link("C", "A");

    bus.partition("C");
    // No explicit probe, mark_suspect, mark_dead or remove_node call: the
    // owned scheduler and its discovery binding must complete the transition.
    let withdrawn = tokio::time::timeout(Duration::from_secs(10), services.recv())
        .await?
        .unwrap();
    assert!(withdrawn.entries.is_empty() && withdrawn.revision > initial.revision);
    assert!(a
        .members()
        .await
        .iter()
        .any(|m| m.node_id == "C" && m.status == MemberStatus::Dead));
    println!("partitioned C: automatic suspicion expiry withdrew its service");

    bus.heal("C");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if a.members()
                .await
                .iter()
                .any(|m| m.node_id == "C" && m.status == MemberStatus::Alive && m.incarnation > 0)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    // Recovery never republishes stale service announcements.
    assert!(services.try_recv().is_err());
    // Membership and its observer bindings converge asynchronously. Seeing
    // Alive is not a barrier proving that discovery has lifted its admission ban.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match discovery.register("worker", "C", "/user/worker").await {
                Ok(_) => return Ok::<(), ClusterError>(()),
                Err(ClusterError::UnavailableMember) => {
                    if !ar.status()? {
                        return Err(ClusterError::NotRunning);
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await??;
    assert_eq!(services.recv().await.unwrap().entries, initial.entries);
    println!("healed C: automatic refutation converged; fresh service registration accepted");

    ar.shutdown().await?;
    br.shutdown().await?;
    cr.shutdown().await?;
    assert_eq!(bus.registered_count(), 0);
    println!("all periodic tasks, helpers and registrations retired");
    Ok(())
}
