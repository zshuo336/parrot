//! Real engine generations, including failures, escaped writes and replacement gateways.
use parrot::{
    actix::{ActixActor, ActixActorSystem, ActixContext},
    system::ParrotActorSystem,
    thread::{
        SchedulingMode, SupervisorStrategy, ThreadActorConfig, ThreadActorSystem,
        ThreadActorSystemConfig, ThreadContext,
    },
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    context::ActorContext,
    errors::ActorError,
    receptionist::{ReceptionistEvent, ReceptionistGateway, ReceptionistKey, ReceptionistStream},
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{acl::AclRules, receptionist::ReceptionistFacade};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const LIMIT: Duration = Duration::from_secs(5);
type Write = BoxedFuture<'static, ActorResult<()>>;
#[derive(Default)]
struct Counts {
    made: AtomicUsize,
    dropped: AtomicUsize,
}
enum Command {
    Ready,
    Capture(bool),
    Register,
    Fail(bool),
}
fn key() -> ReceptionistKey {
    ReceptionistKey::new("edge/generation").unwrap()
}
fn gateway() -> Arc<ReceptionistFacade> {
    Arc::new(ReceptionistFacade::new("node-a").with_acl(AclRules::permissive()))
}
async fn handle<C: ActorContext>(
    generation: usize,
    command: BoxedMessage,
    ctx: &mut C,
) -> ActorResult<BoxedMessage> {
    Ok(match *command.downcast::<Command>().unwrap() {
        Command::Ready => Box::new(generation),
        Command::Register => Box::new(ctx.receptionist_register(key()).await),
        Command::Capture(register) => Box::new(if register {
            ctx.receptionist_register(key())
        } else {
            ctx.receptionist_deregister(&key())
        }),
        Command::Fail(panic) => {
            assert!(!panic, "deliberate generation retirement panic");
            return Err(ActorError::MessageHandlingError(
                "deliberate generation retirement error".into(),
            ));
        }
    })
}
macro_rules! actor {
    ($name:ident, $context:ty) => {
        struct $name {
            counts: Arc<Counts>,
            generation: usize,
        }
        impl $name {
            fn new(counts: Arc<Counts>) -> Self {
                let generation = counts.made.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    counts.dropped.load(Ordering::SeqCst),
                    generation,
                    "new generation must follow old storage retirement"
                );
                Self { counts, generation }
            }
        }
        impl Drop for $name {
            fn drop(&mut self) {
                self.counts.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }
        impl Actor for $name {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn init<'a>(
                &'a mut self,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                ctx.receptionist_register(key())
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(handle(self.generation, message, ctx))
            }
        }
    };
}
actor!(ThreadProbe, ThreadContext<Self>);
actor!(ActixProbe, ActixContext<ActixActor<Self>>);
impl ActixEngineExt for ActixProbe {}

async fn ask<T: Send + 'static>(actor: &BoxedActorRef, command: Command) -> T {
    *tokio::time::timeout(LIMIT, actor.send(Box::new(command)))
        .await
        .unwrap()
        .unwrap()
        .downcast::<T>()
        .expect("response type")
}
async fn event(stream: &mut ReceptionistStream, actor: &BoxedActorRef, registered: bool) {
    let expected = if registered {
        ReceptionistEvent::Registered {
            key: key(),
            remote_path: actor.path(),
        }
    } else {
        ReceptionistEvent::Unregistered {
            key: key(),
            remote_path: actor.path(),
        }
    };
    assert_eq!(
        tokio::time::timeout(LIMIT, stream.recv())
            .await
            .expect("generation event deadline"),
        Some(expected),
        "generation lifecycle event"
    );
}
fn empty(stream: &mut ReceptionistStream) {
    assert_eq!(
        stream.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
}
fn mode(engine: usize) -> SchedulingMode {
    match engine {
        0 => SchedulingMode::SharedPool {
            max_messages_per_run: 4,
        },
        1 => SchedulingMode::DedicatedThread,
        2 => SchedulingMode::Sharded {
            affinity_key: "generation".into(),
        },
        _ => unreachable!(),
    }
}
async fn spawn(
    facade: &ParrotActorSystem,
    counts: Arc<Counts>,
    engine: usize,
    recovery: u8,
) -> BoxedActorRef {
    if engine == 3 {
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        facade
            .register_actix_system("engine".into(), system.clone(), true)
            .await
            .unwrap();
        system
            .spawn_root_factory_with_strategy(
                move || ActixProbe::new(counts.clone()),
                OneForOneStrategy {
                    max_restarts: u32::from(recovery == 1),
                    within: Duration::from_secs(60),
                    decider: BasicDecisionFn::new(move |_| match recovery {
                        0 => SupervisionDecision::Stop,
                        1 => SupervisionDecision::Restart,
                        2 => SupervisionDecision::Resume,
                        _ => unreachable!(),
                    }),
                },
            )
            .await
            .unwrap()
    } else {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            max_dedicated_threads: 1,
            ..Default::default()
        });
        facade
            .register_thread_system("engine".into(), system.clone(), true)
            .await
            .unwrap();
        let actor = system
            .spawn_supervised_with_config(
                move || ThreadProbe::new(counts.clone()),
                "/generation",
                ThreadActorConfig {
                    scheduling_mode: Some(mode(engine)),
                    supervisor_strategy: Some(match recovery {
                        1 => SupervisorStrategy::Restart {
                            max_retries: 1,
                            within: Duration::from_secs(60),
                        },
                        0 => SupervisorStrategy::Stop,
                        2 => SupervisorStrategy::Resume,
                        _ => unreachable!(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        Box::new(actor)
    }
}
async fn lifecycle(engine: usize, failure: Option<bool>, restart: bool) {
    eprintln!("GENERATION engine={engine} failure={failure:?} restart={restart}");
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let gateway = gateway();
    facade
        .register_receptionist_gateway(gateway.clone())
        .await
        .unwrap();
    let mut stream = gateway.subscribe(key()).await.unwrap();
    let counts = Arc::new(Counts::default());
    let actor = spawn(&facade, counts.clone(), engine, u8::from(restart)).await;
    assert_eq!(ask::<usize>(&actor, Command::Ready).await, 0);
    event(&mut stream, &actor, true).await;
    let register = ask::<Write>(&actor, Command::Capture(true)).await;
    let deregister = ask::<Write>(&actor, Command::Capture(false)).await;
    if let Some(panic) = failure {
        assert!(
            tokio::time::timeout(LIMIT, actor.send(Box::new(Command::Fail(panic))))
                .await
                .unwrap()
                .is_err()
        );
    } else {
        tokio::time::timeout(LIMIT, actor.stop())
            .await
            .unwrap()
            .unwrap();
    }
    event(&mut stream, &actor, false).await;
    if restart {
        event(&mut stream, &actor, true).await;
        assert_eq!(ask::<usize>(&actor, Command::Ready).await, 1);
    }
    assert!(
        matches!(register.await, Err(ActorError::Stopped)),
        "retired generation must reject escaped register"
    );
    assert!(
        matches!(deregister.await, Err(ActorError::Stopped)),
        "retired generation must reject escaped deregister"
    );
    let mut snapshot = gateway.subscribe(key()).await.unwrap();
    if restart {
        event(&mut snapshot, &actor, true).await;
        tokio::time::timeout(LIMIT, actor.stop())
            .await
            .unwrap()
            .unwrap();
        event(&mut stream, &actor, false).await;
        event(&mut snapshot, &actor, false).await;
    }
    empty(&mut stream);
    empty(&mut snapshot);
    empty(&mut gateway.subscribe(key()).await.unwrap());
    assert_eq!(counts.made.load(Ordering::SeqCst), 1 + usize::from(restart));
    let shutdown = facade.shutdown_runtime().await;
    if engine == 3 && failure.is_some() && !restart {
        let expected = if failure == Some(true) {
            "engine \"engine\": Panic: deliberate generation retirement panic"
        } else {
            "engine \"engine\": Message handling failed: deliberate generation retirement error"
        };
        assert_eq!(
            shutdown.unwrap_err().to_string(),
            expected,
            "terminal actor error must remain in the shutdown receipt"
        );
    } else {
        shutdown.unwrap();
    }
    assert_eq!(
        counts.dropped.load(Ordering::SeqCst),
        counts.made.load(Ordering::SeqCst)
    );
}
async fn replacement(engine: usize) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let original = gateway();
    facade
        .register_receptionist_gateway(original.clone())
        .await
        .unwrap();
    let mut old = original.subscribe(key()).await.unwrap();
    let counts = Arc::new(Counts::default());
    let actor = spawn(&facade, counts.clone(), engine, 0).await;
    assert_eq!(ask::<usize>(&actor, Command::Ready).await, 0);
    event(&mut old, &actor, true).await;
    let new = gateway();
    facade
        .register_receptionist_gateway(new.clone())
        .await
        .unwrap();
    let mut current = new.subscribe(key()).await.unwrap();
    ask::<ActorResult<()>>(&actor, Command::Register)
        .await
        .unwrap();
    event(&mut current, &actor, true).await;
    actor.stop().await.unwrap();
    event(&mut old, &actor, false).await;
    event(&mut current, &actor, false).await;
    empty(&mut original.subscribe(key()).await.unwrap());
    empty(&mut new.subscribe(key()).await.unwrap());
    facade.shutdown_runtime().await.unwrap();
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
}
async fn matrix(engines: &[usize]) {
    for &engine in engines {
        lifecycle(engine, None, false).await;
        for panic in [false, true] {
            lifecycle(engine, Some(panic), false).await;
            lifecycle(engine, Some(panic), true).await;
        }
        replacement(engine).await;
        resume(engine).await;
    }
}

async fn resume(engine: usize) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let gateway = gateway();
    facade
        .register_receptionist_gateway(gateway.clone())
        .await
        .unwrap();
    let counts = Arc::new(Counts::default());
    let actor = spawn(&facade, counts.clone(), engine, 2).await;
    let mut stream = gateway.subscribe(key()).await.unwrap();
    event(&mut stream, &actor, true).await;
    let register = ask::<Write>(&actor, Command::Capture(true)).await;
    let deregister = ask::<Write>(&actor, Command::Capture(false)).await;
    assert!(actor.send(Box::new(Command::Fail(false))).await.is_err());
    assert_eq!(ask::<usize>(&actor, Command::Ready).await, 0);
    empty(&mut stream);
    deregister.await.unwrap();
    event(&mut stream, &actor, false).await;
    register.await.unwrap();
    event(&mut stream, &actor, true).await;
    actor.stop().await.unwrap();
    event(&mut stream, &actor, false).await;
    empty(&mut gateway.subscribe(key()).await.unwrap());
    facade.shutdown_runtime().await.unwrap();
    assert_eq!(counts.made.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_generation_retirement() {
    matrix(&[0, 1, 2]).await;
}

#[test]
fn actix_generation_retirement() {
    if std::env::var_os("PARROT_GENERATION_CHILD").is_some() {
        actix::System::new().block_on(matrix(&[3]));
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "actix_generation_retirement", "--nocapture"])
        .env("PARROT_GENERATION_CHILD", "1")
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Actix generation watchdog");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
