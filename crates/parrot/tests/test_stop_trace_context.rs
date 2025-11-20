//! Stop ownership must carry the first admitted caller through real hooks.
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalTypedActorRef, LocalTypedDispatch},
    message::Message,
    trace::{SpanStatus, TraceContext, TraceRecorder},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_channel::TypedActorRef,
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug)]
enum Exit {
    Success,
    Error,
    Panic,
    ConstructPanic,
    DropPanic,
    Timeout,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Init,
    LocalStreams,
    Stop,
}
struct State {
    events: Mutex<Vec<(&'static str, Option<TraceContext>)>>,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    disposed: tokio::sync::Semaphore,
    exit: Exit,
    stage: Stage,
    stops: AtomicUsize,
    fail_dispatch: AtomicBool,
}
impl State {
    fn dispatch(self: &Arc<Self>) -> BoxedFuture<'static, ActorResult<()>> {
        let fail = self.fail_dispatch.swap(false, Ordering::SeqCst);
        Box::pin(async move {
            if fail {
                Err(ActorError::InternalError(
                    "restart recording request".into(),
                ))
            } else {
                Ok(())
            }
        })
    }
    fn init(self: &Arc<Self>) -> BoxedFuture<'static, ActorResult<()>> {
        if self.stage == Stage::Init {
            self.hook()
        } else {
            Box::pin(async { Ok(()) })
        }
    }
    fn stop(self: &Arc<Self>) -> BoxedFuture<'static, ActorResult<()>> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.stage == Stage::Stop {
            self.hook()
        } else {
            Box::pin(async { Ok(()) })
        }
    }
    fn record(&self, stage: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((stage, TraceContext::current()));
    }
    fn hook(self: &Arc<Self>) -> BoxedFuture<'static, ActorResult<()>> {
        self.record("construct");
        let guard = Guard(self.clone());
        assert!(
            !matches!(self.exit, Exit::ConstructPanic),
            "stop trace construction panic"
        );
        Box::pin(async move {
            guard.0.record("poll");
            guard.0.entered.add_permits(1);
            guard.0.release.acquire().await.unwrap().forget();
            guard.0.record("resume");
            match guard.0.exit {
                Exit::Success | Exit::DropPanic => Ok(()),
                Exit::Error => Err(ActorError::InternalError("stop trace error".into())),
                Exit::Panic => panic!("stop trace panic"),
                Exit::ConstructPanic | Exit::Timeout => unreachable!("hook must not resume"),
            }
        })
    }
}
struct Guard(Arc<State>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.record("drop");
        self.0.disposed.add_permits(1);
        assert!(
            !matches!(self.0.exit, Exit::DropPanic),
            "stop trace disposal panic"
        );
    }
}
struct Ready;
impl Message for Ready {
    type Result = ();
}
struct Probe(Arc<State>);
impl ParrotTypedDispatch for Probe {
    type Msg = Ready;
    type Reply = ();
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.0.init()
    }
    fn dispatch(&mut self, _: Ready) -> BoxedFuture<'_, ActorResult<()>> {
        self.0.dispatch()
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.0.stop()
    }
}
impl ParrotMsgVariant<Probe> for Ready {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
struct LocalProbe(Arc<State>, Rc<()>);
impl LocalTypedDispatch for LocalProbe {
    type Msg = Ready;
    type Reply = ();
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.0.init()
    }
    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        _: &'a mut parrot_api::local_typed::LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        if self.0.stage == Stage::LocalStreams {
            self.0.hook()
        } else {
            Box::pin(async { Ok(()) })
        }
    }
    fn dispatch<'a>(
        &'a mut self,
        _: Ready,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.0.dispatch()
    }
    fn before_stop<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        let held = self.1.clone();
        let future = self.0.stop();
        Box::pin(async move {
            let _held = held;
            future.await
        })
    }
}
impl ParrotMsgVariant<LocalProtocol<LocalProbe>> for Ready {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
struct ThreadProbe(Arc<State>);
impl Actor for ThreadProbe {
    type Config = EmptyConfig;
    type Context = parrot::thread::ThreadContext<Self>;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.init()
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let work = self.0.dispatch();
        Box::pin(async move {
            work.await?;
            Ok(Box::new(()) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.stop()
    }
}
struct ActixProbe(Arc<State>);
impl Actor for ActixProbe {
    type Config = EmptyConfig;
    type Context = parrot::actix::ActixContext<parrot::actix::ActixActor<Self>>;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.init()
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let work = self.0.dispatch();
        Box::pin(async move {
            work.await?;
            Ok(Box::new(()) as BoxedMessage)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.stop()
    }
}
impl parrot_api::actor::ActixEngineExt for ActixProbe {}
enum Reference {
    Dynamic(BoxedActorRef),
    Typed(TypedActorRef<Probe, Ready>),
    Local(LocalTypedActorRef<LocalProbe, Ready>),
    ThreadSystem(
        Arc<parrot::thread::ThreadActorSystem>,
        BoxedActorRef,
        String,
    ),
    ActixSystem(Arc<parrot::actix::ActixActorSystem>, BoxedActorRef, String),
}
impl Reference {
    fn path(&self) -> String {
        match self {
            Self::Dynamic(actor) => actor.path(),
            Self::Typed(actor) => actor.path().to_owned(),
            Self::Local(actor) => actor.path().to_owned(),
            Self::ThreadSystem(_, _, path) | Self::ActixSystem(_, _, path) => path.clone(),
        }
    }
    fn erased(self) -> Self {
        Self::Dynamic(match self {
            Self::Dynamic(actor) => actor,
            Self::Typed(actor) => actor.into_dyn(),
            Self::Local(actor) => actor.into_dyn(),
            _ => unreachable!(),
        })
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        match self {
            Self::Dynamic(actor) => actor.stop(),
            Self::Typed(actor) => Box::pin(actor.stop()),
            Self::Local(actor) => Box::pin(actor.stop()),
            Self::ThreadSystem(system, _, path) => Box::pin(async_error(system.stop_actor(path))),
            Self::ActixSystem(system, _, path) => Box::pin(system.stop_actor(path)),
        }
    }
    async fn ready(&self) {
        bounded(self.send_ready()).await.unwrap();
    }
    async fn send_ready(&self) -> ActorResult<()> {
        match self {
            Self::Dynamic(actor)
            | Self::ThreadSystem(_, actor, _)
            | Self::ActixSystem(_, actor, _) => actor.send(Box::new(Ready)).await.map(|_| ()),
            Self::Typed(actor) => actor.ask(Ready).await,
            Self::Local(actor) => actor.ask(Ready).await,
        }
    }
    async fn retired(&self) {
        bounded(async {
            loop {
                let status = match self {
                    Self::Dynamic(actor)
                    | Self::ThreadSystem(_, actor, _)
                    | Self::ActixSystem(_, actor, _) => actor.retirement_status(),
                    Self::Typed(actor) => actor.retirement_status(),
                    Self::Local(actor) => actor.retirement_status(),
                };
                match status {
                    parrot_api::address::RetirementStatus::Retired(_) => break,
                    parrot_api::address::RetirementStatus::Pending => {
                        tokio::task::yield_now().await
                    }
                    parrot_api::address::RetirementStatus::Unsupported => {
                        panic!("real engine reference must expose retirement")
                    }
                }
            }
        })
        .await;
    }
}
async fn async_error(
    future: impl Future<Output = Result<(), parrot::thread::SystemError>>,
) -> ActorResult<()> {
    future.await.map_err(ActorError::from)
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("stop trace contract deadline")
}
async fn contract(reference: Reference, state: Arc<State>, traced: bool, cancel_waiter: bool) {
    // Neither actor creation nor an earlier message may supply the stop identity.
    TraceContext::root().scope(reference.ready()).await;
    let recorder = TraceRecorder::new(16).unwrap();
    let parent = traced.then(|| recorder.root());
    let target = reference.path();
    let poller = TraceContext::root();
    // Constructing and dropping a future is not stop admission.
    drop(poller.in_scope(|| reference.stop()));
    let mut stop = parrot_api::trace::in_scope(parent.clone(), || reference.stop());
    if matches!(state.exit, Exit::ConstructPanic) {
        let error = bounded(poller.scope(stop)).await.unwrap_err();
        assert!(error.to_string().contains("stop trace construction panic"));
        check(
            &state,
            parent,
            &["construct", "drop"],
            &recorder,
            &target,
            "ActorBeforeStop",
            None,
        )
        .await;
        return;
    }
    assert!(
        poller
            .scope(std::future::poll_fn(|cx| std::task::Poll::Ready(
                stop.as_mut().poll(cx)
            )))
            .await
            .is_pending()
    );
    bounded(state.entered.acquire()).await.unwrap().forget();
    let pending = recorder.snapshot();
    assert_eq!(pending.counts.started, u64::from(traced));
    assert_eq!(pending.counts.in_flight, u64::from(traced));
    assert!(
        pending.records.is_empty(),
        "a suspended hook is not complete"
    );
    if cancel_waiter {
        drop(stop);
        stop = poller.in_scope(|| reference.stop());
        assert!(futures::poll!(stop.as_mut()).is_pending());
    }
    if !matches!(state.exit, Exit::Timeout) {
        state.release.add_permits(1);
    }
    let result = bounded(poller.scope(stop)).await;
    match state.exit {
        Exit::Success => result.unwrap(),
        Exit::Error => assert!(result.unwrap_err().to_string().contains("stop trace error")),
        Exit::Panic => assert!(result.unwrap_err().to_string().contains("stop trace panic")),
        Exit::DropPanic => assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("stop trace disposal panic")
        ),
        Exit::Timeout => assert!(matches!(
            result,
            Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
        )),
        Exit::ConstructPanic => unreachable!(),
    }
    // A timeout receipt alone does not prove the hook's storage was retired.
    bounded(state.disposed.acquire()).await.unwrap().forget();
    let stages: &[&str] = if matches!(state.exit, Exit::Timeout) {
        &["construct", "poll", "drop"]
    } else {
        &["construct", "poll", "resume", "drop"]
    };
    check(
        &state,
        parent,
        stages,
        &recorder,
        &target,
        "ActorBeforeStop",
        None,
    )
    .await;
}
async fn check(
    state: &State,
    parent: Option<TraceContext>,
    stages: &[&str],
    recorder: &TraceRecorder,
    target: &str,
    kind: &str,
    other: Option<&str>,
) {
    recorder.close();
    bounded(async {
        while !recorder.snapshot().terminal {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let events = state.events.lock().unwrap().clone();
    assert_eq!(
        events.iter().map(|event| event.0).collect::<Vec<_>>(),
        stages
    );
    let child = events[0].1.clone();
    match (&child, &parent) {
        (Some(child), Some(parent)) => {
            assert_eq!(child.trace_id(), parent.trace_id());
            assert_eq!(child.parent_span_id(), Some(parent.span_id()));
            assert_ne!(child.span_id(), parent.span_id());
        }
        (None, None) => {}
        mismatch => panic!("hook lost admitted context: {mismatch:?}"),
    }
    assert!(
        events.iter().all(|event| event.1 == child),
        "hook context changed across construction, polls or disposal: {events:?}"
    );
    let snapshot = recorder.drain();
    let count = u64::from(parent.is_some()) * (1 + u64::from(other.is_some()));
    assert_eq!(snapshot.counts.started, count);
    assert_eq!(snapshot.counts.completed, count);
    assert_eq!(snapshot.counts.overwritten, 0);
    assert_eq!(snapshot.records.len() as u64, count);
    if let Some(other) = other.filter(|_| parent.is_some()) {
        let record = snapshot
            .records
            .iter()
            .find(|record| format!("{:?}", record.kind) == other)
            .expect("missing independent local hook");
        assert_eq!(record.target, target);
        assert_eq!(record.status, SpanStatus::Ok);
        assert_eq!(record.trace_id, parent.as_ref().unwrap().trace_id());
        assert_eq!(
            record.parent_span_id,
            Some(parent.as_ref().unwrap().span_id())
        );
        assert_ne!(record.span_id, child.as_ref().unwrap().span_id());
    }
    if parent.is_some() {
        let record = snapshot
            .records
            .iter()
            .find(|record| format!("{:?}", record.kind) == kind)
            .expect("missing actual hook record");
        let child = child.unwrap();
        assert_eq!(format!("{:?}", record.kind), kind);
        assert_eq!(record.target, target);
        assert_eq!(record.trace_id, child.trace_id());
        assert_eq!(record.span_id, child.span_id());
        assert_eq!(record.parent_span_id, child.parent_span_id());
        assert_eq!(
            record.status,
            match state.exit {
                Exit::Success => SpanStatus::Ok,
                Exit::Error => SpanStatus::Error,
                Exit::Panic | Exit::ConstructPanic | Exit::DropPanic => SpanStatus::Panicked,
                Exit::Timeout => SpanStatus::Cancelled,
            }
        );
    }
    assert!(TraceContext::current().is_none());
}
fn state(exit: Exit) -> Arc<State> {
    Arc::new(State {
        events: Mutex::new(Vec::new()),
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
        disposed: tokio::sync::Semaphore::new(0),
        exit,
        stage: Stage::Stop,
        stops: AtomicUsize::new(0),
        fail_dispatch: AtomicBool::new(false),
    })
}
async fn thread_contract(kind: usize) {
    let system =
        parrot::thread::ThreadActorSystem::shared(parrot::thread::ThreadActorSystemConfig {
            shutdown_timeout: Duration::from_millis(500),
            ..Default::default()
        });
    for (id, (entry, traced, cancel, exit)) in scenarios().into_iter().enumerate() {
        let state = state(exit);
        let path = format!("/stop-{id}");
        let creation = TraceContext::root();
        let reference = creation
            .scope(spawn_thread(&system, kind, &path, state.clone()))
            .await;
        let reference = match entry {
            0 => reference,
            1 => reference.erased(),
            2 => {
                let Reference::Dynamic(actor) = reference.erased() else {
                    unreachable!()
                };
                Reference::ThreadSystem(system.clone(), actor, path.clone())
            }
            _ => unreachable!(),
        };
        let registered_path = reference.path();
        contract(reference, state, traced, cancel).await;
        bounded(async {
            while system.get_actor_ref(&registered_path).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    bounded(system.shutdown_internal()).await.unwrap();
}
fn scenarios() -> Vec<(usize, bool, bool, Exit)> {
    (0..3)
        .flat_map(|entry| {
            [
                (true, false, Exit::Success),
                (false, false, Exit::Success),
                (true, true, Exit::Success),
                (false, true, Exit::Success),
                (true, false, Exit::Error),
                (true, false, Exit::Panic),
                (true, false, Exit::ConstructPanic),
                (true, false, Exit::DropPanic),
                (true, false, Exit::Timeout),
            ]
            .into_iter()
            .map(move |(traced, cancel, exit)| (entry, traced, cancel, exit))
        })
        .collect()
}
async fn actix_contract(kind: usize) {
    let system = Arc::new(
        parrot::actix::ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: Duration::from_millis(500),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    for (id, (entry, traced, cancel, exit)) in scenarios().into_iter().enumerate() {
        let state = state(exit);
        let path = format!("/stop-{id}");
        let creation = TraceContext::root();
        let reference = creation
            .scope(spawn_actix(&system, kind, &path, state.clone()))
            .await;
        let reference = match entry {
            0 => reference,
            1 => reference.erased(),
            2 => {
                let Reference::Dynamic(actor) = reference.erased() else {
                    unreachable!()
                };
                let path = actor.path();
                Reference::ActixSystem(system.clone(), actor, path)
            }
            _ => unreachable!(),
        };
        let registered_path = reference.path();
        contract(reference, state, traced, cancel).await;
        bounded(async {
            while system.resolve_actor(&registered_path).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    bounded(system.as_ref().clone().shutdown()).await.unwrap();
}

async fn spawn_thread(
    system: &Arc<parrot::thread::ThreadActorSystem>,
    kind: usize,
    path: &str,
    state: Arc<State>,
) -> Reference {
    match kind {
        0..=2 => Reference::Dynamic(Box::new(
            system
                .spawn_at(
                    ThreadProbe(state),
                    path,
                    None,
                    parrot::thread::ThreadActorConfig {
                        scheduling_mode: Some(match kind {
                            0 => parrot::thread::SchedulingMode::SharedPool {
                                max_messages_per_run: 10,
                            },
                            1 => parrot::thread::SchedulingMode::DedicatedThread,
                            _ => parrot::thread::SchedulingMode::Sharded {
                                affinity_key: "lifecycle-trace".into(),
                            },
                        }),
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
        )),
        3 => Reference::Typed(system.spawn_typed(Probe(state), path).await.unwrap()),
        4 => Reference::Typed(
            system
                .spawn_typed_factory_with_strategy(
                    move || Probe(state.clone()),
                    path,
                    stop_policy(),
                )
                .await
                .unwrap(),
        ),
        _ => unreachable!(),
    }
}
async fn spawn_actix(
    system: &Arc<parrot::actix::ActixActorSystem>,
    kind: usize,
    path: &str,
    state: Arc<State>,
) -> Reference {
    match kind {
        0 => Reference::Dynamic(
            system
                .spawn_root_typed(ActixProbe(state), EmptyConfig)
                .await
                .unwrap(),
        ),
        1 => Reference::Dynamic(
            system
                .spawn_root_factory_with_strategy(move || ActixProbe(state.clone()), stop_policy())
                .await
                .unwrap(),
        ),
        2 => Reference::Typed(system.spawn_typed(Probe(state), path).await.unwrap()),
        3 => Reference::Typed(
            system
                .spawn_typed_factory_with_strategy(
                    move || Probe(state.clone()),
                    path,
                    stop_policy(),
                )
                .await
                .unwrap(),
        ),
        4 => Reference::Local(
            system
                .spawn_local_typed(move || LocalProbe(state.clone(), Rc::new(())), path)
                .await
                .unwrap(),
        ),
        5 => Reference::Local(
            system
                .spawn_local_typed_factory_with_strategy(
                    move || LocalProbe(state.clone(), Rc::new(())),
                    path,
                    stop_policy(),
                )
                .await
                .unwrap(),
        ),
        _ => unreachable!(),
    }
}

fn stop_policy() -> parrot_api::supervisor::OneForOneStrategy {
    parrot_api::supervisor::OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(60),
        decider: parrot_api::supervisor::BasicDecisionFn::new(|_| {
            parrot_api::supervisor::SupervisionDecision::Stop
        }),
    }
}

async fn init_contract(
    reference: Reference,
    state: Arc<State>,
    parent: Option<TraceContext>,
    recorder: TraceRecorder,
) {
    let target = reference.path();
    let local = matches!(&reference, Reference::Local(_));
    if !matches!(state.exit, Exit::ConstructPanic) {
        bounded(state.entered.acquire()).await.unwrap().forget();
        let pending = recorder.snapshot();
        assert_eq!(pending.counts.in_flight, u64::from(parent.is_some()));
        if let (Stage::LocalStreams, Some(parent)) = (state.stage, &parent) {
            assert_eq!(pending.records.len(), 1);
            assert_eq!(format!("{:?}", pending.records[0].kind), "ActorInit");
            assert_eq!(pending.records[0].status, SpanStatus::Ok);
            assert_eq!(pending.records[0].parent_span_id, Some(parent.span_id()));
            assert_ne!(
                pending.records[0].span_id,
                state.events.lock().unwrap()[0]
                    .1
                    .as_ref()
                    .unwrap()
                    .span_id()
            );
        }
        if !matches!(state.exit, Exit::Timeout) {
            state.release.add_permits(1);
        }
    }
    let succeeded = matches!(state.exit, Exit::Success);
    if succeeded {
        bounded(reference.ready()).await;
        bounded(reference.stop()).await.unwrap();
    } else if matches!(state.exit, Exit::Timeout) {
        // Stop cancels a genuinely suspended initializer. The normal stop hook
        // must never run for this generation.
        bounded(reference.stop()).await.unwrap();
    } else {
        let result = bounded(reference.send_ready()).await;
        assert!(
            result.is_err(),
            "a failed initialization must not process business messages"
        );
    }
    bounded(state.disposed.acquire()).await.unwrap().forget();
    reference.retired().await;
    let stages: &[&str] = match state.exit {
        Exit::ConstructPanic => &["construct", "drop"],
        Exit::Timeout => &["construct", "poll", "drop"],
        _ => &["construct", "poll", "resume", "drop"],
    };
    let kind = if state.stage == Stage::Init {
        "ActorInit"
    } else {
        "ActorInitLocalStreams"
    };
    let other = if state.stage == Stage::LocalStreams {
        Some("ActorInit")
    } else if local && succeeded {
        Some("ActorInitLocalStreams")
    } else {
        None
    };
    check(&state, parent, stages, &recorder, &target, kind, other).await;
    assert_eq!(
        state.stops.load(Ordering::SeqCst),
        usize::from(succeeded),
        "failed init must skip before_stop"
    );
}

fn init_scenarios() -> Vec<(bool, Exit)> {
    [true, false]
        .into_iter()
        .flat_map(|traced| {
            [
                Exit::Success,
                Exit::Error,
                Exit::Panic,
                Exit::ConstructPanic,
                Exit::DropPanic,
                Exit::Timeout,
            ]
            .into_iter()
            .map(move |exit| (traced, exit))
        })
        .collect()
}
async fn thread_init_contract(kind: usize) {
    let system = parrot::thread::ThreadActorSystem::shared(Default::default());
    for (id, (traced, exit)) in init_scenarios().into_iter().enumerate() {
        eprintln!("LIFECYCLE_INIT_CASE thread kind={kind} traced={traced} exit={exit:?}");
        let mut state = state(exit);
        Arc::get_mut(&mut state).unwrap().stage = Stage::Init;
        let recorder = TraceRecorder::new(16).unwrap();
        let parent = traced.then(|| recorder.root());
        let path = format!("/init-{id}");
        let reference = parrot_api::trace::scope(
            parent.clone(),
            spawn_thread(&system, kind, &path, state.clone()),
        )
        .await;
        init_contract(reference, state, parent, recorder).await;
        bounded(async {
            while system.get_actor_ref(&path).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    bounded(system.shutdown_internal()).await.unwrap();
}
async fn actix_init_contract(kind: usize, stage: Stage) {
    let system = Arc::new(
        parrot::actix::ActixActorSystem::with_arbiter_count(1)
            .await
            .unwrap(),
    );
    for (id, (traced, exit)) in init_scenarios().into_iter().enumerate() {
        eprintln!(
            "LIFECYCLE_INIT_CASE actix kind={kind} stage={stage:?} traced={traced} exit={exit:?}"
        );
        let mut state = state(exit);
        Arc::get_mut(&mut state).unwrap().stage = stage;
        let recorder = TraceRecorder::new(16).unwrap();
        let parent = traced.then(|| recorder.root());
        let reference = parrot_api::trace::scope(
            parent.clone(),
            spawn_actix(&system, kind, &format!("/init-{id}"), state.clone()),
        )
        .await;
        let path = reference.path();
        init_contract(reference, state, parent, recorder).await;
        bounded(async {
            while system.resolve_actor(&path).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
    bounded(system.as_ref().clone().shutdown()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_shared_init_recording() {
    thread_init_contract(0).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_dedicated_init_recording() {
    thread_init_contract(1).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_sharded_init_recording() {
    thread_init_contract(2).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_typed_init_recording() {
    thread_init_contract(3).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_typed_factory_init_recording() {
    thread_init_contract(4).await;
}
#[actix::test]
async fn actix_dynamic_init_recording() {
    actix_init_contract(0, Stage::Init).await;
}
#[actix::test]
async fn actix_dynamic_factory_init_recording() {
    actix_init_contract(1, Stage::Init).await;
}
#[actix::test]
async fn actix_typed_init_recording() {
    actix_init_contract(2, Stage::Init).await;
}
#[actix::test]
async fn actix_typed_factory_init_recording() {
    actix_init_contract(3, Stage::Init).await;
}
#[actix::test]
async fn actix_local_init_recording() {
    actix_init_contract(4, Stage::Init).await;
}
#[actix::test]
async fn actix_local_factory_init_recording() {
    actix_init_contract(5, Stage::Init).await;
}
#[actix::test]
async fn actix_local_streams_init_recording() {
    actix_init_contract(4, Stage::LocalStreams).await;
}
#[actix::test]
async fn actix_local_factory_streams_init_recording() {
    actix_init_contract(5, Stage::LocalStreams).await;
}

async fn restart_contract(kind: usize) {
    use parrot_api::supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision};
    let thread = (kind == 0).then(|| parrot::thread::ThreadActorSystem::shared(Default::default()));
    let actix = if kind > 0 {
        Some(Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        ))
    } else {
        None
    };
    let mut state = state(Exit::Success);
    Arc::get_mut(&mut state).unwrap().stage = Stage::Init;
    state.release.add_permits(2);
    let recorder = TraceRecorder::new(16).unwrap();
    let creation = recorder.root();
    let made = state.clone();
    let policy = OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(60),
        decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
    };
    let reference = creation
        .scope(async {
            match kind {
                0 => Reference::Typed(
                    thread
                        .as_ref()
                        .unwrap()
                        .spawn_typed_factory_with_strategy(
                            move || Probe(made.clone()),
                            "/restart",
                            policy,
                        )
                        .await
                        .unwrap(),
                ),
                1 => Reference::Typed(
                    actix
                        .as_ref()
                        .unwrap()
                        .spawn_typed_factory_with_strategy(
                            move || Probe(made.clone()),
                            "/restart",
                            policy,
                        )
                        .await
                        .unwrap(),
                ),
                2 => Reference::Local(
                    actix
                        .as_ref()
                        .unwrap()
                        .spawn_local_typed_factory_with_strategy(
                            move || LocalProbe(made.clone(), Rc::new(())),
                            "/restart",
                            policy,
                        )
                        .await
                        .unwrap(),
                ),
                3 => Reference::Dynamic(
                    actix
                        .as_ref()
                        .unwrap()
                        .spawn_root_factory_with_strategy(move || ActixProbe(made.clone()), policy)
                        .await
                        .unwrap(),
                ),
                _ => unreachable!(),
            }
        })
        .await;
    reference.ready().await;
    state.fail_dispatch.store(true, Ordering::SeqCst);
    let failure = recorder.root();
    let error = failure
        .scope(async { reference.send_ready().await })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("restart recording request"));
    // The dynamic factory facade can reject delivery between generations.
    // Synchronize on the actual replacement hook before the next business call.
    bounded(state.entered.acquire_many(2))
        .await
        .unwrap()
        .forget();
    reference.ready().await;
    assert_eq!(
        state.stops.load(Ordering::SeqCst),
        0,
        "failed generation must skip normal stop hook"
    );
    let stop = recorder.root();
    bounded(stop.scope(async { reference.stop().await }))
        .await
        .unwrap();
    reference.retired().await;
    assert_eq!(state.stops.load(Ordering::SeqCst), 1);
    recorder.close();
    let snapshot = recorder.drain();
    assert!(snapshot.terminal);
    assert_eq!(snapshot.counts.overwritten, 0);
    assert_eq!(snapshot.records.len(), if kind == 2 { 6 } else { 4 });
    let inits: Vec<_> = snapshot
        .records
        .iter()
        .filter(|record| format!("{:?}", record.kind) == "ActorInit")
        .collect();
    assert_eq!(inits.len(), 2);
    assert_eq!(inits[0].parent_span_id, Some(creation.span_id()));
    let handler = snapshot
        .records
        .iter()
        .find(|record| record.status == SpanStatus::Error)
        .unwrap();
    assert_eq!(handler.parent_span_id, Some(failure.span_id()));
    assert_eq!(inits[1].trace_id, handler.trace_id);
    assert_eq!(inits[1].parent_span_id, Some(handler.span_id));
    assert_ne!(inits[0].span_id, inits[1].span_id);
    let events = state.events.lock().unwrap().clone();
    assert_eq!(events.len(), 8);
    for (chunk, record) in events.as_chunks::<4>().0.iter().zip(inits) {
        assert_eq!(
            chunk.iter().map(|event| event.0).collect::<Vec<_>>(),
            ["construct", "poll", "resume", "drop"]
        );
        assert_eq!(record.status, SpanStatus::Ok);
        assert!(chunk.iter().all(|event| {
            let observed = event.1.as_ref().unwrap();
            observed.trace_id() == record.trace_id
                && observed.span_id() == record.span_id
                && observed.parent_span_id() == record.parent_span_id
        }));
    }
    let hook = snapshot
        .records
        .iter()
        .find(|record| format!("{:?}", record.kind) == "ActorBeforeStop")
        .unwrap();
    assert_eq!(hook.parent_span_id, Some(stop.span_id()));
    assert_eq!(hook.trace_id, stop.trace_id());
    assert_eq!(hook.status, SpanStatus::Ok);
    if let Some(system) = thread {
        bounded(system.shutdown_internal()).await.unwrap();
    }
    if let Some(system) = actix {
        bounded(system.as_ref().clone().shutdown()).await.unwrap();
    }
    assert!(TraceContext::current().is_none());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_factory_reinitialization_records_new_causal_child() {
    restart_contract(0).await;
}
#[actix::test]
async fn actix_factory_reinitialization_records_new_causal_child() {
    restart_contract(1).await;
}
#[actix::test]
async fn actix_local_factory_reinitialization_records_new_causal_child() {
    restart_contract(2).await;
}
#[actix::test]
async fn actix_dynamic_factory_reinitialization_records_new_causal_child() {
    restart_contract(3).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_shared_stop_identity() {
    thread_contract(0).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_dedicated_stop_identity() {
    thread_contract(1).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_sharded_stop_identity() {
    thread_contract(2).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_typed_stop_identity() {
    thread_contract(3).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_typed_factory_stop_identity() {
    thread_contract(4).await;
}
#[actix::test]
async fn actix_dynamic_stop_identity() {
    actix_contract(0).await;
}
#[actix::test]
async fn actix_dynamic_factory_stop_identity() {
    actix_contract(1).await;
}
#[actix::test]
async fn actix_typed_stop_identity() {
    actix_contract(2).await;
}
#[actix::test]
async fn actix_typed_factory_stop_identity() {
    actix_contract(3).await;
}
#[actix::test]
async fn actix_local_stop_identity() {
    actix_contract(4).await;
}
#[actix::test]
async fn actix_local_factory_stop_identity() {
    actix_contract(5).await;
}
