//! Export records from actual durable transactions, including reopen and cancellation.
use bytes::Bytes;
use parrot_api::trace::{SpanKind, SpanStatus, TraceContext, TraceRecorder};
use parrot_app::debug::{export_otlp_json, MAX_OTLP_JSON_BYTES};
use parrot_remote::durable::{DurableEnvelope, InboxError, InboxLimits, TransactionalInbox};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

fn message(sequence: u64, key: &str) -> DurableEnvelope {
    DurableEnvelope {
        source: "source".into(),
        session: "session".into(),
        receiver: "receiver".into(),
        path: "/state".into(),
        sequence,
        type_key: key.into(),
        payload: Bytes::new(),
    }
}
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .unwrap()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn actual_transactions_export_outcomes_and_reopen_does_not_replay_old_identity() {
    bounded(async {
        let directory = tempfile::tempdir().unwrap();
        let recorder = TraceRecorder::new(16).unwrap();
        let first = recorder.root();
        let second = recorder.root();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let limits = InboxLimits { max_state_bytes: 8, ..Default::default() };
        let mut inbox = TransactionalInbox::open(directory.path(), "receiver", "/state", Bytes::from_static(b"initial"), limits, move |_, message| {
            observed.lock().unwrap().push(TraceContext::current().unwrap());
            match message.type_key.as_str() {
                "error" => Err("business refused".into()),
                "panic" => panic!("intentional transition panic"),
                "oversize" => Ok(Bytes::from_static(b"too large")),
                _ => Ok(Bytes::from_static(b"saved")),
            }
        }).await.unwrap();
        let handle = inbox.handle();
        for key in ["error", "panic", "oversize"] {
            assert!(first.in_scope(|| handle.apply(message(1, key))).await.is_err());
            assert_eq!(handle.snapshot().revision, 0);
            assert_eq!(&handle.snapshot().state[..], b"initial");
        }
        first.in_scope(|| handle.apply(message(1, "ok"))).await.unwrap();
        inbox.close().await.unwrap();
        drop(inbox);

        let observed = calls.clone();
        let mut reopened = TransactionalInbox::open(directory.path(), "receiver", "/state", Bytes::from_static(b"ignored"), limits, move |state, _| {
            observed.lock().unwrap().push(TraceContext::current().unwrap());
            assert_eq!(state, b"saved");
            Ok(Bytes::from_static(b"next"))
        }).await.unwrap();
        let handle = reopened.handle();
        second.in_scope(|| handle.apply(message(1, "duplicate"))).await.unwrap();
        assert_eq!(calls.lock().unwrap().len(), 4);
        second.in_scope(|| handle.apply(message(2, "next"))).await.unwrap();
        reopened.close().await.unwrap();
        assert_eq!(handle.snapshot().revision, 2);
        assert_eq!(&handle.snapshot().state[..], b"next");

        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.overwritten, 0);
        assert_eq!(snapshot.records.len(), 5);
        let calls = calls.lock().unwrap();
        let statuses = [SpanStatus::Error, SpanStatus::Panicked, SpanStatus::Error, SpanStatus::Ok, SpanStatus::Ok];
        for (index, ((record, call), status)) in snapshot.records.iter().zip(calls.iter()).zip(statuses).enumerate() {
            let parent = if index < 4 { &first } else { &second };
            assert_eq!(record.kind, SpanKind::InboxTransaction);
            assert_eq!(record.status, status);
            assert_eq!(record.trace_id, parent.trace_id());
            assert_eq!(record.parent_span_id, Some(parent.span_id()));
            assert_eq!(record.span_id, call.span_id());
            assert_ne!(record.span_id, parent.span_id());
        }
        let bytes = export_otlp_json(&snapshot, "inbox-service", MAX_OTLP_JSON_BYTES).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let spans = document["resourceSpans"][0]["scopeSpans"][0]["spans"].as_array().unwrap();
        let expected: Vec<_> = snapshot.records.iter().map(|r| serde_json::json!({"trace": hex(&r.trace_id), "span": hex(&r.span_id), "parent": hex(&r.parent_span_id.unwrap()), "status": format!("{:?}",r.status)})).collect();
        assert_eq!(spans.len(), expected.len());
        for (span, expected) in spans.iter().zip(&expected) {
            assert_eq!(span["traceId"], expected["trace"]);
            assert_eq!(span["spanId"], expected["span"]);
            assert_eq!(span["parentSpanId"], expected["parent"]);
            assert!(span["attributes"].as_array().unwrap().iter().any(|a| a["key"] == "parrot.span.kind" && a["value"]["stringValue"] == "inbox_transaction"));
        }
        if let Some(path) = std::env::var_os("PARROT_INBOX_EXPORT_EVIDENCE") {
            let path = std::path::PathBuf::from(path);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("transactions.otlp.json"), bytes).unwrap();
            std::fs::write(path.join("expected.json"), serde_json::to_vec_pretty(&expected).unwrap()).unwrap();
        }
    }).await;
}

#[tokio::test]
async fn queued_cancellation_releases_admission_without_executing_or_recording() {
    bounded(async {
        let directory = tempfile::tempdir().unwrap();
        let recorder = TraceRecorder::new(8).unwrap();
        let root = recorder.root();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let entered = Mutex::new(Some(entered_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut inbox = TransactionalInbox::open(
            directory.path(),
            "receiver",
            "/state",
            Bytes::new(),
            InboxLimits {
                command_capacity: 1,
                ..Default::default()
            },
            move |_, message| {
                observed.lock().unwrap().push(message.sequence);
                if message.sequence == 1 {
                    entered.lock().unwrap().take().unwrap().send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                Ok(Bytes::from_static(b"committed"))
            },
        )
        .await
        .unwrap();
        let handle = inbox.handle();
        let mut first = Box::pin(root.in_scope(|| handle.apply(message(1, "first"))));
        assert!(futures::poll!(&mut first).is_pending());
        entered_rx.await.unwrap();
        let mut queued = Box::pin(root.in_scope(|| handle.apply(message(2, "cancelled"))));
        assert!(futures::poll!(&mut queued).is_pending());
        assert!(matches!(
            handle.apply(message(3, "full")).await,
            Err(InboxError::Capacity)
        ));
        drop(queued);
        release_tx.send(()).unwrap();
        first.await.unwrap();
        // Retry the cancelled sequence. Capacity is transient while the worker
        // retires the queued receipt; retrying must never advance the watermark.
        loop {
            match root.in_scope(|| handle.apply(message(2, "retry"))).await {
                Err(InboxError::Capacity) => tokio::task::yield_now().await,
                result => {
                    result.unwrap();
                    break;
                }
            }
        }
        inbox.close().await.unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![1, 2]);
        assert_eq!(handle.snapshot().revision, 2);
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.records.len(), 2);
        assert!(snapshot
            .records
            .iter()
            .all(|r| r.status == SpanStatus::Ok && r.parent_span_id == Some(root.span_id())));
    })
    .await;
}
