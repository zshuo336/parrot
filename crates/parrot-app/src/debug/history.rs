//! Read-only bounded completion history, independent of OTLP acknowledgement.
use parrot_api::trace::{RecordingSnapshot, SpanRecord, TraceRecorder};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

struct Buffer {
    records: VecDeque<SpanRecord>,
    evicted: u64,
    overflowed: bool,
}

/// A non-consuming query handle. It cannot drain or close the export recorder.
/// Retention is a rolling window of observed completions, not a delivery receipt.
#[derive(Clone)]
pub struct TraceHistory {
    recorder: TraceRecorder,
    buffer: Arc<Mutex<Buffer>>,
    capacity: usize,
}

pub struct TraceHistorySnapshot {
    /// Window projection; counts are the original recorder's cumulative counts.
    pub snapshot: RecordingSnapshot,
    /// Completions evicted from the query window, separate from recorder loss.
    pub evicted: u64,
    pub counters_overflowed: bool,
}

impl TraceHistory {
    pub(super) fn new(recorder: TraceRecorder, capacity: usize) -> Self {
        Self {
            recorder,
            capacity,
            buffer: Arc::new(Mutex::new(Buffer {
                records: VecDeque::new(),
                evicted: 0,
                overflowed: false,
            })),
        }
    }

    /// Snapshot includes both drained export batches and newly completed records.
    /// The shared lock closes the gap between draining and publishing history.
    pub fn snapshot(&self) -> TraceHistorySnapshot {
        let history = self
            .buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut snapshot = self.recorder.snapshot();
        let omitted =
            (history.records.len() + snapshot.records.len()).saturating_sub(self.capacity);
        let records = history
            .records
            .iter()
            .chain(&snapshot.records)
            .skip(omitted)
            .cloned()
            .collect();
        let evicted = history.evicted.checked_add(omitted as u64);
        snapshot.records = records;
        snapshot.capacity = self.capacity;
        snapshot.retained = snapshot.records.len();
        TraceHistorySnapshot {
            snapshot,
            evicted: evicted.unwrap_or(u64::MAX),
            counters_overflowed: history.overflowed || evicted.is_none(),
        }
    }

    pub(super) fn drain(&self) -> RecordingSnapshot {
        let mut history = self
            .buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let snapshot = self.recorder.drain();
        for record in &snapshot.records {
            if history.records.len() == self.capacity {
                history.records.pop_front();
                match history.evicted.checked_add(1) {
                    Some(value) => history.evicted = value,
                    None => history.overflowed = true,
                }
            }
            history.records.push_back(record.clone());
        }
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_api::trace::{SpanGuard, SpanKind, SpanStatus};

    fn record(recorder: &TraceRecorder, target: &str) {
        recorder
            .root()
            .in_scope(|| SpanGuard::current(SpanKind::Application, target).finish(SpanStatus::Ok));
    }

    #[test]
    fn query_does_not_consume_and_export_drain_does_not_erase_history() {
        let recorder = TraceRecorder::new(4).unwrap();
        let history = TraceHistory::new(recorder.clone(), 4);
        record(&recorder, "first");
        let first = history.snapshot();
        assert_eq!(first.snapshot.records.len(), 1);
        assert_eq!(first.snapshot.counts.drained, 0);
        assert_eq!(history.drain().records, first.snapshot.records);
        assert!(recorder.snapshot().records.is_empty());
        record(&recorder, "second");
        let combined = history.snapshot();
        assert_eq!(
            combined
                .snapshot
                .records
                .iter()
                .map(|r| r.target.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(combined.snapshot.counts.drained, 1);
        assert_eq!(history.drain().records.len(), 1);
        assert_eq!(
            history.snapshot().snapshot.records,
            combined.snapshot.records
        );
        assert_eq!(history.snapshot().evicted, 0);
    }

    #[test]
    fn history_eviction_is_bounded_and_distinct_from_recorder_loss() {
        let recorder = TraceRecorder::new(2).unwrap();
        let history = TraceHistory::new(recorder.clone(), 2);
        for target in ["one", "two", "three"] {
            record(&recorder, target);
            history.drain();
        }
        record(&recorder, "four");
        let view = history.snapshot();
        assert_eq!(view.evicted, 2);
        assert_eq!(view.snapshot.counts.overwritten, 0);
        assert_eq!(
            view.snapshot
                .records
                .iter()
                .map(|r| r.target.as_str())
                .collect::<Vec<_>>(),
            ["three", "four"]
        );
        history.drain();
        assert_eq!(history.snapshot().evicted, 2);
        record(&recorder, "five");
        record(&recorder, "six");
        record(&recorder, "seven");
        assert_eq!(history.snapshot().snapshot.counts.overwritten, 1);
    }

    #[test]
    fn out_of_order_completions_keep_parents_and_terminal_receipt() {
        let recorder = TraceRecorder::new(4).unwrap();
        let history = TraceHistory::new(recorder.clone(), 4);
        let root = recorder.root();
        let parent = root.in_scope(|| SpanGuard::current(SpanKind::Application, "parent"));
        root.child().in_scope(|| {
            SpanGuard::current(SpanKind::Application, "child").finish(SpanStatus::Error)
        });
        history.drain();
        parent.finish(SpanStatus::Ok);
        recorder.close();
        let view = history.snapshot();
        assert_eq!(
            view.snapshot
                .records
                .iter()
                .map(|r| r.sequence)
                .collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(
            view.snapshot.records[0].parent_span_id,
            Some(root.span_id())
        );
        assert!(view.snapshot.terminal);
        assert_eq!(view.snapshot.counts.errors, 1);
        assert_eq!(view.snapshot.counts.ok, 1);
        assert_eq!(
            parrot_api::trace::SpanForest::new(&view.snapshot)
                .unwrap()
                .roots()
                .len(),
            1
        );
    }

    #[test]
    fn concurrent_completion_drain_and_query_publish_each_record_once() {
        let recorder = TraceRecorder::new(2048).unwrap();
        let history = TraceHistory::new(recorder.clone(), 2048);
        let (finished, done) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..1000 {
                    record(&recorder, "concurrent");
                }
                recorder.close();
                finished.send(()).unwrap();
            });
            let draining = &history;
            scope.spawn(move || {
                while done.try_recv().is_err() {
                    draining.drain();
                }
                draining.drain();
            });
            loop {
                let view = history.snapshot();
                let unique: std::collections::HashSet<_> = view
                    .snapshot
                    .records
                    .iter()
                    .map(|record| record.sequence)
                    .collect();
                assert_eq!(
                    unique.len(),
                    view.snapshot.records.len(),
                    "drain must not publish duplicates"
                );
                assert_eq!(
                    view.snapshot.records.len() as u64,
                    view.snapshot.counts.completed,
                    "query must not observe a drain/publication gap"
                );
                assert_eq!(view.evicted, 0);
                if view.snapshot.terminal {
                    break;
                }
                std::thread::yield_now();
            }
        });
        let view = history.snapshot();
        assert_eq!(view.snapshot.counts.completed, 1000);
        assert_eq!(view.snapshot.counts.drained, 1000);
        assert_eq!(view.snapshot.records.len(), 1000);
        assert_eq!(view.snapshot.counts.overwritten, 0);
    }
}
