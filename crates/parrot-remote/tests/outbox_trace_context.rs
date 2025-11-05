//! Actual outbox OS worker, WAL restart across processes, TCP and transactional inbox.
use bytes::Bytes;
use parrot_api::trace::{SpanStatus, TraceContext, TraceRecorder};
use parrot_remote::durable::{
    DurableEnvelope, DurableOutbox, DurableReceiver, InboxLimits, OutboxConfig, TransactionalInbox,
    Wal,
};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
const PAYLOAD: [u8; 8] = 1u64.to_le_bytes();
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = std::env::var_os("PARROT_OUTBOX_TRACE_EVIDENCE")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join(format!("outbox-trace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("PARROT_OUTBOX_TRACE_EVIDENCE").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
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
type Events = Arc<Mutex<Vec<(u64, Option<TraceContext>, std::thread::ThreadId)>>>;
struct Pair {
    inbox: TransactionalInbox,
    server: Arc<RemoteActorSystem>,
    client: Arc<RemoteActorSystem>,
    events: Events,
    recorder: TraceRecorder,
}
impl Pair {
    async fn new(directory: &std::path::Path, fail_once: bool) -> Self {
        let events = Events::default();
        let observed = events.clone();
        let failed = std::sync::atomic::AtomicBool::new(false);
        let inbox = TransactionalInbox::open(
            directory.join("inbox"),
            "receiver",
            "/state",
            Bytes::copy_from_slice(&0u64.to_le_bytes()),
            InboxLimits::default(),
            move |state, message: &DurableEnvelope| {
                observed.lock().unwrap().push((
                    message.sequence,
                    TraceContext::current(),
                    std::thread::current().id(),
                ));
                if fail_once && !failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return Err("retry this transaction".into());
                }
                let value = u64::from_le_bytes(state.try_into().unwrap())
                    + u64::from_le_bytes(message.payload[..].try_into().unwrap());
                Ok(Bytes::copy_from_slice(&value.to_le_bytes()))
            },
        )
        .await
        .unwrap();
        let recorder = TraceRecorder::new(16).unwrap();
        let server = RemoteActorSystem::new(
            RemoteConfig::tcp("receiver", Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development(),
            Arc::new(Lookup(Some(Arc::new(inbox.handle())))),
        )
        .unwrap();
        server.set_trace_recorder(Some(recorder.clone()));
        server.start().await.unwrap();
        let client = RemoteActorSystem::new(
            RemoteConfig::tcp("source", None).with_insecure_development(),
            Arc::new(Lookup(None)),
        )
        .unwrap();
        client.start().await.unwrap();
        Self {
            inbox,
            server,
            client,
            events,
            recorder,
        }
    }
    fn address(&self) -> NodeAddr {
        NodeAddr::tcp("receiver", self.server.local_addr().unwrap())
    }
    async fn close(mut self) -> (Events, parrot_api::trace::RecordingSnapshot) {
        self.client.shutdown().await.unwrap();
        self.server.shutdown().await.unwrap();
        self.inbox.close().await.unwrap();
        self.recorder.close();
        (self.events, self.recorder.drain())
    }
}
fn config(idempotent: bool) -> OutboxConfig {
    OutboxConfig {
        transactional: true,
        idempotent_submissions: idempotent,
        attempt_timeout: Duration::from_secs(1),
        retry_min: Duration::from_millis(20),
        retry_max: Duration::from_millis(50),
        ..Default::default()
    }
}
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(12), future)
        .await
        .unwrap()
}
fn check(child: &TraceContext, parent: &TraceContext) {
    assert_eq!(child.trace_id(), parent.trace_id());
    assert_eq!(child.parent_span_id(), Some(parent.span_id()));
    assert_ne!(child.span_id(), parent.span_id());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enqueue_captures_call_time_parent_and_untraced_work_stays_isolated() {
    bounded(async {
        let dir = Scratch::new();
        let pair = Pair::new(&dir.0, false).await;
        let root = TraceContext::root();
        let other = TraceContext::root();
        let payload = 1u64.to_le_bytes();
        let mut outbox = DurableOutbox::open(
            dir.0.join("outbox"),
            &pair.client,
            pair.address(),
            "/state",
            "session",
            config(false),
        )
        .await
        .unwrap();
        let handle = outbox.handle();
        let first = root.in_scope(|| handle.enqueue("delta", &payload));
        assert_eq!(other.scope(first).await.unwrap(), 1);
        let second = handle.enqueue("delta", &payload);
        assert_eq!(root.scope(second).await.unwrap(), 2);
        handle.wait_acked(2).await.unwrap();
        assert_eq!(pair.inbox.handle().snapshot().revision, 2);
        outbox.close().await.unwrap();
        let (events, snapshot) = pair.close().await;
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        check(
            events[0]
                .1
                .as_ref()
                .expect("outbox lost call-time causal parent"),
            &root,
        );
        assert!(events[1].1.is_none());
        assert_ne!(events[0].2, std::thread::current().id());
        assert!(snapshot.terminal);
        assert_eq!(snapshot.records.len(), 1);
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_retry_reuses_persisted_parent_but_executes_distinct_transaction_children() {
    bounded(async {
        let dir = Scratch::new();
        let pair = Pair::new(&dir.0, true).await;
        let root = TraceContext::root();
        let mut outbox = DurableOutbox::open(
            dir.0.join("outbox"),
            &pair.client,
            pair.address(),
            "/state",
            "session",
            config(false),
        )
        .await
        .unwrap();
        let handle = outbox.handle();
        assert_eq!(
            root.in_scope(|| handle.enqueue("delta", &PAYLOAD))
                .await
                .unwrap(),
            1
        );
        handle.wait_acked(1).await.unwrap();
        assert!(handle.status().attempts >= 2);
        outbox.close().await.unwrap();
        assert_eq!(pair.inbox.handle().snapshot().revision, 1);
        let (events, snapshot) = pair.close().await;
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        for (_, context, _) in events.iter() {
            check(
                context.as_ref().expect("retry lost persisted parent"),
                &root,
            );
        }
        assert_ne!(events[0].1, events[1].1);
        assert_eq!(
            snapshot
                .records
                .iter()
                .map(|r| r.status)
                .collect::<Vec<_>>(),
            vec![SpanStatus::Error, SpanStatus::Ok]
        );
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_commit_group_preserves_each_requests_own_parent() {
    bounded(async {
        let dir = Scratch::new();
        let pair = Pair::new(&dir.0, false).await;
        let a = TraceContext::root();
        let b = TraceContext::root();
        let payload = 1u64.to_le_bytes();
        let mut configuration = config(true);
        configuration.group_commit_interval = Duration::from_millis(30);
        let mut outbox = DurableOutbox::open(
            dir.0.join("outbox"),
            &pair.client,
            pair.address(),
            "/state",
            "session",
            configuration,
        )
        .await
        .unwrap();
        let handle = outbox.handle();
        let first = a.in_scope(|| handle.enqueue_idempotent("first", "delta", &payload));
        let second = b.in_scope(|| handle.enqueue_idempotent("second", "delta", &payload));
        let third = handle.enqueue_idempotent("third", "delta", &payload);
        let (x, y, z) = tokio::join!(first, second, third);
        assert_eq!((x.unwrap(), y.unwrap(), z.unwrap()), (1, 2, 3));
        assert_eq!(handle.status().last_group_records, 3);
        handle.wait_acked(3).await.unwrap();
        outbox.close().await.unwrap();
        let (events, snapshot) = pair.close().await;
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 3);
        check(
            events[0]
                .1
                .as_ref()
                .expect("first group member lost parent"),
            &a,
        );
        check(
            events[1]
                .1
                .as_ref()
                .expect("second group member lost parent"),
            &b,
        );
        assert!(events[2].1.is_none());
        assert_eq!(snapshot.records.len(), 2);
    })
    .await;
}
#[test]
fn enqueue_in_subprocess() {
    let Some(path) = std::env::var_os("PARROT_OUTBOX_CHILD_DIRECTORY") else {
        return;
    };
    let path = PathBuf::from(path);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async{
  let client=RemoteActorSystem::new(RemoteConfig::tcp("source",None).with_insecure_development(),Arc::new(Lookup(None))).unwrap();client.start().await.unwrap();let root=TraceContext::root();
  let mut outbox=DurableOutbox::open(path.join("outbox"),&client,NodeAddr::tcp("receiver","127.0.0.1:9".parse().unwrap()),"/state","session",config(true)).await.unwrap();let handle=outbox.handle();assert_eq!(root.in_scope(||handle.enqueue_idempotent("initial","delta",&PAYLOAD)).await.unwrap(),1);assert_eq!(handle.status().last_acked,0);outbox.close().await.unwrap();client.shutdown().await.unwrap();
  std::fs::write(path.join("child.json"),serde_json::to_vec(&serde_json::json!({"process":std::process::id(),"trace":root.trace_id(),"parent":root.span_id()})).unwrap()).unwrap();
 });
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_restart_and_idempotent_retry_preserve_original_accepted_parent() {
    bounded(async {
        let dir = Scratch::new();
        let log = std::fs::File::create(dir.0.join("child.log")).unwrap();
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["enqueue_in_subprocess", "--exact", "--nocapture"])
                .env("PARROT_OUTBOX_CHILD_DIRECTORY", &dir.0)
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(status.success());
        let previous: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.0.join("child.json")).unwrap()).unwrap();
        assert_ne!(previous["process"], std::process::id());
        let expected_trace: [u8; 16] = serde_json::from_value(previous["trace"].clone()).unwrap();
        let expected_parent: [u8; 8] = serde_json::from_value(previous["parent"].clone()).unwrap();
        {
            let wal = Wal::open(dir.0.join("outbox")).unwrap();
            let records = wal.replay("receiver").unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].type_key, "delta");
            assert_eq!(&records[0].payload[..], &PAYLOAD);
        }
        let pair = Pair::new(&dir.0, false).await;
        let other = TraceContext::root();
        let mut outbox = other
            .scope(DurableOutbox::open(
                dir.0.join("outbox"),
                &pair.client,
                pair.address(),
                "/state",
                "session",
                config(true),
            ))
            .await
            .unwrap();
        let handle = outbox.handle();
        assert_eq!(
            other
                .in_scope(|| handle.enqueue_idempotent("initial", "delta", &PAYLOAD))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            other
                .in_scope(|| handle.enqueue_idempotent("next", "delta", &PAYLOAD))
                .await
                .unwrap(),
            2
        );
        handle.wait_acked(2).await.unwrap();
        assert_eq!(
            other
                .in_scope(|| handle.enqueue_idempotent("initial", "delta", &PAYLOAD))
                .await
                .unwrap(),
            1
        );
        outbox.close().await.unwrap();
        assert_eq!(pair.inbox.handle().snapshot().revision, 2);
        let (events, snapshot) = pair.close().await;
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        let first = events[0]
            .1
            .as_ref()
            .expect("restart lost committed causal metadata");
        assert_eq!(first.trace_id(), expected_trace);
        assert_eq!(first.parent_span_id(), Some(expected_parent));
        check(events[1].1.as_ref().unwrap(), &other);
        assert_eq!(snapshot.records.len(), 2);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_unpolled_traced_submission_has_no_durable_or_causal_effect() {
    bounded(async {
        let dir = Scratch::new();
        let pair = Pair::new(&dir.0, false).await;
        let mut outbox = DurableOutbox::open(
            dir.0.join("outbox"),
            &pair.client,
            pair.address(),
            "/state",
            "session",
            config(true),
        )
        .await
        .unwrap();
        let handle = outbox.handle();
        let discarded = TraceContext::root();
        let accepted = TraceContext::root();
        drop(discarded.in_scope(|| handle.enqueue_idempotent("same-id", "delta", &PAYLOAD)));
        assert_eq!(handle.status().last_enqueued, 0);
        assert_eq!(
            accepted
                .in_scope(|| handle.enqueue_idempotent("same-id", "delta", &PAYLOAD))
                .await
                .unwrap(),
            1
        );
        handle.wait_acked(1).await.unwrap();
        outbox.close().await.unwrap();
        let (events, snapshot) = pair.close().await;
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        check(events[0].1.as_ref().unwrap(), &accepted);
        assert_eq!(snapshot.records.len(), 1);
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn causal_prefix_counts_against_command_budget_without_poisoning_next_submission() {
    bounded(async {
        let dir = Scratch::new();
        let pair = Pair::new(&dir.0, false).await;
        let mut options = config(false);
        options.command_bytes = ("delta".len() + PAYLOAD.len()) as u32;
        let mut outbox = DurableOutbox::open(
            dir.0.join("outbox"),
            &pair.client,
            pair.address(),
            "/state",
            "session",
            options,
        )
        .await
        .unwrap();
        let handle = outbox.handle();
        let root = TraceContext::root();
        assert!(matches!(
            root.in_scope(|| handle.enqueue("delta", &PAYLOAD)).await,
            Err(parrot_remote::durable::OutboxError::Capacity)
        ));
        assert_eq!(handle.status().last_enqueued, 0);
        assert_eq!(handle.enqueue("delta", &PAYLOAD).await.unwrap(), 1);
        handle.wait_acked(1).await.unwrap();
        outbox.close().await.unwrap();
        let (events, snapshot) = pair.close().await;
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].1.is_none());
        assert!(snapshot.records.is_empty());
    })
    .await;
}
