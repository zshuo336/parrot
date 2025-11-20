use super::*;
use crate::owned_payload;
use std::{
    sync::{atomic::AtomicUsize, mpsc},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
}

struct Capture(Arc<AtomicUsize>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("pending factory capture failure");
    }
}

struct Release(Option<mpsc::Sender<()>>);
impl Drop for Release {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

fn fill_other_slots(slots: &Arc<Semaphore>) -> Vec<owned_payload::PendingCleanupPermit> {
    (0..owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT - 1)
        .map(|_| owned_payload::try_pending_cleanup_permit(slots).unwrap())
        .collect()
}

fn assert_retired<A: Send + 'static>(
    factory: &Factory<A>,
    receipt: BoxedFuture<'static, Result<(), String>>,
    slots: &Arc<Semaphore>,
    expected: &str,
) {
    let observer = runtime();
    observer.block_on(async {
        let error = tokio::time::timeout(WAIT, receipt)
            .await
            .expect("actual factory retirement did not finish")
            .unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert_eq!(error, factory.retire().await.unwrap_err());
        assert_eq!(error, factory.retire().await.unwrap_err());
        tokio::time::timeout(WAIT, async {
            loop {
                if let Some(returned) = owned_payload::try_pending_cleanup_permit(slots) {
                    drop(returned);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed factory retirement did not return pending capacity");
    });
}

#[test]
fn host_exit_retains_pending_budget_through_active_constructor() {
    crate::test_support::isolated(
        "constructor::retirement::pending_contracts::host_exit_retains_pending_budget_through_active_constructor",
        || {
            let host = runtime();
            let slots = Arc::new(Semaphore::new(1));
            let fillers = fill_other_slots(&slots);
            let drops = Arc::new(AtomicUsize::new(0));
            let capture = Capture(drops.clone());
            let (entered, observed) = tokio::sync::oneshot::channel();
            let entered = Mutex::new(Some(entered));
            let (release, held) = mpsc::channel();
            let release = Release(Some(release));
            let held = Mutex::new(held);
            let factory = Factory::new(
                move || {
                    std::hint::black_box(&capture);
                    entered.lock().unwrap().take().unwrap().send(()).unwrap();
                    held.lock().unwrap().recv_timeout(WAIT).unwrap();
                },
                Arc::new(Semaphore::new(1)),
                slots.clone(),
            );
            let control = Arc::new(ConstructionControl::default());
            let working = factory.clone();
            let worker_control = control.clone();
            let worker = host
                .spawn(async move { working.construct(worker_control, |_| Ok::<_, ()>(())).await });
            host.block_on(async {
                tokio::time::timeout(WAIT, observed).await.unwrap().unwrap();
            });
            let receipt = {
                let _entered = host.enter();
                factory.retire()
            };
            host.block_on(tokio::task::yield_now());
            assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
            control.cancel();
            host.shutdown_timeout(Duration::from_millis(20));
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert!(
                owned_payload::try_pending_cleanup_permit(&slots).is_none(),
                "host exit returned pending capacity while a constructor retained the factory capture"
            );
            drop(release);
            assert_retired(&factory, receipt, &slots, "pending factory capture failure");
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(worker);
            drop(fillers);
        },
    );
}

struct Output(Arc<AtomicUsize>);
impl Drop for Output {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("pending factory output failure");
    }
}

#[test]
fn host_exit_retains_pending_budget_through_prepared_output() {
    crate::test_support::isolated(
        "constructor::retirement::pending_contracts::host_exit_retains_pending_budget_through_prepared_output",
        || {
            let host = runtime();
            let slots = Arc::new(Semaphore::new(1));
            let fillers = fill_other_slots(&slots);
            let drops = Arc::new(AtomicUsize::new(0));
            let outputs = Arc::new(AtomicUsize::new(0));
            let capture = Capture(drops.clone());
            let observed = outputs.clone();
            let factory = Factory::new(
                move || {
                    std::hint::black_box(&capture);
                    Output(observed.clone())
                },
                Arc::new(Semaphore::new(1)),
                slots.clone(),
            );
            let prepared = host
                .block_on(factory.prepare::<()>(Arc::default()))
                .unwrap();
            let receipt = {
                let _entered = host.enter();
                factory.retire()
            };
            host.block_on(tokio::task::yield_now());
            assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
            host.shutdown_timeout(Duration::from_millis(20));
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert_eq!(outputs.load(Ordering::SeqCst), 0);
            assert!(
                owned_payload::try_pending_cleanup_permit(&slots).is_none(),
                "host exit returned pending capacity while a staged output retained the factory capture"
            );
            drop(prepared);
            assert_retired(&factory, receipt, &slots, "pending factory output failure");
            assert_eq!(outputs.load(Ordering::SeqCst), 1);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(fillers);
        },
    );
}

#[test]
fn host_exit_retains_pending_budget_during_prepared_discard_drop() {
    crate::test_support::isolated(
        "constructor::retirement::pending_contracts::host_exit_retains_pending_budget_during_prepared_discard_drop",
        || {
            #[derive(Default)]
            struct Gate {
                open: Mutex<bool>,
                changed: std::sync::Condvar,
            }
            struct Unblock(Arc<Gate>);
            impl Drop for Unblock {
                fn drop(&mut self) {
                    *self.0.open.lock().unwrap() = true;
                    self.0.changed.notify_all();
                }
            }
            struct BlockedOutput {
                gate: Arc<Gate>,
                entered: mpsc::Sender<()>,
                drops: Arc<AtomicUsize>,
            }
            impl Drop for BlockedOutput {
                fn drop(&mut self) {
                    self.entered.send(()).unwrap();
                    let (open, _) = self
                        .gate
                        .changed
                        .wait_timeout_while(self.gate.open.lock().unwrap(), WAIT, |open| !*open)
                        .unwrap();
                    assert!(*open, "test must release the discarded output destructor");
                    self.drops.fetch_add(1, Ordering::SeqCst);
                    panic!("blocked prepared output failure");
                }
            }

            let host = runtime();
            let slots = Arc::new(Semaphore::new(1));
            let fillers = fill_other_slots(&slots);
            let gate = Arc::new(Gate::default());
            let release = Unblock(gate.clone());
            let captures = Arc::new(AtomicUsize::new(0));
            let capture = Capture(captures.clone());
            let drops = Arc::new(AtomicUsize::new(0));
            let outputs = drops.clone();
            let (entered, observed) = mpsc::channel();
            let factory = Factory::new(
                move || {
                    std::hint::black_box(&capture);
                    BlockedOutput {
                        gate: gate.clone(),
                        entered: entered.clone(),
                        drops: outputs.clone(),
                    }
                },
                Arc::new(Semaphore::new(1)),
                slots.clone(),
            );
            let prepared = host
                .block_on(factory.prepare::<()>(Arc::default()))
                .unwrap();
            let active = slots.clone().try_acquire_owned().unwrap();
            let discarded = prepared.discard();
            host.block_on(tokio::task::yield_now());
            assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
            let exiting =
                std::thread::spawn(move || host.shutdown_timeout(Duration::from_millis(20)));
            observed.recv_timeout(WAIT).unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            let returned_early = owned_payload::try_pending_cleanup_permit(&slots).is_some();
            drop(release);
            exiting.join().unwrap();
            drop(active);
            let error = futures::executor::block_on(discarded).unwrap_err();
            assert!(error.contains("blocked prepared output failure"), "{error}");
            assert_retired(&factory, factory.retire(), &slots, &error);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(captures.load(Ordering::SeqCst), 1);
            assert!(
                !returned_early,
                "host shutdown returned pending capacity during actual prepared output Drop"
            );
            drop(fillers);
        },
    );
}
