//! Bounded, generation-specific subscriptions for engines with owned lifecycles.
//!
//! Engines install a weak mailbox endpoint, call `close` at stop admission,
//! and call `terminated` only after the actor and its mailbox have been disposed.
//! A stop deadline alone is not termination. Notifications have a delivery
//! deadline and failures are observable; they are not durable messages.

use crate::{
    context::ActorTerminated,
    errors::ActorError,
    types::{ActorResult, BoxedFuture},
};
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::Duration,
};
use tokio::sync::watch;
use uuid::Uuid;

/// An engine endpoint must use a weak mailbox reference and must not own an actor.
/// `deliver` waits for mailbox capacity; its future must be cancellation safe.
pub trait TerminationRecipient: Send + Sync {
    fn deliver(&self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>>;
}

struct State {
    closed: bool,
    suspended: bool,
    incoming: HashMap<Uuid, Arc<Subscription>>,
    outgoing: HashMap<Uuid, Arc<Subscription>>,
}

struct Inner {
    id: OnceLock<Uuid>,
    limit: usize,
    state: Mutex<State>,
    endpoint: OnceLock<Arc<dyn TerminationRecipient>>,
    failures: AtomicU64,
}

struct Subscription {
    target: Weak<Inner>,
    watcher: Weak<Inner>,
    cancel: watch::Sender<bool>,
}

/// A generation identity and subscription owner, independent of actor paths.
#[derive(Clone)]
pub struct DeathWatch(Arc<Inner>);

impl DeathWatch {
    /// Limit applies independently to incoming and outgoing subscriptions.
    pub fn new(limit: usize) -> Self {
        Self(Arc::new(Inner {
            id: OnceLock::new(),
            limit,
            state: Mutex::new(State {
                closed: false,
                suspended: false,
                incoming: HashMap::new(),
                outgoing: HashMap::new(),
            }),
            endpoint: OnceLock::new(),
            failures: AtomicU64::new(0),
        }))
    }

    /// Stable across clones, suspension and close. Generated on first use;
    /// owners with no observed identity do not request random bytes at creation.
    pub fn id(&self) -> Uuid {
        self.0.id()
    }

    /// Install once, before publishing the actor reference to callers.
    pub fn set_recipient(&self, endpoint: Arc<dyn TerminationRecipient>) -> ActorResult<()> {
        self.0.endpoint.set(endpoint).map_err(|_| {
            ActorError::InternalError("death-watch recipient already installed".into())
        })
    }

    /// Register `watcher` on this target generation. Repeated registration is
    /// idempotent. Self-watch is a no-op. Closed targets/watchers return Stopped.
    pub fn watch(&self, watcher: &Self) -> ActorResult<()> {
        if self.id() == watcher.id() {
            let state = self.0.state.lock().unwrap();
            return if state.closed || state.suspended {
                Err(ActorError::Stopped)
            } else {
                Ok(())
            };
        }
        if watcher.0.endpoint.get().is_none() {
            return Err(ActorError::InternalError(
                "watcher has no termination recipient".into(),
            ));
        }
        // All two-owner operations acquire locks in generation order.
        let (mut target_state, mut watcher_state) = if self.id() < watcher.id() {
            let target = self.0.state.lock().unwrap();
            (target, watcher.0.state.lock().unwrap())
        } else {
            let watcher_state = watcher.0.state.lock().unwrap();
            (self.0.state.lock().unwrap(), watcher_state)
        };
        if target_state.closed
            || target_state.suspended
            || watcher_state.closed
            || watcher_state.suspended
        {
            return Err(ActorError::Stopped);
        }
        if target_state.incoming.contains_key(&watcher.id()) {
            return Ok(());
        }
        if target_state.incoming.len() >= self.0.limit
            || watcher_state.outgoing.len() >= watcher.0.limit
        {
            return Err(ActorError::InternalError(
                "death-watch subscription limit reached".into(),
            ));
        }
        let (cancel, _) = watch::channel(false);
        let subscription = Arc::new(Subscription {
            target: Arc::downgrade(&self.0),
            watcher: Arc::downgrade(&watcher.0),
            cancel,
        });
        target_state
            .incoming
            .insert(watcher.id(), subscription.clone());
        watcher_state.outgoing.insert(self.id(), subscription);
        Ok(())
    }

    /// Cancels a subscription, including a notification still waiting for
    /// admission. Messages already admitted to the mailbox cannot be recalled.
    pub fn unwatch(&self, watcher: &Self) {
        let subscription = watcher.0.state.lock().unwrap().outgoing.remove(&self.id());
        if let Some(subscription) = subscription {
            unlink(&subscription);
        }
    }

    /// Reject new subscriptions and release all outgoing edges immediately.
    /// Incoming edges remain until actual termination, not merely a stop request.
    pub fn close(&self) {
        let outgoing = {
            let mut state = self.0.state.lock().unwrap();
            state.closed = true;
            std::mem::take(&mut state.outgoing)
        };
        for subscription in outgoing.into_values() {
            unlink(&subscription);
        }
    }

    /// Suspend a supervised logical actor during retirement/restart. Existing
    /// observers remain attached to this identity; its own outgoing watches
    /// are cancelled and new subscriptions are rejected until resume.
    pub fn suspend(&self) {
        let outgoing = {
            let mut state = self.0.state.lock().unwrap();
            state.suspended = true;
            std::mem::take(&mut state.outgoing)
        };
        for subscription in outgoing.into_values() {
            unlink(&subscription);
        }
    }

    /// Resume only after the replacement has been published. Final close is
    /// irreversible; this cannot revive a stopped logical actor.
    pub fn resume(&self) -> ActorResult<()> {
        let mut state = self.0.state.lock().unwrap();
        if state.closed {
            return Err(ActorError::Stopped);
        }
        state.suspended = false;
        Ok(())
    }

    /// Count and ownership probes for admission and operational diagnostics.
    pub fn subscriptions(&self) -> (usize, usize) {
        let state = self.0.state.lock().unwrap();
        (state.incoming.len(), state.outgoing.len())
    }

    /// Failed or expired notification deliveries emitted by this target.
    pub fn delivery_failures(&self) -> u64 {
        self.0.failures.load(Ordering::Relaxed)
    }

    /// The engine lost its execution owner without a disposal receipt. Release
    /// subscriptions and count the missing notifications, without fabricating
    /// a termination event.
    pub fn abandon(&self) {
        self.close();
        let incoming = std::mem::take(&mut self.0.state.lock().unwrap().incoming);
        self.0
            .failures
            .fetch_add(incoming.len() as u64, Ordering::Relaxed);
        for subscription in incoming.into_values() {
            unlink(&subscription);
        }
    }

    /// Publish only after the engine confirms actual disposal. Concurrent
    /// recipients share a deadline; cancellation and failed recipients cannot
    /// strand other recipients or retain subscriptions after this future drops.
    pub async fn terminated(&self, event: ActorTerminated, timeout: Duration) {
        let _ = self.terminated_checked(event, timeout).await;
    }

    /// Same owned delivery as `terminated`, also returning the first delivery
    /// failure for compatibility with engines whose stop API reports it.
    pub async fn terminated_checked(
        &self,
        mut event: ActorTerminated,
        timeout: Duration,
    ) -> ActorResult<()> {
        self.close();
        event.generation = self.id();
        let incoming = std::mem::take(&mut self.0.state.lock().unwrap().incoming);
        // A guard is constructed for every edge before any await (even if the
        // whole notification future is cancelled before its first delivery).
        let guards: Vec<_> = incoming.into_values().map(DeliveryGuard).collect();
        let deadline = tokio::time::Instant::now().checked_add(timeout);
        let mut deliveries: FuturesUnordered<_> = guards.into_iter().map(|guard| {
            let event = event.clone();
            async move {
                let mut cancelled = guard.0.cancel.subscribe();
                let Some(watcher) = guard.0.watcher.upgrade() else { return Ok(()); };
                let Some(endpoint) = watcher.endpoint.get().cloned() else { return Ok(()); };
                drop(watcher);
                let Some(deadline) = deadline else { return Err(ActorError::Timeout); };
                let delivery = std::panic::AssertUnwindSafe(async { endpoint.deliver(event).await }).catch_unwind();
                tokio::select! {
                    biased;
                    _ = cancelled.wait_for(|cancelled| *cancelled) => Ok(()),
                    result = tokio::time::timeout_at(deadline, delivery) => match result {
                        Ok(Ok(result)) => result,
                        Ok(Err(_)) => Err(ActorError::Panic("termination recipient panicked".into())),
                        Err(_) => Err(ActorError::Timeout),
                    }
                }
            }
        }).collect();
        let mut failure = None;
        while let Some(result) = deliveries.next().await {
            if let Err(error) = result {
                self.0.failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(target = %self.id(), %error, "death-watch notification failed");
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

fn unlink(subscription: &Arc<Subscription>) {
    subscription.cancel.send_replace(true);
    if let (Some(target), Some(watcher)) = (
        subscription.target.upgrade(),
        subscription.watcher.upgrade(),
    ) {
        // Never hold both locks here; pointer checks protect a replacement edge
        // created between removal and cleanup by a concurrent watch call.
        {
            let mut state = target.state.lock().unwrap();
            if state
                .incoming
                .get(&watcher.id())
                .is_some_and(|current| Arc::ptr_eq(current, subscription))
            {
                state.incoming.remove(&watcher.id());
            }
        }
        let mut state = watcher.state.lock().unwrap();
        if state
            .outgoing
            .get(&target.id())
            .is_some_and(|current| Arc::ptr_eq(current, subscription))
        {
            state.outgoing.remove(&target.id());
        }
    }
}

struct DeliveryGuard(Arc<Subscription>);
impl Drop for DeliveryGuard {
    fn drop(&mut self) {
        unlink(&self.0);
    }
}

impl Inner {
    fn id(&self) -> Uuid {
        // Most newly registered actors have no subscriptions. Allocate the
        // UUID only on first observation, once for every clone of this owner.
        *self.id.get_or_init(Uuid::new_v4)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Edges cannot exist before watch() has observed both identities.
        // Unused owners need neither entropy nor initialization while dropping.
        let Some(id) = self.id.get().copied() else {
            return;
        };
        let state = self.state.get_mut().unwrap();
        // Weak ownership means the peer may already have gone away. Remove
        // surviving peer entries by generation; no user destructors run locked.
        for subscription in state.incoming.values().chain(state.outgoing.values()) {
            subscription.cancel.send_replace(true);
            if let Some(watcher) = subscription.watcher.upgrade() {
                watcher.state.lock().unwrap().outgoing.remove(&id);
            }
            if let Some(target) = subscription.target.upgrade() {
                target.state.lock().unwrap().incoming.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeathReason;
    struct Sink;
    impl TerminationRecipient for Sink {
        fn deliver(&self, _: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
    }
    fn owner(limit: usize) -> DeathWatch {
        let owner = DeathWatch::new(limit);
        owner.set_recipient(Arc::new(Sink)).unwrap();
        owner
    }
    #[test]
    fn concurrent_first_observation_has_one_identity_through_close() {
        let actor = owner(2);
        let barrier = std::sync::Barrier::new(8);
        let identities = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8)
                .map(|_| {
                    let actor = actor.clone();
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        let id = actor.id();
                        for _ in 0..32 {
                            assert_eq!(actor.id(), id);
                        }
                        id
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        let id = identities[0];
        assert!(identities.into_iter().all(|observed| observed == id));
        assert_eq!(id.get_version_num(), 4);
        assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
        let observer = owner(2);
        assert_ne!(observer.id(), id);
        actor.watch(&observer).unwrap();
        actor.suspend();
        actor.resume().unwrap();
        actor.close();
        assert_eq!(actor.id(), id);
        assert_eq!(observer.subscriptions(), (0, 1));
        drop(actor);
        assert_eq!(observer.subscriptions(), (0, 0));
    }

    #[test]
    fn supervised_suspension_preserves_observers_but_releases_outgoing_watches() {
        let actor = owner(2);
        let observer = owner(2);
        let dependency = owner(2);
        actor.watch(&observer).unwrap();
        dependency.watch(&actor).unwrap();
        let identity = actor.id();
        actor.suspend();
        assert_eq!(actor.subscriptions(), (1, 0));
        assert_eq!(dependency.subscriptions(), (0, 0));
        assert!(actor.watch(&owner(2)).is_err());
        assert!(dependency.watch(&actor).is_err());
        actor.resume().unwrap();
        assert_eq!(actor.id(), identity);
        dependency.watch(&actor).unwrap();
        actor.close();
        assert!(
            actor.resume().is_err(),
            "a final close must never resurrect the logical actor"
        );
        assert_eq!(actor.subscriptions(), (1, 0));
    }

    #[test]
    fn bounded_bidirectional_admission_and_generation_drop_release_edges() {
        let target = owner(1);
        let watcher = owner(1);
        target.watch(&watcher).unwrap();
        target.watch(&watcher).unwrap();
        assert!(target.watch(&owner(1)).is_err());
        assert!(owner(1).watch(&watcher).is_err());
        target.unwatch(&watcher);
        let next = owner(1);
        next.watch(&watcher).unwrap();
        drop(next);
        assert_eq!(watcher.subscriptions(), (0, 0));
        target.watch(&watcher).unwrap();
        drop(watcher);
        assert_eq!(target.subscriptions(), (0, 0));
    }
    #[test]
    fn mutual_subscriptions_have_no_strong_owner_cycle() {
        let a = owner(2);
        let b = owner(2);
        let weak_a = Arc::downgrade(&a.0);
        let weak_b = Arc::downgrade(&b.0);
        a.watch(&b).unwrap();
        b.watch(&a).unwrap();
        drop(a);
        assert!(weak_a.upgrade().is_none());
        assert_eq!(b.subscriptions(), (0, 0));
        drop(b);
        assert!(weak_b.upgrade().is_none());
    }
    #[test]
    fn concurrent_registration_and_close_leave_no_stranded_subscription() {
        for _ in 0..100 {
            let a = owner(2);
            let b = owner(2);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let _ = a.watch(&b);
                    let _ = b.watch(&a);
                });
                scope.spawn(|| {
                    a.close();
                    b.close();
                });
            });
            assert_eq!(a.subscriptions(), (0, 0));
            assert_eq!(b.subscriptions(), (0, 0));
            assert!(matches!(a.watch(&b), Err(ActorError::Stopped)));
        }
    }
    struct Pending;
    impl TerminationRecipient for Pending {
        fn deliver(&self, _: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(std::future::pending())
        }
    }
    #[tokio::test]
    async fn cancellation_of_notification_owner_releases_every_edge() {
        let target = owner(2);
        let a = DeathWatch::new(1);
        a.set_recipient(Arc::new(Pending)).unwrap();
        let b = DeathWatch::new(1);
        b.set_recipient(Arc::new(Pending)).unwrap();
        target.watch(&a).unwrap();
        target.watch(&b).unwrap();
        let mut delivery = Box::pin(target.terminated(
            ActorTerminated {
                generation: target.id(),
                path: "p".into(),
                reason: DeathReason::Normal,
            },
            Duration::from_secs(60),
        ));
        assert!(futures::poll!(&mut delivery).is_pending());
        drop(delivery);
        assert_eq!(a.subscriptions(), (0, 0));
        assert_eq!(b.subscriptions(), (0, 0));
        assert_eq!(target.subscriptions(), (0, 0));
    }
}
