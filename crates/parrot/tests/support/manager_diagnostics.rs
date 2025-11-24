use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

struct DiagnosticPayload(Arc<AtomicUsize>);
impl Drop for DiagnosticPayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("manager diagnostic payload destructor");
    }
}
struct Subscriber {
    hostile: Arc<AtomicBool>,
    events: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
}
impl tracing::Subscriber for Subscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata
            .target()
            .starts_with("parrot::thread::processor::manager")
            && *metadata.level() == tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        self.events.fetch_add(1, Ordering::SeqCst);
        if self.hostile.load(Ordering::SeqCst) {
            eprintln!("manager diagnostic hostile event entered");
            std::panic::panic_any(DiagnosticPayload(self.payloads.clone()));
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
struct OnDrop<F: FnOnce()>(Option<F>);
impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        self.0.take().unwrap()();
    }
}
fn protected(during_unwind: bool, cleanup: impl FnOnce()) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if during_unwind {
            let _guard = OnDrop(Some(cleanup));
            panic!("original manager caller unwind");
        } else {
            cleanup();
        }
    }));
    if during_unwind {
        let payload = result.expect_err("original unwind was swallowed");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"original manager caller unwind")
        );
    } else if let Err(payload) = result {
        std::mem::forget(payload);
        panic!("manager diagnostic escaped cleanup");
    }
}

#[derive(Clone, Copy)]
enum Boundary {
    Cancel,
    Advisory,
    Registry,
}
fn check(boundary: Boundary, during_unwind: bool) {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("manager diagnostic panic: {info}")
    }));
    let hostile = Arc::new(AtomicBool::new(false));
    let events = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(Subscriber {
        hostile: hostile.clone(),
        events: events.clone(),
        payloads: payloads.clone(),
    })
    .unwrap();
    for panics in [false, true] {
        hostile.store(panics, Ordering::SeqCst);
        events.store(0, Ordering::SeqCst);
        payloads.store(0, Ordering::SeqCst);
        let manager = ActorProcessorManager::new();
        match boundary {
            Boundary::Registry => {
                let counts = Arc::new(support::Counts::default());
                manager
                    .register(
                        "/bad",
                        Arc::new(support::Probe {
                            counts: counts.clone(),
                            on_drop: Some(Box::new(|| panic!("registry processor disposal"))),
                        }),
                    )
                    .unwrap();
                manager
                    .register("/healthy", support::Probe::plain(&counts))
                    .unwrap();
                protected(during_unwind, || drop(manager));
                assert_eq!(counts.drops.load(Ordering::SeqCst), 2);
                assert_eq!(counts.stops.load(Ordering::SeqCst), 0);
            }
            Boundary::Cancel => {
                let (bad, state) = Probe::new(false, Fault::Drop);
                let (healthy, normal) = Probe::new(false, Fault::None);
                manager.register("/bad", bad).unwrap();
                manager.register("/healthy", healthy).unwrap();
                let mut shutdown = Box::pin(manager.try_shutdown_all());
                assert!(poll_once(shutdown.as_mut()).is_pending());
                assert_eq!(state.polled.load(Ordering::SeqCst), 1);
                assert_eq!(normal.polled.load(Ordering::SeqCst), 1);
                protected(during_unwind, || drop(shutdown));
                assert_eq!(state.futures_dropped.load(Ordering::SeqCst), 1);
                assert_eq!(normal.futures_dropped.load(Ordering::SeqCst), 1);
                assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 0);
                assert_eq!(normal.processors_dropped.load(Ordering::SeqCst), 0);
                assert_eq!(manager.len(), 2);
                normal.ready.store(true, Ordering::SeqCst);
                state.ready.store(true, Ordering::SeqCst);
                assert!(futures::executor::block_on(manager.try_shutdown_all()).is_err());
                assert!(manager.get("/bad").is_some());
                assert!(manager.get("/healthy").is_none());
                manager.try_clear().unwrap();
                assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 1);
                assert_eq!(normal.processors_dropped.load(Ordering::SeqCst), 1);
            }
            Boundary::Advisory => {
                let (bad, state) = Probe::new(true, Fault::Error);
                let (healthy, normal) = Probe::new(true, Fault::None);
                manager.register("/bad", bad).unwrap();
                manager.register("/healthy", healthy).unwrap();
                protected(during_unwind, || {
                    futures::executor::block_on(manager.shutdown_all())
                });
                assert_eq!(normal.completed.load(Ordering::SeqCst), 1);
                assert_eq!(normal.processors_dropped.load(Ordering::SeqCst), 1);
                let error = futures::executor::block_on(manager.try_shutdown_all()).unwrap_err();
                assert!(error.to_string().contains("original stop timeout"));
                assert!(manager.get("/bad").is_some());
                manager.try_clear().unwrap();
                assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 1);
            }
        }
        assert_eq!(events.load(Ordering::SeqCst), 1);
        assert_eq!(payloads.load(Ordering::SeqCst), usize::from(panics));
        if !panics {
            eprintln!("manager diagnostic normal control complete");
        }
    }
}
macro_rules! contract {
    ($name:ident, $boundary:ident, $unwind:expr) => {
        #[test]
        fn $name() {
            support::isolated(concat!("diagnostics::", stringify!($name)), || {
                check(Boundary::$boundary, $unwind)
            });
        }
    };
}
contract!(
    cancelled_stop_preserves_cleanup_after_diagnostic_panic,
    Cancel,
    false
);
contract!(cancelled_stop_preserves_original_unwind, Cancel, true);
contract!(
    advisory_failure_preserves_retryable_receipt_after_diagnostic_panic,
    Advisory,
    false
);
contract!(advisory_failure_preserves_original_unwind, Advisory, true);
contract!(
    registry_drop_retires_all_handles_despite_diagnostic_panic,
    Registry,
    false
);
contract!(registry_drop_preserves_original_unwind, Registry, true);
