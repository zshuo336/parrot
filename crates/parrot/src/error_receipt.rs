//! Shared error receipts retain original causes without invoking user code.
use parrot_api::errors::ActorError;
use std::{fmt, sync::Arc};

struct Storage<E: Send + Sync + 'static>(Option<E>);
impl<E: Send + Sync + 'static> Drop for Storage<E> {
    fn drop(&mut self) {
        // Error references held by callers are not actor retirement work. The
        // final owner still isolates arbitrary user destructors, including when
        // a completion receiver is dropped without ever reading its outcome.
        drop(crate::owned_payload::OwnedPayload::unbound(self.0.take()));
    }
}
pub(crate) struct Receipt<E: Send + Sync + 'static>(Arc<Storage<E>>);
impl<E: Send + Sync + 'static> Clone for Receipt<E> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<E: Send + Sync + 'static> fmt::Debug for Receipt<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ErrorReceipt")
    }
}
impl<E: Send + Sync + 'static> Receipt<E> {
    pub(crate) fn new(error: E) -> Self {
        Self(Arc::new(Storage(Some(error))))
    }
    pub(crate) fn error(&self) -> &E {
        self.0.0.as_ref().expect("live error receipt")
    }
}

pub(crate) type ActorReceipt = Receipt<Arc<ActorError>>;
impl ActorReceipt {
    pub(crate) fn from_error(error: ActorError) -> Self {
        Self::new(Arc::new(error))
    }
    pub(crate) fn same_error(&self, original: &Arc<ActorError>) -> bool {
        Arc::ptr_eq(self.error(), original)
    }
    pub(crate) fn into_error(self) -> ActorError {
        match self.error().as_ref() {
            ActorError::InitializationError(s) => ActorError::InitializationError(s.clone()),
            ActorError::MessageHandlingError(s) => ActorError::MessageHandlingError(s.clone()),
            ActorError::Stopped => ActorError::Stopped,
            ActorError::Timeout => ActorError::TimeoutDetail("actor stop deadline elapsed".into()),
            ActorError::TimeoutDetail(s) => ActorError::TimeoutDetail(s.clone()),
            ActorError::ActorNotFound(s) => ActorError::ActorNotFound(s.clone()),
            ActorError::InternalError(s) => ActorError::InternalError(s.clone()),
            ActorError::ProcessMessageError(s) => ActorError::ProcessMessageError(s.clone()),
            ActorError::ReplyChannelError(s) => ActorError::ReplyChannelError(s.clone()),
            ActorError::Panic(s) => ActorError::Panic(s.clone()),
            ActorError::Other(_) => ActorError::Other(anyhow::Error::new(ActorCause(self))),
        }
    }
}
#[derive(Debug)]
struct ActorCause(ActorReceipt);
impl fmt::Display for ActorCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Explicit caller formatting retains the original text. Framework
        // lifecycle diagnostics use actor_summary, never this user callback.
        fmt::Display::fmt(self.0.error().as_ref(), f)
    }
}
impl std::error::Error for ActorCause {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.0.error().as_ref() {
            ActorError::Other(error) => Some(error.as_ref()),
            error => Some(error),
        }
    }
}

use crate::thread::error::SystemError;
pub(crate) type SystemReceipt = Receipt<SystemError>;
impl SystemReceipt {
    pub(crate) fn actor_receipt(&self) -> ActorReceipt {
        ActorReceipt::from_error(ActorError::Other(anyhow::Error::new(
            self.clone().into_error(),
        )))
    }
    pub(crate) fn into_error(self) -> SystemError {
        match self.error() {
            SystemError::ThreadSetupError(s) => SystemError::ThreadSetupError(s.clone()),
            SystemError::NotRunning => SystemError::NotRunning,
            SystemError::ShuttingDown => SystemError::ShuttingDown,
            SystemError::ShutdownError(s) => SystemError::ShutdownError(s.clone()),
            SystemError::Panic(s) => SystemError::Panic(s.clone()),
            SystemError::ActorCreationError(s) => SystemError::ActorCreationError(s.clone()),
            SystemError::ActorNotFound(s) => SystemError::ActorNotFound(s.clone()),
            SystemError::RegistrationError(s) => SystemError::RegistrationError(s.clone()),
            SystemError::ConfigError(s) => SystemError::ConfigError(s.clone()),
            SystemError::Timeout(s) => SystemError::Timeout(s.clone()),
            SystemError::WorkerStateError(s) => SystemError::WorkerStateError(s.clone()),
            SystemError::Other(_) => SystemError::Other(anyhow::Error::new(SystemCause(self))),
        }
    }
}
#[derive(Debug)]
struct SystemCause(SystemReceipt);
impl fmt::Display for SystemCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.0.error(), f)
    }
}
impl std::error::Error for SystemCause {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.0.error() {
            SystemError::Other(error) => Some(error.as_ref()),
            error => Some(error),
        }
    }
}

// Inspect only framework-owned representations. Walking arbitrary Error::source
// or formatting an unknown business error would execute user code at retirement.
enum Diagnostic<'a> {
    Actor(&'a ActorError),
    System(&'a SystemError),
}
fn diagnostic(mut current: Diagnostic<'_>) -> String {
    const OPAQUE: &str = "actor operation failed (original error retained in source chain)";
    // Both receipt wrappers and nested ActorError/SystemError values are safe
    // to inspect. Bound adversarial nesting without recursive stack growth.
    for _ in 0..64 {
        let other = match current {
            Diagnostic::Actor(ActorError::Other(error))
            | Diagnostic::System(SystemError::Other(error)) => error,
            Diagnostic::Actor(error) => return error.to_string(),
            Diagnostic::System(error) => return error.to_string(),
        };
        current = if let Some(cause) = other.downcast_ref::<ActorCause>() {
            Diagnostic::Actor(cause.0.error().as_ref())
        } else if let Some(cause) = other.downcast_ref::<SystemCause>() {
            Diagnostic::System(cause.0.error())
        } else if let Some(error) = other.downcast_ref::<ActorError>() {
            Diagnostic::Actor(error)
        } else if let Some(error) = other.downcast_ref::<SystemError>() {
            Diagnostic::System(error)
        } else {
            return OPAQUE.into();
        };
    }
    OPAQUE.into()
}
pub(crate) fn actor_summary(error: &ActorError) -> String {
    diagnostic(Diagnostic::Actor(error))
}
pub(crate) fn system_summary(error: &SystemError) -> String {
    diagnostic(Diagnostic::System(error))
}

/// A hook's business error can be superseded by a later resource cleanup error.
/// Acquire protected ownership before that precedence decision can drop it.
pub(crate) fn protect_actor(error: ActorError) -> ActorError {
    match error {
        error @ ActorError::Other(_) => ActorReceipt::new(Arc::new(error)).into_error(),
        error => error,
    }
}

#[cfg(test)]
mod diagnostic_contracts {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Debug)]
    struct HostileDisplay(Arc<AtomicUsize>);
    impl fmt::Display for HostileDisplay {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("business Display must not run");
        }
    }
    impl std::error::Error for HostileDisplay {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("business source must not run");
        }
    }
    fn wrap(error: ActorError) -> ActorError {
        let system = SystemReceipt::new(SystemError::Other(anyhow::Error::new(error))).into_error();
        ActorReceipt::new(Arc::new(ActorError::Other(anyhow::Error::new(system)))).into_error()
    }
    #[test]
    fn known_actor_error_survives_system_receipt_summary() {
        let error = SystemReceipt::new(SystemError::Other(anyhow::Error::new(
            ActorError::InternalError("child hook failed".into()),
        )))
        .into_error();
        assert_eq!(system_summary(&error), "Internal error: child hook failed");
    }
    #[test]
    fn known_panic_survives_nested_actor_and_system_receipt_summaries() {
        let mut error = ActorError::Panic("child destructor failed".into());
        for _ in 0..4 {
            error = wrap(error);
        }
        assert_eq!(actor_summary(&error), "Panic: child destructor failed");
    }
    #[test]
    fn wrapped_business_diagnostics_do_not_invoke_display_or_source() {
        let calls = Arc::new(AtomicUsize::new(0));
        let error = wrap(wrap(ActorError::Other(anyhow::Error::new(HostileDisplay(
            calls.clone(),
        )))));
        assert!(actor_summary(&error).contains("original error retained in source chain"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn deep_framework_diagnostics_are_bounded_without_recursion() {
        let mut error = ActorError::Panic("deep private failure".into());
        for _ in 0..80 {
            error = ActorError::Other(anyhow::Error::new(error));
        }
        let message = actor_summary(&error);
        assert!(message.len() < 128);
        assert!(message.contains("original error retained in source chain"));
    }
    #[test]
    fn direct_timeout_summary_keeps_its_existing_class_and_text() {
        let error = ActorError::TimeoutDetail("deadline witness".into());
        assert_eq!(actor_summary(&error), error.to_string());
    }
}
