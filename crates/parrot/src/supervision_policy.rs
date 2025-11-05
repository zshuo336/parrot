//! Owned user policy state. Callback and final capture disposal share bounded workers.
use parrot_api::{errors::ActorError, supervisor::SupervisorStrategyType, types::ActorResult};
use std::sync::{Arc, Mutex};
use tokio::sync::{Semaphore, watch};

fn retire_strategy(strategy: Option<SupervisorStrategyType>) -> Result<(), String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(strategy))).map_err(|panic| {
        let message = crate::owned_payload::panic_message(panic);
        format!("supervision policy disposal failed: destructor panicked: {message}")
    })
}

// Own the capture before submitting any task. Host cancellation can drop an
// unpolled or permit-waiting task; that is still a real disposal, not a lost
// sender or permission to unwind through another owner's destructor.
struct Disposal {
    strategy: Option<SupervisorStrategyType>,
    finished: watch::Sender<Option<Result<(), String>>>,
}
impl Drop for Disposal {
    fn drop(&mut self) {
        self.finished
            .send_replace(Some(retire_strategy(self.strategy.take())));
    }
}

struct State {
    strategy: Option<SupervisorStrategyType>,
    closed: bool,
    disposal: Option<watch::Receiver<Option<Result<(), String>>>>,
}
pub(crate) struct PolicyStore {
    state: Mutex<State>,
    pub(crate) slots: Arc<Semaphore>,
}
impl Drop for PolicyStore {
    fn drop(&mut self) {
        // A destroyed host cannot execute asynchronous drain. Final synchronous
        // fallback must nevertheless isolate this policy from sibling captures.
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        let _ = retire_strategy(state.strategy.take());
    }
}
impl PolicyStore {
    pub(crate) fn new(strategy: Option<SupervisorStrategyType>, slots: Arc<Semaphore>) -> Self {
        Self {
            state: Mutex::new(State {
                strategy,
                closed: false,
                disposal: None,
            }),
            slots,
        }
    }
    pub(crate) fn set(&self, strategy: Option<SupervisorStrategyType>) {
        let old = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                strategy
            } else {
                std::mem::replace(&mut state.strategy, strategy)
            }
        };
        // Setters execute on their caller, as does destruction of an unaccepted
        // policy. Never run arbitrary captured Drop while holding our lock.
        drop(old);
    }
    pub(crate) fn get(&self) -> Option<SupervisorStrategyType> {
        self.state.lock().unwrap().strategy.clone()
    }
    pub(crate) async fn dispose(&self) -> ActorResult<()> {
        let mut receipt = {
            let mut state = self.state.lock().unwrap();
            if let Some(receipt) = &state.disposal {
                receipt.clone()
            } else {
                state.closed = true;
                let strategy = state.strategy.take();
                let (sender, receipt) = watch::channel(None);
                state.disposal = Some(receipt.clone());
                let slots = self.slots.clone();
                if !matches!(
                    &strategy,
                    Some(
                        SupervisorStrategyType::OneForOne(_) | SupervisorStrategyType::OneForAll(_)
                    )
                ) {
                    drop(strategy);
                    sender.send_replace(Some(Ok(())));
                } else {
                    let disposal = Disposal {
                        strategy,
                        finished: sender,
                    };
                    tokio::spawn(async move {
                        let permit = slots.acquire_owned().await;
                        let _ = tokio::task::spawn_blocking(move || {
                            let _permit = permit;
                            drop(disposal);
                        })
                        .await;
                    });
                }
                receipt
            }
        };
        loop {
            if let Some(result) = receipt.borrow_and_update().clone() {
                return result.map_err(ActorError::InternalError);
            }
            receipt.changed().await.map_err(|_| {
                ActorError::InternalError("supervision policy owner abandoned disposal".into())
            })?;
        }
    }
}

/// Preserve concrete API error variants when sharing a failure with supervision.
/// For arbitrary anyhow errors, retain the original source through an Arc.
pub(crate) fn reply_error(error: Arc<ActorError>) -> ActorError {
    match Arc::try_unwrap(error) {
        Ok(error) => error,
        Err(error) => match error.as_ref() {
            ActorError::InitializationError(s) => ActorError::InitializationError(s.clone()),
            ActorError::MessageHandlingError(s) => ActorError::MessageHandlingError(s.clone()),
            ActorError::Stopped => ActorError::Stopped,
            ActorError::Timeout => ActorError::Timeout,
            ActorError::TimeoutDetail(s) => ActorError::TimeoutDetail(s.clone()),
            ActorError::ActorNotFound(s) => ActorError::ActorNotFound(s.clone()),
            ActorError::InternalError(s) => ActorError::InternalError(s.clone()),
            ActorError::ProcessMessageError(s) => ActorError::ProcessMessageError(s.clone()),
            ActorError::ReplyChannelError(s) => ActorError::ReplyChannelError(s.clone()),
            ActorError::Panic(s) => ActorError::Panic(s.clone()),
            ActorError::Other(_) => ActorError::Other(anyhow::Error::new(error)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_api::supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision};
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    struct Capture {
        drops: Arc<AtomicUsize>,
        thread: Arc<Mutex<Option<std::thread::ThreadId>>>,
        panics: bool,
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            *self.thread.lock().unwrap() = Some(std::thread::current().id());
            assert!(!self.panics, "policy capture destructor");
        }
    }
    fn strategy(capture: Capture) -> SupervisorStrategyType {
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 0,
            within: Duration::from_secs(1),
            decider: BasicDecisionFn::new(move |_| {
                let _ = &capture;
                SupervisionDecision::Stop
            }),
        })
    }

    #[test]
    fn host_cancelled_policy_disposal_preserves_actual_receipt() {
        for wait_for_permit in [false, true] {
            for panics in [false, true] {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let drops = Arc::new(AtomicUsize::new(0));
                let thread = Arc::new(Mutex::new(None));
                let store = PolicyStore::new(
                    Some(strategy(Capture {
                        drops: drops.clone(),
                        thread: thread.clone(),
                        panics,
                    })),
                    Arc::new(Semaphore::new(0)),
                );
                runtime.block_on(async {
                    let mut waiter = Box::pin(store.dispose());
                    assert!(futures::poll!(&mut waiter).is_pending());
                    drop(waiter);
                    if wait_for_permit {
                        tokio::task::yield_now().await;
                    }
                    assert_eq!(drops.load(Ordering::SeqCst), 0);
                });
                runtime.shutdown_timeout(Duration::from_secs(1));
                let receipt = store.state.lock().unwrap().disposal.clone().unwrap();
                let result = receipt.borrow().clone().expect("real disposal receipt");
                assert_eq!(result.is_err(), panics);
                if let Err(error) = result {
                    assert!(
                        error.contains("policy disposal failed: destructor panicked"),
                        "{error}"
                    );
                }
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert_eq!(*thread.lock().unwrap(), Some(std::thread::current().id()));
                drop(store);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            }
        }
    }

    #[tokio::test]
    async fn cancelled_policy_waiter_keeps_bounded_cleanup_and_shared_receipt() {
        for panics in [false, true] {
            let drops = Arc::new(AtomicUsize::new(0));
            let thread = Arc::new(Mutex::new(None));
            let slots = Arc::new(Semaphore::new(1));
            let permit = slots.clone().acquire_owned().await.unwrap();
            let store = PolicyStore::new(
                Some(strategy(Capture {
                    drops: drops.clone(),
                    thread: thread.clone(),
                    panics,
                })),
                slots.clone(),
            );
            let mut waiter = Box::pin(store.dispose());
            assert!(futures::poll!(&mut waiter).is_pending());
            drop(waiter);
            tokio::task::yield_now().await;
            assert_eq!(drops.load(Ordering::SeqCst), 0, "cleanup needs its permit");
            drop(permit);
            let first = tokio::time::timeout(Duration::from_secs(1), store.dispose())
                .await
                .unwrap();
            let repeated = store.dispose().await;
            assert_eq!(first.is_err(), panics);
            assert_eq!(first.is_err(), repeated.is_err());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_ne!(*thread.lock().unwrap(), Some(std::thread::current().id()));
            let restored = tokio::time::timeout(Duration::from_secs(1), slots.acquire_owned())
                .await
                .unwrap()
                .unwrap();
            drop(restored);
        }
    }

    struct RecursivePolicyPayload;
    impl Drop for RecursivePolicyPayload {
        fn drop(&mut self) {
            panic!("secondary policy panic payload destructor");
        }
    }
    struct RecursiveCapture(Arc<AtomicUsize>);
    impl Drop for RecursiveCapture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::panic::panic_any(RecursivePolicyPayload);
        }
    }
    fn recursive_strategy(drops: &Arc<AtomicUsize>) -> SupervisorStrategyType {
        let capture = RecursiveCapture(drops.clone());
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 0,
            within: Duration::from_secs(1),
            decider: BasicDecisionFn::new(move |_| {
                let _ = &capture;
                SupervisionDecision::Stop
            }),
        })
    }
    fn assert_recursive_policy_receipt(error: &str) {
        assert!(
            error.contains("supervision policy disposal failed: destructor panicked"),
            "actual capture disposal was replaced by a generic owner error: {error}"
        );
        assert!(
            error.contains("panic payload destructor panicked"),
            "recursive payload disposal was not retained: {error}"
        );
    }
    #[tokio::test]
    async fn recursive_capture_disposal_has_repeatable_receipts_after_cancellation() {
        let drops = Arc::new(AtomicUsize::new(0));
        let store = PolicyStore::new(
            Some(recursive_strategy(&drops)),
            Arc::new(Semaphore::new(1)),
        );
        let mut cancelled = Box::pin(store.dispose());
        assert!(futures::poll!(&mut cancelled).is_pending());
        drop(cancelled);
        let (first, second) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(store.dispose(), store.dispose())
        })
        .await
        .expect("capture retirement must not strand waiters");
        for result in [first, second] {
            match result {
                Err(ActorError::InternalError(message)) => {
                    assert_recursive_policy_receipt(&message)
                }
                result => panic!("wrong policy disposal result: {result:?}"),
            }
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(store);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn recursive_capture_queued_disposal_survives_runtime_shutdown() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let store = PolicyStore::new(
            Some(recursive_strategy(&drops)),
            Arc::new(Semaphore::new(0)),
        );
        runtime.block_on(async {
            let mut cancelled = Box::pin(store.dispose());
            assert!(futures::poll!(&mut cancelled).is_pending());
            drop(cancelled);
            tokio::task::yield_now().await;
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        });
        runtime.shutdown_timeout(Duration::from_secs(1));
        let receipt = store.state.lock().unwrap().disposal.clone().unwrap();
        let result = receipt
            .borrow()
            .clone()
            .expect("host cancellation must leave a real disposal receipt");
        assert_recursive_policy_receipt(&result.unwrap_err());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn recursive_capture_final_owner_drop_is_isolated_without_runtime() {
        let drops = Arc::new(AtomicUsize::new(0));
        let store = PolicyStore::new(
            Some(recursive_strategy(&drops)),
            Arc::new(Semaphore::new(1)),
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)));
        assert!(
            result.is_ok(),
            "final policy owner leaked a secondary panic to its caller"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
