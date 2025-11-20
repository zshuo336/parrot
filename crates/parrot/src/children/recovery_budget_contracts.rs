use super::*;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

fn setup() -> (Arc<Recovery>, [Uuid; 2], Arc<AtomicUsize>) {
    let members = [Uuid::new_v4(), Uuid::new_v4()];
    let released = Arc::new(AtomicUsize::new(0));
    let observed = released.clone();
    let group = Recovery::new(
        Uuid::new_v4(),
        members,
        Instant::now() + Duration::from_secs(3),
        Box::new(move |active| {
            observed.store(if active { 1 } else { 2 }, Ordering::SeqCst);
        }),
    );
    (group, members, released)
}
async fn phase(group: &Arc<Recovery>, members: [Uuid; 2], step: Step) {
    let (a, b) = tokio::join!(
        group.arrive(members[0], step),
        group.arrive(members[1], step)
    );
    a.unwrap();
    b.unwrap();
}
#[tokio::test]
async fn expired_member_vetoes_final_vote_without_timer_poll() {
    let (group, members, released) = setup();
    phase(&group, members, Step::Retired).await;
    let deadline = Instant::now() + Duration::from_millis(30);
    group.creation_control(members[0], deadline).unwrap();
    group
        .creation_control(members[1], deadline + Duration::from_secs(1))
        .unwrap();
    phase(&group, members, Step::Prepared).await;
    let first = group.arrive(members[0], Step::Published);
    tokio::pin!(first);
    assert!(futures::poll!(&mut first).is_pending());
    // Deliberately prevent this executor from polling its timer before the vote.
    std::thread::sleep(Duration::from_millis(50));
    assert!(group.arrive(members[1], Step::Published).await.is_err());
    assert!(first.await.is_err());
    assert!(!group.is_committed());
    assert_eq!(released.load(Ordering::SeqCst), 0);
    group.retired_after_abort(members[0]);
    assert_eq!(released.load(Ordering::SeqCst), 0);
    group.retired_after_abort(members[1]);
    assert_eq!(released.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn shorter_member_budget_wakes_an_existing_preparation_waiter() {
    let (group, members, _) = setup();
    phase(&group, members, Step::Retired).await;
    group
        .creation_control(members[0], Instant::now() + Duration::from_secs(2))
        .unwrap();
    let first = group.arrive(members[0], Step::Prepared);
    tokio::pin!(first);
    assert!(futures::poll!(&mut first).is_pending());
    group
        .creation_control(members[1], Instant::now() + Duration::from_millis(20))
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), first)
        .await
        .unwrap();
    assert!(result.unwrap_err().contains("deadline"));
    assert!(!group.is_committed());
}
#[tokio::test]
async fn cancellation_after_retirement_rejects_creation_with_its_cancellation_reason() {
    let (group, members, released) = setup();
    phase(&group, members, Step::Retired).await;
    group.cancel("explicit member stop before constructor admission");
    assert!(group.was_cancelled());
    assert!(group.control.is_cancelled());
    let result = group.creation_control(members[0], Instant::now() + Duration::from_secs(1));
    let Err(error) = result else {
        panic!("cancelled recovery admitted a new constructor");
    };
    assert_eq!(error, "explicit member stop before constructor admission");
    assert_eq!(released.load(Ordering::SeqCst), 0);
    group.retired_after_abort(members[0]);
    group.retired_after_abort(members[1]);
    assert_eq!(released.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn completed_commit_is_not_revoked_by_an_old_creation_budget() {
    let (group, members, released) = setup();
    phase(&group, members, Step::Retired).await;
    let deadline = Instant::now() + Duration::from_millis(30);
    group.creation_control(members[0], deadline).unwrap();
    group.creation_control(members[1], deadline).unwrap();
    phase(&group, members, Step::Prepared).await;
    phase(&group, members, Step::Published).await;
    assert_eq!(released.load(Ordering::SeqCst), 1);
    tokio::time::sleep_until(deadline).await;
    group.abort("old creation clock elapsed");
    group.activation().await.unwrap();
    assert!(group.is_committed());
    assert_eq!(released.load(Ordering::SeqCst), 1);
}
