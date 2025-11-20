//! Dynamic Thread lifecycle futures must retire independently of poll unwind.
use parrot::thread::{
    SchedulingMode, SupervisorStrategy, ThreadActorConfig, ThreadActorSystem,
    ThreadActorSystemConfig, ThreadContext,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::{ActorContext, ActorSpawnerExt, ActorTerminated},
    death_watch::{DeathWatch, TerminationRecipient},
    errors::ActorError,
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
    Batch,
    Serial,
    Retirement,
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
    fn primary(self) -> Option<&'static str> {
        match self {
            Self::Error | Self::ErrorDropPanic => Some("thread primary error"),
            Self::ConstructPanic => Some("thread construction panic"),
            Self::PollPanic | Self::PollDropPanic => Some("thread poll panic"),
            Self::DropPanic => Some("thread disposal panic"),
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
            Exit::Error | Exit::ErrorDropPanic => Poll::Ready(Err(ActorError::InternalError(
                "thread primary error".into(),
            ))),
            Exit::PollPanic | Exit::PollDropPanic => panic!("thread poll panic"),
            Exit::Pending | Exit::PendingDropPanic => Poll::Pending,
            Exit::ConstructPanic => unreachable!(),
        }
    }
}
impl<T> Drop for StageFuture<T> {
    fn drop(&mut self) {
        self.counts.disposed.fetch_add(1, Ordering::SeqCst);
        drop(self.value.take());
        assert!(!self.exit.disposal_fails(), "thread disposal panic");
    }
}
impl Probe {
    fn future<T: Send + Unpin + 'static>(&self, value: T) -> BoxedFuture<'static, ActorResult<T>> {
        self.counts.constructed.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.exit != Exit::ConstructPanic,
            "thread construction panic"
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
        assert!(
            self.stage != Stage::Retirement,
            "thread actor retirement panic"
        );
    }
}
// A batch future has a Vec output instead of the ordinary ActorResult output.
// Keep its own poll and Drop boundaries visible to the engine.
struct BatchFuture(StageFuture<Vec<ActorResult<BoxedMessage>>>);
struct SerialProbe(Probe);
impl Actor for SerialProbe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        if message.is::<bool>() {
            self.0.future(message)
        } else {
            Box::pin(async { Ok(message) })
        }
    }
    fn can_batch_message(&self, message: &BoxedMessage) -> bool {
        message.is::<bool>()
    }
}
impl Future for BatchFuture {
    type Output = Vec<ActorResult<BoxedMessage>>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.0).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(values)) => Poll::Ready(values),
            Poll::Ready(Err(error)) => Poll::Ready(vec![Err(error)]),
        }
    }
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
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
        if matches!(self.stage, Stage::Message | Stage::Retirement) && message.is::<bool>() {
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
    fn can_batch_message(&self, message: &BoxedMessage) -> bool {
        self.stage == Stage::Batch && message.is::<bool>()
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        self.counts.constructed.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.exit != Exit::ConstructPanic,
            "thread construction panic"
        );
        Box::pin(BatchFuture(StageFuture {
            counts: self.counts.clone(),
            exit: self.exit,
            value: Some(messages.into_iter().map(Ok).collect()),
        }))
    }
}
struct Observed(tokio::sync::mpsc::UnboundedSender<ActorTerminated>);
impl TerminationRecipient for Observed {
    fn deliver(&self, event: ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move { self.0.send(event).map_err(|_| ActorError::Stopped) })
    }
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("thread stage deadline")
}
async fn run(stage: Stage, exit: Exit, factory: bool, mode: SchedulingMode) {
    let counts = Arc::new(Counts::default());
    let recorder = TraceRecorder::new(8).unwrap();
    let root = recorder.root();
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        default_scheduling_mode: mode,
        shutdown_timeout: Duration::from_millis(500),
        ..Default::default()
    });
    let config = ThreadActorConfig {
        supervisor_strategy: Some(if factory && exit.disposal_fails() {
            SupervisorStrategy::Restart {
                max_retries: 3,
                within: Duration::from_secs(60),
            }
        } else {
            SupervisorStrategy::Resume
        }),
        message_batch_size: Some(4),
        ..Default::default()
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
    let reference = if stage == Stage::Serial {
        if factory {
            bounded(system.spawn_supervised_with_config(
                move || SerialProbe(make()),
                "/user/stage-probe",
                config,
            ))
            .await
            .unwrap()
            .clone_boxed()
        } else {
            bounded(system.spawn_typed_with_config(SerialProbe(make()), EmptyConfig, config))
                .await
                .unwrap()
                .clone_boxed()
        }
    } else if factory {
        bounded(system.spawn_supervised_with_config(make, "/user/stage-probe", config))
            .await
            .unwrap()
            .clone_boxed()
    } else {
        bounded(system.spawn_typed_with_config(make(), EmptyConfig, config))
            .await
            .unwrap()
            .clone_boxed()
    };
    let path = reference.path();
    if stage != Stage::Init || exit == Exit::Success {
        assert_eq!(
            *bounded(reference.send(Box::new(7_u32)))
                .await
                .unwrap()
                .downcast::<u32>()
                .unwrap(),
            7
        );
    }
    let (events, mut terminated) = tokio::sync::mpsc::unbounded_channel();
    let watcher = DeathWatch::new(1);
    watcher.set_recipient(Arc::new(Observed(events))).unwrap();
    if stage != Stage::Init {
        reference.death_watch().unwrap().watch(&watcher).unwrap();
    }
    let mut request = None;
    if matches!(
        stage,
        Stage::Message | Stage::Batch | Stage::Serial | Stage::Retirement
    ) {
        let mut pending = Box::pin(root.scope(async { reference.send(Box::new(true)).await }));
        if exit.pending() {
            assert!(futures::poll!(pending.as_mut()).is_pending());
            request = Some(pending);
        } else {
            let result = bounded(pending).await;
            if let Some(expected) = exit.primary() {
                let error = result.unwrap_err();
                assert!(
                    error.to_string().contains(expected),
                    "lost original reply: {error}"
                );
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
    let resumed = matches!(
        stage,
        Stage::Message | Stage::Child | Stage::Batch | Stage::Serial | Stage::Retirement
    ) && !exit.pending()
        && !exit.disposal_fails();
    if resumed {
        assert_eq!(
            *bounded(reference.send(Box::new(8_u32)))
                .await
                .unwrap()
                .downcast::<u32>()
                .unwrap(),
            8
        );
        assert_eq!(counts.retired.load(Ordering::SeqCst), 0);
    }
    if !exit.pending()
        && (stage == Stage::Init && exit != Exit::Success
            || matches!(
                stage,
                Stage::Message | Stage::Child | Stage::Batch | Stage::Serial | Stage::Retirement
            ) && !resumed)
    {
        bounded(async {
            while system.resolve_actor(&path).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    let first = bounded(reference.stop()).await;
    let timed_out = matches!(
        first,
        Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
    );
    if stage == Stage::Stop && exit.pending() {
        assert!(
            timed_out
                || exit.disposal_fails()
                    && first
                        .as_ref()
                        .is_err_and(|error| error.to_string().contains("thread disposal panic")),
            "pending hook unexpectedly completed: {first:?}"
        );
    }
    bounded(async {
        while system.resolve_actor(&path).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    if let Some(request) = request {
        assert!(bounded(request).await.is_err());
    }
    if stage != Stage::Init {
        let event = bounded(terminated.recv()).await.unwrap();
        assert_eq!(event.path, path);
        if exit.disposal_fails() {
            let reason = format!("{:?}", event.reason);
            assert!(
                reason.contains("thread disposal panic"),
                "lost retirement failure: {reason}"
            );
        }
        assert!(terminated.try_recv().is_err(), "duplicate terminal event");
    }
    for _ in 0..2 {
        let result = bounded(reference.stop()).await;
        if timed_out {
            assert!(matches!(
                result,
                Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
            ));
        } else if exit.disposal_fails() {
            let error = result.unwrap_err();
            assert!(
                error.to_string().contains("thread disposal panic"),
                "lost cleanup receipt: {error}"
            );
        } else if stage == Stage::Stop
            && let Some(expected) = exit.primary()
        {
            let error = result.unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        } else {
            result.unwrap();
        }
    }
    assert_eq!(
        counts.made.load(Ordering::SeqCst),
        1,
        "failed cleanup must veto reconstruction"
    );
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
    bounded(system.shutdown_internal()).await.unwrap();
    if matches!(
        stage,
        Stage::Message | Stage::Batch | Stage::Serial | Stage::Retirement
    ) {
        recorder.close();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.records.len(), 1);
        let record = &snapshot.records[0];
        assert_eq!(record.trace_id, root.trace_id());
        assert_eq!(record.parent_span_id, Some(root.span_id()));
        assert_eq!(record.target, path);
        assert_eq!(
            record.kind,
            if matches!(stage, Stage::Batch | Stage::Serial) {
                SpanKind::BusinessBatch
            } else {
                SpanKind::DynamicHandler
            }
        );
        assert_eq!(
            record.status,
            match exit {
                Exit::Success => SpanStatus::Ok,
                Exit::Error => SpanStatus::Error,
                Exit::Pending => SpanStatus::Cancelled,
                _ => SpanStatus::Panicked,
            }
        );
    }
}
fn isolated(name: &str, stage: Stage, exit: Exit, factory: bool, mode: SchedulingMode) {
    const FLAG: &str = "PARROT_THREAD_STAGE_DISPOSAL";
    if std::env::var(FLAG).as_deref() == Ok(name) {
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
                    "thread construction panic"
                        | "thread poll panic"
                        | "thread disposal panic"
                        | "thread actor retirement panic"
                )
            ) {
                original(info);
            }
        }));
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(stage, exit, factory, mode));
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
    ($name:ident, $stage:ident, $exit:ident) => {
        mod $name {
            use super::*;
            #[test]
            fn shared_instance() {
                isolated(
                    concat!(stringify!($name), "::shared_instance"),
                    Stage::$stage,
                    Exit::$exit,
                    false,
                    SchedulingMode::SharedPool {
                        max_messages_per_run: 4,
                    },
                );
            }
            #[test]
            fn shared_factory() {
                isolated(
                    concat!(stringify!($name), "::shared_factory"),
                    Stage::$stage,
                    Exit::$exit,
                    true,
                    SchedulingMode::SharedPool {
                        max_messages_per_run: 4,
                    },
                );
            }
            #[test]
            fn dedicated_instance() {
                isolated(
                    concat!(stringify!($name), "::dedicated_instance"),
                    Stage::$stage,
                    Exit::$exit,
                    false,
                    SchedulingMode::DedicatedThread,
                );
            }
            #[test]
            fn dedicated_factory() {
                isolated(
                    concat!(stringify!($name), "::dedicated_factory"),
                    Stage::$stage,
                    Exit::$exit,
                    true,
                    SchedulingMode::DedicatedThread,
                );
            }
            #[test]
            fn sharded_instance() {
                isolated(
                    concat!(stringify!($name), "::sharded_instance"),
                    Stage::$stage,
                    Exit::$exit,
                    false,
                    SchedulingMode::Sharded {
                        affinity_key: "stage-contract".into(),
                    },
                );
            }
            #[test]
            fn sharded_factory() {
                isolated(
                    concat!(stringify!($name), "::sharded_factory"),
                    Stage::$stage,
                    Exit::$exit,
                    true,
                    SchedulingMode::Sharded {
                        affinity_key: "stage-contract".into(),
                    },
                );
            }
        }
    };
}
case!(init_success, Init, Success);
case!(init_error, Init, Error);
case!(init_construct, Init, ConstructPanic);
case!(init_poll, Init, PollPanic);
case!(init_drop, Init, DropPanic);
case!(init_error_drop, Init, ErrorDropPanic);
case!(init_poll_drop, Init, PollDropPanic);
case!(init_pending, Init, Pending);
case!(init_pending_drop, Init, PendingDropPanic);
case!(message_success, Message, Success);
case!(message_error, Message, Error);
case!(message_construct, Message, ConstructPanic);
case!(message_poll, Message, PollPanic);
case!(message_drop, Message, DropPanic);
case!(message_error_drop, Message, ErrorDropPanic);
case!(message_poll_drop, Message, PollDropPanic);
case!(message_pending, Message, Pending);
case!(message_pending_drop, Message, PendingDropPanic);
case!(child_success, Child, Success);
case!(child_error, Child, Error);
case!(child_construct, Child, ConstructPanic);
case!(child_poll, Child, PollPanic);
case!(child_drop, Child, DropPanic);
case!(child_error_drop, Child, ErrorDropPanic);
case!(child_poll_drop, Child, PollDropPanic);
case!(child_pending, Child, Pending);
case!(child_pending_drop, Child, PendingDropPanic);
case!(stop_success, Stop, Success);
case!(stop_error, Stop, Error);
case!(stop_construct, Stop, ConstructPanic);
case!(stop_poll, Stop, PollPanic);
case!(stop_drop, Stop, DropPanic);
case!(stop_error_drop, Stop, ErrorDropPanic);
case!(stop_poll_drop, Stop, PollDropPanic);
case!(stop_pending, Stop, Pending);
case!(stop_pending_drop, Stop, PendingDropPanic);
case!(batch_success, Batch, Success);
case!(batch_error, Batch, Error);
case!(batch_construct, Batch, ConstructPanic);
case!(batch_poll, Batch, PollPanic);
case!(batch_drop, Batch, DropPanic);
case!(batch_error_drop, Batch, ErrorDropPanic);
case!(batch_poll_drop, Batch, PollDropPanic);
case!(batch_pending, Batch, Pending);
case!(batch_pending_drop, Batch, PendingDropPanic);
case!(serial_success, Serial, Success);
case!(serial_error, Serial, Error);
case!(serial_construct, Serial, ConstructPanic);
case!(serial_poll, Serial, PollPanic);
case!(serial_drop, Serial, DropPanic);
case!(serial_error_drop, Serial, ErrorDropPanic);
case!(serial_poll_drop, Serial, PollDropPanic);
case!(serial_pending, Serial, Pending);
case!(serial_pending_drop, Serial, PendingDropPanic);
case!(retirement_drop, Retirement, DropPanic);
case!(retirement_error_drop, Retirement, ErrorDropPanic);
case!(retirement_poll_drop, Retirement, PollDropPanic);
case!(retirement_pending_drop, Retirement, PendingDropPanic);
