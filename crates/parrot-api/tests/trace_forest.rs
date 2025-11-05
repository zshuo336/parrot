use parrot_api::trace::{
    ForestError, ParentRelation, SpanForest, SpanGuard, SpanKind, SpanStatus, TraceRecorder,
    MAX_FOREST_TEXT_BYTES, MAX_RECORD_CAPACITY,
};
use std::time::Duration;

fn sample(count: usize) -> parrot_api::trace::RecordingSnapshot {
    let recorder = TraceRecorder::new(count.max(1)).unwrap();
    let root = recorder.root();
    for _ in 0..count {
        root.child().in_scope(|| {
            SpanGuard::current(SpanKind::Application, "/sample").finish(SpanStatus::Ok)
        });
    }
    recorder.close();
    recorder.drain()
}

#[test]
fn real_nested_completion_order_becomes_parent_first_tree() {
    let recorder = TraceRecorder::new(4).unwrap();
    let root = recorder.root();
    let child = root.child();
    root.in_scope(|| {
        let parent = SpanGuard::current(SpanKind::Application, "/root");
        child.in_scope(|| {
            let owner = SpanGuard::current(SpanKind::Application, "/child");
            child.child().in_scope(|| {
                SpanGuard::current(SpanKind::Application, "/grandchild").finish(SpanStatus::Error)
            });
            owner.finish(SpanStatus::Ok);
        });
        parent.finish(SpanStatus::Ok);
    });
    recorder.close();
    let snapshot = recorder.drain();
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|r| r.sequence)
            .collect::<Vec<_>>(),
        [3, 2, 1]
    );
    let forest = SpanForest::new(&snapshot).unwrap();
    assert!(std::ptr::eq(forest.snapshot(), &snapshot));
    assert_eq!(forest.roots(), [0]);
    assert_eq!(forest.nodes()[0].parent(), ParentRelation::Root);
    assert_eq!(forest.nodes()[0].children(), [1]);
    assert_eq!(forest.nodes()[1].parent(), ParentRelation::Recorded(0));
    assert_eq!(forest.nodes()[1].children(), [2]);
    assert_eq!(forest.nodes()[2].parent(), ParentRelation::Recorded(1));
    assert_eq!(forest.nodes()[2].record().status, SpanStatus::Error);
    let view = forest.render_text(4096).unwrap();
    let targets: Vec<_> = view
        .text
        .lines()
        .filter(|l| l.contains("target="))
        .collect();
    assert!(targets[0].contains("target=\"/root\""));
    assert!(targets[1].contains("target=\"/child\""));
    assert!(targets[2].contains("target=\"/grandchild\""));
    assert_eq!((view.emitted_records, view.omitted_records), (3, 0));
}

#[test]
fn parent_matching_includes_trace_namespace() {
    let mut snapshot = sample(4);
    for (index, record) in snapshot.records.iter_mut().enumerate() {
        record.trace_id = [(index / 2) as u8; 16];
        record.span_id = [(index % 2) as u8; 8];
        record.parent_span_id = (index % 2 == 1).then_some([0; 8]);
        record.started = Duration::from_nanos(index as u64);
    }
    let forest = SpanForest::new(&snapshot).unwrap();
    assert_eq!(forest.roots(), [0, 2]);
    assert_eq!(forest.nodes()[1].parent(), ParentRelation::Recorded(0));
    assert_eq!(forest.nodes()[3].parent(), ParentRelation::Recorded(2));
    assert_eq!(forest.diagnostics().reused_identity_records, 0);
}

#[test]
fn unrecorded_root_is_boundary_not_invented_parent() {
    let snapshot = sample(2);
    let forest = SpanForest::new(&snapshot).unwrap();
    assert_eq!(forest.nodes().len(), 2);
    assert_eq!(forest.roots().len(), 2);
    assert!(forest
        .nodes()
        .iter()
        .all(|n| n.parent() == ParentRelation::Unobserved));
    assert_eq!(forest.diagnostics().unobserved_parent_records, 2);
    assert!(forest
        .render_text(4096)
        .unwrap()
        .text
        .contains("[unobserved]"));
}

#[test]
fn reused_parent_identity_preserves_every_occurrence_without_guessing() {
    let mut snapshot = sample(3);
    for (index, record) in snapshot.records.iter_mut().enumerate() {
        record.trace_id = [7; 16];
        record.span_id = if index < 2 { [1; 8] } else { [2; 8] };
        record.parent_span_id = (index == 2).then_some([1; 8]);
        record.started = Duration::from_nanos(index as u64);
    }
    let forest = SpanForest::new(&snapshot).unwrap();
    assert_eq!(forest.nodes().len(), 3);
    assert_eq!(forest.roots(), [0, 1, 2]);
    assert_eq!(forest.nodes()[2].parent(), ParentRelation::Ambiguous);
    assert_eq!(forest.diagnostics().reused_identity_records, 2);
    assert_eq!(forest.diagnostics().ambiguous_parent_records, 1);
    assert_eq!(forest.render_text(4096).unwrap().emitted_records, 3);
}

#[test]
fn every_cycle_edge_is_classified_and_descendants_remain_reachable() {
    let mut snapshot = sample(5);
    for (index, record) in snapshot.records.iter_mut().enumerate() {
        record.trace_id = [1; 16];
        record.span_id = [index as u8; 8];
        record.parent_span_id = Some(
            [match index {
                0 => 1,
                1 => 0,
                2 => 2,
                3 => 1,
                _ => 3,
            }; 8],
        );
        record.started = Duration::from_nanos(index as u64);
    }
    let forest = SpanForest::new(&snapshot).unwrap();
    assert_eq!(forest.diagnostics().cycle_records, 3);
    assert_eq!(forest.roots(), [0, 1, 2]);
    for node in &forest.nodes()[..3] {
        assert_eq!(node.parent(), ParentRelation::Cycle);
    }
    assert_eq!(forest.nodes()[1].children(), [3]);
    assert_eq!(forest.nodes()[3].children(), [4]);
    let rendered = forest.render_text(8192).unwrap();
    assert_eq!(rendered.emitted_records, 5);
    assert_eq!(
        rendered
            .text
            .lines()
            .filter(|l| l.contains("[cycle]"))
            .count(),
        3
    );
}

#[test]
fn maximum_depth_neither_recurses_nor_allocates_quadratic_indentation() {
    let mut snapshot = sample(MAX_RECORD_CAPACITY);
    for (index, record) in snapshot.records.iter_mut().enumerate() {
        record.trace_id = [8; 16];
        record.span_id = (index as u64).to_be_bytes();
        record.parent_span_id = index.checked_sub(1).map(|n| (n as u64).to_be_bytes());
        record.started = Duration::from_nanos(index as u64);
        record.target = "/deep".into();
    }
    let forest = SpanForest::new(&snapshot).unwrap();
    assert_eq!(forest.roots(), [0]);
    assert_eq!(forest.nodes().len(), MAX_RECORD_CAPACITY);
    assert_eq!(
        forest.nodes().last().unwrap().parent(),
        ParentRelation::Recorded(MAX_RECORD_CAPACITY - 2)
    );
    let rendered = forest.render_text(MAX_FOREST_TEXT_BYTES).unwrap();
    assert!(rendered.emitted_records > 1000);
    assert!(rendered.omitted_records > 0);
    assert_eq!(
        rendered.emitted_records + rendered.omitted_records,
        MAX_RECORD_CAPACITY
    );
    assert!(rendered.text.len() <= MAX_FOREST_TEXT_BYTES);
    assert!(rendered.text.lines().all(|l| l.len() < 1024));
}

#[test]
fn output_is_byte_bounded_and_never_contains_a_partial_record() {
    let mut snapshot = sample(8);
    for record in &mut snapshot.records {
        record.target = "汉".repeat(85);
    }
    let forest = SpanForest::new(&snapshot).unwrap();
    let short = forest.render_text(2048).unwrap();
    assert!(short.text.len() <= 2048);
    assert!(short.emitted_records > 0 && short.omitted_records > 0);
    assert_eq!(
        short.text.lines().filter(|l| l.contains("target=")).count(),
        short.emitted_records
    );
    assert!(short.text.ends_with('\n'));
    assert!(short
        .text
        .contains(&format!("omitted={}", short.omitted_records)));
    assert!(matches!(
        forest.render_text(0),
        Err(ForestError::TextBudgetTooSmall { .. })
    ));
    assert!(matches!(
        forest.render_text(MAX_FOREST_TEXT_BYTES + 1),
        Err(ForestError::TextBudgetTooLarge)
    ));
}

#[test]
fn terminal_controls_are_escaped_without_changing_record_data() {
    let mut snapshot = sample(1);
    let hostile = "target\nforged-root\r\u{1b}[2J\t\0\u{202e}";
    snapshot.records[0].target = hostile.into();
    let forest = SpanForest::new(&snapshot).unwrap();
    let view = forest.render_text(4096).unwrap();
    assert_eq!(forest.nodes()[0].record().target, hostile);
    assert_eq!(view.text.lines().count(), 4);
    assert!(!view.text.contains(['\r', '\u{1b}', '\t', '\0', '\u{202e}']));
    assert!(view.text.contains("target\\nforged-root"));
}

#[test]
fn actual_loss_and_closed_admission_receipt_survives_projection() {
    let recorder = TraceRecorder::new(1).unwrap();
    let root = recorder.root();
    for _ in 0..3 {
        root.child()
            .in_scope(|| SpanGuard::current(SpanKind::Application, "/loss").finish(SpanStatus::Ok));
    }
    recorder.drain();
    root.child()
        .in_scope(|| SpanGuard::current(SpanKind::Application, "/later").finish(SpanStatus::Error));
    recorder.close();
    root.child()
        .in_scope(|| SpanGuard::current(SpanKind::Application, "/rejected").finish(SpanStatus::Ok));
    let snapshot = recorder.snapshot();
    let forest = SpanForest::new(&snapshot).unwrap();
    assert_eq!(forest.snapshot().counts.overwritten, 2);
    assert_eq!(forest.snapshot().counts.drained, 1);
    assert_eq!(forest.snapshot().counts.rejected_after_close, 1);
    let rendered = forest.render_text(4096).unwrap();
    for expected in [
        "terminal=true",
        "completed=4",
        "overwritten=2",
        "drained=1",
        "rejected_after_close=1",
        "unobserved_parent_records=1",
    ] {
        assert!(
            rendered.text.contains(expected),
            "missing {expected}: {}",
            rendered.text
        );
    }
}

#[test]
fn public_snapshot_cannot_bypass_resource_bounds() {
    let mut snapshot = sample(2);
    snapshot.capacity = 1;
    assert!(matches!(
        SpanForest::new(&snapshot),
        Err(ForestError::InvalidCapacity)
    ));
    snapshot.capacity = MAX_RECORD_CAPACITY + 1;
    assert!(matches!(
        SpanForest::new(&snapshot),
        Err(ForestError::InvalidCapacity)
    ));
    snapshot.capacity = 2;
    snapshot.records[0].target = "x".repeat(257);
    assert!(matches!(
        SpanForest::new(&snapshot),
        Err(ForestError::TargetTooLong { .. })
    ));
}

#[test]
fn completion_permutation_does_not_change_unique_identity_tree_or_text() {
    let mut snapshot = sample(3);
    for (index, record) in snapshot.records.iter_mut().enumerate() {
        record.trace_id = [5; 16];
        record.span_id = [index as u8; 8];
        record.parent_span_id = (index > 0).then_some([0; 8]);
        record.started = Duration::from_nanos(index as u64);
    }
    let original = SpanForest::new(&snapshot)
        .unwrap()
        .render_text(4096)
        .unwrap()
        .text;
    let rendered_sequences: Vec<_> = original
        .lines()
        .filter_map(|line| {
            line.trim_start()
                .strip_prefix("seq=")
                .map(|rest| rest.split_whitespace().next().unwrap())
        })
        .collect();
    assert_eq!(rendered_sequences, ["1", "2", "3"]);
    snapshot.records.reverse();
    let reversed = SpanForest::new(&snapshot)
        .unwrap()
        .render_text(4096)
        .unwrap()
        .text;
    assert_eq!(original, reversed);
}

#[test]
fn empty_snapshot_preserves_receipt_without_inventing_root() {
    let snapshot = sample(0);
    let forest = SpanForest::new(&snapshot).unwrap();
    assert!(forest.nodes().is_empty() && forest.roots().is_empty());
    let text = forest.render_text(4096).unwrap();
    assert_eq!((text.emitted_records, text.omitted_records), (0, 0));
    assert_eq!(text.text.lines().count(), 3);
}
