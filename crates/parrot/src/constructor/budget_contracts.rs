use super::*;
use std::{sync::atomic::AtomicUsize, time::Duration};

#[test]
fn bounded_deadline_cannot_extend_parent_or_ancestor() {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let parent = Arc::new(ConstructionControl::with_deadline(deadline));
    let child = parent.bounded(deadline + Duration::from_secs(1));
    let grandchild = child.bounded(deadline + Duration::from_secs(2));
    assert_eq!(child.deadline(), Some(deadline));
    assert_eq!(grandchild.deadline(), Some(deadline));
}
#[test]
fn cancelling_child_does_not_cancel_parent_or_sibling() {
    let parent = Arc::new(ConstructionControl::default());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let child = parent.bounded(deadline);
    let sibling = parent.bounded(deadline);
    child.cancel();
    assert!(child.is_cancelled());
    assert!(!parent.is_cancelled());
    assert!(!sibling.is_cancelled());
}
#[tokio::test]
async fn parent_cancel_wakes_registered_descendants() {
    let parent = Arc::new(ConstructionControl::default());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let child = parent.bounded(deadline);
    let grandchild = child.bounded(deadline);
    let waiting = grandchild.cancelled();
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    parent.cancel();
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap();
    assert!(child.is_cancelled());
    assert!(grandchild.is_cancelled());
}
#[tokio::test]
async fn cancellation_before_first_poll_is_not_lost() {
    let parent = Arc::new(ConstructionControl::default());
    let child = parent.bounded(tokio::time::Instant::now() + Duration::from_secs(30));
    parent.cancel();
    tokio::time::timeout(Duration::from_secs(1), child.cancelled())
        .await
        .unwrap();
}
#[tokio::test]
async fn child_clock_expires_without_cancelling_parent() {
    let parent = Arc::new(ConstructionControl::default());
    let child = parent.bounded(tokio::time::Instant::now() + Duration::from_millis(20));
    tokio::time::timeout(Duration::from_secs(1), child.cancelled())
        .await
        .unwrap();
    assert!(child.is_cancelled());
    assert!(!parent.is_cancelled());
}
#[tokio::test]
async fn expired_child_does_not_invoke_actual_factory() {
    let builds = Arc::new(AtomicUsize::new(0));
    let observed = builds.clone();
    let factory = Factory::new(
        move || {
            observed.fetch_add(1, Ordering::SeqCst);
        },
        Arc::new(Semaphore::new(1)),
        Arc::new(Semaphore::new(1)),
    );
    let parent = Arc::new(ConstructionControl::default());
    let child = parent.bounded(tokio::time::Instant::now());
    let result = factory.construct(child, |_| Ok::<_, ()>(())).await;
    assert!(matches!(result, Err(ConstructionError::Cancelled)));
    assert_eq!(builds.load(Ordering::SeqCst), 0);
    assert!(!parent.is_cancelled());
    factory.retire().await.unwrap();
}
