//! Causal indexing of one bounded completion snapshot. Missing observations are
//! boundaries, not invented handlers. All traversal is iterative.
use super::{RecordingSnapshot, SpanRecord, MAX_RECORD_CAPACITY, MAX_TARGET_BYTES};
use std::{collections::HashMap, fmt::Write};

/// Maximum output text length accepted by the renderer.
pub const MAX_FOREST_TEXT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParentRelation {
    /// The record explicitly has no parent identity.
    Root,
    /// Index of the uniquely observed parent in `SpanForest::nodes()`.
    Recorded(usize),
    /// Its parent identity was not observed in this snapshot. This alone cannot
    /// distinguish an unrecorded root, a running parent, or a lost/drained record.
    Unobserved,
    /// Multiple records reuse the parent identity; no occurrence is guessed.
    Ambiguous,
    /// This record participates in a parent cycle. Its edge is not traversed.
    Cycle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForestDiagnostics {
    /// Number of occurrences belonging to reused (trace, span) identities.
    pub reused_identity_records: usize,
    pub unobserved_parent_records: usize,
    pub ambiguous_parent_records: usize,
    pub cycle_records: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ForestError {
    #[error("snapshot capacity or record count exceeds recorder bounds")]
    InvalidCapacity,
    #[error("record {sequence} target exceeds the recorder text bound")]
    TargetTooLong { sequence: u64 },
    #[error("text budget must not exceed {MAX_FOREST_TEXT_BYTES} bytes")]
    TextBudgetTooLarge,
    #[error("text budget must be at least {minimum} bytes to retain diagnostics")]
    TextBudgetTooSmall { minimum: usize },
}

/// One occurrence; repeated span identities never overwrite each other's data.
#[derive(Debug)]
pub struct ForestNode<'a> {
    record: &'a SpanRecord,
    parent: ParentRelation,
    children: Vec<usize>,
}
impl<'a> ForestNode<'a> {
    pub fn record(&self) -> &'a SpanRecord {
        self.record
    }
    pub fn parent(&self) -> ParentRelation {
        self.parent
    }
    pub fn children(&self) -> &[usize] {
        &self.children
    }
}

/// Borrowed view of one snapshot, retaining its exact accounting receipt.
///
/// Roots include explicit roots and unresolved/cyclic boundaries; they do not
/// imply a complete trace. A drained snapshot may lack previously drained parents.
/// Parent matching includes the trace ID and requires a unique span occurrence.
/// Records are sorted by trace, monotonic start, sequence and span ID, with input
/// order breaking exact ties. No recursive object ownership or traversal is used.
#[derive(Debug)]
pub struct SpanForest<'a> {
    snapshot: &'a RecordingSnapshot,
    nodes: Vec<ForestNode<'a>>,
    roots: Vec<usize>,
    diagnostics: ForestDiagnostics,
}

#[derive(Debug)]
pub struct ForestText {
    pub text: String,
    pub emitted_records: usize,
    pub omitted_records: usize,
}

impl<'a> SpanForest<'a> {
    pub fn new(snapshot: &'a RecordingSnapshot) -> Result<Self, ForestError> {
        if !(1..=MAX_RECORD_CAPACITY).contains(&snapshot.capacity)
            || snapshot.records.len() > snapshot.capacity
            || snapshot.retained > snapshot.capacity
        {
            return Err(ForestError::InvalidCapacity);
        }
        for record in &snapshot.records {
            if record.target.len() > MAX_TARGET_BYTES {
                return Err(ForestError::TargetTooLong {
                    sequence: record.sequence,
                });
            }
        }
        let mut records: Vec<_> = snapshot.records.iter().collect();
        records.sort_by_key(|record| {
            (
                record.trace_id,
                record.started,
                record.sequence,
                record.span_id,
            )
        });
        let mut identities = HashMap::with_capacity(records.len());
        for (index, record) in records.iter().enumerate() {
            identities
                .entry((record.trace_id, record.span_id))
                .and_modify(|entry| *entry = None)
                .or_insert(Some(index));
        }
        let mut diagnostics = ForestDiagnostics::default();
        let mut nodes: Vec<_> = records
            .iter()
            .map(|record| {
                if identities[&(record.trace_id, record.span_id)].is_none() {
                    diagnostics.reused_identity_records += 1;
                }
                let parent = match record.parent_span_id {
                    None => ParentRelation::Root,
                    Some(parent) => match identities.get(&(record.trace_id, parent)) {
                        Some(Some(index)) => ParentRelation::Recorded(*index),
                        Some(None) => {
                            diagnostics.ambiguous_parent_records += 1;
                            ParentRelation::Ambiguous
                        }
                        None => {
                            diagnostics.unobserved_parent_records += 1;
                            ParentRelation::Unobserved
                        }
                    },
                };
                ForestNode {
                    record,
                    parent,
                    children: Vec::new(),
                }
            })
            .collect();
        // A functional parent graph has disjoint cycles. Each node is visited
        // once; remove every edge in a cycle so no arbitrary cycle root is chosen.
        let mut state = vec![0u8; nodes.len()];
        let mut path: Vec<usize> = Vec::new();
        for start in 0..nodes.len() {
            if state[start] != 0 {
                continue;
            }
            path.clear();
            let mut next = Some(start);
            while let Some(index) = next {
                if state[index] == 2 {
                    break;
                }
                if state[index] == 1 {
                    let cycle_start = path.iter().position(|&node| node == index).unwrap();
                    for &node in &path[cycle_start..] {
                        nodes[node].parent = ParentRelation::Cycle;
                        diagnostics.cycle_records += 1;
                    }
                    break;
                }
                state[index] = 1;
                path.push(index);
                next = match nodes[index].parent {
                    ParentRelation::Recorded(parent) => Some(parent),
                    _ => None,
                };
            }
            for &index in &path {
                state[index] = 2;
            }
        }
        let mut roots = Vec::new();
        // Iterating the already sorted node array gives sorted roots/siblings.
        for index in 0..nodes.len() {
            if let ParentRelation::Recorded(parent) = nodes[index].parent {
                nodes[parent].children.push(index);
            } else {
                roots.push(index);
            }
        }
        Ok(Self {
            snapshot,
            nodes,
            roots,
            diagnostics,
        })
    }

    pub fn snapshot(&self) -> &'a RecordingSnapshot {
        self.snapshot
    }
    pub fn nodes(&self) -> &[ForestNode<'a>] {
        &self.nodes
    }
    pub fn roots(&self) -> &[usize] {
        &self.roots
    }
    pub fn diagnostics(&self) -> &ForestDiagnostics {
        &self.diagnostics
    }

    /// Render parents before children, with bounded indentation and byte output.
    /// All target control characters are escaped. The header always preserves
    /// cumulative loss/admission counts and reports records omitted by this view.
    /// Returned text is a snapshot view, not a completeness or delivery assertion.
    pub fn render_text(&self, max_bytes: usize) -> Result<ForestText, ForestError> {
        if max_bytes > MAX_FOREST_TEXT_BYTES {
            return Err(ForestError::TextBudgetTooLarge);
        }
        let total = self.nodes.len();
        let header_reserve = self.header(total, total).len();
        if max_bytes < header_reserve {
            return Err(ForestError::TextBudgetTooSmall {
                minimum: header_reserve,
            });
        }
        let mut body = String::new();
        let mut stack: Vec<_> = self
            .roots
            .iter()
            .rev()
            .map(|&index| (index, 0usize))
            .collect();
        let mut emitted = 0;
        while let Some((index, depth)) = stack.pop() {
            let node = &self.nodes[index];
            let record = node.record;
            let relation = match node.parent {
                ParentRelation::Root => "root",
                ParentRelation::Recorded(_) => "recorded",
                ParentRelation::Unobserved => "unobserved",
                ParentRelation::Ambiguous => "ambiguous",
                ParentRelation::Cycle => "cycle",
            };
            let parent = record
                .parent_span_id
                .map(hex)
                .unwrap_or_else(|| "none".into());
            let line = format!(
                "{:indent$}seq={} depth={} trace={} span={} parent={} [{}] {:?} {:?} start_ns={} duration_ns={} target={:?} target_truncated={}\n",
                "", record.sequence, depth, hex(record.trace_id), hex(record.span_id), parent,
                relation, record.kind, record.status, record.started.as_nanos(),
                record.duration.as_nanos(), record.target, record.target_truncated,
                indent = depth.min(16) * 2,
            );
            if line.len() > max_bytes - header_reserve - body.len() {
                break;
            }
            body.push_str(&line);
            emitted += 1;
            stack.extend(node.children.iter().rev().map(|&child| (child, depth + 1)));
        }
        let omitted = total - emitted;
        let mut text = self.header(emitted, omitted);
        text.push_str(&body);
        Ok(ForestText {
            text,
            emitted_records: emitted,
            omitted_records: omitted,
        })
    }

    fn header(&self, emitted: usize, omitted: usize) -> String {
        let snapshot = self.snapshot;
        let c = &snapshot.counts;
        let d = &self.diagnostics;
        format!(
            "recorder={} records={} emitted={} omitted={} closed={} terminal={} retained={}\ncounts started={} in_flight={} completed={} ok={} errors={} panicked={} cancelled={} overwritten={} drained={} rejected_after_close={} targets_truncated={} counters_overflowed={}\nforest reused_identity_records={} unobserved_parent_records={} ambiguous_parent_records={} cycle_records={}\n",
            hex(snapshot.recorder_id), self.nodes.len(), emitted, omitted, snapshot.closed,
            snapshot.terminal, snapshot.retained, c.started, c.in_flight, c.completed, c.ok,
            c.errors, c.panicked, c.cancelled, c.overwritten, c.drained, c.rejected_after_close,
            c.targets_truncated, c.counters_overflowed, d.reused_identity_records,
            d.unobserved_parent_records, d.ambiguous_parent_records, d.cycle_records,
        )
    }
}

fn hex<const N: usize>(bytes: [u8; N]) -> String {
    let mut result = String::with_capacity(N * 2);
    for byte in bytes {
        write!(&mut result, "{byte:02x}").unwrap();
    }
    result
}
