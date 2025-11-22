//! Unpublished instance inputs are retired outside caller and registry locks.
use parrot_api::{
    errors::ActorError,
    types::{ActorResult, BoxedFuture},
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

type Receipt = watch::Receiver<Option<Result<(), String>>>;

/// A cleanup executor has two different resources: workers that are actively
/// dropping values, and owners waiting for one of those workers. The latter
/// must have a finite bound as well; otherwise a saturated executor can retain
/// an unbounded number of inputs in suspended tasks.
pub(crate) const MAX_PENDING_CLEANUPS_PER_SLOT: usize = 1024;
/// Bound retained cleanup owners across all independent runtime executors.
/// Per-executor limits alone still permit an unbounded process-wide backlog
/// when many systems are alive at once.
pub(crate) const MAX_PENDING_CLEANUPS_GLOBAL: usize = 8192;

struct PendingPool {
    // Retain the allocation identity even after the executor's last strong
    // owner exits. A still-live pending pool must not alias a reused address.
    active: Weak<Semaphore>,
    pending: Weak<Semaphore>,
}
static PENDING_CLEANUP_POOLS: OnceLock<Mutex<HashMap<usize, PendingPool>>> = OnceLock::new();
static GLOBAL_PENDING_CLEANUPS: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn global_pending_cleanup_slots() -> Arc<Semaphore> {
    GLOBAL_PENDING_CLEANUPS
        .get_or_init(|| Arc::new(Semaphore::new(MAX_PENDING_CLEANUPS_GLOBAL)))
        .clone()
}

fn pending_cleanup_slots(active: &Arc<Semaphore>) -> Arc<Semaphore> {
    let pools = PENDING_CLEANUP_POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let key = Arc::as_ptr(active) as usize;
    let mut pools = pools.lock().unwrap();
    pools.retain(|_, pool| pool.pending.strong_count() != 0);
    if let Some(pool) = pools
        .get(&key)
        .filter(|pool| pool.active.ptr_eq(&Arc::downgrade(active)))
        .and_then(|pool| pool.pending.upgrade())
    {
        return pool;
    }
    // `available_permits` can be zero while the first owner is already
    // executing. Keep at least one queue slot in that case.
    let capacity = active
        .available_permits()
        .max(1)
        .saturating_mul(MAX_PENDING_CLEANUPS_PER_SLOT);
    let pool = Arc::new(Semaphore::new(capacity));
    pools.insert(
        key,
        PendingPool {
            active: Arc::downgrade(active),
            pending: Arc::downgrade(&pool),
        },
    );
    pool
}

/// Reserve a bounded owner slot before spawning a cleanup waiter. `None`
/// means the caller must apply synchronous backpressure or leave the receipt
/// with an already-live owner; it must not create another suspended task.
pub(crate) struct PendingCleanupPermit {
    _local: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
}

pub(crate) fn try_pending_cleanup_permit(active: &Arc<Semaphore>) -> Option<PendingCleanupPermit> {
    let local = pending_cleanup_slots(active).try_acquire_owned().ok()?;
    let global = match global_pending_cleanup_slots().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            drop(local);
            return None;
        }
    };
    Some(PendingCleanupPermit {
        _local: local,
        _global: global,
    })
}

/// Wait before transferring a value out of an existing owner. Cancelling this
/// admission releases any partial reservation and leaves the value in place.
pub(crate) async fn pending_cleanup_permit(active: &Arc<Semaphore>) -> PendingCleanupPermit {
    let local = pending_cleanup_slots(active)
        .acquire_owned()
        .await
        .expect("private pending cleanup pool remains open");
    let global = global_pending_cleanup_slots()
        .acquire_owned()
        .await
        .expect("private global pending cleanup pool remains open");
    PendingCleanupPermit {
        _local: local,
        _global: global,
    }
}

#[cfg(test)]
mod expired_factory_contracts;
#[cfg(test)]
mod expired_system_contracts;
#[cfg(test)]
mod host_exit_contracts;
#[cfg(test)]
mod publication_contracts;
#[cfg(test)]
mod quota_contracts;
#[cfg(test)]
mod saturation_contracts;

std::thread_local! {
    static DISPOSING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) struct DisposalScope(Option<bool>);
impl DisposalScope {
    pub(crate) fn enter() -> Self {
        Self(DISPOSING.try_with(|active| active.replace(true)).ok())
    }
    pub(crate) fn active() -> bool {
        DISPOSING.try_with(std::cell::Cell::get).unwrap_or(false)
    }
}
impl Drop for DisposalScope {
    fn drop(&mut self) {
        if let Some(previous) = self.0 {
            let _ = DISPOSING.try_with(|active| active.set(previous));
        }
    }
}

/// Retire on the current thread even when it has entered an async runtime.
/// Nested cleanup shares this disposal scope instead of awaiting its own pool.
pub(crate) fn retire_synchronously<T>(value: T) -> ActorResult<()> {
    let _scope = DisposalScope::enter();
    PublicationInput::new(value).retire()
}

/// Input ownership inside an already bounded, synchronous publication owner.
/// Do not acquire another executor permit here: factory/child publication may
/// already hold the last one. Retire each input independently after releasing
/// registry locks, and return its actual disposal result.
pub(crate) struct PublicationInput<T>(Option<T>);
impl<T> PublicationInput<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Some(value))
    }
    pub(crate) fn value(&self) -> &T {
        self.0.as_ref().expect("unconsumed publication input")
    }
    pub(crate) fn value_mut(&mut self) -> &mut T {
        self.0.as_mut().expect("unconsumed publication input")
    }
    pub(crate) fn take(&mut self) -> T {
        self.0.take().expect("unconsumed publication input")
    }
    pub(crate) fn retire(mut self) -> ActorResult<()> {
        self.dispose()
    }
    fn dispose(&mut self) -> ActorResult<()> {
        let _scope = (self.0.is_some() && std::mem::needs_drop::<T>()).then(DisposalScope::enter);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.0.take()))).map_err(
            |payload| {
                ActorError::InternalError(format!(
                    "unpublished actor input disposal failed: {}",
                    panic_message(payload)
                ))
            },
        )
    }
}
impl<T> Drop for PublicationInput<T> {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}

/// Never run arbitrary formatting code, or let a panic payload's destructor
/// unwind a resource owner. Recursive destructor panics leak the final payload.
pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let text = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload");
    let mut message: String = text.chars().take(1024).collect();
    if message.len() < text.len() {
        message.push('…');
    }
    if let Err(secondary) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
    {
        message.push_str("; panic payload destructor panicked");
        std::mem::forget(secondary);
    }
    message
}

pub(crate) fn join_error(error: tokio::task::JoinError) -> String {
    if error.is_panic() {
        panic_message(error.into_panic())
    } else {
        error.to_string()
    }
}

pub(crate) fn publish_watch_value<T>(sender: &watch::Sender<T>, value: T) {
    let notified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sender.send_replace(value);
    }));
    if let Err(payload) = notified {
        let message = panic_message(payload);
        tracing::warn!(error = %message, "Watch receipt waker panicked");
    }
}

pub(crate) fn publish_watch<T>(sender: &watch::Sender<Option<T>>, value: T) {
    publish_watch_value(sender, Some(value));
}

pub(crate) fn publish_watch_if_modified<T>(
    sender: &watch::Sender<T>,
    update: impl FnOnce(&mut T) -> bool,
) {
    let notified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sender.send_if_modified(update);
    }));
    if let Err(payload) = notified {
        let message = panic_message(payload);
        tracing::warn!(error = %message, "Watch receipt waker panicked");
    }
}

fn publish_finished(
    finished: &watch::Sender<Option<Result<(), String>>>,
    result: Result<(), String>,
) {
    let notified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        finished.send_replace(Some(result));
    }));
    if let Err(payload) = notified {
        let message = panic_message(payload);
        tracing::warn!(error = %message, "Owned payload cleanup receipt waker panicked");
    }
}

// Installed before dispatch: cancellation of an unpolled cleanup task must also
// retire its value without unwinding through another capture's destructor.
struct Disposal<T> {
    context: Option<parrot_api::trace::TraceContext>,
    value: Option<T>,
    finished: watch::Sender<Option<Result<(), String>>>,
    reservation: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    pending: Option<OwnedSemaphorePermit>,
    global_pending: Option<OwnedSemaphorePermit>,
    active: Option<OwnedSemaphorePermit>,
}
impl<T> Drop for Disposal<T> {
    fn drop(&mut self) {
        let _scope = DisposalScope::enter();
        parrot_api::trace::in_scope(self.context.clone(), || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.value.take())))
                    .map_err(|payload| {
                        format!(
                            "unpublished actor input disposal failed: {}",
                            panic_message(payload)
                        )
                    });
            drop(self.reservation.take());
            drop(self.active.take());
            drop(self.pending.take());
            drop(self.global_pending.take());
            publish_finished(&self.finished, result);
        });
    }
}

pub(crate) struct OwnedPayload<T: Send + 'static> {
    // Ownership, not a later executor or cancellation caller, identifies cleanup.
    context: Option<parrot_api::trace::TraceContext>,
    value: Option<T>,
    runtime: Option<tokio::runtime::Handle>,
    slots: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    reservation: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    // Inputs transferred without an observer need no completion allocation.
    finished: std::sync::OnceLock<watch::Sender<Option<Result<(), String>>>>,
}
impl<T: Send + 'static> OwnedPayload<T> {
    pub(crate) fn new(value: T, runtime: &tokio::runtime::Handle, slots: &Arc<Semaphore>) -> Self {
        Self {
            context: parrot_api::trace::TraceContext::current(),
            value: Some(value),
            runtime: Some(runtime.clone()),
            slots: slots.clone(),
            pending: pending_cleanup_slots(slots),
            reservation: None,
            finished: std::sync::OnceLock::new(),
        }
    }
    /// Only the fixed-size creation input bundle shares this reservation.
    /// Disposal retains it independently of the submitting future or waiter.
    pub(crate) fn with_reservation(
        mut self,
        reservation: &Arc<tokio::sync::OwnedSemaphorePermit>,
    ) -> Self {
        self.reservation = Some(reservation.clone());
        self
    }
    pub(crate) fn release_reservation(&mut self) {
        drop(self.reservation.take());
    }
    pub(crate) fn with_executor(
        mut self,
        runtime: &tokio::runtime::Handle,
        slots: &Arc<Semaphore>,
    ) -> Self {
        self.runtime = Some(runtime.clone());
        self.slots = slots.clone();
        self.pending = pending_cleanup_slots(slots);
        self
    }
    /// Unbound contexts have no system cleanup executor. Use the caller's
    /// executor when present, with a shared bounded pool. Without an executor,
    /// disposal is synchronous but still isolates each input's destructor.
    pub(crate) fn unbound(value: T) -> Self {
        static SLOTS: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
        Self {
            context: parrot_api::trace::TraceContext::current(),
            value: Some(value),
            runtime: tokio::runtime::Handle::try_current().ok(),
            slots: SLOTS.get_or_init(|| Arc::new(Semaphore::new(1))).clone(),
            pending: pending_cleanup_slots(SLOTS.get_or_init(|| Arc::new(Semaphore::new(1)))),
            reservation: None,
            finished: std::sync::OnceLock::new(),
        }
    }
    pub(crate) fn value(&self) -> &T {
        self.value.as_ref().expect("unconsumed payload")
    }
    pub(crate) fn protect<R: Send + 'static>(&self, value: R) -> OwnedPayload<R> {
        OwnedPayload {
            context: self.context.clone(),
            value: Some(value),
            runtime: self.runtime.clone(),
            slots: self.slots.clone(),
            pending: self.pending.clone(),
            reservation: None,
            finished: std::sync::OnceLock::new(),
        }
    }
    pub(crate) fn take(mut self) -> T {
        let value = self.value.take().expect("unconsumed payload");
        drop(self.reservation.take());
        if let Some(finished) = self.finished.get() {
            publish_finished(finished, Ok(()));
        }
        value
    }
    pub(crate) fn receipt(&self) -> BoxedFuture<'static, ActorResult<()>> {
        wait(self.completion().subscribe())
    }
    fn completion(&self) -> &watch::Sender<Option<Result<(), String>>> {
        self.finished.get_or_init(|| watch::channel(None).0)
    }
    pub(crate) fn retire(self) -> BoxedFuture<'static, ActorResult<()>> {
        if !std::mem::needs_drop::<T>() {
            // Drop completes synchronously and notifies any existing observers.
            // There is no disposal work that needs a new completion channel.
            drop(self);
            return Box::pin(async { Ok(()) });
        }
        let receipt = self.receipt();
        // Start before returning the waiter; cancelling it cannot cancel disposal.
        drop(self);
        receipt
    }
}
impl<T: Send + 'static> Drop for OwnedPayload<T> {
    fn drop(&mut self) {
        let Some(value) = self.value.take() else {
            return;
        };
        if !std::mem::needs_drop::<T>() {
            drop(value);
            drop(self.reservation.take());
            if let Some(finished) = self.finished.get() {
                publish_finished(finished, Ok(()));
            }
            return;
        }
        let slots = self.slots.clone();
        let finished = self.completion().clone();
        let reservation = self.reservation.take();
        let pending = match self.pending.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                // Do not enqueue another owner once the bounded pending
                // queue is full. Dropping this owner synchronously applies
                // backpressure while preserving the same receipt and
                // destructor isolation as the asynchronous path.
                drop(Disposal {
                    context: self.context.clone(),
                    value: Some(value),
                    finished,
                    reservation,
                    pending: None,
                    global_pending: None,
                    active: None,
                });
                return;
            }
        };
        let global_pending = match global_pending_cleanup_slots().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                // The process-wide owner budget is exhausted. Release the
                // local reservation before applying synchronous backpressure;
                // the value still receives the same isolated receipt.
                drop(pending);
                drop(Disposal {
                    context: self.context.clone(),
                    value: Some(value),
                    finished,
                    reservation,
                    pending: None,
                    global_pending: None,
                    active: None,
                });
                return;
            }
        };
        let mut disposal = Disposal {
            context: self.context.clone(),
            value: Some(value),
            finished: finished.clone(),
            reservation,
            pending,
            global_pending,
            active: None,
        };
        // Reentrant cleanup already owns a bounded disposal executor. Waiting
        // for another permit can deadlock a destructor closing another owner.
        if DisposalScope::active() {
            drop(disposal);
            return;
        }
        let Some(runtime) = &self.runtime else {
            drop(disposal);
            return;
        };
        runtime.spawn(async move {
            // These private pools remain open through system retirement.
            disposal.active = slots.acquire_owned().await.ok();
            // Spawn can panic before returning a JoinHandle (OS exhaustion or
            // a worker-builder callback). Failed dispatch retires its captures;
            // retain that actual result instead of unwinding this cleanup task.
            let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tokio::task::spawn_blocking(move || {
                    drop(disposal);
                })
            }));
            let result = match dispatched {
                Ok(worker) => worker.await.map_err(join_error),
                Err(payload) => Err(panic_message(payload)),
            };
            if let Err(error) = result {
                // Disposal normally publishes the more specific payload result.
                if finished.borrow().is_none() {
                    publish_finished(&finished, Err(error));
                } else {
                    tracing::warn!(error = %error, "cleanup executor dispatch failed; actual disposal receipt retained");
                }
            }
        });
    }
}
fn wait(mut receipt: Receipt) -> BoxedFuture<'static, ActorResult<()>> {
    Box::pin(async move {
        loop {
            if let Some(result) = receipt.borrow_and_update().clone() {
                return result.map_err(ActorError::InternalError);
            }
            receipt.changed().await.map_err(|_| {
                ActorError::InternalError(
                    "unpublished input cleanup owner ended without a receipt".into(),
                )
            })?;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn retired_executor_pool_cannot_reduce_a_new_executors_cleanup_budget() {
        crate::test_support::isolated(
            "owned_payload::tests::retired_executor_pool_cannot_reduce_a_new_executors_cleanup_budget",
            || {
                let mut previous = None;
                for generation in 0..64 {
                    let active = Arc::new(Semaphore::new(1));
                    let mut current = Vec::new();
                    for index in 0..MAX_PENDING_CLEANUPS_PER_SLOT {
                        current.push(try_pending_cleanup_permit(&active).unwrap_or_else(|| {
                            panic!(
                                "new executor generation {generation} admitted only {index} cleanup owners"
                            )
                        }));
                    }
                    assert!(try_pending_cleanup_permit(&active).is_none());
                    // Keep a pending owner alive after its executor exits. A
                    // subsequent executor still needs its full independent quota.
                    drop(previous.take());
                    previous = current.pop();
                    drop(current);
                    drop(active);
                }
            },
        );
    }

    struct Hostile(Arc<AtomicUsize>);
    impl Drop for Hostile {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("unpolled disposal failure");
        }
    }

    #[test]
    fn unpolled_cleanup_retains_failure_receipt_when_runtime_shuts_down() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let value = OwnedPayload::new(
            Hostile(count.clone()),
            runtime.handle(),
            &Arc::new(Semaphore::new(1)),
        );
        let receipt = value.retire();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        // No block_on: the cleanup future has never been polled.
        drop(runtime);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let error = futures::executor::block_on(receipt).unwrap_err();
        assert!(
            error.to_string().contains("unpolled disposal failure"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn cancelled_retirement_waiter_does_not_cancel_input_destruction() {
        let count = Arc::new(AtomicUsize::new(0));
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.unwrap();
        let value = OwnedPayload::new(
            Hostile(count.clone()),
            &tokio::runtime::Handle::current(),
            &slots,
        );
        let receipt = value.receipt();
        drop(value.retire());
        tokio::task::yield_now().await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(permit);
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), receipt)
            .await
            .unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unpolled disposal failure")
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod receipt_contracts {
    use super::*;
    use std::{
        cell::Cell,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    struct Counted(Arc<AtomicUsize>);
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Hostile(Arc<AtomicUsize>);
    impl Drop for Hostile {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("receipt contract disposal failed");
        }
    }
    fn ready(mut receipt: BoxedFuture<'static, ActorResult<()>>) -> ActorResult<()> {
        match receipt
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => {
                panic!("completed ownership transfer/disposal left an unresolved receipt")
            }
        }
    }

    #[test]
    fn worker_builder_failure_preserves_actual_cleanup_receipts_and_recovery() {
        for hostile in [false, true] {
            let attempts = Arc::new(AtomicUsize::new(0));
            let called = attempts.clone();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .thread_name_fn(move || {
                    if called.fetch_add(1, Ordering::SeqCst) == 0 {
                        panic!("cleanup worker builder failure");
                    }
                    "healthy-cleanup-worker".to_owned()
                })
                .build()
                .unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let value: Box<dyn Send> = if hostile {
                Box::new(Hostile(count.clone()))
            } else {
                Box::new(Counted(count.clone()))
            };
            let input = OwnedPayload::new(value, runtime.handle(), &Arc::new(Semaphore::new(1)));
            let observer = input.receipt();
            let returned = input.retire();
            runtime.block_on(async {
                for receipt in [observer, returned] {
                    let result = tokio::time::timeout(Duration::from_secs(2), receipt)
                        .await
                        .expect("worker startup failure must still complete actual disposal");
                    if hostile {
                        assert!(
                            result
                                .unwrap_err()
                                .to_string()
                                .contains("receipt contract disposal failed")
                        );
                    } else {
                        result.unwrap();
                    }
                    assert_eq!(count.load(Ordering::SeqCst), 1);
                }
            });
            assert_eq!(runtime.block_on(runtime.spawn_blocking(|| 91)).unwrap(), 91);
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn synchronous_retirement_completes_nested_cleanup_with_exhausted_pool() {
        struct Nested {
            count: Arc<AtomicUsize>,
            runtime: tokio::runtime::Handle,
            slots: Arc<Semaphore>,
        }
        impl Drop for Nested {
            fn drop(&mut self) {
                let input =
                    OwnedPayload::new(Hostile(self.count.clone()), &self.runtime, &self.slots);
                let error = ready(input.retire()).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("receipt contract disposal failed")
                );
                panic!("outer synchronous disposal failure");
            }
        }
        let slots = Arc::new(Semaphore::new(1));
        let _permit = slots.clone().acquire_owned().await.unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        assert!(!DisposalScope::active());
        let error = retire_synchronously(Nested {
            count: count.clone(),
            runtime: tokio::runtime::Handle::current(),
            slots,
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outer synchronous disposal failure")
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(
            !DisposalScope::active(),
            "restore the caller's disposal scope"
        );
    }

    #[tokio::test]
    async fn take_resolves_polled_and_unpolled_receipts_without_disposal() {
        let count = Arc::new(AtomicUsize::new(0));
        let value = OwnedPayload::new(
            Counted(count.clone()),
            &tokio::runtime::Handle::current(),
            &Arc::new(Semaphore::new(1)),
        );
        let mut first = value.receipt();
        let second = value.receipt();
        let abandoned = value.receipt();
        assert!(futures::poll!(&mut first).is_pending());
        drop(abandoned);
        let transferred = value.take();
        ready(first).unwrap();
        ready(second).unwrap();
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "take transfers ownership, not disposal"
        );
        drop(transferred);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn saturated_cleanup_does_not_enqueue_unbounded_owners() {
        let slots = Arc::new(Semaphore::new(1));
        let active = slots.clone().acquire_owned().await.unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let runtime = tokio::runtime::Handle::current();

        // Keep the only active worker occupied. The first owner consumes one
        // bounded pending slot; later overflow owners must retire inline.
        let first = OwnedPayload::new(Counted(count.clone()), &runtime, &slots);
        drop(first.retire());
        tokio::task::yield_now().await;
        for _ in 0..=MAX_PENDING_CLEANUPS_PER_SLOT {
            let input = OwnedPayload::new(Counted(count.clone()), &runtime, &slots);
            drop(input.retire());
        }
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "overflow owners must not become waiting cleanup tasks"
        );

        drop(active);
        tokio::time::timeout(Duration::from_secs(2), async {
            while count.load(Ordering::SeqCst) != MAX_PENDING_CLEANUPS_PER_SLOT + 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bounded cleanup queue did not drain");
    }

    #[test]
    fn global_cleanup_budget_bounds_many_independent_executors() {
        const FLAG: &str = "PARROT_GLOBAL_CLEANUP_BUDGET_CASE";
        if std::env::var(FLAG).as_deref() != Ok("1") {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "owned_payload::receipt_contracts::global_cleanup_budget_bounds_many_independent_executors",
                    "--nocapture",
                ])
                .env(FLAG, "1")
                .status()
                .unwrap();
            assert!(
                status.success(),
                "global cleanup budget child failed: {status}"
            );
            return;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(4)
            .build()
            .unwrap();
        runtime.block_on(async {
            let count = Arc::new(AtomicUsize::new(0));
            let mut active = Vec::with_capacity(MAX_PENDING_CLEANUPS_GLOBAL + 1);
            for _ in 0..=MAX_PENDING_CLEANUPS_GLOBAL {
                let slots = Arc::new(Semaphore::new(1));
                active.push(slots.clone().acquire_owned().await.unwrap());
                let input = OwnedPayload::new(Counted(count.clone()), runtime.handle(), &slots);
                drop(input.retire());
            }

            assert_eq!(
                global_pending_cleanup_slots().available_permits(),
                0,
                "all process-wide pending cleanup permits must be accounted for"
            );
            assert_eq!(
                count.load(Ordering::SeqCst),
                1,
                "the owner beyond the global bound must retire synchronously"
            );

            drop(active);
            tokio::time::timeout(Duration::from_secs(10), async {
                while count.load(Ordering::SeqCst) != MAX_PENDING_CLEANUPS_GLOBAL + 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("global cleanup owners did not drain");
            assert_eq!(
                global_pending_cleanup_slots().available_permits(),
                MAX_PENDING_CLEANUPS_GLOBAL,
                "global cleanup permits must recover after actual disposal"
            );
        });
    }

    #[test]
    fn simultaneous_receipts_share_one_terminal_result() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let value = OwnedPayload::new(42u64, runtime.handle(), &Arc::new(Semaphore::new(1)));
        let barrier = std::sync::Barrier::new(4);
        let receipts = std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        value.receipt()
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(value.take(), 42);
        for receipt in receipts {
            ready(receipt).unwrap();
        }
    }
    #[test]
    fn trivial_drop_resolves_receipts_even_after_executor_shutdown() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        let value = OwnedPayload::new(42u64, &handle, &Arc::new(Semaphore::new(1)));
        let receipt = value.receipt();
        drop(value);
        ready(receipt).unwrap();
    }
    #[test]
    fn closed_executor_still_disposes_and_reports_failure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        let count = Arc::new(AtomicUsize::new(0));
        let value = OwnedPayload::new(
            Hostile(count.clone()),
            &handle,
            &Arc::new(Semaphore::new(1)),
        );
        let first = value.receipt();
        let second = value.retire();
        for receipt in [first, second] {
            assert!(
                ready(receipt)
                    .unwrap_err()
                    .to_string()
                    .contains("receipt contract disposal failed")
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn unbound_without_runtime_preserves_disposal_failure() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let count = Arc::new(AtomicUsize::new(0));
        let value = OwnedPayload::unbound(Hostile(count.clone()));
        let first = value.receipt();
        let second = value.retire();
        for receipt in [first, second] {
            assert!(
                ready(receipt)
                    .unwrap_err()
                    .to_string()
                    .contains("receipt contract disposal failed")
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn unobserved_drop_still_disposes_exactly_once() {
        let count = Arc::new(AtomicUsize::new(0));
        drop(OwnedPayload::new(
            Counted(count.clone()),
            &tokio::runtime::Handle::current(),
            &Arc::new(Semaphore::new(1)),
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            while count.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("unobserved input was not disposed");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    struct Blocking {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        count: Arc<AtomicUsize>,
    }
    impl Drop for Blocking {
        fn drop(&mut self) {
            self.entered.take().unwrap().send(()).unwrap();
            self.release
                .recv_timeout(Duration::from_secs(3))
                .expect("test must release destructor");
            self.count.fetch_add(1, Ordering::SeqCst);
            panic!("blocked disposal failed");
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_waiter_does_not_complete_or_abort_blocked_disposal() {
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let value = OwnedPayload::new(
            Blocking {
                entered: Some(entered),
                release: blocked,
                count: count.clone(),
            },
            &tokio::runtime::Handle::current(),
            &Arc::new(Semaphore::new(1)),
        );
        let mut observer = value.receipt();
        let abandoned = value.retire();
        tokio::time::timeout(Duration::from_secs(3), started)
            .await
            .unwrap()
            .unwrap();
        drop(abandoned);
        assert!(futures::poll!(&mut observer).is_pending());
        assert_eq!(count.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(3), observer)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("blocked disposal failed"));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn send_only_inputs_still_transfer_across_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<OwnedPayload<Cell<u64>>>();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let value = OwnedPayload::new(
            Cell::new(42u64),
            runtime.handle(),
            &Arc::new(Semaphore::new(1)),
        );
        let receipt = value.receipt();
        assert_eq!(
            std::thread::spawn(move || value.take().get())
                .join()
                .unwrap(),
            42
        );
        ready(receipt).unwrap();
    }
}

#[cfg(test)]
mod trivial_retirement_contracts {
    use super::*;
    use std::{
        cell::Cell,
        task::{Context, Poll, Waker},
    };

    fn completed(mut receipt: BoxedFuture<'static, ActorResult<()>>) {
        match receipt
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(result) => result.unwrap(),
            Poll::Pending => panic!("synchronous retirement left an observer pending"),
        }
    }

    #[test]
    fn unpolled_retirement_completes_both_existing_observers() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let slots = Arc::new(Semaphore::new(0));
        let input = OwnedPayload::new(42u64, runtime.handle(), &slots);
        let mut polled = input.receipt();
        let unpolled = input.receipt();
        assert!(
            polled
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let returned = input.retire();
        // No executor progress and no cleanup permits are available.
        completed(polled);
        completed(unpolled);
        completed(returned);
        assert_eq!(slots.available_permits(), 0);
    }

    #[test]
    fn cancelled_retirement_after_executor_shutdown_completes_observers() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        let input = OwnedPayload::new([7u8; 64], &handle, &Arc::new(Semaphore::new(0)));
        let first = input.receipt();
        let second = input.receipt();
        drop(input.retire());
        completed(first);
        completed(second);
    }

    #[test]
    fn send_only_input_retirement_notifies_origin_thread_before_waiter_poll() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let input = OwnedPayload::new(
            Cell::new(42u64),
            runtime.handle(),
            &Arc::new(Semaphore::new(0)),
        );
        let observer = input.receipt();
        let returned = std::thread::spawn(move || input.retire()).join().unwrap();
        completed(observer);
        completed(returned);
    }

    #[test]
    fn unbound_trivial_retirement_completes_without_executor() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        let input = OwnedPayload::unbound(42u64);
        let observer = input.receipt();
        let returned = input.retire();
        completed(observer);
        completed(returned);
    }
}

#[cfg(test)]
mod trace_disposal_tests {
    use super::*;
    use parrot_api::trace::{self, TraceContext};
    use std::sync::Mutex;
    struct Probe(Arc<Mutex<Vec<Option<TraceContext>>>>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.lock().unwrap().push(TraceContext::current());
        }
    }
    fn check(mode: u8) {
        for traced in [true, false] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let events = Arc::new(Mutex::new(Vec::new()));
            let parent = traced.then(TraceContext::root);
            let foreign = TraceContext::root();
            let payload = trace::in_scope(parent.clone(), || {
                if mode == 2 {
                    OwnedPayload::unbound(Probe(events.clone()))
                } else {
                    OwnedPayload::new(
                        Probe(events.clone()),
                        runtime.handle(),
                        &Arc::new(Semaphore::new(1)),
                    )
                }
            });
            let receipt = foreign.in_scope(|| payload.retire());
            if mode == 0 {
                runtime.block_on(receipt).unwrap();
            } else {
                foreign.in_scope(|| drop(runtime));
                futures::executor::block_on(receipt).unwrap();
            }
            assert_eq!(*events.lock().unwrap(), vec![parent]);
            assert!(TraceContext::current().is_none());
        }
    }
    #[test]
    fn background_disposal_keeps_construction_context() {
        check(0);
    }
    #[test]
    fn unpolled_runtime_disposal_keeps_construction_context() {
        check(1);
    }
    #[test]
    fn synchronous_disposal_keeps_construction_context() {
        check(2);
    }
}
