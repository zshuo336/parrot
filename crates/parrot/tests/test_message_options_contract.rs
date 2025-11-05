//! Delivery metadata must affect real admission and execution, not just envelopes.
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot::thread::{
    ThreadActorSystem,
    config::{SchedulingMode, SupervisorStrategy, ThreadActorConfig},
    context::ThreadContext,
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::{ActorRef, ActorRefExt},
    errors::ActorError,
    message::{BackoffStrategy, Message, MessageOptions, MessagePriority, RetryPolicy},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Request {
    id: u32,
    high: bool,
    retries: Option<u32>,
    fail_until: u32,
}
impl Request {
    fn normal(id: u32) -> Self {
        Self {
            id,
            high: false,
            retries: None,
            fail_until: 0,
        }
    }
}
impl Message for Request {
    type Result = Reply;
    fn message_options(&self) -> Option<MessageOptions> {
        Some(MessageOptions {
            timeout: Some(Duration::from_secs(3)),
            priority: if self.high {
                MessagePriority::HIGH
            } else {
                MessagePriority::NORMAL
            },
            retry_policy: self.retries.map(|max_attempts| RetryPolicy {
                max_attempts,
                retry_interval: Duration::from_millis(5),
                backoff_strategy: BackoffStrategy::Linear,
            }),
        })
    }
}
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Reply(u32);
macro_rules! codec {
    ($t:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key, type_id: std::any::TypeId::of::<$t>(),
                encode: |msg| parrot_api::message::serde_remote_serialize(msg.downcast_ref::<$t>().ok_or("wrong type")?),
                decode: |bytes| Ok(Box::new(parrot_api::message::serde_remote_deserialize::<$t>(bytes)?) as BoxedMessage),
            }
        }
    }
}
codec!(Request, "bin:options-contract::Request#v1");
codec!(Reply, "bin:options-contract::Reply#v1");
#[derive(Default)]
struct State {
    entered: Notify,
    release: Notify,
    seen: Mutex<Vec<u32>>,
}
struct Hold;
async fn handle(state: &State, msg: BoxedMessage) -> ActorResult<BoxedMessage> {
    if msg.is::<Hold>() {
        state.entered.notify_one();
        state.release.notified().await;
        return Ok(Box::new(()));
    }
    let r = msg.downcast::<Request>().unwrap();
    let attempt = {
        let mut seen = state.seen.lock().unwrap();
        seen.push(r.id);
        seen.iter().filter(|id| **id == r.id).count() as u32
    };
    if attempt <= r.fail_until {
        return Err(ActorError::MessageHandlingError("transient".into()));
    }
    Ok(Box::new(Reply(r.id)))
}
struct ThreadProbe(Arc<State>);
struct ActixProbe(Arc<State>);
impl ActixEngineExt for ActixProbe {}
macro_rules! actor {
    ($t:ty,$c:ty) => {
        impl Actor for $t {
            type Config = EmptyConfig;
            type Context = $c;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(handle(&self.0, msg))
            }
        }
    };
}
actor!(ThreadProbe, ThreadContext<Self>);
actor!(ActixProbe, ActixContext<ActixActor<Self>>);
async fn thread(
    state: Arc<State>,
    config: ThreadActorConfig,
) -> (Arc<ThreadActorSystem>, BoxedActorRef) {
    let sys = ThreadActorSystem::shared(Default::default());
    let reference = sys
        .spawn_at(
            ThreadProbe(state),
            "/probe",
            None,
            ThreadActorConfig {
                supervisor_strategy: Some(SupervisorStrategy::Resume),
                ..config
            },
        )
        .await
        .unwrap();
    (sys, reference.clone_boxed())
}
async fn gate(reference: &dyn ActorRef, state: &State) {
    reference.deliver(Box::new(Hold)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), state.entered.notified())
        .await
        .unwrap();
}
async fn retry_contract(reference: &dyn ActorRef, state: &State) {
    let mut r = Request::normal(10);
    r.retries = Some(2);
    r.fail_until = 2;
    assert!(
        reference
            .ask(r.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("ask_retrying")
    );
    assert!(
        reference
            .tell(r.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("ask_retrying")
    );
    assert!(
        state.seen.lock().unwrap().is_empty(),
        "single-attempt rejection must precede side effects"
    );
    assert_eq!(reference.ask_retrying(r, |_| true).await.unwrap().0, 10);
    let mut r = Request::normal(20);
    r.retries = Some(2);
    r.fail_until = 10;
    assert!(reference.ask_retrying(r, |_| true).await.is_err());
    let mut r = Request::normal(30);
    r.retries = Some(4);
    r.fail_until = 10;
    assert!(reference.ask_retrying(r, |_| false).await.is_err());
    assert_eq!(*state.seen.lock().unwrap(), [10, 10, 10, 20, 20, 20, 30]);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_thread_retry_budget_predicate_and_single_attempt_rejection() {
    let state = Arc::new(State::default());
    let (sys, r) = thread(state.clone(), Default::default()).await;
    retry_contract(&*r, &state).await;
    sys.shutdown_internal().await.unwrap();
}
#[actix::test]
async fn real_actix_retry_budget_and_unsupported_priority_rejection() {
    let state = Arc::new(State::default());
    let sys = ActixActorSystem::with_settings(Default::default())
        .await
        .unwrap();
    let r = sys
        .spawn_root_typed_with_strategy(
            ActixProbe(state.clone()),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    retry_contract(&*r, &state).await;
    let mut high = Request::normal(40);
    high.high = true;
    assert!(
        r.ask(high.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("priority")
    );
    assert!(
        r.tell(high)
            .await
            .unwrap_err()
            .to_string()
            .contains("priority")
    );
    assert!(!state.seen.lock().unwrap().contains(&40));
    sys.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tell_deadline_bounds_full_mailbox_and_cannot_admit_late() {
    let state = Arc::new(State::default());
    let (sys, r) = thread(
        state.clone(),
        ThreadActorConfig {
            mailbox_capacity: Some(1),
            ..Default::default()
        },
    )
    .await;
    gate(&*r, &state).await;
    r.tell(Request::normal(1)).await.unwrap();
    let result = r
        .deliver_with_options(
            Box::new(Request::normal(2)),
            MessageOptions {
                timeout: Some(Duration::from_millis(30)),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(ActorError::Timeout)));
    state.release.notify_one();
    r.ask(Request::normal(3)).await.unwrap();
    assert_eq!(*state.seen.lock().unwrap(), [1, 3]);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_priority_survives_legacy_and_admission_wrappers_with_fifo_in_each_lane() {
    #[derive(Debug)]
    struct Admission;
    impl parrot::thread::mailbox::admission::MessageAdmission for Admission {
        fn prepare(
            &self,
            msg: BoxedMessage,
        ) -> Result<BoxedMessage, parrot::thread::error::MailboxError> {
            Ok(msg)
        }
    }
    for wrapped in [false, true] {
        let state = Arc::new(State::default());
        let (sys, r) = thread(
            state.clone(),
            ThreadActorConfig {
                message_admission: wrapped.then(|| Arc::new(Admission) as _),
                ..Default::default()
            },
        )
        .await;
        gate(&*r, &state).await;
        for id in 0..3 {
            r.tell(Request::normal(id)).await.unwrap();
        }
        let mut h = Request::normal(10);
        h.high = true;
        let mut first = r.ask(h.clone());
        assert!(futures::poll!(&mut first).is_pending());
        h.id = 11;
        let mut second = r.ask(h);
        assert!(futures::poll!(&mut second).is_pending());
        assert_eq!(sys.get_mailbox("/probe").unwrap().len().await, 5);
        state.release.notify_one();
        first.await.unwrap();
        second.await.unwrap();
        r.ask(Request::normal(20)).await.unwrap();
        assert_eq!(*state.seen.lock().unwrap(), [10, 11, 0, 1, 2, 20]);
        sys.shutdown_internal().await.unwrap();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedicated_fifo_mailbox_rejects_high_before_admission() {
    let state = Arc::new(State::default());
    let (sys, r) = thread(
        state.clone(),
        ThreadActorConfig {
            scheduling_mode: Some(SchedulingMode::DedicatedThread),
            ..Default::default()
        },
    )
    .await;
    let mut h = Request::normal(1);
    h.high = true;
    assert!(
        r.tell(h.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("priority")
    );
    assert!(r.ask(h).await.unwrap_err().to_string().contains("priority"));
    assert!(state.seen.lock().unwrap().is_empty());
    sys.shutdown_internal().await.unwrap();
}
struct Lookup(Option<Arc<dyn ActorRef>>);
#[async_trait::async_trait]
impl parrot_remote::LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        if path != "/probe" {
            return None;
        }
        self.0.as_ref().map(|r| r.clone_boxed())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_priority_reaches_real_mailbox_after_wire_ordering_and_retry_executes() {
    let state = Arc::new(State::default());
    let (sys, local) = thread(state.clone(), Default::default()).await;
    use parrot_remote::{NodeAddr, RemoteActorSystem, RemoteConfig};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let server = RemoteActorSystem::new(
        RemoteConfig::tcp("options-server", Some(addr)).with_insecure_development(),
        Arc::new(Lookup(Some(Arc::from(local.clone_boxed())))),
    )
    .unwrap();
    server.start().await.unwrap();
    let client = RemoteActorSystem::new(
        RemoteConfig::tcp("options-client", None).with_insecure_development(),
        Arc::new(Lookup(None)),
    )
    .unwrap();
    client.start().await.unwrap();
    client
        .connect(&NodeAddr::tcp("options-server", addr))
        .await
        .unwrap();
    let r = client.remote_ref("parrot://options-server/probe").unwrap();
    retry_contract(&r, &state).await;
    state.seen.lock().unwrap().clear();
    gate(&*local, &state).await;
    for id in 0..3 {
        r.tell(Request::normal(id)).await.unwrap();
    }
    let mut h = Request::normal(10);
    h.high = true;
    r.tell(h.clone()).await.unwrap();
    h.id = 11;
    let mut high_ask = r.ask(h);
    assert!(futures::poll!(&mut high_ask).is_pending());
    let mailbox = sys.get_mailbox("/probe").unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while mailbox.len().await != 5 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    state.release.notify_one();
    high_ask.await.unwrap();
    local.ask(Request::normal(20)).await.unwrap();
    let seen = state.seen.lock().unwrap().clone();
    // ASK and TELL can arrive concurrently; both high messages must precede normals.
    assert!(
        seen[..2].contains(&10) && seen[..2].contains(&11),
        "{seen:?}"
    );
    assert_eq!(&seen[2..], &[0, 1, 2, 20]);
    client.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
    sys.shutdown_internal().await.unwrap();
}

#[derive(Clone)]
struct TimedRequest(RetryPolicy);
impl Message for TimedRequest {
    type Result = u32;
    fn message_options(&self) -> Option<MessageOptions> {
        Some(MessageOptions {
            retry_policy: Some(self.0.clone()),
            ..Default::default()
        })
    }
}
#[derive(Debug, Clone)]
struct TimingRef {
    calls: Arc<Mutex<Vec<tokio::time::Instant>>>,
    succeed_at: usize,
    wrong_type: bool,
}
impl ActorRef for TimingRef {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let n = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(tokio::time::Instant::now());
                calls.len()
            };
            if n < self.succeed_at {
                Err(ActorError::MessageHandlingError("transient".into()))
            } else if self.wrong_type {
                Ok(Box::new(()) as BoxedMessage)
            } else {
                Ok(Box::new(42u32) as BoxedMessage)
            }
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
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "timing".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn timing(succeed_at: usize) -> TimingRef {
    TimingRef {
        calls: Default::default(),
        succeed_at,
        wrong_type: false,
    }
}
#[tokio::test(start_paused = true)]
async fn retry_backoff_has_exact_attempt_times_and_caps_exponential_growth() {
    for (backoff, expected) in [
        (BackoffStrategy::Fixed, vec![0, 10, 20, 30]),
        (BackoffStrategy::Linear, vec![0, 10, 30, 60]),
        (
            BackoffStrategy::Exponential {
                base: 2.0,
                max_interval: Duration::from_millis(25),
            },
            vec![0, 10, 30, 55],
        ),
    ] {
        let r = timing(4);
        let start = tokio::time::Instant::now();
        assert_eq!(
            r.ask_retrying(
                TimedRequest(RetryPolicy {
                    max_attempts: 3,
                    retry_interval: Duration::from_millis(10),
                    backoff_strategy: backoff
                }),
                |_| true
            )
            .await
            .unwrap(),
            42
        );
        let actual: Vec<_> = r
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.duration_since(start).as_millis())
            .collect();
        assert_eq!(actual, expected);
    }
}
#[tokio::test(start_paused = true)]
async fn retry_cancellation_invalid_policy_zero_budget_and_bad_response_are_not_replayed() {
    let policy = RetryPolicy {
        max_attempts: 3,
        retry_interval: Duration::from_secs(1),
        backoff_strategy: BackoffStrategy::Fixed,
    };
    let r = timing(4);
    let mut pending = r.ask_retrying(TimedRequest(policy.clone()), |_| true);
    assert!(futures::poll!(&mut pending).is_pending());
    assert_eq!(r.calls.lock().unwrap().len(), 1);
    drop(pending);
    tokio::time::advance(Duration::from_secs(30)).await;
    assert_eq!(r.calls.lock().unwrap().len(), 1);
    let r = timing(2);
    assert!(
        r.ask_retrying(
            TimedRequest(RetryPolicy {
                max_attempts: 0,
                ..policy.clone()
            }),
            |_| true
        )
        .await
        .is_err()
    );
    assert_eq!(r.calls.lock().unwrap().len(), 1);
    for base in [f64::NAN, f64::INFINITY, 0.5, -1.0] {
        let r = timing(1);
        let bad = RetryPolicy {
            backoff_strategy: BackoffStrategy::Exponential {
                base,
                max_interval: Duration::from_secs(1),
            },
            ..policy.clone()
        };
        assert!(r.ask_retrying(TimedRequest(bad), |_| true).await.is_err());
        assert!(r.calls.lock().unwrap().is_empty());
    }
    let r = TimingRef {
        wrong_type: true,
        ..timing(1)
    };
    assert!(
        r.ask_retrying(TimedRequest(policy), |_| panic!(
            "decoding error is not retryable"
        ))
        .await
        .is_err()
    );
    assert_eq!(r.calls.lock().unwrap().len(), 1);
}

pub struct StaticRequest {
    timeout: Option<Duration>,
    high: bool,
}
impl Message for StaticRequest {
    type Result = u32;
    fn message_options(&self) -> Option<MessageOptions> {
        Some(MessageOptions {
            timeout: self.timeout,
            priority: if self.high {
                MessagePriority::HIGH
            } else {
                MessagePriority::NORMAL
            },
            ..Default::default()
        })
    }
}
#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(StaticRequest))]
struct StaticProbe;
impl parrot_api::typed::TypedReceive<StaticRequest> for StaticProbe {
    fn receive_typed<'a>(&'a mut self, _: StaticRequest) -> BoxedFuture<'a, ActorResult<u32>> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(60)).await;
            Ok(7)
        })
    }
}
async fn static_contract(r: parrot_api::typed_channel::TypedActorRef<StaticProbe, StaticRequest>) {
    let message = || StaticRequest {
        timeout: Some(Duration::from_millis(10)),
        high: false,
    };
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), r.ask(message()))
            .await
            .unwrap(),
        Err(ActorError::TimeoutDetail(_))
    ));
    assert_eq!(
        r.ask_unbounded(message()).await.unwrap(),
        7,
        "explicit unbounded wait overrides message timeout"
    );
    assert_eq!(
        r.ask_with_timeout(message(), Duration::from_secs(2))
            .await
            .unwrap(),
        7
    );
    assert!(
        r.tell(StaticRequest {
            high: true,
            timeout: None
        })
        .await
        .unwrap_err()
        .to_string()
        .contains("priority")
    );
    let dynamic = r.clone().into_dyn();
    assert!(matches!(
        dynamic.ask(message()).await,
        Err(ActorError::TimeoutDetail(_))
    ));
    // Raw boxed send keeps its existing unbounded contract.
    assert_eq!(
        *dynamic
            .send(Box::new(message()))
            .await
            .unwrap()
            .downcast::<u32>()
            .unwrap(),
        7
    );
    r.stop().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_thread_channel_and_dynamic_bridge_resolve_timeouts_consistently() {
    let sys = ThreadActorSystem::shared(Default::default());
    static_contract(
        sys.spawn_typed::<StaticProbe, StaticRequest>(StaticProbe, "/static")
            .await
            .unwrap(),
    )
    .await;
    sys.shutdown_internal().await.unwrap();
}
#[actix::test]
async fn static_actix_channel_and_dynamic_bridge_resolve_timeouts_consistently() {
    let sys = Arc::new(
        ActixActorSystem::with_settings(Default::default())
            .await
            .unwrap(),
    );
    static_contract(
        sys.spawn_typed::<StaticProbe, StaticRequest>(StaticProbe, "/static")
            .await
            .unwrap(),
    )
    .await;
    sys.as_ref().clone().shutdown().await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn static_tell_timeout_cancels_capacity_wait() {
    let (tx, rx) = parrot_api::typed_channel::typed_channel::<StaticProbe>(1);
    let r = parrot_api::typed_channel::TypedActorRef::<StaticProbe, StaticRequest>::new(
        tx,
        "/raw".into(),
    );
    r.tell(StaticRequest {
        timeout: None,
        high: false,
    })
    .await
    .unwrap();
    let started = tokio::time::Instant::now();
    assert!(matches!(
        r.tell(StaticRequest {
            timeout: Some(Duration::from_millis(10)),
            high: false
        })
        .await,
        Err(ActorError::Timeout)
    ));
    assert_eq!(started.elapsed(), Duration::from_millis(10));
    assert_eq!(rx.len(), 1);
    rx.recv().unwrap();
    assert!(rx.try_recv().is_err());
}
