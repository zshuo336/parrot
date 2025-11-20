use super::super::super::{receive_segment, receive_views, segments};
use super::*;
use crate::system::{MembershipOwner, MEMBERSHIP_ACQUIRE_OBSERVER};
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Poll, Wake, Waker},
};

struct ShutdownProbe {
    membership: Arc<tokio::sync::Mutex<crate::swim::Membership>>,
    calls: AtomicUsize,
    held: AtomicBool,
}

impl Wake for ShutdownProbe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.held
            .fetch_or(self.membership.try_lock().is_err(), Ordering::SeqCst);
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

struct ShutdownWaiters {
    probe: Arc<ShutdownProbe>,
    waiters: Vec<Pin<Box<dyn Future<Output = bool>>>>,
}

impl ShutdownWaiters {
    fn register(system: &RemoteActorSystem) -> Self {
        let probe = Arc::new(ShutdownProbe {
            membership: system.membership.clone(),
            calls: AtomicUsize::new(0),
            held: AtomicBool::new(false),
        });
        let waker = Waker::from(probe.clone());
        let mut context = Context::from_waker(&waker);
        let mut waiters: Vec<Pin<Box<dyn Future<Output = bool>>>> = Vec::new();
        for _ in 0..40 {
            let mut signal = system.shutdown_signal();
            let mut waiter: Pin<Box<dyn Future<Output = bool>>> = Box::pin(async move {
                let changed = signal.changed().await.is_ok();
                changed && *signal.borrow()
            });
            assert!(waiter.as_mut().poll(&mut context).is_pending());
            waiters.push(waiter);
        }
        Self { probe, waiters }
    }

    fn assert_notified_outside_membership(&mut self) {
        let waker = Waker::from(self.probe.clone());
        let mut context = Context::from_waker(&waker);
        let calls = self.probe.calls.load(Ordering::SeqCst);
        let ready = self
            .waiters
            .iter_mut()
            .all(|waiter| matches!(waiter.as_mut().poll(&mut context), Poll::Ready(true)));
        assert_eq!(calls, 40, "shutdown must notify the original forty waiters");
        assert!(ready, "all notified shutdown waiters must complete");
        assert!(
            !self.probe.held.load(Ordering::SeqCst),
            "last-owner shutdown notified while the receiver held membership"
        );
    }
}

#[derive(Clone, Copy)]
enum Case {
    MissingMember,
    SupersededConnection,
    Accepted,
    MalformedTail,
    PartialSegment,
}

struct AcquireObserver;
impl Drop for AcquireObserver {
    fn drop(&mut self) {
        MEMBERSHIP_ACQUIRE_OBSERVER.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

async fn exercise(case: Case, label: &str) {
    let node = |suffix| {
        RemoteActorSystem::new(
            RemoteConfig::mem(format!("last-owner-{label}-{suffix}"))
                .with_insecure_development()
                .with_receptionist_acl(AclRules::permissive())
                .with_receptionist_relay_acl(AclRules::permissive()),
            Arc::new(Empty),
        )
        .unwrap()
    };
    let system = node("a");
    let peer = node("b");
    // Establish real HELLO state without starting periodic system owners.
    memory_pair(&system, &peer).await;
    let back = system.direct_sender_of(&peer.config.node_id).unwrap();
    if matches!(case, Case::SupersededConnection) {
        // Witness the replacement's publication rather than the first link's
        // existing member. A queued lock permit would make try_lock ambiguous.
        system
            .membership
            .lock()
            .await
            .members
            .remove(&peer.config.node_id);
        memory_pair(&system, &peer).await;
        assert!(!system
            .direct_sender_of(&peer.config.node_id)
            .unwrap()
            .same_connection(&back));
    }
    if matches!(case, Case::MissingMember) {
        system
            .membership
            .lock()
            .await
            .members
            .remove(&peer.config.node_id);
    }
    let suffixes: Vec<_> = (0..32768).map(|i| format!("service-{i}")).collect();
    let mut advertisement = view(
        &[&peer.config.node_id],
        "edge/last-owner",
        &suffixes.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    if matches!(case, Case::MalformedTail) {
        advertisement.entries.last_mut().unwrap().node = "wrong-origin".into();
    }
    let body = if matches!(case, Case::PartialSegment) {
        let data = vec![0; (back.max_frame_len() as usize).min(segments::MAX_SNAPSHOT) - 128];
        segment(1, segments::MAX_SNAPSHOT, 0, &data)
    } else {
        Vec::new()
    };
    let mut waiters = ShutdownWaiters::register(&system);
    assert_eq!(Arc::strong_count(&system), 1);
    let weak = Arc::downgrade(&system);
    let transferred = Arc::new(AtomicBool::new(false));
    let observed_transfer = transferred.clone();
    // Release the external owner after the real receiver acquires both its
    // membership guard and system owner, without racing a polling OS thread.
    MEMBERSHIP_ACQUIRE_OBSERVER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Box::new(move |acquired| {
            assert!(std::ptr::eq(acquired, Arc::as_ptr(&system)));
            assert_eq!(Arc::strong_count(&system), 2);
            assert!(system.membership.try_lock().is_err());
            drop(system);
            observed_transfer.store(true, Ordering::SeqCst);
        }));
    });
    let _observer = AcquireObserver;
    if matches!(case, Case::PartialSegment) {
        receive_segment(weak.clone(), &body, &back, true).await;
    } else {
        receive_views(weak.clone(), vec![advertisement], &back, true).await;
    }
    peer.shutdown().await.unwrap();
    assert!(
        transferred.load(Ordering::SeqCst),
        "fixture never witnessed the receiver holding the last system owner"
    );
    assert!(
        weak.upgrade().is_none(),
        "receiver retained the system owner"
    );
    waiters.assert_notified_outside_membership();
}

fn idle_system(label: &str) -> Arc<RemoteActorSystem> {
    RemoteActorSystem::new(
        RemoteConfig::mem(format!("membership-owner-{label}")).with_insecure_development(),
        Arc::new(Empty),
    )
    .unwrap()
}

#[tokio::test]
async fn guard_drop_unlocks_before_last_owner_shutdown() {
    let system = idle_system("drop");
    let membership = system.membership.clone();
    let weak = Arc::downgrade(&system);
    let mut waiters = ShutdownWaiters::register(&system);
    let owner = MembershipOwner::acquire(&weak, &membership).await.unwrap();
    drop(system);
    drop(owner);
    assert!(weak.upgrade().is_none());
    waiters.assert_notified_outside_membership();
}

#[tokio::test]
async fn guard_unwind_unlocks_before_last_owner_shutdown() {
    let system = idle_system("unwind");
    let membership = system.membership.clone();
    let weak = Arc::downgrade(&system);
    let mut waiters = ShutdownWaiters::register(&system);
    let owner = MembershipOwner::acquire(&weak, &membership).await.unwrap();
    drop(system);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _owner = owner;
        panic!("exercise membership owner unwinding");
    }));
    assert!(result.is_err());
    assert!(weak.upgrade().is_none());
    waiters.assert_notified_outside_membership();
}

#[tokio::test]
async fn pending_acquisition_does_not_retain_system_owner() {
    let system = idle_system("pending");
    let membership = system.membership.clone();
    let weak = Arc::downgrade(&system);
    let guard = membership.lock().await;
    let mut acquisition = Box::pin(MembershipOwner::acquire(&weak, &membership));
    let mut context = Context::from_waker(Waker::noop());
    assert!(acquisition.as_mut().poll(&mut context).is_pending());
    let pending_owners = Arc::strong_count(&system);
    drop(system);
    let expired_while_waiting = weak.upgrade().is_none();
    drop(guard);
    let acquired = acquisition.await;
    let rejected = acquired.is_none();
    drop(acquired);
    assert_eq!(
        pending_owners, 1,
        "lock waiter retained a strong system owner"
    );
    assert!(
        expired_while_waiting,
        "lock waiter delayed system destruction"
    );
    assert!(rejected, "lock waiter acquired an already dropped system");
    assert!(membership.try_lock().is_ok());
}

#[tokio::test]
async fn cancelled_acquisition_releases_waiter_without_system_owner() {
    let system = idle_system("cancelled");
    let membership = system.membership.clone();
    let weak = Arc::downgrade(&system);
    let guard = membership.lock().await;
    let mut acquisition = Box::pin(MembershipOwner::acquire(&weak, &membership));
    let mut context = Context::from_waker(Waker::noop());
    assert!(acquisition.as_mut().poll(&mut context).is_pending());
    let pending_owners = Arc::strong_count(&system);
    drop(acquisition);
    drop(guard);
    let successor = MembershipOwner::acquire(&weak, &membership).await;
    let successor_acquired = successor.is_some();
    drop(successor);
    let mut waiters = ShutdownWaiters::register(&system);
    drop(system);
    assert_eq!(pending_owners, 1, "cancelled lock waiter owned the system");
    assert!(successor_acquired, "cancelled waiter blocked its successor");
    assert!(weak.upgrade().is_none());
    waiters.assert_notified_outside_membership();
    assert!(membership.try_lock().is_ok());
}

#[tokio::test]
async fn missing_member_releases_membership_before_last_owner_shutdown() {
    exercise(Case::MissingMember, "missing").await;
}

#[tokio::test]
async fn superseded_connection_releases_membership_before_last_owner_shutdown() {
    exercise(Case::SupersededConnection, "superseded").await;
}

#[tokio::test]
async fn accepted_snapshot_releases_membership_before_last_owner_shutdown() {
    exercise(Case::Accepted, "accepted").await;
}

#[tokio::test]
async fn malformed_snapshot_releases_membership_before_last_owner_shutdown() {
    exercise(Case::MalformedTail, "malformed").await;
}

#[tokio::test]
async fn partial_segment_releases_membership_before_last_owner_shutdown() {
    exercise(Case::PartialSegment, "segment").await;
}
