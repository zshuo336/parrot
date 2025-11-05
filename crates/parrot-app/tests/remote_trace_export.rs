//! Real TCP actor requests and real HTTP collection through owned recorder bindings.
use bytes::Bytes;
use parrot::thread::{ThreadActorConfig, ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    trace::{TraceContext, TraceRecorder},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::debug::{TraceExportConfig, TraceExportEnd, TraceExporter};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorSystem, RemoteConfig};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
#[derive(Serialize, Deserialize)]
struct Work {
    value: u64,
    blocked: bool,
}
#[derive(Serialize, Deserialize, Debug)]
struct Receipt {
    value: u64,
    trace: Option<[u8; 16]>,
    span: Option<[u8; 8]>,
    parent: Option<[u8; 8]>,
}
macro_rules! codec{($ty:ty,$key:literal)=>{parrot_api::message::inventory::submit!{parrot_api::message::CodecRegistration{type_key:$key,type_id:std::any::TypeId::of::<$ty>(),encode:|m|parrot_api::message::serde_remote_serialize(m.downcast_ref::<$ty>().ok_or("wrong type")?),decode:|b|Ok(Box::new(parrot_api::message::serde_remote_deserialize::<$ty>(b)?) as BoxedMessage)}}};}
codec!(Work, "bin:remote-export.Work#v1");
codec!(Receipt, "bin:remote-export.Receipt#v1");
struct Counter {
    value: u64,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl Actor for Counter {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let work = message.downcast::<Work>().unwrap();
            let context = TraceContext::current();
            if work.blocked {
                self.entered.notify_one();
                self.release.notified().await;
            }
            assert_eq!(context, TraceContext::current());
            self.value += work.value;
            Ok(Box::new(Receipt {
                value: self.value,
                trace: context.as_ref().map(TraceContext::trace_id),
                span: context.as_ref().map(TraceContext::span_id),
                parent: context.as_ref().and_then(TraceContext::parent_span_id),
            }) as BoxedMessage)
        })
    }
}
struct Lookup {
    actor: Option<BoxedActorRef>,
    inbox: Option<Arc<dyn parrot_remote::durable::DurableReceiver>>,
}
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/counter")
            .then(|| self.actor.as_ref().map(|a| a.clone_boxed()))
            .flatten()
    }
    fn durable_receiver(
        &self,
        path: &str,
    ) -> Option<Arc<dyn parrot_remote::durable::DurableReceiver>> {
        (path == "/state").then(|| self.inbox.clone()).flatten()
    }
}
struct Harness {
    engine: Arc<ThreadActorSystem>,
    server: Arc<RemoteActorSystem>,
    client: Arc<RemoteActorSystem>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
impl Harness {
    async fn new(inbox: Option<Arc<dyn parrot_remote::durable::DurableReceiver>>) -> Self {
        let engine = ThreadActorSystem::shared(Default::default());
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let actor = engine
            .spawn_at(
                Counter {
                    value: 0,
                    entered: entered.clone(),
                    release: release.clone(),
                },
                "/counter",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        let server = RemoteActorSystem::new(
            RemoteConfig::tcp("receiver", Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development(),
            Arc::new(Lookup {
                actor: Some(Box::new(actor)),
                inbox,
            }),
        )
        .unwrap();
        server.start().await.unwrap();
        let client = RemoteActorSystem::new(
            RemoteConfig::tcp("source", None).with_insecure_development(),
            Arc::new(Lookup {
                actor: None,
                inbox: None,
            }),
        )
        .unwrap();
        client.start().await.unwrap();
        client
            .connect(&NodeAddr::tcp("receiver", server.local_addr().unwrap()))
            .await
            .unwrap();
        Self {
            engine,
            server,
            client,
            entered,
            release,
        }
    }
    async fn request(&self, parent: Option<&TraceContext>, blocked: bool) -> Receipt {
        let target = self.client.remote_ref("parrot://receiver/counter").unwrap();
        *parrot_api::trace::in_scope(parent.cloned(), || {
            target.send(Box::new(Work { value: 1, blocked }))
        })
        .await
        .unwrap()
        .downcast::<Receipt>()
        .unwrap()
    }
    async fn close(self) {
        self.client.shutdown().await.unwrap();
        self.server.shutdown().await.unwrap();
        self.engine.shutdown_internal().await.unwrap();
    }
}
struct Collector {
    url: String,
    documents: Arc<Mutex<Vec<serde_json::Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Collector {
    async fn new(status: u16) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
        let documents = Arc::new(Mutex::new(Vec::new()));
        let saved = documents.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = tokio::io::BufReader::new(stream);
                let mut length = None;
                let mut total = 0;
                loop {
                    let mut line = String::new();
                    assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
                    total += line.len();
                    assert!(total < 32768);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = Some(value.trim().parse::<usize>().unwrap());
                        }
                    }
                }
                let length = length.unwrap();
                assert!(length <= 1024 * 1024);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                saved
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap());
                stream.get_mut().write_all(format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").as_bytes()).await.unwrap();
            }
        });
        Self {
            url,
            documents,
            task,
        }
    }
    fn exporter(&self, name: &str) -> TraceExporter {
        let mut config = TraceExportConfig::new(&self.url, name);
        config.allow_insecure_http = true;
        config.capacity = 16;
        config.interval = Duration::from_millis(10);
        config.request_timeout = Duration::from_secs(2);
        config.max_attempts = 1;
        TraceExporter::start(config).unwrap()
    }
    fn spans(&self, service: &str) -> Vec<serde_json::Value> {
        self.documents
            .lock()
            .unwrap()
            .iter()
            .flat_map(|d| d["resourceSpans"].as_array().unwrap())
            .filter(|r| {
                r["resource"]["attributes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a["key"] == "service.name" && a["value"]["stringValue"] == service)
            })
            .flat_map(|r| r["scopeSpans"].as_array().unwrap())
            .flat_map(|s| s["spans"].as_array().unwrap())
            .cloned()
            .collect()
    }
    fn save(&self, name: &str, expected: &[Receipt]) {
        if let Some(path) = std::env::var_os("PARROT_REMOTE_EXPORT_EVIDENCE") {
            let path = std::path::PathBuf::from(path).join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("documents.json"),
                serde_json::to_vec_pretty(&*self.documents.lock().unwrap()).unwrap(),
            )
            .unwrap();
            std::fs::write(
                path.join("expected.json"),
                serde_json::to_vec_pretty(expected).unwrap(),
            )
            .unwrap();
        }
    }
}
impl Drop for Collector {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn check(span: &serde_json::Value, receipt: &Receipt, parent: &TraceContext) {
    assert_eq!(receipt.trace, Some(parent.trace_id()));
    assert_eq!(receipt.parent, Some(parent.span_id()));
    assert_ne!(receipt.span, Some(parent.span_id()));
    assert_eq!(span["traceId"], hex(&receipt.trace.unwrap()));
    assert_eq!(span["spanId"], hex(&receipt.span.unwrap()));
    assert_eq!(span["parentSpanId"], hex(&receipt.parent.unwrap()));
    assert_eq!(span["status"]["code"], 1);
}
async fn finish(exporter: &mut TraceExporter, count: u64) {
    let report = exporter.shutdown(Duration::from_secs(3)).await.unwrap();
    assert_eq!(report.end, TraceExportEnd::Complete);
    assert_eq!(report.status.accepted_spans, count);
    assert!(report.remaining.terminal);
    assert!(report.pending.is_none());
    assert_eq!(report.remaining.counts.overwritten, 0);
}
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(12), future)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_incoming_actor_spans_reach_http_collector() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut exporter = collector.exporter("receiver");
        let attachment = exporter.attach_remote(&h.server).unwrap();
        let parent = TraceContext::root();
        let receipt = h.request(Some(&parent), false).await;
        assert_eq!(receipt.value, 1);
        h.close().await;
        drop(attachment);
        finish(&mut exporter, 1).await;
        let spans = collector.spans("receiver");
        assert_eq!(spans.len(), 1);
        check(&spans[0], &receipt, &parent);
        collector.save("actor", &[receipt]);
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untraced_request_does_not_gain_a_root_from_the_exporter() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut exporter = collector.exporter("receiver");
        let _attachment = exporter.attach_remote(&h.server).unwrap();
        let receipt = h.request(None, false).await;
        assert_eq!(receipt.value, 1);
        assert!(receipt.trace.is_none() && receipt.span.is_none() && receipt.parent.is_none());
        h.close().await;
        finish(&mut exporter, 0).await;
        assert!(collector.spans("receiver").is_empty());
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_stops_recording_but_preserves_wire_parent_and_business() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut exporter = collector.exporter("receiver");
        let attachment = exporter.attach_remote(&h.server).unwrap();
        let parent = TraceContext::root();
        let before = h.request(Some(&parent), false).await;
        drop(attachment);
        let after = h.request(Some(&parent), false).await;
        assert_eq!(after.value, 2);
        assert_eq!(after.trace, Some(parent.trace_id()));
        assert_eq!(after.parent, Some(parent.span_id()));
        assert_ne!(before.span, after.span);
        h.close().await;
        finish(&mut exporter, 1).await;
        let spans = collector.spans("receiver");
        assert_eq!(spans.len(), 1);
        check(&spans[0], &before, &parent);
        collector.save("detach", &[before]);
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_old_binding_cannot_remove_new_exporter() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut old = collector.exporter("old");
        let mut new = collector.exporter("new");
        let stale = old.attach_remote(&h.server).unwrap();
        let _current = new.attach_remote(&h.server).unwrap();
        drop(stale);
        let parent = TraceContext::root();
        let receipt = h.request(Some(&parent), false).await;
        h.close().await;
        finish(&mut old, 0).await;
        finish(&mut new, 1).await;
        assert!(collector.spans("old").is_empty());
        let spans = collector.spans("new");
        assert_eq!(spans.len(), 1);
        check(&spans[0], &receipt, &parent);
        collector.save("replacement", &[receipt]);
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacing_with_same_recorder_still_has_independent_binding_ownership() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut exporter = collector.exporter("receiver");
        let stale = exporter.attach_remote(&h.server).unwrap();
        let current = exporter.attach_remote(&h.server).unwrap();
        drop(stale);
        let parent = TraceContext::root();
        let receipt = h.request(Some(&parent), false).await;
        drop(current);
        h.request(Some(&parent), false).await;
        h.close().await;
        finish(&mut exporter, 1).await;
        let spans = collector.spans("receiver");
        assert_eq!(spans.len(), 1);
        check(&spans[0], &receipt, &parent);
        collector.save("same-recorder", &[receipt]);
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_replacement_does_not_resurrect_previous_binding() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut old = collector.exporter("old");
        let mut new = collector.exporter("new");
        let _stale = old.attach_remote(&h.server).unwrap();
        let current = new.attach_remote(&h.server).unwrap();
        drop(current);
        let parent = TraceContext::root();
        let receipt = h.request(Some(&parent), false).await;
        assert_eq!(receipt.value, 1);
        h.close().await;
        finish(&mut old, 0).await;
        finish(&mut new, 0).await;
        assert!(collector.spans("old").is_empty() && collector.spans("new").is_empty());
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_set_replaces_guard_without_later_being_cleared() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut exporter = collector.exporter("receiver");
        let stale = exporter.attach_remote(&h.server).unwrap();
        let recorder = TraceRecorder::new(8).unwrap();
        h.server.set_trace_recorder(Some(recorder.clone()));
        drop(stale);
        let parent = TraceContext::root();
        let receipt = h.request(Some(&parent), false).await;
        h.close().await;
        finish(&mut exporter, 0).await;
        recorder.close();
        let snapshot = recorder.drain();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(Some(snapshot.records[0].span_id), receipt.span);
        assert!(collector.spans("receiver").is_empty());
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admitted_handler_keeps_old_recorder_after_rebinding() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut old = collector.exporter("old");
        let mut new = collector.exporter("new");
        let stale = old.attach_remote(&h.server).unwrap();
        let parent = TraceContext::root();
        let mut pending = Box::pin(h.request(Some(&parent), true));
        assert!(futures::poll!(&mut pending).is_pending());
        h.entered.notified().await;
        let _current = new.attach_remote(&h.server).unwrap();
        drop(stale);
        h.release.notify_one();
        let before = pending.await;
        let after = h.request(Some(&parent), false).await;
        assert_eq!(after.value, 2);
        h.close().await;
        finish(&mut old, 1).await;
        finish(&mut new, 1).await;
        let old_spans = collector.spans("old");
        let new_spans = collector.spans("new");
        assert_eq!(old_spans.len(), 1);
        assert_eq!(new_spans.len(), 1);
        check(&old_spans[0], &before, &parent);
        check(&new_spans[0], &after, &parent);
        collector.save("in-flight", &[before, after]);
    })
    .await;
}
#[tokio::test]
async fn attachment_does_not_keep_remote_system_alive_and_closed_exporter_refuses_binding() {
    bounded(async {
        let collector = Collector::new(200).await;
        let mut exporter = collector.exporter("receiver");
        let remote = RemoteActorSystem::new(
            RemoteConfig::tcp("unused", None),
            Arc::new(Lookup {
                actor: None,
                inbox: None,
            }),
        )
        .unwrap();
        let weak = Arc::downgrade(&remote);
        let attachment = exporter.attach_remote(&remote).unwrap();
        drop(remote);
        assert!(weak.upgrade().is_none());
        drop(attachment);
        finish(&mut exporter, 0).await;
        let remote = RemoteActorSystem::new(
            RemoteConfig::tcp("unused", None),
            Arc::new(Lookup {
                actor: None,
                inbox: None,
            }),
        )
        .unwrap();
        assert!(exporter.attach_remote(&remote).is_err());
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exporter_failure_preserves_unconfirmed_remote_records_and_refuses_new_binding() {
    bounded(async {
        let collector = Collector::new(413).await;
        let h = Harness::new(None).await;
        let mut exporter = collector.exporter("receiver");
        let _attachment = exporter.attach_remote(&h.server).unwrap();
        let parent = TraceContext::root();
        let receipt = h.request(Some(&parent), false).await;
        while exporter.status().end.is_none() {
            tokio::task::yield_now().await;
        }
        assert!(exporter.attach_remote(&h.server).is_err());
        h.close().await;
        let report = exporter.shutdown(Duration::from_secs(3)).await.unwrap();
        assert_eq!(
            report.end,
            TraceExportEnd::Failed(parrot_app::debug::TraceExportFailure::Http(413))
        );
        assert_eq!(report.status.accepted_spans, 0);
        let pending = report.pending.as_ref().unwrap();
        assert_eq!(pending.snapshot.records.len(), 1);
        assert_eq!(Some(pending.snapshot.records[0].span_id), receipt.span);
        collector.save("rejected", &[receipt]);
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_durable_inbox_transaction_is_exported_over_http() {
    bounded(async {
        use parrot_remote::durable::{
            DurableEnvelope, InboxLimits, TransactionalInbox, DURABLE_KEY, DURABLE_RECEIPT_KEY,
        };
        let directory = tempfile::tempdir().unwrap();
        let observed = Arc::new(Mutex::new(None));
        let calls = observed.clone();
        let mut inbox = TransactionalInbox::open(
            directory.path(),
            "receiver",
            "/state",
            Bytes::new(),
            InboxLimits::default(),
            move |_, _| {
                *calls.lock().unwrap() = TraceContext::current();
                Ok(Bytes::from_static(b"committed"))
            },
        )
        .await
        .unwrap();
        let collector = Collector::new(200).await;
        let h = Harness::new(Some(Arc::new(inbox.handle()))).await;
        let mut exporter = collector.exporter("receiver");
        let _attachment = exporter.attach_remote(&h.server).unwrap();
        let parent = TraceContext::root();
        let sender = h.client.links_snapshot().await[0].1.clone();
        let cid = h.client.callbacks.next_cid();
        let (tx, rx) = tokio::sync::oneshot::channel();
        h.client
            .callbacks
            .insert_on(cid, "receiver", tx, &sender)
            .unwrap();
        let message = DurableEnvelope {
            source: "source".into(),
            session: "session".into(),
            receiver: "receiver".into(),
            path: "/state".into(),
            sequence: 1,
            type_key: "add".into(),
            payload: Bytes::new(),
        };
        sender
            .send(
                parrot_remote::Frame::tell_with_ack(
                    cid,
                    &message.target_path(),
                    DURABLE_KEY,
                    message.encode().unwrap(),
                )
                .with_trace_parent(parent.as_parent())
                .unwrap(),
            )
            .await
            .unwrap();
        match rx.await.unwrap() {
            parrot_remote::ReplyPayload::Ok(payload, key) => {
                assert_eq!(key, DURABLE_RECEIPT_KEY);
                assert_eq!(&payload[..], &1u64.to_le_bytes());
            }
            other => panic!("unexpected receipt: {other:?}"),
        };
        h.client.callbacks.remove(cid);
        h.close().await;
        inbox.close().await.unwrap();
        assert_eq!(inbox.handle().snapshot().revision, 1);
        assert_eq!(&inbox.handle().snapshot().state[..], b"committed");
        finish(&mut exporter, 1).await;
        let context = observed.lock().unwrap().clone().unwrap();
        let receipt = Receipt {
            value: 1,
            trace: Some(context.trace_id()),
            span: Some(context.span_id()),
            parent: context.parent_span_id(),
        };
        let spans = collector.spans("receiver");
        assert_eq!(spans.len(), 1);
        check(&spans[0], &receipt, &parent);
        assert!(spans[0]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == "parrot.span.kind"
                && a["value"]["stringValue"] == "inbox_transaction"));
        collector.save("inbox", &[receipt]);
    })
    .await;
}

#[test]
fn cancelled_export_task_cannot_accept_a_remote_binding() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (exporter, remote) = runtime.block_on(async {
        let mut config = TraceExportConfig::new("http://127.0.0.1:9/v1/traces", "cancelled");
        config.allow_insecure_http = true;
        let exporter = TraceExporter::start(config).unwrap();
        let remote = RemoteActorSystem::new(
            RemoteConfig::tcp("unused", None),
            Arc::new(Lookup {
                actor: None,
                inbox: None,
            }),
        )
        .unwrap();
        (exporter, remote)
    });
    // Runtime destruction cancels the owned exporter task without executing
    // work's normal closing path. An idle recorder alone is not a live exporter.
    drop(runtime);
    assert!(exporter.attach_remote(&remote).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopened_outbox_reaches_transaction_and_http_export_with_original_causal_identity() {
    bounded(async {
        use parrot_remote::durable::{
            DurableOutbox, InboxLimits, OutboxConfig, TransactionalInbox,
        };
        let evidence =
            std::env::var_os("PARROT_REMOTE_EXPORT_EVIDENCE").map(std::path::PathBuf::from);
        let directory = if let Some(path) = &evidence {
            std::fs::create_dir_all(path).unwrap();
            tempfile::tempdir_in(path).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let configuration = OutboxConfig {
            transactional: true,
            idempotent_submissions: true,
            attempt_timeout: Duration::from_secs(1),
            retry_min: Duration::from_millis(20),
            retry_max: Duration::from_millis(50),
            ..Default::default()
        };
        let original = TraceContext::root();
        let other = TraceContext::root();
        let offline = RemoteActorSystem::new(
            RemoteConfig::tcp("source", None).with_insecure_development(),
            Arc::new(Lookup {
                actor: None,
                inbox: None,
            }),
        )
        .unwrap();
        offline.start().await.unwrap();
        let mut outbox = DurableOutbox::open(
            directory.path().join("outbox"),
            &offline,
            NodeAddr::tcp("receiver", "127.0.0.1:9".parse().unwrap()),
            "/state",
            "session",
            configuration.clone(),
        )
        .await
        .unwrap();
        let handle = outbox.handle();
        assert_eq!(
            original
                .in_scope(|| handle.enqueue_idempotent("first", "delta", b"committed"))
                .await
                .unwrap(),
            1
        );
        assert_eq!(handle.status().last_acked, 0);
        outbox.close().await.unwrap();
        offline.shutdown().await.unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let calls = observed.clone();
        let mut inbox = TransactionalInbox::open(
            directory.path().join("inbox"),
            "receiver",
            "/state",
            Bytes::new(),
            InboxLimits::default(),
            move |_, message| {
                calls.lock().unwrap().push(TraceContext::current());
                Ok(message.payload.clone())
            },
        )
        .await
        .unwrap();
        let collector = Collector::new(200).await;
        let h = Harness::new(Some(Arc::new(inbox.handle()))).await;
        let mut exporter = collector.exporter("receiver");
        let _attachment = exporter.attach_remote(&h.server).unwrap();
        let mut outbox = other
            .scope(DurableOutbox::open(
                directory.path().join("outbox"),
                &h.client,
                NodeAddr::tcp("receiver", h.server.local_addr().unwrap()),
                "/state",
                "session",
                configuration,
            ))
            .await
            .unwrap();
        let handle = outbox.handle();
        assert_eq!(
            other
                .in_scope(|| handle.enqueue_idempotent("first", "delta", b"committed"))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            handle
                .enqueue_idempotent("second", "delta", b"untraced")
                .await
                .unwrap(),
            2
        );
        handle.wait_acked(2).await.unwrap();
        assert_eq!(handle.status().pending, 0);
        assert_eq!(handle.status().last_acked, 2);
        outbox.close().await.unwrap();
        h.close().await;
        inbox.close().await.unwrap();
        assert_eq!(inbox.handle().snapshot().revision, 2);
        assert_eq!(&inbox.handle().snapshot().state[..], b"untraced");
        finish(&mut exporter, 1).await;
        let contexts = observed.lock().unwrap();
        assert_eq!(contexts.len(), 2);
        assert!(contexts[1].is_none());
        let context = contexts[0].as_ref().unwrap();
        let receipt = Receipt {
            value: 1,
            trace: Some(context.trace_id()),
            span: Some(context.span_id()),
            parent: context.parent_span_id(),
        };
        let spans = collector.spans("receiver");
        assert_eq!(spans.len(), 1);
        check(&spans[0], &receipt, &original);
        collector.save("outbox-reopen", &[receipt]);
        if evidence.is_some() {
            let _ = directory.keep();
        }
    })
    .await;
}

struct TimerOwner;
struct StartTimer {
    target: BoxedActorRef,
    cancelled: bool,
}
impl Actor for TimerOwner {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        use parrot_api::context::ActorContext;
        Box::pin(async move {
            let start = *message.downcast::<StartTimer>().unwrap();
            let parent = TraceContext::current().unwrap();
            ctx.schedule_once(
                start.target,
                Box::new(Work {
                    value: 7,
                    blocked: false,
                }),
                if start.cancelled {
                    Duration::from_secs(3600)
                } else {
                    Duration::ZERO
                },
            )
            .await?;
            Ok(Box::new(parent) as BoxedMessage)
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_timer_lifetime_reaches_real_http_collector() {
    bounded(async {
        for cancelled in [false, true] {
            let collector = Collector::new(200).await;
            let service = if cancelled {
                "timer-cancelled"
            } else {
                "timer-completed"
            };
            let mut exporter = collector.exporter(service);
            let root = exporter.root();
            let engine = ThreadActorSystem::shared(Default::default());
            let target = engine
                .spawn_at(
                    Counter {
                        value: 0,
                        entered: Arc::new(tokio::sync::Notify::new()),
                        release: Arc::new(tokio::sync::Notify::new()),
                    },
                    "/timer-target",
                    None,
                    ThreadActorConfig::default(),
                )
                .await
                .unwrap();
            let owner = engine
                .spawn_at(
                    TimerOwner,
                    "/timer-owner",
                    None,
                    ThreadActorConfig::default(),
                )
                .await
                .unwrap();
            let registration = *root
                .in_scope(|| {
                    owner.send(Box::new(StartTimer {
                        target: target.clone_boxed(),
                        cancelled,
                    }))
                })
                .await
                .unwrap()
                .downcast::<TraceContext>()
                .unwrap();
            if !cancelled {
                loop {
                    if collector.spans(service).iter().any(|span| {
                        span["attributes"].as_array().unwrap().iter().any(|a| {
                            a["key"] == "parrot.span.kind"
                                && a["value"]["stringValue"] == "managed_task"
                        })
                    }) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
            owner.stop().await.unwrap();
            target.stop().await.unwrap();
            engine.shutdown_internal().await.unwrap();
            finish(&mut exporter, if cancelled { 2 } else { 3 }).await;
            let spans = collector.spans(service);
            let task = spans
                .iter()
                .find(|span| {
                    span["attributes"].as_array().unwrap().iter().any(|a| {
                        a["key"] == "parrot.span.kind"
                            && a["value"]["stringValue"] == "managed_task"
                    })
                })
                .unwrap();
            assert_eq!(task["traceId"], hex(&root.trace_id()));
            assert_eq!(task["parentSpanId"], hex(&registration.span_id()));
            assert_ne!(task["spanId"], hex(&registration.span_id()));
            assert_eq!(task["name"], "context.schedule.once");
            assert_eq!(task["kind"], 1);
            assert_eq!(task["status"]["code"], if cancelled { 2 } else { 1 });
            let status = task["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == "parrot.span.status")
                .unwrap();
            assert_eq!(
                status["value"]["stringValue"],
                if cancelled { "cancelled" } else { "ok" }
            );
            if !cancelled {
                let delivered = spans
                    .iter()
                    .find(|span| span["name"] == "/timer-target")
                    .unwrap();
                assert_eq!(delivered["parentSpanId"], hex(&registration.span_id()));
                assert_ne!(delivered["spanId"], task["spanId"]);
            }
            collector.save(
                service,
                &[Receipt {
                    value: 7,
                    trace: Some(registration.trace_id()),
                    span: Some(registration.span_id()),
                    parent: registration.parent_span_id(),
                }],
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn local_source_lifetime_reaches_real_http_collector() {
    use parrot_api::local_typed::{LocalStreamEventKind, LocalStreamRegistry};
    use std::rc::Rc;
    bounded(async {
        for cancelled in [false, true] {
            let collector = Collector::new(200).await;
            let service = if cancelled {
                "local-source-cancelled"
            } else {
                "local-source-completed"
            };
            let mut exporter = collector.exporter(service);
            let root = exporter.root();
            let mut registry = LocalStreamRegistry::new();
            let value = Rc::new(7u32);
            root.in_scope(|| {
                registry
                    .add_stream(futures::stream::iter([value.clone()]))
                    .unwrap()
            });
            let mut delivered = 0;
            if !cancelled {
                while let Some(mut event) = registry.next_event().await {
                    let finished = event.kind() == LocalStreamEventKind::Finished;
                    if let Some(item) = event.take_item() {
                        let item = item.downcast::<Rc<u32>>().unwrap();
                        assert!(Rc::ptr_eq(&value, &item));
                        delivered += **item;
                    }
                    event.complete(&mut registry, !finished);
                }
            }
            drop(registry);
            assert_eq!(delivered, if cancelled { 0 } else { 7 });
            assert_eq!(
                Rc::strong_count(&value),
                1,
                "source actually retired before shutdown"
            );
            finish(&mut exporter, 1).await;
            let spans = collector.spans(service);
            assert_eq!(spans.len(), 1);
            let task = &spans[0];
            assert_eq!(task["traceId"], hex(&root.trace_id()));
            assert_eq!(task["parentSpanId"], hex(&root.span_id()));
            assert_ne!(task["spanId"], hex(&root.span_id()));
            assert_eq!(task["name"], "context.stream.local");
            assert_eq!(task["kind"], 1);
            assert_eq!(task["status"]["code"], if cancelled { 2 } else { 1 });
            let attribute = |name| {
                task["attributes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|a| a["key"] == name)
                    .unwrap()["value"]["stringValue"]
                    .clone()
            };
            assert_eq!(attribute("parrot.span.kind"), "managed_task");
            assert_eq!(
                attribute("parrot.span.status"),
                if cancelled { "cancelled" } else { "ok" }
            );
            collector.save(service, &[]);
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_roots_are_independent_and_preserve_incoming_parents() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut exporter = collector.exporter("roots");
        let attachment = exporter
            .attach_remote_with_policy(&h.server, parrot_remote::IncomingTracePolicy::StartRoots)
            .unwrap();
        let first = h.request(None, false).await;
        let second = h.request(None, false).await;
        assert!(first.trace.is_some() && first.parent.is_some());
        assert_ne!(first.trace, second.trace);
        assert_ne!(first.parent, second.parent);
        let parent = TraceContext::root();
        let traced = h.request(Some(&parent), false).await;
        drop(attachment);
        let after = h.request(None, false).await;
        assert!(after.trace.is_none());
        h.close().await;
        finish(&mut exporter, 3).await;
        let spans = collector.spans("roots");
        assert_eq!(spans.len(), 3);
        for receipt in [&first, &second, &traced] {
            let span = spans
                .iter()
                .find(|s| s["spanId"] == hex(&receipt.span.unwrap()))
                .unwrap();
            assert_eq!(span["traceId"], hex(&receipt.trace.unwrap()));
            assert_eq!(span["parentSpanId"], hex(&receipt.parent.unwrap()));
            assert_eq!(span["status"]["code"], 1);
        }
        check(
            spans
                .iter()
                .find(|s| s["spanId"] == hex(&traced.span.unwrap()))
                .unwrap(),
            &traced,
            &parent,
        );
        collector.save("automatic-roots", &[first, second, traced]);
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_root_policy_is_replaced_with_the_binding() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut exporter = collector.exporter("roots");
        let stale = exporter
            .attach_remote_with_policy(&h.server, parrot_remote::IncomingTracePolicy::StartRoots)
            .unwrap();
        let passive = exporter.attach_remote(&h.server).unwrap();
        drop(stale);
        assert!(h.request(None, false).await.trace.is_none());
        let active = exporter
            .attach_remote_with_policy(&h.server, parrot_remote::IncomingTracePolicy::StartRoots)
            .unwrap();
        drop(passive);
        assert!(h.request(None, false).await.trace.is_some());
        h.server
            .set_trace_recorder(Some(TraceRecorder::new(8).unwrap()));
        drop(active);
        assert!(h.request(None, false).await.trace.is_none());
        h.close().await;
        finish(&mut exporter, 1).await;
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_inflight_root_keeps_its_recorder_after_detach() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut exporter = collector.exporter("roots");
        let attachment = exporter
            .attach_remote_with_policy(&h.server, parrot_remote::IncomingTracePolicy::StartRoots)
            .unwrap();
        let mut request = Box::pin(h.request(None, true));
        assert!(futures::poll!(&mut request).is_pending());
        h.entered.notified().await;
        drop(attachment);
        h.release.notify_one();
        let receipt = request.await;
        assert!(receipt.trace.is_some());
        assert!(h.request(None, false).await.trace.is_none());
        h.close().await;
        finish(&mut exporter, 1).await;
        assert_eq!(
            collector.spans("roots")[0]["spanId"],
            hex(&receipt.span.unwrap())
        );
        collector.save("automatic-inflight", &[receipt]);
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_roots_cover_plain_and_completion_tell() {
    bounded(async {
        let collector = Collector::new(200).await;
        let h = Harness::new(None).await;
        let mut exporter = collector.exporter("roots");
        let _attachment = exporter
            .attach_remote_with_policy(&h.server, parrot_remote::IncomingTracePolicy::StartRoots)
            .unwrap();
        let target = h.client.remote_ref("parrot://receiver/counter").unwrap();
        target
            .deliver(Box::new(Work {
                value: 3,
                blocked: true,
            }))
            .await
            .unwrap();
        h.entered.notified().await;
        h.release.notify_one();
        target
            .deliver_with_ack(
                Box::new(Work {
                    value: 4,
                    blocked: false,
                }),
                Duration::from_secs(3),
            )
            .await
            .unwrap();
        // Same actor FIFO: this receipt observes both real effects, not just enqueue.
        let receipt = h.request(None, false).await;
        assert_eq!(receipt.value, 8);
        h.close().await;
        finish(&mut exporter, 3).await;
        let spans = collector.spans("roots");
        assert_eq!(spans.len(), 3);
        let ids: std::collections::HashSet<_> = spans
            .iter()
            .map(|s| s["traceId"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 3);
        collector.save("automatic-tell", &[receipt]);
    })
    .await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exporter_stop_waiter_is_cancellation_safe_and_observes_failure() {
    bounded(async {
        let collector = Collector::new(400).await;
        let h = Harness::new(None).await;
        let mut exporter = collector.exporter("roots");
        let attachment = exporter
            .attach_remote_with_policy(&h.server, parrot_remote::IncomingTracePolicy::StartRoots)
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), exporter.wait_until_stopped())
                .await
                .is_err()
        );
        let receipt = h.request(None, false).await;
        let stopped = exporter.wait_until_stopped().await;
        assert_eq!(
            stopped.end,
            Some(TraceExportEnd::Failed(
                parrot_app::debug::TraceExportFailure::Http(400)
            ))
        );
        assert!(exporter
            .attach_remote_with_policy(&h.server, parrot_remote::IncomingTracePolicy::StartRoots)
            .is_err());
        assert_eq!(exporter.wait_until_stopped().await.end, stopped.end);
        h.close().await;
        drop(attachment);
        let report = exporter.shutdown(Duration::from_secs(1)).await.unwrap();
        assert_eq!(Some(report.end.clone()), stopped.end);
        assert_eq!(
            report.pending.as_ref().unwrap().snapshot.records[0].span_id,
            receipt.span.unwrap()
        );
        collector.save("automatic-failure", &[receipt]);
    })
    .await;
}
