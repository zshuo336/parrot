//! Bounded, best-effort shadow delivery for references looked up through the facade.
//!
//! Register an explicit copier for each supported message type. Copying executes
//! synchronously on the caller and must be bounded and side-effect free. The
//! worker delivers only TELLs; its errors never replace the primary result.
mod runtime;
use parrot_api::{
    address::ActorRef,
    message::MessageOptions,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
pub use runtime::MirrorHandle;
pub(crate) use runtime::MirrorRegistry;
use std::{any::TypeId, sync::Arc, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorPolicy {
    pub src_prefix: String,
    pub mirror_path: String,
}
impl MirrorPolicy {
    pub fn new(src_prefix: impl Into<String>, mirror_path: impl Into<String>) -> Self {
        Self {
            src_prefix: src_prefix.into(),
            mirror_path: mirror_path.into(),
        }
    }
    fn validate(&self) -> Result<(), MirrorError> {
        let local = |path: &str| {
            path.starts_with('/')
                && !path.starts_with("//")
                && path.len() <= 4096
                && !path.chars().any(char::is_control)
        };
        let actix_prefix = self.src_prefix.starts_with("actix://")
            && self.src_prefix.len() <= 4096
            && !self.src_prefix.chars().any(char::is_control);
        let actix_target = self
            .mirror_path
            .strip_prefix("actix://")
            .is_some_and(|tail| {
                let id = tail.rsplit_once('/').map_or(tail, |(_, id)| id);
                self.mirror_path.len() <= 4096
                    && !tail.chars().any(char::is_control)
                    && uuid::Uuid::parse_str(id).is_ok()
            });
        if !(local(&self.src_prefix) || actix_prefix)
            || !(local(&self.mirror_path) || actix_target)
            || self.src_prefix == self.mirror_path
        {
            return Err(MirrorError::InvalidPolicy);
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy)]
pub struct MirrorOptions {
    /// Total pending copies, including copying, queue and in-flight admission.
    pub capacity: usize,
    pub max_message_bytes: usize,
    pub max_pending_bytes: usize,
    pub delivery_timeout: Duration,
}
impl Default for MirrorOptions {
    fn default() -> Self {
        Self {
            capacity: 256,
            max_message_bytes: 1024 * 1024,
            max_pending_bytes: 8 * 1024 * 1024,
            delivery_timeout: Duration::from_millis(100),
        }
    }
}
impl MirrorOptions {
    fn validate(self) -> Result<(), MirrorError> {
        if self.capacity == 0
            || self.capacity > 65_536
            || self.max_message_bytes == 0
            || self.max_message_bytes > self.max_pending_bytes
            || self.max_pending_bytes > 64 * 1024 * 1024
            || self.delivery_timeout.is_zero()
            || self.delivery_timeout > Duration::from_secs(60)
        {
            return Err(MirrorError::InvalidOptions);
        }
        Ok(())
    }
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MirrorError {
    #[error("mirror requires distinct local /paths or native actix:// paths (at most 4096 bytes)")]
    InvalidPolicy,
    #[error("invalid mirror count, byte budget or delivery timeout")]
    InvalidOptions,
    #[error("register at least one message copier before registering a mirror policy")]
    NoCopier,
    #[error("mirror registration requires a Tokio runtime")]
    NoRuntime,
    #[error("mirror registry is closed")]
    Closed,
    #[error("mirror registry limit reached")]
    RegistryFull,
    #[error("mirror copier disposal failed (registration committed: {committed}): {reason}")]
    CopierDisposal { committed: bool, reason: String },
}
type Measure = dyn Fn(&BoxedMessage) -> Result<usize, String> + Send + Sync;
type CopyMessage = dyn Fn(&BoxedMessage) -> Result<BoxedMessage, String> + Send + Sync;
/// Typed copying contract. Account for owned heap data in `measure` before
/// copying. These application functions must terminate promptly and may not
/// mutate the source. Aliasing through Arc/interior mutability remains the
/// application's responsibility; arbitrary user allocation is not sandboxed.
#[derive(Clone)]
pub struct MirrorCopier {
    type_id: TypeId,
    measure: Arc<Measure>,
    copy: Arc<CopyMessage>,
}
impl MirrorCopier {
    // Fields may own unrelated hostile captures. Dispose each independently.
    fn retire(self) -> Result<(), String> {
        fn dispose<T>(value: T) -> Result<(), String> {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value)))
                .map_err(crate::owned_payload::panic_message)
        }
        let Self { measure, copy, .. } = self;
        let measure = dispose(measure);
        let copy = dispose(copy);
        match (measure, copy) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(a), Err(b)) => Err(format!("measure: {a}; copy: {b}")),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }

    pub fn new<T: Send + 'static>(
        measure: impl Fn(&T) -> usize + Send + Sync + 'static,
        copy: impl Fn(&T) -> Result<T, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            measure: Arc::new(move |msg| {
                msg.downcast_ref::<T>()
                    .map(&measure)
                    .ok_or_else(|| "copier type mismatch".into())
            }),
            copy: Arc::new(move |msg| {
                let value = msg.downcast_ref::<T>().ok_or("copier type mismatch")?;
                Ok(Box::new(copy(value)?) as BoxedMessage)
            }),
        }
    }
    pub fn cloned<T: Clone + Send + 'static>(
        measure: impl Fn(&T) -> usize + Send + Sync + 'static,
    ) -> Self {
        Self::new(measure, |value: &T| Ok(value.clone()))
    }
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MirrorStats {
    pub attempted: u64,
    pub queued: u64,
    /// Shadow mailbox admissions, not completed processing.
    pub admitted: u64,
    pub copy_failed: u64,
    pub full: u64,
    pub missing_target: u64,
    pub delivery_failed: u64,
    pub cancelled: u64,
    /// Failed framework-owned shadow input or copier retirements. A copier
    /// retirement counts once even if both of its captures fail. These failures
    /// do not replace the primary result or imply shadow processing completed.
    pub disposal_failed: u64,
    /// Unexpected worker termination, distinct from an individual admission failure.
    pub worker_failed: u64,
}
/// The facade keeps the policy worker alive. Clones share its capacity and
/// cancellation state. Creating and dropping an unpolled call has no effects.
pub(crate) trait MirrorLookup: Send + Sync {
    fn lookup<'a>(&'a self, path: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>>;
}

pub struct MirrorRef {
    inner: Arc<dyn ActorRef>,
    lookup: Arc<dyn MirrorLookup>,
    handle: MirrorHandle,
}
impl std::fmt::Debug for MirrorRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorRef")
            .field("path", &self.inner.path())
            .field("policy", self.handle.policy())
            .finish()
    }
}
impl MirrorRef {
    pub(crate) fn new(
        inner: Arc<dyn ActorRef>,
        lookup: Arc<dyn MirrorLookup>,
        handle: MirrorHandle,
    ) -> Self {
        Self {
            inner,
            lookup,
            handle,
        }
    }
}
#[async_trait::async_trait]
impl ActorRef for MirrorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.handle
                .offer(&msg, self.lookup.clone(), self.inner.path());
            self.inner.send(msg).await
        }))
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            let started = tokio::time::Instant::now();
            self.handle
                .offer(&msg, self.lookup.clone(), self.inner.path());
            self.inner
                .send_with_timeout(msg, remaining_budget(started, timeout)?)
                .await
        }))
    }
    fn send_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(parrot_api::trace::capture(async move {
            options.validate_single_attempt()?;
            let started = tokio::time::Instant::now();
            let mut options = options;
            self.handle
                .offer(&msg, self.lookup.clone(), self.inner.path());
            options.timeout = remaining_budget(started, options.timeout)?;
            self.inner.send_with_options(msg, options).await
        }))
    }
    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            self.handle
                .offer(&msg, self.lookup.clone(), self.inner.path());
            self.inner.deliver(msg).await
        }))
    }
    fn deliver_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(parrot_api::trace::capture(async move {
            options.validate_single_attempt()?;
            let started = tokio::time::Instant::now();
            let mut options = options;
            self.handle
                .offer(&msg, self.lookup.clone(), self.inner.path());
            options.timeout = remaining_budget(started, options.timeout)?;
            self.inner.deliver_with_options(msg, options).await
        }))
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        self.inner.stop()
    }
    fn path(&self) -> String {
        self.inner.path()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.inner.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            inner: self.inner.clone(),
            lookup: self.lookup.clone(),
            handle: self.handle.clone(),
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn remaining_budget(
    started: tokio::time::Instant,
    timeout: Option<Duration>,
) -> ActorResult<Option<Duration>> {
    timeout
        .map(|budget| {
            budget
                .checked_sub(started.elapsed())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(parrot_api::errors::ActorError::Timeout)
        })
        .transpose()
}
