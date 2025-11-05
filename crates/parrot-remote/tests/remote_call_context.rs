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
    fn assert_client_context(&self, expected: Identity, decode: bool, encoded: bool) {
        let events = self.events.lock().unwrap();
        assert_eq!(
            events.iter().filter(|e| e.0 == "outbound.drop").count(),
            1,
            "{events:?}"
        );
        assert_eq!(
            events.iter().any(|e| e.0 == "request.encode"),
            encoded,
            "{events:?}"
        );
        assert_eq!(
            events.iter().any(|e| e.0 == "reply.decode"),
            decode,
            "{events:?}"
        );
        for (kind, context) in events.iter() {
            if matches!(*kind, "request.encode" | "reply.decode" | "outbound.drop") {
                assert_eq!(*context, expected, "{kind}: {events:?}");
            }
        }
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
    }
}
#[derive(parrot_api_derive::RemoteMessage)]
struct Reply(u64);
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
        Ok(Self(id))
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
            Ok(Box::new(Reply(work.state.id)) as BoxedMessage)
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
) -> BoxedFuture<'a, ActorResult<()>> {
    let id = state.id;
    let timeout = if state.mode == Mode::Deadline {
        Duration::from_millis(20)
    } else {
        Duration::from_secs(5)
    };
    let message: BoxedMessage = Box::new(Work {
        state: state.clone(),
        outbound: true,
    });
    let request = match entry {
        Entry::Send => reference.send(message),
        Entry::Timeout => reference.send_with_timeout(message, Some(timeout)),
        Entry::Options => reference.send_with_options(
            message,
            MessageOptions {
                timeout: Some(timeout),
                ..Default::default()
            },
        ),
        Entry::Tell => return reference.deliver(message),
        Entry::TellOptions => {
            return reference.deliver_with_options(message, MessageOptions::default())
        }
        Entry::Completion => return Box::pin(reference.deliver_with_ack(message, timeout)),
    };
    Box::pin(async move {
        let reply = request.await?;
        assert_eq!(reply.downcast::<Reply>().unwrap().0, id);
        Ok(())
    })
}
async fn scenario(entry: Entry, tcp: bool, mode: Mode, unpolled: bool) {
    for traced in [true, false] {
        let state = State::new(mode);
        let pair = Pair::new(tcp, state.id).await;
        let caller = traced.then(TraceContext::root);
        let expected = caller.as_ref().map(|c| (c.trace_id(), c.span_id()));
        let poller = TraceContext::root();
        let mut future = trace::in_scope(caller, || invoke(&pair.reference, &state, entry));
        assert!(current().is_none());
        if unpolled {
            poller.in_scope(|| {
                drop(future);
                assert_eq!(current(), Some((poller.trace_id(), poller.span_id())));
            });
        } else if mode == Mode::Pending {
            poller
                .scope(async {
                    tokio::time::timeout(Duration::from_secs(5), async {
                        tokio::select! {
                            result = &mut future => panic!("pending handler completed: {result:?}"),
                            permit = state.started.acquire() => permit.unwrap().forget(),
                        }
                    })
                    .await
                    .unwrap();
                })
                .await;
            poller.in_scope(|| {
                drop(future);
                assert_eq!(current(), Some((poller.trace_id(), poller.span_id())));
            });
            state.release.add_permits(1);
        } else {
            let result = poller
                .scope(async {
                    let result = AssertUnwindSafe(future).catch_unwind().await;
                    assert_eq!(current(), Some((poller.trace_id(), poller.span_id())));
                    result
                })
                .await;
            match mode {
                Mode::Normal => result.unwrap().unwrap(),
                Mode::EncodeError | Mode::DecodeError => assert!(result.unwrap().is_err()),
                Mode::EncodePanic | Mode::DecodePanic => assert!(result.is_err()),
                Mode::Deadline => {
                    assert!(matches!(
                        result.unwrap(),
                        Err(parrot_api::errors::ActorError::Timeout
                            | parrot_api::errors::ActorError::TimeoutDetail(_))
                    ));
                    state.release.add_permits(1);
                }
                Mode::Pending => unreachable!(),
            }
            if mode == Mode::Normal {
                tokio::time::timeout(Duration::from_secs(5), state.started.acquire())
                    .await
                    .unwrap()
                    .unwrap()
                    .forget();
            }
        }
        assert!(current().is_none());
        assert!(
            pair.source.callbacks.is_empty(),
            "completed/cancelled calls must retire callback ownership"
        );
        pair.close().await;
        let reply_decode = !unpolled
            && matches!(entry, Entry::Send | Entry::Timeout | Entry::Options)
            && matches!(mode, Mode::Normal | Mode::DecodeError | Mode::DecodePanic);
        state.assert_client_context(expected, reply_decode, !unpolled);
    }
}
macro_rules! case {
    ($name:ident, $entry:ident, $tcp:expr, $mode:ident, $unpolled:expr) => {
        #[tokio::test]
        async fn $name() {
            scenario(Entry::$entry, $tcp, Mode::$mode, $unpolled).await;
        }
    };
}
case!(send_mem, Send, false, Normal, false);
case!(send_tcp, Send, true, Normal, false);
case!(timeout_mem, Timeout, false, Normal, false);
case!(timeout_tcp, Timeout, true, Normal, false);
case!(options_mem, Options, false, Normal, false);
case!(options_tcp, Options, true, Normal, false);
case!(tell_mem, Tell, false, Normal, false);
case!(tell_tcp, Tell, true, Normal, false);
case!(tell_options_mem, TellOptions, false, Normal, false);
case!(tell_options_tcp, TellOptions, true, Normal, false);
case!(completion_mem, Completion, false, Normal, false);
case!(completion_tcp, Completion, true, Normal, false);
case!(unpolled_send, Send, false, Normal, true);
case!(unpolled_timeout, Timeout, false, Normal, true);
case!(unpolled_options, Options, false, Normal, true);
case!(unpolled_tell, Tell, false, Normal, true);
case!(unpolled_tell_options, TellOptions, false, Normal, true);
case!(unpolled_completion, Completion, false, Normal, true);
case!(cancel_send_mem, Send, false, Pending, false);
case!(cancel_send_tcp, Send, true, Pending, false);
case!(cancel_timeout_mem, Timeout, false, Pending, false);
case!(cancel_timeout_tcp, Timeout, true, Pending, false);
case!(cancel_options_mem, Options, false, Pending, false);
case!(cancel_options_tcp, Options, true, Pending, false);
case!(cancel_completion_mem, Completion, false, Pending, false);
case!(cancel_completion_tcp, Completion, true, Pending, false);
case!(encode_error_send, Send, false, EncodeError, false);
case!(encode_error_timeout, Timeout, false, EncodeError, false);
case!(encode_error_options, Options, false, EncodeError, false);
case!(encode_error_tell, Tell, false, EncodeError, false);
case!(
    encode_error_tell_options,
    TellOptions,
    false,
    EncodeError,
    false
);
case!(
    encode_error_completion,
    Completion,
    false,
    EncodeError,
    false
);
case!(encode_panic_send, Send, false, EncodePanic, false);
case!(encode_panic_timeout, Timeout, false, EncodePanic, false);
case!(encode_panic_options, Options, false, EncodePanic, false);
case!(encode_panic_tell, Tell, false, EncodePanic, false);
case!(
    encode_panic_tell_options,
    TellOptions,
    false,
    EncodePanic,
    false
);
case!(
    encode_panic_completion,
    Completion,
    false,
    EncodePanic,
    false
);
case!(decode_error_send, Send, false, DecodeError, false);
case!(decode_error_timeout, Timeout, false, DecodeError, false);
case!(decode_error_options, Options, false, DecodeError, false);
case!(decode_panic_send, Send, false, DecodePanic, false);
case!(decode_panic_timeout, Timeout, false, DecodePanic, false);
case!(decode_panic_options, Options, false, DecodePanic, false);

case!(deadline_timeout_mem, Timeout, false, Deadline, false);
case!(deadline_timeout_tcp, Timeout, true, Deadline, false);
case!(deadline_options_mem, Options, false, Deadline, false);
case!(deadline_options_tcp, Options, true, Deadline, false);
case!(deadline_completion_mem, Completion, false, Deadline, false);
case!(deadline_completion_tcp, Completion, true, Deadline, false);
