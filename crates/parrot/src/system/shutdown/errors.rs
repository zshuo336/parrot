use crate::error_receipt::{Receipt, public_system_summary};
use parrot_api::system::SystemError;
use std::{error::Error, fmt};

/// One failure from facade shutdown, retaining its original cause.
/// Formatting uses a safe summary; inspecting `error` is an explicit caller action.
#[derive(Debug)]
pub struct ShutdownFailure {
    engine: Option<String>,
    operation: &'static str,
    error: Receipt<SystemError>,
    summary: String,
}
impl ShutdownFailure {
    pub(super) fn new(engine: Option<String>, operation: &'static str, error: SystemError) -> Self {
        Self::from_receipt(engine, operation, Receipt::new(error))
    }
    pub(super) fn from_receipt(
        engine: Option<String>,
        operation: &'static str,
        error: Receipt<SystemError>,
    ) -> Self {
        let summary = public_system_summary(error.error());
        Self {
            engine,
            operation,
            error,
            summary,
        }
    }
    /// The lexically first registered alias of this physical engine at shutdown.
    /// `None` identifies a facade or mirror failure.
    pub fn engine_name(&self) -> Option<&str> {
        self.engine.as_deref()
    }
    /// Failed operation, such as `shutdown`, `shutdown future disposal`, or `mirrors`.
    pub fn operation(&self) -> &'static str {
        self.operation
    }
    /// Borrow the original public error, including its concrete anyhow payload.
    /// For `Other`, downcast that payload directly, or use this failure's
    /// `Error::source` to inspect the payload without skipping its concrete type.
    pub fn error(&self) -> &SystemError {
        self.error.error()
    }
    /// Cached diagnostic which does not call business Display, Debug, or source.
    pub fn summary(&self) -> &str {
        &self.summary
    }
}
impl fmt::Display for ShutdownFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(engine) = &self.engine {
            write!(f, "engine {engine:?}")?;
            if self.operation != "shutdown" {
                write!(f, " ({})", self.operation)?;
            }
            f.write_str(": ")?;
        } else if self.operation != "facade" {
            write!(f, "{}: ", self.operation)?;
        }
        f.write_str(&self.summary)
    }
}
impl Error for ShutdownFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // Transparent enum sources can skip the wrapped concrete error itself.
        match self.error() {
            SystemError::Other(error) => Some(error.as_ref()),
            SystemError::ActorError(parrot_api::errors::ActorError::Other(error)) => {
                Some(error.as_ref())
            }
            SystemError::ActorError(error) => Some(error),
            error => Some(error),
        }
    }
}

/// Shared, ordered failures from one facade shutdown operation.
///
/// Available through `SystemError::Other` using `downcast_ref::<ShutdownErrors>()`.
/// The standard source chain exposes the first failure; `failures` exposes the
/// collected engine and disposal failures. A published timeout remains sticky
/// and supersedes later outcomes, whose causes are safely disposed.
/// Repeated receipts share the same original errors.
/// Final destruction of retained errors is isolated from arbitrary user panics.
#[derive(Clone, Debug)]
pub struct ShutdownErrors(Receipt<Vec<ShutdownFailure>>);
impl ShutdownErrors {
    pub(super) fn new(failures: Vec<ShutdownFailure>) -> Self {
        assert!(
            !failures.is_empty(),
            "shutdown failure collection is nonempty"
        );
        Self(Receipt::new(failures))
    }
    pub(super) fn message(message: &str) -> Self {
        Self::new(vec![ShutdownFailure::new(
            None,
            "facade",
            SystemError::Other(anyhow::anyhow!(message.to_owned())),
        )])
    }
    /// Failures in sorted engine-name order, followed by mirror failures.
    pub fn failures(&self) -> &[ShutdownFailure] {
        self.0.error()
    }
}
impl fmt::Display for ShutdownErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, failure) in self.failures().iter().enumerate() {
            if index != 0 {
                f.write_str("; ")?;
            }
            fmt::Display::fmt(failure, f)?;
        }
        Ok(())
    }
}
impl Error for ShutdownErrors {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.failures().first().map(|failure| failure as _)
    }
}
