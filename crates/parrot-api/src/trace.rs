//! Explicit causal identities and poll-scoped context for message delivery.
//!
//! A context is metadata, not authentication or a completed span record. The
//! Rust mailbox implementations capture the caller context and create a child
//! for each handler invocation. Arbitrary spawned tasks do not inherit it: use
//! [`capture`] when that task is intentionally part of the current operation.

use std::{future::Future, sync::Arc};

#[path = "trace/forest.rs"]
mod forest;
pub use forest::{
    ForestDiagnostics, ForestError, ForestNode, ForestText, ParentRelation, SpanForest,
    MAX_FOREST_TEXT_BYTES,
};

#[path = "trace/recording.rs"]
mod recording;
pub use recording::{
    RecordingCounts, RecordingError, RecordingSnapshot, SpanGuard, SpanKind, SpanRecord,
    SpanStatus, TraceRecorder, MAX_RECORD_CAPACITY, MAX_TARGET_BYTES,
};

#[derive(Debug, PartialEq, Eq)]
struct Identity {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    parent_span_id: Option<[u8; 8]>,
    recorder: Option<TraceRecorder>,
}

/// Immutable identity shared by one operation's polls and owned cleanup.
/// Cloning retains only this identity, never an ancestor context or actor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceContext(Arc<Identity>);

/// Transportable causal parent identity. This metadata is not authentication.
/// No recorder, clock, process address or ancestor ownership crosses the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceParent {
    trace_id: [u8; 16],
    span_id: [u8; 8],
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("trace and parent span identifiers must both be nonzero")]
pub struct InvalidTraceParent;
impl TraceParent {
    pub fn new(trace_id: [u8; 16], span_id: [u8; 8]) -> Result<Self, InvalidTraceParent> {
        if trace_id == [0; 16] || span_id == [0; 8] {
            return Err(InvalidTraceParent);
        }
        Ok(Self { trace_id, span_id })
    }
    pub fn trace_id(self) -> [u8; 16] {
        self.trace_id
    }
    pub fn span_id(self) -> [u8; 8] {
        self.span_id
    }
}

tokio::task_local! {
    static CURRENT: Option<TraceContext>;
}

fn span_id() -> [u8; 8] {
    // The first eight bytes include UUID v4's nonzero version nibble.
    uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap()
}

impl TraceContext {
    /// Import a remote parent, without inventing a completed local invocation.
    /// Invoke `child` at the actual operation boundary. The parent's own parent
    /// is unknown, so this context must not be recorded as a new local root.
    pub fn remote_parent(parent: TraceParent) -> Self {
        Self::remote_parent_with_recorder(parent, None)
    }
    fn remote_parent_with_recorder(parent: TraceParent, recorder: Option<TraceRecorder>) -> Self {
        Self(Arc::new(Identity {
            trace_id: parent.trace_id,
            span_id: parent.span_id,
            parent_span_id: None,
            recorder,
        }))
    }
    /// Identify this operation as the parent of a subsequent remote operation.
    pub fn as_parent(&self) -> TraceParent {
        TraceParent {
            trace_id: self.0.trace_id,
            span_id: self.0.span_id,
        }
    }
    /// Start an independent trace. Identifiers use fresh OS-backed UUID v4
    /// randomness, rather than connection-local correlation IDs or actor paths.
    pub fn root() -> Self {
        Self(Arc::new(Identity {
            trace_id: *uuid::Uuid::new_v4().as_bytes(),
            span_id: span_id(),
            parent_span_id: None,
            recorder: None,
        }))
    }

    fn with_recorder(recorder: TraceRecorder) -> Self {
        Self(Arc::new(Identity {
            trace_id: *uuid::Uuid::new_v4().as_bytes(),
            span_id: span_id(),
            parent_span_id: None,
            recorder: Some(recorder),
        }))
    }

    /// New operation with an explicit causal parent in the same trace.
    pub fn child(&self) -> Self {
        Self(Arc::new(Identity {
            trace_id: self.0.trace_id,
            span_id: span_id(),
            parent_span_id: Some(self.0.span_id),
            recorder: self.0.recorder.clone(),
        }))
    }

    pub fn trace_id(&self) -> [u8; 16] {
        self.0.trace_id
    }

    pub fn span_id(&self) -> [u8; 8] {
        self.0.span_id
    }

    pub fn parent_span_id(&self) -> Option<[u8; 8]> {
        self.0.parent_span_id
    }

    pub fn current() -> Option<Self> {
        CURRENT.try_with(Clone::clone).ok().flatten()
    }

    /// Set this context while the closure runs, restoring the caller even if
    /// the closure unwinds. Do not use a thread guard across an async await.
    pub fn in_scope<R>(&self, operation: impl FnOnce() -> R) -> R {
        CURRENT.sync_scope(Some(self.clone()), operation)
    }

    /// Set this context on every poll and during disposal of the future.
    /// It is not left installed on a worker while the future is suspended.
    pub fn scope<F: Future>(&self, future: F) -> impl Future<Output = F::Output> {
        scope(Some(self.clone()), future)
    }
}

/// Run with an explicit context; `None` isolates an untraced operation from
/// ambient executor context. Works with Send and local, non-Send futures.
pub fn scope<F: Future>(
    context: Option<TraceContext>,
    future: F,
) -> impl Future<Output = F::Output> {
    CURRENT.scope(context, future)
}

/// Capture at future construction, so later polling in another task/context
/// cannot silently change the operation's parent. No automatic task inheritance.
pub fn capture<F: Future>(future: F) -> impl Future<Output = F::Output> {
    scope(TraceContext::current(), future)
}

/// Run synchronous construction or a handler with explicit context isolation.
/// `None` clears ambient context and unwinding restores the previous scope.
pub fn in_scope<R>(context: Option<TraceContext>, operation: impl FnOnce() -> R) -> R {
    CURRENT.sync_scope(context, operation)
}
