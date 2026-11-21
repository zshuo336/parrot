use super::*;
use crate::{
    constructor::{Factory, prepared::Prepared},
    test_support::isolated,
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
}

struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn completed<T>(mut receipt: BoxedFuture<'static, T>) -> T {
    match receipt
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("publication cleanup queued behind its own worker or permit"),
    }
}

struct Reenter {
    runtime: tokio::runtime::Handle,
    slots: Arc<Semaphore>,
    count: Arc<AtomicUsize>,
}
impl Drop for Reenter {
    fn drop(&mut self) {
        let input = OwnedPayload::new(Counted(self.count.clone()), &self.runtime, &self.slots);
        completed(input.retire()).unwrap();
    }
}
struct PanicPayload(Reenter);
impl Drop for PanicPayload {
    fn drop(&mut self) {
        // Keep the nested ownership in the panic payload itself.
        let _ = &self.0;
    }
}
struct PanicWithPayload(Option<Reenter>);
impl Drop for PanicWithPayload {
    fn drop(&mut self) {
        std::panic::panic_any(PanicPayload(self.0.take().unwrap()));
    }
}

fn publication(mode: u8) {
    let runtime = runtime();
    let slots = Arc::new(Semaphore::new(1));
    let count = Arc::new(AtomicUsize::new(0));
    let nested = Reenter {
        runtime: runtime.handle().clone(),
        slots: slots.clone(),
        count: count.clone(),
    };
    runtime.block_on(async {
        let permit = slots.clone().acquire_owned().await.unwrap();
        let observed = count.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match mode {
                0 => PublicationInput::new(nested).retire().unwrap(),
                1 => drop(PublicationInput::new(nested)),
                2 => {
                    let error = PublicationInput::new(PanicWithPayload(Some(nested)))
                        .retire()
                        .unwrap_err()
                        .to_string();
                    assert!(error.contains("non-string panic payload"), "{error}");
                    assert!(
                        !error.contains("panic payload destructor panicked"),
                        "{error}"
                    );
                }
                _ => unreachable!(),
            }
            assert_eq!(
                observed.load(Ordering::SeqCst),
                1,
                "publication returned before actual nested disposal"
            );
            assert!(
                !DisposalScope::active(),
                "restore the reused worker's scope"
            );
        })
        .await
        .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(tokio::task::spawn_blocking(|| 73).await.unwrap(), 73);
    });
}

#[test]
fn explicit_publication_retirement_reenters_cleanup() {
    isolated(
        "owned_payload::publication_contracts::explicit_publication_retirement_reenters_cleanup",
        || publication(0),
    );
}
#[test]
fn implicit_publication_drop_reenters_cleanup() {
    isolated(
        "owned_payload::publication_contracts::implicit_publication_drop_reenters_cleanup",
        || publication(1),
    );
}
#[test]
fn publication_panic_payload_reenters_cleanup_without_secondary_failure() {
    isolated(
        "owned_payload::publication_contracts::publication_panic_payload_reenters_cleanup_without_secondary_failure",
        || publication(2),
    );
}

#[test]
fn publication_receipt_waker_panic_does_not_unwind_disposal() {
    isolated(
        "owned_payload::publication_contracts::publication_receipt_waker_panic_does_not_unwind_disposal",
        || {
            struct WakePanic(AtomicUsize);
            impl futures::task::ArcWake for WakePanic {
                fn wake_by_ref(this: &Arc<Self>) {
                    this.0.fetch_add(1, Ordering::SeqCst);
                    panic!("publication receipt wake failure");
                }
            }

            let runtime = runtime();
            let slots = Arc::new(Semaphore::new(1));
            let count = Arc::new(AtomicUsize::new(0));
            let handle = runtime.handle().clone();
            runtime.block_on(async {
                let input = OwnedPayload::new(Counted(count.clone()), &handle, &slots);
                let mut receipt = input.receipt();
                let wake = Arc::new(WakePanic(AtomicUsize::new(0)));
                let waker = futures::task::waker(wake.clone());
                let mut context = Context::from_waker(&waker);
                assert!(receipt.as_mut().poll(&mut context).is_pending());
                drop(input);
                tokio::time::timeout(std::time::Duration::from_secs(8), async {
                    while count.load(Ordering::SeqCst) == 0 || wake.0.load(Ordering::SeqCst) == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(wake.0.load(Ordering::SeqCst), 1);
                assert_eq!(count.load(Ordering::SeqCst), 1);
                assert!(matches!(
                    receipt.as_mut().poll(&mut context),
                    Poll::Ready(Ok(()))
                ));
            });
        },
    );
}

#[test]
fn publication_transfer_preserves_original_value_without_early_disposal() {
    isolated(
        "owned_payload::publication_contracts::publication_transfer_preserves_original_value_without_early_disposal",
        || {
            let count = Arc::new(AtomicUsize::new(0));
            let mut input = PublicationInput::new(Counted(count.clone()));
            let original = Arc::as_ptr(&input.value().0);
            let output = input.take();
            drop(input);
            assert_eq!(Arc::as_ptr(&output.0), original);
            assert_eq!(count.load(Ordering::SeqCst), 0);
            drop(output);
            assert_eq!(count.load(Ordering::SeqCst), 1);
            assert!(!DisposalScope::active());
        },
    );
}

struct Capture {
    count: Arc<AtomicUsize>,
    hostile: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        assert!(!self.hostile, "active factory capture failure");
    }
}
struct Active {
    factory: Arc<Factory<Counted>>,
    prepared: Option<Prepared<Counted>>,
    outputs: Arc<AtomicUsize>,
    captures: Arc<AtomicUsize>,
    hostile: bool,
}
impl Drop for Active {
    fn drop(&mut self) {
        let mut first = self.factory.retire();
        let mut second = self.factory.retire();
        assert!(
            first
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(
            second
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(self.outputs.load(Ordering::SeqCst), 0);
        assert_eq!(self.captures.load(Ordering::SeqCst), 0);
        let discard = completed(self.prepared.take().unwrap().discard());
        let first = completed(first);
        let second = completed(second);
        if self.hostile {
            for result in [discard, first, second] {
                assert!(
                    result
                        .unwrap_err()
                        .contains("active factory capture failure")
                );
            }
        } else {
            discard.unwrap();
            first.unwrap();
            second.unwrap();
        }
        assert_eq!(self.outputs.load(Ordering::SeqCst), 1);
        assert_eq!(self.captures.load(Ordering::SeqCst), 1);
    }
}
fn active_factory(hostile: bool) {
    let runtime = runtime();
    let slots = Arc::new(Semaphore::new(1));
    let outputs = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let capture = Capture {
        count: captures.clone(),
        hostile,
    };
    let created = outputs.clone();
    let factory = Factory::new(
        move || {
            let _ = &capture;
            Counted(created.clone())
        },
        Arc::new(Semaphore::new(1)),
        slots.clone(),
    );
    let prepared = runtime
        .block_on(factory.prepare::<()>(Arc::default()))
        .unwrap();
    runtime.block_on(async {
        let permit = slots.acquire_owned().await.unwrap();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            retire_synchronously(Active {
                factory,
                prepared: Some(prepared),
                outputs,
                captures,
                hostile,
            })
            .unwrap();
        })
        .await
        .unwrap();
    });
}
#[test]
fn nested_active_factory_waits_for_its_live_prepared_output() {
    isolated(
        "owned_payload::publication_contracts::nested_active_factory_waits_for_its_live_prepared_output",
        || active_factory(false),
    );
}
#[test]
fn nested_active_factory_retains_capture_failure_in_every_receipt() {
    isolated(
        "owned_payload::publication_contracts::nested_active_factory_retains_capture_failure_in_every_receipt",
        || active_factory(true),
    );
}
