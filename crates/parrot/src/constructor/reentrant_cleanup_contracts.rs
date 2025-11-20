use super::*;
use crate::owned_payload::{MAX_PENDING_CLEANUPS_PER_SLOT, OwnedPayload};
use std::{
    sync::atomic::AtomicUsize,
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

fn isolated(name: &str, body: impl FnOnce()) {
    const FLAG: &str = "PARROT_REENTRANT_CLEANUP_CASE";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        body();
        return;
    }
    let full = format!("constructor::reentrant_cleanup_contracts::{name}");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &full, "--nocapture"])
        .env(FLAG, name)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: {status}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{name}: child failed to retire within the isolation deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Nested {
    runtime: tokio::runtime::Handle,
    slots: Arc<Semaphore>,
    count: Arc<AtomicUsize>,
    mode: u8,
    prepared: Option<super::prepared::Prepared<Counted>>,
}
impl Drop for Nested {
    fn drop(&mut self) {
        // The outer destructor owns the only blocking worker and cleanup slot.
        // Waiting for any queued nested job would therefore deadlock.
        let mut receipt: BoxedFuture<'static, Result<(), String>> = match self.mode {
            0 => {
                let input =
                    OwnedPayload::new(Counted(self.count.clone()), &self.runtime, &self.slots);
                let receipt = input.retire();
                Box::pin(async move { receipt.await.map_err(|e| e.to_string()) })
            }
            1 => {
                let capture = Counted(self.count.clone());
                let factory = Factory::new(
                    move || {
                        let _ = &capture;
                    },
                    Arc::new(Semaphore::new(1)),
                    self.slots.clone(),
                );
                factory.retire_on(Some(self.runtime.clone()))
            }
            2 => self.prepared.take().unwrap().discard(),
            _ => unreachable!(),
        };
        match receipt
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(result) => result.unwrap(),
            Poll::Pending => panic!("nested disposal queued behind its own outer cleanup owner"),
        }
    }
}

fn check(output: bool, mode: u8) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let slots = Arc::new(Semaphore::new(1));
    let count = Arc::new(AtomicUsize::new(0));
    let prepared = (mode == 2).then(|| {
        runtime.block_on(async {
            let count = count.clone();
            let factory = Factory::new(
                move || Counted(count.clone()),
                Arc::new(Semaphore::new(1)),
                slots.clone(),
            );
            factory.prepare::<()>(Arc::default()).await.unwrap()
        })
    });
    let nested = Nested {
        runtime: runtime.handle().clone(),
        slots: slots.clone(),
        count: count.clone(),
        mode,
        prepared,
    };
    runtime.block_on(async {
        if output {
            let nested = Mutex::new(Some(nested));
            let factory = Factory::new(
                move || nested.lock().unwrap().take().unwrap(),
                Arc::new(Semaphore::new(1)),
                slots.clone(),
            );
            let prepared = factory.prepare::<()>(Arc::default()).await.unwrap();
            prepared.discard().await.unwrap();
            factory.retire().await.unwrap();
        } else {
            let factory = Factory::new(
                move || {
                    let _ = &nested;
                },
                Arc::new(Semaphore::new(1)),
                slots.clone(),
            );
            factory.retire().await.unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let recovered = tokio::time::timeout(Duration::from_secs(3), slots.clone().acquire_owned())
            .await
            .expect("outer cleanup capacity must recover")
            .unwrap();
        drop(recovered);
        // Unrelated healthy work must still reach the sole blocking worker.
        assert_eq!(tokio::task::spawn_blocking(|| 73).await.unwrap(), 73);
        let exhausted = Arc::new(Semaphore::new(0));
        let worker_slots = exhausted.clone();
        let restored = Arc::new(AtomicUsize::new(0));
        let worker_count = restored.clone();
        let mut receipt = tokio::task::spawn_blocking(move || {
            OwnedPayload::new(
                Counted(worker_count),
                &tokio::runtime::Handle::current(),
                &worker_slots,
            )
            .retire()
        })
        .await
        .unwrap();
        assert!(futures::poll!(&mut receipt).is_pending());
        assert_eq!(restored.load(Ordering::SeqCst), 0, "restore worker scope");
        exhausted.add_permits(1);
        receipt.await.unwrap();
        assert_eq!(restored.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn capture_cleanup_reenters_payload_disposal_without_queueing() {
    isolated(
        "capture_cleanup_reenters_payload_disposal_without_queueing",
        || check(false, 0),
    );
}

#[test]
fn capture_cleanup_reenters_factory_disposal_without_queueing() {
    isolated(
        "capture_cleanup_reenters_factory_disposal_without_queueing",
        || check(false, 1),
    );
}

#[test]
fn output_cleanup_reenters_payload_disposal_without_queueing() {
    isolated(
        "output_cleanup_reenters_payload_disposal_without_queueing",
        || check(true, 0),
    );
}

#[test]
fn output_cleanup_reenters_factory_disposal_without_queueing() {
    isolated(
        "output_cleanup_reenters_factory_disposal_without_queueing",
        || check(true, 1),
    );
}

#[test]
fn capture_cleanup_reenters_prepared_discard_without_queueing() {
    isolated(
        "capture_cleanup_reenters_prepared_discard_without_queueing",
        || check(false, 2),
    );
}

#[test]
fn output_cleanup_reenters_prepared_discard_without_queueing() {
    isolated(
        "output_cleanup_reenters_prepared_discard_without_queueing",
        || check(true, 2),
    );
}

#[test]
fn cancelled_constructor_disposes_nested_output_inside_its_worker() {
    isolated(
        "cancelled_constructor_disposes_nested_output_inside_its_worker",
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .build()
                .unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let slots = Arc::new(Semaphore::new(1));
            let output = Mutex::new(Some(Nested {
                runtime: runtime.handle().clone(),
                slots: slots.clone(),
                count: count.clone(),
                mode: 0,
                prepared: None,
            }));
            let control = Arc::new(ConstructionControl::default());
            let cancelled = control.clone();
            let factory = Factory::new(
                move || {
                    cancelled.cancel();
                    output.lock().unwrap().take().unwrap()
                },
                Arc::new(Semaphore::new(1)),
                slots,
            );
            runtime.block_on(async {
                let result = factory.construct(control, |_| Ok::<_, ()>(())).await;
                assert!(
                    matches!(result, Err(ConstructionError::Cancelled)),
                    "{result:?}"
                );
                assert_eq!(count.load(Ordering::SeqCst), 1);
                factory.retire().await.unwrap();
            });
        },
    );
}

#[test]
fn cancelled_queued_publication_disposes_nested_output_inside_its_worker() {
    isolated(
        "cancelled_queued_publication_disposes_nested_output_inside_its_worker",
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .build()
                .unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let slots = Arc::new(Semaphore::new(1));
            let workers = Arc::new(Semaphore::new(1));
            let output = Mutex::new(Some(Nested {
                runtime: runtime.handle().clone(),
                slots: slots.clone(),
                count: count.clone(),
                mode: 0,
                prepared: None,
            }));
            let factory = Factory::new(
                move || output.lock().unwrap().take().unwrap(),
                workers.clone(),
                slots,
            );
            let control = Arc::new(ConstructionControl::default());
            runtime.block_on(async {
                let prepared = factory.prepare::<()>(control.clone()).await.unwrap();
                let (entered, started) = tokio::sync::oneshot::channel();
                let (release, held) = std::sync::mpsc::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    entered.send(()).unwrap();
                    // Disconnect also releases the gate if a test assertion unwinds.
                    let _ = held.recv_timeout(Duration::from_secs(10));
                });
                started.await.unwrap();
                let publishing = tokio::spawn(prepared.publish(|_| Ok::<_, ()>(())));
                tokio::time::timeout(Duration::from_secs(3), async {
                    while workers.available_permits() != 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                control.cancel();
                release.send(()).unwrap();
                blocker.await.unwrap();
                let result = publishing.await.unwrap();
                assert!(
                    matches!(result, Err(ConstructionError::Cancelled)),
                    "{result:?}"
                );
                assert_eq!(count.load(Ordering::SeqCst), 1);
                factory.retire().await.unwrap();
            });
        },
    );
}

#[test]
fn saturation_retains_queued_inputs_and_real_receipts_across_host_exit() {
    isolated(
        "saturation_retains_queued_inputs_and_real_receipts_across_host_exit",
        || {
            struct Gated {
                entered: Option<tokio::sync::oneshot::Sender<()>>,
                release: std::sync::mpsc::Receiver<()>,
                count: Arc<AtomicUsize>,
            }
            impl Drop for Gated {
                fn drop(&mut self) {
                    self.entered.take().unwrap().send(()).unwrap();
                    self.release
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release the real destructor gate");
                    self.count.fetch_add(1, Ordering::SeqCst);
                    panic!("gated cleanup failure");
                }
            }
            struct Retained {
                count: Arc<AtomicUsize>,
                hostile: bool,
            }
            impl Drop for Retained {
                fn drop(&mut self) {
                    self.count.fetch_add(1, Ordering::SeqCst);
                    assert!(!self.hostile, "queued cleanup failure");
                }
            }
            struct Release(Option<std::sync::mpsc::Sender<()>>);
            impl Drop for Release {
                fn drop(&mut self) {
                    if let Some(sender) = self.0.take() {
                        let _ = sender.send(());
                    }
                }
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(2)
                .build()
                .unwrap();
            let slots = Arc::new(Semaphore::new(1));
            let count = Arc::new(AtomicUsize::new(0));
            let (entered, started) = tokio::sync::oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            let release = Release(Some(release));
            let first = OwnedPayload::new(
                Gated {
                    entered: Some(entered),
                    release: held,
                    count: count.clone(),
                },
                runtime.handle(),
                &slots,
            );
            let mut first_receipt = first.receipt();
            drop(first.retire());
            let mut retained = Vec::new();
            let mut observers = Vec::new();
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(3), started)
                    .await
                    .unwrap()
                    .unwrap();
                // Two bursts distinguish queued ownership from active worker capacity.
                for burst in 0..2 {
                    for index in 0..16 {
                        let hostile = index % 7 == 0;
                        let value = Arc::new(Retained {
                            count: count.clone(),
                            hostile,
                        });
                        retained.push(Arc::downgrade(&value));
                        let input = OwnedPayload::new(value, runtime.handle(), &slots);
                        observers.push((input.receipt(), hostile));
                        // An unpolled and cancelled waiter does not own execution.
                        drop(input.retire());
                    }
                    tokio::task::yield_now().await;
                    tokio::task::yield_now().await;
                    let pending = 16 * (burst + 1);
                    let queued_capacity = MAX_PENDING_CLEANUPS_PER_SLOT.saturating_sub(1);
                    let queued = pending.min(queued_capacity);
                    let overflow = pending - queued;
                    assert_eq!(
                        retained
                            .iter()
                            .filter(|owner| owner.strong_count() == 1)
                            .count(),
                        queued
                    );
                    assert!(runtime.metrics().num_alive_tasks() > queued);
                    assert_eq!(count.load(Ordering::SeqCst), overflow);
                    assert_eq!(slots.available_permits(), 0);
                    assert!(futures::poll!(&mut first_receipt).is_pending());
                    println!(
                        "PARROT_CLEANUP_SATURATION pending={pending} queued={queued} overflow={overflow} active=1"
                    );
                }
                let healthy_count = Arc::new(AtomicUsize::new(0));
                tokio::time::timeout(
                    Duration::from_secs(3),
                    OwnedPayload::new(
                        Counted(healthy_count.clone()),
                        runtime.handle(),
                        &Arc::new(Semaphore::new(1)),
                    )
                    .retire(),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(healthy_count.load(Ordering::SeqCst), 1);
            });
            runtime.shutdown_timeout(Duration::from_millis(20));
            assert_eq!(count.load(Ordering::SeqCst), 32);
            assert!(retained.iter().all(|owner| owner.strong_count() == 0));
            assert_eq!(
                slots.available_permits(),
                0,
                "active disposal still owns admission"
            );
            assert!(
                futures::executor::block_on(async { futures::poll!(&mut first_receipt) })
                    .is_pending()
            );
            for (receipt, hostile) in observers {
                let result = futures::executor::block_on(receipt);
                if hostile {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("queued cleanup failure")
                    );
                } else {
                    result.unwrap();
                }
            }
            drop(release);
            let error = futures::executor::block_on(first_receipt).unwrap_err();
            assert!(error.to_string().contains("gated cleanup failure"));
            assert_eq!(count.load(Ordering::SeqCst), 33);
            // Receipt publication precedes release of the surrounding worker permit.
            let deadline = Instant::now() + Duration::from_secs(3);
            while slots.available_permits() != 1 {
                assert!(Instant::now() < deadline, "cleanup permit did not recover");
                std::thread::yield_now();
            }
        },
    );
}
