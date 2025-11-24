use parrot::thread::{ActorProcessorManager, ProcessorInterface, SystemError};
use parrot_api::types::BoxedFuture;
use std::{
    any::Any,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

#[path = "support/manager_ownership.rs"]
#[allow(dead_code)]
mod support;

#[derive(Debug, Default)]
struct State {
    constructed: AtomicUsize,
    polled: AtomicUsize,
    completed: AtomicUsize,
    futures_dropped: AtomicUsize,
    processors_dropped: AtomicUsize,
    error_callbacks: AtomicUsize,
    errors_dropped: AtomicUsize,
    ready: AtomicBool,
    waker: Mutex<Option<std::task::Waker>>,
}

#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    None,
    Construct,
    Poll,
    Drop,
    PollAndDrop,
    RecursivePayload,
    ProcessorDrop,
    ErrorAndDrop,
    Error,
    HostileError,
}

struct RecursivePayload;
impl Drop for RecursivePayload {
    fn drop(&mut self) {
        panic!("recursive stop payload destructor");
    }
}

#[derive(Debug)]
struct HostileError(Arc<State>);
impl std::fmt::Display for HostileError {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.error_callbacks.fetch_add(1, Ordering::SeqCst);
        panic!("user error formatter must not run during shutdown");
    }
}
impl std::error::Error for HostileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.error_callbacks.fetch_add(1, Ordering::SeqCst);
        panic!("user error source must not run during shutdown");
    }
}
impl Drop for HostileError {
    fn drop(&mut self) {
        self.0.errors_dropped.fetch_add(1, Ordering::SeqCst);
        panic!("user error destructor failed");
    }
}

struct Probe {
    state: Arc<State>,
    fault: Fault,
    on_stop: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl Probe {
    fn new(ready: bool, fault: Fault) -> (Arc<Self>, Arc<State>) {
        let state = Arc::new(State::default());
        state.ready.store(ready, Ordering::SeqCst);
        (
            Arc::new(Self {
                state: state.clone(),
                fault,
                on_stop: Mutex::new(None),
            }),
            state,
        )
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.state.processors_dropped.fetch_add(1, Ordering::SeqCst);
        if matches!(self.fault, Fault::ProcessorDrop) {
            panic!("processor destructor failed");
        }
    }
}

struct Stop {
    state: Arc<State>,
    fault: Fault,
    _owner: Arc<Probe>,
    _pinned: std::marker::PhantomPinned,
}
impl Future for Stop {
    type Output = Result<(), SystemError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.state.polled.fetch_add(1, Ordering::SeqCst);
        match self.fault {
            Fault::Poll | Fault::PollAndDrop => panic!("stop poll failed"),
            Fault::RecursivePayload => std::panic::panic_any(RecursivePayload),
            _ => {}
        }
        if !self.state.ready.load(Ordering::SeqCst) {
            *self.state.waker.lock().unwrap() = Some(cx.waker().clone());
            return Poll::Pending;
        }
        if matches!(self.fault, Fault::Error | Fault::ErrorAndDrop) {
            return Poll::Ready(Err(SystemError::Timeout("original stop timeout".into())));
        }
        if matches!(self.fault, Fault::HostileError) {
            return Poll::Ready(Err(SystemError::Other(anyhow::Error::new(HostileError(
                self.state.clone(),
            )))));
        }
        self.state.completed.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(Ok(()))
    }
}
impl Drop for Stop {
    fn drop(&mut self) {
        self.state.futures_dropped.fetch_add(1, Ordering::SeqCst);
        if matches!(
            self.fault,
            Fault::Drop | Fault::PollAndDrop | Fault::ErrorAndDrop
        ) {
            panic!("stop future destructor failed");
        }
    }
}
impl parrot::thread::processor::ProcessorStatsTrait for Probe {
    fn get_statistics(&self) -> Option<Arc<parrot::thread::processor::ProcessorStats>> {
        None
    }
}
impl ProcessorInterface for Probe {
    fn is_initialized(&self) -> bool {
        true
    }
    fn initialize_and_start_erased(
        self: Arc<Self>,
    ) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
    fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
        self.state.constructed.fetch_add(1, Ordering::SeqCst);
        let callback = self.on_stop.lock().unwrap().take();
        if let Some(callback) = callback {
            callback();
        }
        if matches!(self.fault, Fault::Construct) {
            panic!("stop construction failed");
        }
        Box::pin(Stop {
            state: self.state.clone(),
            fault: self.fault,
            _owner: self,
            _pinned: std::marker::PhantomPinned,
        })
    }
    fn process_batch_erased(
        self: Arc<Self>,
        _: Arc<dyn parrot::thread::mailbox::Mailbox>,
        _: usize,
        _: bool,
    ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
        Box::pin(async { Ok((0, 0)) })
    }
    fn as_any(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(std::task::Waker::noop()))
}

#[test]
fn successful_shutdown_really_polls_and_completes_each_stop() {
    let manager = ActorProcessorManager::new();
    let mut states = Vec::new();
    for path in ["/one", "/two", "/three"] {
        let (probe, state) = Probe::new(true, Fault::None);
        manager.register(path, probe).unwrap();
        states.push(state);
    }
    futures::executor::block_on(manager.shutdown_all());
    assert!(manager.is_empty());
    for state in states {
        assert_eq!(state.constructed.load(Ordering::SeqCst), 1);
        assert_eq!(state.polled.load(Ordering::SeqCst), 1);
        assert_eq!(state.completed.load(Ordering::SeqCst), 1);
        assert_eq!(state.futures_dropped.load(Ordering::SeqCst), 1);
        assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn unpolled_shutdown_does_not_take_or_stop_any_processor() {
    let manager = ActorProcessorManager::new();
    let (probe, state) = Probe::new(true, Fault::None);
    manager.register("/one", probe).unwrap();
    drop(manager.shutdown_all());
    assert_eq!(state.constructed.load(Ordering::SeqCst), 0);
    assert_eq!(manager.len(), 1);
}

#[test]
fn cancelled_shutdown_retains_pending_processor_for_explicit_retry() {
    let manager = ActorProcessorManager::new();
    let (probe, state) = Probe::new(false, Fault::None);
    manager.register("/pending", probe).unwrap();
    let mut shutdown = Box::pin(manager.shutdown_all());
    assert!(poll_once(shutdown.as_mut()).is_pending());
    assert_eq!(state.polled.load(Ordering::SeqCst), 1);
    drop(shutdown);
    assert_eq!(state.futures_dropped.load(Ordering::SeqCst), 1);
    assert!(manager.get("/pending").is_some(), "pending owner was lost");
    assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 0);
    state.ready.store(true, Ordering::SeqCst);
    futures::executor::block_on(manager.shutdown_all());
    assert!(manager.is_empty());
    assert_eq!(state.constructed.load(Ordering::SeqCst), 2);
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn pending_stop_does_not_prevent_other_snapshot_members_from_stopping() {
    let manager = ActorProcessorManager::new();
    let (probe, pending) = Probe::new(false, Fault::None);
    manager.register("/pending", probe).unwrap();
    let mut healthy = Vec::new();
    for index in 0..8 {
        let (probe, state) = Probe::new(true, Fault::None);
        manager
            .register(&format!("/healthy-{index}"), probe)
            .unwrap();
        healthy.push(state);
    }
    let mut shutdown = Box::pin(manager.shutdown_all());
    assert!(poll_once(shutdown.as_mut()).is_pending());
    drop(shutdown);
    for state in healthy {
        assert_eq!(state.completed.load(Ordering::SeqCst), 1);
        assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 1);
    }
    assert_eq!(manager.len(), 1);
    assert_eq!(pending.completed.load(Ordering::SeqCst), 0);
}

fn isolates_failure(name: &str, fault: Fault) {
    support::isolated(name, || {
        let manager = ActorProcessorManager::new();
        let (bad, state) = Probe::new(true, fault);
        manager.register("/bad", bad).unwrap();
        let (healthy, healthy_state) = Probe::new(true, Fault::None);
        manager.register("/healthy", healthy).unwrap();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            futures::executor::block_on(manager.shutdown_all());
        }));
        assert!(
            unwound.is_ok(),
            "one stop failure escaped snapshot shutdown"
        );
        assert_eq!(healthy_state.completed.load(Ordering::SeqCst), 1);
        assert_eq!(healthy_state.processors_dropped.load(Ordering::SeqCst), 1);
        if matches!(fault, Fault::ProcessorDrop) {
            assert!(manager.is_empty());
            assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 1);
        } else {
            assert!(
                manager.get("/bad").is_some(),
                "failed stop must stay retryable"
            );
            manager.try_clear().unwrap();
        }
    });
}

#[test]
fn stop_construction_panic_is_isolated() {
    isolates_failure("stop_construction_panic_is_isolated", Fault::Construct);
}

#[test]
fn stop_poll_panic_is_isolated() {
    isolates_failure("stop_poll_panic_is_isolated", Fault::Poll);
}

#[test]
fn completed_stop_future_disposal_panic_is_isolated() {
    isolates_failure(
        "completed_stop_future_disposal_panic_is_isolated",
        Fault::Drop,
    );
}

#[test]
fn stop_poll_and_disposal_panics_do_not_abort_process() {
    isolates_failure(
        "stop_poll_and_disposal_panics_do_not_abort_process",
        Fault::PollAndDrop,
    );
}

#[test]
fn recursive_stop_panic_payload_does_not_abort_process() {
    isolates_failure(
        "recursive_stop_panic_payload_does_not_abort_process",
        Fault::RecursivePayload,
    );
}

#[test]
fn last_processor_destructor_panic_does_not_skip_other_stops() {
    isolates_failure(
        "last_processor_destructor_panic_does_not_skip_other_stops",
        Fault::ProcessorDrop,
    );
}

#[test]
fn cancellation_disposes_hostile_future_without_unwinding_or_losing_owner() {
    support::isolated(
        "cancellation_disposes_hostile_future_without_unwinding_or_losing_owner",
        || {
            let manager = ActorProcessorManager::new();
            let (probe, state) = Probe::new(false, Fault::Drop);
            manager.register("/pending", probe).unwrap();
            let mut shutdown = Box::pin(manager.shutdown_all());
            assert!(poll_once(shutdown.as_mut()).is_pending());
            assert!(!support::panics(|| drop(shutdown)));
            assert_eq!(state.futures_dropped.load(Ordering::SeqCst), 1);
            assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 0);
            assert!(manager.get("/pending").is_some());
            manager.try_clear().unwrap();
        },
    );
}

#[test]
fn stop_callback_can_replace_same_path_without_new_generation_being_removed() {
    support::isolated(
        "stop_callback_can_replace_same_path_without_new_generation_being_removed",
        || {
            let manager = Arc::new(ActorProcessorManager::new());
            let (old, old_state) = Probe::new(true, Fault::None);
            let (new, new_state) = Probe::new(true, Fault::None);
            let expected: Arc<dyn ProcessorInterface> = new.clone();
            let weak = Arc::downgrade(&manager);
            *old.on_stop.lock().unwrap() = Some(Box::new(move || {
                let manager = weak.upgrade().unwrap();
                drop(manager.remove("/same"));
                manager.register("/same", new).unwrap();
            }));
            manager.register("/same", old).unwrap();
            futures::executor::block_on(manager.shutdown_all());
            assert!(Arc::ptr_eq(&manager.get("/same").unwrap(), &expected));
            assert_eq!(old_state.completed.load(Ordering::SeqCst), 1);
            assert_eq!(old_state.processors_dropped.load(Ordering::SeqCst), 1);
            assert_eq!(new_state.constructed.load(Ordering::SeqCst), 0);
            manager.try_clear().unwrap();
        },
    );
}

#[test]
fn original_stop_error_and_future_disposal_failure_leave_retryable_owner() {
    isolates_failure(
        "original_stop_error_and_future_disposal_failure_leave_retryable_owner",
        Fault::ErrorAndDrop,
    );
}

#[test]
fn receipts_preserve_both_original_error_and_future_disposal_failure() {
    let manager = ActorProcessorManager::new();
    let (bad, state) = Probe::new(true, Fault::ErrorAndDrop);
    manager.register("/both", bad).unwrap();
    let error = futures::executor::block_on(manager.try_shutdown_all()).unwrap_err();
    let detail = error.to_string();
    assert!(detail.contains("/both (stop):"), "{detail}");
    assert!(detail.contains("original stop timeout"), "{detail}");
    assert!(detail.contains("/both (stop future disposal):"), "{detail}");
    assert!(detail.contains("stop future destructor failed"), "{detail}");
    let SystemError::Other(error) = error else {
        panic!("aggregate receipt must retain original causes");
    };
    assert!(error.chain().any(|cause| matches!(
        cause.downcast_ref::<SystemError>(),
        Some(SystemError::Timeout(message)) if message == "original stop timeout"
    )));
    assert_eq!(state.futures_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 0);
    manager.try_clear().unwrap();
}

#[test]
fn ordinary_stop_error_is_reported_and_does_not_remove_original() {
    let manager = ActorProcessorManager::new();
    let (bad, state) = Probe::new(true, Fault::Error);
    manager.register("/failed", bad).unwrap();
    let (healthy, healthy_state) = Probe::new(true, Fault::None);
    manager.register("/healthy", healthy).unwrap();
    let error = futures::executor::block_on(manager.try_shutdown_all()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("/failed (stop): Operation timed out:")
    );
    assert_eq!(state.completed.load(Ordering::SeqCst), 0);
    assert_eq!(healthy_state.completed.load(Ordering::SeqCst), 1);
    assert_eq!(manager.len(), 1);
    assert!(manager.get("/failed").is_some());
    manager.try_clear().unwrap();
}

#[test]
fn hostile_business_error_is_neither_formatted_nor_unsafely_dropped() {
    support::isolated(
        "hostile_business_error_is_neither_formatted_nor_unsafely_dropped",
        || {
            let manager = ActorProcessorManager::new();
            let (bad, state) = Probe::new(true, Fault::HostileError);
            manager.register("/business", bad).unwrap();
            let error = futures::executor::block_on(manager.try_shutdown_all()).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("original error retained in source chain")
            );
            assert_eq!(state.error_callbacks.load(Ordering::SeqCst), 0);
            assert_eq!(state.errors_dropped.load(Ordering::SeqCst), 0);
            assert!(!support::panics(|| drop(error)));
            assert_eq!(state.errors_dropped.load(Ordering::SeqCst), 1);
            assert_eq!(state.error_callbacks.load(Ordering::SeqCst), 0);
            manager.try_clear().unwrap();
        },
    );
}

#[test]
fn concurrent_shutdown_waiter_does_not_duplicate_or_cancel_active_stops() {
    let manager = ActorProcessorManager::new();
    let (pending, state) = Probe::new(false, Fault::None);
    manager.register("/pending", pending).unwrap();
    let mut first = Box::pin(manager.try_shutdown_all());
    let mut second = Box::pin(manager.try_shutdown_all());
    assert!(poll_once(first.as_mut()).is_pending());
    assert!(poll_once(second.as_mut()).is_pending());
    assert_eq!(state.constructed.load(Ordering::SeqCst), 1);
    drop(second);
    assert_eq!(state.futures_dropped.load(Ordering::SeqCst), 0);
    state.ready.store(true, Ordering::SeqCst);
    state.waker.lock().unwrap().take().unwrap().wake();
    assert!(matches!(poll_once(first.as_mut()), Poll::Ready(Ok(()))));
    drop(first);
    futures::executor::block_on(manager.try_shutdown_all()).unwrap();
    assert_eq!(state.constructed.load(Ordering::SeqCst), 1);
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
}

#[test]
fn cancelled_active_shutdown_releases_gate_for_waiting_caller() {
    let manager = ActorProcessorManager::new();
    let (pending, state) = Probe::new(false, Fault::None);
    manager.register("/pending", pending).unwrap();
    let mut first = Box::pin(manager.try_shutdown_all());
    let mut second = Box::pin(manager.try_shutdown_all());
    assert!(poll_once(first.as_mut()).is_pending());
    assert!(poll_once(second.as_mut()).is_pending());
    drop(first);
    state.ready.store(true, Ordering::SeqCst);
    assert!(matches!(poll_once(second.as_mut()), Poll::Ready(Ok(()))));
    assert_eq!(state.constructed.load(Ordering::SeqCst), 2);
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    assert!(manager.is_empty());
}

#[tokio::test(start_paused = true)]
async fn timeout_retains_pending_owner_and_does_not_spawn_background_stops() {
    let manager = ActorProcessorManager::new();
    let (pending, state) = Probe::new(false, Fault::None);
    manager.register("/pending", pending).unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            manager.try_shutdown_all()
        )
        .await
        .is_err()
    );
    assert_eq!(state.constructed.load(Ordering::SeqCst), 1);
    assert_eq!(state.futures_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(manager.len(), 1);
    state.ready.store(true, Ordering::SeqCst);
    state.waker.lock().unwrap().take().unwrap().wake();
    tokio::task::yield_now().await;
    assert_eq!(state.completed.load(Ordering::SeqCst), 0);
    manager.try_shutdown_all().await.unwrap();
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
}

#[test]
fn empty_shutdown_succeeds_and_external_handles_remain_owned_by_caller() {
    let manager = ActorProcessorManager::new();
    futures::executor::block_on(manager.try_shutdown_all()).unwrap();
    let (probe, state) = Probe::new(true, Fault::None);
    manager.register("/retained", probe.clone()).unwrap();
    futures::executor::block_on(manager.try_shutdown_all()).unwrap();
    assert!(manager.is_empty());
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&probe), 1);
    assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 0);
    drop(probe);
    assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn dropping_manager_isolates_each_remaining_processor_destructor() {
    support::isolated(
        "dropping_manager_isolates_each_remaining_processor_destructor",
        || {
            let manager = ActorProcessorManager::new();
            let mut states = Vec::new();
            for path in ["/unfinished-a", "/unfinished-b"] {
                let (probe, state) = Probe::new(false, Fault::ProcessorDrop);
                manager.register(path, probe).unwrap();
                states.push(state);
            }
            assert!(!support::panics(|| drop(manager)));
            for state in states {
                assert_eq!(state.constructed.load(Ordering::SeqCst), 0);
                assert_eq!(state.processors_dropped.load(Ordering::SeqCst), 1);
            }
        },
    );
}

#[test]
fn manager_preserves_send_sync_and_unwind_safety_traits() {
    fn require<T: Send + Sync + std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}
    require::<ActorProcessorManager>();
    let manager = ActorProcessorManager::new();
    assert!(std::panic::catch_unwind(|| manager.is_empty()).unwrap());
}

#[path = "support/manager_diagnostics.rs"]
mod diagnostics;
