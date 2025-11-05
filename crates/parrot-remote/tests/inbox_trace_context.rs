//! Real owned inbox OS worker, real disk state and actual callback lifetime.
use bytes::Bytes;
use parrot_api::trace::{SpanStatus, TraceContext, TraceRecorder};
use parrot_remote::durable::{DurableEnvelope, InboxLimits, TransactionalInbox};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
type Events = Arc<Mutex<Vec<(&'static str, Option<TraceContext>, std::thread::ThreadId)>>>;
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = std::env::var_os("PARROT_INBOX_TRACE_EVIDENCE")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join(format!("inbox-trace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("PARROT_INBOX_TRACE_EVIDENCE").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
struct CallbackOwner(Events);
impl Drop for CallbackOwner {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap()
            .push(("drop", TraceContext::current(), std::thread::current().id()));
    }
}
fn transition(
    events: Events,
) -> impl Fn(&[u8], &DurableEnvelope) -> Result<Bytes, String> + Send + 'static {
    let owner = CallbackOwner(events.clone());
    move |state, message| {
        let _keep = &owner;
        events
            .lock()
            .unwrap()
            .push(("call", TraceContext::current(), std::thread::current().id()));
        match message.type_key.as_str() {
            "error" => return Err("business failure".into()),
            "panic" => panic!("business panic"),
            _ => {}
        }
        let value = u64::from_le_bytes(state.try_into().unwrap())
            + u64::from_le_bytes(message.payload[..].try_into().unwrap());
        Ok(Bytes::copy_from_slice(&value.to_le_bytes()))
    }
}
fn message(sequence: u64, key: &str) -> DurableEnvelope {
    DurableEnvelope {
        source: "source".into(),
        session: "session".into(),
        receiver: "receiver".into(),
        path: "/state".into(),
        sequence,
        type_key: key.into(),
        payload: Bytes::copy_from_slice(&1u64.to_le_bytes()),
    }
}
fn initial() -> Bytes {
    Bytes::copy_from_slice(&0u64.to_le_bytes())
}
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .unwrap()
}
fn assert_child(child: &TraceContext, parent: &TraceContext) {
    assert_eq!(child.trace_id(), parent.trace_id());
    assert_eq!(child.parent_span_id(), Some(parent.span_id()));
    assert_ne!(child.span_id(), parent.span_id());
}
#[tokio::test]
async fn apply_captures_call_time_parent_across_the_actual_storage_thread() {
    let directory = Scratch::new();
    let events = Events::default();
    let recorder = TraceRecorder::new(8).unwrap();
    let parent = recorder.root();
    let mut inbox = bounded(TransactionalInbox::open(
        &directory.0,
        "receiver",
        "/state",
        initial(),
        InboxLimits::default(),
        transition(events.clone()),
    ))
    .await
    .unwrap();
    let handle = inbox.handle();
    let future = parent.in_scope(|| handle.apply(message(1, "add")));
    let other = TraceContext::root();
    bounded(other.scope(future)).await.unwrap();
    assert!(TraceContext::current().is_none());
    bounded(inbox.close()).await.unwrap();
    assert_eq!(handle.snapshot().revision, 1);
    assert_eq!(&handle.snapshot().state[..], &1u64.to_le_bytes());
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, "call");
    assert_ne!(events[0].2, std::thread::current().id());
    assert_child(
        events[0]
            .1
            .as_ref()
            .expect("actual transaction lost causal context"),
        &parent,
    );
    assert!(events[1].1.is_none());
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.records.len(), 1);
    let span = &snapshot.records[0];
    assert_eq!(span.status, SpanStatus::Ok);
    assert_eq!(format!("{:?}", span.kind), "InboxTransaction");
    assert_eq!(span.span_id, events[0].1.as_ref().unwrap().span_id());
}
#[tokio::test]
async fn untraced_transaction_is_isolated_from_registration_and_polling_context() {
    let directory = Scratch::new();
    let events = Events::default();
    let registration = TraceContext::root();
    let polling = TraceContext::root();
    let open = registration.in_scope(|| {
        TransactionalInbox::open(
            &directory.0,
            "receiver",
            "/state",
            initial(),
            InboxLimits::default(),
            transition(events.clone()),
        )
    });
    let mut inbox = bounded(polling.scope(open)).await.unwrap();
    let handle = inbox.handle();
    let future = handle.apply(message(1, "add"));
    bounded(polling.scope(future)).await.unwrap();
    bounded(inbox.close()).await.unwrap();
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events[0].1.is_none());
    assert_eq!(events[1].1.as_ref(), Some(&registration));
}
#[tokio::test]
async fn callback_disposal_before_open_is_polled_uses_registration_identity() {
    let directory = Scratch::new();
    let events = Events::default();
    let registration = TraceContext::root();
    let other = TraceContext::root();
    let future = registration.in_scope(|| {
        TransactionalInbox::open(
            &directory.0,
            "receiver",
            "/state",
            initial(),
            InboxLimits::default(),
            transition(events.clone()),
        )
    });
    other.in_scope(|| drop(future));
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "drop");
    assert_eq!(events[0].1.as_ref(), Some(&registration));
    assert!(!directory.0.join("inbox.state").exists());
    assert!(TraceContext::current().is_none());
}
#[tokio::test]
async fn callback_disposal_after_failed_storage_open_uses_registration_identity() {
    let directory = Scratch::new();
    let file = directory.0.join("file");
    std::fs::write(&file, b"not a directory").unwrap();
    let events = Events::default();
    let registration = TraceContext::root();
    let other = TraceContext::root();
    let future = registration.in_scope(|| {
        TransactionalInbox::open(
            file,
            "receiver",
            "/state",
            initial(),
            InboxLimits::default(),
            transition(events.clone()),
        )
    });
    assert!(bounded(other.scope(future)).await.is_err());
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "drop");
    assert_eq!(events[0].1.as_ref(), Some(&registration));
    assert_ne!(events[0].2, std::thread::current().id());
}
#[tokio::test]
async fn retries_get_distinct_real_transaction_spans_and_duplicate_or_gap_get_none() {
    let directory = Scratch::new();
    let events = Events::default();
    let recorder = TraceRecorder::new(8).unwrap();
    let root = recorder.root();
    let mut inbox = bounded(TransactionalInbox::open(
        &directory.0,
        "receiver",
        "/state",
        initial(),
        InboxLimits::default(),
        transition(events.clone()),
    ))
    .await
    .unwrap();
    let handle = inbox.handle();
    for key in ["error", "panic", "add"] {
        let result = bounded(root.in_scope(|| handle.apply(message(1, key)))).await;
        assert_eq!(result.is_ok(), key == "add");
    }
    bounded(root.in_scope(|| handle.apply(message(1, "add"))))
        .await
        .unwrap();
    assert!(bounded(root.in_scope(|| handle.apply(message(3, "add"))))
        .await
        .is_err());
    bounded(inbox.close()).await.unwrap();
    assert_eq!(handle.snapshot().revision, 1);
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.records.len(), 3);
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|s| s.status)
            .collect::<Vec<_>>(),
        vec![SpanStatus::Error, SpanStatus::Panicked, SpanStatus::Ok]
    );
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 4);
    let mut ids = std::collections::HashSet::new();
    for event in &events[..3] {
        let child = event.1.as_ref().unwrap();
        assert_child(child, &root);
        assert!(ids.insert(child.span_id()));
    }
    for (span, event) in snapshot.records.iter().zip(events.iter()) {
        assert_eq!(span.span_id, event.1.as_ref().unwrap().span_id());
    }
}
#[tokio::test]
async fn cancelling_unpolled_apply_does_not_execute_or_create_a_record() {
    let directory = Scratch::new();
    let events = Events::default();
    let recorder = TraceRecorder::new(8).unwrap();
    let root = recorder.root();
    let mut inbox = bounded(TransactionalInbox::open(
        &directory.0,
        "receiver",
        "/state",
        initial(),
        InboxLimits::default(),
        transition(events.clone()),
    ))
    .await
    .unwrap();
    let handle = inbox.handle();
    drop(root.in_scope(|| handle.apply(message(1, "add"))));
    bounded(inbox.close()).await.unwrap();
    assert_eq!(handle.snapshot().revision, 0);
    assert_eq!(events.lock().unwrap().len(), 1);
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert!(snapshot.records.is_empty());
}

struct ThrowingOwner(Events);
impl Drop for ThrowingOwner {
    fn drop(&mut self) {
        self.0.lock().unwrap().push((
            "owner_panic",
            TraceContext::current(),
            std::thread::current().id(),
        ));
        panic!("callback owner disposal failed");
    }
}
#[tokio::test]
async fn throwing_callback_disposal_keeps_registration_context_and_failure_receipt() {
    let directory = Scratch::new();
    let events = Events::default();
    let registration = TraceContext::root();
    let other = TraceContext::root();
    let owner = ThrowingOwner(events.clone());
    let future = registration.in_scope(|| {
        TransactionalInbox::open(
            &directory.0,
            "receiver",
            "/state",
            initial(),
            InboxLimits::default(),
            move |state, _| {
                let _keep = &owner;
                Ok(Bytes::copy_from_slice(state))
            },
        )
    });
    let mut inbox = bounded(other.scope(future)).await.unwrap();
    assert!(bounded(other.scope(inbox.close())).await.is_err());
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "owner_panic");
    assert_eq!(events[0].1.as_ref(), Some(&registration));
    assert_ne!(events[0].2, std::thread::current().id());
    assert!(TraceContext::current().is_none());
}
struct ThrowingPayload(Events);
impl Drop for ThrowingPayload {
    fn drop(&mut self) {
        self.0.lock().unwrap().push((
            "payload_drop",
            TraceContext::current(),
            std::thread::current().id(),
        ));
        panic!("panic payload disposal failed");
    }
}
#[tokio::test]
async fn panic_payload_disposal_uses_transaction_child_and_remains_recoverable() {
    let directory = Scratch::new();
    let events = Events::default();
    let observed = events.clone();
    let recorder = TraceRecorder::new(8).unwrap();
    let root = recorder.root();
    let mut inbox = bounded(TransactionalInbox::open(
        &directory.0,
        "receiver",
        "/state",
        initial(),
        InboxLimits::default(),
        move |state, message| {
            observed.lock().unwrap().push((
                "call",
                TraceContext::current(),
                std::thread::current().id(),
            ));
            if message.type_key == "panic" {
                std::panic::panic_any(ThrowingPayload(observed.clone()));
            }
            Ok(Bytes::copy_from_slice(state))
        },
    ))
    .await
    .unwrap();
    let handle = inbox.handle();
    assert!(bounded(root.in_scope(|| handle.apply(message(1, "panic"))))
        .await
        .is_err());
    bounded(root.in_scope(|| handle.apply(message(1, "add"))))
        .await
        .unwrap();
    bounded(inbox.close()).await.unwrap();
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[1].0, "payload_drop");
    assert_eq!(events[0].1, events[1].1);
    assert_child(events[0].1.as_ref().unwrap(), &root);
    assert_child(events[2].1.as_ref().unwrap(), &root);
    assert_ne!(events[0].1, events[2].1);
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|s| s.status)
            .collect::<Vec<_>>(),
        vec![SpanStatus::Panicked, SpanStatus::Ok]
    );
    assert_eq!(handle.snapshot().revision, 1);
}
#[tokio::test]
async fn cancelling_started_apply_does_not_cancel_committed_transaction_or_change_identity() {
    let directory = Scratch::new();
    let events = Events::default();
    let observed = events.clone();
    let recorder = TraceRecorder::new(8).unwrap();
    let root = recorder.root();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered = Mutex::new(Some(entered_tx));
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut inbox = bounded(TransactionalInbox::open(
        &directory.0,
        "receiver",
        "/state",
        initial(),
        InboxLimits::default(),
        move |_, _| {
            observed.lock().unwrap().push((
                "before",
                TraceContext::current(),
                std::thread::current().id(),
            ));
            entered.lock().unwrap().take().unwrap().send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            observed.lock().unwrap().push((
                "after",
                TraceContext::current(),
                std::thread::current().id(),
            ));
            Ok(Bytes::copy_from_slice(&1u64.to_le_bytes()))
        },
    ))
    .await
    .unwrap();
    let handle = inbox.handle();
    let worker_handle = handle.clone();
    let operation = root.scope(async move { worker_handle.apply(message(1, "add")).await });
    let task = tokio::spawn(operation);
    bounded(entered_rx).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    release_tx.send(()).unwrap();
    bounded(async {
        while handle.snapshot().revision != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(inbox.close()).await.unwrap();
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].1, events[1].1);
    assert_child(events[0].1.as_ref().unwrap(), &root);
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(snapshot.records[0].status, SpanStatus::Ok);
    assert_eq!(&handle.snapshot().state[..], &1u64.to_le_bytes());
}
#[tokio::test]
async fn failed_commit_records_error_without_claiming_durable_completion() {
    let directory = Scratch::new();
    let events = Events::default();
    let recorder = TraceRecorder::new(8).unwrap();
    let root = recorder.root();
    let mut inbox = bounded(TransactionalInbox::open(
        &directory.0,
        "receiver",
        "/state",
        initial(),
        InboxLimits::default(),
        transition(events.clone()),
    ))
    .await
    .unwrap();
    let handle = inbox.handle();
    // Force the real create_new storage boundary to reject its next snapshot.
    let next = directory.0.join("inbox.next");
    assert!(!next.exists());
    std::fs::write(next, b"preserved fault injection").unwrap();
    assert!(bounded(root.in_scope(|| handle.apply(message(1, "add"))))
        .await
        .is_err());
    assert!(bounded(inbox.close()).await.is_err());
    assert_eq!(handle.snapshot().revision, 0);
    assert_eq!(&handle.snapshot().state[..], &0u64.to_le_bytes());
    assert!(handle.snapshot().closed);
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_child(events[0].1.as_ref().unwrap(), &root);
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(snapshot.records[0].status, SpanStatus::Error);
}

struct NetworkLookup(Arc<dyn parrot_remote::durable::DurableReceiver>);
#[async_trait::async_trait]
impl parrot_remote::LocalLookup for NetworkLookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
    fn durable_receiver(
        &self,
        path: &str,
    ) -> Option<Arc<dyn parrot_remote::durable::DurableReceiver>> {
        (path == "/state").then(|| self.0.clone())
    }
}
struct EmptyLookup;
#[async_trait::async_trait]
impl parrot_remote::LocalLookup for EmptyLookup {
    async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
        None
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_tcp_durable_request_reaches_transaction_child_without_bypassing_origin_checks() {
    use parrot_remote::{
        durable::{DURABLE_KEY, DURABLE_RECEIPT_KEY},
        error::ErrCode,
        frame::Frame,
        registry::ReplyPayload,
        NodeAddr, RemoteActorSystem, RemoteConfig,
    };
    bounded(async {
        let directory = Scratch::new();
        let events = Events::default();
        let recorder = TraceRecorder::new(8).unwrap();
        let mut inbox = TransactionalInbox::open(
            &directory.0,
            "receiver",
            "/state",
            initial(),
            InboxLimits::default(),
            transition(events.clone()),
        )
        .await
        .unwrap();
        let handle = inbox.handle();
        let receiver = RemoteActorSystem::new(
            RemoteConfig::tcp("receiver", Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development(),
            Arc::new(NetworkLookup(Arc::new(handle.clone()))),
        )
        .unwrap();
        receiver.set_trace_recorder(Some(recorder.clone()));
        receiver.start().await.unwrap();
        let source = RemoteActorSystem::new(
            RemoteConfig::tcp("source", None).with_insecure_development(),
            Arc::new(EmptyLookup),
        )
        .unwrap();
        source.start().await.unwrap();
        source
            .connect(&NodeAddr::tcp("receiver", receiver.local_addr().unwrap()))
            .await
            .unwrap();
        let sender = source.links_snapshot().await[0].1.clone();
        assert!(sender.supports_trace_context());
        let root = TraceContext::root();
        let other = TraceContext::root();
        for (parent, forged) in [(&root, false), (&other, false), (&other, true)] {
            let mut message = message(1, "add");
            if forged {
                message.source = "forged-origin".into();
            }
            let cid = source.callbacks.next_cid();
            let (tx, rx) = tokio::sync::oneshot::channel();
            source
                .callbacks
                .insert_on(cid, "receiver", tx, &sender)
                .unwrap();
            let frame = Frame::tell_with_ack(
                cid,
                &message.target_path(),
                DURABLE_KEY,
                message.encode().unwrap(),
            )
            .with_trace_parent(parent.as_parent())
            .unwrap();
            sender.send(frame).await.unwrap();
            match rx.await.unwrap() {
                ReplyPayload::Ok(bytes, key) => {
                    assert!(!forged);
                    assert_eq!(key, DURABLE_RECEIPT_KEY);
                    assert_eq!(&bytes[..], &1u64.to_le_bytes());
                }
                ReplyPayload::Err(code, _) => {
                    assert!(forged);
                    assert_eq!(code, ErrCode::Forbidden);
                }
            }
            source.callbacks.remove(cid);
        }
        source.shutdown().await.unwrap();
        receiver.shutdown().await.unwrap();
        inbox.close().await.unwrap();
        assert_eq!(handle.snapshot().revision, 1);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        let child = events[0].1.as_ref().unwrap();
        assert_child(child, &root);
        assert!(events[1].1.is_none());
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].span_id, child.span_id());
        assert_eq!(snapshot.records[0].status, SpanStatus::Ok);
    })
    .await;
}
