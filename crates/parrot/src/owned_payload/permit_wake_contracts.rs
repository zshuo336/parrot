use super::*;
use std::{
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};

struct PanicPayload(Arc<AtomicUsize>);
impl Drop for PanicPayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("quota wake panic payload destructor");
    }
}
struct HostileWake {
    calls: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl Wake for HostileWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(PanicPayload(self.payloads.clone()));
    }
}

pub(crate) struct Waiter<T> {
    future: BoxedFuture<'static, T>,
    calls: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl<T> Waiter<T> {
    pub(crate) fn ready(mut self) -> T {
        assert_eq!(self.calls.load(Ordering::SeqCst), 1);
        assert_eq!(self.payloads.load(Ordering::SeqCst), 1);
        match self
            .future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("the panicking wake lost its granted capacity"),
        }
    }
}
pub(crate) fn watch<T>(future: impl Future<Output = T> + Send + 'static) -> Waiter<T> {
    let calls = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(AtomicUsize::new(0));
    let waker = Waker::from(Arc::new(HostileWake {
        calls: calls.clone(),
        payloads: payloads.clone(),
    }));
    let mut future: BoxedFuture<'static, T> = Box::pin(future);
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    Waiter {
        future,
        calls,
        payloads,
    }
}
pub(crate) fn quota() -> (
    OwnedSemaphorePermit,
    Waiter<Result<OwnedSemaphorePermit, tokio::sync::AcquireError>>,
) {
    let semaphore = Arc::new(Semaphore::new(1));
    let permit = semaphore.clone().try_acquire_owned().unwrap();
    (permit, watch(semaphore.acquire_owned()))
}

struct Input(Arc<AtomicUsize>, bool);
impl Drop for Input {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(!self.1, "original input disposal witness");
    }
}
fn assert_result(result: ActorResult<()>, failure: bool) {
    if failure {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("original input disposal witness")
        );
    } else {
        result.unwrap();
    }
}

#[test]
fn owned_disposal_keeps_actual_receipt_when_creation_quota_wake_panics() {
    crate::test_support::isolated(
        "owned_payload::permit_wake_contracts::owned_disposal_keeps_actual_receipt_when_creation_quota_wake_panics",
        || {
            for mode in 0..3 {
                for failure in [false, true] {
                    let host = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let slots = Arc::new(Semaphore::new(1));
                    let (permit, waiter) = quota();
                    let reservation = Arc::new(permit);
                    let drops = Arc::new(AtomicUsize::new(0));
                    let input =
                        OwnedPayload::new(Input(drops.clone(), failure), host.handle(), &slots)
                            .with_reservation(&reservation);
                    drop(reservation);
                    let receipt = input.receipt();
                    let repeated = input.receipt();
                    match mode {
                        0 => {
                            let _scope = DisposalScope::enter();
                            drop(input.retire());
                        }
                        1 => drop(input.retire()),
                        _ => {
                            let unwind =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    let _scope = DisposalScope::enter();
                                    let _input = input;
                                    panic!("outer unwind witness");
                                }));
                            assert_eq!(
                                unwind.unwrap_err().downcast_ref::<&str>(),
                                Some(&"outer unwind witness")
                            );
                        }
                    }
                    host.block_on(async {
                        for receipt in [receipt, repeated] {
                            let result = tokio::time::timeout(Duration::from_secs(3), receipt)
                                .await
                                .unwrap();
                            assert_result(result, failure);
                        }
                    });
                    assert_eq!(drops.load(Ordering::SeqCst), 1);
                    assert_eq!(slots.available_permits(), 1);
                    assert_eq!(
                        pending_cleanup_slots(&slots).available_permits(),
                        MAX_PENDING_CLEANUPS_PER_SLOT
                    );
                    assert_eq!(
                        global_pending_cleanup_slots().available_permits(),
                        MAX_PENDING_CLEANUPS_GLOBAL
                    );
                    drop(waiter.ready().unwrap());
                }
            }
        },
    );
}

#[test]
fn transferred_and_trivial_inputs_keep_receipts_when_reservation_wake_panics() {
    crate::test_support::isolated(
        "owned_payload::permit_wake_contracts::transferred_and_trivial_inputs_keep_receipts_when_reservation_wake_panics",
        || {
            let host = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let slots = Arc::new(Semaphore::new(1));
            for explicit in [false, true] {
                let (permit, waiter) = quota();
                let reservation = Arc::new(permit);
                let drops = Arc::new(AtomicUsize::new(0));
                let mut input =
                    OwnedPayload::new(Input(drops.clone(), false), host.handle(), &slots)
                        .with_reservation(&reservation);
                drop(reservation);
                let receipt = input.receipt();
                if explicit {
                    input.release_reservation();
                }
                let transferred = input.take();
                futures::executor::block_on(receipt).unwrap();
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                drop(waiter.ready().unwrap());
                drop(transferred);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            }
            let (permit, waiter) = quota();
            let reservation = Arc::new(permit);
            let input =
                OwnedPayload::new(17u32, host.handle(), &slots).with_reservation(&reservation);
            drop(reservation);
            let receipt = input.receipt();
            futures::executor::block_on(input.retire()).unwrap();
            futures::executor::block_on(receipt).unwrap();
            drop(waiter.ready().unwrap());
        },
    );
}

#[test]
fn pending_quota_release_isolates_local_and_global_capacity_wakers() {
    crate::test_support::isolated(
        "owned_payload::permit_wake_contracts::pending_quota_release_isolates_local_and_global_capacity_wakers",
        || {
            for global in [false, true] {
                let slots = Arc::new(Semaphore::new(if global { 8 } else { 1 }));
                let count = if global {
                    MAX_PENDING_CLEANUPS_GLOBAL
                } else {
                    MAX_PENDING_CLEANUPS_PER_SLOT
                };
                let mut held: Vec<_> = (0..count)
                    .map(|_| try_pending_cleanup_permit(&slots).unwrap())
                    .collect();
                let next_slots = if global {
                    Arc::new(Semaphore::new(1))
                } else {
                    slots.clone()
                };
                let waiter = watch(async move { pending_cleanup_permit(&next_slots).await });
                drop(held.pop());
                drop(waiter.ready());
                drop(held);
                assert_eq!(
                    global_pending_cleanup_slots().available_permits(),
                    MAX_PENDING_CLEANUPS_GLOBAL
                );
            }
        },
    );
}

#[test]
fn cancelling_partial_pending_admission_contains_local_waiter_wake() {
    crate::test_support::isolated(
        "owned_payload::permit_wake_contracts::cancelling_partial_pending_admission_contains_local_waiter_wake",
        || {
            let filler = Arc::new(Semaphore::new(8));
            let global: Vec<_> = (0..MAX_PENDING_CLEANUPS_GLOBAL)
                .map(|_| try_pending_cleanup_permit(&filler).unwrap())
                .collect();
            let slots = Arc::new(Semaphore::new(1));
            let local = pending_cleanup_slots(&slots);
            let held = local
                .clone()
                .try_acquire_many_owned((MAX_PENDING_CLEANUPS_PER_SLOT - 1) as u32)
                .unwrap();
            let mut partial = Box::pin(pending_cleanup_permit(&slots));
            assert!(
                partial
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            let waiter = watch(local.clone().acquire_owned());
            drop(partial);
            drop(waiter.ready().unwrap());
            drop(held);
            drop(global);
            assert_eq!(local.available_permits(), MAX_PENDING_CLEANUPS_PER_SLOT);
            assert_eq!(
                global_pending_cleanup_slots().available_permits(),
                MAX_PENDING_CLEANUPS_GLOBAL
            );
        },
    );
}
