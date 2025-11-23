//! Recoverable rejection before any dynamic child creation work is admitted.
use parrot_api::{errors::ActorError, supervisor::SupervisorStrategyType, types::BoxedMessage};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChildInstanceAdmissionFailure {
    #[error("pending instance creation capacity exhausted")]
    Full,
    #[error("actor system is shutting down")]
    ShuttingDown,
}

/// Original context arguments returned without validation or background cleanup.
/// The actor remains in its engine wrapper and can be passed back to `spawn`.
/// Rejected inputs remain caller-owned. Before admission, the returned future
/// protects the original inputs; dropping it unpolled retires them through the
/// context's cleanup executor, including after the system or host has exited.
pub struct ChildInstanceAdmissionInputs {
    pub actor: BoxedMessage,
    pub config: BoxedMessage,
    pub strategy: Option<SupervisorStrategyType>,
}

/// Downcast [`ActorError::Other`]'s anyhow error to recover this rejection.
/// Calling `into_inputs` permits retry on the original parent context. Dropping
/// the error instead drops its caller-owned inputs; no cleanup task is created.
pub struct ChildInstanceAdmissionError {
    pub reason: ChildInstanceAdmissionFailure,
    inputs: Mutex<ChildInstanceAdmissionInputs>,
}
impl ChildInstanceAdmissionError {
    pub fn into_inputs(self) -> ChildInstanceAdmissionInputs {
        self.inputs
            .into_inner()
            .unwrap_or_else(|error| error.into_inner())
    }
}
impl std::fmt::Display for ChildInstanceAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.reason.fmt(f)
    }
}
impl std::fmt::Debug for ChildInstanceAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildInstanceAdmissionError")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl std::error::Error for ChildInstanceAdmissionError {}

pub(super) fn reject(
    reason: ChildInstanceAdmissionFailure,
    actor: super::disposal::OwnedPayload<BoxedMessage>,
    config: super::disposal::OwnedPayload<BoxedMessage>,
    strategy: super::disposal::OwnedPayload<Option<SupervisorStrategyType>>,
) -> ActorError {
    ActorError::Other(anyhow::Error::new(ChildInstanceAdmissionError {
        reason,
        inputs: Mutex::new(ChildInstanceAdmissionInputs {
            actor: actor.take(),
            config: config.take(),
            strategy: strategy.take(),
        }),
    }))
}
