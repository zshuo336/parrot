//! Bounded OTLP/JSON serialization of an explicit completion snapshot.
//!
//! No network, global collector, sampling decision or fabricated parent spans.
//! Keep the snapshot until the consumer has accepted the returned bytes; export
//! borrows it and never drains or mutates the recorder.
use parrot_api::trace::{RecordingSnapshot, SpanForest, SpanKind, SpanRecord, SpanStatus};
use serde::{ser::SerializeSeq, Serialize, Serializer};
use std::{collections::HashSet, io::Write, time::UNIX_EPOCH};

pub const MAX_OTLP_JSON_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_SERVICE_NAME_BYTES: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum TraceExportError {
    #[error("OTLP byte budget must be between 1 and {MAX_OTLP_JSON_BYTES}")]
    InvalidBudget,
    #[error("service name must contain 1..={MAX_SERVICE_NAME_BYTES} UTF-8 bytes")]
    InvalidServiceName,
    #[error(transparent)]
    Forest(#[from] parrot_api::trace::ForestError),
    #[error("OTLP requires unique span identities and an acyclic parent graph")]
    InvalidGraph,
    #[error("recording snapshot is inconsistent: {0}")]
    InvalidSnapshot(&'static str),
    #[error("span {sequence} has a zero trace, span or parent ID")]
    InvalidIdentity { sequence: u64 },
    #[error("snapshot wall-clock origin is before the Unix epoch")]
    BeforeEpoch,
    #[error("span {sequence} timestamp is outside the OTLP unsigned nanosecond range")]
    TimestampOverflow { sequence: u64 },
    #[error("OTLP document exceeds its {limit}-byte budget; no document was returned")]
    OutputLimit { limit: usize },
    #[error("unable to allocate the bounded OTLP output")]
    Allocation,
    #[error("OTLP JSON serialization failed: {0}")]
    Serialization(#[source] serde_json::Error),
}

/// Export one immutable snapshot as an OTLP ExportTraceServiceRequest JSON body.
/// IDs are hex; 64-bit timestamps are decimal strings. Recorded invocations use
/// INTERNAL spans; their Parrot kind and status remain explicit attributes.
/// Unknown/unobserved parents keep their original ID. Duplicate IDs and cycles
/// are rejected rather than merged or rewritten. Resource attributes retain
/// the recording receipt, including loss, saturation and terminal state.
pub fn export_otlp_json(
    snapshot: &RecordingSnapshot,
    service_name: &str,
    byte_limit: usize,
) -> Result<Vec<u8>, TraceExportError> {
    if !(1..=MAX_OTLP_JSON_BYTES).contains(&byte_limit) {
        return Err(TraceExportError::InvalidBudget);
    }
    if service_name.is_empty() || service_name.len() > MAX_SERVICE_NAME_BYTES {
        return Err(TraceExportError::InvalidServiceName);
    }
    let forest = SpanForest::new(snapshot)?;
    if forest.diagnostics().reused_identity_records != 0 || forest.diagnostics().cycle_records != 0
    {
        return Err(TraceExportError::InvalidGraph);
    }
    validate_receipt(snapshot)?;
    let origin = snapshot
        .origin
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TraceExportError::BeforeEpoch)?
        .as_nanos();
    if origin > u128::from(u64::MAX) {
        return Err(TraceExportError::TimestampOverflow { sequence: 0 });
    }
    let mut sequences = HashSet::with_capacity(snapshot.records.len());
    let mut observed = [0u64; 5];
    for record in &snapshot.records {
        if record.trace_id == [0; 16]
            || record.span_id == [0; 8]
            || record.parent_span_id == Some([0; 8])
        {
            return Err(TraceExportError::InvalidIdentity {
                sequence: record.sequence,
            });
        }
        let end = record.started.checked_add(record.duration).ok_or(
            TraceExportError::TimestampOverflow {
                sequence: record.sequence,
            },
        )?;
        if end > snapshot.elapsed {
            return Err(TraceExportError::InvalidSnapshot(
                "a record ends after the snapshot",
            ));
        }
        timestamps(origin, record)?;
        if record.sequence == 0 || record.sequence > snapshot.counts.started {
            return Err(TraceExportError::InvalidSnapshot(
                "record sequence exceeds admission accounting",
            ));
        }
        if !sequences.insert(record.sequence)
            && !(snapshot.counts.counters_overflowed && record.sequence == u64::MAX)
        {
            return Err(TraceExportError::InvalidSnapshot(
                "reused admission sequence",
            ));
        }
        observed[match record.status {
            SpanStatus::Ok => 0,
            SpanStatus::Error => 1,
            SpanStatus::Panicked => 2,
            SpanStatus::Cancelled => 3,
        }] += 1;
        observed[4] += u64::from(record.target_truncated);
    }
    let c = &snapshot.counts;
    if observed
        .into_iter()
        .zip([c.ok, c.errors, c.panicked, c.cancelled, c.targets_truncated])
        .any(|(retained, total)| retained > total)
        || snapshot.records.len() as u64 > c.completed
    {
        return Err(TraceExportError::InvalidSnapshot(
            "records exceed cumulative outcome counters",
        ));
    }
    let mut attributes = vec![
        string("service.name", service_name),
        string("parrot.recorder.id", hex(&snapshot.recorder_id)),
        boolean("parrot.recording.closed", snapshot.closed),
        boolean("parrot.recording.terminal", snapshot.terminal),
        boolean(
            "parrot.recording.counters_overflowed",
            snapshot.counts.counters_overflowed,
        ),
        unsigned("parrot.recording.capacity", snapshot.capacity),
        unsigned("parrot.recording.retained", snapshot.retained),
        unsigned("parrot.recording.exported", snapshot.records.len()),
        unsigned(
            "parrot.recording.unobserved_parents",
            forest.diagnostics().unobserved_parent_records,
        ),
        unsigned("parrot.recording.origin_unix_nano", origin),
        unsigned("parrot.recording.elapsed_nano", snapshot.elapsed.as_nanos()),
    ];
    let c = &snapshot.counts;
    for (key, value) in [
        ("parrot.recording.started", c.started),
        ("parrot.recording.in_flight", c.in_flight),
        ("parrot.recording.completed", c.completed),
        ("parrot.recording.ok", c.ok),
        ("parrot.recording.errors", c.errors),
        ("parrot.recording.panicked", c.panicked),
        ("parrot.recording.cancelled", c.cancelled),
        ("parrot.recording.overwritten", c.overwritten),
        ("parrot.recording.drained", c.drained),
        (
            "parrot.recording.rejected_after_close",
            c.rejected_after_close,
        ),
        ("parrot.recording.targets_truncated", c.targets_truncated),
    ] {
        attributes.push(unsigned(key, value));
    }
    let document = Document {
        resource_spans: [ResourceSpans {
            resource: Resource { attributes },
            scope_spans: [ScopeSpans {
                scope: Scope {
                    name: "parrot",
                    version: env!("CARGO_PKG_VERSION"),
                },
                spans: Spans { snapshot, origin },
            }],
        }],
    };
    let mut output = BoundedOutput {
        bytes: Vec::new(),
        limit: byte_limit,
        limit_hit: false,
        allocation_failed: false,
    };
    if let Err(error) = serde_json::to_writer(&mut output, &document) {
        return Err(if output.limit_hit {
            TraceExportError::OutputLimit { limit: byte_limit }
        } else if output.allocation_failed {
            TraceExportError::Allocation
        } else {
            TraceExportError::Serialization(error)
        });
    }
    Ok(output.bytes)
}

fn validate_receipt(s: &RecordingSnapshot) -> Result<(), TraceExportError> {
    if s.recorder_id == [0; 16] {
        return Err(TraceExportError::InvalidSnapshot("zero recorder ID"));
    }
    let c = &s.counts;
    if s.terminal != (s.closed && c.in_flight == 0) {
        return Err(TraceExportError::InvalidSnapshot(
            "terminal state disagrees with admission",
        ));
    }
    if s.retained != 0 && s.retained != s.records.len() {
        return Err(TraceExportError::InvalidSnapshot(
            "retained records disagree with the snapshot",
        ));
    }
    if !c.counters_overflowed {
        let completed =
            c.ok.checked_add(c.errors)
                .and_then(|v| v.checked_add(c.panicked))
                .and_then(|v| v.checked_add(c.cancelled));
        let accounted = c
            .overwritten
            .checked_add(c.drained)
            .and_then(|v| v.checked_add(s.retained as u64));
        if c.completed.checked_add(c.in_flight) != Some(c.started)
            || completed != Some(c.completed)
            || accounted != Some(c.completed)
            || c.targets_truncated > c.started
            || (s.retained == 0 && s.records.len() as u64 > c.drained)
        {
            return Err(TraceExportError::InvalidSnapshot(
                "recording counters disagree",
            ));
        }
    }
    Ok(())
}
fn timestamps(origin: u128, record: &SpanRecord) -> Result<(u64, u64), TraceExportError> {
    let fail = || TraceExportError::TimestampOverflow {
        sequence: record.sequence,
    };
    let start = origin
        .checked_add(record.started.as_nanos())
        .ok_or_else(fail)?;
    let end = start
        .checked_add(record.duration.as_nanos())
        .ok_or_else(fail)?;
    Ok((
        u64::try_from(start).map_err(|_| fail())?,
        u64::try_from(end).map_err(|_| fail())?,
    ))
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}
fn kind(value: SpanKind) -> &'static str {
    match value {
        SpanKind::TypedHandler => "typed_handler",
        SpanKind::DynamicHandler => "dynamic_handler",
        SpanKind::BusinessBatch => "business_batch",
        SpanKind::InboxTransaction => "inbox_transaction",
        SpanKind::ManagedTask => "managed_task",
        SpanKind::StreamStarted => "stream_started",
        SpanKind::StreamItem => "stream_item",
        SpanKind::StreamFinished => "stream_finished",
        SpanKind::StreamError => "stream_error",
        SpanKind::Application => "application",
    }
}
fn status(value: SpanStatus) -> &'static str {
    match value {
        SpanStatus::Ok => "ok",
        SpanStatus::Error => "error",
        SpanStatus::Panicked => "panicked",
        SpanStatus::Cancelled => "cancelled",
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
enum Value {
    StringValue(String),
    BoolValue(bool),
}
#[derive(Serialize)]
struct Attribute {
    key: &'static str,
    value: Value,
}
fn string(key: &'static str, value: impl Into<String>) -> Attribute {
    Attribute {
        key,
        value: Value::StringValue(value.into()),
    }
}
// OTLP AnyValue.int_value is signed. Decimal strings preserve every unsigned
// recorder counter, even above i64::MAX and JavaScript's exact-integer range.
fn unsigned(key: &'static str, value: impl ToString) -> Attribute {
    string(key, value.to_string())
}
fn boolean(key: &'static str, value: bool) -> Attribute {
    Attribute {
        key,
        value: Value::BoolValue(value),
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Document<'a> {
    resource_spans: [ResourceSpans<'a>; 1],
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceSpans<'a> {
    resource: Resource,
    scope_spans: [ScopeSpans<'a>; 1],
}
#[derive(Serialize)]
struct Resource {
    attributes: Vec<Attribute>,
}
#[derive(Serialize)]
struct Scope {
    name: &'static str,
    version: &'static str,
}
#[derive(Serialize)]
struct ScopeSpans<'a> {
    scope: Scope,
    spans: Spans<'a>,
}
struct Spans<'a> {
    snapshot: &'a RecordingSnapshot,
    origin: u128,
}
impl Serialize for Spans<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.snapshot.records.len()))?;
        for record in &self.snapshot.records {
            let (start, end) =
                timestamps(self.origin, record).map_err(serde::ser::Error::custom)?;
            let label = kind(record.kind);
            let message = (!matches!(record.status, SpanStatus::Ok)).then(|| status(record.status));
            let span = ExportSpan {
                trace_id: hex(&record.trace_id),
                span_id: hex(&record.span_id),
                parent_span_id: record.parent_span_id.map(|id| hex(&id)),
                name: if record.target.is_empty() {
                    label
                } else {
                    &record.target
                },
                kind: 1,
                start_time_unix_nano: start.to_string(),
                end_time_unix_nano: end.to_string(),
                attributes: [
                    unsigned("parrot.span.sequence", record.sequence),
                    string("parrot.span.kind", label),
                    string("parrot.span.target", &record.target),
                    boolean("parrot.span.target_truncated", record.target_truncated),
                    string("parrot.span.status", status(record.status)),
                ],
                status: Status {
                    code: if matches!(record.status, SpanStatus::Ok) {
                        1
                    } else {
                        2
                    },
                    message,
                },
            };
            sequence.serialize_element(&span)?;
        }
        sequence.end()
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportSpan<'a> {
    trace_id: String,
    span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_span_id: Option<String>,
    name: &'a str,
    kind: u8,
    start_time_unix_nano: String,
    end_time_unix_nano: String,
    attributes: [Attribute; 5],
    status: Status,
}
#[derive(Serialize)]
struct Status {
    code: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'static str>,
}
struct BoundedOutput {
    bytes: Vec<u8>,
    limit: usize,
    limit_hit: bool,
    allocation_failed: bool,
}
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let Some(needed) = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|size| *size <= self.limit)
        else {
            self.limit_hit = true;
            return Err(std::io::Error::other("OTLP output limit"));
        };
        if needed > self.bytes.capacity() {
            let capacity = needed
                .max(self.bytes.capacity().saturating_mul(2))
                .max(1024)
                .min(self.limit);
            if self
                .bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .is_err()
            {
                self.allocation_failed = true;
                return Err(std::io::Error::other("OTLP allocation failed"));
            }
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
