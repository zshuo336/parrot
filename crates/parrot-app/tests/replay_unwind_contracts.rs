//! User callbacks and each replay-owned value must have their own unwind boundary.
use futures::FutureExt;
use parrot_api::{
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::debug::replay::{record_sink, JsonlRecord, ReplayError, ReplayLog, ReplayOptions};
use parrot_remote::{
    capture::CapturedWire,
    frame::{flags, frame_type},
    LocalLookup,
};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    Healthy,
    ErrorDisplayDrop,
    ErrorDrop,
    LateConstructor,
    LateReady,
    AskConstructPanic,
    TellConstructPanic,
    CompletionConstructPanic,
    ErrorFutureDrop,
    TellTimeoutDrop,
    CompletionTimeoutDrop,
    DecodePanic,
    LookupPanic,
    AskPanic,
    TellPanic,
    CompletionPanic,
    EncodePanic,
    PreflightMessageDrop,
    RemainingMessageDrop,
    RefDrop,
    ReplyDrop,
    CompletionReplyDrop,
    AskFutureDrop,
    TellFutureDrop,
    CompletionFutureDrop,
    LookupTimeoutDrop,
    SendTimeoutDrop,
    CancelLookup,
    CancelPrepared,
    CancelSend,
}
#[derive(Debug)]
struct State {
    case: Case,
    lookups: AtomicUsize,
    sends: AtomicUsize,
    effects: AtomicUsize,
    encodes: AtomicUsize,
    payload_drops: AtomicUsize,
    nested_drops: AtomicUsize,
    messages: [AtomicUsize; 3],
    references: [AtomicUsize; 3],
    replies: [AtomicUsize; 3],
    future_drops: AtomicUsize,
    future_polls: AtomicUsize,
    error_drops: AtomicUsize,
    error_displays: AtomicUsize,
}
static STATE: OnceLock<Arc<State>> = OnceLock::new();
impl State {
    fn new(case: Case) -> Self {
        Self {
            case,
            lookups: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            effects: AtomicUsize::new(0),
            encodes: AtomicUsize::new(0),
            payload_drops: AtomicUsize::new(0),
            nested_drops: AtomicUsize::new(0),
            messages: std::array::from_fn(|_| AtomicUsize::new(0)),
            references: std::array::from_fn(|_| AtomicUsize::new(0)),
            replies: std::array::from_fn(|_| AtomicUsize::new(0)),
            future_drops: AtomicUsize::new(0),
            future_polls: AtomicUsize::new(0),
            error_drops: AtomicUsize::new(0),
            error_displays: AtomicUsize::new(0),
        }
    }
}
fn n(value: &AtomicUsize) -> usize {
    value.load(Ordering::SeqCst)
}
fn counts(values: &[AtomicUsize; 3]) -> [usize; 3] {
    values.each_ref().map(n)
}
struct PanicPayload(Arc<State>);
struct NestedPayload(Arc<State>);
impl Drop for PanicPayload {
    fn drop(&mut self) {
        self.0.payload_drops.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(NestedPayload(self.0.clone()));
    }
}
impl Drop for NestedPayload {
    fn drop(&mut self) {
        self.0.nested_drops.fetch_add(1, Ordering::SeqCst);
        panic!("second panic payload destructor");
    }
}
fn throwing(state: &Arc<State>) -> ! {
    std::panic::panic_any(PanicPayload(state.clone()))
}
#[derive(Debug)]
struct UserError(Arc<State>);
impl std::fmt::Display for UserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.error_displays.fetch_add(1, Ordering::SeqCst);
        if self.0.case == Case::ErrorDisplayDrop {
            throwing(&self.0);
        }
        f.write_str("deliberate foreign error")
    }
}
impl std::error::Error for UserError {}
impl Drop for UserError {
    fn drop(&mut self) {
        self.0.error_drops.fetch_add(1, Ordering::SeqCst);
        throwing(&self.0);
    }
}
struct Request {
    index: usize,
    state: Arc<State>,
}
impl Drop for Request {
    fn drop(&mut self) {
        self.state.messages[self.index].fetch_add(1, Ordering::SeqCst);
        if matches!(
            self.state.case,
            Case::PreflightMessageDrop | Case::CancelLookup | Case::CancelPrepared
        ) || (self.state.case == Case::RemainingMessageDrop && self.index == 2)
            || (self.state.case == Case::CancelSend && self.index > 0)
        {
            throwing(&self.state);
        }
    }
}
struct Reply {
    index: usize,
    state: Arc<State>,
}
impl Drop for Reply {
    fn drop(&mut self) {
        self.state.replies[self.index].fetch_add(1, Ordering::SeqCst);
        if self.index == 1 && matches!(self.state.case, Case::ReplyDrop | Case::CompletionReplyDrop)
        {
            throwing(&self.state);
        }
    }
}
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:replay-unwind.Request",
        type_id: std::any::TypeId::of::<Request>(),
        encode: |_| unreachable!("test requests come from the recorded bytes"),
        decode: |bytes| {
            let state = STATE.get().unwrap().clone();
            let index = usize::from(bytes[0]);
            if index == 1 && state.case == Case::DecodePanic { throwing(&state); }
            Ok(Box::new(Request { index, state }))
        },
    }
}
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:replay-unwind.Reply",
        type_id: std::any::TypeId::of::<Reply>(),
        encode: |message| {
            let reply = message.downcast_ref::<Reply>().unwrap();
            reply.state.encodes.fetch_add(1, Ordering::SeqCst);
            if reply.index == 1 && reply.state.case == Case::EncodePanic { throwing(&reply.state); }
            Ok(vec![reply.index as u8])
        },
        decode: |_| unreachable!("reply decode is not part of this contract"),
    }
}
// Move Tokio's paused clock while still inside a synchronous user callback.
// Poll advance once: it changes the clock before yielding to the scheduler.
// Assert that effect directly; do not block a nested executor on that yield.
fn advance_inside_callback() {
    let before = tokio::time::Instant::now();
    let mut advance = Box::pin(tokio::time::advance(Duration::from_millis(10)));
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    let _ = advance.as_mut().poll(&mut cx);
    assert_eq!(
        tokio::time::Instant::now() - before,
        Duration::from_millis(10)
    );
}
struct UserFuture<T> {
    output: Option<T>,
    state: Arc<State>,
    pending: bool,
    throw_on_drop: bool,
}
impl<T: Unpin> Future for UserFuture<T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<T> {
        self.state.future_polls.fetch_add(1, Ordering::SeqCst);
        if self.state.case == Case::LateReady && n(&self.state.sends) == 2 {
            advance_inside_callback();
        }
        if self.pending {
            Poll::Pending
        } else {
            Poll::Ready(self.output.take().unwrap())
        }
    }
}
impl<T> Drop for UserFuture<T> {
    fn drop(&mut self) {
        self.state.future_drops.fetch_add(1, Ordering::SeqCst);
        if self.throw_on_drop {
            throwing(&self.state);
        }
    }
}
#[derive(Debug)]
struct Reference {
    index: usize,
    state: Arc<State>,
}
impl Drop for Reference {
    fn drop(&mut self) {
        self.state.references[self.index].fetch_add(1, Ordering::SeqCst);
        if (self.index == 1 && self.state.case == Case::RefDrop)
            || matches!(self.state.case, Case::CancelPrepared | Case::CancelSend)
        {
            throwing(&self.state);
        }
    }
}
impl Reference {
    fn admit(&self, msg: BoxedMessage) -> bool {
        let request = msg.downcast::<Request>().unwrap();
        assert_eq!(request.index, self.index);
        self.state.sends.fetch_add(1, Ordering::SeqCst);
        drop(request);
        let pending = (self.index == 1
            && matches!(
                self.state.case,
                Case::SendTimeoutDrop | Case::TellTimeoutDrop | Case::CompletionTimeoutDrop
            ))
            || (self.index == 0 && self.state.case == Case::CancelSend);
        if !pending
            && !(self.index == 1
                && matches!(
                    self.state.case,
                    Case::RemainingMessageDrop
                        | Case::ErrorFutureDrop
                        | Case::ErrorDisplayDrop
                        | Case::ErrorDrop
                ))
        {
            self.state.effects.fetch_add(1, Ordering::SeqCst);
        }
        if self.index == 1 && self.state.case == Case::LateConstructor {
            advance_inside_callback();
        }
        pending
    }
}
impl ActorRef for Reference {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send_with_timeout(msg, None)
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let pending = self.admit(msg);
        if self.index == 1
            && matches!(
                self.state.case,
                Case::AskConstructPanic | Case::CompletionConstructPanic
            )
        {
            throwing(&self.state);
        }
        if self.index == 1 && matches!(self.state.case, Case::AskPanic | Case::CompletionPanic) {
            let state = self.state.clone();
            return Box::pin(async move { throwing(&state) });
        }
        let output = if self.index == 1
            && matches!(
                self.state.case,
                Case::RemainingMessageDrop
                    | Case::ErrorFutureDrop
                    | Case::ErrorDisplayDrop
                    | Case::ErrorDrop
            ) {
            if matches!(self.state.case, Case::ErrorDisplayDrop | Case::ErrorDrop) {
                Err(ActorError::Other(UserError(self.state.clone()).into()))
            } else {
                Err(ActorError::MessageHandlingError(
                    "deliberate business error".into(),
                ))
            }
        } else {
            Ok(Box::new(Reply {
                index: self.index,
                state: self.state.clone(),
            }) as BoxedMessage)
        };
        Box::pin(UserFuture {
            output: Some(output),
            state: self.state.clone(),
            pending,
            throw_on_drop: pending
                || (self.index == 1
                    && matches!(
                        self.state.case,
                        Case::AskFutureDrop | Case::CompletionFutureDrop | Case::ErrorFutureDrop
                    )),
        })
    }
    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        let pending = self.admit(msg);
        if self.index == 1 && self.state.case == Case::TellConstructPanic {
            throwing(&self.state);
        }
        if self.index == 1 && self.state.case == Case::TellPanic {
            let state = self.state.clone();
            return Box::pin(async move { throwing(&state) });
        }
        Box::pin(UserFuture {
            output: Some(Ok(())),
            state: self.state.clone(),
            pending,
            throw_on_drop: pending || (self.index == 1 && self.state.case == Case::TellFutureDrop),
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        unreachable!("replay must not stop business actors")
    }
    fn path(&self) -> String {
        format!("/{}", self.index)
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        unreachable!("replay uses supplied lookup")
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        panic!("replay must retain original reference identity")
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Lookup(Arc<State>);
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        let index: usize = path.strip_prefix('/').unwrap().parse().unwrap();
        self.0.lookups.fetch_add(1, Ordering::SeqCst);
        if index == 1 && self.0.case == Case::LookupPanic {
            throwing(&self.0);
        }
        if index == 1 && self.0.case == Case::PreflightMessageDrop {
            return None;
        }
        let pending = (index == 1 && self.0.case == Case::LookupTimeoutDrop)
            || (index == 0 && self.0.case == Case::CancelLookup)
            || (index == 2 && self.0.case == Case::CancelPrepared);
        if pending {
            UserFuture::<Option<BoxedActorRef>> {
                output: None,
                state: self.0.clone(),
                pending: true,
                throw_on_drop: true,
            }
            .await
        } else {
            Some(Box::new(Reference {
                index,
                state: self.0.clone(),
            }))
        }
    }
}
fn records(case: Case) -> Vec<JsonlRecord> {
    (0..3)
        .map(|index| {
            let tell = matches!(
                case,
                Case::TellPanic
                    | Case::TellConstructPanic
                    | Case::TellTimeoutDrop
                    | Case::TellFutureDrop
                    | Case::RefDrop
            );
            let completion = matches!(
                case,
                Case::CompletionPanic
                    | Case::CompletionConstructPanic
                    | Case::CompletionTimeoutDrop
                    | Case::CompletionFutureDrop
                    | Case::CompletionReplyDrop
            );
            let mut record = record_sink(
                index,
                100 + index,
                "rx",
                if tell || completion {
                    frame_type::TELL
                } else {
                    frame_type::ASK
                },
                &format!("/{index}"),
                "bin:replay-unwind.Request",
                &[index as u8],
            );
            if completion {
                record.wire = Some(CapturedWire {
                    connection: 1,
                    local_node: "local".into(),
                    peer_node: Some("peer".into()),
                    scheme: "mem".into(),
                    elapsed_micros: index,
                    version: 1,
                    flags: flags::TELL_ACK,
                    hop_count: 0,
                    hop_limit: 8,
                    wire_seq: 0,
                });
            }
            record
        })
        .collect()
}
async fn exercise(case: Case, state: Arc<State>) {
    let late = matches!(case, Case::LateConstructor | Case::LateReady);
    if late {
        tokio::time::pause();
    }
    let log = ReplayLog {
        records: records(case),
    };
    let lookup = Lookup(state.clone());
    let mut future = Box::pin(log.replay_local(
        &lookup,
        ReplayOptions {
            call_timeout: if late {
                Duration::from_millis(5)
            } else if matches!(
                case,
                Case::LookupTimeoutDrop
                    | Case::SendTimeoutDrop
                    | Case::TellTimeoutDrop
                    | Case::CompletionTimeoutDrop
            ) {
                Duration::from_millis(20)
            } else {
                Duration::from_secs(5)
            },
            ..Default::default()
        },
    ));
    if matches!(
        case,
        Case::CancelLookup | Case::CancelPrepared | Case::CancelSend
    ) {
        assert!(futures::poll!(&mut future).is_pending());
        assert!(
            std::panic::catch_unwind(AssertUnwindSafe(|| drop(future))).is_ok(),
            "cancellation leaked a destructor panic"
        );
        let (messages, references, sends) = match case {
            Case::CancelLookup => ([1, 0, 0], [0, 0, 0], 0),
            Case::CancelPrepared => ([1, 1, 1], [1, 1, 0], 0),
            _ => ([1, 1, 1], [1, 1, 1], 1),
        };
        assert_eq!(counts(&state.messages), messages);
        assert_eq!(counts(&state.references), references);
        assert_eq!(n(&state.sends), sends);
        assert_eq!(n(&state.effects), 0);
        assert!(n(&state.payload_drops) > 0);
        // Each owned-value destructor gets a bounded boundary. If disposal of
        // its panic payload also throws, the secondary payload is retained
        // without recursively invoking arbitrary user destructors.
        assert_eq!(n(&state.nested_drops), 0);
        assert_eq!(n(&state.future_drops), 1);
        assert_eq!(
            counts(&state.replies),
            if case == Case::CancelSend {
                [1, 0, 0]
            } else {
                [0, 0, 0]
            }
        );
        assert_eq!(n(&state.encodes), 0);
        return;
    }
    let outer = AssertUnwindSafe(future).catch_unwind().await;
    assert!(
        outer.is_ok(),
        "replay leaked an unwind instead of a typed result"
    );
    let result = outer.unwrap();
    if case == Case::Healthy {
        let report = result.unwrap();
        assert_eq!(report.delivered, 3);
        assert_eq!(
            report
                .replies
                .iter()
                .map(|r| r.payload.clone())
                .collect::<Vec<_>>(),
            [vec![0], vec![1], vec![2]]
        );
        assert_eq!(counts(&state.messages), [1, 1, 1]);
        assert_eq!(counts(&state.references), [1, 1, 1]);
        assert_eq!(counts(&state.replies), [1, 1, 1]);
        assert_eq!(n(&state.effects), 3);
        assert_eq!(n(&state.future_drops), 3);
        assert_eq!(n(&state.encodes), 3);
        return;
    }
    let (expected_completed, expected_sends, expected_effects, reason) = match case {
        Case::DecodePanic => (0, 0, 0, "codec panicked"),
        Case::LookupPanic => (0, 0, 0, "actor lookup panicked"),
        Case::PreflightMessageDrop => (0, 0, 0, "actor not found"),
        Case::LookupTimeoutDrop => (0, 0, 0, "actor lookup timed out"),
        Case::SendTimeoutDrop => (1, 2, 1, "ASK timed out"),
        Case::LateConstructor | Case::LateReady => (1, 2, 2, "ASK timed out"),
        Case::TellTimeoutDrop => (1, 2, 1, "TELL admission timed out"),
        Case::CompletionTimeoutDrop => (1, 2, 1, "completion TELL timed out"),
        Case::AskPanic | Case::AskConstructPanic => (1, 2, 2, "ASK panicked"),
        Case::TellPanic | Case::TellConstructPanic => (1, 2, 2, "TELL admission panicked"),
        Case::CompletionPanic | Case::CompletionConstructPanic => {
            (1, 2, 2, "completion TELL panicked")
        }
        Case::EncodePanic => (2, 2, 2, "reply codec panicked"),
        Case::RemainingMessageDrop | Case::ErrorFutureDrop => {
            (1, 2, 1, "deliberate business error")
        }
        Case::ErrorDisplayDrop => (1, 2, 1, "error formatting panicked"),
        Case::ErrorDrop => (1, 2, 1, "deliberate foreign error"),
        _ => (2, 2, 2, "disposal panicked"),
    };
    match result {
        Err(ReplayError::Delivery {
            seq,
            completed,
            reason: actual,
        }) => {
            assert_eq!(seq, 1);
            assert_eq!(
                completed, expected_completed,
                "must preserve known business progress: {actual}"
            );
            assert!(actual.contains(reason), "{actual}");
        }
        other => panic!("unexpected replay result {other:?}"),
    }
    assert_eq!(
        n(&state.sends),
        expected_sends,
        "never deliver the third request after failure"
    );
    assert_eq!(
        n(&state.effects),
        expected_effects,
        "a failure must not claim rollback"
    );
    let (messages, refs) = match case {
        Case::DecodePanic => ([1, 0, 0], [1, 0, 0]),
        Case::LookupPanic | Case::PreflightMessageDrop | Case::LookupTimeoutDrop => {
            ([1, 1, 0], [1, 0, 0])
        }
        _ => ([1, 1, 1], [1, 1, 1]),
    };
    assert_eq!(counts(&state.messages), messages);
    assert_eq!(counts(&state.references), refs);
    let expected_future_drops = match case {
        Case::DecodePanic | Case::LookupPanic | Case::PreflightMessageDrop => 0,
        Case::LookupTimeoutDrop
        | Case::AskPanic
        | Case::TellPanic
        | Case::CompletionPanic
        | Case::AskConstructPanic
        | Case::TellConstructPanic
        | Case::CompletionConstructPanic => 1,
        _ => 2,
    };
    assert_eq!(n(&state.future_drops), expected_future_drops);
    let expected_replies = match case {
        Case::DecodePanic
        | Case::LookupPanic
        | Case::PreflightMessageDrop
        | Case::LookupTimeoutDrop
        | Case::TellPanic
        | Case::TellConstructPanic
        | Case::TellFutureDrop
        | Case::TellTimeoutDrop
        | Case::RefDrop => [0, 0, 0],
        Case::AskPanic
        | Case::CompletionPanic
        | Case::AskConstructPanic
        | Case::CompletionConstructPanic
        | Case::RemainingMessageDrop
        | Case::ErrorFutureDrop
        | Case::ErrorDisplayDrop
        | Case::ErrorDrop => [1, 0, 0],
        _ => [1, 1, 0],
    };
    assert_eq!(counts(&state.replies), expected_replies);
    let expected_encodes = match case {
        Case::LateConstructor
        | Case::LateReady
        | Case::AskPanic
        | Case::AskConstructPanic
        | Case::AskFutureDrop
        | Case::SendTimeoutDrop
        | Case::RemainingMessageDrop
        | Case::ErrorFutureDrop
        | Case::ErrorDisplayDrop
        | Case::ErrorDrop => 1,
        Case::EncodePanic | Case::ReplyDrop => 2,
        _ => 0,
    };
    assert_eq!(n(&state.encodes), expected_encodes);
    if late {
        assert_eq!(
            n(&state.future_polls),
            if case == Case::LateConstructor { 1 } else { 2 }
        );
        assert_eq!(n(&state.payload_drops), 0);
        return;
    }

    assert!(n(&state.payload_drops) > 0);
    if matches!(case, Case::ErrorDisplayDrop | Case::ErrorDrop) {
        assert_eq!(n(&state.error_displays), 1);
        assert_eq!(n(&state.error_drops), 1);
        assert_eq!(
            n(&state.payload_drops),
            if case == Case::ErrorDisplayDrop { 2 } else { 1 }
        );
        assert_eq!(
            n(&state.nested_drops),
            if case == Case::ErrorDisplayDrop { 1 } else { 0 }
        );
        return;
    }
    if matches!(
        case,
        Case::DecodePanic
            | Case::LookupPanic
            | Case::AskPanic
            | Case::TellPanic
            | Case::CompletionPanic
            | Case::EncodePanic
            | Case::AskConstructPanic
            | Case::TellConstructPanic
            | Case::CompletionConstructPanic
    ) {
        assert_eq!(n(&state.payload_drops), 1);
        assert_eq!(n(&state.nested_drops), 1);
    } else {
        assert_eq!(n(&state.nested_drops), 0);
    }
}
fn run_case(name: &str, case: Case) {
    use std::{
        io::{Read, Seek},
        process::{Command, Stdio},
        time::Instant,
    };
    const CHILD: &str = "PARROT_REPLAY_UNWIND_CASE";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        let state = Arc::new(State::new(case));
        STATE.set(state.clone()).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(exercise(case, state));
        return;
    }
    let mut log = tempfile::tempfile().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .stdout(log.try_clone().unwrap())
        .stderr(Stdio::from(log.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("probe did not exit: {result:?}");
            }
        }
    };
    log.rewind().unwrap();
    let mut text = String::new();
    log.read_to_string(&mut text).unwrap();
    assert!(status.success(), "{name}: {status}\n{text}");
    assert!(
        text.contains("test result: ok. 1 passed; 0 failed;"),
        "child did not execute the contract: {text}"
    );
}
macro_rules! contract {
    ($name:ident, $case:ident) => {
        #[test]
        fn $name() {
            run_case(stringify!($name), Case::$case);
        }
    };
}
contract!(
    healthy_replay_preserves_replies_and_releases_each_original_once,
    Healthy
);
contract!(
    decode_panic_payloads_remain_typed_preflight_errors,
    DecodePanic
);
contract!(lookup_panic_payloads_release_decoded_messages, LookupPanic);
contract!(
    ask_panic_payloads_stop_following_calls_without_claiming_rollback,
    AskPanic
);
contract!(
    tell_panic_payloads_preserve_admission_uncertainty,
    TellPanic
);
contract!(
    completion_panic_payloads_preserve_completion_uncertainty,
    CompletionPanic
);
contract!(
    encode_panic_payloads_preserve_successful_call_count,
    EncodePanic
);
contract!(
    failed_preflight_drops_throwing_messages_individually,
    PreflightMessageDrop
);
contract!(
    business_error_releases_remaining_throwing_messages,
    RemainingMessageDrop
);
contract!(
    reference_disposal_failure_preserves_progress_and_stops_replay,
    RefDrop
);
contract!(
    reply_disposal_failure_preserves_progress_and_stops_replay,
    ReplyDrop
);
contract!(
    completion_reply_disposal_failure_is_reported_after_completion,
    CompletionReplyDrop
);
contract!(
    ask_future_disposal_does_not_erase_a_successful_reply,
    AskFutureDrop
);
contract!(
    tell_future_disposal_does_not_erase_successful_admission,
    TellFutureDrop
);
contract!(
    completion_future_disposal_does_not_erase_successful_completion,
    CompletionFutureDrop
);
contract!(lookup_timeout_catches_future_disposal, LookupTimeoutDrop);
contract!(send_timeout_catches_future_disposal, SendTimeoutDrop);
contract!(
    cancellation_during_lookup_releases_current_message,
    CancelLookup
);
contract!(
    cancellation_during_preflight_releases_each_prepared_identity,
    CancelPrepared
);
contract!(
    cancellation_during_send_releases_future_and_remaining_inputs,
    CancelSend
);

contract!(
    ask_constructor_panic_payloads_are_contained,
    AskConstructPanic
);
contract!(
    tell_constructor_panic_payloads_are_contained,
    TellConstructPanic
);
contract!(
    completion_constructor_panic_payloads_are_contained,
    CompletionConstructPanic
);
contract!(
    business_error_is_not_replaced_by_future_disposal_failure,
    ErrorFutureDrop
);
contract!(tell_timeout_catches_future_disposal, TellTimeoutDrop);
contract!(
    completion_timeout_catches_future_disposal,
    CompletionTimeoutDrop
);

contract!(
    callback_construction_is_inside_the_call_deadline,
    LateConstructor
);
contract!(ready_reply_after_deadline_remains_a_timeout, LateReady);

contract!(
    foreign_error_formatting_and_disposal_panics_are_contained,
    ErrorDisplayDrop
);
contract!(
    foreign_error_disposal_preserves_the_primary_error_text,
    ErrorDrop
);
