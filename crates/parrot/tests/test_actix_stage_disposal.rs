//! Poll failure and future disposal are independent engine boundaries.
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext, ActixSystemSettings};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::{ActorContext, ActorSpawnerExt, ActorTerminated},
    death_watch::{DeathWatch, TerminationRecipient},
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    trace::{SpanKind, SpanStatus, TraceRecorder},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Init,
    Message,
    Child,
    Stop,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Exit {
    Success,
    Error,
    ConstructPanic,
    PollPanic,
    DropPanic,
    ErrorDropPanic,
    PollDropPanic,
    Pending,
    PendingDropPanic,
}
impl Exit {
    fn disposal_fails(self) -> bool {
        matches!(
            self,
            Self::DropPanic | Self::ErrorDropPanic | Self::PollDropPanic | Self::PendingDropPanic
        )
    }
    fn pending(self) -> bool {
        matches!(self, Self::Pending | Self::PendingDropPanic)
    }
    fn diagnostic(self) -> Option<&'static str> {
        if self.disposal_fails() {
            Some("secondary stage disposal panic")
        } else {
            self.execution_diagnostic()
        }
    }
    fn execution_diagnostic(self) -> Option<&'static str> {
        match self {
            Self::Error | Self::ErrorDropPanic => Some("primary stage error"),
            Self::ConstructPanic => Some("primary stage construction panic"),
            Self::PollPanic | Self::PollDropPanic => Some("primary stage poll panic"),
            Self::DropPanic => Some("secondary stage disposal panic"),
            _ => None,
        }
    }
}
#[derive(Default)]
struct Counts {
    made: AtomicUsize,
    constructed: AtomicUsize,
    polled: AtomicUsize,
    disposed: AtomicUsize,
    retired: AtomicUsize,
}
struct Observed(tokio::sync::mpsc::UnboundedSender<ActorTerminated>);
impl TerminationRecipient for Observed {
    fn deliver(&self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move { self.0.send(event).map_err(|_| ActorError::Stopped) })
    }
}
struct Probe {
    stage: Stage,
    exit: Exit,
    counts: Arc<Counts>,
}
struct StageFuture<T> {
    counts: Arc<Counts>,
    exit: Exit,
    value: Option<T>,
}
impl<T: Unpin> Future for StageFuture<T> {
    type Output = ActorResult<T>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.counts.polled.fetch_add(1, Ordering::SeqCst);
        match this.exit {
            Exit::Success | Exit::DropPanic => Poll::Ready(Ok(this.value.take().unwrap())),
            Exit::Error | Exit::ErrorDropPanic => {
                Poll::Ready(Err(ActorError::InternalError("primary stage error".into())))
            }
            Exit::PollPanic | Exit::PollDropPanic => panic!("primary stage poll panic"),
            Exit::Pending | Exit::PendingDropPanic => Poll::Pending,
            Exit::ConstructPanic => unreachable!(),
        }
    }
}
impl<T> Drop for StageFuture<T> {
    fn drop(&mut self) {
        self.counts.disposed.fetch_add(1, Ordering::SeqCst);
        // Retire any successful output before intentionally failing disposal.
        drop(self.value.take());
        assert!(
            !self.exit.disposal_fails(),
            "secondary stage disposal panic"
        );
    }
}
impl Probe {
    fn future<T: Send + Unpin + 'static>(&self, value: T) -> BoxedFuture<'static, ActorResult<T>> {
        self.counts.constructed.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.exit != Exit::ConstructPanic,
            "primary stage construction panic"
        );
        Box::pin(StageFuture {
            counts: self.counts.clone(),
            exit: self.exit,
            value: Some(value),
        })
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.retired.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActixEngineExt for Probe {}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        if self.stage == Stage::Init {
            self.future(())
        } else {
            Box::pin(async { Ok(()) })
        }
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        if self.stage == Stage::Message && message.is::<bool>() {
            self.future(message)
        } else if self.stage == Stage::Child && message.is::<bool>() {
            Box::pin(async move {
                let child = ctx
                    .spawner()
                    .spawn_factory(|| Probe {
                        stage: Stage::Stop,
                        exit: Exit::Success,
                        counts: Arc::new(Counts::default()),
                    })
                    .await?;
                child.stop().await?;
                Ok(message)
            })
        } else {
            Box::pin(async { Ok(message) })
        }
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        if self.stage == Stage::Stop {
            self.future(())
        } else {
            Box::pin(async { Ok(()) })
        }
    }
    fn handle_child_terminated<'a>(
        &'a mut self,
        _: BoxedActorRef,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        assert_eq!(self.stage, Stage::Child);
        self.future(())
    }
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("stage disposal deadline")
}
async fn run(stage: Stage, exit: Exit, factory: bool) {
    let counts = Arc::new(Counts::default());
    let recorder = TraceRecorder::new(8).unwrap();
    let root = recorder.root();
    let system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_millis(500),
        ..Default::default()
    })
    .await
    .unwrap();
    let strategy = OneForOneStrategy {
        max_restarts: 3,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(move |_| {
            if factory && exit.disposal_fails() {
                SupervisionDecision::Restart
            } else {
                SupervisionDecision::Resume
            }
        }),
    };
    let make = {
        let counts = counts.clone();
        move || {
            counts.made.fetch_add(1, Ordering::SeqCst);
            Probe {
                stage,
                exit,
                counts: counts.clone(),
            }
        }
    };
    let reference = if factory {
        bounded(system.spawn_root_factory_with_strategy(make, strategy))
            .await
            .unwrap()
    } else {
        bounded(system.spawn_root_typed_with_strategy(
            make(),
            EmptyConfig,
            SupervisorStrategyType::OneForOne(strategy),
        ))
        .await
        .unwrap()
    };
    let path = reference.path();
    if stage != Stage::Init || exit == Exit::Success {
        let reply = bounded(reference.send(Box::new(7_u32))).await.unwrap();
        assert_eq!(*reply.downcast::<u32>().unwrap(), 7);
    }
    let (events, mut terminated) = tokio::sync::mpsc::unbounded_channel();
    let watcher = DeathWatch::new(1);
    if stage == Stage::Stop && exit.pending() {
        watcher.set_recipient(Arc::new(Observed(events))).unwrap();
        reference.death_watch().unwrap().watch(&watcher).unwrap();
    }
    let mut cancelled_request = None;
    if stage == Stage::Message {
        let mut request = Box::pin(root.scope(async { reference.send(Box::new(true)).await }));
        if exit.pending() {
            assert!(futures::poll!(request.as_mut()).is_pending());
            cancelled_request = Some(request);
        } else {
            let result = bounded(request).await;
            if let Some(expected) = exit.execution_diagnostic() {
                let error = result.unwrap_err();
                assert!(error.to_string().contains(expected), "{error}");
            } else {
                assert!(*result.unwrap().downcast::<bool>().unwrap());
            }
        }
    }
    if stage == Stage::Child {
        assert!(
            *bounded(reference.send(Box::new(true)))
                .await
                .unwrap()
                .downcast::<bool>()
                .unwrap()
        );
        bounded(async {
            while counts.constructed.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    if exit.pending() && stage != Stage::Stop {
        bounded(async {
            while counts.polled.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    let resumed =
        matches!(stage, Stage::Message | Stage::Child) && !exit.pending() && !exit.disposal_fails();
    if resumed {
        let reply = bounded(reference.send(Box::new(8_u32))).await.unwrap();
        assert_eq!(*reply.downcast::<u32>().unwrap(), 8);
        assert_eq!(counts.retired.load(Ordering::SeqCst), 0);
    }
    // A physical disposal failure vetoes Resume and factory reconstruction.
    if !exit.pending()
        && (stage == Stage::Init && exit != Exit::Success
            || matches!(stage, Stage::Message | Stage::Child) && !resumed)
    {
        bounded(async {
            while system.resolve_actor(&path).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    let first_stop = bounded(reference.stop()).await;
    let timeout_published = matches!(
        first_stop,
        Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
    );
    if exit.pending() && stage == Stage::Stop {
        match first_stop {
            Err(ActorError::Timeout | ActorError::TimeoutDetail(_)) => {}
            Err(error) if exit.disposal_fails() => assert!(
                error.to_string().contains("secondary stage disposal panic"),
                "{error}"
            ),
            other => panic!("pending hook returned before timeout/disposal: {other:?}"),
        }
    }
    bounded(async {
        while system.resolve_actor(&path).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if let Some(request) = cancelled_request {
        assert!(bounded(request).await.is_err());
    }
    if stage == Stage::Stop && exit.pending() {
        let event = bounded(terminated.recv()).await.unwrap();
        assert_eq!(event.path, path);
        match event.reason {
            parrot_api::supervisor::DeathReason::Panic(message) if exit.disposal_fails() => {
                assert!(
                    message.contains("secondary stage disposal panic"),
                    "{message}"
                )
            }
            parrot_api::supervisor::DeathReason::Killed if !exit.disposal_fails() => {}
            other => panic!("lost physical cleanup outcome: {other:?}"),
        }
    }
    for _ in 0..2 {
        let result = bounded(reference.stop()).await;
        if timeout_published {
            assert!(matches!(
                result,
                Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
            ));
        } else if !resumed && let Some(expected) = exit.diagnostic() {
            let error = result.unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        } else {
            result.unwrap();
        }
    }
    assert_eq!(counts.made.load(Ordering::SeqCst), 1);
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 1);
    if exit == Exit::ConstructPanic {
        assert_eq!(counts.polled.load(Ordering::SeqCst), 0);
        assert_eq!(counts.disposed.load(Ordering::SeqCst), 0);
    } else {
        if exit.pending() {
            assert!(counts.polled.load(Ordering::SeqCst) >= 1);
        } else {
            assert_eq!(counts.polled.load(Ordering::SeqCst), 1);
        }
        assert_eq!(counts.disposed.load(Ordering::SeqCst), 1);
    }
    assert_eq!(counts.retired.load(Ordering::SeqCst), 1);
    bounded(system.shutdown()).await.unwrap();
    if stage == Stage::Message {
        recorder.close();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.records.len(), 1);
        let record = &snapshot.records[0];
        assert_eq!(record.trace_id, root.trace_id());
        assert_eq!(record.parent_span_id, Some(root.span_id()));
        assert_eq!(record.kind, SpanKind::DynamicHandler);
        assert_eq!(record.target, path);
        let expected = match exit {
            Exit::Success => SpanStatus::Ok,
            Exit::Error => SpanStatus::Error,
            Exit::Pending => SpanStatus::Cancelled,
            _ => SpanStatus::Panicked,
        };
        assert_eq!(
            record.status, expected,
            "incorrect physical handler outcome"
        );
    }
}
fn isolated(name: &str, stage: Stage, exit: Exit, factory: bool) {
    const FLAG: &str = "PARROT_ACTIX_STAGE_DISPOSAL";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        // Intentional double panics otherwise spend the subprocess budget in
        // std's forced symbolic backtrace, independently of engine behavior.
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let text = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str));
            if !matches!(
                text,
                Some(
                    "primary stage poll panic"
                        | "primary stage construction panic"
                        | "secondary stage disposal panic"
                )
            ) {
                original(info);
            }
        }));
        actix::System::new().block_on(run(stage, exit, factory));
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(FLAG, name)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{name}: {status}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{name}: subprocess deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! case {
    ($name:ident, $stage:ident, $exit:ident, $factory:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), Stage::$stage, Exit::$exit, $factory);
        }
    };
}
case!(instance_init_success, Init, Success, false);
case!(factory_init_success, Init, Success, true);
case!(instance_init_error, Init, Error, false);
case!(factory_init_error, Init, Error, true);
case!(
    instance_init_construction_panic,
    Init,
    ConstructPanic,
    false
);
case!(factory_init_construction_panic, Init, ConstructPanic, true);
case!(instance_init_poll_panic, Init, PollPanic, false);
case!(factory_init_poll_panic, Init, PollPanic, true);
case!(instance_init_drop_panic, Init, DropPanic, false);
case!(factory_init_drop_panic, Init, DropPanic, true);
case!(
    instance_init_error_and_drop_panic,
    Init,
    ErrorDropPanic,
    false
);
case!(
    factory_init_error_and_drop_panic,
    Init,
    ErrorDropPanic,
    true
);
case!(
    instance_init_poll_and_drop_panic,
    Init,
    PollDropPanic,
    false
);
case!(factory_init_poll_and_drop_panic, Init, PollDropPanic, true);
case!(instance_init_cancelled, Init, Pending, false);
case!(factory_init_cancelled, Init, Pending, true);
case!(
    instance_init_cancelled_drop_panic,
    Init,
    PendingDropPanic,
    false
);
case!(
    factory_init_cancelled_drop_panic,
    Init,
    PendingDropPanic,
    true
);
case!(instance_message_success, Message, Success, false);
case!(factory_message_success, Message, Success, true);
case!(instance_message_error, Message, Error, false);
case!(factory_message_error, Message, Error, true);
case!(
    instance_message_construction_panic,
    Message,
    ConstructPanic,
    false
);
case!(
    factory_message_construction_panic,
    Message,
    ConstructPanic,
    true
);
case!(instance_message_poll_panic, Message, PollPanic, false);
case!(factory_message_poll_panic, Message, PollPanic, true);
case!(instance_message_drop_panic, Message, DropPanic, false);
case!(factory_message_drop_panic, Message, DropPanic, true);
case!(
    instance_message_error_and_drop_panic,
    Message,
    ErrorDropPanic,
    false
);
case!(
    factory_message_error_and_drop_panic,
    Message,
    ErrorDropPanic,
    true
);
case!(
    instance_message_poll_and_drop_panic,
    Message,
    PollDropPanic,
    false
);
case!(
    factory_message_poll_and_drop_panic,
    Message,
    PollDropPanic,
    true
);
case!(instance_message_cancelled, Message, Pending, false);
case!(factory_message_cancelled, Message, Pending, true);
case!(
    instance_message_cancelled_drop_panic,
    Message,
    PendingDropPanic,
    false
);
case!(
    factory_message_cancelled_drop_panic,
    Message,
    PendingDropPanic,
    true
);
case!(instance_stop_success, Stop, Success, false);
case!(factory_stop_success, Stop, Success, true);
case!(instance_stop_error, Stop, Error, false);
case!(factory_stop_error, Stop, Error, true);
case!(
    instance_stop_construction_panic,
    Stop,
    ConstructPanic,
    false
);
case!(factory_stop_construction_panic, Stop, ConstructPanic, true);
case!(instance_stop_poll_panic, Stop, PollPanic, false);
case!(factory_stop_poll_panic, Stop, PollPanic, true);
case!(instance_stop_drop_panic, Stop, DropPanic, false);
case!(factory_stop_drop_panic, Stop, DropPanic, true);
case!(
    instance_stop_error_and_drop_panic,
    Stop,
    ErrorDropPanic,
    false
);
case!(
    factory_stop_error_and_drop_panic,
    Stop,
    ErrorDropPanic,
    true
);
case!(
    instance_stop_poll_and_drop_panic,
    Stop,
    PollDropPanic,
    false
);
case!(factory_stop_poll_and_drop_panic, Stop, PollDropPanic, true);
case!(instance_stop_timeout, Stop, Pending, false);
case!(factory_stop_timeout, Stop, Pending, true);
case!(
    instance_stop_timeout_drop_panic,
    Stop,
    PendingDropPanic,
    false
);
case!(
    factory_stop_timeout_drop_panic,
    Stop,
    PendingDropPanic,
    true
);
case!(instance_child_success, Child, Success, false);
case!(factory_child_success, Child, Success, true);
case!(instance_child_error, Child, Error, false);
case!(factory_child_error, Child, Error, true);
case!(
    instance_child_construction_panic,
    Child,
    ConstructPanic,
    false
);
case!(
    factory_child_construction_panic,
    Child,
    ConstructPanic,
    true
);
case!(instance_child_poll_panic, Child, PollPanic, false);
case!(factory_child_poll_panic, Child, PollPanic, true);
case!(instance_child_drop_panic, Child, DropPanic, false);
case!(factory_child_drop_panic, Child, DropPanic, true);
case!(
    instance_child_error_and_drop_panic,
    Child,
    ErrorDropPanic,
    false
);
case!(
    factory_child_error_and_drop_panic,
    Child,
    ErrorDropPanic,
    true
);
case!(
    instance_child_poll_and_drop_panic,
    Child,
    PollDropPanic,
    false
);
case!(
    factory_child_poll_and_drop_panic,
    Child,
    PollDropPanic,
    true
);
case!(instance_child_cancelled, Child, Pending, false);
case!(factory_child_cancelled, Child, Pending, true);
case!(
    instance_child_cancelled_drop_panic,
    Child,
    PendingDropPanic,
    false
);
case!(
    factory_child_cancelled_drop_panic,
    Child,
    PendingDropPanic,
    true
);
