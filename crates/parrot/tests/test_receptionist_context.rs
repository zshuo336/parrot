//! Exercise all three context methods, actual discovery and actor retirement.
use parrot::{
    system::ParrotActorSystem,
    thread::{
        SchedulingMode, ThreadActorConfig, ThreadActorSystem, ThreadActorSystemConfig,
        ThreadContext,
    },
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorPath,
    context::ActorContext,
    errors::ActorError,
    receptionist::{ReceptionistEvent, ReceptionistKey, ReceptionistStream},
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
const FORBIDDEN: &str = "forbidden: receptionist key outside role scope (E5 ACL)";
#[derive(Default)]
struct Counts {
    initialized: AtomicUsize,
    dropped: AtomicUsize,
}
struct Probe(Arc<Counts>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
enum Command {
    Register,
    Deregister,
    Subscribe,
    Identity,
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            // Expected gateway errors are reply values, not supervision failures.
            let value: BoxedMessage = match *msg.downcast::<Command>().unwrap() {
                Command::Register => Box::new(ctx.receptionist_register(key()).await),
                Command::Deregister => Box::new(ctx.receptionist_deregister(&key()).await),
                Command::Subscribe => Box::new(ctx.receptionist_subscribe(key()).await),
                Command::Identity => Box::new(0x655u64),
            };
            Ok(value)
        })
    }
}
fn key() -> ReceptionistKey {
    ReceptionistKey::new("jvm/echo").unwrap()
}
fn gateway() -> Arc<ReceptionistFacade> {
    Arc::new(
        ReceptionistFacade::new("node-a")
            .with_acl(AclRules::from_json(r#"{"roles":{"node-a":["jvm/echo"]}}"#).unwrap()),
    )
}
fn modes() -> [SchedulingMode; 3] {
    [
        SchedulingMode::SharedPool {
            max_messages_per_run: 4,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "receptionist-contract".into(),
        },
    ]
}
fn system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        max_dedicated_threads: 1,
        ..Default::default()
    })
}
async fn register(
    facade: &ParrotActorSystem,
    name: &str,
    system: Arc<ThreadActorSystem>,
    neutral: bool,
) {
    if neutral {
        facade.register_runtime(name.into(), system, false).unwrap();
    } else {
        facade
            .register_thread_system(name.into(), system, false)
            .await
            .unwrap();
    }
}
async fn spawn(
    system: &Arc<ThreadActorSystem>,
    path: &str,
    mode: SchedulingMode,
) -> (BoxedActorRef, Arc<Counts>) {
    let counts = Arc::new(Counts::default());
    let actor = system
        .spawn_at(
            Probe(counts.clone()),
            path,
            None,
            ThreadActorConfig {
                scheduling_mode: Some(mode),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    (Box::new(actor), counts)
}
async fn ask<T: Send + 'static>(actor: &BoxedActorRef, command: Command) -> T {
    *tokio::time::timeout(LIMIT, actor.send(Box::new(command)))
        .await
        .unwrap()
        .unwrap()
        .downcast::<T>()
        .expect("probe response type")
}
async fn event(stream: &mut ReceptionistStream, actor: &BoxedActorRef, registered: bool) -> String {
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
    let actual = tokio::time::timeout(LIMIT, stream.recv())
        .await
        .unwrap()
        .expect("live event stream");
    assert_eq!(actual, expected);
    match actual {
        ReceptionistEvent::Registered { remote_path, .. }
        | ReceptionistEvent::Unregistered { remote_path, .. } => remote_path,
    }
}
async fn roundtrip(facade: &ParrotActorSystem, actor: &BoxedActorRef) {
    let mut facade_events = facade
        .receptionist_gateway()
        .unwrap()
        .subscribe(key())
        .await
        .unwrap();
    let mut live = ask::<ActorResult<ReceptionistStream>>(actor, Command::Subscribe)
        .await
        .unwrap();
    ask::<ActorResult<()>>(actor, Command::Register)
        .await
        .unwrap();
    let received_path = event(&mut live, actor, true).await;
    event(&mut facade_events, actor, true).await;
    let mut snapshot = ask::<ActorResult<ReceptionistStream>>(actor, Command::Subscribe)
        .await
        .unwrap();
    event(&mut snapshot, actor, true).await;
    let discovered = facade
        .internal_get_actor(&ActorPath::placeholder(received_path))
        .await
        .unwrap();
    assert_eq!(ask::<u64>(&discovered, Command::Identity).await, 0x655);
    ask::<ActorResult<()>>(actor, Command::Deregister)
        .await
        .unwrap();
    event(&mut live, actor, false).await;
    event(&mut snapshot, actor, false).await;
    event(&mut facade_events, actor, false).await;
    for stream in [&mut live, &mut snapshot, &mut facade_events] {
        assert!(matches!(
            stream.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }
    let mut empty = ask::<ActorResult<ReceptionistStream>>(actor, Command::Subscribe)
        .await
        .unwrap();
    assert!(matches!(
        empty.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}
async fn stop(actor: &BoxedActorRef, counts: &Counts) {
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 0);
    tokio::time::timeout(LIMIT, actor.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(!actor.is_alive().await);
    assert_eq!(counts.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receptionist_context_roundtrip() {
    for mode in modes() {
        for neutral in [false, true] {
            for gateway_first in [false, true] {
                eprintln!(
                    "SCENARIO roundtrip {mode:?} neutral={neutral} gateway_first={gateway_first}"
                );
                let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
                let first = system();
                let (actor, counts) = spawn(&first, "/user/reg", mode.clone()).await;
                let original = gateway();
                if gateway_first {
                    facade
                        .register_receptionist_gateway(original.clone())
                        .await
                        .unwrap();
                }
                register(&facade, "first", first, neutral).await;
                if !gateway_first {
                    facade
                        .register_receptionist_gateway(original)
                        .await
                        .unwrap();
                }
                roundtrip(&facade, &actor).await;
                // Verify replacement through actual calls by the already-running actor.
                facade
                    .register_receptionist_gateway(gateway())
                    .await
                    .unwrap();
                roundtrip(&facade, &actor).await;
                let late = system();
                let (late_actor, late_counts) = spawn(&late, "/user/late", mode.clone()).await;
                register(&facade, "late", late, neutral).await;
                roundtrip(&facade, &late_actor).await;
                stop(&actor, &counts).await;
                stop(&late_actor, &late_counts).await;
                facade.shutdown_runtime().await.unwrap();
            }
        }
    }
}
fn error<T>(result: ActorResult<T>, expected: &str) {
    match result {
        Err(ActorError::InternalError(message)) => assert_eq!(message, expected),
        _ => panic!("expected exact InternalError: {expected}"),
    }
}
async fn errors(actor: &BoxedActorRef, expected: &str) {
    error(
        ask::<ActorResult<()>>(actor, Command::Register).await,
        expected,
    );
    error(
        ask::<ActorResult<()>>(actor, Command::Deregister).await,
        expected,
    );
    error(
        ask::<ActorResult<ReceptionistStream>>(actor, Command::Subscribe).await,
        expected,
    );
    assert_eq!(ask::<u64>(actor, Command::Identity).await, 0x655);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receptionist_not_enabled_without_gateway() {
    for mode in modes() {
        for neutral in [false, true] {
            eprintln!("SCENARIO errors {mode:?} neutral={neutral}");
            let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
            let engine = system();
            let (actor, counts) = spawn(&engine, "/user/reg", mode.clone()).await;
            register(&facade, "engine", engine, neutral).await;
            errors(&actor, "receptionist not enabled").await;
            facade
                .register_receptionist_gateway(Arc::new(ReceptionistFacade::new("node-a")))
                .await
                .unwrap();
            errors(&actor, FORBIDDEN).await;
            facade
                .register_receptionist_gateway(gateway())
                .await
                .unwrap();
            roundtrip(&facade, &actor).await;
            stop(&actor, &counts).await;
            facade.shutdown_runtime().await.unwrap();
        }
    }
}
