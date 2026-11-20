//! System-wide creation ownership is separate from each factory's invocation budget.
use super::{InstanceAdmissionFailure, SpawnError, ThreadActorSystem};
use parrot_api::supervisor::SupervisorStrategyType;
use std::sync::{Arc, Mutex};
use tokio::sync::OwnedSemaphorePermit;

/// Original inputs returned by a rejected native Thread child factory request.
/// `F` is the concrete typed factory, or `BoxedMessage` for the erased spawner.
pub struct FactoryAdmissionInputs<F> {
    pub factory: F,
    pub strategy: Option<SupervisorStrategyType>,
}

/// Downcast `ActorError::Other` to recover and retry rejected factory inputs.
/// A rejection registers no child and starts no construction or cleanup task.
pub struct FactoryAdmissionError<F: Send + 'static> {
    pub reason: InstanceAdmissionFailure,
    inputs: Mutex<FactoryAdmissionInputs<F>>,
}
impl<F: Send + 'static> FactoryAdmissionError<F> {
    pub fn into_inputs(self) -> FactoryAdmissionInputs<F> {
        self.inputs
            .into_inner()
            .unwrap_or_else(|error| error.into_inner())
    }
    pub(super) fn new(
        reason: InstanceAdmissionFailure,
        factory: F,
        strategy: Option<SupervisorStrategyType>,
    ) -> Self {
        Self {
            reason,
            inputs: Mutex::new(FactoryAdmissionInputs { factory, strategy }),
        }
    }
}
impl<F: Send + 'static> std::fmt::Debug for FactoryAdmissionError<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FactoryAdmissionError")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl<F: Send + 'static> std::fmt::Display for FactoryAdmissionError<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.reason.fmt(f)
    }
}
impl<F: Send + 'static> std::error::Error for FactoryAdmissionError<F> {}

impl ThreadActorSystem {
    pub(super) fn try_factory_creation(
        &self,
    ) -> Result<Arc<OwnedSemaphorePermit>, InstanceAdmissionFailure> {
        if self.is_shutting_down() {
            return Err(InstanceAdmissionFailure::ShuttingDown);
        }
        self.instance_admission
            .clone()
            .try_acquire_owned()
            .map(Arc::new)
            .map_err(|error| match error {
                tokio::sync::TryAcquireError::Closed => InstanceAdmissionFailure::ShuttingDown,
                tokio::sync::TryAcquireError::NoPermits => InstanceAdmissionFailure::Full,
            })
    }
    pub(super) async fn wait_factory_creation(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Arc<OwnedSemaphorePermit>, SpawnError> {
        if self.is_shutting_down() {
            return Err(SpawnError::SystemShutdown);
        }
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => Err(SpawnError::Timeout("factory admission exceeded creation deadline".into())),
            permit = self.instance_admission.clone().acquire_owned() => permit.map(Arc::new).map_err(|_| SpawnError::SystemShutdown),
        }
    }
}
