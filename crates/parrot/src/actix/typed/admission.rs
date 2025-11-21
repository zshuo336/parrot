//! Recoverable child admission and independent retirement before publication.
use super::*;
use crate::actix::ChildInstanceAdmissionFailure;
use crate::owned_payload::OwnedPayload;
use parrot_api::supervisor::SupervisorStrategyType;
use std::sync::Mutex;

/// Original static child inputs; rejected values remain owned by the caller.
pub struct TypedInstanceAdmissionInputs<A: ParrotTypedDispatch> {
    pub actor: A,
    pub strategy: Option<SupervisorStrategyType>,
}

/// Downcast `ActorError::Other` and recover the inputs to retry on a live parent.
pub struct TypedInstanceAdmissionError<A: ParrotTypedDispatch> {
    pub reason: ChildInstanceAdmissionFailure,
    pub(super) inputs: Mutex<TypedInstanceAdmissionInputs<A>>,
}
impl<A: ParrotTypedDispatch> TypedInstanceAdmissionError<A> {
    pub fn into_inputs(self) -> TypedInstanceAdmissionInputs<A> {
        self.inputs
            .into_inner()
            .unwrap_or_else(|error| error.into_inner())
    }
}
impl<A: ParrotTypedDispatch> std::fmt::Display for TypedInstanceAdmissionError<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.reason.fmt(f)
    }
}
impl<A: ParrotTypedDispatch> std::fmt::Debug for TypedInstanceAdmissionError<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedInstanceAdmissionError")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl<A: ParrotTypedDispatch> std::error::Error for TypedInstanceAdmissionError<A> {}

/// Original factory and strategy returned without invocation or registration.
/// Dynamic Actix child factories use `F = BoxedMessage`; retry that unchanged
/// wrapper with the parent's `ActorSpawner::spawn_factory_erased` method.
pub struct FactoryAdmissionInputs<F> {
    pub factory: F,
    pub strategy: Option<SupervisorStrategyType>,
}
/// Recoverable child factory rejection carried by `ActorError::Other`.
pub struct FactoryAdmissionError<F: Send + 'static> {
    pub reason: ChildInstanceAdmissionFailure,
    inputs: Mutex<FactoryAdmissionInputs<F>>,
}
impl<F: Send + 'static> FactoryAdmissionError<F> {
    pub fn into_inputs(self) -> FactoryAdmissionInputs<F> {
        self.inputs
            .into_inner()
            .unwrap_or_else(|error| error.into_inner())
    }
    pub(in crate::actix) fn new(
        reason: ChildInstanceAdmissionFailure,
        factory: F,
        strategy: Option<SupervisorStrategyType>,
    ) -> Self {
        Self {
            reason,
            inputs: Mutex::new(FactoryAdmissionInputs { factory, strategy }),
        }
    }
}
impl<F: Send + 'static> std::fmt::Display for FactoryAdmissionError<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.reason.fmt(f)
    }
}
impl<F: Send + 'static> std::fmt::Debug for FactoryAdmissionError<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FactoryAdmissionError")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl<F: Send + 'static> std::error::Error for FactoryAdmissionError<F> {}

pub(super) async fn retire_source<A: ParrotTypedDispatch>(
    payload: Option<OwnedPayload<A>>,
    factory: Option<crate::constructor::owned::OwnedFactory<A>>,
    policy: Option<OwnedPayload<Option<SupervisorStrategyType>>>,
) -> ActorResult<()> {
    let (actor, factory, policy) = tokio::join!(
        async {
            match payload {
                Some(payload) => payload.retire().await,
                None => Ok(()),
            }
        },
        async {
            match factory {
                Some(factory) => factory.retire_in_lifecycle().await,
                None => Ok(()),
            }
        },
        async {
            match policy {
                Some(policy) => policy.retire().await,
                None => Ok(()),
            }
        },
    );
    actor.and(factory).and(policy)
}

impl ActixActorSystem {
    pub(in crate::actix) fn try_instance_creation(
        &self,
    ) -> Result<Arc<tokio::sync::OwnedSemaphorePermit>, ChildInstanceAdmissionFailure> {
        self.instance_admission
            .clone()
            .try_acquire_owned()
            .map(Arc::new)
            .map_err(|error| match error {
                tokio::sync::TryAcquireError::Closed => ChildInstanceAdmissionFailure::ShuttingDown,
                tokio::sync::TryAcquireError::NoPermits => ChildInstanceAdmissionFailure::Full,
            })
    }
}
