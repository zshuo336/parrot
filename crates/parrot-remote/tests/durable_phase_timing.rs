use bytes::Bytes;
use parrot_remote::{durable::*, LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
#[path = "support/durable_timing.rs"]
mod diagnostic;
use diagnostic::{observer, Capture, Fields};
fn events(capture: &Capture, source: &str) -> Vec<Fields> {
    capture
        .events()
        .into_iter()
        .filter(|row| {
            ["source", "local_node", "peer_node"]
                .iter()
                .any(|key| row.get(*key).and_then(Value::as_str) == Some(source))
        })
        .collect()
}
fn unique() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        "timing-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(unique());
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Lookup(Option<Arc<dyn DurableReceiver>>);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
    fn durable_receiver(&self, path: &str) -> Option<Arc<dyn DurableReceiver>> {
        (path == "/state").then(|| self.0.clone()).flatten()
    }
}
struct Gate {
    inbox: InboxHandle,
    first: AtomicBool,
    started: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}
#[async_trait::async_trait]
impl DurableReceiver for Gate {
    async fn receive(&self, message: DurableEnvelope) -> Result<(), InboxError> {
        if !self.first.swap(true, Ordering::SeqCst) {
            self.started.add_permits(1);
            self.release.acquire().await.unwrap().forget();
        }
        self.inbox.apply(message).await
    }
}
async fn run(
    tcp: bool,
    gate: bool,
    reject: bool,
    second_stream: bool,
    collect: bool,
) -> (Vec<Fields>, OutboxStatus) {
    let capture = if collect {
        observer()
    } else {
        Arc::new(Capture::default())
    };
    let directory = Scratch::new();
    let source_id = unique();
    let target_id = unique();
    let mut inbox = TransactionalInbox::open(
        directory.0.join("inbox"),
        &target_id,
        "/state",
        Bytes::new(),
        InboxLimits::default(),
        move |_, message| {
            if reject {
                Err("rejected business transition".into())
            } else {
                Ok(message.payload.clone())
            }
        },
    )
    .await
    .unwrap();
    let blocker = Arc::new(Gate {
        inbox: inbox.handle(),
        first: AtomicBool::new(false),
        started: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
    let receiver: Arc<dyn DurableReceiver> = if gate {
        blocker.clone()
    } else {
        Arc::new(inbox.handle())
    };
    let config = |id: &str, listen| {
        if tcp {
            RemoteConfig::tcp(id, listen).with_insecure_development()
        } else {
            RemoteConfig::mem(id)
        }
    };
    let target = RemoteActorSystem::new(
        config(&target_id, Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Lookup(Some(receiver))),
    )
    .unwrap();
    let source = RemoteActorSystem::new(config(&source_id, None), Arc::new(Lookup(None))).unwrap();
    target.start().await.unwrap();
    source.start().await.unwrap();
    let addr = if tcp {
        NodeAddr::tcp(&target_id, target.local_addr().unwrap())
    } else {
        NodeAddr::mem(&target_id)
    };
    if !tcp {
        source.connect_mem_pair(&target).await.unwrap();
    }
    let mut owner = DurableOutbox::open(
        directory.0.join("outbox"),
        &source,
        addr.clone(),
        "/state",
        "stream",
        OutboxConfig {
            transactional: true,
            group_commit_max_records: 1,
            group_commit_interval: Duration::ZERO,
            attempt_timeout: Duration::from_millis(100),
            retry_min: Duration::from_millis(10),
            retry_max: Duration::from_millis(30),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let handle = owner.handle();
    assert_eq!(
        handle
            .enqueue("bin:PRIVATE_TYPE_KEY#v1", b"PRIVATE_PAYLOAD_BYTES")
            .await
            .unwrap(),
        1
    );
    if gate {
        tokio::time::timeout(Duration::from_secs(10), blocker.started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }
    if gate || reject {
        tokio::time::timeout(Duration::from_secs(10), async {
            while handle.status().last_error.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    if gate {
        blocker.release.add_permits(1);
    }
    if !reject {
        tokio::time::timeout(Duration::from_secs(10), handle.wait_acked(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inbox.handle().snapshot().revision, 1);
        assert_eq!(
            &inbox.handle().snapshot().state[..],
            b"PRIVATE_PAYLOAD_BYTES"
        );
    } else {
        assert_eq!(inbox.handle().snapshot().revision, 0);
        assert_eq!(handle.status().last_acked, 0);
    }
    if second_stream {
        let mut other = DurableOutbox::open(
            directory.0.join("other-outbox"),
            &source,
            addr,
            "/state",
            "other-stream",
            OutboxConfig {
                transactional: true,
                group_commit_max_records: 1,
                group_commit_interval: Duration::ZERO,
                attempt_timeout: Duration::from_millis(100),
                retry_min: Duration::from_millis(10),
                retry_max: Duration::from_millis(30),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            other
                .handle()
                .enqueue("bin:PRIVATE_TYPE_KEY#v1", b"PRIVATE_PAYLOAD_BYTES")
                .await
                .unwrap(),
            1
        );
        tokio::time::timeout(Duration::from_secs(10), other.handle().wait_acked(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inbox.handle().snapshot().revision, 2);
        other.close().await.unwrap();
    }
    let status = handle.status();
    owner.close().await.unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
    inbox.close().await.unwrap();
    assert!(source.callbacks.is_empty());
    let captured = events(&capture, &source_id);
    let rendered = serde_json::to_string(&captured).unwrap();
    assert!(!rendered.contains("PRIVATE_TYPE_KEY") && !rendered.contains("PRIVATE_PAYLOAD_BYTES"));
    if let Some(root) = std::env::var_os("PARROT_TIMING_EVIDENCE") {
        let root = PathBuf::from(root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(format!("{source_id}.json")),
            serde_json::to_vec_pretty(&captured).unwrap(),
        )
        .unwrap();
    }
    (captured, status)
}
fn phase<'a>(rows: &'a [Fields], name: &str, boundary: &str) -> Vec<(usize, &'a Fields)> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| {
            r.get("phase").and_then(Value::as_str) == Some(name)
                && r.get("boundary").and_then(Value::as_str) == Some(boundary)
        })
        .collect()
}
struct MeasuredPhase<'a> {
    start: usize,
    finish: usize,
    fields: &'a Fields,
}
fn paired<'a>(rows: &'a [Fields], name: &str) -> Vec<MeasuredPhase<'a>> {
    let starts = phase(rows, name, "start");
    let finishes = phase(rows, name, "finish");
    assert_eq!(starts.len(), finishes.len(), "unbalanced {name}");
    let mut spans = std::collections::HashSet::new();
    starts
        .into_iter()
        .map(|(start, fields)| {
            let id = fields["diagnostic_span_id"].as_u64().unwrap();
            assert!(spans.insert(id), "duplicate {name} start in span {id}");
            let matching: Vec<_> = finishes
                .iter()
                .filter(|(_, finish)| finish["diagnostic_span_id"] == id)
                .collect();
            assert_eq!(matching.len(), 1, "{name} span {id}");
            let (finish, terminal) = *matching[0];
            assert!(start < finish, "{name} finished before it started");
            assert!(terminal["elapsed_us"].is_u64());
            assert!(matches!(
                terminal["outcome"].as_str(),
                Some("ok" | "error" | "unfinished")
            ));
            for key in [
                "source", "session", "receiver", "path", "sequence", "attempt", "cid",
            ] {
                if let Some(value) = fields.get(key) {
                    assert_eq!(terminal.get(key), Some(value), "{name} changed {key}");
                }
            }
            MeasuredPhase {
                start,
                finish,
                fields: terminal,
            }
        })
        .collect()
}
fn healthy(rows: &[Fields], sessions: &[&str]) {
    let names: std::collections::HashSet<_> = rows
        .iter()
        .filter(|row| matches!(row["boundary"].as_str(), Some("start" | "finish")))
        .map(|row| row["phase"].as_str().unwrap())
        .collect();
    for name in names {
        paired(rows, name);
    }
    for name in [
        "outbox.group_collect",
        "outbox.acceptance_sync",
        "outbox.attempt",
        "completion.connection",
        "completion.queue_admission",
        "completion.reply_wait",
        "completion.total",
        "ingress.receiver",
        "ingress.receipt_admission",
        "inbox.queue",
        "inbox.transaction",
        "inbox.file_sync",
        "inbox.directory_sync",
        "outbox.ack_sync",
    ] {
        assert!(!paired(rows, name).is_empty(), "missing {name}");
    }
    let completions = paired(rows, "completion.total");
    let mut cids = std::collections::HashSet::new();
    for completion in &completions {
        let attempts: Vec<_> = paired(rows, "outbox.attempt")
            .into_iter()
            .filter(|entry| {
                entry.fields["attempt"] == completion.fields["attempt"]
                    && entry.fields["session"] == completion.fields["session"]
            })
            .collect();
        assert_eq!(attempts.len(), 1, "completion lost its outbox attempt");
        assert!(attempts[0].start < completion.start);
        assert!(completion.finish < attempts[0].finish);
        if let Some(cid) = completion.fields.get("cid") {
            assert!(cid.as_u64().unwrap() > 0);
            assert!(
                cids.insert(cid.as_u64().unwrap()),
                "CID reused across attempts/streams"
            );
        }
    }
    for session in sessions {
        let single = |name: &str| {
            let phases: Vec<_> = paired(rows, name)
                .into_iter()
                .filter(|entry| entry.fields["session"] == *session)
                .collect();
            assert_eq!(phases.len(), 1, "{session}: {name} must happen once");
            let entry = phases.into_iter().next().unwrap();
            assert_eq!(entry.fields["outcome"], "ok", "{session}: {name}");
            entry
        };
        let collect = single("outbox.group_collect");
        let acceptance = single("outbox.acceptance_sync");
        assert!(collect.finish < acceptance.start);
        for attempt in paired(rows, "outbox.attempt")
            .into_iter()
            .filter(|entry| entry.fields["session"] == *session)
        {
            assert!(acceptance.finish < attempt.start);
        }
        let file = single("inbox.file_sync");
        let directory = single("inbox.directory_sync");
        let ack = single("outbox.ack_sync");
        assert_eq!(
            file.fields["diagnostic_span_id"],
            directory.fields["diagnostic_span_id"]
        );
        assert!(file.finish < directory.start);
        let transactions = paired(rows, "inbox.transaction");
        for transaction in transactions
            .iter()
            .filter(|entry| entry.fields["session"] == *session)
        {
            assert_eq!(transaction.fields["outcome"], "ok");
            let queued = paired(rows, "inbox.queue")
                .into_iter()
                .find(|entry| {
                    entry.fields["diagnostic_span_id"] == transaction.fields["diagnostic_span_id"]
                })
                .unwrap();
            assert_eq!(queued.fields["outcome"], "ok");
            assert!(queued.finish < transaction.start);
        }
        let committed: Vec<_> = transactions
            .iter()
            .filter(|entry| entry.fields["diagnostic_span_id"] == file.fields["diagnostic_span_id"])
            .collect();
        assert_eq!(committed.len(), 1);
        let committed = committed[0];
        assert!(committed.start < file.start && directory.finish < committed.finish);
        assert_eq!(committed.fields["outcome"], "ok");
        // A receipt may acknowledge a deduplicated retry with a different CID.
        let successful: Vec<_> = completions
            .iter()
            .filter(|entry| entry.fields["session"] == *session && entry.fields["outcome"] == "ok")
            .collect();
        assert_eq!(successful.len(), 1, "{session}: one successful completion");
        let completion = successful[0];
        let cid = &completion.fields["cid"];
        let ingresses = paired(rows, "ingress.receiver");
        for ingress in ingresses
            .iter()
            .filter(|entry| entry.fields["session"] == *session)
        {
            assert_eq!(ingress.fields["outcome"], "ok");
            assert!(
                committed.finish < ingress.finish,
                "receipt preceded the actual commit"
            );
            let receipt = paired(rows, "ingress.receipt_admission")
                .into_iter()
                .find(|entry| {
                    entry.fields["diagnostic_span_id"] == ingress.fields["diagnostic_span_id"]
                })
                .unwrap();
            assert_eq!(receipt.fields["outcome"], "ok");
            assert!(ingress.finish < receipt.start);
        }
        let ingress: Vec<_> = ingresses
            .iter()
            .filter(|entry| &entry.fields["cid"] == cid)
            .collect();
        assert_eq!(ingress.len(), 1);
        let ingress = ingress[0];
        assert_eq!(ingress.fields["session"], *session);
        let same_completion = |name| {
            paired(rows, name)
                .into_iter()
                .find(|entry| {
                    entry.fields["diagnostic_span_id"] == completion.fields["diagnostic_span_id"]
                })
                .unwrap()
        };
        let connection = same_completion("completion.connection");
        let admission = same_completion("completion.queue_admission");
        let reply = same_completion("completion.reply_wait");
        for entry in [&connection, &admission, &reply] {
            assert_eq!(entry.fields["outcome"], "ok");
        }
        assert!(connection.finish < admission.start && admission.finish < reply.start);
        assert!(ingress.finish < reply.finish && reply.finish < completion.finish);
        let attempt = paired(rows, "outbox.attempt")
            .into_iter()
            .find(|entry| entry.fields["diagnostic_span_id"] == ack.fields["diagnostic_span_id"])
            .unwrap();
        assert_eq!(attempt.fields["attempt"], completion.fields["attempt"]);
        assert_eq!(attempt.fields["outcome"], "ok");
        assert!(attempt.start < completion.start && completion.finish < attempt.finish);
        assert!(attempt.finish < ack.start);
        let writes: Vec<_> = paired(rows, "transport.write_flush")
            .into_iter()
            .filter(|entry| {
                &entry.fields["cid"] == cid
                    && matches!(entry.fields["frame_type"].as_u64(), Some(0x13 | 0x11))
            })
            .collect();
        assert_eq!(writes.len(), 2, "one actual request and one actual receipt");
        let reads: Vec<_> = phase(rows, "transport.read", "observed")
            .into_iter()
            .filter(|(_, row)| {
                &row["cid"] == cid && matches!(row["frame_type"].as_u64(), Some(0x13 | 0x11))
            })
            .collect();
        assert_eq!(reads.len(), 2);
        for frame_type in [0x13, 0x11] {
            assert_eq!(
                writes
                    .iter()
                    .filter(|entry| entry.fields["frame_type"] == frame_type)
                    .count(),
                1
            );
            assert_eq!(
                reads
                    .iter()
                    .filter(|(_, row)| row["frame_type"] == frame_type)
                    .count(),
                1
            );
        }
        for write in writes {
            assert_eq!(write.fields["outcome"], "ok");
            assert!(write.fields["connection"].as_u64().unwrap() > 0);
        }
        for (at, row) in reads {
            if row["local_node"] == completion.fields["source"] {
                assert_eq!(row["frame_type"], 0x11);
                assert!(at < reply.finish);
            } else {
                assert_eq!(row["local_node"], completion.fields["receiver"]);
                assert_eq!(row["frame_type"], 0x13);
                assert!(at < ingress.start);
            }
        }
    }
    for row in rows {
        if row
            .get("phase")
            .and_then(Value::as_str)
            .is_some_and(|phase| phase.starts_with("inbox."))
        {
            assert!(
                phase(rows, "ingress.receiver", "start")
                    .iter()
                    .any(|(_, ingress)| {
                        ingress["cid"] == row["cid"] && ingress["session"] == row["session"]
                    }),
                "inbox attempt lost its ingress correlation"
            );
        }
        if row.get("local_node").is_some() {
            continue;
        }
        let local = row["path"] == "/state";
        let qualified =
            row["path"] == format!("parrot://{}/state", row["receiver"].as_str().unwrap());
        assert!(local || qualified, "{row:?}");
        if let Some(sequence) = row.get("sequence") {
            assert_eq!(sequence, 1);
        }
        if let Some(session) = row.get("session") {
            assert!(sessions.iter().any(|expected| session == expected));
        }
    }
}
#[tokio::test]
async fn real_mem_commit_and_ack_phases_have_causal_order() {
    let (rows, status) = run(false, false, false, false, true).await;
    assert_eq!(status.last_acked, 1);
    healthy(&rows, &["stream"]);
}
#[tokio::test]
async fn real_tcp_commit_and_ack_phases_have_causal_order() {
    let (rows, status) = run(true, false, false, false, true).await;
    assert_eq!(status.last_acked, 1);
    healthy(&rows, &["stream"]);
    let connect = phase(&rows, "outbox.connect", "finish");
    assert_eq!(connect.len(), 1);
    assert_eq!(connect[0].1["outcome"], "ok");
}
#[tokio::test]
async fn expired_attempt_reports_unfinished_reply_and_commits_once_on_retry() {
    let (rows, status) = run(false, true, false, false, true).await;
    assert!(status.attempts >= 2);
    assert!(phase(&rows, "completion.reply_wait", "finish")
        .iter()
        .any(|(_, r)| r["outcome"] == "unfinished"));
    assert!(phase(&rows, "completion.total", "finish")
        .iter()
        .any(|(_, r)| r["outcome"] == "error"));
    assert!(phase(&rows, "outbox.attempt", "finish")
        .iter()
        .any(|(_, r)| r["outcome"] == "error"));
    assert_eq!(phase(&rows, "outbox.ack_sync", "finish").len(), 1);
    assert_eq!(phase(&rows, "inbox.file_sync", "finish").len(), 1);
    healthy(&rows, &["stream"]);
}
#[tokio::test]
async fn rejected_transaction_never_emits_a_commit_or_ack_sync() {
    let (rows, status) = run(false, false, true, false, true).await;
    assert_eq!(status.last_acked, 0);
    assert!(phase(&rows, "inbox.transaction", "finish")
        .iter()
        .any(|(_, r)| r["outcome"] == "error"));
    assert!(phase(&rows, "inbox.file_sync", "finish").is_empty());
    assert!(phase(&rows, "outbox.ack_sync", "finish").is_empty());
}

#[tokio::test]
async fn identical_sequences_in_distinct_streams_keep_wire_correlation() {
    let (rows, _) = run(true, false, false, true, true).await;
    healthy(&rows, &["stream", "other-stream"]);
}

#[tokio::test]
async fn retry_causality_checks_reject_missing_duplicate_reordered_and_miscorrelated_evidence() {
    let (rows, status) = run(true, true, false, true, true).await;
    assert!(status.attempts >= 2);
    healthy(&rows, &["stream", "other-stream"]);
    let check = |label, corrupted: Vec<Fields>| {
        assert!(
            std::panic::catch_unwind(|| healthy(&corrupted, &["stream", "other-stream"])).is_err(),
            "causality checker accepted {label}"
        );
    };
    let file = phase(&rows, "inbox.file_sync", "start")[0].0;
    let file_end = phase(&rows, "inbox.file_sync", "finish")[0].0;
    let mut missing = rows.clone();
    missing.remove(file_end);
    missing.remove(file);
    check("missing durable commit", missing);
    let mut duplicate = rows.clone();
    duplicate.insert(file_end + 1, rows[file_end].clone());
    duplicate.insert(file, rows[file].clone());
    check("duplicate durable commit", duplicate);
    let directory_end = phase(&rows, "inbox.directory_sync", "finish")[0].0;
    let transaction_end = phase(&rows, "inbox.transaction", "finish")
        .into_iter()
        .find(|(_, row)| row["diagnostic_span_id"] == rows[directory_end]["diagnostic_span_id"])
        .unwrap()
        .0;
    let mut late_directory = rows.clone();
    let directory = late_directory.remove(directory_end);
    late_directory.insert(transaction_end, directory);
    check("transaction before directory sync", late_directory);
    let completion = phase(&rows, "completion.total", "finish")
        .into_iter()
        .find(|(_, row)| row["outcome"] == "ok")
        .unwrap()
        .0;
    let mut wrong_receipt = rows.clone();
    wrong_receipt[completion].insert("cid".into(), u64::MAX.into());
    check("unrelated receipt CID", wrong_receipt);
    let ack = phase(&rows, "outbox.ack_sync", "start")[0].0;
    let ack_end = phase(&rows, "outbox.ack_sync", "finish")[0].0;
    let attempt = phase(&rows, "outbox.attempt", "start")
        .into_iter()
        .find(|(_, row)| row["diagnostic_span_id"] == rows[ack]["diagnostic_span_id"])
        .unwrap()
        .0;
    let mut early_ack = rows.clone();
    let finish = early_ack.remove(ack_end);
    let start = early_ack.remove(ack);
    early_ack.insert(attempt, finish);
    early_ack.insert(attempt, start);
    check("ACK before delivery attempt", early_ack);
    let mut wrong_stream = rows.clone();
    for row in &mut wrong_stream {
        if row.get("diagnostic_span_id") == rows[file].get("diagnostic_span_id") {
            row.insert("session".into(), "unrelated-stream".into());
        }
    }
    check("cross-stream commit", wrong_stream);
}
#[test]
fn diagnostics_disabled_preserve_real_delivery() {
    const CHILD: &str = "PARROT_TIMING_DISABLED_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert!(!tracing::enabled!(target: "parrot::durable::timing", tracing::Level::TRACE));
        let (rows, status) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(false, false, false, false, false));
        assert_eq!(status.last_acked, 1);
        assert!(rows.is_empty());
        return;
    }
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "diagnostics_disabled_preserve_real_delivery",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed; 0 failed;"));
}

#[test]
fn diagnostics_filter_does_not_enable_payload_frame_logging() {
    observer();
    assert!(tracing::enabled!(target: "parrot::durable::timing", tracing::Level::TRACE));
    assert!(!tracing::enabled!(tracing::Level::TRACE));
}

#[tokio::test]
async fn cancelled_queued_inbox_command_is_unfinished_without_a_transaction() {
    let capture = observer();
    let directory = Scratch::new();
    let source_id = unique();
    let target_id = unique();
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let entered = started.clone();
    let (release, blocked) = std::sync::mpsc::channel();
    let mut inbox = TransactionalInbox::open(
        directory.0.join("inbox"),
        &target_id,
        "/state",
        Bytes::new(),
        InboxLimits::default(),
        move |_, message| {
            entered.add_permits(1);
            blocked.recv_timeout(Duration::from_secs(10)).unwrap();
            Ok(message.payload.clone())
        },
    )
    .await
    .unwrap();
    let envelope = |sequence| DurableEnvelope {
        source: source_id.clone(),
        session: "queue-cancel".into(),
        receiver: target_id.clone(),
        path: "/state".into(),
        sequence,
        type_key: "bin:PRIVATE_TYPE_KEY#v1".into(),
        payload: Bytes::from_static(b"first"),
    };
    let first_handle = inbox.handle();
    let first_message = envelope(1);
    let first = tokio::spawn(async move { first_handle.apply(first_message).await });
    tokio::time::timeout(Duration::from_secs(10), started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let second_handle = inbox.handle();
    let mut second = Box::pin(second_handle.apply(envelope(2)));
    assert!(futures::poll!(second.as_mut()).is_pending());
    let before = events(&capture, &source_id);
    assert!(phase(&before, "inbox.queue", "start")
        .iter()
        .any(|(_, r)| r["sequence"] == 2));
    drop(second);
    release.send(()).unwrap();
    first.await.unwrap().unwrap();
    inbox.close().await.unwrap();
    let rows = events(&capture, &source_id);
    let ends: Vec<_> = phase(&rows, "inbox.queue", "finish")
        .into_iter()
        .filter(|(_, r)| r["sequence"] == 2)
        .collect();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].1["outcome"], "unfinished");
    assert!(phase(&rows, "inbox.transaction", "start")
        .iter()
        .all(|(_, r)| r["sequence"] == 1));
    assert_eq!(inbox.handle().snapshot().revision, 1);
}

struct FaultyWriter {
    io: tokio::io::DuplexStream,
    mode: Arc<std::sync::atomic::AtomicU8>,
    entered: Arc<tokio::sync::Notify>,
}
impl tokio::io::AsyncRead for FaultyWriter {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for FaultyWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.mode.load(Ordering::SeqCst) {
            1 => {
                self.entered.notify_one();
                std::task::Poll::Pending
            }
            2 => {
                self.entered.notify_one();
                std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
            }
            _ => std::pin::Pin::new(&mut self.io).poll_write(cx, bytes),
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
async fn write_outcome(fail: bool) {
    use futures::{SinkExt, StreamExt};
    use parrot_remote::{
        frame::Frame,
        handshake::HandshakeBody,
        transport::{run_connection, ConnParams, ConnSide, FrameCodec, RuntimeKnobs},
    };
    let capture = observer();
    let source_id = unique();
    let peer_id = unique();
    let (local, peer) = tokio::io::duplex(4096);
    let mode = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let io = FaultyWriter {
        io: local,
        mode: mode.clone(),
        entered: entered.clone(),
    };
    let (inbound, _inbound_rx) = tokio::sync::mpsc::channel(1);
    let (_shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let exchange = async {
        let mut peer = tokio_util::codec::Framed::new(peer, FrameCodec);
        let mut body = bytes::BytesMut::new();
        HandshakeBody {
            node_id: peer_id,
            ..Default::default()
        }
        .encode_tlv(&mut body)
        .unwrap();
        peer.send(Frame::handshake(body.freeze())).await.unwrap();
        assert_eq!(
            peer.next().await.unwrap().unwrap().header.frame_type,
            parrot_remote::frame::frame_type::HANDSHAKE_ACK
        );
        peer
    };
    let connection = run_connection(
        io,
        ConnParams {
            side: ConnSide::Accept,
            local_addr: None,
            peer_addr: None,
            scheme: "mem",
            local_handshake: HandshakeBody {
                node_id: source_id.clone(),
                ..Default::default()
            },
            inbound,
            on_disconnect: Arc::new(|_| {}),
            shutdown: shutdown_rx,
            knobs: Some(RuntimeKnobs {
                heartbeat_enabled: false,
                ..Default::default()
            }),
        },
    );
    let (handle, _peer) = tokio::join!(connection, exchange);
    let mut handle = handle.unwrap();
    mode.store(if fail { 2 } else { 1 }, Ordering::SeqCst);
    handle
        .sender
        .send(Frame::tell_with_ack(
            42,
            "/state",
            "bin:PRIVATE_TYPE_KEY#v1",
            Bytes::from_static(b"PRIVATE_PAYLOAD_BYTES"),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    if !fail {
        let rows = events(&capture, &source_id);
        assert_eq!(phase(&rows, "transport.write_flush", "start").len(), 1);
        assert!(
            phase(&rows, "transport.write_flush", "finish").is_empty(),
            "queue admission must not be reported as completed IO"
        );
        handle.sender.close();
    }
    tokio::time::timeout(Duration::from_secs(5), &mut handle.closed)
        .await
        .unwrap()
        .unwrap();
    let rows = events(&capture, &source_id);
    let ends = phase(&rows, "transport.write_flush", "finish");
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].1["cid"], 42);
    assert_eq!(
        ends[0].1["outcome"],
        if fail { "error" } else { "unfinished" }
    );
    let rendered = serde_json::to_string(&rows).unwrap();
    assert!(!rendered.contains("PRIVATE_TYPE_KEY") && !rendered.contains("PRIVATE_PAYLOAD_BYTES"));
}
#[tokio::test]
async fn blocked_actual_write_remains_unfinished_until_owner_cancellation() {
    write_outcome(false).await;
}
#[tokio::test]
async fn actual_write_failure_is_recorded_as_error() {
    write_outcome(true).await;
}
