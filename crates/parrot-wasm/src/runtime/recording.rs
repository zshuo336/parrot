//! Invocation-owned optional recording, independent of the actor API layer.
use super::{guest_boundary, HostPanic, StoreState, TraceParent, WitTraceParent};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
};
use wasmtime::component::{ComponentType, Lift, Lower};

pub const MAX_GUEST_SPANS: usize = 256;
pub const MAX_GUEST_SPAN_TARGET_BYTES: usize = 256;

/// Host recording capability, supplied explicitly for one guest invocation.
/// Implementations must return fresh child identities and inert admission as None.
/// Accepted children must keep the invocation trace and use a nonzero span ID
/// distinct from the root and every child already admitted in that invocation.
/// Callback panics are reported at the invocation boundary. Rejected capability
/// cleanup is contained and cannot replace the original admission error.
pub trait GuestTrace: std::fmt::Debug + Send + Sync {
    fn parent(&self) -> TraceParent;
    fn start(&self, target: &str) -> Option<Box<dyn GuestSpan>>;

    /// Explicitly opt into recording a required recovery startup. The runtime
    /// calls this only after rebuilding a poisoned, previously started guest.
    /// Return a fresh child in this trace, owned separately from this invocation.
    /// Legacy adapters keep recovery unrecorded. Drop must retire unfinished work.
    fn start_recovery(&self, _target: &str) -> Option<Box<dyn GuestSpan>> {
        None
    }

    /// Optional owned execution context for recovery host callbacks. The
    /// runtime installs it lexically on each poll and future disposal.
    fn execution_scope(&self) -> Option<Arc<dyn GuestScope>> {
        None
    }
}

/// Installs host context for exactly one synchronous callback, restoring the
/// caller on return and unwind. Implementations must never retain the callback.
pub trait GuestScope: std::fmt::Debug + Send + Sync {
    fn in_scope(&self, operation: &mut dyn FnMut());
}

#[derive(Debug)]
pub(super) struct Scope(HostOwner<Arc<dyn GuestScope>>);
impl Scope {
    fn retire(self) -> wasmtime::Result<()> {
        guest_boundary(|| {
            drop(self.0.take());
            Ok(())
        })
    }
}

pub(super) fn scoped<T>(
    scope: Option<&Scope>,
    operation: impl FnOnce() -> wasmtime::Result<T>,
) -> wasmtime::Result<T> {
    guest_boundary(|| {
        let Some(scope) = scope else {
            return operation();
        };
        let mut operation = Some(operation);
        let mut result = None;
        let mut repeated = false;
        scope.0.get().in_scope(&mut || {
            if let Some(operation) = operation.take() {
                result = Some(operation());
            } else {
                repeated = true;
            }
        });
        wasmtime::ensure!(!repeated, "host execution scope repeated callback");
        result.ok_or_else(|| wasmtime::Error::msg("host execution scope omitted callback"))?
    })
}

pub(super) fn retire_scope(scope: Option<Scope>) -> wasmtime::Result<()> {
    scope.map_or(Ok(()), Scope::retire)
}

pub(super) fn scoped_owned<T>(
    scope: Option<Scope>,
    failures: super::asynchronous::CleanupFailures,
    operation: impl FnOnce() -> wasmtime::Result<T>,
) -> wasmtime::Result<T> {
    let result = scoped(scope.as_ref(), operation);
    let cleanup = retire_scope(scope);
    failures.record(&cleanup);
    cleanup?;
    result
}

/// Owns one accepted recording. Dropping it must retire unfinished work.
pub trait GuestSpan: GuestTrace {
    fn finish(self: Box<Self>, status: GuestSpanStatus);
}

#[derive(ComponentType, Lift, Lower, Clone, Copy, Debug, PartialEq, Eq)]
#[component(enum)]
#[repr(u8)]
pub enum GuestSpanStatus {
    #[component(name = "ok")]
    Ok,
    #[component(name = "error")]
    Error,
    #[component(name = "cancelled")]
    Cancelled,
    #[component(name = "panicked")]
    Panicked,
}

#[derive(ComponentType, Lift, Lower, Clone, Copy, Debug, PartialEq, Eq)]
#[component(enum)]
#[repr(u8)]
enum RecordingError {
    #[component(name = "disabled")]
    Disabled,
    #[component(name = "limit")]
    Limit,
    #[component(name = "invalid-target")]
    InvalidTarget,
    #[component(name = "unknown-handle")]
    UnknownHandle,
    #[component(name = "exhausted")]
    Exhausted,
}

#[derive(ComponentType, Lower)]
#[component(record)]
struct Started {
    handle: u64,
    parent: WitTraceParent,
}

/// Protect ownership before calling user code, including paths which unwind
/// before a capability or child can be installed in the Store.
#[derive(Debug)]
struct HostOwner<T: ?Sized>(Option<Box<T>>);

impl<T: ?Sized> HostOwner<T> {
    fn new(value: Box<T>) -> Self {
        Self(Some(value))
    }

    fn get(&self) -> &T {
        self.0.as_deref().expect("live host recording owner")
    }

    fn take(mut self) -> Box<T> {
        self.0.take().expect("live host recording owner")
    }
}

impl<T: ?Sized> Drop for HostOwner<T> {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            let _ = guest_boundary(|| {
                drop(value);
                Ok(())
            });
        }
    }
}

#[derive(Debug)]
enum Root {
    Invocation(HostOwner<dyn GuestTrace>),
    Recovery(HostOwner<dyn GuestSpan>),
}
impl Root {
    fn execution_scope(&self) -> Option<Arc<dyn GuestScope>> {
        match self {
            Self::Invocation(root) => root.get().execution_scope(),
            Self::Recovery(root) => root.get().execution_scope(),
        }
    }

    fn parent(&self) -> TraceParent {
        match self {
            Self::Invocation(root) => root.get().parent(),
            Self::Recovery(root) => root.get().parent(),
        }
    }

    fn start(&self, target: &str) -> Option<Box<dyn GuestSpan>> {
        match self {
            Self::Invocation(root) => root.get().start(target),
            Self::Recovery(root) => root.get().start(target),
        }
    }

    fn start_recovery(&self, target: &str) -> Option<Box<dyn GuestSpan>> {
        match self {
            Self::Invocation(root) => root.get().start_recovery(target),
            Self::Recovery(root) => root.get().start_recovery(target),
        }
    }

    fn retire(self, status: GuestSpanStatus) -> wasmtime::Result<()> {
        guest_boundary(|| {
            match self {
                Self::Invocation(root) => drop(root.take()),
                Self::Recovery(root) => root.take().finish(status),
            }
            Ok(())
        })
    }
}

#[derive(Debug)]
pub(super) struct Invocation {
    root: Root,
    parent: OnceLock<TraceParent>,
    spans: BTreeMap<u64, HostOwner<dyn GuestSpan>>,
    identities: BTreeSet<[u8; 8]>,
    admitted: usize,
}
impl Invocation {
    pub(super) fn new(root: Box<dyn GuestTrace>) -> Self {
        Self::with_root(Root::Invocation(HostOwner::new(root)))
    }

    fn with_root(root: Root) -> Self {
        Self {
            root,
            parent: OnceLock::new(),
            spans: BTreeMap::new(),
            identities: BTreeSet::new(),
            admitted: 0,
        }
    }

    pub(super) fn parent(&self) -> wasmtime::Result<TraceParent> {
        if let Some(parent) = self.parent.get() {
            return Ok(*parent);
        }
        let parent = guest_boundary(|| Ok(self.root.parent()))?;
        let _ = self.parent.set(parent);
        Ok(parent)
    }

    pub(super) fn execution_scope(&self) -> wasmtime::Result<Option<Scope>> {
        guest_boundary(|| Ok(self.root.execution_scope()))
            .map(|scope| scope.map(|scope| Scope(HostOwner::new(Box::new(scope)))))
    }

    pub(super) fn recovery(&self, target: &str) -> wasmtime::Result<Option<Self>> {
        let Some(root) = guest_boundary(|| Ok(self.root.start_recovery(target)))? else {
            return Ok(None);
        };
        let recovery = Self::with_root(Root::Recovery(HostOwner::new(root)));
        let parent = recovery.parent()?;
        let source = self.parent()?;
        wasmtime::ensure!(
            parent.is_valid()
                && parent.trace_id == source.trace_id
                && parent.span_id != source.span_id,
            "invalid host recovery recording identity"
        );
        Ok(Some(recovery))
    }

    fn start(
        &mut self,
        next: &mut u64,
        parent: Option<u64>,
        target: &str,
    ) -> wasmtime::Result<Result<Started, RecordingError>> {
        let (span, following) = match self.admit(next, parent, target) {
            Ok(admission) => admission,
            Err(error) => return Ok(Err(error)),
        };
        let root = self.parent()?;
        let identity = span.get().parent();
        wasmtime::ensure!(
            identity.is_valid()
                && identity.trace_id == root.trace_id
                && identity.span_id != root.span_id
                && !self.identities.contains(&identity.span_id),
            "invalid host recording child identity"
        );
        // Keep completed identities until invocation exit, so finish cannot
        // make the same ID available again. Admission bounds this set at 256.
        self.identities.insert(identity.span_id);
        let handle = *next;
        *next = following;
        self.spans.insert(handle, span);
        self.admitted += 1;
        Ok(Ok(Started {
            handle,
            parent: WitTraceParent {
                trace_id: identity.trace_id.to_vec(),
                span_id: identity.span_id.to_vec(),
            },
        }))
    }

    fn admit(
        &self,
        next: &u64,
        parent: Option<u64>,
        target: &str,
    ) -> Result<(HostOwner<dyn GuestSpan>, u64), RecordingError> {
        if target.is_empty() || target.len() > MAX_GUEST_SPAN_TARGET_BYTES {
            return Err(RecordingError::InvalidTarget);
        }
        if self.admitted >= MAX_GUEST_SPANS {
            return Err(RecordingError::Limit);
        }
        let following = next.checked_add(1).ok_or(RecordingError::Exhausted)?;
        let span = match parent {
            Some(handle) => self
                .spans
                .get(&handle)
                .ok_or(RecordingError::UnknownHandle)?
                .get()
                .start(target),
            None => self.root.start(target),
        }
        .ok_or(RecordingError::Disabled)?;
        Ok((HostOwner::new(span), following))
    }

    fn finish(&mut self, handle: u64, status: GuestSpanStatus) -> Result<(), RecordingError> {
        self.spans
            .remove(&handle)
            .ok_or(RecordingError::UnknownHandle)?
            .take()
            .finish(status);
        Ok(())
    }

    pub(super) fn retire(
        self,
        status: GuestSpanStatus,
        completed: GuestSpanStatus,
    ) -> wasmtime::Result<()> {
        let Self { root, spans, .. } = self;
        let mut failure = None;
        for span in spans.into_values() {
            let status = if failure.is_some() {
                GuestSpanStatus::Panicked
            } else {
                status
            };
            // Host adapters are user code too. Retire every accepted owner even
            // if an earlier finish callback panics.
            if let Err(error) = guest_boundary(|| {
                span.take().finish(status);
                Ok(())
            }) {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
        // Complete a recovery owner after all of its children. A callback
        // failure belongs to this phase even when the guest hook succeeded.
        let completed = if failure.is_some() {
            GuestSpanStatus::Panicked
        } else {
            completed
        };
        if let Err(error) = root.retire(completed) {
            if failure.is_none() {
                failure = Some(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

pub(super) fn outcome<T>(result: &wasmtime::Result<T>) -> GuestSpanStatus {
    match result {
        Ok(_) => GuestSpanStatus::Cancelled,
        Err(error) if error.downcast_ref::<HostPanic>().is_some() => GuestSpanStatus::Panicked,
        Err(error)
            if matches!(
                error.downcast_ref::<wasmtime::Trap>(),
                Some(wasmtime::Trap::OutOfFuel | wasmtime::Trap::Interrupt)
            ) =>
        {
            GuestSpanStatus::Cancelled
        }
        Err(_) => GuestSpanStatus::Error,
    }
}

pub(super) fn link(linker: &mut wasmtime::component::Linker<StoreState>) -> wasmtime::Result<()> {
    let mut recording = linker.instance("parrot:recording/spans@0.1.0")?;
    recording.func_wrap(
        "start",
        |mut caller: wasmtime::StoreContextMut<'_, StoreState>,
         (parent, target): (Option<u64>, String)| {
            let state = caller.data_mut();
            Ok((match state.recording.as_mut() {
                Some(invocation) => {
                    invocation.start(&mut state.next_span_handle, parent, &target)?
                }
                None => Err(RecordingError::Disabled),
            },))
        },
    )?;
    recording.func_wrap(
        "finish",
        |mut caller: wasmtime::StoreContextMut<'_, StoreState>,
         (handle, status): (u64, GuestSpanStatus)| {
            let state = caller.data_mut();
            let result = guest_boundary(|| {
                Ok(match state.recording.as_mut() {
                    Some(invocation) => invocation.finish(handle, status),
                    None => Err(RecordingError::Disabled),
                })
            });
            state.cleanup.record(&result);
            result.map(|result| (result,))
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Arc,
    };
    #[derive(Debug)]
    struct Disabled;
    impl GuestTrace for Disabled {
        fn parent(&self) -> TraceParent {
            TraceParent {
                trace_id: [1; 16],
                span_id: [2; 8],
            }
        }
        fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
            None
        }
    }

    #[test]
    fn handle_exhaustion_never_wraps_and_rejected_admission_does_not_consume_handles() {
        let mut invocation = Invocation::new(Box::new(Disabled));
        let mut next = u64::MAX;
        assert!(matches!(
            invocation.start(&mut next, None, "span").unwrap(),
            Err(RecordingError::Exhausted)
        ));
        assert_eq!(next, u64::MAX);
        next -= 1;
        assert!(matches!(
            invocation.start(&mut next, None, "span").unwrap(),
            Err(RecordingError::Disabled)
        ));
        assert_eq!(next, u64::MAX - 1);
        assert_eq!(invocation.admitted, 0);
        assert!(invocation.spans.is_empty());
    }

    #[test]
    fn invalid_child_never_publishes_or_consumes_a_handle_or_budget() {
        #[derive(Debug)]
        struct Capability {
            identity: TraceParent,
            starts: Arc<AtomicUsize>,
        }
        impl GuestTrace for Capability {
            fn parent(&self) -> TraceParent {
                self.identity
            }
            fn start(&self, _: &str) -> Option<Box<dyn GuestSpan>> {
                let first = self.starts.fetch_add(1, SeqCst) == 0;
                Some(Box::new(Self {
                    identity: TraceParent {
                        trace_id: if first { [0; 16] } else { [1; 16] },
                        span_id: [3; 8],
                    },
                    starts: self.starts.clone(),
                }))
            }
        }
        impl GuestSpan for Capability {
            fn finish(self: Box<Self>, _: GuestSpanStatus) {}
        }
        let mut invocation = Invocation::new(Box::new(Capability {
            identity: TraceParent {
                trace_id: [1; 16],
                span_id: [2; 8],
            },
            starts: Arc::new(AtomicUsize::new(0)),
        }));
        invocation.parent().unwrap();
        let mut next = 44;
        assert!(invocation.start(&mut next, None, "invalid").is_err());
        assert_eq!(next, 44);
        assert_eq!(invocation.admitted, 0);
        assert!(invocation.spans.is_empty());
        assert!(invocation.identities.is_empty());

        let span = invocation.start(&mut next, None, "valid").unwrap().unwrap();
        assert_eq!(span.handle, 44);
        assert_eq!(next, 45);
        assert_eq!(invocation.admitted, 1);
        assert_eq!(invocation.identities.len(), 1);
        invocation.finish(span.handle, GuestSpanStatus::Ok).unwrap();
        assert!(invocation.spans.is_empty());
        assert!(invocation.start(&mut next, None, "duplicate").is_err());
        assert_eq!(next, 45);
        assert_eq!(invocation.admitted, 1);
        assert_eq!(invocation.identities.len(), 1);
        assert!(invocation.spans.is_empty());
        invocation
            .retire(GuestSpanStatus::Cancelled, GuestSpanStatus::Cancelled)
            .unwrap();
    }
}
