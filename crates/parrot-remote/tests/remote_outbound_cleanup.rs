use futures::FutureExt;
use parrot_api::{
    address::ActorRef,
    message::MessageOptions,
    trace::{self, TraceContext},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{LocalLookup, NodeAddr, RemoteActorRef, RemoteActorSystem, RemoteConfig};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    any::Any,
    collections::HashMap,
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::Duration,
};

type Identity = Option<([u8; 16], [u8; 8])>;
fn current() -> Identity {
    TraceContext::current().map(|c| (c.trace_id(), c.span_id()))
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Normal,
    Pending,
    Deadline,
    EncodeError,
    EncodePanic,
    DecodeError,
    DecodePanic,
    ReplyDrop,
    InvalidOptions,
    InvalidDeadline,
    Unavailable,
    Unpolled,
    ExternalUnwind,
}
#[derive(Debug)]
struct State {
    id: u64,
    mode: Mode,
    events: Mutex<Vec<(&'static str, Identity)>>,
    started: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}
fn registry() -> &'static Mutex<HashMap<u64, Weak<State>>> {
    static STORE: OnceLock<Mutex<HashMap<u64, Weak<State>>>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}
impl State {
    fn new(mode: Mode) -> Arc<Self> {
        static ID: AtomicU64 = AtomicU64::new(1);
        let state = Arc::new(Self {
            id: ID.fetch_add(1, Ordering::Relaxed),
            mode,
            events: Mutex::new(Vec::new()),
            started: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        });
        registry()
            .lock()
            .unwrap()
            .insert(state.id, Arc::downgrade(&state));
        state
    }
    fn event(&self, name: &'static str) {
        self.events.lock().unwrap().push((name, current()));
    }
}
impl Drop for State {
    fn drop(&mut self) {
        registry().lock().unwrap().remove(&self.id);
    }
}
fn state(id: u64) -> Arc<State> {
    registry()
        .lock()
        .unwrap()
        .get(&id)
        .unwrap()
        .upgrade()
        .unwrap()
}

#[derive(parrot_api_derive::RemoteMessage)]
struct Work {
    state: Arc<State>,
    outbound: bool,
}
impl parrot_api::Message for Work {
    type Result = ();
}
impl Serialize for Work {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.state.event("request.encode");
        match self.state.mode {
            Mode::EncodeError => return Err(serde::ser::Error::custom("injected encode error")),
            Mode::EncodePanic => panic!("injected encode panic"),
            _ => {}
        }
        self.state.id.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Work {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let state = state(u64::deserialize(deserializer)?);
        state.event("request.decode");
        Ok(Self {
            state,
            outbound: false,
        })
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        self.state.event(if self.outbound {
            "outbound.drop"
        } else {
            "inbound.drop"
        });
        if self.outbound {
            std::panic::panic_any(Payload(self.state.clone()));
        }
    }
}
#[derive(parrot_api_derive::RemoteMessage)]
struct Reply(u64, bool);
impl parrot_api::Message for Reply {
    type Result = ();
}
impl Serialize for Reply {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        state(self.0).event("reply.encode");
        self.0.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Reply {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = u64::deserialize(deserializer)?;
        let state = state(id);
        state.event("reply.decode");
        match state.mode {
            Mode::DecodeError => return Err(serde::de::Error::custom("injected decode error")),
            Mode::DecodePanic => panic!("injected decode panic"),
            _ => {}
        }
        Ok(Self(id, true))
    }
}
#[derive(Debug)]
struct Payload(Arc<State>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.event("payload.drop");
        std::panic::panic_any(Secondary(self.0.clone()));
    }
}
#[derive(Debug)]
struct Secondary(Arc<State>);
impl Drop for Secondary {
    fn drop(&mut self) {
        self.0.event("secondary.drop");
        panic!("recursive panic payload");
    }
}
impl Drop for Reply {
    fn drop(&mut self) {
        if self.1 {
            let state = state(self.0);
            state.event("reply.drop");
            if state.mode == Mode::ReplyDrop {
                std::panic::panic_any(Payload(state));
            }
        }
    }
}
#[derive(Clone, Debug)]
struct Target;
#[async_trait::async_trait]
impl ActorRef for Target {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let work = message.downcast::<Work>().unwrap();
            work.state.event("handler");
            work.state.started.add_permits(1);
            if matches!(work.state.mode, Mode::Pending | Mode::Deadline) {
                work.state.release.acquire().await.unwrap().forget();
            }
            Ok(Box::new(Reply(work.state.id, false)) as BoxedMessage)
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
        Box::pin(async move {
            self.send(message).await?;
            Ok(())
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[async_trait::async_trait]
impl LocalLookup for Target {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        (path == "/target").then(|| self.clone_boxed())
    }
}
struct Pair {
    source: Arc<RemoteActorSystem>,
    peer: Arc<RemoteActorSystem>,
    reference: RemoteActorRef,
}
impl Pair {
    async fn new(tcp: bool, id: u64) -> Self {
        let config = |suffix| {
            let id = format!("context-{}-{id}-{suffix}", std::process::id());
            if tcp {
                RemoteConfig::tcp(&id, Some("127.0.0.1:0".parse().unwrap()))
            } else {
                RemoteConfig::mem(&id)
            }
        };
        let source = RemoteActorSystem::new(config("source"), Arc::new(Target)).unwrap();
        let peer = RemoteActorSystem::new(config("peer"), Arc::new(Target)).unwrap();
        source.start().await.unwrap();
        peer.start().await.unwrap();
        if tcp {
            source
                .connect(&NodeAddr::tcp(
                    &peer.config.node_id,
                    peer.local_addr().unwrap(),
                ))
                .await
                .unwrap();
        } else {
            source.connect_mem_pair(&peer).await.unwrap();
        }
        let reference = source
            .remote_ref(&format!("parrot://{}/target", peer.config.node_id))
            .unwrap();
        Self {
            source,
            peer,
            reference,
        }
    }
    async fn close(self) {
        self.source.shutdown().await.unwrap();
        self.peer.shutdown().await.unwrap();
    }
}
#[derive(Clone, Copy, Debug)]
enum Entry {
    Send,
    Timeout,
    Options,
    Tell,
    TellOptions,
    Completion,
}
fn invoke<'a>(
    reference: &'a RemoteActorRef,
    state: &Arc<State>,
    entry: Entry,
) -> BoxedFuture<'a, ActorResult<Option<BoxedMessage>>> {
    let timeout = if state.mode == Mode::InvalidDeadline {
        Duration::ZERO
    } else if state.mode == Mode::Deadline {
        Duration::from_millis(20)
    } else {
        Duration::from_secs(5)
    };
    let options = MessageOptions {
        timeout: Some(timeout),
        retry_policy: (state.mode == Mode::InvalidOptions).then_some(
            parrot_api::message::RetryPolicy {
                max_attempts: 1,
                retry_interval: Duration::from_millis(1),
                backoff_strategy: parrot_api::message::BackoffStrategy::Fixed,
            },
        ),
        ..Default::default()
    };
    let message: BoxedMessage = Box::new(Work {
        state: state.clone(),
        outbound: true,
    });
    let request = match entry {
        Entry::Send => reference.send(message),
        Entry::Timeout => reference.send_with_timeout(message, Some(timeout)),
        Entry::Options => reference.send_with_options(message, options.clone()),
        Entry::Tell => {
            let request = reference.deliver(message);
            return Box::pin(async move { request.await.map(|()| None) });
        }
        Entry::TellOptions => {
            let request = reference.deliver_with_options(message, options);
            return Box::pin(async move { request.await.map(|()| None) });
        }
        Entry::Completion => {
            let request = reference.deliver_with_ack(message, timeout);
            return Box::pin(async move { request.await.map(|()| None) });
        }
    };
    Box::pin(async move { request.await.map(Some) })
}
async fn scenario(entry: Entry, tcp: bool, mode: Mode) {
    use parrot_api::errors::ActorError;
    for traced in [true, false] {
        let state = State::new(mode);
        let pair = Pair::new(tcp, state.id).await;
        if mode == Mode::Unavailable {
            pair.source.shutdown().await.unwrap();
        }
        let caller = traced.then(TraceContext::root);
        let expected = caller.as_ref().map(|c| (c.trace_id(), c.span_id()));
        let poller = TraceContext::root();
        let poller_identity = Some((poller.trace_id(), poller.span_id()));
        let mut future = trace::in_scope(caller, || invoke(&pair.reference, &state, entry));
        if mode == Mode::Unpolled {
            let result =
                poller.in_scope(|| std::panic::catch_unwind(AssertUnwindSafe(|| drop(future))));
            if let Err(payload) = result {
                std::mem::forget(payload);
                panic!("unpolled cancellation leaked a destructor panic");
            }
        } else if mode == Mode::ExternalUnwind {
            let result = poller.in_scope(|| {
                std::panic::catch_unwind(AssertUnwindSafe(|| {
                    let _owned = future;
                    panic!("unrelated caller panic");
                }))
            });
            assert_eq!(
                result.unwrap_err().downcast_ref::<&str>(),
                Some(&"unrelated caller panic")
            );
        } else if mode == Mode::Pending {
            poller
                .scope(async {
                    tokio::time::timeout(Duration::from_secs(5), async {
                        tokio::select! {
                            _ = &mut future => panic!("pending handler completed"),
                            permit = state.started.acquire() => permit.unwrap().forget(),
                        }
                    })
                    .await
                    .unwrap();
                })
                .await;
            let result =
                poller.in_scope(|| std::panic::catch_unwind(AssertUnwindSafe(|| drop(future))));
            state.release.add_permits(1);
            if let Err(payload) = result {
                std::mem::forget(payload);
                panic!("pending cancellation leaked a destructor panic");
            }
        } else {
            let result = poller
                .scope(async {
                    let result = AssertUnwindSafe(future).catch_unwind().await;
                    assert_eq!(current(), poller_identity);
                    result
                })
                .await;
            if matches!(mode, Mode::EncodePanic | Mode::DecodePanic) {
                let payload = result.expect_err("original codec panic must reach caller");
                let expected_panic = if mode == Mode::EncodePanic {
                    "injected encode panic"
                } else {
                    "injected decode panic"
                };
                assert_eq!(payload.downcast_ref::<&str>(), Some(&expected_panic));
            } else {
                let outcome = match result {
                    Ok(outcome) => outcome,
                    Err(payload) => {
                        std::mem::forget(payload);
                        panic!("owned input Drop escaped completion");
                    }
                };
                match mode {
                    Mode::Normal | Mode::ReplyDrop => assert!(
                        matches!(outcome, Err(ActorError::Panic(_))),
                        "input cleanup failure must suppress success"
                    ),
                    Mode::EncodeError | Mode::DecodeError => assert!(
                        matches!(outcome,Err(ActorError::MessageHandlingError(ref detail)) if detail.starts_with("codec: ")),
                        "original codec error must survive cleanup: {outcome:?}"
                    ),
                    Mode::InvalidOptions => assert!(
                        matches!(outcome,Err(ActorError::MessageHandlingError(ref detail)) if detail.starts_with("retry_policy requires"))
                    ),
                    Mode::InvalidDeadline => assert!(
                        matches!(outcome,Err(ActorError::InternalError(ref detail)) if detail=="invalid completion deadline")
                    ),
                    Mode::Unavailable => assert!(
                        matches!(outcome,Err(ActorError::InternalError(ref detail)) if detail.contains("unavailable")||detail=="remote connection lost")
                    ),
                    Mode::Deadline => {
                        assert!(matches!(
                            outcome,
                            Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
                        ));
                        state.release.add_permits(1);
                    }
                    _ => unreachable!(),
                }
            }
        }
        assert!(current().is_none());
        assert!(
            pair.source.callbacks.is_empty(),
            "callback must retire on every terminal path"
        );
        if matches!(mode, Mode::Normal | Mode::ReplyDrop) {
            tokio::time::timeout(Duration::from_secs(5), state.started.acquire())
                .await
                .unwrap()
                .unwrap()
                .forget();
        }
        pair.close().await;
        let events = state.events.lock().unwrap();
        let count = |kind| events.iter().filter(|(name, _)| *name == kind).count();
        assert_eq!(count("outbound.drop"), 1, "{events:?}");
        let asks = matches!(entry, Entry::Send | Entry::Timeout | Entry::Options);
        let reply = asks && matches!(mode, Mode::Normal | Mode::ReplyDrop);
        assert_eq!(count("reply.drop"), usize::from(reply), "{events:?}");
        assert_eq!(
            count("payload.drop"),
            1 + usize::from(reply && mode == Mode::ReplyDrop),
            "{events:?}"
        );
        assert_eq!(
            count("secondary.drop"),
            0,
            "recursive panic payload must be bounded"
        );
        assert_eq!(
            count("request.encode"),
            usize::from(!matches!(
                mode,
                Mode::Unpolled
                    | Mode::ExternalUnwind
                    | Mode::InvalidOptions
                    | Mode::InvalidDeadline
            )),
            "{events:?}"
        );
        for (kind, identity) in events.iter() {
            if matches!(
                *kind,
                "outbound.drop" | "payload.drop" | "reply.drop" | "request.encode" | "reply.decode"
            ) {
                assert_eq!(*identity, expected, "{kind}: {events:?}");
            }
        }
    }
}
fn run(name: &str, entry: Entry, tcp: bool, mode: Mode) {
    const CHILD: &str = "PARROT_OUTBOUND_CLEANUP_CASE";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(scenario(entry, tcp, mode));
        return;
    }
    use std::{
        io::{Read, Seek},
        process::{Command, Stdio},
        time::Instant,
    };
    let root = std::env::var_os("PARROT_OUTBOUND_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join(format!(
        "outbound-{}-{}-{name}.log",
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
            other => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "bounded outbound child failed: {other:?}; log={}",
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
macro_rules! case {
    ($name:ident,$entry:ident,$tcp:expr,$mode:ident) => {
        #[test]
        fn $name() {
            run(stringify!($name), Entry::$entry, $tcp, Mode::$mode);
        }
    };
}
case!(send_normal, Send, false, Normal);
case!(send_unpolled, Send, false, Unpolled);
case!(send_externalunwind, Send, false, ExternalUnwind);
case!(send_encodeerror, Send, false, EncodeError);
case!(send_encodepanic, Send, false, EncodePanic);
case!(timeout_normal, Timeout, false, Normal);
case!(timeout_unpolled, Timeout, false, Unpolled);
case!(timeout_externalunwind, Timeout, false, ExternalUnwind);
case!(timeout_encodeerror, Timeout, false, EncodeError);
case!(timeout_encodepanic, Timeout, false, EncodePanic);
case!(options_normal, Options, false, Normal);
case!(options_unpolled, Options, false, Unpolled);
case!(options_externalunwind, Options, false, ExternalUnwind);
case!(options_encodeerror, Options, false, EncodeError);
case!(options_encodepanic, Options, false, EncodePanic);
case!(tell_normal, Tell, false, Normal);
case!(tell_unpolled, Tell, false, Unpolled);
case!(tell_externalunwind, Tell, false, ExternalUnwind);
case!(tell_encodeerror, Tell, false, EncodeError);
case!(tell_encodepanic, Tell, false, EncodePanic);
case!(telloptions_normal, TellOptions, false, Normal);
case!(telloptions_unpolled, TellOptions, false, Unpolled);
case!(
    telloptions_externalunwind,
    TellOptions,
    false,
    ExternalUnwind
);
case!(telloptions_encodeerror, TellOptions, false, EncodeError);
case!(telloptions_encodepanic, TellOptions, false, EncodePanic);
case!(completion_normal, Completion, false, Normal);
case!(completion_unpolled, Completion, false, Unpolled);
case!(completion_externalunwind, Completion, false, ExternalUnwind);
case!(completion_encodeerror, Completion, false, EncodeError);
case!(completion_encodepanic, Completion, false, EncodePanic);
case!(send_decodeerror, Send, true, DecodeError);
case!(send_decodepanic, Send, true, DecodePanic);
case!(send_replydrop, Send, true, ReplyDrop);
case!(timeout_decodeerror, Timeout, true, DecodeError);
case!(timeout_decodepanic, Timeout, true, DecodePanic);
case!(timeout_replydrop, Timeout, true, ReplyDrop);
case!(options_decodeerror, Options, true, DecodeError);
case!(options_decodepanic, Options, true, DecodePanic);
case!(options_replydrop, Options, true, ReplyDrop);
case!(send_pending, Send, true, Pending);
case!(timeout_pending, Timeout, true, Pending);
case!(options_pending, Options, true, Pending);
case!(completion_pending, Completion, true, Pending);
case!(timeout_deadline, Timeout, true, Deadline);
case!(options_deadline, Options, true, Deadline);
case!(completion_deadline, Completion, true, Deadline);

case!(options_invalid, Options, false, InvalidOptions);
case!(telloptions_invalid, TellOptions, false, InvalidOptions);
case!(completion_invalid, Completion, false, InvalidDeadline);
case!(send_unavailable, Send, false, Unavailable);
case!(timeout_unavailable, Timeout, false, Unavailable);
case!(options_unavailable, Options, false, Unavailable);
case!(tell_unavailable, Tell, false, Unavailable);
case!(telloptions_unavailable, TellOptions, false, Unavailable);
case!(completion_unavailable, Completion, false, Unavailable);
