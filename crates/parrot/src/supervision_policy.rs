//! Owned user policy state. Callback and final capture disposal share bounded workers.
#[cfg(test)]
mod receipt_contracts;
#[cfg(test)]
mod trace_contracts;
use parrot_api::{
    errors::ActorError,
    supervisor::{DecisionFn, DefaultStrategy, SupervisionDecision, SupervisorStrategyType},
    types::ActorResult,
};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};

#[derive(Clone)]
pub(crate) struct OwnedStrategy {
    // A decision can outlive policy replacement and become the final capture owner.
    strategy: SupervisorStrategyType,
    context: Option<parrot_api::trace::TraceContext>,
}
impl OwnedStrategy {
    fn new(strategy: SupervisorStrategyType) -> Self {
        Self {
            strategy,
            context: parrot_api::trace::TraceContext::current(),
        }
    }
}
impl Default for OwnedStrategy {
    fn default() -> Self {
        Self::new(SupervisorStrategyType::default())
    }
}

#[derive(Default)]
struct DecisionState {
    active: usize,
    failure: Option<String>,
}
#[derive(Default)]
struct DecisionWork {
    state: Mutex<DecisionState>,
    changed: tokio::sync::Notify,
}
#[derive(Default)]
pub(crate) struct DecisionTracker {
    closed: AtomicBool,
    work: OnceLock<Arc<DecisionWork>>,
}
impl DecisionTracker {
    pub(crate) fn snapshot(
        &self,
        slots: Arc<Semaphore>,
        select: impl FnOnce() -> Option<OwnedStrategy>,
    ) -> ActorResult<Option<DecisionSnapshot>> {
        let work = self.work.get_or_init(Default::default);
        let mut state = work.state.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {
            return Err(ActorError::Stopped);
        }
        if state.active == usize::MAX {
            return Err(ActorError::InternalError(
                "supervision snapshot capacity exhausted".into(),
            ));
        }
        // Selection only clones framework-owned enum/Arc storage. User code and
        // capture disposal run after releasing both this gate and policy locks.
        let Some(strategy) = select() else {
            return Ok(None);
        };
        state.active += 1;
        Ok(Some(DecisionSnapshot {
            retirement: Some(SnapshotRetirement {
                strategy: Some(strategy),
                failure: None,
                work: work.clone(),
            }),
            slots,
        }))
    }
    pub(crate) fn close(
        &self,
    ) -> impl std::future::Future<Output = ActorResult<()>> + Send + 'static {
        self.closed.store(true, Ordering::Release);
        let work = self.work.get().cloned();
        async move {
            let Some(work) = work else {
                return Ok(());
            };
            loop {
                let changed = work.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let state = work.state.lock().unwrap();
                    if state.active == 0 {
                        return state
                            .failure
                            .clone()
                            .map_or(Ok(()), |error| Err(ActorError::InternalError(error)));
                    }
                }
                changed.await;
            }
        }
    }
}
struct SnapshotRetirement {
    strategy: Option<OwnedStrategy>,
    failure: Option<String>,
    work: Arc<DecisionWork>,
}
impl SnapshotRetirement {
    fn retire(&mut self) -> Result<(), String> {
        let result = retire_strategy(self.strategy.take());
        if let Err(error) = &result {
            self.failure.get_or_insert_with(|| error.clone());
        }
        result
    }
}
impl Drop for SnapshotRetirement {
    fn drop(&mut self) {
        let _ = self.retire();
        {
            let mut state = self.work.state.lock().unwrap();
            if let Some(error) = self.failure.take() {
                state.failure.get_or_insert(error);
            }
            state.active -= 1;
        }
        self.work.changed.notify_waiters();
    }
}
pub(crate) struct DecisionSnapshot {
    retirement: Option<SnapshotRetirement>,
    slots: Arc<Semaphore>,
}
impl DecisionSnapshot {
    pub(crate) fn strategy(&self) -> &SupervisorStrategyType {
        &self
            .retirement
            .as_ref()
            .unwrap()
            .strategy
            .as_ref()
            .unwrap()
            .strategy
    }
    fn retire(&mut self) -> Result<(), String> {
        self.retirement.as_mut().unwrap().retire()
    }
    pub(crate) fn reject(mut self) -> ActorResult<()> {
        self.retire().map_err(ActorError::InternalError)
    }
}
impl Drop for DecisionSnapshot {
    fn drop(&mut self) {
        let retirement = self.retirement.take().unwrap();
        if matches!(
            retirement.strategy.as_ref().map(|policy| &policy.strategy),
            None | Some(SupervisorStrategyType::Default(_))
        ) {
            drop(retirement);
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            // A cancelled decision can be one of many concurrent captures.
            // Route it through the same bounded owner accounting as every
            // other user payload instead of creating an unbounded waiter and
            // blocking-pool job for each snapshot.
            drop(crate::owned_payload::OwnedPayload::new(
                retirement,
                &runtime,
                &self.slots,
            ));
        } else {
            drop(retirement);
        }
    }
}

fn drop_strategy(strategy: Option<OwnedStrategy>) {
    if let Some(policy) = strategy {
        parrot_api::trace::in_scope(policy.context, || drop(policy.strategy));
    }
}

fn retire_strategy(strategy: Option<OwnedStrategy>) -> Result<(), String> {
    let Some(policy) = strategy else {
        return Ok(());
    };
    let _scope = crate::owned_payload::DisposalScope::enter();
    parrot_api::trace::in_scope(policy.context, || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(policy.strategy))).map_err(
            |panic| {
                let message = crate::owned_payload::panic_message(panic);
                format!("supervision policy disposal failed: destructor panicked: {message}")
            },
        )
    })
}

pub(crate) fn decide(
    mut strategy: DecisionSnapshot,
    error: &ActorError,
) -> ActorResult<SupervisionDecision> {
    // A concurrent setter can leave this snapshot as the final capture owner.
    // Catch the callback before disposing it, so both failures remain observable.
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match strategy.strategy() {
            SupervisorStrategyType::Default(DefaultStrategy::StopOnFailure) => {
                SupervisionDecision::Stop
            }
            SupervisorStrategyType::Default(DefaultStrategy::RestartOnFailure) => {
                SupervisionDecision::Restart
            }
            SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure) => {
                SupervisionDecision::Resume
            }
            SupervisorStrategyType::Default(DefaultStrategy::EscalateFailure) => {
                SupervisionDecision::Escalate
            }
            SupervisorStrategyType::OneForOne(strategy) => strategy.decider.decide(error),
            SupervisorStrategyType::OneForAll(strategy) => strategy.decider.decide(error),
        }));
    let disposal = strategy.retire();
    let result = result.map_err(|payload| {
        format!(
            "supervision decider failed: {}",
            crate::owned_payload::panic_message(payload)
        )
    });
    let result = match (result, disposal) {
        (Ok(decision), Ok(())) => Ok(decision),
        (Err(callback), Ok(())) => Err(callback),
        (Ok(_), Err(disposal)) => Err(disposal),
        (Err(callback), Err(disposal)) => Err(format!("{callback}; {disposal}")),
    };
    if let Err(error) = &result {
        // The waiter may already be cancelled. Retirement still owns both
        // callback and capture failures, including their combined diagnostic.
        strategy.retirement.as_mut().unwrap().failure = Some(error.clone());
    }
    result.map_err(ActorError::InternalError)
}

// Own the capture before submitting any task. Host cancellation can drop an
// unpolled or permit-waiting task; that is still a real disposal, not a lost
// sender or permission to unwind through another owner's destructor.
struct Disposal {
    strategy: Option<OwnedStrategy>,
    finished: watch::Sender<Option<Result<(), String>>>,
    // Keep quotas through actual capture disposal, including host cancellation.
    pending: Option<crate::owned_payload::PendingCleanupPermit>,
    active: Option<OwnedSemaphorePermit>,
}
impl Drop for Disposal {
    fn drop(&mut self) {
        let result = retire_strategy(self.strategy.take());
        drop(self.active.take());
        drop(self.pending.take());
        crate::owned_payload::publish_watch(&self.finished, result);
    }
}

struct State {
    strategy: Option<OwnedStrategy>,
    closed: bool,
    disposal: Option<watch::Receiver<Option<Result<(), String>>>>,
}
pub(crate) struct PolicyStore {
    state: Mutex<State>,
    changed: Notify,
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
                strategy: strategy.map(OwnedStrategy::new),
                closed: false,
                disposal: None,
            }),
            changed: Notify::new(),
            slots,
        }
    }
    pub(crate) fn set(&self, strategy: Option<SupervisorStrategyType>) {
        let strategy = strategy.map(OwnedStrategy::new);
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
        drop_strategy(old);
    }
    pub(crate) fn get(&self) -> Option<OwnedStrategy> {
        let state = self.state.lock().unwrap();
        if state.closed {
            None
        } else {
            state.strategy.clone()
        }
    }
    pub(crate) async fn dispose(&self) -> ActorResult<()> {
        let mut pending = None;
        let (mut receipt, disposal) = loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let claimed = {
                let mut state = self.state.lock().unwrap();
                state.closed = true;
                if let Some(receipt) = &state.disposal {
                    Some((receipt.clone(), None))
                } else {
                    let needs_worker = matches!(
                        state.strategy.as_ref().map(|policy| &policy.strategy),
                        Some(
                            SupervisorStrategyType::OneForOne(_)
                                | SupervisorStrategyType::OneForAll(_)
                        )
                    ) && !crate::owned_payload::DisposalScope::active();
                    if needs_worker && pending.is_none() {
                        None
                    } else {
                        let strategy = state.strategy.take();
                        let (sender, receipt) = watch::channel(None);
                        state.disposal = Some(receipt.clone());
                        Some((
                            receipt,
                            Some(Disposal {
                                strategy,
                                finished: sender,
                                pending: None,
                                active: None,
                            }),
                        ))
                    }
                }
            };
            if let Some(claimed) = claimed {
                break claimed;
            }
            // Retain the sealed capture in this store while waiting. No new
            // task or user Drop may stall the shared lifecycle driver on overload.
            tokio::select! {
                permit = crate::owned_payload::pending_cleanup_permit(&self.slots) => {
                    pending = Some(permit);
                }
                _ = changed => {}
            }
        };
        self.changed.notify_waiters();
        if let Some(disposal) = disposal {
            let slots = self.slots.clone();
            if let Some(pending) = pending.take() {
                // Retain a finite owner before waiting for an active
                // cleanup worker. If the runtime cannot accept this
                // task, dropping the owner publishes the real receipt
                // synchronously rather than losing the policy.
                let mut disposal = Some(disposal);
                disposal.as_mut().expect("policy disposal owner").pending = Some(pending);
                let dispatched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut disposal = disposal.take().expect("policy disposal owner");
                    tokio::spawn(async move {
                        disposal.active = slots.acquire_owned().await.ok();
                        let _ = tokio::task::spawn_blocking(move || {
                            drop(disposal);
                        })
                        .await;
                    });
                }));
                if dispatched.is_err() {
                    drop(disposal.take());
                }
            } else {
                // Default policies have no user captures. Reentrant disposal
                // runs inside its existing worker instead of waiting for itself.
                drop(disposal);
            }
        }
        drop(pending);
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
pub(crate) fn reply_error(error: impl Into<crate::error_receipt::ActorReceipt>) -> ActorError {
    let error = error.into();
    // Stop receipts enrich Timeout, but message replies retain their original class.
    if matches!(error.error().as_ref(), ActorError::Timeout) {
        ActorError::Timeout
    } else {
        error.into_error()
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

    #[tokio::test]
    async fn saturated_policy_disposal_releases_state_lock_before_capture_drop() {
        struct Reenter {
            owner: std::sync::Weak<PolicyStore>,
            observed: Arc<AtomicUsize>,
            panics: bool,
        }
        impl Drop for Reenter {
            fn drop(&mut self) {
                let store = self.owner.upgrade().expect("store survives disposal");
                // Detect the deadlock boundary without hanging the test process.
                let state = store.state.try_lock().ok().map(|state| {
                    assert!(state.closed);
                    assert!(state.strategy.is_none());
                    assert!(state.disposal.is_some());
                });
                self.observed
                    .store(if state.is_some() { 1 } else { 2 }, Ordering::SeqCst);
                assert!(!self.panics, "reentrant policy capture failure");
            }
        }
        for panics in [false, true] {
            let slots = Arc::new(Semaphore::new(1));
            let store = Arc::new(PolicyStore::new(None, slots.clone()));
            let observed = Arc::new(AtomicUsize::new(0));
            let capture = Reenter {
                owner: Arc::downgrade(&store),
                observed: observed.clone(),
                panics,
            };
            store.set(Some(SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 0,
                within: Duration::from_secs(1),
                decider: BasicDecisionFn::new(move |_| {
                    let _ = &capture;
                    SupervisionDecision::Stop
                }),
            })));
            let mut pending: Vec<_> = (0..crate::owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT)
                .map(|_| crate::owned_payload::try_pending_cleanup_permit(&slots).unwrap())
                .collect();
            assert!(crate::owned_payload::try_pending_cleanup_permit(&slots).is_none());
            let mut cancelled = Box::pin(store.dispose());
            assert!(futures::poll!(&mut cancelled).is_pending());
            assert!(
                store.get().is_none(),
                "sealed policy cannot issue snapshots"
            );
            {
                let state = store.state.lock().unwrap();
                assert!(state.closed);
                assert!(state.strategy.is_some(), "unadmitted capture stays owned");
                assert!(state.disposal.is_none());
            }
            assert_eq!(observed.load(Ordering::SeqCst), 0);
            drop(cancelled);
            let mut first = Box::pin(store.dispose());
            let mut second = Box::pin(store.dispose());
            assert!(futures::poll!(&mut first).is_pending());
            assert!(futures::poll!(&mut second).is_pending());
            drop(pending.pop());
            let (result, repeated) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(first, second)
            })
            .await
            .expect("one returned slot must complete all policy observers");
            assert_eq!(result.is_err(), panics);
            assert_eq!(repeated.is_err(), panics);
            if panics {
                let error = result.unwrap_err().to_string();
                assert!(error.contains("reentrant policy capture failure"));
                assert_eq!(error, repeated.unwrap_err().to_string());
                assert_eq!(error, store.dispose().await.unwrap_err().to_string());
            }
            drop(pending);
            assert_eq!(
                observed.load(Ordering::SeqCst),
                1,
                "capture Drop ran under the policy lock"
            );
        }
    }

    #[test]
    fn cancelled_policy_admission_keeps_capture_for_retry_or_final_owner() {
        for retry in [false, true] {
            for panics in [false, true] {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let drops = Arc::new(AtomicUsize::new(0));
                let thread = Arc::new(Mutex::new(None));
                let slots = Arc::new(Semaphore::new(1));
                let store = PolicyStore::new(
                    Some(strategy(Capture {
                        drops: drops.clone(),
                        thread: thread.clone(),
                        panics,
                    })),
                    slots.clone(),
                );
                let mut pending: Vec<_> = (0..crate::owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT)
                    .map(|_| crate::owned_payload::try_pending_cleanup_permit(&slots).unwrap())
                    .collect();
                runtime.block_on(async {
                    let mut admission = Box::pin(store.dispose());
                    assert!(futures::poll!(&mut admission).is_pending());
                    drop(admission);
                });
                runtime.shutdown_timeout(Duration::from_secs(1));
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert!(store.get().is_none());
                {
                    let state = store.state.lock().unwrap();
                    assert!(state.strategy.is_some());
                    assert!(state.disposal.is_none());
                }
                if retry {
                    drop(pending.pop());
                    let observer = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    observer.block_on(async {
                        let result = tokio::time::timeout(Duration::from_secs(5), store.dispose())
                            .await
                            .expect("new host must retry unadmitted capture retirement");
                        assert_eq!(result.is_err(), panics);
                        if let Err(error) = result {
                            let error = error.to_string();
                            assert!(error.contains("policy capture destructor"));
                            assert_eq!(error, store.dispose().await.unwrap_err().to_string());
                        }
                    });
                    assert_ne!(*thread.lock().unwrap(), Some(std::thread::current().id()));
                }
                drop(store);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                if !retry {
                    assert_eq!(*thread.lock().unwrap(), Some(std::thread::current().id()));
                }
                drop(pending);
            }
        }
    }

    #[test]
    fn running_policy_disposal_keeps_pending_budget_after_host_exit() {
        use std::sync::{Condvar, mpsc};
        struct Gate {
            open: Mutex<bool>,
            changed: Condvar,
        }
        struct Release(Arc<Gate>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.open.lock().unwrap() = true;
                self.0.changed.notify_all();
            }
        }
        struct Blocked {
            gate: Arc<Gate>,
            entered: mpsc::Sender<()>,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Blocked {
            fn drop(&mut self) {
                self.entered.send(()).unwrap();
                let (open, _) = self
                    .gate
                    .changed
                    .wait_timeout_while(
                        self.gate.open.lock().unwrap(),
                        Duration::from_secs(5),
                        |open| !*open,
                    )
                    .unwrap();
                assert!(*open, "test must release running policy disposal");
                self.drops.fetch_add(1, Ordering::SeqCst);
                panic!("running policy disposal after host exit");
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let gate = Arc::new(Gate {
            open: Mutex::new(false),
            changed: Condvar::new(),
        });
        let release = Release(gate.clone());
        let drops = Arc::new(AtomicUsize::new(0));
        let (entered, receive) = mpsc::channel();
        let capture = Blocked {
            gate,
            entered,
            drops: drops.clone(),
        };
        let slots = Arc::new(Semaphore::new(1));
        let store = PolicyStore::new(
            Some(SupervisorStrategyType::OneForOne(OneForOneStrategy {
                max_restarts: 0,
                within: Duration::from_secs(1),
                decider: BasicDecisionFn::new(move |_| {
                    let _ = &capture;
                    SupervisionDecision::Stop
                }),
            })),
            slots.clone(),
        );
        let fillers: Vec<_> = (0..crate::owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT - 1)
            .map(|_| crate::owned_payload::try_pending_cleanup_permit(&slots).unwrap())
            .collect();
        runtime.block_on(async {
            let mut cancelled = Box::pin(store.dispose());
            assert!(futures::poll!(&mut cancelled).is_pending());
            drop(cancelled);
            tokio::task::yield_now().await;
            receive.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        assert!(crate::owned_payload::try_pending_cleanup_permit(&slots).is_none());
        runtime.shutdown_timeout(Duration::from_millis(20));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(
            crate::owned_payload::try_pending_cleanup_permit(&slots).is_none(),
            "host exit returned pending capacity before actual policy disposal"
        );
        let observer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        drop(release);
        observer.block_on(async {
            let error = tokio::time::timeout(Duration::from_secs(5), store.dispose())
                .await
                .unwrap()
                .unwrap_err()
                .to_string();
            assert!(error.contains("running policy disposal after host exit"));
            assert_eq!(error, store.dispose().await.unwrap_err().to_string());
        });
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        observer.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(returned) = crate::owned_payload::try_pending_cleanup_permit(&slots)
                    {
                        drop(returned);
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("actual disposal must return its pending owner capacity");
        });
        drop(fillers);
    }

    #[test]
    fn nested_policy_retirement_does_not_wait_for_its_own_worker_or_pending_slot() {
        crate::test_support::isolated(
            "supervision_policy::tests::nested_policy_retirement_does_not_wait_for_its_own_worker_or_pending_slot",
            || {
                struct Nested {
                    inner: Arc<PolicyStore>,
                    drops: Arc<AtomicUsize>,
                }
                impl Drop for Nested {
                    fn drop(&mut self) {
                        let first = futures::executor::block_on(self.inner.dispose())
                            .unwrap_err()
                            .to_string();
                        assert!(first.contains("policy capture destructor"));
                        assert_eq!(
                            first,
                            futures::executor::block_on(self.inner.dispose())
                                .unwrap_err()
                                .to_string()
                        );
                        self.drops.fetch_add(1, Ordering::SeqCst);
                    }
                }
                for saturated in [false, true] {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .max_blocking_threads(1)
                        .build()
                        .unwrap();
                    let slots = Arc::new(Semaphore::new(1));
                    let inner_drops = Arc::new(AtomicUsize::new(0));
                    let inner = Arc::new(PolicyStore::new(
                        Some(strategy(Capture {
                            drops: inner_drops.clone(),
                            thread: Arc::new(Mutex::new(None)),
                            panics: true,
                        })),
                        slots.clone(),
                    ));
                    let outer_drops = Arc::new(AtomicUsize::new(0));
                    let capture = Nested {
                        inner: inner.clone(),
                        drops: outer_drops.clone(),
                    };
                    let outer = PolicyStore::new(
                        Some(SupervisorStrategyType::OneForAll(
                            parrot_api::supervisor::OneForAllStrategy {
                                max_restarts: 0,
                                within: Duration::from_secs(1),
                                decider: BasicDecisionFn::new(move |_| {
                                    let _ = &capture;
                                    SupervisionDecision::Stop
                                }),
                            },
                        )),
                        slots.clone(),
                    );
                    let pending: Vec<_> = (0..if saturated {
                        crate::owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT - 1
                    } else {
                        0
                    })
                        .map(|_| crate::owned_payload::try_pending_cleanup_permit(&slots).unwrap())
                        .collect();
                    let result = runtime.block_on(async {
                        tokio::time::timeout(Duration::from_secs(5), outer.dispose()).await
                    });
                    runtime.shutdown_timeout(Duration::from_secs(1));
                    result
                        .expect("nested policy disposal waited for its own cleanup pool")
                        .unwrap();
                    assert_eq!(outer_drops.load(Ordering::SeqCst), 1);
                    assert_eq!(inner_drops.load(Ordering::SeqCst), 1);
                    drop(pending);
                }
            },
        );
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

    #[test]
    fn cancelled_policy_waiter_keeps_bounded_cleanup_and_shared_receipt() {
        // Other saturation cases consume process-wide cleanup ownership. Keep
        // this worker/receipt contract's one-second bounds in its own process.
        crate::test_support::isolated(
            "supervision_policy::tests::cancelled_policy_waiter_keeps_bounded_cleanup_and_shared_receipt",
            || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
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
                        let restored =
                            tokio::time::timeout(Duration::from_secs(1), slots.acquire_owned())
                                .await
                                .unwrap()
                                .unwrap();
                        drop(restored);
                    }
                });
            },
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_decision_snapshot_cleanup_has_a_bounded_owner_queue() {
        let slots = Arc::new(Semaphore::new(0));
        let tracker = DecisionTracker::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let thread = Arc::new(Mutex::new(None));
        let count = crate::owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT + 32;
        let mut snapshots = Vec::with_capacity(count);
        for _ in 0..count {
            let capture = Capture {
                drops: drops.clone(),
                thread: thread.clone(),
                panics: false,
            };
            snapshots.push(
                tracker
                    .snapshot(slots.clone(), || {
                        Some(OwnedStrategy::new(strategy(capture)))
                    })
                    .unwrap()
                    .unwrap(),
            );
        }

        drop(snapshots);
        tokio::task::yield_now().await;
        assert!(
            drops.load(Ordering::SeqCst) >= 32,
            "cleanup beyond the pending-owner budget must not remain queued"
        );

        slots.add_permits(count);
        tokio::time::timeout(Duration::from_secs(5), async {
            while drops.load(Ordering::SeqCst) != count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all bounded decision snapshot captures must retire");
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
