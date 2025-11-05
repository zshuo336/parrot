use async_trait::async_trait;
use futures::FutureExt;
use parrot_api::{
    address::{ActorRef, ActorRefExt},
    errors::ActorError,
    message::{BackoffStrategy, Message, MessageOptions, RetryPolicy},
    typed::{ParrotMsgVariant, ParrotTypedDispatch, TypedAskRef},
    typed_channel::{typed_channel, TypedActorRef},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Healthy,
    InjectPanic,
    InjectPayload,
    ExtractPanic,
    ExtractPayload,
    ExtractError,
    DecodePanic,
    DecodePayload,
    DecodeError,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Api {
    Ask,
    Tell,
    BridgeAsk,
    BridgeTell,
    TraitAsk,
    DynamicAsk,
    Retrying,
}
#[derive(Debug, Default)]
struct Counts {
    messages: AtomicUsize,
    replies: AtomicUsize,
    injected: AtomicUsize,
    extracted: AtomicUsize,
    decoded: AtomicUsize,
    sends: AtomicUsize,
    predicates: AtomicUsize,
    payloads: AtomicUsize,
}
struct Payload(Arc<Counts>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("conversion panic payload destructor");
    }
}
#[derive(Debug, Clone)]
struct Input {
    mode: Mode,
    counts: Arc<Counts>,
    retry: bool,
}
impl Drop for Input {
    fn drop(&mut self) {
        self.counts.messages.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Debug)]
struct Reply {
    mode: Mode,
    counts: Arc<Counts>,
}
impl Drop for Reply {
    fn drop(&mut self) {
        self.counts.replies.fetch_add(1, Ordering::SeqCst);
    }
}
impl Message for Input {
    type Result = usize;
    fn message_options(&self) -> Option<MessageOptions> {
        Some(MessageOptions {
            retry_policy: self.retry.then_some(RetryPolicy {
                max_attempts: 3,
                retry_interval: Duration::ZERO,
                backoff_strategy: BackoffStrategy::Fixed,
            }),
            ..Default::default()
        })
    }
    fn extract_result(value: BoxedMessage) -> ActorResult<usize> {
        let reply = value.downcast::<Reply>().unwrap();
        reply.counts.decoded.fetch_add(1, Ordering::SeqCst);
        match reply.mode {
            Mode::DecodePanic => panic!("first decode failure"),
            Mode::DecodePayload => std::panic::panic_any(Payload(reply.counts.clone())),
            Mode::DecodeError => Err(ActorError::MessageHandlingError("decode refusal".into())),
            _ => Ok(42),
        }
    }
}
struct Actor;
impl ParrotTypedDispatch for Actor {
    type Msg = Input;
    type Reply = Reply;
    fn dispatch(&mut self, _: Input) -> BoxedFuture<'_, ActorResult<Reply>> {
        panic!("conversion contract uses a controlled receiver")
    }
}
impl ParrotMsgVariant<Actor> for Input {
    fn inject(value: Self) -> Input {
        value.counts.injected.fetch_add(1, Ordering::SeqCst);
        match value.mode {
            Mode::InjectPanic => panic!("first inject failure"),
            Mode::InjectPayload => std::panic::panic_any(Payload(value.counts.clone())),
            _ => value,
        }
    }
    fn extract(reply: Reply) -> ActorResult<usize> {
        reply.counts.extracted.fetch_add(1, Ordering::SeqCst);
        match reply.mode {
            Mode::ExtractPanic => panic!("first extract failure"),
            Mode::ExtractPayload => std::panic::panic_any(Payload(reply.counts.clone())),
            Mode::ExtractError => Err(ActorError::MessageHandlingError("extract refusal".into())),
            _ => Ok(42),
        }
    }
}
#[derive(Debug, Clone)]
struct Target;
#[async_trait]
impl ActorRef for Target {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let msg = msg.downcast::<Input>().unwrap();
            msg.counts.sends.fetch_add(1, Ordering::SeqCst);
            let reply = Reply {
                mode: msg.mode,
                counts: msg.counts.clone(),
            };
            drop(msg);
            Ok(Box::new(reply) as BoxedMessage)
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(msg)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { panic!("unexpected dynamic tell") })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/conversion".into()
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
async fn exercise(api: Api, mode: Mode) {
    let counts = Arc::new(Counts::default());
    let input = Input {
        mode,
        counts: counts.clone(),
        retry: api == Api::Retrying,
    };
    let dynamic = matches!(api, Api::DynamicAsk | Api::Retrying);
    let tell = matches!(api, Api::Tell | Api::BridgeTell);
    let inject_failure = matches!(mode, Mode::InjectPanic | Mode::InjectPayload);
    let (tx, rx) = typed_channel::<Actor>(1);
    let reference = TypedActorRef::<Actor, Input>::new(tx, "/conversion".into());
    let worker_rx = rx.clone();
    let worker = (!dynamic).then(|| {
        tokio::spawn(async move {
            let envelope = worker_rx.recv_async().await.unwrap();
            envelope.msg.counts.sends.fetch_add(1, Ordering::SeqCst);
            if let Some(reply) = envelope.reply {
                reply
                    .try_send(Ok(Reply {
                        mode: envelope.msg.mode,
                        counts: envelope.msg.counts.clone(),
                    }))
                    .unwrap();
            }
        })
    });
    let observed = AssertUnwindSafe(async {
        match api {
            Api::Ask => reference.ask(input).await,
            Api::Tell => reference.tell(input).await.map(|_| 42),
            Api::TraitAsk => TypedAskRef::ask(&reference, input).await,
            Api::BridgeAsk => reference
                .into_dyn()
                .send(Box::new(input))
                .await
                .map(|value| *value.downcast::<usize>().unwrap()),
            Api::BridgeTell => reference
                .into_dyn()
                .deliver(Box::new(input))
                .await
                .map(|_| 42),
            Api::DynamicAsk => Target.ask(input).await,
            Api::Retrying => {
                Target
                    .ask_retrying(input, |_| {
                        counts.predicates.fetch_add(1, Ordering::SeqCst);
                        true
                    })
                    .await
            }
        }
    })
    .catch_unwind()
    .await;
    if let Some(worker) = worker {
        if inject_failure {
            worker.abort();
            let _ = worker.await;
        } else {
            tokio::time::timeout(Duration::from_secs(2), worker)
                .await
                .unwrap()
                .unwrap();
        }
    }
    assert!(
        rx.try_recv().is_err(),
        "conversion failure left a queued message"
    );
    // Consume any uncaught recursive panic payload safely before failing the oracle.
    let result = match observed {
        Ok(result) => result,
        Err(payload) => {
            if let Err(secondary) = std::panic::catch_unwind(AssertUnwindSafe(|| drop(payload))) {
                std::mem::forget(secondary);
            }
            panic!("conversion panic escaped the public API future");
        }
    };
    match mode {
        Mode::Healthy => assert_eq!(result.unwrap(), 42),
        Mode::ExtractError | Mode::DecodeError => assert!(
            matches!(result, Err(ActorError::MessageHandlingError(ref text)) if text.ends_with(" refusal"))
        ),
        _ => {
            let Err(ActorError::Panic(text)) = result else {
                panic!("conversion panic lost: {result:?}")
            };
            if matches!(
                mode,
                Mode::InjectPayload | Mode::ExtractPayload | Mode::DecodePayload
            ) {
                assert!(
                    text.contains("non-string panic payload")
                        && text.contains("panic payload destructor panicked")
                );
                assert_eq!(counts.payloads.load(Ordering::SeqCst), 1);
            } else {
                assert!(text.contains("first ") && text.contains(" failure"));
            }
        }
    }
    assert_eq!(
        counts.messages.load(Ordering::SeqCst),
        if api == Api::Retrying { 2 } else { 1 }
    );
    assert_eq!(
        counts.sends.load(Ordering::SeqCst),
        usize::from(!inject_failure)
    );
    assert_eq!(
        counts.injected.load(Ordering::SeqCst),
        usize::from(!dynamic)
    );
    assert_eq!(
        counts.extracted.load(Ordering::SeqCst),
        usize::from(!dynamic && !tell && !inject_failure)
    );
    assert_eq!(counts.decoded.load(Ordering::SeqCst), usize::from(dynamic));
    assert_eq!(
        counts.replies.load(Ordering::SeqCst),
        usize::from(!tell && !inject_failure)
    );
    assert_eq!(
        counts.predicates.load(Ordering::SeqCst),
        0,
        "decode failures must never replay business work"
    );
}
macro_rules! case {
    ($name:ident,$api:ident,$mode:ident) => {
        #[tokio::test]
        async fn $name() {
            exercise(Api::$api, Mode::$mode).await;
        }
    };
}
case!(ask_inject_panic, Ask, InjectPanic);
case!(ask_inject_recursive_payload, Ask, InjectPayload);
case!(tell_inject_panic, Tell, InjectPanic);
case!(tell_inject_recursive_payload, Tell, InjectPayload);
case!(bridgeask_inject_panic, BridgeAsk, InjectPanic);
case!(bridgeask_inject_recursive_payload, BridgeAsk, InjectPayload);
case!(bridgetell_inject_panic, BridgeTell, InjectPanic);
case!(
    bridgetell_inject_recursive_payload,
    BridgeTell,
    InjectPayload
);
case!(ask_extract_panic, Ask, ExtractPanic);
case!(ask_extract_recursive_payload, Ask, ExtractPayload);
case!(ask_extract_error, Ask, ExtractError);
case!(ask_extract_success, Ask, Healthy);
case!(bridgeask_extract_panic, BridgeAsk, ExtractPanic);
case!(
    bridgeask_extract_recursive_payload,
    BridgeAsk,
    ExtractPayload
);
case!(bridgeask_extract_error, BridgeAsk, ExtractError);
case!(bridgeask_extract_success, BridgeAsk, Healthy);
case!(traitask_extract_panic, TraitAsk, ExtractPanic);
case!(traitask_extract_recursive_payload, TraitAsk, ExtractPayload);
case!(traitask_extract_error, TraitAsk, ExtractError);
case!(traitask_extract_success, TraitAsk, Healthy);
case!(dynamicask_decode_panic, DynamicAsk, DecodePanic);
case!(
    dynamicask_decode_recursive_payload,
    DynamicAsk,
    DecodePayload
);
case!(dynamicask_decode_error, DynamicAsk, DecodeError);
case!(dynamicask_decode_success, DynamicAsk, Healthy);
case!(retrying_decode_panic, Retrying, DecodePanic);
case!(retrying_decode_recursive_payload, Retrying, DecodePayload);
case!(retrying_decode_error, Retrying, DecodeError);
case!(retrying_decode_success, Retrying, Healthy);
case!(tell_inject_success, Tell, Healthy);
case!(bridgetell_inject_success, BridgeTell, Healthy);
