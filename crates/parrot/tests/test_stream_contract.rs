//! Real engine registration and ordered actor stream hooks, not a memory registry mock.
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::ActorContext,
    stream::StreamRegistryExt,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::time::Duration;
use tokio::sync::mpsc;

struct Register;
struct ThreadProbe(mpsc::UnboundedSender<String>);
struct ActixProbe(mpsc::UnboundedSender<String>);
impl ActixEngineExt for ActixProbe {}
macro_rules! probe {
    ($actor:ident, $context:ty) => {
        impl Actor for $actor {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    if msg.is::<Register>() {
                        ctx.stream_registry()
                            .add_stream(futures::stream::iter([1_u32, 2, 3]))?;
                    } else {
                        self.0
                            .send(format!("item:{}", *msg.downcast::<u32>().unwrap()))
                            .unwrap();
                    }
                    Ok(Box::new(()) as BoxedMessage)
                })
            }
            fn stream_started<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    self.0.send("started".into()).unwrap();
                    Ok(())
                })
            }
            fn stream_finished<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    self.0.send("finished".into()).unwrap();
                    Ok(())
                })
            }
        }
    };
}
probe!(ThreadProbe, parrot::thread::ThreadContext<Self>);
probe!(
    ActixProbe,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);
async fn ordered_events(mut events: mpsc::UnboundedReceiver<String>) {
    for expected in ["started", "item:1", "item:2", "item:3", "finished"] {
        let actual = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(actual, expected);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_stream_calls_real_actor_hooks() {
    let sys = parrot::thread::ThreadActorSystem::shared(Default::default());
    let (send, events) = mpsc::unbounded_channel();
    let actor = sys
        .spawn_at(ThreadProbe(send), "/stream", None, Default::default())
        .await
        .unwrap();
    actor
        .send(Box::new(Register))
        .await
        .expect("register a stream on a live Thread actor");
    ordered_events(events).await;
    sys.shutdown_internal().await.unwrap();
}
#[actix::test]
async fn actix_stream_calls_real_actor_hooks() {
    let sys = parrot::actix::ActixActorSystem::new().await.unwrap();
    let (send, events) = mpsc::unbounded_channel();
    let actor = sys
        .spawn_root_typed(ActixProbe(send), EmptyConfig)
        .await
        .unwrap();
    actor
        .send(Box::new(Register))
        .await
        .expect("register a stream on a live Actix actor");
    ordered_events(events).await;
    sys.shutdown().await.unwrap();
}

use futures::Stream;
use parrot_api::{errors::ActorError, types::BoxedActorRef};
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::sync::Notify;

#[derive(Clone, Copy)]
enum Source {
    Finite,
    Pending,
    Panic,
}
struct TrackedSource {
    kind: Source,
    items: VecDeque<u32>,
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    drop_gate: Option<Arc<DropGate>>,
    panic_drop: bool,
}
impl Stream for TrackedSource {
    type Item = u32;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<u32>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        match self.kind {
            Source::Finite => Poll::Ready(self.items.pop_front()),
            Source::Pending => Poll::Pending,
            Source::Panic => panic!("stream poll failure"),
        }
    }
}
impl Drop for TrackedSource {
    fn drop(&mut self) {
        if let Some(gate) = &self.drop_gate {
            gate.wait();
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "stream destructor failure");
    }
}
#[derive(Clone)]
struct Data {
    events: mpsc::UnboundedSender<String>,
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    handler_drops: Arc<AtomicUsize>,
    panic_handler_drop: bool,
    panic_start: bool,
    callback_drop_failure: bool,
    callback_drops: Arc<AtomicUsize>,
    return_panic_error: bool,
    stop_on_failure: bool,
    drop_gate: Option<Arc<DropGate>>,
    panic_drop: bool,
    default_calls: Arc<AtomicUsize>,
    contexts: Arc<Mutex<Vec<(String, std::thread::ThreadId)>>>,
    start_gate: Option<Arc<Notify>>,
    item_gate: Option<Arc<Notify>>,
    init_gate: Option<Arc<Notify>>,
    init_stream: bool,
    fail_init: bool,
    fail_item: bool,
    recover_error: bool,
}
impl Data {
    fn new() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (events, recv) = mpsc::unbounded_channel();
        (
            Self {
                events,
                polls: Arc::default(),
                drops: Arc::default(),
                handler_drops: Arc::default(),
                panic_handler_drop: false,
                panic_start: false,
                callback_drop_failure: false,
                callback_drops: Arc::default(),
                return_panic_error: false,
                stop_on_failure: false,
                drop_gate: None,
                panic_drop: false,
                default_calls: Arc::default(),
                contexts: Arc::default(),
                start_gate: None,
                item_gate: None,
                init_gate: None,
                init_stream: false,
                fail_init: false,
                fail_item: false,
                recover_error: false,
            },
            recv,
        )
    }
    fn stream(&self, kind: Source) -> TrackedSource {
        TrackedSource {
            kind,
            items: [1, 2, 3].into(),
            polls: self.polls.clone(),
            drops: self.drops.clone(),
            drop_gate: self.drop_gate.clone(),
            panic_drop: self.panic_drop,
        }
    }
    fn record(&self, event: impl Into<String>, ctx: &dyn ActorContext) {
        self.contexts
            .lock()
            .unwrap()
            .push((ctx.path().path.clone(), std::thread::current().id()));
        let _ = self.events.send(event.into());
    }
}
// The callback allocation has its own Drop, independent of its poll result.
// An async-local guard would panic during the Ready poll, testing supervision
// rather than future-retirement receipt plumbing.
struct CallbackFutureProbe<'a> {
    future: BoxedFuture<'a, ()>,
    drops: Arc<AtomicUsize>,
    panic_drop: bool,
}
impl std::future::Future for CallbackFutureProbe<'_> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.future.as_mut().poll(cx)
    }
}
impl Drop for CallbackFutureProbe<'_> {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "real callback future destructor failure");
    }
}
struct Custom<C: ?Sized>(Data, std::marker::PhantomData<fn(&C)>);
impl<C: ?Sized> Drop for Custom<C> {
    fn drop(&mut self) {
        self.0.handler_drops.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.0.panic_handler_drop,
            "custom handler destructor failure"
        );
    }
}
#[async_trait::async_trait]
impl<C: ActorContext + ?Sized> parrot_api::stream::StreamHandler<TrackedSource, C> for Custom<C> {
    fn started<'life0, 'life1, 'async_trait>(
        &'life0 mut self,
        ctx: &'life1 mut C,
    ) -> BoxedFuture<'async_trait, ()>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        let drops = self.0.callback_drops.clone();
        let panic_drop = self.0.callback_drop_failure;
        Box::pin(CallbackFutureProbe {
            future: Box::pin(async move {
                self.0
                    .contexts
                    .lock()
                    .unwrap()
                    .push((ctx.path().path.clone(), std::thread::current().id()));
                let _ = self.0.events.send("custom:start".into());
                assert!(!self.0.panic_start, "actual stream callback panic");
                if let Some(gate) = &self.0.start_gate {
                    gate.notified().await;
                }
            }),
            drops,
            panic_drop,
        })
    }
    async fn handle(&mut self, item: u32, ctx: &mut C) {
        self.0
            .contexts
            .lock()
            .unwrap()
            .push((ctx.path().path.clone(), std::thread::current().id()));
        let _ = self.0.events.send(format!("custom:{item}"));
        if let Some(gate) = &self.0.item_gate {
            gate.notified().await;
        }
    }
    async fn finished(&mut self, ctx: &mut C) {
        self.0
            .contexts
            .lock()
            .unwrap()
            .push((ctx.path().path.clone(), std::thread::current().id()));
        let _ = self.0.events.send("custom:finish".into());
    }
}
struct WrongContext;
#[async_trait::async_trait]
impl parrot_api::stream::StreamHandler<TrackedSource, ()> for WrongContext {
    async fn handle(&mut self, _: u32, _: &mut ()) {
        panic!("wrong context must never be called")
    }
}
enum Work {
    Register(Source),
    Custom(Source, bool),
    WrongContext,
    InvalidErased,
    HoldAndRegister(
        tokio::sync::oneshot::Sender<()>,
        Arc<Notify>,
        tokio::sync::oneshot::Sender<()>,
    ),
    Timer,
    Ping,
    Crash,
}
struct AdvancedThread(Data);
struct AdvancedActix(Data);
impl ActixEngineExt for AdvancedActix {}
macro_rules! advanced {
    ($actor:ident, $context:ty) => {
        impl Actor for $actor {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn can_batch_message(&self, message: &BoxedMessage) -> bool {
                !message.is::<Work>()
            }
            fn receive_batch<'a>(
                &'a mut self,
                _: Vec<BoxedMessage>,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
                Box::pin(async {
                    panic!("system stream callbacks must not enter business batches")
                })
            }
            fn init<'a>(
                &'a mut self,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    if self.0.init_stream {
                        ctx.stream_registry()
                            .add_stream(self.0.stream(Source::Pending))?;
                        self.0.record("init", ctx);
                        if let Some(gate) = &self.0.init_gate {
                            gate.notified().await;
                        }
                        if self.0.fail_init {
                            return Err(ActorError::InitializationError(
                                "stream init failed".into(),
                            ));
                        }
                    }
                    Ok(())
                })
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    match *msg
                        .downcast::<Work>()
                        .expect("stream items must use stream dispatch")
                    {
                        Work::Register(kind) => {
                            ctx.stream_registry().add_stream(self.0.stream(kind))?
                        }
                        Work::Custom(kind, dynamic) => {
                            if dynamic {
                                ctx.stream_registry()
                                    .add_stream_with_handler::<_, _, dyn ActorContext>(
                                        self.0.stream(kind),
                                        Custom::<dyn ActorContext>(
                                            self.0.clone(),
                                            std::marker::PhantomData,
                                        ),
                                    )?;
                            } else {
                                ctx.stream_registry()
                                    .add_stream_with_handler::<_, _, Self::Context>(
                                        self.0.stream(kind),
                                        Custom::<Self::Context>(
                                            self.0.clone(),
                                            std::marker::PhantomData,
                                        ),
                                    )?;
                            }
                        }
                        Work::WrongContext => {
                            ctx.stream_registry().add_stream_with_handler::<_, _, ()>(
                                self.0.stream(Source::Finite),
                                WrongContext,
                            )?
                        }
                        Work::InvalidErased => {
                            ctx.stream_registry().add_stream_with_handler_erased(
                                Box::new(futures::stream::empty()),
                                Box::new(17_u32),
                            )?
                        }
                        Work::HoldAndRegister(entered, release, registered) => {
                            let _ = entered.send(());
                            release.notified().await;
                            ctx.stream_registry()
                                .add_stream_with_handler::<_, _, dyn ActorContext>(
                                    self.0.stream(Source::Pending),
                                    Custom::<dyn ActorContext>(
                                        self.0.clone(),
                                        std::marker::PhantomData,
                                    ),
                                )?;
                            let _ = registered.send(());
                            release.notified().await;
                        }
                        Work::Timer => {
                            ctx.schedule_once(
                                ctx.get_self_ref(),
                                Box::new(Work::Ping),
                                Duration::from_secs(600),
                            )
                            .await?
                        }
                        Work::Ping => {}
                        Work::Crash => panic!("stream owner restart"),
                    }
                    Ok(Box::new(()) as BoxedMessage)
                })
            }
            fn stream_started<'a>(
                &'a mut self,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    self.0.default_calls.fetch_add(1, Ordering::SeqCst);
                    self.0.record("start", ctx);
                    assert!(!self.0.panic_start, "actual stream callback panic");
                    if let Some(gate) = &self.0.start_gate {
                        gate.notified().await;
                    }
                    Ok(())
                })
            }
            fn handle_stream<'a>(
                &'a mut self,
                item: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    self.0.default_calls.fetch_add(1, Ordering::SeqCst);
                    let item = *item.downcast::<u32>().unwrap();
                    self.0.record(format!("item:{item}"), ctx);
                    if let Some(gate) = &self.0.item_gate {
                        gate.notified().await;
                    }
                    if self.0.return_panic_error {
                        return Err(ActorError::Panic("returned panic value".into()));
                    }
                    if self.0.fail_item {
                        return Err(ActorError::MessageHandlingError(
                            "stream item failed".into(),
                        ));
                    }
                    Ok(Box::new(()) as BoxedMessage)
                })
            }
            fn stream_finished<'a>(
                &'a mut self,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    self.0.record("finish", ctx);
                    Ok(())
                })
            }
            fn stream_error<'a>(
                &'a mut self,
                error: ActorError,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    self.0.record(format!("error:{error}"), ctx);
                    if self.0.recover_error {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
            }
        }
    };
}
advanced!(AdvancedThread, parrot::thread::ThreadContext<Self>);
advanced!(
    AdvancedActix,
    parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>
);

enum Backend {
    Thread(Arc<parrot::thread::ThreadActorSystem>),
    Actix(parrot::actix::ActixActorSystem),
}
impl Backend {
    async fn new(index: usize, limit: usize) -> Self {
        if index == 0 {
            Self::Actix(
                parrot::actix::ActixActorSystem::with_settings(
                    parrot::actix::system::ActixSystemSettings {
                        max_context_tasks: limit,
                        arbiter_count: 2,
                        mailbox_capacity: 2,
                        ask_timeout: Duration::from_millis(100),
                        shutdown_timeout: Duration::from_millis(100),
                    },
                )
                .await
                .unwrap(),
            )
        } else {
            let mode = match index {
                1 => parrot::thread::SchedulingMode::SharedPool {
                    max_messages_per_run: 8,
                },
                2 => parrot::thread::SchedulingMode::DedicatedThread,
                _ => parrot::thread::SchedulingMode::Sharded {
                    affinity_key: "stream".into(),
                },
            };
            Self::Thread(parrot::thread::ThreadActorSystem::shared(
                parrot::thread::ThreadActorSystemConfig {
                    max_context_tasks: limit,
                    default_scheduling_mode: mode,
                    shared_pool_size: 2,
                    default_mailbox_capacity: 2,
                    default_ask_timeout: Duration::from_millis(100),
                    shutdown_timeout: Duration::from_millis(100),
                    ..Default::default()
                },
            ))
        }
    }
    async fn actor(&self, data: Data) -> BoxedActorRef {
        let stop_on_failure = data.stop_on_failure;
        match self {
            Self::Thread(sys) => sys
                .spawn_at(
                    AdvancedThread(data),
                    "/stream",
                    None,
                    parrot::thread::ThreadActorConfig {
                        supervisor_strategy: Some(if stop_on_failure {
                            parrot::thread::SupervisorStrategy::Stop
                        } else {
                            parrot::thread::SupervisorStrategy::Resume
                        }),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .clone_boxed(),
            Self::Actix(sys) => sys
                .spawn_root_typed_with_strategy(
                    AdvancedActix(data),
                    EmptyConfig,
                    parrot_api::supervisor::SupervisorStrategyType::Default(if stop_on_failure {
                        parrot_api::supervisor::DefaultStrategy::StopOnFailure
                    } else {
                        parrot_api::supervisor::DefaultStrategy::ResumeOnFailure
                    }),
                )
                .await
                .unwrap(),
        }
    }
    async fn shutdown(self) {
        match self {
            Self::Thread(sys) => {
                sys.shutdown_internal().await.unwrap();
            }
            Self::Actix(sys) => {
                sys.shutdown().await.unwrap();
            }
        }
    }
}
async fn next(events: &mut mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn count(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while counter.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), expected);
}
async fn work(actor: &dyn ActorRef, msg: Work) -> ActorResult<BoxedMessage> {
    actor.send(Box::new(msg)).await
}

#[actix::test]
async fn custom_handlers_use_actual_typed_or_dynamic_context_in_every_backend() {
    for mode in 0..4 {
        for dynamic in [false, true] {
            let backend = Backend::new(mode, 1).await;
            let (data, mut events) = Data::new();
            let actor = backend.actor(data.clone()).await;
            work(&*actor, Work::Custom(Source::Finite, dynamic))
                .await
                .unwrap();
            for expected in [
                "custom:start",
                "custom:1",
                "custom:2",
                "custom:3",
                "custom:finish",
            ] {
                assert_eq!(next(&mut events).await, expected);
            }
            count(&data.drops, 1).await;
            count(&data.handler_drops, 1).await;
            assert_eq!(data.default_calls.load(Ordering::SeqCst), 0);
            let contexts = data.contexts.lock().unwrap().clone();
            assert_eq!(contexts.len(), 5);
            assert!(contexts.iter().all(|(path, _)| *path == actor.path()));
            if mode != 1 {
                assert!(contexts.iter().all(|(_, thread)| *thread == contexts[0].1));
            }
            actor.stop().await.unwrap();
            backend.shutdown().await;
        }
    }
}

#[actix::test]
async fn started_and_item_callbacks_apply_processing_backpressure_and_serialize_business() {
    for mode in 0..4 {
        let backend = Backend::new(mode, 1).await;
        let (mut data, mut events) = Data::new();
        let start = Arc::new(Notify::new());
        let item = Arc::new(Notify::new());
        data.start_gate = Some(start.clone());
        data.item_gate = Some(item.clone());
        let actor = backend.actor(data.clone()).await;
        work(&*actor, Work::Register(Source::Finite)).await.unwrap();
        assert_eq!(next(&mut events).await, "start");
        assert!(
            tokio::time::timeout(Duration::from_millis(140), events.recv())
                .await
                .is_err()
        );
        assert_eq!(data.polls.load(Ordering::SeqCst), 0);
        // Processing takes longer than the system ask timeout; the stream must survive.
        start.notify_one();
        for n in 1..=3 {
            assert_eq!(next(&mut events).await, format!("item:{n}"));
            assert!(
                tokio::time::timeout(Duration::from_millis(25), work(&*actor, Work::Ping))
                    .await
                    .is_err(),
                "stream callback must own serial actor dispatch"
            );
            assert_eq!(data.polls.load(Ordering::SeqCst), n);
            item.notify_one();
        }
        assert_eq!(next(&mut events).await, "finish");
        count(&data.drops, 1).await;
        backend.shutdown().await;
    }
}

#[actix::test]
async fn registration_rejects_wrong_context_and_raw_handler_before_poll_or_quota_use() {
    for mode in 0..4 {
        let backend = Backend::new(mode, 1).await;
        let (data, mut events) = Data::new();
        let actor = backend.actor(data.clone()).await;
        let wrong = work(&*actor, Work::WrongContext).await.unwrap_err();
        assert!(wrong.to_string().contains("context type mismatch"));
        assert_eq!(data.polls.load(Ordering::SeqCst), 0);
        assert_eq!(data.drops.load(Ordering::SeqCst), 1);
        let raw = work(&*actor, Work::InvalidErased).await.unwrap_err();
        assert!(raw.to_string().contains("StreamHandlerRegistration"));
        work(&*actor, Work::Register(Source::Finite)).await.unwrap();
        for expected in ["start", "item:1", "item:2", "item:3", "finish"] {
            assert_eq!(next(&mut events).await, expected);
        }
        actor.stop().await.unwrap();
        backend.shutdown().await;
    }
}

#[actix::test]
async fn initialization_gates_stream_polling_and_failure_disposes_unstarted_stream() {
    for mode in 0..4 {
        for fail in [false, true] {
            let backend = Backend::new(mode, 1).await;
            let (mut data, mut events) = Data::new();
            let gate = Arc::new(Notify::new());
            data.init_gate = Some(gate.clone());
            data.init_stream = true;
            data.fail_init = fail;
            let actor = backend.actor(data.clone()).await;
            assert_eq!(next(&mut events).await, "init");
            assert!(
                tokio::time::timeout(Duration::from_millis(40), events.recv())
                    .await
                    .is_err()
            );
            assert_eq!(data.polls.load(Ordering::SeqCst), 0);
            gate.notify_one();
            if fail {
                count(&data.drops, 1).await;
                assert_eq!(data.polls.load(Ordering::SeqCst), 0);
                assert_eq!(data.default_calls.load(Ordering::SeqCst), 0);
            } else {
                assert_eq!(next(&mut events).await, "start");
                count(&data.polls, 1).await;
                actor.stop().await.unwrap();
                count(&data.drops, 1).await;
            }
            // Failed initialization remains an error; dispose the owning system explicitly.
            match backend {
                Backend::Thread(sys) => {
                    let _ = sys.shutdown_internal().await;
                }
                Backend::Actix(sys) => {
                    let _ = sys.shutdown().await;
                }
            }
        }
    }
}

#[actix::test]
async fn stop_joins_pending_stream_and_custom_handler_disposal() {
    for mode in 0..4 {
        let backend = Backend::new(mode, 1).await;
        let (data, mut events) = Data::new();
        let actor = backend.actor(data.clone()).await;
        work(&*actor, Work::Custom(Source::Pending, true))
            .await
            .unwrap();
        assert_eq!(next(&mut events).await, "custom:start");
        count(&data.polls, 1).await;
        actor.stop().await.unwrap();
        assert_eq!(data.drops.load(Ordering::SeqCst), 1);
        assert_eq!(data.handler_drops.load(Ordering::SeqCst), 1);
        assert!(
            events.try_recv().is_err(),
            "cancelled streams must not emit successful completion"
        );
        backend.shutdown().await;
    }
}

#[actix::test]
async fn streams_share_bounded_user_task_quota_with_timers_and_release_on_completion() {
    for mode in 0..4 {
        let disabled = Backend::new(mode, 0).await;
        let (data, _) = Data::new();
        let actor = disabled.actor(data.clone()).await;
        assert!(
            work(&*actor, Work::Register(Source::Pending))
                .await
                .unwrap_err()
                .to_string()
                .contains("task limit")
        );
        assert_eq!(data.polls.load(Ordering::SeqCst), 0);
        assert_eq!(data.drops.load(Ordering::SeqCst), 1);
        disabled.shutdown().await;
        let backend = Backend::new(mode, 1).await;
        let (data, mut events) = Data::new();
        let actor = backend.actor(data.clone()).await;
        work(&*actor, Work::Register(Source::Finite)).await.unwrap();
        for expected in ["start", "item:1", "item:2", "item:3", "finish"] {
            assert_eq!(next(&mut events).await, expected);
        }
        count(&data.drops, 1).await;
        // Worker destruction precedes release of its task ticket; retry admission until released.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match work(&*actor, Work::Timer).await {
                    Ok(_) => break,
                    Err(error) if error.to_string().contains("task limit") => {
                        tokio::task::yield_now().await
                    }
                    Err(error) => panic!("unexpected timer error: {error}"),
                }
            }
        })
        .await
        .unwrap();
        assert!(
            work(&*actor, Work::Register(Source::Pending))
                .await
                .unwrap_err()
                .to_string()
                .contains("task limit")
        );
        actor.stop().await.unwrap();
        backend.shutdown().await;
    }
}

#[actix::test]
async fn stream_errors_recover_or_terminate_without_false_completion_or_actor_death() {
    for mode in 0..4 {
        for recover in [false, true] {
            let backend = Backend::new(mode, 1).await;
            let (mut data, mut events) = Data::new();
            data.fail_item = true;
            data.recover_error = recover;
            let actor = backend.actor(data.clone()).await;
            work(&*actor, Work::Register(Source::Finite)).await.unwrap();
            assert_eq!(next(&mut events).await, "start");
            for n in 1..=if recover { 3 } else { 1 } {
                assert_eq!(next(&mut events).await, format!("item:{n}"));
                assert!(next(&mut events).await.contains("stream item failed"));
            }
            if recover {
                assert_eq!(next(&mut events).await, "finish");
            }
            count(&data.drops, 1).await;
            work(&*actor, Work::Ping).await.unwrap();
            assert!(events.try_recv().is_err());
            backend.shutdown().await;
        }
        let backend = Backend::new(mode, 1).await;
        let (data, mut events) = Data::new();
        let actor = backend.actor(data.clone()).await;
        work(&*actor, Work::Register(Source::Panic)).await.unwrap();
        assert_eq!(next(&mut events).await, "start");
        assert!(next(&mut events).await.contains("stream poll failure"));
        count(&data.drops, 1).await;
        work(&*actor, Work::Ping).await.unwrap();
        assert!(events.try_recv().is_err());
        backend.shutdown().await;
    }
}

#[actix::test]
async fn supervision_disposes_old_stream_before_constructing_replacement_in_every_thread_mode() {
    for mode in 1..4 {
        let Backend::Thread(sys) = Backend::new(mode, 1).await else {
            unreachable!()
        };
        let (mut data, mut events) = Data::new();
        data.init_stream = true;
        let builds = Arc::new(AtomicUsize::new(0));
        let state = data.clone();
        let build_counter = builds.clone();
        let actor = sys
            .spawn_supervised_with_config(
                move || {
                    let generation = build_counter.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        state.drops.load(Ordering::SeqCst),
                        generation,
                        "old source must be disposed before replacement"
                    );
                    AdvancedThread(state.clone())
                },
                "/stream",
                parrot::thread::ThreadActorConfig {
                    supervisor_strategy: Some(parrot::thread::SupervisorStrategy::Restart {
                        max_retries: 1,
                        within: Duration::from_secs(5),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(next(&mut events).await, "init");
        assert_eq!(next(&mut events).await, "start");
        count(&data.polls, 1).await;
        assert!(work(&actor, Work::Crash).await.is_err());
        assert_eq!(next(&mut events).await, "init");
        assert_eq!(next(&mut events).await, "start");
        count(&data.polls, 2).await;
        work(&actor, Work::Ping).await.unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        actor.stop().await.unwrap();
        assert_eq!(data.drops.load(Ordering::SeqCst), 2);
        assert!(events.try_recv().is_err());
        sys.shutdown_internal().await.unwrap();
    }
}

#[derive(Debug)]
struct BusinessOnly;
impl parrot::thread::mailbox::admission::MessageAdmission for BusinessOnly {
    fn prepare(&self, msg: BoxedMessage) -> Result<BoxedMessage, parrot::thread::MailboxError> {
        if msg.is::<Work>() {
            Ok(msg)
        } else {
            Err(parrot::thread::MailboxError::PushError(
                "business only".into(),
            ))
        }
    }
}
#[actix::test]
async fn internal_stream_callbacks_bypass_business_admission_and_batching() {
    for mode in 1..4 {
        let Backend::Thread(sys) = Backend::new(mode, 1).await else {
            unreachable!()
        };
        let (data, mut events) = Data::new();
        let actor = sys
            .spawn_at(
                AdvancedThread(data),
                "/stream",
                None,
                parrot::thread::ThreadActorConfig {
                    message_admission: Some(Arc::new(BusinessOnly)),
                    message_batch_size: Some(8),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        work(&actor, Work::Register(Source::Finite)).await.unwrap();
        for expected in ["start", "item:1", "item:2", "item:3", "finish"] {
            assert_eq!(next(&mut events).await, expected);
        }
        actor.stop().await.unwrap();
        sys.shutdown_internal().await.unwrap();
    }
}

struct SynchronousActix(Data);
impl Actor for SynchronousActix {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            ctx.stream_registry()
                .add_stream(self.0.stream(Source::Finite))
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { panic!("normal messages require synchronous engine dispatch") })
    }
    fn handle_stream<'a>(
        &'a mut self,
        item: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.0
                .record(format!("item:{}", *item.downcast::<u32>().unwrap()), ctx);
            Ok(Box::new(()) as BoxedMessage)
        })
    }
    fn stream_finished<'a>(
        &'a mut self,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.record("finish", ctx);
            Ok(())
        })
    }
}
impl ActixEngineExt for SynchronousActix {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
        _: parrot_api::actor::EngineContextHandle<'_>,
    ) -> Option<ActorResult<BoxedMessage>> {
        assert!(matches!(msg.downcast_ref::<Work>(), Some(Work::Ping)));
        Some(Ok(Box::new(())))
    }
}
#[actix::test]
async fn synchronous_actix_business_handlers_still_receive_async_stream_hooks() {
    let sys = parrot::actix::ActixActorSystem::new().await.unwrap();
    let (data, mut events) = Data::new();
    let actor = sys
        .spawn_root_typed_with_strategy(
            SynchronousActix(data),
            EmptyConfig,
            parrot_api::supervisor::SupervisorStrategyType::Default(
                parrot_api::supervisor::DefaultStrategy::ResumeOnFailure,
            ),
        )
        .await
        .unwrap();
    for expected in ["item:1", "item:2", "item:3", "finish"] {
        assert_eq!(next(&mut events).await, expected);
    }
    work(&*actor, Work::Ping).await.unwrap();
    actor.stop().await.unwrap();
    sys.shutdown().await.unwrap();
}

#[derive(Default)]
struct DropGate {
    release: (Mutex<bool>, std::sync::Condvar),
    entered: Notify,
}
impl DropGate {
    fn wait(&self) {
        self.entered.notify_one();
        let mut released = self.release.0.lock().unwrap();
        while !*released {
            released = self.release.1.wait(released).unwrap();
        }
    }
    fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
}
struct ReleaseOnDrop(Arc<DropGate>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}
impl Backend {
    fn actor_count(&self) -> usize {
        match self {
            Self::Thread(sys) => sys.actor_count(),
            Self::Actix(sys) => sys.status().active_actors,
        }
    }
}
#[actix::test]
async fn slow_stream_disposal_retains_registration_past_stop_timeout() {
    for mode in 0..4 {
        let backend = Backend::new(mode, 1).await;
        let (mut data, mut events) = Data::new();
        let gate = Arc::new(DropGate::default());
        let release = ReleaseOnDrop(gate.clone());
        data.drop_gate = Some(gate.clone());
        let actor = backend.actor(data.clone()).await;
        work(&*actor, Work::Register(Source::Pending))
            .await
            .unwrap();
        assert_eq!(next(&mut events).await, "start");
        count(&data.polls, 1).await;
        let stop_ref = actor.clone_boxed();
        let stop = tokio::spawn(async move { stop_ref.stop().await });
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), stop)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                result,
                Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
            ),
            "slow disposal cannot acknowledge successful stop: {result:?}"
        );
        assert_eq!(
            backend.actor_count(),
            1,
            "live disposal retains the registration"
        );
        assert_eq!(data.drops.load(Ordering::SeqCst), 0);
        drop(release);
        count(&data.drops, 1).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while backend.actor_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Original stop outcome remains timeout even after background cleanup.
        assert!(actor.stop().await.is_err());
        match backend {
            Backend::Thread(sys) => {
                let _ = sys.shutdown_internal().await;
            }
            Backend::Actix(sys) => {
                let _ = sys.shutdown().await;
            }
        }
    }
}

#[actix::test]
async fn stop_cancels_stream_waiting_for_a_full_mailbox_without_calling_hooks() {
    for mode in 0..4 {
        let backend = Backend::new(mode, 1).await;
        let (data, mut events) = Data::new();
        let actor = backend.actor(data.clone()).await;
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (registered, registration) = tokio::sync::oneshot::channel();
        let release = Arc::new(Notify::new());
        actor
            .deliver(Box::new(Work::HoldAndRegister(
                entered,
                release.clone(),
                registered,
            )))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        // Fill both slots while the registration handler owns serial execution.
        actor.deliver(Box::new(Work::Ping)).await.unwrap();
        actor.deliver(Box::new(Work::Ping)).await.unwrap();
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), registration)
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(40), events.recv())
                .await
                .is_err()
        );
        assert_eq!(data.polls.load(Ordering::SeqCst), 0);
        let stop_ref = actor.clone_boxed();
        let stopping = tokio::spawn(async move { stop_ref.stop().await });
        count(&data.drops, 1).await;
        // The test's business handler cooperates after stream cancellation;
        // a permanently blocked handler would correctly time out on Thread.
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), stopping)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(data.drops.load(Ordering::SeqCst), 1);
        assert_eq!(data.handler_drops.load(Ordering::SeqCst), 1);
        assert_eq!(data.default_calls.load(Ordering::SeqCst), 0);
        assert!(events.try_recv().is_err());
        backend.shutdown().await;
    }
}

#[actix::test]
async fn system_drop_cancels_streams_despite_surviving_actor_references() {
    for mode in 0..4 {
        let backend = Backend::new(mode, 1).await;
        let (data, mut events) = Data::new();
        let actor = backend.actor(data.clone()).await;
        work(&*actor, Work::Custom(Source::Pending, true))
            .await
            .unwrap();
        assert_eq!(next(&mut events).await, "custom:start");
        count(&data.polls, 1).await;
        drop(backend);
        count(&data.drops, 1).await;
        count(&data.handler_drops, 1).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while actor.is_alive().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(events.try_recv().is_err());
    }
}
#[actix::test]
async fn stream_destructor_panic_is_isolated_and_reported_by_real_stop_receipt() {
    for mode in 0..4 {
        let backend = Backend::new(mode, 1).await;
        let (mut data, mut events) = Data::new();
        data.panic_drop = true;
        let actor = backend.actor(data.clone()).await;
        work(&*actor, Work::Custom(Source::Pending, true))
            .await
            .unwrap();
        assert_eq!(next(&mut events).await, "custom:start");
        count(&data.polls, 1).await;
        let result = actor.stop().await;
        assert!(
            matches!(result, Err(ActorError::Panic(_))),
            "destructor panic must reach stop: {result:?}"
        );
        assert_eq!(data.drops.load(Ordering::SeqCst), 1);
        assert_eq!(data.handler_drops.load(Ordering::SeqCst), 1);
        tokio::time::timeout(Duration::from_secs(2), async {
            while backend.actor_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        match backend {
            Backend::Thread(sys) => {
                let _ = sys.shutdown_internal().await;
            }
            Backend::Actix(sys) => {
                let _ = sys.shutdown().await;
            }
        }
    }
}

fn isolated_stream_cleanup(name: &str, callback_pending: bool, cooperative: bool) {
    isolated_cleanup_kind(name, callback_pending, cooperative, false);
}
fn isolated_cleanup_kind(name: &str, callback_pending: bool, cooperative: bool, future_only: bool) {
    const FLAG: &str = "PARROT_REAL_STREAM_CLEANUP_CASE";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        actix::System::new().block_on(async {
            for mode in 0..4 {
                let backend = Backend::new(mode, 1).await;
                let (mut data, mut events) = Data::new();
                data.panic_drop = !future_only;
                data.panic_handler_drop = !future_only;
                data.callback_drop_failure = future_only;
                if callback_pending {
                    data.start_gate = Some(Arc::new(Notify::new()));
                }
                let actor = backend.actor(data.clone()).await;
                work(&*actor, Work::Custom(Source::Pending, true))
                    .await
                    .unwrap();
                assert_eq!(next(&mut events).await, "custom:start");
                if callback_pending {
                    assert_eq!(data.polls.load(Ordering::SeqCst), 0);
                } else {
                    count(&data.polls, 1).await;
                }
                let stopping_ref = actor.clone_boxed();
                let stopping = tokio::spawn(async move { stopping_ref.stop().await });
                if callback_pending && cooperative {
                    // Wait for the stream worker's actual cancellation before
                    // releasing the actor callback; never assume a yield stopped it.
                    count(&data.drops, 1).await;
                    data.start_gate.as_ref().unwrap().notify_one();
                }
                let result = tokio::time::timeout(Duration::from_secs(3), stopping)
                    .await
                    .expect("stop must produce its result")
                    .unwrap();
                // The cancelled source worker closes its processing receipt,
                // so every backend releases a suspended callback cooperatively.
                // Report actual cleanup failure, never preserve a timeout that
                // only resulted from the previously missing cancellation edge.
                assert!(
                    matches!(result, Err(ActorError::Panic(_))),
                    "mode={mode}: {result:?}"
                );
                let first_error = result.as_ref().unwrap_err().to_string();
                assert_eq!(
                    data.drops.load(Ordering::SeqCst),
                    1,
                    "stop must follow stream destruction"
                );
                assert_eq!(
                    data.handler_drops.load(Ordering::SeqCst),
                    1,
                    "stop must follow transferred handler destruction"
                );
                tokio::time::timeout(Duration::from_secs(2), async {
                    while backend.actor_count() != 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                if future_only {
                    assert_eq!(data.callback_drops.load(Ordering::SeqCst), 1);
                    assert!(first_error.contains("real callback future destructor failure"));
                }
                assert_eq!(
                    actor.stop().await.unwrap_err().to_string(),
                    first_error,
                    "repeated stop must preserve the actual cleanup receipt"
                );
                match backend {
                    Backend::Thread(sys) => {
                        let _ = sys.shutdown_internal().await;
                    }
                    Backend::Actix(sys) => {
                        let _ = sys.shutdown().await;
                    }
                }
            }
        });
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(FLAG, name)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: {status}");
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{name}: deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn real_stop_retires_both_panicking_stream_and_handler() {
    isolated_stream_cleanup(
        "real_stop_retires_both_panicking_stream_and_handler",
        false,
        true,
    );
}
#[test]
fn real_stop_during_callback_retires_both_panicking_stream_and_handler() {
    isolated_stream_cleanup(
        "real_stop_during_callback_retires_both_panicking_stream_and_handler",
        true,
        true,
    );
}

#[test]
fn real_stop_cancels_pending_callback_and_retires_transferred_handler() {
    isolated_stream_cleanup(
        "real_stop_cancels_pending_callback_and_retires_transferred_handler",
        true,
        false,
    );
}

async fn assert_callback_panic_supervised(mode: usize, custom: bool) {
    let backend = Backend::new(mode, 1).await;
    let (mut data, mut events) = Data::new();
    data.panic_start = true;
    data.stop_on_failure = true;
    // Ordinary stream errors may recover; actual callback panics must still
    // reach actor supervision rather than this recovery hook.
    data.recover_error = true;
    let actor = backend.actor(data.clone()).await;
    work(
        &*actor,
        if custom {
            Work::Custom(Source::Pending, true)
        } else {
            Work::Register(Source::Pending)
        },
    )
    .await
    .unwrap();
    assert_eq!(
        next(&mut events).await,
        if custom { "custom:start" } else { "start" }
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while backend.actor_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual callback panic must trigger Stop supervision");
    assert!(!actor.is_alive().await);
    assert!(work(&*actor, Work::Ping).await.is_err());
    assert_eq!(data.polls.load(Ordering::SeqCst), 0);
    assert_eq!(data.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        data.handler_drops.load(Ordering::SeqCst),
        usize::from(custom)
    );
    assert!(
        events.try_recv().is_err(),
        "callback panic must not invoke stream error recovery"
    );
    match backend {
        Backend::Thread(sys) => {
            let _ = sys.shutdown_internal().await;
        }
        Backend::Actix(sys) => {
            let _ = sys.shutdown().await;
        }
    }
}

#[actix::test]
async fn callback_supervision_0_default() {
    assert_callback_panic_supervised(0, false).await;
}

#[actix::test]
async fn callback_supervision_0_custom() {
    assert_callback_panic_supervised(0, true).await;
}

#[actix::test]
async fn callback_supervision_1_default() {
    assert_callback_panic_supervised(1, false).await;
}

#[actix::test]
async fn callback_supervision_1_custom() {
    assert_callback_panic_supervised(1, true).await;
}

#[actix::test]
async fn callback_supervision_2_default() {
    assert_callback_panic_supervised(2, false).await;
}

#[actix::test]
async fn callback_supervision_2_custom() {
    assert_callback_panic_supervised(2, true).await;
}

#[actix::test]
async fn callback_supervision_3_default() {
    assert_callback_panic_supervised(3, false).await;
}

#[actix::test]
async fn callback_supervision_3_custom() {
    assert_callback_panic_supervised(3, true).await;
}

#[test]
fn real_callback_cleanup_cooperative_stop_reports_future_drop() {
    isolated_cleanup_kind(
        "real_callback_cleanup_cooperative_stop_reports_future_drop",
        true,
        true,
        true,
    );
}
#[test]
fn real_callback_cleanup_cancellation_reports_future_drop() {
    isolated_cleanup_kind(
        "real_callback_cleanup_cancellation_reports_future_drop",
        true,
        false,
        true,
    );
}
#[actix::test]
async fn returned_panic_error_remains_recoverable_without_supervision() {
    for mode in 0..4 {
        let backend = Backend::new(mode, 1).await;
        let (mut data, mut events) = Data::new();
        data.return_panic_error = true;
        data.recover_error = true;
        data.stop_on_failure = true;
        let actor = backend.actor(data.clone()).await;
        work(&*actor, Work::Register(Source::Finite)).await.unwrap();
        assert_eq!(next(&mut events).await, "start");
        for item in 1..=3 {
            assert_eq!(next(&mut events).await, format!("item:{item}"));
            let error = next(&mut events).await;
            assert!(error.starts_with("error:") && error.contains("returned panic value"));
        }
        assert_eq!(next(&mut events).await, "finish");
        count(&data.drops, 1).await;
        assert!(actor.is_alive().await);
        work(&*actor, Work::Ping).await.unwrap();
        actor.stop().await.unwrap();
        backend.shutdown().await;
    }
}
