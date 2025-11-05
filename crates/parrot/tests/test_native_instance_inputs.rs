//! Native creation owns independent inputs before polling and through rejection.
use futures::FutureExt;
use parrot::{
    actix::{ActixActor, ActixActorSystem, ActixContext},
    thread::{ThreadActorConfig, ThreadActorSystem, ThreadContext},
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorConfig, ActorState},
    context::ActorContext,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    system::ActorSystem,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct Counts {
    drops: AtomicUsize,
    init: AtomicUsize,
}
struct BadPayload;
impl Drop for BadPayload {
    fn drop(&mut self) {
        panic!("native input panic payload cleanup");
    }
}
struct Input {
    counts: Arc<Counts>,
    gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    recursive: bool,
    hostile: bool,
    name: &'static str,
    entered: Option<tokio::sync::oneshot::Sender<()>>,
}
impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Input")
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        if let Some(gate) = self.gate.get_mut().unwrap() {
            gate.recv_timeout(Duration::from_secs(5))
                .expect("caller blocked by native input disposal");
        }
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
        if self.recursive {
            std::panic::panic_any(BadPayload);
        }
        if self.hostile {
            panic!("{} cleanup", self.name);
        }
    }
}
fn input(c: &Arc<Counts>, hostile: bool) -> Input {
    Input {
        counts: c.clone(),
        gate: Mutex::new(None),
        recursive: false,
        hostile,
        name: "input",
        entered: None,
    }
}
impl ActorConfig for Input {}
impl parrot::thread::mailbox::admission::MessageAdmission for Input {
    fn prepare(
        &self,
        message: BoxedMessage,
    ) -> Result<BoxedMessage, parrot::thread::error::MailboxError> {
        Ok(message)
    }
}
#[derive(Debug)]
struct Parent(Input);
impl parrot_api::address::ActorRef for Parent {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("unused parent send")
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("unused parent deliver")
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("unpolled creation must not stop its parent")
    }
    fn path(&self) -> String {
        let _ = &self.0;
        "/parent".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> parrot_api::types::BoxedActorRef {
        panic!("unpolled creation must not clone its parent")
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct ThreadProbe(Input);
struct ActixProbe(Input);
struct ErasedProbe(Input);
macro_rules! actor {
    ($name:ident, $context:ty, $config:ty) => {
        impl Actor for $name {
            type Config = $config;
            type Context = $context;
            fn init<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                self.0.counts.init.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move { Ok(message) })
            }
            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }
    };
}
actor!(ThreadProbe, ThreadContext<Self>, Input);
actor!(ActixProbe, ActixContext<ActixActor<Self>>, Input);
actor!(ErasedProbe, dyn ActorContext, Box<dyn std::any::Any + Send>);
impl ActixEngineExt for ActixProbe {}
fn cancel<F: Future + Send>(future: F, admitted: usize) {
    assert_eq!(
        admitted, 0,
        "calling an unpolled creation future must not register an actor"
    );
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok(),
        "unpolled native creation escaped the caller"
    );
}
async fn probe(mode: usize) {
    if mode >= 10 {
        publication_probe(mode).await;
        return;
    }
    let c = Arc::new(Counts::default());
    if mode >= 8 {
        let s = ThreadActorSystem::shared(Default::default());
        let hostile = mode == 8;
        let result =
            std::panic::AssertUnwindSafe(<ThreadActorSystem as ActorSystem>::spawn_root_typed(
                &s,
                ThreadProbe(input(&c, hostile)),
                input(&c, hostile),
            ))
            .catch_unwind()
            .await;
        assert_eq!(
            c.drops.load(Ordering::SeqCst),
            2,
            "rejection returned before input retirement"
        );
        assert_eq!(s.actor_count(), 0);
        assert_eq!(c.init.load(Ordering::SeqCst), 0);
        if hostile {
            assert!(
                matches!(
                    result,
                    Ok(Err(parrot_api::system::SystemError::ActorError(_)))
                ),
                "rejection must return the actual disposal error"
            );
        } else {
            assert!(
                matches!(
                    result,
                    Ok(Err(parrot_api::system::SystemError::ActorCreationError(_)))
                ),
                "ordinary rejection must retain its error kind"
            );
        }
        s.shutdown_internal().await.unwrap();
        return;
    }
    let expected = if matches!(mode, 1 | 2 | 6) { 3 } else { 2 };
    for (round, blocked) in [false, true].into_iter().enumerate() {
        let (release, receive) = std::sync::mpsc::channel();
        let mut gate = Some(receive);
        let mut actor_input = || Input {
            counts: c.clone(),
            gate: Mutex::new(if blocked { gate.take() } else { None }),
            recursive: !blocked,
            hostile: true,
            name: "actor",
            entered: None,
        };
        let input = || input(&c, true);
        if mode < 5 {
            let s = ThreadActorSystem::shared(Default::default());
            match mode {
                0 => cancel(
                    s.spawn_root_typed_thread(ThreadProbe(actor_input()), input()),
                    s.actor_count(),
                ),
                1 => cancel(
                    s.spawn_typed_with_config(
                        ThreadProbe(actor_input()),
                        input(),
                        ThreadActorConfig {
                            message_admission: Some(Arc::new(input())),
                            ..Default::default()
                        },
                    ),
                    s.actor_count(),
                ),
                2 => cancel(
                    s.spawn_at(
                        ThreadProbe(actor_input()),
                        "/unpolled",
                        Some(Box::new(Parent(input()))),
                        ThreadActorConfig {
                            message_admission: Some(Arc::new(input())),
                            ..Default::default()
                        },
                    ),
                    s.actor_count(),
                ),
                3 => cancel(
                    <ThreadActorSystem as ActorSystem>::spawn_root_boxed(
                        &s,
                        Box::new(ErasedProbe(actor_input())),
                        Box::new(input()),
                    ),
                    s.actor_count(),
                ),
                4 => cancel(
                    <ThreadActorSystem as ActorSystem>::spawn_root_typed(
                        &s,
                        ThreadProbe(actor_input()),
                        input(),
                    ),
                    s.actor_count(),
                ),
                _ => unreachable!(),
            }
            if blocked {
                release.send(()).unwrap();
            }
            retired(&c, (round + 1) * expected).await;
            assert_eq!(s.actor_count(), 0);
            s.shutdown_internal().await.unwrap();
        } else {
            let s = ActixActorSystem::with_arbiter_count(1).await.unwrap();
            match mode {
                5 => cancel(
                    s.spawn_root_typed(ActixProbe(actor_input()), input()),
                    s.status().active_actors,
                ),
                6 => {
                    let captured = input();
                    let policy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
                        max_restarts: 1,
                        within: Duration::from_secs(60),
                        decider: BasicDecisionFn::new(move |_| {
                            let _ = &captured;
                            SupervisionDecision::Stop
                        }),
                    });
                    cancel(
                        s.spawn_root_typed_with_strategy(
                            ActixProbe(actor_input()),
                            input(),
                            policy,
                        ),
                        s.status().active_actors,
                    );
                }
                7 => cancel(
                    s.spawn_root_boxed(Box::new(ErasedProbe(actor_input())), Box::new(input())),
                    s.status().active_actors,
                ),
                _ => unreachable!(),
            }
            if blocked {
                release.send(()).unwrap();
            }
            retired(&c, (round + 1) * expected).await;
            assert_eq!(s.status().active_actors, 0);
            s.shutdown().await.unwrap();
        }
    }
}
async fn retired(c: &Counts, expected: usize) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while c.drops.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(c.drops.load(Ordering::SeqCst), expected);
    assert_eq!(c.init.load(Ordering::SeqCst), 0);
}
fn isolated(name: &str, mode: usize) {
    const FLAG: &str = "PARROT_NATIVE_INSTANCE_PROBE";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        actix::System::new().block_on(probe(mode));
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
            panic!("{name}: child exceeded deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! case {
    ($name:ident, $mode:expr) => {
        #[test]
        fn $name() {
            isolated(stringify!($name), $mode);
        }
    };
}
case!(thread_native_root_unpolled, 0);
case!(thread_native_configured_unpolled, 1);
case!(thread_native_path_unpolled, 2);
case!(thread_boxed_trait_unpolled, 3);
case!(thread_unsupported_generic_unpolled, 4);
case!(actix_native_root_unpolled, 5);
case!(actix_native_policy_unpolled, 6);
case!(actix_boxed_unpolled, 7);

case!(thread_unsupported_generic_retires_before_rejection, 8);
case!(thread_unsupported_generic_preserves_normal_error, 9);

// Each case runs in its own subprocess: the original aggregate Drop can abort
// rather than unwind, and a failing destructor must not kill the test runner.
async fn publication_probe(mode: usize) {
    use parrot::thread::{SchedulingMode, ThreadActorSystemConfig, error::SpawnError};
    let s = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        max_dedicated_threads: 0,
        ..Default::default()
    });
    let c = Arc::new(Counts::default());
    let hostile = !matches!(mode, 16..=19);
    let mut actor = input(&c, hostile);
    actor.name = "actor";
    let mut parent = input(&c, hostile);
    parent.name = "parent";
    let mut policy = input(&c, mode != 19);
    policy.name = "policy";
    let mut config = input(&c, mode == 11);
    config.name = "configuration";
    let (entered, wait) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    if matches!(mode, 16 | 17) {
        policy.entered = Some(entered);
        policy.gate = Mutex::new(Some(gate));
    } else if mode == 18 {
        config.entered = Some(entered);
        config.gate = Mutex::new(Some(gate));
    }
    if matches!(mode, 12..=15) {
        actor.recursive = true;
    }
    let engine = ThreadActorConfig {
        message_admission: Some(Arc::new(policy)),
        message_batch_size: matches!(mode, 10 | 12 | 16 | 19).then_some(0),
        scheduling_mode: matches!(mode, 15 | 17).then_some(SchedulingMode::DedicatedThread),
        ..Default::default()
    };
    // Discard fixture inputs that this public entry does not receive.
    let configured = matches!(mode, 10 | 11 | 18);
    let spare = if configured { &mut parent } else { &mut config };
    spare.hostile = false;
    let existing = if mode == 13 {
        let other = Arc::new(Counts::default());
        Some(
            s.spawn_at(
                ThreadProbe(input(&other, false)),
                "/publication",
                None,
                Default::default(),
            )
            .await
            .unwrap(),
        )
    } else {
        None
    };
    if mode == 14 {
        s.shutdown_internal().await.unwrap();
    }
    let owner = s.clone();
    let caller = tokio::spawn(async move {
        if configured {
            drop(parent);
            owner
                .spawn_typed_with_config(ThreadProbe(actor), config, engine)
                .await
        } else {
            drop(config);
            owner
                .spawn_at(
                    ThreadProbe(actor),
                    "/publication",
                    Some(Box::new(Parent(parent))),
                    engine,
                )
                .await
        }
    });
    if matches!(mode, 16..=18) {
        tokio::time::timeout(Duration::from_secs(3), wait)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !caller.is_finished(),
            "creation completed before engine/config retirement"
        );
        // A destructor waiting for this task proves it runs outside the caller
        // executor. Inspecting the registry proves it runs outside its lock.
        let count = s.actor_count();
        if mode == 16 {
            assert_eq!(count, 0);
        } else {
            assert_eq!(count, 1);
        }
        if mode == 18 {
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
            release.send(()).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(3), s.shutdown_internal())
                .await
                .unwrap();
            assert!(
                result.is_err(),
                "cancelled instance lost its disposal error"
            );
            assert_eq!(c.drops.load(Ordering::SeqCst), 4);
            assert_eq!(c.init.load(Ordering::SeqCst), 0);
            assert_eq!(s.actor_count(), 0);
            return;
        }
        release.send(()).unwrap();
    }
    let result = tokio::time::timeout(Duration::from_secs(3), caller)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        c.drops.load(Ordering::SeqCst),
        4,
        "rejection returned before every owned input retired"
    );
    assert_eq!(
        c.init.load(Ordering::SeqCst),
        0,
        "rejected actor initialized"
    );
    if mode == 19 {
        assert!(
            matches!(result, Err(SpawnError::InvalidConfig(_))),
            "clean rejection changed error category: {result:?}"
        );
    } else {
        let error = result.unwrap_err();
        assert!(
            matches!(error, SpawnError::DisposalFailed(_)),
            "missing disposal error: {error}"
        );
        if mode == 11 {
            assert!(
                error.to_string().contains("configuration cleanup"),
                "first error was overwritten: {error}"
            );
        }
    }
    assert_eq!(s.actor_count(), usize::from(existing.is_some()));
    if let Some(existing) = existing {
        use parrot_api::address::ActorRef;
        existing.stop().await.unwrap();
    }
    s.shutdown_internal().await.unwrap();
}
case!(thread_configured_invalid_retires_engine, 10);
case!(
    thread_config_failure_keeps_first_error_and_retires_engine,
    11
);
case!(thread_path_invalid_retires_each_input, 12);
case!(thread_path_duplicate_retires_each_input, 13);
case!(thread_path_shutdown_retires_each_input, 14);
case!(thread_scheduler_refusal_retires_allocated_processor, 15);
case!(thread_early_refusal_waits_for_policy_disposal, 16);
case!(thread_late_refusal_waits_for_policy_disposal, 17);
case!(
    thread_cancelled_configuration_retires_engine_before_shutdown,
    18
);
case!(thread_path_clean_refusal_preserves_error_category, 19);

#[derive(Default)]
struct Location {
    init: Mutex<Option<(String, Option<String>, std::thread::ThreadId)>>,
    receive: Mutex<Option<std::thread::ThreadId>>,
    stop: Mutex<Option<std::thread::ThreadId>>,
    drop: Mutex<Option<std::thread::ThreadId>>,
}
struct Located(Arc<Location>);
impl Drop for Located {
    fn drop(&mut self) {
        *self.0.drop.lock().unwrap() = Some(std::thread::current().id());
    }
}
impl Actor for Located {
    // Explicit-path creation receives no creation config; it must not invent one.
    type Config = Input;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        *self.0.init.lock().unwrap() = Some((
            ctx.get_self_ref().path(),
            ctx.parent().map(|p| p.path()),
            std::thread::current().id(),
        ));
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        *self.0.receive.lock().unwrap() = Some(std::thread::current().id());
        Box::pin(async move { Ok(message) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        *self.0.stop.lock().unwrap() = Some(std::thread::current().id());
        Box::pin(async { Ok(()) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
#[tokio::test]
async fn explicit_path_keeps_parent_address_and_scheduler_affinity() {
    use parrot::thread::SchedulingMode;
    use parrot_api::address::ActorRef;
    let system = ThreadActorSystem::shared(Default::default());
    let parent = system
        .spawn_at(
            ThreadProbe(input(&Arc::new(Counts::default()), false)),
            "/parent",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    for (index, mode) in [
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "native-owner".into(),
        },
        SchedulingMode::SharedPool {
            max_messages_per_run: 10,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let location = Arc::new(Location::default());
        let path = format!("/child-{index}");
        let reference = system
            .spawn_at(
                Located(location.clone()),
                &path,
                Some(parent.clone_boxed()),
                ThreadActorConfig {
                    scheduling_mode: Some(mode),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(reference.path(), path);
        let reply = tokio::time::timeout(Duration::from_secs(3), reference.send(Box::new(42usize)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*reply.downcast::<usize>().unwrap(), 42);
        reference.stop().await.unwrap();
        assert!(!reference.is_alive().await);
        let init = location.init.lock().unwrap().clone().unwrap();
        assert_eq!(init.0, path);
        assert_eq!(init.1.as_deref(), Some("/parent"));
        assert!(
            location.drop.lock().unwrap().is_some(),
            "stop completed before actor disposal"
        );
        if index < 2 {
            assert_eq!(*location.receive.lock().unwrap(), Some(init.2));
            assert_eq!(*location.stop.lock().unwrap(), Some(init.2));
            assert_eq!(*location.drop.lock().unwrap(), Some(init.2));
        }
    }
    assert!(
        parent.is_alive().await,
        "child retirement stopped its parent"
    );
    parent.stop().await.unwrap();
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn explicit_path_zero_creation_budget_rejects_without_initializing() {
    use parrot::thread::{ThreadActorSystemConfig, error::SpawnError};
    let c = Arc::new(Counts::default());
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shutdown_timeout: Duration::ZERO,
        ..Default::default()
    });
    let result = system
        .spawn_at(
            ThreadProbe(input(&c, false)),
            "/zero-creation",
            None,
            Default::default(),
        )
        .await;
    assert!(
        matches!(
            result,
            Err(SpawnError::Timeout(_)) | Err(SpawnError::Cancelled)
        ),
        "zero creation budget must not publish an actor: {result:?}"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while system.actor_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(c.drops.load(Ordering::SeqCst), 1);
    assert_eq!(c.init.load(Ordering::SeqCst), 0);
    let _ = system.shutdown_internal().await;
}
