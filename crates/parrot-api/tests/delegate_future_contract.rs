//! Exercise public adapters with user-supplied, independently owned futures.
//! Each case runs in a subprocess so a baseline double panic cannot abort the suite.
use async_trait::async_trait;
use futures::{task::noop_waker, FutureExt};
use parrot_api::{
    address::{ActorRef, ActorRefExt},
    errors::ActorError,
    message::{BackoffStrategy, Message, MessageOptions, MessagePriority, RetryPolicy},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    future::Future,
    marker::PhantomPinned,
    panic::AssertUnwindSafe,
    pin::Pin,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Api {
    DefaultAsk,
    DefaultTell,
    Ask,
    Tell,
    Retry,
}
impl Api {
    fn tell(self) -> bool {
        matches!(self, Self::DefaultTell | Self::Tell)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Healthy,
    WakeReady,
    TransportError,
    ConstructPanic,
    PollPanic,
    PollPayload,
    SuccessDropPanic,
    Cancel,
    CancelDropPanic,
    Unpolled,
    MessageDropPanic,
    InvalidPriority,
    InvalidRetry,
    Timeout,
    ReferenceDropPanic,
}
#[derive(Debug, Default)]
struct Counts {
    messages: AtomicUsize,
    futures: AtomicUsize,
    polls: AtomicUsize,
    calls: AtomicUsize,
    replies: AtomicUsize,
    decodes: AtomicUsize,
    references: AtomicUsize,
    predicates: AtomicUsize,
    payloads: AtomicUsize,
}
fn count(value: &AtomicUsize) -> usize {
    value.load(Ordering::SeqCst)
}
#[derive(Debug, Clone)]
struct Request {
    counts: Arc<Counts>,
    scenario: Scenario,
    retry: bool,
}
impl Drop for Request {
    fn drop(&mut self) {
        self.counts.messages.fetch_add(1, Ordering::SeqCst);
        if matches!(
            self.scenario,
            Scenario::Unpolled
                | Scenario::MessageDropPanic
                | Scenario::InvalidPriority
                | Scenario::InvalidRetry
                | Scenario::Timeout
        ) {
            panic!("message retirement");
        }
    }
}
#[derive(Debug)]
struct Reply {
    counts: Arc<Counts>,
    poison: bool,
}
impl Drop for Reply {
    fn drop(&mut self) {
        self.counts.replies.fetch_add(1, Ordering::SeqCst);
        assert!(!self.poison, "reply retirement");
    }
}
impl Message for Request {
    type Result = Reply;
    fn message_options(&self) -> Option<MessageOptions> {
        Some(MessageOptions {
            retry_policy: self.retry.then_some(RetryPolicy {
                max_attempts: 2,
                retry_interval: Duration::ZERO,
                backoff_strategy: BackoffStrategy::Fixed,
            }),
            ..Default::default()
        })
    }
    fn extract_result(value: BoxedMessage) -> ActorResult<Reply> {
        let value = *value.downcast::<Reply>().unwrap();
        value.counts.decodes.fetch_add(1, Ordering::SeqCst);
        Ok(value)
    }
}
struct Payload(Arc<Counts>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("panic payload retirement");
    }
}
struct Delegate<T> {
    request: Option<BoxedMessage>,
    counts: Arc<Counts>,
    scenario: Scenario,
    reply: fn(Arc<Counts>, bool) -> T,
    _pin: PhantomPinned,
}
impl<T> Future for Delegate<T> {
    type Output = ActorResult<T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_ref().get_ref();
        let previous = this.counts.polls.fetch_add(1, Ordering::SeqCst);
        if this.scenario == Scenario::WakeReady && previous == 0 {
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        match this.scenario {
            Scenario::PollPanic => panic!("delegate poll"),
            Scenario::PollPayload => std::panic::panic_any(Payload(this.counts.clone())),
            Scenario::Cancel | Scenario::CancelDropPanic | Scenario::Timeout => Poll::Pending,
            Scenario::TransportError => Poll::Ready(Err(ActorError::MessageHandlingError(
                "first transport error".into(),
            ))),
            _ => Poll::Ready(Ok((this.reply)(
                this.counts.clone(),
                matches!(
                    this.scenario,
                    Scenario::SuccessDropPanic
                        | Scenario::MessageDropPanic
                        | Scenario::ReferenceDropPanic
                ),
            ))),
        }
    }
}
impl<T> Drop for Delegate<T> {
    fn drop(&mut self) {
        self.counts.futures.fetch_add(1, Ordering::SeqCst);
        drop(self.request.take());
        if matches!(
            self.scenario,
            Scenario::TransportError
                | Scenario::PollPayload
                | Scenario::SuccessDropPanic
                | Scenario::CancelDropPanic
        ) {
            panic!("delegate retirement");
        }
    }
}
#[derive(Debug)]
struct Target {
    counts: Arc<Counts>,
    scenario: Scenario,
    owned: bool,
}
impl Target {
    fn cloned(&self) -> Self {
        Self {
            counts: self.counts.clone(),
            scenario: self.scenario,
            owned: true,
        }
    }
    fn construct<T: Send + 'static>(
        &self,
        msg: BoxedMessage,
        reply: fn(Arc<Counts>, bool) -> T,
    ) -> BoxedFuture<'_, ActorResult<T>> {
        self.counts.calls.fetch_add(1, Ordering::SeqCst);
        assert_ne!(
            self.scenario,
            Scenario::ConstructPanic,
            "delegate construction"
        );
        Box::pin(Delegate {
            request: Some(msg),
            counts: self.counts.clone(),
            scenario: self.scenario,
            reply,
            _pin: PhantomPinned,
        })
    }
}
impl Drop for Target {
    fn drop(&mut self) {
        if self.owned {
            self.counts.references.fetch_add(1, Ordering::SeqCst);
            if matches!(
                self.scenario,
                Scenario::CancelDropPanic | Scenario::Unpolled | Scenario::ReferenceDropPanic
            ) {
                panic!("reference retirement");
            }
        }
    }
}
fn reply(counts: Arc<Counts>, poison: bool) -> BoxedMessage {
    Box::new(Reply { counts, poison })
}
macro_rules! reference_methods {
    () => {
        fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.target().construct(msg, reply)
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send(msg)
        }
        fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            self.target().construct(msg, |_, _| ())
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn path(&self) -> String {
            "/delegate".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { true })
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    };
}
impl Target {
    fn target(&self) -> &Target {
        self
    }
}
#[async_trait]
impl ActorRef for Target {
    reference_methods!();
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.cloned())
    }
}
#[derive(Debug)]
struct Override(Target);
impl Override {
    fn target(&self) -> &Target {
        &self.0
    }
}
#[async_trait]
impl ActorRef for Override {
    reference_methods!();
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self(self.0.cloned()))
    }
    fn send_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        _: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(msg)
    }
    fn deliver_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        _: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        self.deliver(msg)
    }
}

async fn exercise(api: Api, scenario: Scenario) {
    let counts = Arc::new(Counts::default());
    let target = Target {
        counts: counts.clone(),
        scenario,
        owned: false,
    };
    let overriding = Override(Target {
        counts: counts.clone(),
        scenario,
        owned: false,
    });
    let msg = Request {
        counts: counts.clone(),
        scenario,
        retry: api == Api::Retry,
    };
    let mut options = MessageOptions::default();
    if scenario == Scenario::InvalidPriority {
        options.priority = MessagePriority::new(99).unwrap();
    }
    if scenario == Scenario::InvalidRetry {
        options.retry_policy = Some(RetryPolicy {
            max_attempts: 1,
            retry_interval: Duration::ZERO,
            backoff_strategy: BackoffStrategy::Fixed,
        });
    }
    if scenario == Scenario::Timeout {
        options.timeout = Some(Duration::from_millis(1));
    }
    // Map only the public adapter output; do not guard its own cancellation here.
    let mut future: BoxedFuture<'_, ActorResult<Option<BoxedMessage>>> = match api {
        Api::DefaultAsk => Box::pin(
            target
                .send_with_options(Box::new(msg), options)
                .map(|r| r.map(Some)),
        ),
        Api::DefaultTell => Box::pin(
            target
                .deliver_with_options(Box::new(msg), options)
                .map(|r| r.map(|()| None)),
        ),
        Api::Ask => Box::pin(
            overriding
                .ask(msg)
                .map(|r| r.map(|v| Some(Box::new(v) as BoxedMessage))),
        ),
        Api::Tell => Box::pin(overriding.tell(msg).map(|r| r.map(|()| None))),
        Api::Retry => Box::pin(
            overriding
                .ask_retrying(msg, |_| {
                    counts.predicates.fetch_add(1, Ordering::SeqCst);
                    false
                })
                .map(|r| r.map(|v| Some(Box::new(v) as BoxedMessage))),
        ),
    };
    if matches!(
        scenario,
        Scenario::Unpolled | Scenario::Cancel | Scenario::CancelDropPanic
    ) {
        if scenario != Scenario::Unpolled {
            let waker = noop_waker();
            assert!(future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending());
        }
        assert!(
            std::panic::catch_unwind(AssertUnwindSafe(|| drop(future))).is_ok(),
            "cancellation escaped the adapter"
        );
    } else {
        let result = AssertUnwindSafe(future)
            .catch_unwind()
            .await
            .expect("delegate panic escaped public adapter");
        match scenario {
            Scenario::Healthy | Scenario::WakeReady => {
                let result = result.unwrap();
                assert_eq!(result.is_none(), api.tell());
                drop(result);
            }
            Scenario::TransportError => assert!(
                matches!(result, Err(ActorError::MessageHandlingError(ref text)) if text == "first transport error")
            ),
            Scenario::InvalidPriority => assert!(
                matches!(result, Err(ActorError::MessageHandlingError(ref text)) if text.contains("priority"))
            ),
            Scenario::InvalidRetry => assert!(
                matches!(result, Err(ActorError::MessageHandlingError(ref text)) if text.contains("retry_policy"))
            ),
            Scenario::Timeout => assert!(matches!(result, Err(ActorError::Timeout))),
            _ => {
                let expected = match scenario {
                    Scenario::ConstructPanic => "delegate construction",
                    Scenario::PollPanic => "delegate poll",
                    Scenario::PollPayload => {
                        "non-string panic payload; panic payload destructor panicked"
                    }
                    Scenario::SuccessDropPanic => "delegate retirement",
                    Scenario::MessageDropPanic => "message retirement",
                    Scenario::ReferenceDropPanic => "reference retirement",
                    _ => unreachable!(),
                };
                assert!(
                    matches!(result, Err(ActorError::Panic(ref text)) if text.contains(expected)),
                    "first panic identity must survive retirement"
                );
            }
        }
    }
    let entered = !matches!(
        scenario,
        Scenario::Unpolled | Scenario::InvalidPriority | Scenario::InvalidRetry
    );
    assert_eq!(count(&counts.calls), usize::from(entered), "business calls");
    assert_eq!(
        count(&counts.messages),
        1 + usize::from(api == Api::Retry && entered),
        "message retirement"
    );
    assert_eq!(
        count(&counts.futures),
        usize::from(entered && scenario != Scenario::ConstructPanic),
        "future retirement"
    );
    assert_eq!(
        count(&counts.references),
        usize::from(api == Api::Ask),
        "owned reference retirement"
    );
    let produced = matches!(
        scenario,
        Scenario::Healthy
            | Scenario::WakeReady
            | Scenario::SuccessDropPanic
            | Scenario::MessageDropPanic
            | Scenario::ReferenceDropPanic
    ) && !api.tell();
    assert_eq!(
        count(&counts.replies),
        usize::from(produced),
        "reply retirement"
    );
    let decoded = matches!(
        scenario,
        Scenario::Healthy | Scenario::WakeReady | Scenario::ReferenceDropPanic
    ) && matches!(api, Api::Ask | Api::Retry);
    assert_eq!(
        count(&counts.decodes),
        usize::from(decoded),
        "decode only after successful delegate retirement"
    );
    let predicate = api == Api::Retry
        && entered
        && !matches!(
            scenario,
            Scenario::Healthy
                | Scenario::WakeReady
                | Scenario::Cancel
                | Scenario::CancelDropPanic
                | Scenario::Unpolled
        );
    assert_eq!(
        count(&counts.predicates),
        usize::from(predicate),
        "retry consent"
    );
    assert_eq!(
        count(&counts.payloads),
        usize::from(scenario == Scenario::PollPayload),
        "recursive panic payload retirement"
    );
    if entered && !matches!(scenario, Scenario::Timeout | Scenario::ConstructPanic) {
        assert_eq!(
            count(&counts.polls),
            if scenario == Scenario::WakeReady {
                2
            } else {
                1
            }
        );
    }
}
fn isolated(name: &str, api: Api, scenario: Scenario) {
    const CHILD: &str = "PARROT_DELEGATE_CONTRACT_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(exercise(api, scenario));
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated contract failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
macro_rules! case {
    ($name:ident, $api:ident, $scenario:ident) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), Api::$api, Scenario::$scenario);
        }
    };
}
case!(defaultask_healthy, DefaultAsk, Healthy);
case!(defaultask_transporterror, DefaultAsk, TransportError);
case!(defaultask_constructpanic, DefaultAsk, ConstructPanic);
case!(defaultask_pollpanic, DefaultAsk, PollPanic);
case!(defaultask_pollpayload, DefaultAsk, PollPayload);
case!(defaultask_successdroppanic, DefaultAsk, SuccessDropPanic);
case!(defaultask_cancel, DefaultAsk, Cancel);
case!(defaultask_canceldroppanic, DefaultAsk, CancelDropPanic);
case!(defaultask_unpolled, DefaultAsk, Unpolled);
case!(defaultask_messagedroppanic, DefaultAsk, MessageDropPanic);
case!(defaulttell_healthy, DefaultTell, Healthy);
case!(defaulttell_transporterror, DefaultTell, TransportError);
case!(defaulttell_constructpanic, DefaultTell, ConstructPanic);
case!(defaulttell_pollpanic, DefaultTell, PollPanic);
case!(defaulttell_pollpayload, DefaultTell, PollPayload);
case!(defaulttell_successdroppanic, DefaultTell, SuccessDropPanic);
case!(defaulttell_cancel, DefaultTell, Cancel);
case!(defaulttell_canceldroppanic, DefaultTell, CancelDropPanic);
case!(defaulttell_unpolled, DefaultTell, Unpolled);
case!(defaulttell_messagedroppanic, DefaultTell, MessageDropPanic);
case!(ask_healthy, Ask, Healthy);
case!(ask_transporterror, Ask, TransportError);
case!(ask_constructpanic, Ask, ConstructPanic);
case!(ask_pollpanic, Ask, PollPanic);
case!(ask_pollpayload, Ask, PollPayload);
case!(ask_successdroppanic, Ask, SuccessDropPanic);
case!(ask_cancel, Ask, Cancel);
case!(ask_canceldroppanic, Ask, CancelDropPanic);
case!(ask_unpolled, Ask, Unpolled);
case!(ask_messagedroppanic, Ask, MessageDropPanic);
case!(tell_healthy, Tell, Healthy);
case!(tell_transporterror, Tell, TransportError);
case!(tell_constructpanic, Tell, ConstructPanic);
case!(tell_pollpanic, Tell, PollPanic);
case!(tell_pollpayload, Tell, PollPayload);
case!(tell_successdroppanic, Tell, SuccessDropPanic);
case!(tell_cancel, Tell, Cancel);
case!(tell_canceldroppanic, Tell, CancelDropPanic);
case!(tell_unpolled, Tell, Unpolled);
case!(tell_messagedroppanic, Tell, MessageDropPanic);
case!(retry_healthy, Retry, Healthy);
case!(retry_transporterror, Retry, TransportError);
case!(retry_constructpanic, Retry, ConstructPanic);
case!(retry_pollpanic, Retry, PollPanic);
case!(retry_pollpayload, Retry, PollPayload);
case!(retry_successdroppanic, Retry, SuccessDropPanic);
case!(retry_cancel, Retry, Cancel);
case!(retry_canceldroppanic, Retry, CancelDropPanic);
case!(retry_unpolled, Retry, Unpolled);
case!(retry_messagedroppanic, Retry, MessageDropPanic);
case!(defaultask_invalidpriority, DefaultAsk, InvalidPriority);
case!(defaultask_invalidretry, DefaultAsk, InvalidRetry);
case!(defaulttell_invalidpriority, DefaultTell, InvalidPriority);
case!(defaulttell_invalidretry, DefaultTell, InvalidRetry);
case!(defaulttell_timeout, DefaultTell, Timeout);
case!(ask_referencedroppanic, Ask, ReferenceDropPanic);
case!(defaultask_wakeready, DefaultAsk, WakeReady);
case!(defaulttell_wakeready, DefaultTell, WakeReady);
case!(ask_wakeready, Ask, WakeReady);
case!(tell_wakeready, Tell, WakeReady);
case!(retry_wakeready, Retry, WakeReady);
