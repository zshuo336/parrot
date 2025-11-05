//! Opt-in bounded completion records. No process-global collector or exporter.
//!
//! ```
//! use parrot_api::trace::{SpanGuard, SpanKind, SpanStatus, TraceRecorder};
//! let recorder = TraceRecorder::new(128).unwrap();
//! let root = recorder.root();
//! root.in_scope(|| {
//!     let operation = SpanGuard::current(SpanKind::Application, "/example");
//!     root.child().in_scope(|| {
//!         SpanGuard::current(SpanKind::Application, "/example/child")
//!             .finish(SpanStatus::Ok);
//!     });
//!     operation.finish(SpanStatus::Ok);
//! });
//! recorder.close();
//! let receipt = recorder.drain();
//! assert!(receipt.terminal);
//! assert_eq!(receipt.records.len(), 2);
//! assert_eq!(receipt.retained, 0);
//! assert_eq!(receipt.counts.overwritten, 0);
//! ```
use super::TraceContext;
use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime},
};

/// Maximum retained target length in bytes. Truncation respects UTF-8 boundaries.
pub const MAX_TARGET_BYTES: usize = 256;
/// Hard limit for retained records per recorder (in-flight owners are separate).
pub const MAX_RECORD_CAPACITY: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    TypedHandler,
    DynamicHandler,
    BusinessBatch,
    /// One new inbox transaction, including its transition and durable commit.
    InboxTransaction,
    /// Admitted context work, from registration through owned cleanup.
    ManagedTask,
    StreamStarted,
    StreamItem,
    StreamFinished,
    StreamError,
    Application,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanStatus {
    Ok,
    Error,
    Panicked,
    Cancelled,
}
impl SpanStatus {
    /// Classify the handler's actual result without formatting user errors.
    pub fn from_result<T>(result: &crate::types::ActorResult<T>) -> Self {
        match result {
            Ok(_) => Self::Ok,
            Err(crate::errors::ActorError::Panic(_)) => Self::Panicked,
            Err(_) => Self::Error,
        }
    }
}

/// One completed invocation. This carries IDs, never a context/ancestor owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanRecord {
    /// Assigned at admission, unique within this recorder unless counters overflow.
    pub sequence: u64,
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
    pub parent_span_id: Option<[u8; 8]>,
    pub kind: SpanKind,
    pub target: String,
    pub target_truncated: bool,
    /// Monotonic offset from the recorder's origin; not cross-process wall time.
    pub started: Duration,
    pub duration: Duration,
    pub status: SpanStatus,
}

/// Cumulative accounting. A snapshot never silently presents loss as completeness.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordingCounts {
    pub started: u64,
    pub in_flight: u64,
    pub completed: u64,
    pub ok: u64,
    pub errors: u64,
    pub panicked: u64,
    pub cancelled: u64,
    pub overwritten: u64,
    pub drained: u64,
    pub rejected_after_close: u64,
    pub targets_truncated: u64,
    /// If true, counts are saturated and must not be used as exact accounting.
    pub counters_overflowed: bool,
}

/// Atomically captured buffer and accounting, including the effect of this drain.
#[derive(Clone, Debug)]
pub struct RecordingSnapshot {
    pub recorder_id: [u8; 16],
    pub origin: SystemTime,
    pub elapsed: Duration,
    pub capacity: usize,
    /// Records still retained in the collector after this operation.
    pub retained: usize,
    pub closed: bool,
    /// Admission is closed and all accepted recording owners have retired.
    pub terminal: bool,
    pub counts: RecordingCounts,
    /// Completion order, which may differ from sequence/start order.
    pub records: Vec<SpanRecord>,
}

#[derive(Debug, thiserror::Error)]
pub enum RecordingError {
    #[error("recording capacity must be between 1 and {MAX_RECORD_CAPACITY}")]
    InvalidCapacity,
    #[error("unable to allocate the bounded recording buffer: {0}")]
    Allocation(std::collections::TryReserveError),
}

struct Buffer {
    records: VecDeque<SpanRecord>,
    counts: RecordingCounts,
    closed: bool,
}
struct State {
    id: [u8; 16],
    origin: SystemTime,
    clock: Instant,
    capacity: usize,
    buffer: Mutex<Buffer>,
}
impl State {
    fn lock(&self) -> MutexGuard<'_, Buffer> {
        // No user code executes under this lock. Recovery keeps a recorder's
        // diagnostics from introducing a second panic during unwinding.
        self.buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// A bounded collector shared only by explicitly attached trace roots.
/// Retaining a context retains this bounded collector, never a parent chain.
#[derive(Clone)]
pub struct TraceRecorder(Arc<State>);
impl fmt::Debug for TraceRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TraceRecorder")
            .field("id", &self.0.id)
            .field("capacity", &self.0.capacity)
            .finish_non_exhaustive()
    }
}
impl PartialEq for TraceRecorder {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for TraceRecorder {}

fn add(value: &mut u64, by: u64, overflowed: &mut bool) {
    match value.checked_add(by) {
        Some(next) => *value = next,
        None => {
            *value = u64::MAX;
            *overflowed = true;
        }
    }
}

impl TraceRecorder {
    pub fn new(capacity: usize) -> Result<Self, RecordingError> {
        if !(1..=MAX_RECORD_CAPACITY).contains(&capacity) {
            return Err(RecordingError::InvalidCapacity);
        }
        let mut records = VecDeque::new();
        records
            .try_reserve_exact(capacity)
            .map_err(RecordingError::Allocation)?;
        Ok(Self(Arc::new(State {
            id: *uuid::Uuid::new_v4().as_bytes(),
            origin: SystemTime::now(),
            clock: Instant::now(),
            capacity,
            buffer: Mutex::new(Buffer {
                records,
                counts: RecordingCounts::default(),
                closed: false,
            }),
        })))
    }

    /// Start a fresh causal root attached to this recorder. The root itself is
    /// not a completed invocation; use `SpanGuard` to explicitly record its work.
    pub fn root(&self) -> TraceContext {
        TraceContext::with_recorder(self.clone())
    }

    /// Attach this process's recorder to a validated remote parent. Its actual
    /// children are recorded locally; importing it does not fabricate a span.
    pub fn remote_parent(&self, parent: super::TraceParent) -> TraceContext {
        TraceContext::remote_parent_with_recorder(parent, Some(self.clone()))
    }

    /// Seal new recording admission. Accepted handlers can still finish; this
    /// does not cancel work or block waiting for it. Repeated calls are harmless.
    pub fn close(&self) {
        self.0.lock().closed = true;
    }

    /// Whether admission is sealed. Already admitted work may still finish.
    pub fn is_closed(&self) -> bool {
        self.0.lock().closed
    }

    /// Copy retained records and a consistent receipt without consuming them.
    pub fn snapshot(&self) -> RecordingSnapshot {
        self.read(false)
    }

    /// Consume retained records atomically. Cumulative counters are not reset.
    pub fn drain(&self) -> RecordingSnapshot {
        self.read(true)
    }

    fn read(&self, drain: bool) -> RecordingSnapshot {
        let mut buffer = self.0.lock();
        let records = if drain {
            let count = buffer.records.len() as u64;
            let counts = &mut buffer.counts;
            add(&mut counts.drained, count, &mut counts.counters_overflowed);
            buffer.records.drain(..).collect()
        } else {
            buffer.records.iter().cloned().collect()
        };
        RecordingSnapshot {
            recorder_id: self.0.id,
            origin: self.0.origin,
            elapsed: self.0.clock.elapsed(),
            capacity: self.0.capacity,
            retained: buffer.records.len(),
            closed: buffer.closed,
            terminal: buffer.closed && buffer.counts.in_flight == 0,
            counts: buffer.counts.clone(),
            records,
        }
    }

    fn start(&self, context: &TraceContext, kind: SpanKind, target: &str) -> Option<Owner> {
        let mut end = target.len().min(MAX_TARGET_BYTES);
        while !target.is_char_boundary(end) {
            end -= 1;
        }
        let truncated = end != target.len();
        let target = target[..end].to_owned();
        let mut buffer = self.0.lock();
        if buffer.closed {
            let counts = &mut buffer.counts;
            add(
                &mut counts.rejected_after_close,
                1,
                &mut counts.counters_overflowed,
            );
            return None;
        }
        let counts = &mut buffer.counts;
        add(&mut counts.started, 1, &mut counts.counters_overflowed);
        add(&mut counts.in_flight, 1, &mut counts.counters_overflowed);
        if truncated {
            add(
                &mut counts.targets_truncated,
                1,
                &mut counts.counters_overflowed,
            );
        }
        Some(Owner {
            recorder: self.clone(),
            record: Some(SpanRecord {
                sequence: counts.started,
                trace_id: context.trace_id(),
                span_id: context.span_id(),
                parent_span_id: context.parent_span_id(),
                kind,
                target,
                target_truncated: truncated,
                started: self.0.clock.elapsed(),
                duration: Duration::ZERO,
                status: SpanStatus::Cancelled,
            }),
        })
    }
}

struct Owner {
    recorder: TraceRecorder,
    record: Option<SpanRecord>,
}
impl Owner {
    fn current(kind: SpanKind, target: &str) -> Option<Self> {
        let context = TraceContext::current()?;
        context.0.recorder.as_ref()?.start(&context, kind, target)
    }
    fn finish(mut self, status: SpanStatus) {
        self.publish(status);
    }
    fn publish(&mut self, status: SpanStatus) {
        let Some(mut record) = self.record.take() else {
            return;
        };
        record.status = status;
        record.duration = self
            .recorder
            .0
            .clock
            .elapsed()
            .saturating_sub(record.started);
        let mut buffer = self.recorder.0.lock();
        let counts = &mut buffer.counts;
        counts.in_flight = counts.in_flight.saturating_sub(1);
        add(&mut counts.completed, 1, &mut counts.counters_overflowed);
        let outcome = match status {
            SpanStatus::Ok => &mut counts.ok,
            SpanStatus::Error => &mut counts.errors,
            SpanStatus::Panicked => &mut counts.panicked,
            SpanStatus::Cancelled => &mut counts.cancelled,
        };
        add(outcome, 1, &mut counts.counters_overflowed);
        if buffer.records.len() == self.recorder.0.capacity {
            buffer.records.pop_front();
            let counts = &mut buffer.counts;
            add(&mut counts.overwritten, 1, &mut counts.counters_overflowed);
        }
        buffer.records.push_back(record);
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.publish(if std::thread::panicking() {
            SpanStatus::Panicked
        } else {
            SpanStatus::Cancelled
        });
    }
}

/// Own one invocation's recording lifetime in the current causal identity.
///
/// Engines create this immediately before calling user code, inside their child
/// scope, and finish it with the actual result before supervision/reply handling.
/// Dropping an unfinished guard records cancellation (or panic while unwinding).
/// A caller which catches a panic must explicitly finish with `Panicked`; a guard
/// cannot infer a panic which its caller already caught. This guard does not run,
/// box, poll, catch or dispose user code. `current` keeps the current identity;
/// `child` allocates a separate identity without changing execution context.
#[must_use = "retain the guard until the invocation completes"]
pub struct SpanGuard(Option<Box<Owner>>);
impl SpanGuard {
    /// Returns an inert guard when no recorder is attached or admission is closed.
    pub fn current(kind: SpanKind, target: &str) -> Self {
        Self(Owner::current(kind, target).map(Box::new))
    }

    /// Record a distinct child lifetime without changing the caller's context.
    /// Useful for ownership records whose work keeps its registration identity.
    /// No identity is allocated when the current context has no recorder.
    pub fn child(kind: SpanKind, target: &str) -> Self {
        let owner = TraceContext::current().and_then(|context| {
            let recorder = context.0.recorder.as_ref()?;
            recorder.start(&context.child(), kind, target)
        });
        Self(owner.map(Box::new))
    }

    pub fn finish(mut self, status: SpanStatus) {
        if let Some(owner) = self.0.take() {
            (*owner).finish(status);
        }
    }

    pub fn finish_result<T>(self, result: &crate::types::ActorResult<T>) {
        self.finish(SpanStatus::from_result(result));
    }
}
