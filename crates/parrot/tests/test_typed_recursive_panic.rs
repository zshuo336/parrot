//! Static dispatch and cleanup must consume recursive panic payloads safely.
use parrot::{actix::ActixActorSystem, thread::ThreadActorSystem};
use parrot_api::{
    errors::ActorError,
    message::Message,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
struct RecursivePanic;
impl Drop for RecursivePanic {
    fn drop(&mut self) {
        panic!("secondary typed panic payload destructor");
    }
}
struct Queued {
    drops: Arc<AtomicUsize>,
    kind: u8,
}
impl Drop for Queued {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        match self.kind {
            1 => panic!("first queued destructor"),
            2 => std::panic::panic_any(RecursivePanic),
            _ => {}
        }
    }
}
enum Op {
    Read,
    Crash,
    Hold,
    Queued(Queued),
}
impl Message for Op {
    type Result = usize;
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Healthy,
    Handler,
    Constructor,
    Stop,
    Queue,
    QueueActorDrop,
    StopAndDrop,
}
struct Probe {
    phase: Phase,
    value: usize,
    entered: Arc<tokio::sync::Notify>,
    drops: Arc<AtomicUsize>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if matches!(self.phase, Phase::QueueActorDrop | Phase::StopAndDrop) {
            std::panic::panic_any(RecursivePanic);
        }
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Op;
    type Reply = usize;
    fn dispatch(&mut self, op: Op) -> BoxedFuture<'_, ActorResult<usize>> {
        if matches!(op, Op::Crash) && self.phase == Phase::Constructor {
            self.value += 1;
            std::panic::panic_any(RecursivePanic);
        }
        Box::pin(async move {
            match op {
                Op::Read => Ok(self.value),
                Op::Crash => {
                    self.value += 1;
                    std::panic::panic_any(RecursivePanic)
                }
                Op::Hold => {
                    self.entered.notify_one();
                    std::future::pending().await
                }
                Op::Queued(value) => {
                    drop(value);
                    Ok(self.value)
                }
            }
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            if matches!(self.phase, Phase::Stop | Phase::StopAndDrop) {
                std::panic::panic_any(RecursivePanic);
            }
            Ok(())
        })
    }
}
impl ParrotMsgVariant<Probe> for Op {
    fn inject(value: Self) -> Self {
        value
    }
    fn extract(value: usize) -> ActorResult<usize> {
        Ok(value)
    }
}
fn strategy() -> SupervisorStrategyType {
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Resume),
    })
}
fn diagnostic(error: &ActorError) {
    assert!(
        matches!(error, ActorError::Panic(text) if text.contains("panic payload destructor panicked")),
        "wrong recursive panic receipt: {error:?}"
    );
}
struct WatchSink(tokio::sync::mpsc::UnboundedSender<parrot_api::context::ActorTerminated>);
impl parrot_api::death_watch::TerminationRecipient for WatchSink {
    fn deliver(
        &self,
        event: parrot_api::context::ActorTerminated,
    ) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move { self.0.send(event).map_err(|_| ActorError::Stopped) })
    }
}
async fn run(actix: bool, phase: Phase) {
    let actor_drops = Arc::new(AtomicUsize::new(0));
    let queued_drops = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let data = Probe {
        phase,
        value: 0,
        entered: entered.clone(),
        drops: actor_drops.clone(),
    };
    let (thread, actix_system, reference) = if actix {
        let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
        let reference = system
            .spawn_typed_with_strategy::<Probe, Op>(data, "/recursive", strategy())
            .await
            .unwrap();
        (None, Some(system), reference)
    } else {
        let system = ThreadActorSystem::shared(Default::default());
        let reference = system
            .spawn_typed_with_strategy::<Probe, Op>(data, "/recursive", strategy())
            .await
            .unwrap();
        (Some(system), None, reference)
    };
    assert_eq!(reference.ask(Op::Read).await.unwrap(), 0);
    let target = reference.clone().into_dyn().death_watch().unwrap();
    let watcher = parrot_api::death_watch::DeathWatch::new(1);
    let (events, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    watcher.set_recipient(Arc::new(WatchSink(events))).unwrap();
    target.watch(&watcher).unwrap();
    if matches!(phase, Phase::Handler | Phase::Constructor) {
        let error = tokio::time::timeout(Duration::from_secs(3), reference.ask(Op::Crash))
            .await
            .expect("panic must produce a reply")
            .unwrap_err();
        diagnostic(&error);
        assert_eq!(
            reference.ask(Op::Read).await.unwrap(),
            1,
            "Resume must keep the same instance and its effects"
        );
        assert_eq!(actor_drops.load(Ordering::SeqCst), 0);
    }
    if matches!(phase, Phase::Queue | Phase::QueueActorDrop) {
        reference.tell(Op::Hold).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        for kind in [1, 2, 0] {
            reference
                .tell(Op::Queued(Queued {
                    drops: queued_drops.clone(),
                    kind,
                }))
                .await
                .unwrap();
        }
    }
    let result = tokio::time::timeout(Duration::from_secs(4), reference.stop())
        .await
        .expect("stop receipt must complete");
    match phase {
        Phase::Stop | Phase::StopAndDrop => diagnostic(&result.unwrap_err()),
        Phase::Queue | Phase::QueueActorDrop => {
            let error = result.unwrap_err();
            assert!(
                matches!(error, ActorError::Panic(ref message) if message.contains("first queued destructor")),
                "first cleanup error must survive later recursive panic: {error:?}"
            );
            assert_eq!(
                queued_drops.load(Ordering::SeqCst),
                3,
                "every admitted payload must actually retire"
            );
        }
        _ => result.unwrap(),
    }
    assert_eq!(actor_drops.load(Ordering::SeqCst), 1);
    // A stop error alone is insufficient: an execution owner can still have
    // lost the termination reason and silently abandoned every observer.
    let event = tokio::time::timeout(Duration::from_secs(3), notifications.recv())
        .await
        .expect("actual retirement must notify the registered watcher")
        .expect("watch subscription must survive panic normalization");
    assert_eq!(event.path, "/recursive");
    assert_eq!(event.generation, target.id());
    if matches!(
        phase,
        Phase::Stop | Phase::StopAndDrop | Phase::Queue | Phase::QueueActorDrop
    ) {
        assert!(
            matches!(event.reason, parrot_api::supervisor::DeathReason::Panic(_)),
            "wrong termination reason: {:?}",
            event.reason
        );
    } else {
        assert!(matches!(
            event.reason,
            parrot_api::supervisor::DeathReason::Normal
        ));
    }
    if let Some(system) = thread {
        system.shutdown_internal().await.unwrap();
    }
    if let Some(system) = actix_system {
        system.as_ref().clone().shutdown().await.unwrap();
    }
}
fn isolated(name: &str, body: impl FnOnce()) {
    const FLAG: &str = "PARROT_TYPED_RECURSIVE_CASE";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        body();
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
            panic!("{name}: deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! case {
    ($name:ident,$actix:expr,$phase:ident) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), || {
                actix::System::new().block_on(run($actix, Phase::$phase))
            });
        }
    };
}
case!(thread_healthy_positive_control, false, Healthy);
case!(actix_healthy_positive_control, true, Healthy);
case!(
    thread_handler_recursive_payload_preserves_resume,
    false,
    Handler
);
case!(
    actix_handler_recursive_payload_preserves_resume,
    true,
    Handler
);
case!(
    thread_constructor_recursive_payload_preserves_resume,
    false,
    Constructor
);
case!(
    actix_constructor_recursive_payload_preserves_resume,
    true,
    Constructor
);
case!(thread_stop_recursive_payload_preserves_receipt, false, Stop);
case!(actix_stop_recursive_payload_preserves_receipt, true, Stop);
case!(
    thread_later_cleanup_panic_keeps_first_error_and_drains,
    false,
    Queue
);
case!(
    actix_later_cleanup_panic_keeps_first_error_and_drains,
    true,
    Queue
);

case!(
    thread_actor_drop_after_cleanup_error_is_isolated,
    false,
    QueueActorDrop
);
case!(
    actix_actor_drop_after_cleanup_error_is_isolated,
    true,
    QueueActorDrop
);
case!(
    thread_cleanup_override_consumes_hook_panic,
    false,
    StopAndDrop
);
case!(
    actix_cleanup_override_consumes_hook_panic,
    true,
    StopAndDrop
);

mod dispatch_future_ownership {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    #[derive(Clone, Copy)]
    enum Mode {
        Healthy,
        PollDrop,
        ReadyDrop,
        ErrorDrop,
        CancelDrop,
        CancelHealthy,
    }
    struct DispatchFuture {
        mode: Mode,
        drops: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
    }
    impl Future for DispatchFuture {
        type Output = ActorResult<usize>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            match self.mode {
                Mode::CancelDrop | Mode::CancelHealthy => {
                    self.entered.notify_one();
                    Poll::Pending
                }
                Mode::PollDrop => panic!("original typed dispatch poll failure"),
                Mode::ErrorDrop => Poll::Ready(Err(ActorError::MessageHandlingError(
                    "original typed business error".into(),
                ))),
                _ => Poll::Ready(Ok(17)),
            }
        }
    }
    impl Drop for DispatchFuture {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            if !matches!(self.mode, Mode::Healthy | Mode::CancelHealthy) {
                panic!("typed dispatch future cleanup failed");
            }
        }
    }
    enum Request {
        Exercise,
        Read,
    }
    impl Message for Request {
        type Result = usize;
    }
    struct Caller {
        mode: Mode,
        drops: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
        calls: usize,
        hooks: Arc<AtomicUsize>,
    }
    impl ParrotTypedDispatch for Caller {
        type Msg = Request;
        type Reply = usize;
        fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            self.hooks.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn dispatch(&mut self, request: Request) -> BoxedFuture<'_, ActorResult<usize>> {
            match request {
                Request::Read => {
                    let calls = self.calls;
                    Box::pin(async move { Ok(calls) })
                }
                Request::Exercise => {
                    self.calls += 1;
                    Box::pin(DispatchFuture {
                        mode: self.mode,
                        drops: self.drops.clone(),
                        entered: self.entered.clone(),
                    })
                }
            }
        }
    }
    impl ParrotMsgVariant<Caller> for Request {
        fn inject(request: Self) -> Self {
            request
        }
        fn extract(reply: usize) -> ActorResult<usize> {
            Ok(reply)
        }
    }
    fn resume() -> OneForOneStrategy {
        OneForOneStrategy {
            max_restarts: 0,
            within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Resume),
        }
    }
    async fn exercise(actix: bool, factory: bool, mode: Mode) {
        let drops = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let hooks = Arc::new(AtomicUsize::new(0));
        let make = {
            let drops = drops.clone();
            let entered = entered.clone();
            let hooks = hooks.clone();
            move || Caller {
                mode,
                drops: drops.clone(),
                entered: entered.clone(),
                calls: 0,
                hooks: hooks.clone(),
            }
        };
        let (thread, actix_system, actor) = if actix {
            let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
            let actor = if factory {
                system
                    .spawn_typed_factory_with_strategy::<Caller, Request, _>(
                        make,
                        "/dispatch-owner",
                        resume(),
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed_with_strategy::<Caller, Request>(
                        make(),
                        "/dispatch-owner",
                        SupervisorStrategyType::OneForOne(resume()),
                    )
                    .await
                    .unwrap()
            };
            (None, Some(system), actor)
        } else {
            let system = ThreadActorSystem::shared(Default::default());
            let actor = if factory {
                system
                    .spawn_typed_factory_with_strategy::<Caller, Request, _>(
                        make,
                        "/dispatch-owner",
                        resume(),
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed_with_strategy::<Caller, Request>(
                        make(),
                        "/dispatch-owner",
                        SupervisorStrategyType::OneForOne(resume()),
                    )
                    .await
                    .unwrap()
            };
            (Some(system), None, actor)
        };
        use futures::FutureExt;
        let checked = std::panic::AssertUnwindSafe(async {
            if matches!(mode, Mode::CancelDrop | Mode::CancelHealthy) {
                actor.tell(Request::Exercise).await.unwrap();
                tokio::time::timeout(Duration::from_secs(3), entered.notified()).await.expect("dispatch did not enter");
            } else {
            let result = actor.ask_with_timeout(Request::Exercise, Duration::from_secs(3)).await;
            assert_eq!(drops.load(Ordering::SeqCst), 1, "reply preceded actual future disposal");
            match mode {
                Mode::Healthy => assert_eq!(result.unwrap(), 17),
                Mode::PollDrop => assert!(matches!(result, Err(ActorError::Panic(ref text)) if text.contains("original typed dispatch poll failure")), "first poll error was replaced: {result:?}"),
                Mode::ReadyDrop => assert!(matches!(result, Err(ActorError::Panic(ref text)) if text.contains("typed dispatch future cleanup failed")), "successful reply masked cleanup failure: {result:?}"),
                Mode::ErrorDrop => assert!(matches!(result, Err(ActorError::MessageHandlingError(ref text)) if text == "original typed business error"), "original business error was replaced: {result:?}"),
                Mode::CancelDrop | Mode::CancelHealthy => unreachable!(),
            }
            assert_eq!(actor.ask_with_timeout(Request::Read, Duration::from_secs(3)).await.unwrap(), 1, "Resume must retain instance effects without replay");
            }
            let stopped = tokio::time::timeout(Duration::from_secs(4), actor.stop()).await.expect("typed stop did not complete");
            assert_eq!(drops.load(Ordering::SeqCst), 1, "stop preceded actual dispatch disposal");
            assert_eq!(hooks.load(Ordering::SeqCst), usize::from(!matches!(mode, Mode::CancelDrop)),
                "cancelled destructor panic must skip normal hook; healthy cancellation and explicit Resume retain it");
            if matches!(mode, Mode::Healthy | Mode::CancelHealthy) {
                stopped.expect("healthy dispatch/cancellation poisoned cleanup");
            } else {
                let error = stopped.expect_err("stop masked actual typed dispatch disposal failure");
                assert!(error.to_string().contains("typed dispatch future cleanup failed"), "wrong cleanup receipt: {error}");
            }
        }).catch_unwind().await;
        let _ = tokio::time::timeout(Duration::from_secs(4), actor.stop()).await;
        if let Some(system) = thread {
            let _ = system.shutdown_internal().await;
        }
        if let Some(system) = actix_system {
            let _ = system.as_ref().clone().shutdown().await;
        }
        if let Err(payload) = checked {
            std::panic::resume_unwind(payload);
        }
    }
    macro_rules! case {
        ($name:ident, $actix:expr, $factory:expr, $mode:ident) => {
            #[test]
            fn $name() {
                super::isolated(
                    &format!("dispatch_future_ownership::{}", stringify!($name)),
                    || actix::System::new().block_on(exercise($actix, $factory, Mode::$mode)),
                );
            }
        };
    }
    case!(thread_instance_healthy, false, false, Healthy);
    case!(thread_factory_healthy, false, true, Healthy);
    case!(actix_instance_healthy, true, false, Healthy);
    case!(actix_factory_healthy, true, true, Healthy);
    case!(thread_instance_poll_drop, false, false, PollDrop);
    case!(thread_factory_poll_drop, false, true, PollDrop);
    case!(actix_instance_poll_drop, true, false, PollDrop);
    case!(actix_factory_poll_drop, true, true, PollDrop);
    case!(thread_instance_ready_drop, false, false, ReadyDrop);
    case!(thread_factory_ready_drop, false, true, ReadyDrop);
    case!(actix_instance_ready_drop, true, false, ReadyDrop);
    case!(actix_factory_ready_drop, true, true, ReadyDrop);
    case!(thread_instance_error_drop, false, false, ErrorDrop);
    case!(thread_factory_error_drop, false, true, ErrorDrop);
    case!(actix_instance_error_drop, true, false, ErrorDrop);
    case!(actix_factory_error_drop, true, true, ErrorDrop);
    case!(thread_instance_cancel_drop, false, false, CancelDrop);
    case!(thread_factory_cancel_drop, false, true, CancelDrop);
    case!(actix_instance_cancel_drop, true, false, CancelDrop);
    case!(actix_factory_cancel_drop, true, true, CancelDrop);
    case!(thread_instance_cancel_healthy, false, false, CancelHealthy);
    case!(thread_factory_cancel_healthy, false, true, CancelHealthy);
    case!(actix_instance_cancel_healthy, true, false, CancelHealthy);
    case!(actix_factory_cancel_healthy, true, true, CancelHealthy);

    async fn restart_rejects_failed_cleanup(actix: bool) {
        let built = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let make = {
            let built = built.clone();
            let drops = drops.clone();
            move || {
                built.fetch_add(1, Ordering::SeqCst);
                Caller {
                    mode: Mode::ErrorDrop,
                    drops: drops.clone(),
                    entered: Arc::new(tokio::sync::Notify::new()),
                    calls: 0,
                    hooks: Arc::new(AtomicUsize::new(0)),
                }
            }
        };
        let policy = OneForOneStrategy {
            max_restarts: 1,
            within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
        };
        let (thread, actix_system, actor) = if actix {
            let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
            let actor = system
                .spawn_typed_factory_with_strategy::<Caller, Request, _>(
                    make,
                    "/dispatch-restart",
                    policy,
                )
                .await
                .unwrap();
            (None, Some(system), actor)
        } else {
            let system = ThreadActorSystem::shared(Default::default());
            let actor = system
                .spawn_typed_factory_with_strategy::<Caller, Request, _>(
                    make,
                    "/dispatch-restart",
                    policy,
                )
                .await
                .unwrap();
            (Some(system), None, actor)
        };
        use futures::FutureExt;
        let checked = std::panic::AssertUnwindSafe(async {
            assert!(
                actor
                    .ask_with_timeout(Request::Exercise, Duration::from_secs(3))
                    .await
                    .is_err()
            );
            let next = actor
                .ask_with_timeout(Request::Read, Duration::from_secs(3))
                .await;
            assert!(
                matches!(next, Err(ActorError::Stopped)),
                "failed cleanup allowed another generation: {next:?}"
            );
            let stopped = tokio::time::timeout(Duration::from_secs(4), actor.stop())
                .await
                .expect("retirement lost progress")
                .expect_err("restart hid actual cleanup failure");
            assert!(
                stopped
                    .to_string()
                    .contains("typed dispatch future cleanup failed")
            );
            assert_eq!(
                built.load(Ordering::SeqCst),
                1,
                "reconstructed after failed generation disposal"
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        })
        .catch_unwind()
        .await;
        let _ = actor.stop().await;
        if let Some(system) = thread {
            let _ = system.shutdown_internal().await;
        }
        if let Some(system) = actix_system {
            let _ = system.as_ref().clone().shutdown().await;
        }
        if let Err(payload) = checked {
            std::panic::resume_unwind(payload);
        }
    }
    #[test]
    fn thread_restart_rejects_failed_cleanup() {
        super::isolated(
            "dispatch_future_ownership::thread_restart_rejects_failed_cleanup",
            || actix::System::new().block_on(restart_rejects_failed_cleanup(false)),
        );
    }
    #[test]
    fn actix_restart_rejects_failed_cleanup() {
        super::isolated(
            "dispatch_future_ownership::actix_restart_rejects_failed_cleanup",
            || actix::System::new().block_on(restart_rejects_failed_cleanup(true)),
        );
    }
}

mod dispatch_reply_ownership {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    #[derive(Debug, Default)]
    struct Counts {
        built: AtomicUsize,
        actors: AtomicUsize,
        futures: AtomicUsize,
        replies: AtomicUsize,
    }
    #[derive(Debug)]
    struct Reply {
        counts: Option<Arc<Counts>>,
        hostile: bool,
    }
    impl Drop for Reply {
        fn drop(&mut self) {
            if let Some(counts) = &self.counts {
                counts.replies.fetch_add(1, Ordering::SeqCst);
            }
            assert!(!self.hostile, "later typed reply disposal");
        }
    }
    struct Response {
        counts: Arc<Counts>,
        healthy: bool,
    }
    impl Future for Response {
        type Output = ActorResult<Reply>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(Ok(Reply {
                counts: Some(self.counts.clone()),
                hostile: !self.healthy,
            }))
        }
    }
    impl Drop for Response {
        fn drop(&mut self) {
            self.counts.futures.fetch_add(1, Ordering::SeqCst);
            assert!(self.healthy, "first typed future disposal");
        }
    }
    struct Request(bool);
    impl Message for Request {
        type Result = Reply;
    }
    struct Caller {
        counts: Arc<Counts>,
        healthy: bool,
        actor_hostile: bool,
    }
    impl Drop for Caller {
        fn drop(&mut self) {
            self.counts.actors.fetch_add(1, Ordering::SeqCst);
            assert!(!self.actor_hostile, "later typed actor disposal");
        }
    }
    impl ParrotTypedDispatch for Caller {
        type Msg = Request;
        type Reply = Reply;
        fn dispatch(&mut self, request: Request) -> BoxedFuture<'_, ActorResult<Reply>> {
            if request.0 {
                Box::pin(Response {
                    counts: self.counts.clone(),
                    healthy: self.healthy,
                })
            } else {
                Box::pin(async {
                    Ok(Reply {
                        counts: None,
                        hostile: false,
                    })
                })
            }
        }
    }
    impl ParrotMsgVariant<Caller> for Request {
        fn inject(request: Self) -> Self {
            request
        }
        fn extract(reply: Reply) -> ActorResult<Reply> {
            Ok(reply)
        }
    }
    async fn exercise(
        actix: bool,
        factory: bool,
        healthy: bool,
        actor_hostile: bool,
        restart: bool,
    ) {
        let counts = Arc::new(Counts::default());
        let make = {
            let counts = counts.clone();
            move || {
                counts.built.fetch_add(1, Ordering::SeqCst);
                Caller {
                    counts: counts.clone(),
                    healthy,
                    actor_hostile,
                }
            }
        };
        let policy = OneForOneStrategy {
            max_restarts: 1,
            within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(move |_| {
                if restart {
                    SupervisionDecision::Restart
                } else {
                    SupervisionDecision::Resume
                }
            }),
        };
        let (thread, actix_system, actor) = if actix {
            let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
            let actor = if factory {
                system
                    .spawn_typed_factory_with_strategy::<Caller, Request, _>(
                        make,
                        "/reply-owner",
                        policy,
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed_with_strategy::<Caller, Request>(
                        make(),
                        "/reply-owner",
                        SupervisorStrategyType::OneForOne(policy),
                    )
                    .await
                    .unwrap()
            };
            (None, Some(system), actor)
        } else {
            let system = ThreadActorSystem::shared(Default::default());
            let actor = if factory {
                system
                    .spawn_typed_factory_with_strategy::<Caller, Request, _>(
                        make,
                        "/reply-owner",
                        policy,
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed_with_strategy::<Caller, Request>(
                        make(),
                        "/reply-owner",
                        SupervisorStrategyType::OneForOne(policy),
                    )
                    .await
                    .unwrap()
            };
            (Some(system), None, actor)
        };
        use futures::FutureExt;
        let checked = std::panic::AssertUnwindSafe(async {
            let result = actor.ask_with_timeout(Request(true), Duration::from_secs(3)).await;
            assert_eq!(counts.futures.load(Ordering::SeqCst), 1, "reply preceded actual future cleanup");
            if healthy {
                let reply = result.expect("healthy reply was suppressed");
                assert_eq!(counts.replies.load(Ordering::SeqCst), 0, "framework disposed caller-owned success");
                drop(reply);
            } else {
                // If a broken implementation exposes the hostile success, retire
                // it separately so the assertion itself cannot fabricate abort.
                let error = match result {
                    Err(error) => error,
                    Ok(reply) => {
                        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(reply))) {
                            drop(payload);
                        }
                        panic!("framework exposed success after failed future cleanup");
                    }
                };
                assert!(matches!(error, ActorError::Panic(ref text) if text.contains("first typed future disposal")),
                        "later reply cleanup replaced first failure: {error:?}");
            }
            assert_eq!(counts.replies.load(Ordering::SeqCst), 1);
            if restart {
                let next = actor.ask_with_timeout(Request(false), Duration::from_secs(3)).await;
                assert!(matches!(next, Err(ActorError::Stopped)), "reconstructed after multiple cleanup failures: {next:?}");
            }
            let stopped = tokio::time::timeout(Duration::from_secs(4), actor.stop()).await.expect("stop lost progress");
            if healthy {
                stopped.expect("healthy reply poisoned stop receipt");
            } else {
                let error = stopped.expect_err("stop masked dispatch/reply cleanup");
                assert!(error.to_string().contains("first typed future disposal"), "later actor cleanup replaced first failure: {error}");
            }
            assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
            assert_eq!(counts.built.load(Ordering::SeqCst), 1);
            assert_eq!(counts.futures.load(Ordering::SeqCst), 1);
            assert_eq!(counts.replies.load(Ordering::SeqCst), 1);
        }).catch_unwind().await;
        let _ = tokio::time::timeout(Duration::from_secs(4), actor.stop()).await;
        if let Some(system) = thread {
            let _ = system.shutdown_internal().await;
        }
        if let Some(system) = actix_system {
            let _ = system.as_ref().clone().shutdown().await;
        }
        if let Err(payload) = checked {
            std::panic::resume_unwind(payload);
        }
    }
    macro_rules! case {
        ($name:ident,$actix:expr,$factory:expr,$healthy:expr,$actor:expr,$restart:expr) => {
            #[test]
            fn $name() {
                super::isolated(
                    &format!("dispatch_reply_ownership::{}", stringify!($name)),
                    || {
                        actix::System::new()
                            .block_on(exercise($actix, $factory, $healthy, $actor, $restart))
                    },
                );
            }
        };
    }
    case!(
        thread_instance_healthy_reply,
        false,
        false,
        true,
        false,
        false
    );
    case!(
        thread_factory_healthy_reply,
        false,
        true,
        true,
        false,
        false
    );
    case!(
        actix_instance_healthy_reply,
        true,
        false,
        true,
        false,
        false
    );
    case!(actix_factory_healthy_reply, true, true, true, false, false);
    case!(
        thread_instance_suppressed_reply,
        false,
        false,
        false,
        false,
        false
    );
    case!(
        thread_factory_suppressed_reply,
        false,
        true,
        false,
        false,
        false
    );
    case!(
        actix_instance_suppressed_reply,
        true,
        false,
        false,
        false,
        false
    );
    case!(
        actix_factory_suppressed_reply,
        true,
        true,
        false,
        false,
        false
    );
    case!(
        thread_instance_first_cleanup,
        false,
        false,
        false,
        true,
        false
    );
    case!(
        thread_factory_first_cleanup,
        false,
        true,
        false,
        true,
        false
    );
    case!(
        actix_instance_first_cleanup,
        true,
        false,
        false,
        true,
        false
    );
    case!(actix_factory_first_cleanup, true, true, false, true, false);
    case!(thread_restart_first_cleanup, false, true, false, true, true);
    case!(actix_restart_first_cleanup, true, true, false, true, true);
}

mod lifecycle_future_ownership {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    #[derive(Clone, Copy)]
    enum Stage {
        Init,
        Stop,
    }
    #[derive(Clone, Copy)]
    enum Mode {
        Healthy,
        PollDrop,
        PollOnly,
        ReturnedPanic,
    }
    struct StageFuture {
        mode: Mode,
        drops: Arc<AtomicUsize>,
    }
    impl Future for StageFuture {
        type Output = ActorResult<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            match self.mode {
                Mode::PollDrop | Mode::PollOnly => panic!("original lifecycle future poll failure"),
                Mode::ReturnedPanic => Poll::Ready(Err(ActorError::Panic(
                    "explicit lifecycle panic error".into(),
                ))),
                Mode::Healthy => Poll::Ready(Ok(())),
            }
        }
    }
    impl Drop for StageFuture {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            assert!(
                !matches!(self.mode, Mode::PollDrop),
                "later lifecycle future disposal failure"
            );
        }
    }
    struct Request;
    impl Message for Request {
        type Result = usize;
    }
    struct Owner {
        stage: Stage,
        mode: Mode,
        future_drops: Arc<AtomicUsize>,
        actor_drops: Arc<AtomicUsize>,
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            self.actor_drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Owner {
        fn stage_future(&self) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(StageFuture {
                mode: self.mode,
                drops: self.future_drops.clone(),
            })
        }
    }
    impl ParrotTypedDispatch for Owner {
        type Msg = Request;
        type Reply = usize;
        fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            match self.stage {
                Stage::Init => self.stage_future(),
                Stage::Stop => Box::pin(async { Ok(()) }),
            }
        }
        fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            match self.stage {
                Stage::Stop => self.stage_future(),
                Stage::Init => Box::pin(async { Ok(()) }),
            }
        }
        fn dispatch(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<usize>> {
            Box::pin(async { Ok(7) })
        }
    }
    impl ParrotMsgVariant<Owner> for Request {
        fn inject(msg: Self) -> Self {
            msg
        }
        fn extract(reply: usize) -> ActorResult<usize> {
            Ok(reply)
        }
    }
    async fn exercise(actix: bool, factory: bool, stage: Stage, mode: Mode) {
        let fail = !matches!(mode, Mode::Healthy);
        let future_drops = Arc::new(AtomicUsize::new(0));
        let actor_drops = Arc::new(AtomicUsize::new(0));
        let make = {
            let future_drops = future_drops.clone();
            let actor_drops = actor_drops.clone();
            move || Owner {
                stage,
                mode,
                future_drops: future_drops.clone(),
                actor_drops: actor_drops.clone(),
            }
        };
        let policy = || OneForOneStrategy {
            max_restarts: 0,
            within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Stop),
        };
        let (thread, actix_system, actor) = if actix {
            let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
            let actor = if factory {
                system
                    .spawn_typed_factory_with_strategy::<Owner, Request, _>(
                        make,
                        "/stage-owner",
                        policy(),
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed_with_strategy::<Owner, Request>(
                        make(),
                        "/stage-owner",
                        SupervisorStrategyType::OneForOne(policy()),
                    )
                    .await
                    .unwrap()
            };
            (None, Some(system), actor)
        } else {
            let system = ThreadActorSystem::shared(Default::default());
            let actor = if factory {
                system
                    .spawn_typed_factory_with_strategy::<Owner, Request, _>(
                        make,
                        "/stage-owner",
                        policy(),
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed_with_strategy::<Owner, Request>(
                        make(),
                        "/stage-owner",
                        SupervisorStrategyType::OneForOne(policy()),
                    )
                    .await
                    .unwrap()
            };
            (Some(system), None, actor)
        };
        use futures::FutureExt;
        let checked = std::panic::AssertUnwindSafe(async {
            let response = actor
                .ask_with_timeout(Request, Duration::from_secs(3))
                .await;
            if fail && matches!(stage, Stage::Init) {
                assert!(
                    response.is_err(),
                    "failed initialization dispatched a message"
                );
            } else {
                assert_eq!(response.unwrap(), 7);
            }
            let stopped = tokio::time::timeout(Duration::from_secs(4), actor.stop())
                .await
                .expect("stage retirement deadline");
            if matches!(stage, Stage::Stop) {
                match mode {
                    Mode::PollOnly => assert!(matches!(&stopped, Err(ActorError::Panic(message)) if message == "original lifecycle future poll failure"), "unwind classification lost: {stopped:?}"),
                    Mode::ReturnedPanic if !actix => assert!(matches!(&stopped, Err(ActorError::Other(_))), "explicit returned error changed classification: {stopped:?}"),
                    Mode::ReturnedPanic => assert!(matches!(&stopped, Err(ActorError::Panic(message)) if message == "explicit lifecycle panic error"), "returned panic payload changed: {stopped:?}"),
                    _ => {}
                }
            }
            if fail {
                assert!(
                    stopped.is_err(),
                    "failed lifecycle future was reported as clean retirement"
                );
            } else {
                stopped.expect("healthy lifecycle future poisoned retirement");
            }
            assert_eq!(
                future_drops.load(Ordering::SeqCst),
                1,
                "user future must retire exactly once"
            );
            assert_eq!(
                actor_drops.load(Ordering::SeqCst),
                1,
                "actor must retire exactly once"
            );
            assert!(!actor.is_alive());
        })
        .catch_unwind()
        .await;
        if let Some(system) = thread {
            let _ = system.shutdown_internal().await;
        }
        if let Some(system) = actix_system {
            let _ = system.as_ref().clone().shutdown().await;
        }
        if let Err(payload) = checked {
            std::panic::resume_unwind(payload);
        }
    }
    macro_rules! case {
        ($name:ident, $actix:expr, $factory:expr, $stage:ident, $fail:expr) => {
            #[test]
            fn $name() {
                super::isolated(
                    &format!("lifecycle_future_ownership::{}", stringify!($name)),
                    || {
                        actix::System::new().block_on(exercise(
                            $actix,
                            $factory,
                            Stage::$stage,
                            if $fail { Mode::PollDrop } else { Mode::Healthy },
                        ))
                    },
                );
            }
        };
    }
    case!(thread_instance_init_healthy, false, false, Init, false);
    case!(thread_instance_init_poll_drop, false, false, Init, true);
    case!(thread_factory_init_healthy, false, true, Init, false);
    case!(thread_factory_init_poll_drop, false, true, Init, true);
    case!(actix_instance_init_healthy, true, false, Init, false);
    case!(actix_instance_init_poll_drop, true, false, Init, true);
    case!(actix_factory_init_healthy, true, true, Init, false);
    case!(actix_factory_init_poll_drop, true, true, Init, true);
    case!(thread_instance_stop_healthy, false, false, Stop, false);
    case!(thread_instance_stop_poll_drop, false, false, Stop, true);
    case!(thread_factory_stop_healthy, false, true, Stop, false);
    case!(thread_factory_stop_poll_drop, false, true, Stop, true);
    case!(actix_instance_stop_healthy, true, false, Stop, false);
    case!(actix_instance_stop_poll_drop, true, false, Stop, true);
    case!(actix_factory_stop_healthy, true, true, Stop, false);
    case!(actix_factory_stop_poll_drop, true, true, Stop, true);

    macro_rules! classification {
        ($name:ident, $actix:expr, $factory:expr, $mode:ident) => {
            #[test]
            fn $name() {
                super::isolated(
                    &format!("lifecycle_future_ownership::{}", stringify!($name)),
                    || {
                        actix::System::new().block_on(exercise(
                            $actix,
                            $factory,
                            Stage::Stop,
                            Mode::$mode,
                        ))
                    },
                );
            }
        };
    }
    classification!(thread_instance_stop_unwind, false, false, PollOnly);
    classification!(
        thread_instance_stop_returned_panic,
        false,
        false,
        ReturnedPanic
    );
    classification!(thread_factory_stop_unwind, false, true, PollOnly);
    classification!(
        thread_factory_stop_returned_panic,
        false,
        true,
        ReturnedPanic
    );
    classification!(actix_instance_stop_unwind, true, false, PollOnly);
    classification!(
        actix_instance_stop_returned_panic,
        true,
        false,
        ReturnedPanic
    );
    classification!(actix_factory_stop_unwind, true, true, PollOnly);
    classification!(actix_factory_stop_returned_panic, true, true, ReturnedPanic);
}

mod lifecycle_composition {
    use super::*;
    use std::{
        future::Future,
        pin::Pin,
        sync::{Condvar, Mutex},
        task::{Context, Poll},
    };

    #[derive(Clone, Copy, PartialEq)]
    enum Stage {
        Init,
        Stop,
    }
    #[derive(Clone, Copy)]
    enum Mode {
        Healthy,
        ReadyDrop,
        ErrorDrop,
        ErrorOnly,
        PendingDrop,
        Pending,
    }
    impl Mode {
        fn drop_fails(self) -> bool {
            matches!(self, Self::ReadyDrop | Self::ErrorDrop | Self::PendingDrop)
        }
        fn pending(self) -> bool {
            matches!(self, Self::PendingDrop | Self::Pending)
        }
    }
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
        entered: tokio::sync::Notify,
    }
    impl Gate {
        fn block(&self) {
            self.entered.notify_one();
            let (_guard, timeout) = self
                .changed
                .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(5), |open| {
                    !*open
                })
                .unwrap();
            assert!(!timeout.timed_out(), "test must release future disposal");
        }
        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    #[derive(Default)]
    struct State {
        built: AtomicUsize,
        future_drops: AtomicUsize,
        actor_drops: AtomicUsize,
        dispatched: AtomicUsize,
        hooks: AtomicUsize,
        entered: tokio::sync::Notify,
        policy: Mutex<Vec<String>>,
    }
    struct StageFuture {
        mode: Mode,
        state: Arc<State>,
        gate: Option<Arc<Gate>>,
    }
    impl Future for StageFuture {
        type Output = ActorResult<()>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            self.state.entered.notify_one();
            match self.mode {
                Mode::Pending | Mode::PendingDrop => Poll::Pending,
                Mode::ErrorDrop | Mode::ErrorOnly => Poll::Ready(Err(
                    ActorError::MessageHandlingError("original lifecycle business error".into()),
                )),
                _ => Poll::Ready(Ok(())),
            }
        }
    }
    impl Drop for StageFuture {
        fn drop(&mut self) {
            if let Some(gate) = &self.gate {
                gate.block();
            }
            self.state.future_drops.fetch_add(1, Ordering::SeqCst);
            assert!(!self.mode.drop_fails(), "first lifecycle future cleanup");
        }
    }
    struct Request;
    impl Message for Request {
        type Result = usize;
    }
    struct Owner {
        stage: Stage,
        mode: Mode,
        actor_bomb: bool,
        state: Arc<State>,
        gate: Option<Arc<Gate>>,
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            self.state.actor_drops.fetch_add(1, Ordering::SeqCst);
            assert!(!self.actor_bomb, "later lifecycle actor cleanup");
        }
    }
    impl Owner {
        fn future(&self) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(StageFuture {
                mode: self.mode,
                state: self.state.clone(),
                gate: self.gate.clone(),
            })
        }
    }
    impl ParrotTypedDispatch for Owner {
        type Msg = Request;
        type Reply = usize;
        fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            if self.stage == Stage::Init {
                self.future()
            } else {
                Box::pin(async { Ok(()) })
            }
        }
        fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
            self.state.hooks.fetch_add(1, Ordering::SeqCst);
            if self.stage == Stage::Stop {
                self.future()
            } else {
                Box::pin(async { Ok(()) })
            }
        }
        fn dispatch(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<usize>> {
            self.state.dispatched.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(7) })
        }
    }
    impl ParrotMsgVariant<Owner> for Request {
        fn inject(msg: Self) -> Self {
            msg
        }
        fn extract(reply: usize) -> ActorResult<usize> {
            Ok(reply)
        }
    }
    // Every policy inspects concrete known variants, not arbitrary Display/source callbacks.
    async fn exercise(
        actix: bool,
        factory: bool,
        stage: Stage,
        mode: Mode,
        actor_bomb: bool,
        decision: SupervisionDecision,
        recover: bool,
    ) {
        let state = Arc::new(State::default());
        let gate = (stage == Stage::Stop && mode.pending()).then(|| Arc::new(Gate::default()));
        let _release = gate.clone().map(Release);
        let make = {
            let state = state.clone();
            let gate = gate.clone();
            move || {
                let built = state.built.fetch_add(1, Ordering::SeqCst);
                Owner {
                    stage,
                    mode: if recover && built > 0 {
                        Mode::Healthy
                    } else {
                        mode
                    },
                    actor_bomb,
                    state: state.clone(),
                    gate: gate.clone(),
                }
            }
        };
        let policy = {
            let state = state.clone();
            OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(60),
                decider: BasicDecisionFn::new(move |error| {
                    let observed = match error {
                        ActorError::MessageHandlingError(message) => format!("business:{message}"),
                        ActorError::Panic(message) => format!("panic:{message}"),
                        _ => "unexpected error variant".into(),
                    };
                    state.policy.lock().unwrap().push(observed);
                    decision
                }),
            }
        };
        let timeout = if gate.is_some() {
            Duration::from_millis(120)
        } else {
            Duration::from_secs(3)
        };
        let (thread, actix_system, actor) = if actix {
            let system = Arc::new(
                ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
                    arbiter_count: 1,
                    shutdown_timeout: timeout,
                    ..Default::default()
                })
                .await
                .unwrap(),
            );
            let actor = if factory {
                system
                    .spawn_typed_factory_with_strategy::<Owner, Request, _>(
                        make,
                        "/composition",
                        policy,
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed_with_strategy::<Owner, Request>(
                        make(),
                        "/composition",
                        SupervisorStrategyType::OneForOne(policy),
                    )
                    .await
                    .unwrap()
            };
            (None, Some(system), actor)
        } else {
            let system =
                ThreadActorSystem::shared(parrot::thread::config::ThreadActorSystemConfig {
                    shutdown_timeout: timeout,
                    ..Default::default()
                });
            let actor = if factory {
                system
                    .spawn_typed_factory_with_strategy::<Owner, Request, _>(
                        make,
                        "/composition",
                        policy,
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed_with_strategy::<Owner, Request>(
                        make(),
                        "/composition",
                        SupervisorStrategyType::OneForOne(policy),
                    )
                    .await
                    .unwrap()
            };
            (Some(system), None, actor)
        };
        use futures::FutureExt;
        let checked = std::panic::AssertUnwindSafe(async {
            let recovering = recover && !mode.drop_fails();
            let mut notification = None;
            let mut retained_watch = None;
            if mode.pending() {
                let target = actor.clone().into_dyn().death_watch().unwrap();
                let watcher = parrot_api::death_watch::DeathWatch::new(1);
                let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
                watcher.set_recipient(Arc::new(super::WatchSink(events))).unwrap();
                target.watch(&watcher).unwrap();
                notification = Some(receiver); retained_watch = Some(watcher);
            }
            if mode.pending() && stage == Stage::Init {
                tokio::time::timeout(Duration::from_secs(3), state.entered.notified()).await.expect("init did not enter");
            } else {
                let response = actor.ask_with_timeout(Request, Duration::from_secs(3)).await;
                if stage == Stage::Init && !recovering { assert!(response.is_err(), "failed initialization dispatched a message"); }
                else { assert_eq!(response.unwrap(), 7); }
            }
            if stage == Stage::Init && !mode.pending() {
                // Let the naturally failed initializer reach supervision before
                // an explicit stop can legitimately cancel its policy decision.
                tokio::time::timeout(Duration::from_secs(3), async {
                    while state.policy.lock().unwrap().is_empty() { tokio::task::yield_now().await; }
                }).await.expect("original initialization error never reached supervision");
            }
            let stop_actor = actor.clone();
            let stop = tokio::spawn(async move { stop_actor.stop().await });
            if let Some(gate) = &gate {
                tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await.expect("expired hook future was not disposed");
                let result = tokio::time::timeout(Duration::from_secs(3), stop).await.unwrap().unwrap();
                assert!(matches!(result, Err(ActorError::Timeout | ActorError::TimeoutDetail(_))), "stop deadline must precede blocked cleanup: {result:?}");
                assert_eq!(state.future_drops.load(Ordering::SeqCst), 0);
                assert_eq!(state.actor_drops.load(Ordering::SeqCst), 0);
                assert_eq!(state.built.load(Ordering::SeqCst), 1);
                gate.release();
            } else {
                let result = tokio::time::timeout(Duration::from_secs(4), stop).await.unwrap().unwrap();
                if mode.drop_fails() {
                    assert!(matches!(result, Err(ActorError::Panic(ref text)) if text == "first lifecycle future cleanup"), "first physical cleanup failure lost: {result:?}");
                } else if mode.pending() || recovering { result.expect("healthy cancellation/rebuild poisoned stop"); }
            }
            if let Some(receiver) = &mut notification {
                let event = tokio::time::timeout(Duration::from_secs(3), receiver.recv()).await.unwrap().unwrap();
                if mode.drop_fails() {
                    assert!(matches!(event.reason, parrot_api::supervisor::DeathReason::Panic(ref text) if text == "first lifecycle future cleanup"), "late cleanup reason lost: {:?}", event.reason);
                } else if stage == Stage::Stop { assert!(matches!(event.reason, parrot_api::supervisor::DeathReason::Killed)); }
                else { assert!(matches!(event.reason, parrot_api::supervisor::DeathReason::Normal)); }
            }
            tokio::time::timeout(Duration::from_secs(3), async {
                while state.actor_drops.load(Ordering::SeqCst) != 1 + usize::from(recovering) { tokio::task::yield_now().await; }
            }).await.expect("actor disposal never completed");
            assert_eq!(state.future_drops.load(Ordering::SeqCst), 1 + usize::from(recovering));
            assert_eq!(state.built.load(Ordering::SeqCst), 1 + usize::from(recovering), "failed cleanup allowed reconstruction");
            assert_eq!(state.dispatched.load(Ordering::SeqCst), usize::from(stage == Stage::Stop || recovering));
            assert_eq!(state.hooks.load(Ordering::SeqCst), usize::from(stage == Stage::Stop || recovering), "normal hook ran on incomplete initialization");
            if stage == Stage::Init && !mode.pending() {
                let expected = if matches!(mode, Mode::ReadyDrop) { "panic:first lifecycle future cleanup" } else { "business:original lifecycle business error" };
                assert_eq!(*state.policy.lock().unwrap(), [expected], "policy lost the original init failure or replayed it");
            } else { assert!(state.policy.lock().unwrap().is_empty(), "stop/cancel unexpectedly requested recovery"); }
            if gate.is_some() {
                let result = actor.stop().await;
                assert!(matches!(result, Err(ActorError::Timeout | ActorError::TimeoutDetail(_))), "late cleanup rewrote an already published timeout: {result:?}");
            }
            assert!(!actor.is_alive());
            drop(retained_watch);
        }).catch_unwind().await;
        if let Some(gate) = &gate {
            gate.release();
        }
        let _ = actor.stop().await;
        if let Some(system) = thread {
            let _ = system.shutdown_internal().await;
        }
        if let Some(system) = actix_system {
            let _ = system.as_ref().clone().shutdown().await;
        }
        if let Err(payload) = checked {
            std::panic::resume_unwind(payload);
        }
    }
    macro_rules! case {
        ($name:ident, $actix:expr, $factory:expr, $stage:ident, $mode:ident, $bomb:expr, $decision:ident, $recover:expr) => {
            #[test]
            fn $name() {
                super::isolated(
                    &format!("lifecycle_composition::{}", stringify!($name)),
                    || {
                        actix::System::new().block_on(exercise(
                            $actix,
                            $factory,
                            Stage::$stage,
                            Mode::$mode,
                            $bomb,
                            SupervisionDecision::$decision,
                            $recover,
                        ))
                    },
                );
            }
        };
    }
    case!(
        thread_instance_init_ready_future_drop,
        false,
        false,
        Init,
        ReadyDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_instance_init_ready_actor_drop,
        false,
        false,
        Init,
        ReadyDrop,
        true,
        Stop,
        false
    );
    case!(
        thread_instance_init_error_future_drop,
        false,
        false,
        Init,
        ErrorDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_instance_init_error_actor_drop,
        false,
        false,
        Init,
        ErrorDrop,
        true,
        Stop,
        false
    );
    case!(
        thread_instance_init_cancel_healthy,
        false,
        false,
        Init,
        Pending,
        false,
        Stop,
        false
    );
    case!(
        thread_instance_init_cancel_drop,
        false,
        false,
        Init,
        PendingDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_instance_stop_ready_future_drop,
        false,
        false,
        Stop,
        ReadyDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_instance_stop_ready_actor_drop,
        false,
        false,
        Stop,
        ReadyDrop,
        true,
        Stop,
        false
    );
    case!(
        thread_instance_stop_error_future_drop,
        false,
        false,
        Stop,
        ErrorDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_instance_stop_error_actor_drop,
        false,
        false,
        Stop,
        ErrorDrop,
        true,
        Stop,
        false
    );
    case!(
        thread_instance_stop_cancel_healthy,
        false,
        false,
        Stop,
        Pending,
        false,
        Stop,
        false
    );
    case!(
        thread_instance_stop_cancel_drop,
        false,
        false,
        Stop,
        PendingDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_instance_init_resume_failed_cleanup,
        false,
        false,
        Init,
        ErrorDrop,
        false,
        Resume,
        false
    );
    case!(
        thread_factory_init_ready_future_drop,
        false,
        true,
        Init,
        ReadyDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_factory_init_ready_actor_drop,
        false,
        true,
        Init,
        ReadyDrop,
        true,
        Stop,
        false
    );
    case!(
        thread_factory_init_error_future_drop,
        false,
        true,
        Init,
        ErrorDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_factory_init_error_actor_drop,
        false,
        true,
        Init,
        ErrorDrop,
        true,
        Stop,
        false
    );
    case!(
        thread_factory_init_cancel_healthy,
        false,
        true,
        Init,
        Pending,
        false,
        Stop,
        false
    );
    case!(
        thread_factory_init_cancel_drop,
        false,
        true,
        Init,
        PendingDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_factory_stop_ready_future_drop,
        false,
        true,
        Stop,
        ReadyDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_factory_stop_ready_actor_drop,
        false,
        true,
        Stop,
        ReadyDrop,
        true,
        Stop,
        false
    );
    case!(
        thread_factory_stop_error_future_drop,
        false,
        true,
        Stop,
        ErrorDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_factory_stop_error_actor_drop,
        false,
        true,
        Stop,
        ErrorDrop,
        true,
        Stop,
        false
    );
    case!(
        thread_factory_stop_cancel_healthy,
        false,
        true,
        Stop,
        Pending,
        false,
        Stop,
        false
    );
    case!(
        thread_factory_stop_cancel_drop,
        false,
        true,
        Stop,
        PendingDrop,
        false,
        Stop,
        false
    );
    case!(
        thread_factory_init_resume_failed_cleanup,
        false,
        true,
        Init,
        ErrorDrop,
        false,
        Resume,
        false
    );
    case!(
        thread_init_restart_healthy_cleanup,
        false,
        true,
        Init,
        ErrorOnly,
        false,
        Restart,
        true
    );
    case!(
        thread_init_restart_failed_cleanup,
        false,
        true,
        Init,
        ErrorDrop,
        false,
        Restart,
        true
    );
    case!(
        actix_instance_init_ready_future_drop,
        true,
        false,
        Init,
        ReadyDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_instance_init_ready_actor_drop,
        true,
        false,
        Init,
        ReadyDrop,
        true,
        Stop,
        false
    );
    case!(
        actix_instance_init_error_future_drop,
        true,
        false,
        Init,
        ErrorDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_instance_init_error_actor_drop,
        true,
        false,
        Init,
        ErrorDrop,
        true,
        Stop,
        false
    );
    case!(
        actix_instance_init_cancel_healthy,
        true,
        false,
        Init,
        Pending,
        false,
        Stop,
        false
    );
    case!(
        actix_instance_init_cancel_drop,
        true,
        false,
        Init,
        PendingDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_instance_stop_ready_future_drop,
        true,
        false,
        Stop,
        ReadyDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_instance_stop_ready_actor_drop,
        true,
        false,
        Stop,
        ReadyDrop,
        true,
        Stop,
        false
    );
    case!(
        actix_instance_stop_error_future_drop,
        true,
        false,
        Stop,
        ErrorDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_instance_stop_error_actor_drop,
        true,
        false,
        Stop,
        ErrorDrop,
        true,
        Stop,
        false
    );
    case!(
        actix_instance_stop_cancel_healthy,
        true,
        false,
        Stop,
        Pending,
        false,
        Stop,
        false
    );
    case!(
        actix_instance_stop_cancel_drop,
        true,
        false,
        Stop,
        PendingDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_instance_init_resume_failed_cleanup,
        true,
        false,
        Init,
        ErrorDrop,
        false,
        Resume,
        false
    );
    case!(
        actix_factory_init_ready_future_drop,
        true,
        true,
        Init,
        ReadyDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_factory_init_ready_actor_drop,
        true,
        true,
        Init,
        ReadyDrop,
        true,
        Stop,
        false
    );
    case!(
        actix_factory_init_error_future_drop,
        true,
        true,
        Init,
        ErrorDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_factory_init_error_actor_drop,
        true,
        true,
        Init,
        ErrorDrop,
        true,
        Stop,
        false
    );
    case!(
        actix_factory_init_cancel_healthy,
        true,
        true,
        Init,
        Pending,
        false,
        Stop,
        false
    );
    case!(
        actix_factory_init_cancel_drop,
        true,
        true,
        Init,
        PendingDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_factory_stop_ready_future_drop,
        true,
        true,
        Stop,
        ReadyDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_factory_stop_ready_actor_drop,
        true,
        true,
        Stop,
        ReadyDrop,
        true,
        Stop,
        false
    );
    case!(
        actix_factory_stop_error_future_drop,
        true,
        true,
        Stop,
        ErrorDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_factory_stop_error_actor_drop,
        true,
        true,
        Stop,
        ErrorDrop,
        true,
        Stop,
        false
    );
    case!(
        actix_factory_stop_cancel_healthy,
        true,
        true,
        Stop,
        Pending,
        false,
        Stop,
        false
    );
    case!(
        actix_factory_stop_cancel_drop,
        true,
        true,
        Stop,
        PendingDrop,
        false,
        Stop,
        false
    );
    case!(
        actix_factory_init_resume_failed_cleanup,
        true,
        true,
        Init,
        ErrorDrop,
        false,
        Resume,
        false
    );
    case!(
        actix_init_restart_healthy_cleanup,
        true,
        true,
        Init,
        ErrorOnly,
        false,
        Restart,
        true
    );
    case!(
        actix_init_restart_failed_cleanup,
        true,
        true,
        Init,
        ErrorDrop,
        false,
        Restart,
        true
    );
}

mod undelivered_reply {
    use super::*;
    use futures::FutureExt;
    use parrot_api::supervisor::DeathReason;
    #[derive(Clone, Copy)]
    enum Delivery {
        Delivered,
        Tell,
        Cancelled,
        Timeout,
    }
    #[derive(Default)]
    struct State {
        built: AtomicUsize,
        actors: AtomicUsize,
        replies: AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        disposed: tokio::sync::Notify,
    }
    #[derive(Debug)]
    struct Reply {
        state: Option<Arc<State>>,
        hostile: bool,
    }
    impl std::fmt::Debug for State {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("reply ownership counters")
        }
    }
    impl Drop for Reply {
        fn drop(&mut self) {
            if let Some(state) = &self.state {
                state.replies.fetch_add(1, Ordering::SeqCst);
                state.disposed.notify_one();
            }
            assert!(!self.hostile, "first undelivered reply disposal");
        }
    }
    struct Request(bool);
    impl Message for Request {
        type Result = Reply;
    }
    struct Owner {
        state: Arc<State>,
        reply_hostile: bool,
        actor_hostile: bool,
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            self.state.actors.fetch_add(1, Ordering::SeqCst);
            assert!(!self.actor_hostile, "later undelivered actor disposal");
        }
    }
    impl ParrotTypedDispatch for Owner {
        type Msg = Request;
        type Reply = Reply;
        fn dispatch(&mut self, request: Request) -> BoxedFuture<'_, ActorResult<Reply>> {
            Box::pin(async move {
                if request.0 {
                    self.state.entered.notify_one();
                    self.state.release.notified().await;
                    Ok(Reply {
                        state: Some(self.state.clone()),
                        hostile: self.reply_hostile,
                    })
                } else {
                    Ok(Reply {
                        state: None,
                        hostile: false,
                    })
                }
            })
        }
    }
    impl ParrotMsgVariant<Owner> for Request {
        fn inject(value: Self) -> Self {
            value
        }
        fn extract(value: Reply) -> ActorResult<Reply> {
            Ok(value)
        }
    }
    async fn exercise(
        actix: bool,
        factory: bool,
        delivery: Delivery,
        reply_hostile: bool,
        actor_hostile: bool,
    ) {
        let state = Arc::new(State::default());
        let make = {
            let state = state.clone();
            move || {
                state.built.fetch_add(1, Ordering::SeqCst);
                Owner {
                    state: state.clone(),
                    reply_hostile,
                    actor_hostile,
                }
            }
        };
        let policy = OneForOneStrategy {
            max_restarts: 1,
            within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
        };
        let (thread, actix_system, actor) = if actix {
            let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
            let actor = if factory {
                system
                    .spawn_typed_factory_with_strategy::<Owner, Request, _>(
                        make,
                        "/undelivered",
                        policy,
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed_with_strategy::<Owner, Request>(
                        make(),
                        "/undelivered",
                        SupervisorStrategyType::OneForOne(policy),
                    )
                    .await
                    .unwrap()
            };
            (None, Some(system), actor)
        } else {
            let system = ThreadActorSystem::shared(Default::default());
            let actor = if factory {
                system
                    .spawn_typed_factory_with_strategy::<Owner, Request, _>(
                        make,
                        "/undelivered",
                        policy,
                    )
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_typed_with_strategy::<Owner, Request>(
                        make(),
                        "/undelivered",
                        SupervisorStrategyType::OneForOne(policy),
                    )
                    .await
                    .unwrap()
            };
            (Some(system), None, actor)
        };
        let checked = std::panic::AssertUnwindSafe(async {
            actor.ask_with_timeout(Request(false), Duration::from_secs(3)).await.unwrap();
            let target = actor.clone().into_dyn().death_watch().unwrap();
            let watcher = parrot_api::death_watch::DeathWatch::new(1);
            let (events, mut notifications) = tokio::sync::mpsc::unbounded_channel();
            watcher.set_recipient(Arc::new(super::WatchSink(events))).unwrap();
            target.watch(&watcher).unwrap();
            match delivery {
                Delivery::Tell => {
                    actor.tell(Request(true)).await.unwrap();
                    tokio::time::timeout(Duration::from_secs(3), state.entered.notified()).await.unwrap();
                    state.release.notify_one();
                }
                Delivery::Cancelled | Delivery::Timeout | Delivery::Delivered => {
                    let mut request = Box::pin(actor.ask_unbounded(Request(true)));
                    tokio::select! {
                        biased;
                        result = &mut request => panic!("reply before release: {result:?}"),
                        _ = state.entered.notified() => {},
                        _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("dispatch never entered"),
                    }
                    match delivery {
                        Delivery::Cancelled => drop(request),
                        Delivery::Timeout => {
                            assert!(tokio::time::timeout(Duration::from_millis(30), request).await.is_err());
                        }
                        Delivery::Delivered => {
                            state.release.notify_one();
                            let reply = tokio::time::timeout(Duration::from_secs(3), request).await.unwrap().unwrap();
                            assert_eq!(state.replies.load(Ordering::SeqCst), 0, "caller-owned reply dropped by runtime");
                            let disposed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(reply)));
                            assert_eq!(disposed.is_err(), reply_hostile, "caller controls delivered reply destruction");
                        }
                        Delivery::Tell => unreachable!(),
                    }
                    state.release.notify_one();
                }
            }
            tokio::time::timeout(Duration::from_secs(3), state.disposed.notified()).await.unwrap();
            let runtime_cleanup_failed = reply_hostile && !matches!(delivery, Delivery::Delivered);
            if !runtime_cleanup_failed {
                actor.ask_with_timeout(Request(false), Duration::from_secs(3)).await.expect("healthy disposal must preserve actor progress");
            } else {
                let event = tokio::time::timeout(Duration::from_secs(3), notifications.recv()).await.expect("failed reply cleanup must retire generation").unwrap();
                assert!(matches!(event.reason, DeathReason::Panic(ref text) if text.contains("first undelivered reply disposal")), "first reply cleanup lost in death watch: {:?}", event.reason);
            }
            let stopped = tokio::time::timeout(Duration::from_secs(4), actor.stop()).await.expect("reply cleanup lost stop progress");
            if runtime_cleanup_failed {
                let error = stopped.expect_err("failed reply disposal masked by stop success");
                assert!(error.to_string().contains("first undelivered reply disposal"), "first reply cleanup lost in stop: {error}");
                assert!(matches!(actor.ask_with_timeout(Request(false), Duration::from_secs(3)).await, Err(ActorError::Stopped)), "failed reply cleanup reconstructed actor");
            } else {
                stopped.unwrap();
                let event = tokio::time::timeout(Duration::from_secs(3), notifications.recv()).await.unwrap().unwrap();
                assert!(matches!(event.reason, DeathReason::Normal), "healthy retirement: {:?}", event.reason);
            }
            assert_eq!(state.built.load(Ordering::SeqCst), 1, "cleanup failure cannot reconstruct");
            assert_eq!(state.actors.load(Ordering::SeqCst), 1);
            assert_eq!(state.replies.load(Ordering::SeqCst), 1);
        }).catch_unwind().await;
        state.release.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(4), actor.stop()).await;
        if let Some(system) = thread {
            let _ = system.shutdown_internal().await;
        }
        if let Some(system) = actix_system {
            let _ = system.as_ref().clone().shutdown().await;
        }
        if let Err(payload) = checked {
            std::panic::resume_unwind(payload);
        }
    }
    macro_rules! case {
        ($name:ident, $actix:expr, $factory:expr, $delivery:ident, $reply:expr, $actor:expr) => {
            #[test]
            fn $name() {
                super::isolated(&format!("undelivered_reply::{}", stringify!($name)), || {
                    actix::System::new().block_on(exercise(
                        $actix,
                        $factory,
                        Delivery::$delivery,
                        $reply,
                        $actor,
                    ))
                });
            }
        };
    }
    case!(
        thread_instance_healthy_tell,
        false,
        false,
        Tell,
        false,
        false
    );
    case!(
        thread_instance_healthy_cancel,
        false,
        false,
        Cancelled,
        false,
        false
    );
    case!(
        thread_instance_healthy_timeout,
        false,
        false,
        Timeout,
        false,
        false
    );
    case!(
        thread_instance_delivered_hostile,
        false,
        false,
        Delivered,
        true,
        false
    );
    case!(thread_instance_reply_tell, false, false, Tell, true, false);
    case!(
        thread_instance_reply_cancel,
        false,
        false,
        Cancelled,
        true,
        false
    );
    case!(
        thread_instance_reply_timeout,
        false,
        false,
        Timeout,
        true,
        false
    );
    case!(
        thread_instance_reply_actor_tell,
        false,
        false,
        Tell,
        true,
        true
    );
    case!(
        thread_instance_reply_actor_cancel,
        false,
        false,
        Cancelled,
        true,
        true
    );
    case!(
        thread_instance_reply_actor_timeout,
        false,
        false,
        Timeout,
        true,
        true
    );
    case!(thread_factory_healthy_tell, false, true, Tell, false, false);
    case!(
        thread_factory_healthy_cancel,
        false,
        true,
        Cancelled,
        false,
        false
    );
    case!(
        thread_factory_healthy_timeout,
        false,
        true,
        Timeout,
        false,
        false
    );
    case!(
        thread_factory_delivered_hostile,
        false,
        true,
        Delivered,
        true,
        false
    );
    case!(thread_factory_reply_tell, false, true, Tell, true, false);
    case!(
        thread_factory_reply_cancel,
        false,
        true,
        Cancelled,
        true,
        false
    );
    case!(
        thread_factory_reply_timeout,
        false,
        true,
        Timeout,
        true,
        false
    );
    case!(
        thread_factory_reply_actor_tell,
        false,
        true,
        Tell,
        true,
        true
    );
    case!(
        thread_factory_reply_actor_cancel,
        false,
        true,
        Cancelled,
        true,
        true
    );
    case!(
        thread_factory_reply_actor_timeout,
        false,
        true,
        Timeout,
        true,
        true
    );
    case!(actix_instance_healthy_tell, true, false, Tell, false, false);
    case!(
        actix_instance_healthy_cancel,
        true,
        false,
        Cancelled,
        false,
        false
    );
    case!(
        actix_instance_healthy_timeout,
        true,
        false,
        Timeout,
        false,
        false
    );
    case!(
        actix_instance_delivered_hostile,
        true,
        false,
        Delivered,
        true,
        false
    );
    case!(actix_instance_reply_tell, true, false, Tell, true, false);
    case!(
        actix_instance_reply_cancel,
        true,
        false,
        Cancelled,
        true,
        false
    );
    case!(
        actix_instance_reply_timeout,
        true,
        false,
        Timeout,
        true,
        false
    );
    case!(
        actix_instance_reply_actor_tell,
        true,
        false,
        Tell,
        true,
        true
    );
    case!(
        actix_instance_reply_actor_cancel,
        true,
        false,
        Cancelled,
        true,
        true
    );
    case!(
        actix_instance_reply_actor_timeout,
        true,
        false,
        Timeout,
        true,
        true
    );
    case!(actix_factory_healthy_tell, true, true, Tell, false, false);
    case!(
        actix_factory_healthy_cancel,
        true,
        true,
        Cancelled,
        false,
        false
    );
    case!(
        actix_factory_healthy_timeout,
        true,
        true,
        Timeout,
        false,
        false
    );
    case!(
        actix_factory_delivered_hostile,
        true,
        true,
        Delivered,
        true,
        false
    );
    case!(actix_factory_reply_tell, true, true, Tell, true, false);
    case!(
        actix_factory_reply_cancel,
        true,
        true,
        Cancelled,
        true,
        false
    );
    case!(
        actix_factory_reply_timeout,
        true,
        true,
        Timeout,
        true,
        false
    );
    case!(actix_factory_reply_actor_tell, true, true, Tell, true, true);
    case!(
        actix_factory_reply_actor_cancel,
        true,
        true,
        Cancelled,
        true,
        true
    );
    case!(
        actix_factory_reply_actor_timeout,
        true,
        true,
        Timeout,
        true,
        true
    );
}
