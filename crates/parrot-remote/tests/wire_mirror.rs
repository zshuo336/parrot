use parrot_api::{
    address::ActorRef,
    message::RemoteMessage,
    trace::{TraceContext, TraceParent},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    frame::{flags, frame_type, Frame},
    ingress::{Ingress, LocalLookup, RelayRouter},
    mirror::{WireMirrorError, WireMirrorHandle, WireMirrorOptions, WireMirrorPolicy},
    CallbackRegistry, FrameSender,
};
use std::{
    any::Any,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};
#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Work(u64);
impl parrot_api::Message for Work {
    type Result = ();
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    path: String,
    value: u64,
    context: Option<TraceParent>,
    admission: bool,
}
#[derive(Debug, Default)]
struct State {
    urgent: AtomicUsize,
    seen: Mutex<Vec<Seen>>,
    mode: AtomicUsize,
    polls: AtomicUsize,
    future_drops: AtomicUsize,
    target_drops: AtomicUsize,
    generation: AtomicUsize,
    generations: Mutex<Vec<usize>>,
}
#[derive(Debug)]
struct Target {
    state: Arc<State>,
    path: String,
    shadow: bool,
    generation: usize,
}
impl Drop for Target {
    fn drop(&mut self) {
        if self.shadow {
            self.state.target_drops.fetch_add(1, Ordering::SeqCst);
            if self.state.mode.load(Ordering::SeqCst) == 4 {
                panic!("shadow ref drop");
            }
        }
    }
}
#[derive(Debug)]
struct BadError;
impl std::fmt::Display for BadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("shadow error")
    }
}
impl std::error::Error for BadError {}
impl Drop for BadError {
    fn drop(&mut self) {
        panic!("shadow error drop");
    }
}
struct BadPayload;
impl Drop for BadPayload {
    fn drop(&mut self) {
        panic!("shadow panic payload drop");
    }
}
struct Admission {
    state: Arc<State>,
}
impl Future for Admission {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.state.polls.fetch_add(1, Ordering::SeqCst);
        match self.state.mode.load(Ordering::SeqCst) {
            1 | 6 => Poll::Pending,
            2 | 11 => panic!("shadow poll"),
            9 => Poll::Ready(Err(parrot_api::errors::ActorError::Other(BadError.into()))),
            10 => std::panic::panic_any(BadPayload),
            7 => Poll::Ready(Err(parrot_api::errors::ActorError::Stopped)),
            _ => Poll::Ready(Ok(())),
        }
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        self.state.future_drops.fetch_add(1, Ordering::SeqCst);
        if matches!(self.state.mode.load(Ordering::SeqCst), 3 | 6 | 11) {
            panic!("shadow future drop");
        }
    }
}
impl Target {
    fn observe(&self, message: &BoxedMessage, admission: bool) {
        let value = message
            .downcast_ref::<Work>()
            .expect("decoded business input")
            .0;
        self.state.seen.lock().unwrap().push(Seen {
            path: self.path.clone(),
            value,
            context: TraceContext::current().map(|c| c.as_parent()),
            admission,
        });
        if self.shadow {
            self.state.generations.lock().unwrap().push(self.generation);
        }
    }
}
#[async_trait::async_trait]
impl ActorRef for Target {
    fn send<'a>(&'a self, m: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.observe(&m, false);
            Ok(Box::new(Work(42)) as BoxedMessage)
        })
    }
    fn send_with_options<'a>(
        &'a self,
        m: BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        assert!(!self.shadow, "shadow must use ordinary admission");
        assert_eq!(options.priority, parrot_api::message::MessagePriority::HIGH);
        self.state.urgent.fetch_add(1, Ordering::SeqCst);
        self.send(m)
    }
    fn deliver_with_options<'a>(
        &'a self,
        m: BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        assert!(!self.shadow, "shadow must use ordinary admission");
        assert_eq!(options.priority, parrot_api::message::MessagePriority::HIGH);
        self.state.urgent.fetch_add(1, Ordering::SeqCst);
        self.deliver(m)
    }
    fn send_with_timeout<'a>(
        &'a self,
        m: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(m)
    }
    fn deliver<'a>(&'a self, m: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.observe(&m, true);
        drop(m);
        if self.shadow {
            assert_ne!(
                self.state.mode.load(Ordering::SeqCst),
                5,
                "shadow constructor"
            );
            Box::pin(Admission {
                state: self.state.clone(),
            })
        } else {
            Box::pin(async { Ok(()) })
        }
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn path(&self) -> String {
        self.path.clone()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            state: self.state.clone(),
            path: self.path.clone(),
            shadow: self.shadow,
            generation: self.generation,
        })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
struct Lookup {
    state: Arc<State>,
}
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        if path.starts_with("/shadow") && self.state.mode.load(Ordering::SeqCst) == 8 {
            panic!("shadow lookup");
        }
        if path == "/missing" {
            return None;
        }
        Some(Box::new(Target {
            state: self.state.clone(),
            path: path.into(),
            shadow: path.starts_with("/shadow"),
            generation: self.state.generation.load(Ordering::SeqCst),
        }))
    }
    fn forwarding_target(&self, path: &str) -> Option<String> {
        if path == "/app/gateway" || path == "/shadow-alias" {
            Some("parrot://other/remote".into())
        } else {
            None
        }
    }
}
struct Router(FrameSender);
impl RelayRouter for Router {
    fn next_cid(&self) -> u64 {
        1001
    }
    fn dial_addr_of(&self, _: &str) -> Option<String> {
        None
    }
    fn on_hint(&self, _: &str, _: &str) {}
    fn self_node(&self) -> &str {
        "here"
    }
    fn sender_of(&self, node: &str) -> Option<FrameSender> {
        (node == "other").then(|| self.0.clone())
    }
}
struct Harness {
    state: Arc<State>,
    ingress: Arc<Ingress>,
    back: FrameSender,
    replies: tokio::sync::mpsc::Receiver<Frame>,
}
impl Harness {
    fn new() -> Self {
        let state = Arc::new(State::default());
        let ingress = Arc::new(Ingress::new(
            Arc::new(Lookup {
                state: state.clone(),
            }),
            Arc::new(CallbackRegistry::default()),
        ));
        let (tx, replies) = tokio::sync::mpsc::channel(128);
        Self {
            state,
            ingress,
            back: FrameSender::anon(tx),
            replies,
        }
    }
    fn register(&self, source: &str, target: &str, options: WireMirrorOptions) -> WireMirrorHandle {
        self.ingress
            .register_wire_mirror(WireMirrorPolicy::new(source, target), options)
            .unwrap()
    }
    async fn send(&self, frame: Frame) {
        // Exercise the actual wire envelope, not only a policy callback.
        let mut bytes = bytes::BytesMut::new();
        frame.encode(&mut bytes).unwrap();
        let decoded = Frame::decode(&mut bytes).unwrap().unwrap();
        assert!(bytes.is_empty());
        self.ingress.dispatch(decoded, &self.back, "peer").await;
    }
    async fn reply(&mut self, cid: u64, completion: bool) {
        let reply = tokio::time::timeout(Duration::from_secs(3), self.replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.header.correlation_id, cid);
        assert_eq!(reply.header.frame_type, frame_type::REPLY);
        if completion {
            assert_eq!(reply.type_key, parrot_remote::frame::TELL_COMPLETION_KEY);
        } else {
            assert_eq!(
                parrot_api::message::serde_remote_deserialize::<Work>(&reply.payload)
                    .unwrap()
                    .0,
                42
            );
        }
    }
    async fn close(&self) {
        self.ingress.close();
        tokio::time::timeout(Duration::from_secs(3), self.ingress.wait_closed())
            .await
            .unwrap();
    }
    fn shadows(&self) -> Vec<Seen> {
        self.state
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.path.starts_with("/shadow"))
            .cloned()
            .collect()
    }
}
fn frame(path: &str, kind: u8) -> Frame {
    let data = parrot_api::message::serde_remote_serialize(&Work(17))
        .unwrap()
        .into();
    match kind {
        0 => Frame::ask(
            71,
            path,
            Work::TYPE_KEY,
            data,
            Some("parrot://peer/replies"),
        ),
        1 => Frame::tell(path, Work::TYPE_KEY, data),
        _ => Frame::tell_with_ack(72, path, Work::TYPE_KEY, data),
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    // Paused Tokio time cannot bound a continuously-ready yield loop when the
    // implementation is broken. Keep an independent wall-clock failure bound.
    let wall_deadline = std::time::Instant::now() + Duration::from_secs(3);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            assert!(
                std::time::Instant::now() < wall_deadline,
                "no progress before wall-clock deadline"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("observable progress");
}
async fn settled(h: &WireMirrorHandle, n: u64) {
    until(|| {
        let s = h.stats();
        s.delivered + s.failed + s.timed_out + s.cancelled == n
    })
    .await;
}
async fn semantics(kind: u8) {
    let mut h = Harness::new();
    let policy = h.register("/app", "/shadow", WireMirrorOptions::default());
    let mut f = frame("/app/worker", kind);
    f.header.flags |= flags::URGENT;
    h.send(f).await;
    if kind != 1 {
        h.reply(if kind == 0 { 71 } else { 72 }, kind == 2).await;
    }
    settled(&policy, 1).await;
    assert_eq!(
        h.shadows(),
        vec![Seen {
            path: "/shadow".into(),
            value: 17,
            context: None,
            admission: true
        }]
    );
    assert!(h.replies.try_recv().is_err());
    assert_eq!(policy.stats().delivered, 1);
    assert_eq!(h.state.urgent.load(Ordering::SeqCst), 1);
    h.close().await;
}
#[tokio::test]
async fn ask_strips_reply_to_and_keeps_primary_reply() {
    semantics(0).await;
}
#[tokio::test]
async fn tell_creates_one_ordinary_shadow_admission() {
    semantics(1).await;
}
#[tokio::test]
async fn completion_tell_does_not_wait_for_shadow_completion() {
    semantics(2).await;
}
#[tokio::test]
async fn slow_shadow_does_not_hold_primary_reply_and_close_retires_future() {
    let mut h = Harness::new();
    h.state.mode.store(1, Ordering::SeqCst);
    let p = h.register(
        "/app",
        "/shadow",
        WireMirrorOptions {
            delivery_timeout: Duration::from_secs(60),
            ..Default::default()
        },
    );
    h.send(frame("/app/x", 0)).await;
    h.reply(71, false).await;
    until(|| h.state.polls.load(Ordering::SeqCst) > 0).await;
    assert_eq!(p.stats().delivered, 0);
    h.close().await;
    assert!(p.is_closed());
    assert_eq!(p.stats().cancelled, 1);
    assert_eq!(h.state.future_drops.load(Ordering::SeqCst), 1);
    assert_eq!(h.state.target_drops.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn pending_count_includes_active_and_rejected_shadows_keep_primary_progress() {
    let h = Harness::new();
    h.state.mode.store(1, Ordering::SeqCst);
    let p = h.register(
        "/app",
        "/shadow",
        WireMirrorOptions {
            capacity: 2,
            delivery_timeout: Duration::from_secs(60),
            ..Default::default()
        },
    );
    h.send(frame("/app/x", 1)).await;
    until(|| h.state.polls.load(Ordering::SeqCst) > 0).await;
    for _ in 0..4 {
        h.send(frame("/app/x", 1)).await;
    }
    assert_eq!(p.stats().enqueued, 2);
    assert_eq!(p.stats().full, 3);
    assert_eq!(
        h.state
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.path == "/app/x")
            .count(),
        5
    );
    h.close().await;
    assert_eq!(p.stats().cancelled, 2);
}
#[tokio::test]
async fn byte_budget_counts_payload_path_and_key_until_retirement() {
    let h = Harness::new();
    h.state.mode.store(1, Ordering::SeqCst);
    let size = frame("/app/x", 1).payload.len() + "/app/x".len() + Work::TYPE_KEY.len();
    let p = h.register(
        "/app",
        "/shadow",
        WireMirrorOptions {
            capacity: 10,
            max_message_bytes: size,
            max_pending_bytes: size,
            delivery_timeout: Duration::from_secs(60),
        },
    );
    h.send(frame("/app/x", 1)).await;
    h.send(frame("/app/x", 1)).await;
    assert_eq!(p.stats().enqueued, 1);
    assert_eq!(p.stats().full, 1);
    h.close().await;
    assert_eq!(p.stats().cancelled, 1);
}
#[tokio::test]
async fn oversized_shadow_is_not_copied_or_decoded() {
    let h = Harness::new();
    let p = h.register(
        "/app",
        "/shadow",
        WireMirrorOptions {
            max_message_bytes: 1,
            ..Default::default()
        },
    );
    h.send(frame("/app/x", 1)).await;
    h.close().await;
    assert_eq!(p.stats().oversized, 1);
    assert_eq!(p.stats().enqueued, 0);
    assert!(h.shadows().is_empty());
}
#[tokio::test(start_paused = true)]
async fn deadline_includes_queue_wait_and_drops_inflight_admission() {
    let h = Harness::new();
    h.state.mode.store(1, Ordering::SeqCst);
    let p = h.register("/app", "/shadow", WireMirrorOptions::default());
    h.send(frame("/app/x", 1)).await;
    until(|| h.state.polls.load(Ordering::SeqCst) > 0).await;
    h.send(frame("/app/x", 1)).await;
    tokio::time::advance(Duration::from_millis(101)).await;
    settled(&p, 2).await;
    assert_eq!(p.stats().timed_out, 2);
    assert_eq!(h.shadows().len(), 1);
    assert_eq!(h.state.future_drops.load(Ordering::SeqCst), 1);
    h.close().await;
}
#[tokio::test]
async fn longest_segment_prefix_wins_without_catching_namesakes() {
    let h = Harness::new();
    let broad = h.register("/app", "/shadow-broad", WireMirrorOptions::default());
    let narrow = h.register("/app/x", "/shadow-narrow", WireMirrorOptions::default());
    for path in ["/application/x", "/app/x/child", "/app/xy"] {
        h.send(frame(path, 1)).await;
    }
    settled(&broad, 1).await;
    settled(&narrow, 1).await;
    assert_eq!(h.shadows().len(), 2);
    h.close().await;
}
#[tokio::test]
async fn replacement_cancels_old_work_without_old_handle_cancelling_new_policy() {
    let h = Harness::new();
    h.state.mode.store(1, Ordering::SeqCst);
    let old = h.register("/app", "/shadow-old", WireMirrorOptions::default());
    h.send(frame("/app/x", 1)).await;
    until(|| h.state.polls.load(Ordering::SeqCst) > 0).await;
    let new = h.register("/app", "/shadow-new", WireMirrorOptions::default());
    old.wait_closed().await;
    old.close();
    h.state.mode.store(0, Ordering::SeqCst);
    h.send(frame("/app/x", 1)).await;
    settled(&new, 1).await;
    assert_eq!(old.stats().cancelled, 1);
    assert_eq!(new.stats().delivered, 1);
    h.close().await;
}
#[tokio::test]
async fn failed_replacement_leaves_original_policy_usable() {
    let h = Harness::new();
    let old = h.register("/app", "/shadow-old", WireMirrorOptions::default());
    assert!(matches!(
        h.ingress.register_wire_mirror(
            WireMirrorPolicy::new("/app", "/shadow-new"),
            WireMirrorOptions {
                capacity: 0,
                ..Default::default()
            }
        ),
        Err(WireMirrorError::InvalidOptions)
    ));
    h.send(frame("/app/x", 1)).await;
    settled(&old, 1).await;
    assert_eq!(old.stats().delivered, 1);
    h.close().await;
}
#[tokio::test]
async fn removal_receipt_fences_admission() {
    let h = Harness::new();
    let p = h.register("/app", "/shadow", WireMirrorOptions::default());
    let removed = h.ingress.remove_wire_mirror("/app").unwrap();
    removed.wait_closed().await;
    h.send(frame("/app/x", 1)).await;
    assert_eq!(p.stats().enqueued, 0);
    assert!(h.ingress.remove_wire_mirror("/app").is_none());
    h.close().await;
}
#[tokio::test]
async fn policy_workers_include_retiring_generations_in_quota() {
    let h = Harness::new();
    let mut policies = Vec::new();
    for n in 0..64 {
        policies.push(h.register(
            "/app",
            &format!("/shadow-{n}"),
            WireMirrorOptions::default(),
        ));
    }
    assert!(matches!(
        h.ingress.register_wire_mirror(
            WireMirrorPolicy::new("/app", "/shadow-last"),
            WireMirrorOptions::default()
        ),
        Err(WireMirrorError::RegistryFull)
    ));
    for p in &policies[..63] {
        p.wait_closed().await;
    }
    let last = h.register("/app", "/shadow-last", WireMirrorOptions::default());
    h.send(frame("/app/x", 1)).await;
    settled(&last, 1).await;
    h.close().await;
}
#[tokio::test]
async fn registry_closure_is_permanent() {
    let h = Harness::new();
    h.close().await;
    assert!(matches!(
        h.ingress.register_wire_mirror(
            WireMirrorPolicy::new("/app", "/shadow"),
            WireMirrorOptions::default()
        ),
        Err(WireMirrorError::Closed)
    ));
}
#[test]
fn unpolled_runtime_shutdown_publishes_worker_receipt() {
    let h = Harness::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let p = {
        let _entered = runtime.enter();
        h.register("/app", "/shadow", WireMirrorOptions::default())
    };
    assert!(!p.is_closed());
    drop(runtime);
    assert!(p.is_closed());
}
#[test]
fn registration_requires_runtime_valid_paths_and_bound_remote_system() {
    let h = Harness::new();
    assert!(matches!(
        h.ingress.register_wire_mirror(
            WireMirrorPolicy::new("/app", "/shadow"),
            WireMirrorOptions::default()
        ),
        Err(WireMirrorError::NoRuntime)
    ));
    assert!(matches!(
        h.ingress.register_wire_mirror(
            WireMirrorPolicy::new("/app", "parrot://elsewhere/shadow"),
            WireMirrorOptions::default()
        ),
        Err(WireMirrorError::RemoteUnavailable)
    ));
    for path in [
        "//shadow",
        "/a/../shadow",
        "/shadow/",
        "/shadow?x",
        "/shadow\n",
    ] {
        assert!(matches!(
            h.ingress.register_wire_mirror(
                WireMirrorPolicy::new("/app", path),
                WireMirrorOptions::default()
            ),
            Err(WireMirrorError::InvalidPolicy)
        ));
    }
}
#[tokio::test]
async fn forwarding_alias_matches_original_path_and_preserves_raw_primary_frame() {
    let h = Harness::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    *h.ingress.relay_slot() = Some(Arc::new(Router(FrameSender::anon(tx))));
    let p = h.register("/app", "/shadow", WireMirrorOptions::default());
    let original = frame("/app/gateway", 0);
    h.send(original.clone()).await;
    let forwarded = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(forwarded.path, "parrot://other/remote");
    assert_eq!(forwarded.payload, original.payload);
    assert_eq!(forwarded.type_key, original.type_key);
    settled(&p, 1).await;
    assert_eq!(p.stats().delivered, 1);
    h.close().await;
}
#[tokio::test]
async fn transit_is_not_mirrored_but_self_qualified_destination_is() {
    let h = Harness::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    *h.ingress.relay_slot() = Some(Arc::new(Router(FrameSender::anon(tx))));
    let p = h.register("/app", "/shadow", WireMirrorOptions::default());
    h.send(frame("parrot://other/app/x", 1)).await;
    assert!(rx.recv().await.is_some());
    assert_eq!(p.stats().enqueued, 0);
    h.send(frame("parrot://here/app/x", 1)).await;
    settled(&p, 1).await;
    assert_eq!(p.stats().delivered, 1);
    h.close().await;
}
#[tokio::test]
async fn unknown_node_identity_cannot_authorize_mirroring() {
    let h = Harness::new();
    let p = h.register("/app", "/shadow", WireMirrorOptions::default());
    h.send(frame("parrot://unknown/app/x", 1)).await;
    h.close().await;
    assert_eq!(p.stats().enqueued, 0);
}
#[tokio::test]
async fn shadow_alias_and_missing_target_fail_only_shadow() {
    for target in ["/shadow-alias", "/missing"] {
        let mut h = Harness::new();
        let p = h.register("/app", target, WireMirrorOptions::default());
        h.send(frame("/app/x", 0)).await;
        h.reply(71, false).await;
        settled(&p, 1).await;
        assert_eq!(p.stats().failed, 1);
        assert!(h.shadows().is_empty());
        h.close().await;
    }
}
#[tokio::test]
async fn shadow_does_not_reenter_ingress_or_mirror_itself() {
    let h = Harness::new();
    let p = h.register("/", "/shadow", WireMirrorOptions::default());
    h.send(frame("/app/x", 1)).await;
    settled(&p, 1).await;
    h.send(frame("/shadow", 1)).await;
    h.close().await;
    assert_eq!(p.stats().enqueued, 1);
    assert_eq!(h.shadows().len(), 2);
}
#[tokio::test]
async fn controls_do_not_create_shadows() {
    for key in [
        parrot_remote::roles::directory::client::READ_KEY,
        parrot_remote::roles::directory::client::CLIENT_KEY,
        parrot_remote::roles::directory::network::RAFT_KEY,
    ] {
        let h = Harness::new();
        let p = h.register("/", "/shadow", WireMirrorOptions::default());
        let mut f = frame("/app/x", 1);
        f.type_key = key.into();
        h.send(f).await;
        assert!(!h.back.is_closed());
        assert_eq!(p.stats().enqueued, 0);
        h.close().await;
    }
    let mut h = Harness::new();
    let p = h.register("/", "/shadow", WireMirrorOptions::default());
    let mut f = frame("/app/x", 2);
    f.type_key = parrot_remote::durable::DURABLE_KEY.into();
    h.send(f).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if h.back.is_closed() || h.replies.try_recv().is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(p.stats().enqueued, 0);
    h.close().await;
}
#[tokio::test]
async fn malformed_ask_is_rejected_without_shadow_admission() {
    let mut h = Harness::new();
    let p = h.register("/", "/shadow", WireMirrorOptions::default());
    let mut bad = frame("/app/x", 0);
    bad.payload = bytes::Bytes::from_static(&[255, 255]);
    h.send(bad).await;
    let reply = tokio::time::timeout(Duration::from_secs(3), h.replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.header.frame_type, frame_type::REPLY_ERR);
    assert_eq!(reply.header.correlation_id, 71);
    assert_eq!(p.stats().enqueued, 0);
    h.close().await;
}
#[tokio::test]
async fn raw_lookup_selects_current_generation_for_each_admission() {
    let h = Harness::new();
    let p = h.register(
        "/app",
        "/shadow",
        WireMirrorOptions {
            capacity: 1,
            ..Default::default()
        },
    );
    h.send(frame("/app/x", 1)).await;
    settled(&p, 1).await;
    h.state.generation.store(2, Ordering::SeqCst);
    h.send(frame("/app/x", 1)).await;
    settled(&p, 2).await;
    assert_eq!(*h.state.generations.lock().unwrap(), vec![0, 2]);
    h.close().await;
}
async fn isolation(mode: usize, success: bool) {
    let mut h = Harness::new();
    h.state.mode.store(mode, Ordering::SeqCst);
    let p = h.register("/app", "/shadow", WireMirrorOptions::default());
    h.send(frame("/app/x", 0)).await;
    h.reply(71, false).await;
    settled(&p, 1).await;
    let s = p.stats();
    assert_eq!(s.panicked_attempts, u64::from(mode != 7));
    assert_eq!(s.delivered, u64::from(success));
    assert_eq!(s.failed, u64::from(!success));
    assert!(!h.back.is_closed());
    h.close().await;
}
#[tokio::test]
async fn shadow_constructor_panic_is_isolated() {
    isolation(5, false).await;
}
#[tokio::test]
async fn shadow_poll_panic_is_isolated() {
    isolation(2, false).await;
}
#[tokio::test]
async fn shadow_future_drop_panic_preserves_admission_fact() {
    isolation(3, true).await;
}
#[tokio::test]
async fn shadow_reference_drop_panic_preserves_admission_fact() {
    isolation(4, true).await;
}
#[tokio::test]
async fn shadow_lookup_panic_is_isolated() {
    isolation(8, false).await;
}
#[tokio::test]
async fn shadow_rejection_does_not_replace_primary_reply() {
    isolation(7, false).await;
}
#[tokio::test]
async fn cancelled_future_drop_panic_is_isolated_before_close_receipt() {
    let h = Harness::new();
    h.state.mode.store(6, Ordering::SeqCst);
    let p = h.register("/app", "/shadow", WireMirrorOptions::default());
    h.send(frame("/app/x", 1)).await;
    until(|| h.state.polls.load(Ordering::SeqCst) > 0).await;
    h.close().await;
    assert_eq!(p.stats().cancelled, 1);
    assert_eq!(p.stats().panicked_attempts, 1);
    assert_eq!(h.state.future_drops.load(Ordering::SeqCst), 1);
    assert_eq!(h.state.target_drops.load(Ordering::SeqCst), 1);
}
#[test]
fn runtime_shutdown_retires_active_future_even_when_its_destructor_panics() {
    let h = Harness::new();
    h.state.mode.store(6, Ordering::SeqCst);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let p = runtime.block_on(async {
        let p = h.register(
            "/app",
            "/shadow",
            WireMirrorOptions {
                delivery_timeout: Duration::from_secs(60),
                ..Default::default()
            },
        );
        h.send(frame("/app/x", 1)).await;
        until(|| h.state.polls.load(Ordering::SeqCst) > 0).await;
        p
    });
    drop(runtime);
    assert!(p.is_closed());
    assert_eq!(p.stats().cancelled, 1);
    assert_eq!(p.stats().panicked_attempts, 1);
    assert_eq!(h.state.future_drops.load(Ordering::SeqCst), 1);
    assert_eq!(h.state.target_drops.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn unknown_shadow_codec_does_not_block_opaque_gateway_forwarding() {
    let h = Harness::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    *h.ingress.relay_slot() = Some(Arc::new(Router(FrameSender::anon(tx))));
    let p = h.register("/app", "/shadow", WireMirrorOptions::default());
    let mut original = frame("/app/gateway", 1);
    original.type_key = "bin:opaque-foreign-type#v1".into();
    h.send(original.clone()).await;
    let forwarded = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(forwarded.payload, original.payload);
    assert_eq!(forwarded.type_key, original.type_key);
    settled(&p, 1).await;
    assert_eq!(p.stats().failed, 1);
    assert!(!h.back.is_closed());
    h.close().await;
}

#[tokio::test]
async fn shadow_error_destructor_panic_is_isolated() {
    isolation(9, false).await;
}
#[tokio::test]
async fn shadow_panic_payload_destructor_is_isolated() {
    isolation(10, false).await;
}
#[tokio::test]
async fn shadow_poll_and_future_destructor_panics_are_independent() {
    isolation(11, false).await;
}
#[test]
fn dead_runtime_policy_cannot_change_stats_after_its_retirement_receipt() {
    let h = Harness::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let p = {
        let _entered = runtime.enter();
        h.register("/app", "/shadow", WireMirrorOptions::default())
    };
    drop(runtime);
    assert!(p.is_closed());
    let stats = p.stats();
    let replacement = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    replacement.block_on(async {
        h.send(frame("/app/x", 1)).await;
        h.close().await;
    });
    assert_eq!(p.stats(), stats);
}

#[tokio::test]
async fn handle_close_receipt_waits_for_inflight_future_and_target_disposal() {
    let h = Harness::new();
    h.state.mode.store(1, Ordering::SeqCst);
    let p = h.register(
        "/app",
        "/shadow",
        WireMirrorOptions {
            delivery_timeout: Duration::from_secs(60),
            ..Default::default()
        },
    );
    h.send(frame("/app/x", 1)).await;
    until(|| h.state.polls.load(Ordering::SeqCst) > 0).await;
    p.close();
    p.wait_closed().await;
    assert!(p.is_closed());
    assert_eq!(h.state.future_drops.load(Ordering::SeqCst), 1);
    assert_eq!(h.state.target_drops.load(Ordering::SeqCst), 1);
    let stats = p.stats();
    h.send(frame("/app/x", 1)).await;
    assert_eq!(p.stats(), stats);
    h.close().await;
}

#[tokio::test]
async fn closed_registered_policies_cannot_accumulate_unbounded_metadata() {
    let h = Harness::new();
    for n in 0..64 {
        let p = h.register(
            &format!("/app/{n}"),
            "/shadow",
            WireMirrorOptions::default(),
        );
        p.close();
        p.wait_closed().await;
    }
    assert!(matches!(
        h.ingress.register_wire_mirror(
            WireMirrorPolicy::new("/another", "/shadow"),
            WireMirrorOptions::default()
        ),
        Err(WireMirrorError::RegistryFull)
    ));
    h.ingress
        .remove_wire_mirror("/app/0")
        .unwrap()
        .wait_closed()
        .await;
    let next = h.register("/another", "/shadow", WireMirrorOptions::default());
    h.send(frame("/another", 1)).await;
    settled(&next, 1).await;
    assert_eq!(next.stats().delivered, 1);
    h.close().await;
}

// Local encoding shares the Wire policy worker and budgets, but never runs user
// code while holding registry/admission locks.
struct Encode(Box<dyn Fn() -> Result<Vec<u8>, String> + Send + Sync>);
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:mirror.local#v1",
        type_id: std::any::TypeId::of::<Encode>(),
        encode: |m| (m.downcast_ref::<Encode>().unwrap().0)(),
        decode: |b| Ok(Box::new(parrot_api::message::serde_remote_deserialize::<Work>(b)?) as BoxedMessage),
    }
}
fn encoded(f: impl Fn() -> Result<Vec<u8>, String> + Send + Sync + 'static) -> BoxedMessage {
    Box::new(Encode(Box::new(f)))
}
fn work_bytes() -> Result<Vec<u8>, String> {
    parrot_api::message::serde_remote_serialize(&Work(91))
}
fn group_policy(source: &str) -> (WireMirrorPolicy, WireMirrorOptions) {
    (
        WireMirrorPolicy::new(source, "/shadow"),
        WireMirrorOptions::default(),
    )
}
#[tokio::test]
async fn local_codec_and_wire_share_one_budget_and_stats() {
    let h = Harness::new();
    let p = h.register(
        "/app",
        "/shadow",
        WireMirrorOptions {
            capacity: 1,
            ..Default::default()
        },
    );
    p.offer_local("/app", &encoded(work_bytes));
    h.send(frame("/app", 1)).await;
    assert_eq!(p.stats().enqueued, 1);
    assert_eq!(p.stats().full, 1);
    until(|| p.stats().delivered == 1).await;
    assert_eq!(h.shadows()[0].value, 91);
    h.close().await;
}
#[tokio::test]
async fn missing_failing_and_panicking_local_codecs_are_isolated() {
    let h = Harness::new();
    let p = h.register("/app", "/shadow", Default::default());
    let unknown: BoxedMessage = Box::new(());
    p.offer_local("/app", &unknown);
    p.offer_local("/app", &encoded(|| Err("encode failure".into())));
    p.offer_local("/app", &encoded(|| std::panic::panic_any(BadPayload)));
    p.offer_local("/app", &encoded(work_bytes));
    until(|| p.stats().delivered == 1).await;
    assert_eq!(p.stats().copy_failed, 3);
    assert_eq!(p.stats().panicked_attempts, 1);
    h.close().await;
}
#[tokio::test]
async fn full_local_queue_never_calls_encoder() {
    let h = Harness::new();
    let p = h.register(
        "/app",
        "/shadow",
        WireMirrorOptions {
            capacity: 1,
            ..Default::default()
        },
    );
    p.offer_local("/app", &encoded(work_bytes));
    let called = Arc::new(AtomicUsize::new(0));
    let c = called.clone();
    p.offer_local(
        "/app",
        &encoded(move || {
            c.fetch_add(1, Ordering::SeqCst);
            work_bytes()
        }),
    );
    assert_eq!(called.load(Ordering::SeqCst), 0);
    assert_eq!(p.stats().full, 1);
    h.close().await;
}
#[tokio::test]
async fn local_copy_reserves_maximum_then_refunds_unused_bytes() {
    let h = Harness::new();
    let p = h.register(
        "/app",
        "/shadow",
        WireMirrorOptions {
            max_message_bytes: 128,
            max_pending_bytes: 180,
            ..Default::default()
        },
    );
    p.offer_local("/app", &encoded(work_bytes));
    p.offer_local("/app", &encoded(work_bytes));
    assert_eq!(
        p.stats().enqueued,
        2,
        "unused first reservation must be returned before delivery"
    );
    h.close().await;
}
#[tokio::test]
async fn oversized_local_encoding_releases_all_reservations() {
    let h = Harness::new();
    let p = h.register(
        "/app",
        "/shadow",
        WireMirrorOptions {
            capacity: 1,
            max_message_bytes: 128,
            max_pending_bytes: 128,
            ..Default::default()
        },
    );
    p.offer_local("/app", &encoded(|| Ok(vec![0; 129])));
    p.offer_local("/app", &encoded(work_bytes));
    assert_eq!(p.stats().oversized, 1);
    assert_eq!(p.stats().enqueued, 1);
    h.close().await;
}
#[tokio::test]
async fn encoder_can_close_its_policy_without_lock_reentry_deadlock() {
    let h = Harness::new();
    let p = h.register("/app", "/shadow", Default::default());
    let close = p.clone();
    p.offer_local(
        "/app",
        &encoded(move || {
            close.close();
            work_bytes()
        }),
    );
    p.wait_closed().await;
    assert_eq!(p.stats().enqueued, 0);
    assert!(h.shadows().is_empty());
    h.close().await;
}
#[tokio::test]
async fn encoder_can_replace_its_policy_without_rebinding_old_copy() {
    let h = Harness::new();
    let p = h.register("/app", "/shadow-old", Default::default());
    let ingress = h.ingress.clone();
    p.offer_local(
        "/app",
        &encoded(move || {
            ingress
                .register_wire_mirror(
                    WireMirrorPolicy::new("/app", "/shadow-new"),
                    Default::default(),
                )
                .unwrap();
            work_bytes()
        }),
    );
    p.wait_closed().await;
    assert_eq!(p.stats().enqueued, 0);
    h.send(frame("/app", 1)).await;
    until(|| h.shadows().len() == 1).await;
    assert_eq!(h.shadows()[0].path, "/shadow-new");
    h.close().await;
}
#[tokio::test]
async fn local_encoding_time_counts_toward_shadow_deadline() {
    let h = Harness::new();
    let p = h.register(
        "/app",
        "/shadow",
        WireMirrorOptions {
            delivery_timeout: Duration::from_millis(1),
            ..Default::default()
        },
    );
    p.offer_local(
        "/app",
        &encoded(|| {
            std::thread::sleep(Duration::from_millis(10));
            work_bytes()
        }),
    );
    until(|| p.stats().timed_out == 1).await;
    assert!(h.shadows().is_empty());
    h.close().await;
}
#[test]
fn closed_receipt_waits_for_encoder_after_runtime_destroyed() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = runtime.block_on(async { Harness::new() });
    let p = runtime.block_on(async { h.register("/app", "/shadow", Default::default()) });
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let (e, r, copy) = (entered.clone(), release.clone(), p.clone());
    let thread = std::thread::spawn(move || {
        copy.offer_local(
            "/app",
            &encoded(move || {
                e.wait();
                r.wait();
                Err("late failure".into())
            }),
        )
    });
    entered.wait();
    drop(runtime);
    assert!(
        !p.is_closed(),
        "unpolled worker drop must retain encoder ownership"
    );
    release.wait();
    thread.join().unwrap();
    assert!(p.is_closed());
    assert_eq!(p.stats().copy_failed, 1);
    let stats = p.stats();
    p.offer_local("/app", &encoded(work_bytes));
    assert_eq!(p.stats(), stats);
}
#[tokio::test]
async fn group_validation_does_not_partially_install_or_replace() {
    let h = Harness::new();
    let old = h.register("/existing", "/shadow-old", Default::default());
    assert!(matches!(
        h.ingress
            .register_wire_mirror_group(vec![group_policy("/new"), group_policy("/existing")]),
        Err(WireMirrorError::Conflict)
    ));
    assert!(h.ingress.remove_wire_mirror("/new").is_none());
    assert!(!old.is_closed());
    let invalid = (WireMirrorPolicy::new("bad", "/shadow"), Default::default());
    assert!(matches!(
        h.ingress
            .register_wire_mirror_group(vec![group_policy("/new"), invalid]),
        Err(WireMirrorError::InvalidPolicy)
    ));
    assert!(h.ingress.remove_wire_mirror("/new").is_none());
    h.close().await;
}
#[tokio::test]
async fn group_duplicate_sources_are_rejected_before_publication() {
    let h = Harness::new();
    assert!(matches!(
        h.ingress
            .register_wire_mirror_group(vec![group_policy("/app"), group_policy("/app")]),
        Err(WireMirrorError::Conflict)
    ));
    assert!(h.ingress.remove_wire_mirror("/app").is_none());
    h.close().await;
}
#[tokio::test]
async fn old_group_drop_preserves_replacement_generation() {
    let h = Harness::new();
    let group = h
        .ingress
        .register_wire_mirror_group(vec![group_policy("/app")])
        .unwrap();
    let old = group.handle_for("/app").unwrap();
    let new = h.register("/app", "/shadow-new", Default::default());
    drop(group);
    old.wait_closed().await;
    new.offer_local("/app", &encoded(work_bytes));
    until(|| new.stats().delivered == 1).await;
    h.send(frame("/app", 1)).await;
    until(|| new.stats().delivered == 2).await;
    assert_eq!(h.shadows().len(), 2);
    assert!(h.shadows().iter().all(|seen| seen.path == "/shadow-new"));
    h.close().await;
}
#[tokio::test]
async fn group_drop_removes_exact_sources_and_fences_retained_handles() {
    let h = Harness::new();
    let group = h
        .ingress
        .register_wire_mirror_group(vec![group_policy("/app")])
        .unwrap();
    let handle = group.handle_for("/app").unwrap();
    drop(group);
    handle.offer_local("/app", &encoded(work_bytes));
    tokio::time::timeout(Duration::from_secs(3), handle.wait_closed())
        .await
        .unwrap();
    assert_eq!(handle.stats().enqueued, 0);
    assert!(h.ingress.remove_wire_mirror("/app").is_none());
    h.close().await;
}
#[tokio::test]
async fn group_reserves_all_worker_slots_before_publishing() {
    let h = Harness::new();
    let mut existing = Vec::new();
    for i in 0..63 {
        existing.push(h.register(&format!("/p{i}"), "/shadow", Default::default()));
    }
    assert!(matches!(
        h.ingress
            .register_wire_mirror_group(vec![group_policy("/a"), group_policy("/b")]),
        Err(WireMirrorError::RegistryFull)
    ));
    assert!(h.ingress.remove_wire_mirror("/a").is_none());
    assert!(h.ingress.remove_wire_mirror("/b").is_none());
    let last = h.register("/last", "/shadow", Default::default());
    assert!(!last.is_closed());
    h.close().await;
}
#[tokio::test]
async fn closed_group_releases_registration_and_worker_capacity() {
    let h = Harness::new();
    let policies = || (0..64).map(|i| group_policy(&format!("/p{i}"))).collect();
    let first = h.ingress.register_wire_mirror_group(policies()).unwrap();
    first.close().await;
    let next = h.ingress.register_wire_mirror_group(policies()).unwrap();
    drop(first);
    let handle = next.handle_for("/p63").unwrap();
    handle.offer_local("/p63", &encoded(work_bytes));
    until(|| handle.stats().delivered == 1).await;
    next.close().await;
    h.close().await;
}
#[test]
fn retirement_waker_can_reenter_policy_close() {
    struct CloseOnWake(WireMirrorHandle);
    impl futures::task::ArcWake for CloseOnWake {
        fn wake_by_ref(owner: &Arc<Self>) {
            owner.0.close();
        }
    }
    let (done, receive) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let h = runtime.block_on(async { Harness::new() });
        let policy = runtime.block_on(async { h.register("/app", "/shadow", Default::default()) });
        let mut wait = Box::pin(policy.wait_closed());
        let waker = futures::task::waker(Arc::new(CloseOnWake(policy.clone())));
        assert!(wait
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        policy.close();
        runtime.block_on(h.close());
        assert!(policy.is_closed());
        done.send(()).unwrap();
    });
    // A lock reentry defect blocks the runtime itself, so a Tokio timeout cannot
    // serve as an independent watchdog. The owning test process bounds this thread.
    receive
        .recv_timeout(Duration::from_secs(3))
        .expect("receipt wake deadlocked against admission lock");
    thread.join().unwrap();
}
