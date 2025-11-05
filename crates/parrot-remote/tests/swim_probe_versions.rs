//! A failed observation belongs to the member version that was probed.
use parrot_remote::swim::{
    Member, MemberEvent, MemberStatus, Membership, NodeAddrWire, Swim, SwimConfig,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

fn member(incarnation: u64, status: MemberStatus) -> Member {
    Member {
        node_id: "peer".into(),
        addr: NodeAddrWire {
            node_id: "peer".into(),
            scheme: "tcp".into(),
            host: "127.0.0.1".into(),
            port: 8000 + incarnation as u16,
        },
        status,
        incarnation,
        status_until_ms: 0,
        metadata: bytes::Bytes::from(format!("generation-{incarnation}")),
    }
}

struct Finished(Option<oneshot::Sender<()>>);
impl Drop for Finished {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

// The single-thread paused runtime makes timeouts deterministic. The future's
// actual retirement is a barrier; no caller holds the membership lock while
// the run loop applies its result. Join the driver before inspecting final state.
async fn observe(update: Option<MemberEvent>, outcome: Option<bool>) -> Option<Member> {
    let membership = Arc::new(Mutex::new(Membership::new(1000)));
    membership
        .lock()
        .await
        .merge_event(&MemberEvent::Upsert(member(1, MemberStatus::Alive)));
    let swim = Arc::new(Swim {
        config: SwimConfig {
            probe_interval: Duration::from_secs(100),
            probe_timeout: Duration::from_secs(1),
            suspect_timeout: Duration::from_secs(10),
            gossip_interval: Duration::from_millis(50),
            ..SwimConfig::default()
        },
        membership: membership.clone(),
        self_node: "self".into(),
    });
    let (started, start) = oneshot::channel();
    let (finished, finish) = oneshot::channel();
    let (complete, result) = oneshot::channel();
    let mut channels = Some((started, finished, result));
    let (gossip, _receipts) = mpsc::channel(1);
    let (stop, stopped) = watch::channel(false);
    let driver = tokio::spawn(swim.run(
        Box::new(move |target| {
            assert_eq!(target, "peer");
            let (started, finished, result) =
                channels.take().expect("only one probe before shutdown");
            Box::pin(async move {
                let _retired = Finished(Some(finished));
                started.send(()).unwrap();
                result
                    .await
                    .expect("test retains the response sender until completion")
            })
        }),
        gossip,
        stopped,
    ));
    start.await.unwrap();
    if let Some(update) = update {
        membership.lock().await.merge_event(&update);
    }
    // Keep the sender alive for timeout cases, so EOF cannot imitate a timeout.
    let mut complete = Some(complete);
    if let Some(value) = outcome {
        complete.take().unwrap().send(value).unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), finish)
        .await
        .unwrap()
        .unwrap();
    stop.send(true).unwrap();
    driver.await.unwrap();
    drop(complete);
    let result = membership.lock().await.members.get("peer").cloned();
    result
}

#[tokio::test(start_paused = true)]
async fn stale_failure_cannot_suspect_a_new_incarnation_and_endpoint() {
    let fresh = member(2, MemberStatus::Alive);
    assert_eq!(
        observe(Some(MemberEvent::Upsert(fresh.clone())), Some(false)).await,
        Some(fresh)
    );
}

#[tokio::test(start_paused = true)]
async fn stale_timeout_cannot_suspect_a_new_incarnation_and_endpoint() {
    let fresh = member(2, MemberStatus::Alive);
    assert_eq!(
        observe(Some(MemberEvent::Upsert(fresh.clone())), None).await,
        Some(fresh)
    );
}

#[tokio::test(start_paused = true)]
async fn current_failure_still_starts_the_configured_suspicion_deadline() {
    let failed = observe(None, Some(false)).await.unwrap();
    assert_eq!(failed.incarnation, 1);
    assert_eq!(failed.status, MemberStatus::Suspect);
    assert_eq!(failed.status_until_ms, 11000);
}

#[tokio::test(start_paused = true)]
async fn current_timeout_still_starts_the_configured_suspicion_deadline() {
    let failed = observe(None, None).await.unwrap();
    assert_eq!(failed.incarnation, 1);
    assert_eq!(failed.status, MemberStatus::Suspect);
    assert_eq!(failed.status_until_ms, 12000);
}

#[tokio::test(start_paused = true)]
async fn duplicate_gossip_does_not_mask_a_current_failed_probe() {
    let failed = observe(
        Some(MemberEvent::Upsert(member(1, MemberStatus::Alive))),
        Some(false),
    )
    .await
    .unwrap();
    assert_eq!(failed.status, MemberStatus::Suspect);
    assert_eq!(failed.incarnation, 1);
}

#[tokio::test(start_paused = true)]
async fn completion_does_not_restore_a_removed_member() {
    assert!(
        observe(Some(MemberEvent::Remove("peer".into())), Some(false))
            .await
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn successful_old_probe_does_not_overwrite_a_new_failure_state() {
    let failed = observe(
        Some(MemberEvent::Upsert(member(2, MemberStatus::Dead))),
        Some(true),
    )
    .await
    .unwrap();
    assert_eq!(failed.incarnation, 2);
    assert_eq!(failed.status, MemberStatus::Dead);
}
