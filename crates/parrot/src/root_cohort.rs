//! Explicit supervision domains for top-level actors on the built-in engines.
use crate::{children::ChildScope, error_receipt::ActorReceipt};
use parrot_api::{
    errors::ActorError,
    supervisor::SupervisorStrategyType,
    types::{ActorResult, BoxedFuture},
};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::{runtime::Handle, sync::Semaphore};

mod retirement;

/// A system-owned group of roots with a common failure policy and recovery barriers.
/// Clones identify the same group. Dropping a handle does not stop its members;
/// use `stop` or system shutdown to join their actual resource retirement.
#[derive(Clone)]
pub struct RootCohort {
    pub(crate) scope: Arc<ChildScope>,
    owner: Weak<RootCohorts>,
    retirement: Arc<retirement::Retirement>,
}
impl RootCohort {
    /// Snapshot the currently owned roots, including pending first constructors.
    pub fn members(&self) -> parrot_api::context::ReadOnlyChildrenVec {
        self.scope.view()
    }

    /// Close admission immediately and join all members and policy captures.
    /// Cleanup continues if this future is dropped. This receipt waits for actual
    /// disposal, including work that has exceeded a member's public stop deadline.
    pub fn stop(&self) -> BoxedFuture<'static, ActorResult<()>> {
        self.scope.close();
        self.retirement
            .start(self.scope.clone(), self.owner.clone())
    }

    pub(crate) fn validate(&self, owner: &Arc<RootCohorts>) -> ActorResult<()> {
        if !Weak::ptr_eq(&self.owner, &Arc::downgrade(owner)) {
            return Err(ActorError::InternalError(
                "root cohort belongs to a different actor system".into(),
            ));
        }
        if self.scope.is_closed() {
            return Err(ActorError::Stopped);
        }
        Ok(())
    }
}

pub(crate) fn lifecycle_runtime() -> ActorResult<Handle> {
    retirement::runtime()
}

#[derive(Default)]
struct State {
    closed: bool,
    scopes: Vec<RootCohort>,
    closing: Option<Vec<RootCohort>>,
}

/// Retain live groups independently of public handles, including pending cleanup.
pub(crate) struct RootCohorts {
    state: Mutex<State>,
    guardian: OnceLock<RootCohort>,
    retirement_runtime: Handle,
}
impl Drop for RootCohorts {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap();
        for group in state.scopes.iter().chain(state.closing.iter().flatten()) {
            drop(group.stop());
        }
    }
}
impl RootCohorts {
    pub(crate) fn new() -> ActorResult<Self> {
        Ok(Self {
            state: Mutex::default(),
            guardian: OnceLock::new(),
            retirement_runtime: retirement::runtime()?,
        })
    }

    pub(crate) fn retirement_runtime(&self) -> &Handle {
        &self.retirement_runtime
    }

    #[cfg(test)]
    pub(crate) fn set_test_retirement_runtime(&mut self, runtime: Handle) {
        self.retirement_runtime = runtime;
    }
    pub(crate) fn guardian(&self) -> Option<&RootCohort> {
        self.guardian.get()
    }

    pub(crate) async fn install_guardian(
        self: &Arc<Self>,
        strategy: SupervisorStrategyType,
        timeout: Duration,
        slots: Arc<Semaphore>,
    ) -> ActorResult<()> {
        let group = self.create(strategy, timeout, slots).await?;
        if let Err(group) = self.guardian.set(group) {
            group.stop().await?;
            return Err(ActorError::InternalError(
                "guardian already configured".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn create(
        self: &Arc<Self>,
        strategy: SupervisorStrategyType,
        timeout: Duration,
        slots: Arc<Semaphore>,
    ) -> BoxedFuture<'static, ActorResult<RootCohort>> {
        let strategy =
            crate::owned_payload::OwnedPayload::new(strategy, &self.retirement_runtime, &slots);
        let owner = self.clone();
        Box::pin(parrot_api::trace::capture(async move {
            let retirement = match retirement::Retirement::new() {
                Ok(retirement) => retirement,
                Err(error) => {
                    strategy.retire().await?;
                    return Err(error);
                }
            };
            let scope = ChildScope::new(timeout, slots);
            scope.set_strategy(strategy.take());
            let group = RootCohort {
                scope: scope.clone(),
                owner: Arc::downgrade(&owner),
                retirement,
            };
            let accepted = {
                let mut state = owner.state.lock().unwrap();
                if state.closed {
                    Err(ActorError::Stopped)
                } else if state.scopes.len() == 1024 {
                    Err(ActorError::InternalError(
                        "root cohort capacity exhausted (1024 live or retiring groups)".into(),
                    ))
                } else {
                    state.scopes.push(group.clone());
                    Ok(())
                }
            };
            if let Err(error) = accepted {
                group.stop().await?;
                return Err(error);
            }
            Ok(group)
        }))
    }

    pub(crate) fn close(&self) {
        for group in self.begin_close() {
            drop(group.stop());
        }
    }

    fn begin_close(&self) -> Vec<RootCohort> {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        if state.closing.is_none() {
            state.closing = Some(state.scopes.clone());
        }
        state.closing.as_ref().unwrap().clone()
    }

    fn remove(&self, scope: &Arc<ChildScope>) {
        let removed = {
            let mut state = self.state.lock().unwrap();
            state
                .scopes
                .iter()
                .position(|entry| Arc::ptr_eq(&entry.scope, scope))
                .map(|index| state.scopes.swap_remove(index))
        };
        drop(removed);
    }

    pub(crate) async fn join(&self) -> ActorResult<()> {
        let groups = self.begin_close();
        let results = futures::future::join_all(groups.iter().map(RootCohort::stop)).await;
        let mut failure = None;
        for result in results {
            if let Err(error) = result {
                failure.get_or_insert_with(|| ActorReceipt::from_error(error));
            }
        }
        failure.map_or(Ok(()), |error| Err(error.into_error()))
    }
}

pub(crate) async fn guardian_strategy(
    config: crate::owned_payload::OwnedPayload<parrot_api::system::GuardianConfig>,
) -> ActorResult<SupervisorStrategyType> {
    use parrot_api::supervisor::{DefaultStrategy, DefaultSupervisorStrategyFactory};
    let value = config.value();
    let has_budget = matches!(
        value.supervision_strategy,
        SupervisorStrategyType::OneForOne(_)
            | SupervisorStrategyType::OneForAll(_)
            | SupervisorStrategyType::Default(DefaultStrategy::RestartOnFailure)
    );
    if has_budget && value.max_restarts > 0 && value.restart_window.is_zero() {
        config.retire().await?;
        return Err(ActorError::InternalError(
            "guardian restart_window must be positive when max_restarts is nonzero".into(),
        ));
    }
    let config = config.take();
    let mut strategy = config.supervision_strategy;
    match &mut strategy {
        SupervisorStrategyType::OneForOne(policy) => {
            policy.max_restarts = config.max_restarts;
            policy.within = config.restart_window;
        }
        SupervisorStrategyType::OneForAll(policy) => {
            policy.max_restarts = config.max_restarts;
            policy.within = config.restart_window;
        }
        SupervisorStrategyType::Default(DefaultStrategy::RestartOnFailure) => {
            strategy =
                SupervisorStrategyType::OneForOne(DefaultSupervisorStrategyFactory::one_for_one(
                    config.max_restarts,
                    config.restart_window,
                ));
        }
        SupervisorStrategyType::Default(_) => {}
    }
    Ok(strategy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Policy(Arc<AtomicUsize>);
    impl Policy {
        fn decide(&self) -> parrot_api::supervisor::SupervisionDecision {
            parrot_api::supervisor::SupervisionDecision::Stop
        }
    }
    impl Drop for Policy {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("closing snapshot policy disposal failure");
        }
    }

    #[tokio::test]
    async fn closing_snapshot_preserves_failure_after_completed_group_is_removed() {
        let owner = Arc::new(RootCohorts::new().unwrap());
        let disposed = Arc::new(AtomicUsize::new(0));
        let policy = Policy(disposed.clone());
        let group = owner
            .create(
                SupervisorStrategyType::OneForOne(parrot_api::supervisor::OneForOneStrategy {
                    max_restarts: 0,
                    within: Duration::from_secs(5),
                    decider: parrot_api::supervisor::BasicDecisionFn::new(move |_| policy.decide()),
                }),
                Duration::from_secs(5),
                Arc::new(Semaphore::new(1)),
            )
            .await
            .unwrap();
        owner.close();
        let first = tokio::time::timeout(Duration::from_secs(5), group.stop())
            .await
            .unwrap()
            .unwrap_err()
            .to_string();
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
        assert!(owner.state.lock().unwrap().scopes.is_empty());
        assert!(first.contains("closing snapshot policy disposal failure"));
        for _ in 0..2 {
            let joined = tokio::time::timeout(Duration::from_secs(5), owner.join())
                .await
                .unwrap()
                .unwrap_err()
                .to_string();
            assert_eq!(first, joined, "system join lost completed group's failure");
        }
    }
}
