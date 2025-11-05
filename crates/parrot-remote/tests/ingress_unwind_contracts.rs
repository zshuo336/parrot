//! User poll and independent cleanup failures must not abort ingress or publish success.
use parrot_api::{
    address::ActorRef,
    errors::ActorError,
    message::RemoteMessage,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    frame::{frame_type, Frame},
    ingress::{Ingress, LocalLookup},
    registry::CallbackRegistry,
    transport::FrameSender,
};
use std::{
    any::Any,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
static DECODED_DROPS: AtomicUsize = AtomicUsize::new(0);
#[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
struct Work(u8);
impl parrot_api::Message for Work {
    type Result = ();
}
impl Drop for Work {
    fn drop(&mut self) {
        if matches!(self.0, 11 | 12 | 14) {
            DECODED_DROPS.fetch_add(1, Ordering::SeqCst);
            panic!("decoded input drop");
        }
    }
}
#[derive(Debug, Default)]
struct State {
    polls: AtomicUsize,
    futures: AtomicUsize,
    references: AtomicUsize,
    replies: AtomicUsize,
    payloads: AtomicUsize,
    secondaries: AtomicUsize,
}
#[derive(Debug)]
struct Payload(Arc<State>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Secondary(self.0.clone()));
    }
}
#[derive(Debug)]
struct Secondary(Arc<State>);
impl Drop for Secondary {
    fn drop(&mut self) {
        self.0.secondaries.fetch_add(1, Ordering::SeqCst);
        panic!("recursive secondary payload drop");
    }
}
fn fail(state: &Arc<State>) -> ! {
    std::panic::panic_any(Payload(state.clone()));
}
struct Reply(Arc<State>);
inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:ingress-unwind-reply#v1",
        type_id: std::any::TypeId::of::<Reply>(),
        encode: |value| fail(&value.downcast_ref::<Reply>().unwrap().0),
        decode: |_| Err("reply decode intentionally unavailable".into()),
    }
}
impl Drop for Reply {
    fn drop(&mut self) {
        self.0.replies.fetch_add(1, Ordering::SeqCst);
        fail(&self.0);
    }
}
// This codec succeeds; only retiring its result fails. A failing encoder
// or reference destructor must not mask publication before reply cleanup.
struct EncodableReply(Arc<State>);
inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:ingress-unwind-encodable-reply#v1",
        type_id: std::any::TypeId::of::<EncodableReply>(),
        encode: |_| Ok(vec![42]),
        decode: |_| Err("reply decode intentionally unavailable".into()),
    }
}
impl Drop for EncodableReply {
    fn drop(&mut self) {
        self.0.replies.fetch_add(1, Ordering::SeqCst);
        fail(&self.0);
    }
}
#[derive(Debug)]
struct BadError(Arc<State>);
impl std::fmt::Display for BadError {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fail(&self.0)
    }
}
impl std::error::Error for BadError {}
impl Drop for BadError {
    fn drop(&mut self) {
        fail(&self.0);
    }
}
struct WorkFuture {
    mode: u8,
    state: Arc<State>,
}
impl Future for WorkFuture {
    type Output = ActorResult<BoxedMessage>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.state.polls.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            1 | 8 => fail(&self.state),
            3 => Poll::Pending,
            4 | 16 => Poll::Ready(Ok(Box::new(Reply(self.state.clone())))),
            18 => Poll::Ready(Err(ActorError::Stopped)),
            19 => Poll::Ready(Ok(Box::new(EncodableReply(self.state.clone())))),
            5 => Poll::Ready(Err(ActorError::Other(BadError(self.state.clone()).into()))),
            _ => Poll::Ready(Ok(Box::new(Work(0)))),
        }
    }
}
impl Drop for WorkFuture {
    fn drop(&mut self) {
        self.state.futures.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, 2 | 3 | 8 | 17) {
            fail(&self.state);
        }
    }
}
// Keep the independently failing future as stored state. An async wrapper
// which destroys its awaited child while that child's poll is unwinding would
// deliberately abort inside user code, before ingress can regain control.
struct AdmissionFuture(WorkFuture);
impl Future for AdmissionFuture {
    type Output = ActorResult<()>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|result| result.map(drop))
    }
}

#[derive(Debug)]
struct Target {
    mode: u8,
    state: Arc<State>,
}
impl Drop for Target {
    fn drop(&mut self) {
        self.state.references.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, 2..=8 | 18) {
            fail(&self.state);
        }
    }
}
#[async_trait::async_trait]
impl ActorRef for Target {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        drop(message);
        if self.mode == 6 {
            fail(&self.state);
        }
        Box::pin(WorkFuture {
            mode: self.mode,
            state: self.state.clone(),
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        drop(message);
        if self.mode == 6 {
            fail(&self.state);
        }
        Box::pin(AdmissionFuture(WorkFuture {
            mode: self.mode,
            state: self.state.clone(),
        }))
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(AdmissionFuture(WorkFuture {
            mode: self.mode,
            state: self.state.clone(),
        }))
    }
    fn path(&self) -> String {
        "/target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Target {
            mode: self.mode,
            state: self.state.clone(),
        })
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
struct Lookup(Arc<State>);
struct LookupFuture {
    mode: u8,
    state: Arc<State>,
}
impl Future for LookupFuture {
    type Output = Option<BoxedActorRef>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        if matches!(self.mode, 11 | 12) {
            fail(&self.state);
        }
        if self.mode == 14 {
            return Poll::Ready(None);
        }
        Poll::Ready(Some(Box::new(Target {
            mode: if self.mode == 13 { 7 } else { self.mode },
            state: self.state.clone(),
        })))
    }
}
impl Drop for LookupFuture {
    fn drop(&mut self) {
        if matches!(self.mode, 12 | 13) {
            fail(&self.state);
        }
    }
}
impl LocalLookup for Lookup {
    fn forwarding_target(&self, path: &str) -> Option<String> {
        if path == "/15" {
            fail(&self.0);
        }
        None
    }

    fn lookup<'life0, 'life1, 'async_trait>(
        &'life0 self,
        path: &'life1 str,
    ) -> BoxedFuture<'async_trait, Option<BoxedActorRef>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(LookupFuture {
            mode: path.trim_start_matches('/').parse().unwrap(),
            state: self.0.clone(),
        })
    }
    fn durable_receiver(
        &self,
        path: &str,
    ) -> Option<Arc<dyn parrot_remote::durable::DurableReceiver>> {
        Some(Arc::new(Endpoint {
            mode: path.trim_start_matches('/').parse().unwrap(),
            state: self.0.clone(),
        }))
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ask,
    Completion,
    Tell,
    OrderedTell,
    Cancel,
    Disconnect,
    Stop,
    Durable,
    DurableCancel,
    TcpAsk,
    TcpCompletion,
}
fn frame(mode: u8, kind: Kind, cid: u64) -> Frame {
    let path = format!("/{mode}");
    let payload = parrot_api::message::serde_remote_serialize(&mode)
        .unwrap()
        .into();
    match kind {
        Kind::Stop => Frame::stop(&path),
        Kind::Completion => Frame::tell_with_ack(cid, &path, Work::TYPE_KEY, payload),
        Kind::Tell => Frame::tell(&path, Work::TYPE_KEY, payload),
        Kind::OrderedTell => Frame::tell_with_seq(&path, Work::TYPE_KEY, payload, 1),
        _ => Frame::ask(cid, &path, Work::TYPE_KEY, payload, None),
    }
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("ingress contract deadline")
}
async fn count(value: &AtomicUsize, expected: usize) {
    bounded(async {
        while value.load(Ordering::SeqCst) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await;
}
async fn exercise(mode: u8, kind: Kind) {
    if matches!(kind, Kind::Durable | Kind::DurableCancel) {
        return durable(mode, kind).await;
    }
    if matches!(kind, Kind::TcpAsk | Kind::TcpCompletion) {
        return tcp(mode, kind).await;
    }
    let state = Arc::new(State::default());
    let ingress = Arc::new(
        Ingress::with_ask_limits(
            Arc::new(Lookup(state.clone())),
            Arc::new(CallbackRegistry::default()),
            1,
            1024,
        )
        .unwrap()
        .with_control_policy(parrot_remote::ControlPolicy::InsecureDevelopment),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let back = FrameSender::anon(tx);
    bounded(ingress.dispatch(frame(mode, kind, 71), &back, "peer")).await;
    if matches!(kind, Kind::Cancel | Kind::Disconnect) {
        count(&state.polls, 1).await;
        if kind == Kind::Cancel {
            ingress.close();
        } else {
            back.close();
        }
        count(&state.references, 1).await;
        assert_eq!(state.futures.load(Ordering::SeqCst), 1);
        assert!(
            rx.try_recv().is_err(),
            "cancelled work cannot acknowledge success"
        );
    } else if matches!(kind, Kind::Tell | Kind::OrderedTell | Kind::Stop) {
        if mode == 15 {
            assert_eq!(state.references.load(Ordering::SeqCst), 0);
        } else {
            count(&state.references, 1).await;
        }
        assert!(rx.try_recv().is_err());
    } else {
        let response = bounded(rx.recv()).await.unwrap();
        assert_eq!(response.header.correlation_id, 71);
        assert_eq!(
            response.header.frame_type,
            if mode == 0 {
                frame_type::REPLY
            } else {
                frame_type::REPLY_ERR
            },
            "success cannot precede failing owned cleanup"
        );
        if mode != 0 {
            let (code, detail) =
                parrot_remote::error::decode_err_payload(&response.payload).unwrap();
            assert_eq!(
                code,
                if mode == 18 {
                    parrot_remote::ErrCode::Stopped
                } else {
                    parrot_remote::ErrCode::ProtocolViolation
                }
            );
            if mode == 18 {
                assert_eq!(detail, "actor stopped");
            }
        }
        if !matches!(mode, 11 | 12 | 14) {
            assert_eq!(
                state.references.load(Ordering::SeqCst),
                1,
                "reference cleanup must precede receipt"
            );
        }
    }
    if matches!(mode, 11 | 12 | 14) {
        assert_eq!(DECODED_DROPS.load(Ordering::SeqCst), 1);
    }
    if mode == 13 {
        assert_eq!(
            state.polls.load(Ordering::SeqCst),
            0,
            "do not invoke actor after lookup cleanup failed"
        );
    }
    if matches!(mode, 4 | 16 | 19) {
        assert_eq!(state.replies.load(Ordering::SeqCst), 1);
    }
    assert_eq!(
        state.futures.load(Ordering::SeqCst),
        usize::from(!matches!(mode, 6 | 11..=15)),
        "each constructed user future retires exactly once"
    );
    assert_eq!(
        state.secondaries.load(Ordering::SeqCst),
        0,
        "throwing panic payload retirement must be bounded"
    );
    let payloads = match mode {
        0 | 14 => 0,
        1 | 7 | 11 | 15..=19 => 1,
        2 | 3 | 6 | 12 | 13 => 2,
        4 if kind == Kind::Completion => 2,
        4 | 5 | 8 => 3,
        _ => unreachable!("unexpected scenario"),
    };
    assert_eq!(
        state.payloads.load(Ordering::SeqCst),
        payloads,
        "each original user panic payload must be retired once"
    );
    if matches!(kind, Kind::Tell | Kind::OrderedTell) {
        assert_eq!(
            parrot_remote::ingress::DEAD_TELL_DROPPED.load(Ordering::SeqCst),
            1,
            "exactly one dead letter for the failed delivery"
        );
    }

    if kind != Kind::Cancel {
        let (next_tx, mut next_rx) = tokio::sync::mpsc::channel(8);
        let next_back = if kind == Kind::Disconnect {
            FrameSender::anon(next_tx)
        } else {
            back.clone()
        };
        bounded(ingress.dispatch(frame(0, Kind::Ask, 999), &next_back, "next-peer")).await;
        let reply = if kind == Kind::Disconnect {
            bounded(next_rx.recv()).await.unwrap()
        } else {
            bounded(rx.recv()).await.unwrap()
        };
        assert_eq!(
            reply.header.correlation_id, 999,
            "no duplicate failure or stale success receipt"
        );
        assert_eq!(
            reply.header.frame_type,
            frame_type::REPLY,
            "failed task must release the one-slot admission budget"
        );
    }
    ingress.close();
    bounded(ingress.wait_closed()).await;
}
fn run(name: &str, mode: u8, kind: Kind) {
    const CHILD: &str = "PARROT_INGRESS_UNWIND_CASE";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(exercise(mode, kind));
        return;
    }
    use std::{
        io::{Read, Seek},
        process::{Command, Stdio},
        time::Instant,
    };
    let root = std::env::var_os("PARROT_INGRESS_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!(
        "ingress-{}-{}-{name}.log",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut log = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .stdout(log.try_clone().unwrap())
        .stderr(Stdio::from(log.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "bounded ingress child failed: {result:?}; log={}",
                    path.display()
                );
            }
        }
    };
    log.rewind().unwrap();
    let mut text = String::new();
    log.read_to_string(&mut text).unwrap();
    assert!(
        status.success(),
        "{name}: {status}; log={}\n{text}",
        path.display()
    );
    assert!(
        text.contains("test result: ok. 1 passed; 0 failed;"),
        "{text}"
    );
}
macro_rules! contract {
    ($name:ident,$mode:expr,$kind:expr) => {
        #[test]
        fn $name() {
            run(stringify!($name), $mode, $kind);
        }
    };
}
contract!(healthy_ask_keeps_single_receipt_and_capacity, 0, Kind::Ask);
contract!(
    healthy_completion_keeps_single_receipt_and_capacity,
    0,
    Kind::Completion
);
contract!(ask_throwing_panic_payload_is_bounded, 1, Kind::Ask);
contract!(ask_future_and_reference_drop_are_independent, 2, Kind::Ask);
contract!(
    completion_future_and_reference_drop_are_independent,
    2,
    Kind::Completion
);
contract!(shutdown_cancellation_retires_both_owners, 3, Kind::Cancel);
contract!(
    disconnect_cancellation_releases_capacity,
    3,
    Kind::Disconnect
);
contract!(
    completion_reply_cleanup_precedes_success,
    4,
    Kind::Completion
);
contract!(
    error_display_and_drop_do_not_unwind_each_other,
    5,
    Kind::Ask
);
contract!(
    constructor_panic_does_not_unwind_reference_drop,
    6,
    Kind::Ask
);
contract!(
    completion_reference_cleanup_precedes_success,
    7,
    Kind::Completion
);
contract!(
    ordinary_tell_failure_does_not_escape_dispatch,
    8,
    Kind::Tell
);
contract!(
    ordered_tell_failure_does_not_kill_reorder_owner,
    8,
    Kind::OrderedTell
);
contract!(
    lookup_failure_keeps_decoded_input_cleanup_independent,
    11,
    Kind::Ask
);

struct Endpoint {
    mode: u8,
    state: Arc<State>,
}
struct EndpointFutureGuard(u8, Arc<State>);
impl Drop for EndpointFutureGuard {
    fn drop(&mut self) {
        self.1.futures.fetch_add(1, Ordering::SeqCst);
        if self.0 == 3 {
            fail(&self.1);
        }
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.state.references.fetch_add(1, Ordering::SeqCst);
        fail(&self.state);
    }
}
#[async_trait::async_trait]
impl parrot_remote::durable::DurableReceiver for Endpoint {
    async fn receive(
        &self,
        _: parrot_remote::durable::DurableEnvelope,
    ) -> Result<(), parrot_remote::durable::InboxError> {
        let _guard = EndpointFutureGuard(self.mode, self.state.clone());
        self.state.polls.fetch_add(1, Ordering::SeqCst);
        if self.mode == 1 {
            fail(&self.state);
        }
        if self.mode == 3 {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}
struct LocalRouter;
impl parrot_remote::ingress::RelayRouter for LocalRouter {
    fn sender_of(&self, _: &str) -> Option<FrameSender> {
        None
    }
    fn next_cid(&self) -> u64 {
        1
    }
    fn self_node(&self) -> &str {
        "receiver"
    }
    fn dial_addr_of(&self, _: &str) -> Option<String> {
        None
    }
    fn on_hint(&self, _: &str, _: &str) {}
}
async fn durable(mode: u8, kind: Kind) {
    let state = Arc::new(State::default());
    let ingress = Arc::new(
        Ingress::new(
            Arc::new(Lookup(state.clone())),
            Arc::new(CallbackRegistry::default()),
        )
        .with_control_policy(parrot_remote::ControlPolicy::InsecureDevelopment),
    );
    *ingress.relay_slot() = Some(Arc::new(LocalRouter));
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let back = FrameSender::new(tx, "source");
    let envelope = parrot_remote::durable::DurableEnvelope {
        source: "source".into(),
        session: "session".into(),
        receiver: "receiver".into(),
        path: format!("/{mode}"),
        sequence: 1,
        type_key: Work::TYPE_KEY.into(),
        payload: bytes::Bytes::from_static(&[0]),
    };
    let frame = Frame::tell_with_ack(
        71,
        &envelope.target_path(),
        parrot_remote::durable::DURABLE_KEY,
        envelope.encode().unwrap(),
    );
    bounded(ingress.dispatch(frame, &back, "source")).await;
    if kind == Kind::DurableCancel {
        count(&state.polls, 1).await;
        ingress.close();
        bounded(ingress.wait_closed()).await;
        assert!(
            rx.try_recv().is_err(),
            "cancelled durable call cannot produce success"
        );
    } else {
        let response = bounded(rx.recv()).await.unwrap();
        assert_eq!(response.header.correlation_id, 71);
        assert_eq!(response.header.frame_type, frame_type::REPLY_ERR);
    }
    assert_eq!(state.references.load(Ordering::SeqCst), 1);
    assert_eq!(state.futures.load(Ordering::SeqCst), 1);
    assert_eq!(state.secondaries.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.payloads.load(Ordering::SeqCst),
        if mode == 7 { 1 } else { 2 },
        "each original durable panic payload must be retired once"
    );
    ingress.close();
    bounded(ingress.wait_closed()).await;
}
async fn tcp(mode: u8, kind: Kind) {
    use parrot_remote::{NodeAddr, RemoteActorSystem, RemoteConfig};
    let state = Arc::new(State::default());
    let target = RemoteActorSystem::new(
        RemoteConfig::tcp("receiver", Some("127.0.0.1:0".parse().unwrap())),
        Arc::new(Lookup(state.clone())),
    )
    .unwrap();
    let source = RemoteActorSystem::new(
        RemoteConfig::tcp("source", None),
        Arc::new(Lookup(Arc::default())),
    )
    .unwrap();
    bounded(target.start()).await.unwrap();
    bounded(source.start()).await.unwrap();
    bounded(source.connect(&NodeAddr::tcp("receiver", target.local_addr().unwrap())))
        .await
        .unwrap();
    let original = source
        .links_snapshot()
        .await
        .into_iter()
        .find(|(node, _, _)| node == "receiver")
        .unwrap()
        .1;
    let actor = source
        .remote_ref(&format!("parrot://receiver/{mode}"))
        .unwrap();
    let outcome = if kind == Kind::TcpAsk {
        bounded(actor.send_with_timeout(Box::new(Work(mode)), Some(Duration::from_secs(2))))
            .await
            .map(drop)
    } else {
        bounded(actor.deliver_with_ack(Box::new(Work(mode)), Duration::from_secs(2))).await
    };
    assert!(
        matches!(outcome, Err(ActorError::InternalError(_))),
        "{outcome:?}"
    );
    assert_eq!(source.callbacks.len(), 0);
    let healthy = source.remote_ref("parrot://receiver/0").unwrap();
    let reply = bounded(healthy.send_with_timeout(Box::new(Work(0)), Some(Duration::from_secs(2))))
        .await
        .unwrap();
    assert_eq!(reply.downcast_ref::<Work>().unwrap().0, 0);
    assert!(
        source
            .links_snapshot()
            .await
            .into_iter()
            .find(|(node, _, _)| node == "receiver")
            .unwrap()
            .1
            .same_connection(&original),
        "must not hide a killed ingress behind reconnect"
    );
    bounded(source.shutdown()).await.unwrap();
    bounded(target.shutdown()).await.unwrap();
}
contract!(
    lookup_poll_drop_and_decoded_input_are_independent,
    12,
    Kind::Ask
);
contract!(
    lookup_cleanup_failure_prevents_actor_invocation,
    13,
    Kind::Ask
);
contract!(
    authorized_stop_keeps_user_cleanup_independent,
    8,
    Kind::Stop
);
contract!(
    durable_poll_and_endpoint_drop_are_independent,
    1,
    Kind::Durable
);
contract!(durable_endpoint_cleanup_precedes_success, 7, Kind::Durable);
contract!(
    durable_cancel_retires_future_and_endpoint,
    3,
    Kind::DurableCancel
);
contract!(
    tcp_ask_cleanup_failure_keeps_connection_usable,
    2,
    Kind::TcpAsk
);
contract!(
    tcp_completion_cleanup_failure_cannot_report_success,
    4,
    Kind::TcpCompletion
);

contract!(
    missing_lookup_retires_decoded_input_before_error_receipt,
    14,
    Kind::Ask
);
contract!(
    reply_encoder_panic_does_not_unwind_reply_or_reference,
    4,
    Kind::Ask
);

contract!(tell_forwarding_callback_panic_is_contained, 15, Kind::Tell);

contract!(
    completion_reply_only_drop_failure_precedes_success,
    16,
    Kind::Completion
);
contract!(ask_future_only_drop_failure_precedes_success, 17, Kind::Ask);
contract!(
    completion_future_only_drop_failure_precedes_success,
    17,
    Kind::Completion
);
contract!(
    business_stopped_error_survives_reference_cleanup_failure,
    18,
    Kind::Ask
);
contract!(
    ask_encoded_reply_only_drop_failure_precedes_success,
    19,
    Kind::Ask
);
