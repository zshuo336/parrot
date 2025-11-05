use parrot_api::trace::{
    RecordingCounts, RecordingSnapshot, SpanGuard, SpanKind, SpanRecord, SpanStatus, TraceRecorder,
};
use parrot_app::debug::{export_otlp_json, TraceExportError, MAX_OTLP_JSON_BYTES};
use serde_json::{json, Value};
use std::time::{Duration, UNIX_EPOCH};
fn snapshot() -> RecordingSnapshot {
    RecordingSnapshot {
        recorder_id: [9; 16],
        origin: UNIX_EPOCH + Duration::new(1_700_000_000, 123),
        elapsed: Duration::from_nanos(20),
        capacity: 8,
        retained: 0,
        closed: true,
        terminal: true,
        counts: RecordingCounts {
            started: 2,
            completed: 2,
            ok: 1,
            errors: 1,
            drained: 2,
            ..Default::default()
        },
        records: vec![
            SpanRecord {
                sequence: 2,
                trace_id: [1; 16],
                span_id: [3; 8],
                parent_span_id: Some([2; 8]),
                kind: SpanKind::DynamicHandler,
                target: "/child\n\"\\汉字".into(),
                target_truncated: false,
                started: Duration::from_nanos(2),
                duration: Duration::from_nanos(3),
                status: SpanStatus::Error,
            },
            SpanRecord {
                sequence: 1,
                trace_id: [1; 16],
                span_id: [2; 8],
                parent_span_id: None,
                kind: SpanKind::Application,
                target: "/root".into(),
                target_truncated: false,
                started: Duration::from_nanos(1),
                duration: Duration::from_nanos(9),
                status: SpanStatus::Ok,
            },
        ],
    }
}
fn export(s: &RecordingSnapshot) -> Value {
    serde_json::from_slice(&export_otlp_json(s, "sample-service", MAX_OTLP_JSON_BYTES).unwrap())
        .unwrap()
}
fn spans(v: &Value) -> &Vec<Value> {
    v["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .unwrap()
}
fn attribute<'a>(v: &'a Value, key: &str) -> &'a Value {
    &v.as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == key)
        .unwrap()["value"]
}
fn resource(v: &Value, key: &str) -> Value {
    attribute(&v["resourceSpans"][0]["resource"]["attributes"], key).clone()
}
fn record_artifact(name: &str, bytes: &[u8]) {
    if let Some(root) = std::env::var_os("PARROT_OTLP_EVIDENCE") {
        let root = std::path::PathBuf::from(root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(name), bytes).unwrap();
    }
}
#[test]
fn official_proto_fixture_preserves_ids_times_types_and_escaped_text() {
    let s = snapshot();
    let bytes = export_otlp_json(&s, "sample-service", MAX_OTLP_JSON_BYTES).unwrap();
    let actual: Value = serde_json::from_slice(&bytes).unwrap();
    let reference: Value =
        serde_json::from_str(include_str!("fixtures/otlp/recording-v1.json")).unwrap();
    assert_eq!(actual, reference);
    assert_eq!(
        spans(&actual)[0]["startTimeUnixNano"],
        "1700000000000000125"
    );
    assert_eq!(spans(&actual)[0]["endTimeUnixNano"], "1700000000000000128");
    record_artifact("canonical.json", &bytes);
}
#[test]
fn exact_budget_succeeds_and_one_byte_less_returns_no_document() {
    let s = snapshot();
    let before = s.records.clone();
    let bytes = export_otlp_json(&s, "sample-service", MAX_OTLP_JSON_BYTES).unwrap();
    assert_eq!(
        export_otlp_json(&s, "sample-service", bytes.len()).unwrap(),
        bytes
    );
    assert!(matches!(
        export_otlp_json(&s, "sample-service", bytes.len() - 1),
        Err(TraceExportError::OutputLimit { .. })
    ));
    assert_eq!(s.records, before);
    assert_eq!(s.counts.drained, 2);
}
#[test]
fn budgets_and_service_names_are_bounded_before_export() {
    let s = snapshot();
    for budget in [0, MAX_OTLP_JSON_BYTES + 1, usize::MAX] {
        assert!(matches!(
            export_otlp_json(&s, "ok", budget),
            Err(TraceExportError::InvalidBudget)
        ));
    }
    for name in [String::new(), "a".repeat(257), "中".repeat(86)] {
        assert!(matches!(
            export_otlp_json(&s, &name, MAX_OTLP_JSON_BYTES),
            Err(TraceExportError::InvalidServiceName)
        ));
    }
    assert!(export_otlp_json(&s, &"a".repeat(256), MAX_OTLP_JSON_BYTES).is_ok());
}
#[test]
fn unobserved_parent_is_preserved_without_synthetic_spans() {
    let mut s = snapshot();
    s.records[1].parent_span_id = Some([7; 8]);
    let value = export(&s);
    assert_eq!(spans(&value).len(), 2);
    assert_eq!(spans(&value)[1]["parentSpanId"], "0707070707070707");
    assert_eq!(
        resource(&value, "parrot.recording.unobserved_parents"),
        json!({"stringValue":"1"})
    );
}
#[test]
fn parent_matching_is_scoped_by_trace_id() {
    let mut s = snapshot();
    s.records[0].trace_id = [8; 16];
    let value = export(&s);
    assert_eq!(
        resource(&value, "parrot.recording.unobserved_parents"),
        json!({"stringValue":"1"})
    );
    s.records[0].span_id = [2; 8];
    s.records[0].parent_span_id = None;
    assert_eq!(spans(&export(&s)).len(), 2);
}
#[test]
fn duplicate_span_identity_is_rejected_without_merging() {
    let mut s = snapshot();
    s.records[0].span_id = [2; 8];
    s.records[0].parent_span_id = None;
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::InvalidGraph)
    ));
}
#[test]
fn cycles_and_self_parent_are_rejected_without_reparenting() {
    let mut s = snapshot();
    s.records[1].parent_span_id = Some([3; 8]);
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::InvalidGraph)
    ));
    s.records[1].parent_span_id = Some([2; 8]);
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::InvalidGraph)
    ));
}
#[test]
fn zero_ids_are_rejected() {
    for which in 0..3 {
        let mut s = snapshot();
        match which {
            0 => s.records[0].trace_id = [0; 16],
            1 => s.records[0].span_id = [0; 8],
            _ => s.records[0].parent_span_id = Some([0; 8]),
        };
        assert!(matches!(
            export_otlp_json(&s, "s", 4096),
            Err(TraceExportError::InvalidIdentity { sequence: 2 })
        ));
    }
    let mut s = snapshot();
    s.recorder_id = [0; 16];
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::InvalidSnapshot(_))
    ));
}
#[test]
fn timestamps_reject_pre_epoch_and_unsigned_overflow() {
    let mut s = snapshot();
    s.origin = UNIX_EPOCH - Duration::from_nanos(1);
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::BeforeEpoch)
    ));
    s.origin = UNIX_EPOCH + Duration::from_nanos(u64::MAX);
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::TimestampOverflow { .. })
    ));
    s.origin = UNIX_EPOCH + Duration::from_secs(u64::MAX / 1_000_000_000 + 1);
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::TimestampOverflow { .. })
    ));
}
#[test]
fn largest_representable_end_timestamp_is_exact() {
    let mut s = snapshot();
    s.origin = UNIX_EPOCH + Duration::from_nanos(u64::MAX - 10);
    let value = export(&s);
    assert_eq!(spans(&value)[1]["endTimeUnixNano"], u64::MAX.to_string());
}
#[test]
fn duration_overflow_and_records_after_snapshot_are_rejected() {
    let mut s = snapshot();
    s.records[0].duration = Duration::MAX;
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::TimestampOverflow { .. })
    ));
    s = snapshot();
    s.elapsed = Duration::from_nanos(9);
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::InvalidSnapshot(_))
    ));
}
#[test]
fn forged_receipts_and_out_of_bound_buffers_are_rejected() {
    for which in 0..7 {
        let mut s = snapshot();
        match which {
            0 => s.terminal = false,
            1 => s.counts.completed += 1,
            2 => s.counts.started += 1,
            3 => s.counts.drained = 0,
            4 => s.retained = 1,
            5 => s.records[0].sequence = 0,
            _ => s.records[0].sequence = 3,
        };
        assert!(
            matches!(
                export_otlp_json(&s, "s", 4096),
                Err(TraceExportError::InvalidSnapshot(_))
            ),
            "variant {which}"
        );
    }
    let mut s = snapshot();
    s.capacity = 1;
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::Forest(_))
    ));
    s = snapshot();
    s.records[0].target = "x".repeat(257);
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::Forest(_))
    ));
}
#[test]
fn all_statuses_and_kinds_keep_their_recorded_meaning() {
    for (status, label) in [
        (SpanStatus::Ok, "ok"),
        (SpanStatus::Error, "error"),
        (SpanStatus::Panicked, "panicked"),
        (SpanStatus::Cancelled, "cancelled"),
    ] {
        let r = TraceRecorder::new(16).unwrap();
        let kinds = [
            (SpanKind::TypedHandler, "typed_handler"),
            (SpanKind::DynamicHandler, "dynamic_handler"),
            (SpanKind::BusinessBatch, "business_batch"),
            (SpanKind::StreamStarted, "stream_started"),
            (SpanKind::StreamItem, "stream_item"),
            (SpanKind::StreamFinished, "stream_finished"),
            (SpanKind::StreamError, "stream_error"),
            (SpanKind::Application, "application"),
        ];
        for (kind, _) in kinds {
            r.root()
                .in_scope(|| SpanGuard::current(kind, "").finish(status));
        }
        r.close();
        let value = export(&r.drain());
        assert_eq!(spans(&value).len(), 8);
        for (span, (_, kind_label)) in spans(&value).iter().zip(kinds) {
            assert_eq!(span["kind"], 1);
            assert_eq!(span["name"], kind_label);
            assert_eq!(
                attribute(&span["attributes"], "parrot.span.kind"),
                &json!({"stringValue":kind_label})
            );
            assert_eq!(
                attribute(&span["attributes"], "parrot.span.target"),
                &json!({"stringValue":""})
            );
            assert_eq!(
                attribute(&span["attributes"], "parrot.span.status"),
                &json!({"stringValue":label})
            );
            assert_eq!(
                span["status"]["code"],
                if status == SpanStatus::Ok { 1 } else { 2 }
            );
            if status == SpanStatus::Ok {
                assert!(span["status"].get("message").is_none());
            } else {
                assert_eq!(span["status"]["message"], label);
            }
        }
    }
}
#[test]
fn overwritten_truncated_and_rejected_records_are_visible() {
    let r = TraceRecorder::new(2).unwrap();
    for _ in 0..3 {
        r.root().in_scope(|| {
            SpanGuard::current(SpanKind::Application, &"中".repeat(100)).finish(SpanStatus::Ok)
        });
    }
    r.close();
    r.root()
        .in_scope(|| SpanGuard::current(SpanKind::Application, "late").finish(SpanStatus::Ok));
    let s = r.snapshot();
    let v = export(&s);
    assert_eq!(spans(&v).len(), 2);
    for (key, n) in [
        ("overwritten", "1"),
        ("targets_truncated", "3"),
        ("rejected_after_close", "1"),
        ("retained", "2"),
    ] {
        assert_eq!(
            resource(&v, &format!("parrot.recording.{key}")),
            json!({"stringValue":n})
        );
    }
    assert_eq!(r.snapshot().records, s.records);
    assert_eq!(r.snapshot().counts, s.counts);
    for span in spans(&v) {
        assert_eq!(
            attribute(&span["attributes"], "parrot.span.target_truncated"),
            &json!({"boolValue":true})
        );
    }
}
#[test]
fn open_and_closed_inflight_snapshots_do_not_claim_terminal() {
    let r = TraceRecorder::new(2).unwrap();
    let owner = r
        .root()
        .in_scope(|| SpanGuard::current(SpanKind::Application, "pending"));
    let v = export(&r.snapshot());
    assert_eq!(
        resource(&v, "parrot.recording.terminal"),
        json!({"boolValue":false})
    );
    assert!(spans(&v).is_empty());
    r.close();
    let v = export(&r.snapshot());
    assert_eq!(
        resource(&v, "parrot.recording.closed"),
        json!({"boolValue":true})
    );
    assert_eq!(
        resource(&v, "parrot.recording.terminal"),
        json!({"boolValue":false})
    );
    drop(owner);
    let v = export(&r.drain());
    assert_eq!(
        resource(&v, "parrot.recording.terminal"),
        json!({"boolValue":true})
    );
    assert_eq!(spans(&v)[0]["status"]["message"], "cancelled");
}
#[test]
fn large_unsigned_counters_are_lossless_strings_with_saturation_diagnostics() {
    let mut s = snapshot();
    s.counts.counters_overflowed = true;
    s.counts.started = u64::MAX;
    s.counts.completed = u64::MAX;
    s.counts.overwritten = u64::MAX;
    s.records[0].sequence = 9_007_199_254_740_993;
    let v = export(&s);
    assert_eq!(
        resource(&v, "parrot.recording.started"),
        json!({"stringValue":u64::MAX.to_string()})
    );
    assert_eq!(
        resource(&v, "parrot.recording.counters_overflowed"),
        json!({"boolValue":true})
    );
    assert_eq!(
        attribute(&spans(&v)[0]["attributes"], "parrot.span.sequence"),
        &json!({"stringValue":"9007199254740993"})
    );
}
#[test]
fn draining_twice_exports_no_duplicate_records() {
    let r = TraceRecorder::new(8).unwrap();
    r.root()
        .in_scope(|| SpanGuard::current(SpanKind::Application, "work").finish(SpanStatus::Ok));
    r.close();
    let first = r.drain();
    let second = r.drain();
    assert_eq!(spans(&export(&first)).len(), 1);
    assert!(spans(&export(&second)).is_empty());
    assert_eq!(
        resource(&export(&second), "parrot.recording.drained"),
        json!({"stringValue":"1"})
    );
}

#[test]
fn records_must_fit_outcome_counters_even_when_accounting_saturated() {
    for saturated in [false, true] {
        for variant in 0..5 {
            let mut s = snapshot();
            s.counts.counters_overflowed = saturated;
            match variant {
                0 => s.records[0].status = SpanStatus::Ok,
                1 => s.records[0].status = SpanStatus::Panicked,
                2 => s.records[0].status = SpanStatus::Cancelled,
                3 => s.records[0].target_truncated = true,
                _ => s.records[0].sequence = s.records[1].sequence,
            }
            assert!(
                matches!(
                    export_otlp_json(&s, "s", 4096),
                    Err(TraceExportError::InvalidSnapshot(_))
                ),
                "variant {variant}, saturated {saturated}"
            );
        }
    }
}

#[test]
fn only_saturated_maximum_admission_sequences_can_repeat() {
    let mut s = snapshot();
    s.counts.counters_overflowed = true;
    s.counts.started = u64::MAX;
    for record in &mut s.records {
        record.sequence = u64::MAX;
    }
    let v = export(&s);
    for span in spans(&v) {
        assert_eq!(
            attribute(&span["attributes"], "parrot.span.sequence"),
            &json!({"stringValue":u64::MAX.to_string()})
        );
    }
    s.counts.started = u64::MAX - 1;
    assert!(matches!(
        export_otlp_json(&s, "s", 4096),
        Err(TraceExportError::InvalidSnapshot(_))
    ));
}

#[test]
fn maximum_capacity_deep_chain_exports_iteratively_with_every_parent_intact() {
    use parrot_api::trace::MAX_RECORD_CAPACITY;
    let mut s = snapshot();
    s.capacity = MAX_RECORD_CAPACITY;
    let total = MAX_RECORD_CAPACITY as u64;
    s.counts = RecordingCounts {
        started: total,
        completed: total,
        ok: total,
        drained: total,
        ..Default::default()
    };
    s.records = (1..=total)
        .map(|sequence| SpanRecord {
            sequence,
            trace_id: [1; 16],
            span_id: sequence.to_be_bytes(),
            parent_span_id: (sequence > 1).then(|| (sequence - 1).to_be_bytes()),
            kind: SpanKind::Application,
            target: "deep".into(),
            target_truncated: false,
            started: Duration::ZERO,
            duration: Duration::from_nanos(1),
            status: SpanStatus::Ok,
        })
        .collect();
    let bytes = export_otlp_json(&s, "deep-chain", MAX_OTLP_JSON_BYTES).unwrap();
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Document {
        resource_spans: Vec<Resource>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Resource {
        scope_spans: Vec<Scope>,
    }
    #[derive(serde::Deserialize)]
    struct Scope {
        spans: Vec<Span>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Span {
        span_id: String,
        parent_span_id: Option<String>,
    }
    let decoded: Document = serde_json::from_slice(&bytes).unwrap();
    let spans = &decoded.resource_spans[0].scope_spans[0].spans;
    assert_eq!(spans.len(), MAX_RECORD_CAPACITY);
    for (index, span) in spans.iter().enumerate() {
        assert_eq!(span.span_id, format!("{:016x}", index + 1));
        assert_eq!(
            span.parent_span_id,
            (index > 0).then(|| format!("{index:016x}"))
        );
    }
    assert!(matches!(
        export_otlp_json(&s, "deep-chain", 1024),
        Err(TraceExportError::OutputLimit { limit: 1024 })
    ));
    assert_eq!(s.records.len(), MAX_RECORD_CAPACITY);
}

struct Probe;
impl parrot_api::actor::Actor for Probe {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: parrot_api::types::BoxedMessage,
        _: &'a mut Self::Context,
    ) -> parrot_api::types::BoxedFuture<
        'a,
        parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
    > {
        Box::pin(async move { Ok(message) })
    }
}
#[tokio::test]
async fn actual_thread_mailbox_record_exports_the_same_causal_identity() {
    use parrot_api::address::ActorRef;
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    let actor = system
        .spawn_root_typed_thread(Probe, parrot_api::actor::EmptyConfig)
        .await
        .unwrap();
    let r = TraceRecorder::new(8).unwrap();
    let root = r.root();
    let reply = root
        .scope(async { actor.send(Box::new(7u64)).await })
        .await
        .unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 7);
    actor.stop().await.unwrap();
    system.shutdown_internal().await.unwrap();
    r.close();
    let s = r.drain();
    assert_eq!(s.records.len(), 1);
    assert_eq!(s.records[0].trace_id, root.trace_id());
    assert_eq!(s.records[0].parent_span_id, Some(root.span_id()));
    let bytes = export_otlp_json(&s, "real-mailbox", MAX_OTLP_JSON_BYTES).unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(spans(&v).len(), 1);
    assert_eq!(spans(&v)[0]["status"]["code"], 1);
    assert_eq!(
        attribute(&spans(&v)[0]["attributes"], "parrot.span.kind"),
        &json!({"stringValue":"dynamic_handler"})
    );
    record_artifact("thread-mailbox.json", &bytes);
}
