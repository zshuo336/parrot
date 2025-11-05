//! Managed Actix actors must use the same receptionist publication as the facade.
use parrot::{
    actix::{ActixActor, ActixActorSystem, ActixContext},
    system::{ParrotActorSystem, ReceptionistGateway},
    thread::system::ThreadActorSystem,
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
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
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

const LIMIT: Duration = Duration::from_secs(5);
type Write = BoxedFuture<'static, ActorResult<()>>;
type Context = ActixContext<ActixActor<Probe>>;
enum Command {
    Register,
    Deregister,
    Subscribe,
    CaptureWrite(bool),
    DelayedSubscribe(oneshot::Sender<()>, oneshot::Receiver<()>),
    Context,
    Identity,
}
struct Probe(Arc<AtomicUsize>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActixEngineExt for Probe {}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = Context;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        ctx: &'a mut Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            // Return gateway errors as values so supervision does not restart the probe.
            let value: BoxedMessage = match *msg.downcast::<Command>().unwrap() {
                Command::Register => Box::new(ctx.receptionist_register(key()).await),
                Command::Deregister => Box::new(ctx.receptionist_deregister(&key()).await),
                Command::Subscribe => Box::new(ctx.receptionist_subscribe(key()).await),
                Command::CaptureWrite(register) => {
                    let future = if register {
                        ctx.receptionist_register(key())
                    } else {
                        ctx.receptionist_deregister(&key())
                    };
                    Box::new(future)
                }
                Command::DelayedSubscribe(ready, release) => {
                    let future = ctx.receptionist_subscribe(key());
                    ready.send(()).unwrap();
                    release.await.unwrap();
                    Box::new(future.await)
                }
                Command::Context => Box::new(ctx.clone()),
                Command::Identity => Box::new(0x654u64),
            };
            Ok(value)
        })
    }
}
fn key() -> ReceptionistKey {
    ReceptionistKey::new("jvm/actix-contract").unwrap()
}
fn registry() -> Arc<ReceptionistFacade> {
    Arc::new(ReceptionistFacade::new("actix-node").with_acl(
        AclRules::from_json(r#"{"roles":{"actix-node":["jvm/actix-contract"]}}"#).unwrap(),
    ))
}
async fn facade() -> Arc<ParrotActorSystem> {
    Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap())
}
async fn ask<T: Send + 'static>(actor: &BoxedActorRef, command: Command) -> T {
    *tokio::time::timeout(LIMIT, actor.send(Box::new(command)))
        .await
        .unwrap()
        .unwrap()
        .downcast::<T>()
        .expect("response type")
}
async fn spawn(system: &ActixActorSystem) -> (BoxedActorRef, Arc<AtomicUsize>) {
    let drops = Arc::new(AtomicUsize::new(0));
    let actor = system
        .spawn_root_typed(Probe(drops.clone()), EmptyConfig)
        .await
        .unwrap();
    assert_eq!(ask::<u64>(&actor, Command::Identity).await, 0x654);
    (actor, drops)
}
async fn stop(actor: &BoxedActorRef, drops: &AtomicUsize) {
    tokio::time::timeout(LIMIT, actor.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(!actor.is_alive().await);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
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
        tokio::time::timeout(LIMIT, stream.recv()).await.unwrap(),
        Some(expected)
    );
}
async fn write(actor: &BoxedActorRef, register: bool) {
    ask::<ActorResult<()>>(
        actor,
        if register {
            Command::Register
        } else {
            Command::Deregister
        },
    )
    .await
    .expect("configured actor must forward receptionist writes");
}
async fn stream(actor: &BoxedActorRef) -> ReceptionistStream {
    ask::<ActorResult<ReceptionistStream>>(actor, Command::Subscribe)
        .await
        .expect("configured actor must forward receptionist subscriptions")
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
fn isolated(name: &str, future: impl std::future::Future<Output = ()>) {
    if std::env::var("PARROT_ACTIX_RECEPTIONIST_CASE")
        .ok()
        .as_deref()
        == Some(name)
    {
        actix::System::new().block_on(future);
        return;
    }
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_ACTIX_RECEPTIONIST_CASE", name)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "child contract failed: {name}: {status}");
            return;
        }
        assert!(Instant::now() < deadline, "child watchdog expired: {name}");
        std::thread::sleep(Duration::from_millis(10));
    }
}
async fn roundtrip(gateway_first: bool, neutral: bool) {
    let f = facade().await;
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let (actor, drops) = spawn(&system).await;
    let gateway = registry();
    if gateway_first {
        f.register_receptionist_gateway(gateway.clone())
            .await
            .unwrap();
    }
    if neutral {
        f.register_runtime("actix".into(), Arc::new(system), true)
            .unwrap();
    } else {
        f.register_actix_system("actix".into(), system, true)
            .await
            .unwrap();
    }
    if !gateway_first {
        f.register_receptionist_gateway(gateway.clone())
            .await
            .unwrap();
    }
    let mut live = stream(&actor).await;
    write(&actor, true).await;
    event(&mut live, &actor, true).await;
    let mut snapshot = stream(&actor).await;
    event(&mut snapshot, &actor, true).await;
    let resolved = f
        .internal_get_actor(&ActorPath::placeholder(actor.path()))
        .await
        .unwrap();
    assert_eq!(ask::<u64>(&resolved, Command::Identity).await, 0x654);
    write(&actor, false).await;
    event(&mut live, &actor, false).await;
    event(&mut snapshot, &actor, false).await;
    assert!(live.try_recv().is_err() && snapshot.try_recv().is_err());
    stop(&actor, &drops).await;
    f.shutdown_runtime().await.unwrap();
}
#[test]
fn engine_before_gateway_roundtrip() {
    isolated("engine_before_gateway_roundtrip", roundtrip(false, false));
}
#[test]
fn gateway_before_engine_roundtrip() {
    isolated("gateway_before_engine_roundtrip", roundtrip(true, false));
}
#[test]
fn neutral_engine_before_gateway_roundtrip() {
    isolated(
        "neutral_engine_before_gateway_roundtrip",
        roundtrip(false, true),
    );
}
#[test]
fn gateway_before_neutral_engine_roundtrip() {
    isolated(
        "gateway_before_neutral_engine_roundtrip",
        roundtrip(true, true),
    );
}
#[test]
fn replacement_reaches_running_actor_and_preserves_old_stream() {
    isolated(
        "replacement_reaches_running_actor_and_preserves_old_stream",
        async {
            let f = facade().await;
            let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
            let (actor, drops) = spawn(&system).await;
            f.register_actix_system("actix".into(), system, true)
                .await
                .unwrap();
            let old = registry();
            f.register_receptionist_gateway(old.clone()).await.unwrap();
            let mut old_stream = stream(&actor).await;
            let new = registry();
            f.register_receptionist_gateway(new.clone()).await.unwrap();
            let mut new_stream = stream(&actor).await;
            write(&actor, true).await;
            event(&mut new_stream, &actor, true).await;
            assert!(matches!(
                old_stream.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            old.register(key(), actor.path()).await.unwrap();
            event(&mut old_stream, &actor, true).await;
            assert!(matches!(
                new_stream.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            stop(&actor, &drops).await;
            f.shutdown_runtime().await.unwrap();
        },
    );
}
#[test]
fn unpolled_writes_retain_original_gateway_and_path() {
    isolated("unpolled_writes_retain_original_gateway_and_path", async {
        let f = facade().await;
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let (actor, drops) = spawn(&system).await;
        f.register_actix_system("actix".into(), system, true)
            .await
            .unwrap();
        let old = registry();
        f.register_receptionist_gateway(old.clone()).await.unwrap();
        let mut observed = old.subscribe(key()).await.unwrap();
        let register = ask::<Write>(&actor, Command::CaptureWrite(true)).await;
        let deregister = ask::<Write>(&actor, Command::CaptureWrite(false)).await;
        let new = registry();
        f.register_receptionist_gateway(new.clone()).await.unwrap();
        let mut other = new.subscribe(key()).await.unwrap();
        register
            .await
            .expect("unpolled register must retain configured gateway");
        event(&mut observed, &actor, true).await;
        deregister
            .await
            .expect("unpolled deregister must retain configured gateway");
        event(&mut observed, &actor, false).await;
        assert!(matches!(
            other.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        write(&actor, true).await;
        event(&mut other, &actor, true).await;
        stop(&actor, &drops).await;
        f.shutdown_runtime().await.unwrap();
    });
}
#[test]
fn unpolled_subscription_retains_original_gateway() {
    isolated("unpolled_subscription_retains_original_gateway", async {
        let f = facade().await;
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let (actor, drops) = spawn(&system).await;
        f.register_actix_system("actix".into(), system, true)
            .await
            .unwrap();
        let old = registry();
        f.register_receptionist_gateway(old.clone()).await.unwrap();
        let (ready, constructed) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let a = actor.clone_boxed();
        let task = tokio::spawn(async move {
            ask::<ActorResult<ReceptionistStream>>(&a, Command::DelayedSubscribe(ready, released))
                .await
        });
        tokio::time::timeout(LIMIT, constructed)
            .await
            .unwrap()
            .unwrap();
        f.register_receptionist_gateway(registry()).await.unwrap();
        old.register(key(), actor.path()).await.unwrap();
        release.send(()).unwrap();
        let mut observed = task
            .await
            .unwrap()
            .expect("unpolled subscription must retain configured gateway");
        event(&mut observed, &actor, true).await;
        old.deregister(key(), actor.path()).await.unwrap();
        event(&mut observed, &actor, false).await;
        stop(&actor, &drops).await;
        f.shutdown_runtime().await.unwrap();
    });
}

struct Reject;
#[async_trait::async_trait]
impl ReceptionistGateway for Reject {
    async fn register(&self, _: ReceptionistKey, _: String) -> ActorResult<()> {
        Err(ActorError::InternalError("register rejected".into()))
    }
    async fn deregister(&self, _: ReceptionistKey, _: String) -> ActorResult<()> {
        Err(ActorError::InternalError("deregister rejected".into()))
    }
    async fn subscribe(&self, _: ReceptionistKey) -> ActorResult<ReceptionistStream> {
        Err(ActorError::InternalError("subscribe rejected".into()))
    }
}
fn error<T>(result: ActorResult<T>, expected: &str) {
    match result {
        Err(ActorError::InternalError(text)) => assert_eq!(text, expected),
        _ => panic!("expected exact InternalError: {expected}"),
    }
}
#[test]
fn missing_gateway_and_configured_errors_preserve_contract() {
    isolated(
        "missing_gateway_and_configured_errors_preserve_contract",
        async {
            let f = facade().await;
            let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
            let (actor, drops) = spawn(&system).await;
            f.register_actix_system("actix".into(), system, true)
                .await
                .unwrap();
            error(
                ask::<ActorResult<()>>(&actor, Command::Register).await,
                "receptionist not enabled",
            );
            error(
                ask::<ActorResult<()>>(&actor, Command::Deregister).await,
                "receptionist not enabled",
            );
            error(
                ask::<ActorResult<ReceptionistStream>>(&actor, Command::Subscribe).await,
                "receptionist not enabled",
            );
            f.register_receptionist_gateway(Arc::new(Reject))
                .await
                .unwrap();
            error(
                ask::<ActorResult<()>>(&actor, Command::Register).await,
                "register rejected",
            );
            error(
                ask::<ActorResult<()>>(&actor, Command::Deregister).await,
                "deregister rejected",
            );
            error(
                ask::<ActorResult<ReceptionistStream>>(&actor, Command::Subscribe).await,
                "subscribe rejected",
            );
            stop(&actor, &drops).await;
            f.shutdown_runtime().await.unwrap();
        },
    );
}
#[test]
fn retained_context_does_not_own_system_or_gateway() {
    isolated("retained_context_does_not_own_system_or_gateway", async {
        let f = facade().await;
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let (actor, drops) = spawn(&system).await;
        f.register_actix_system("actix".into(), system, true)
            .await
            .unwrap();
        let gateway = registry();
        let weak = Arc::downgrade(&gateway);
        f.register_receptionist_gateway(gateway).await.unwrap();
        let mut context = ask::<Context>(&actor, Command::Context).await;
        stop(&actor, &drops).await;
        f.shutdown_runtime().await.unwrap();
        drop(actor);
        drop(f);
        assert!(
            weak.upgrade().is_none(),
            "cloned context must not retain the gateway"
        );
        error(
            context.receptionist_register(key()).await,
            "receptionist not enabled",
        );
    });
}

#[test]
fn retained_context_and_writes_cannot_revive_retired_generation() {
    isolated(
        "retained_context_and_writes_cannot_revive_retired_generation",
        async {
            let f = facade().await;
            let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
            f.register_actix_system("actix".into(), system.clone(), true)
                .await
                .unwrap();
            let gateway = registry();
            f.register_receptionist_gateway(gateway.clone())
                .await
                .unwrap();
            let (actor, drops) = spawn(&system).await;
            let mut context = ask::<Context>(&actor, Command::Context).await;
            let register = context.receptionist_register(key());
            let deregister = context.receptionist_deregister(&key());
            let mut events = gateway.subscribe(key()).await.unwrap();
            write(&actor, true).await;
            event(&mut events, &actor, true).await;
            stop(&actor, &drops).await;
            event(&mut events, &actor, false).await;
            for result in [
                register.await,
                deregister.await,
                context.receptionist_register(key()).await,
                context.receptionist_deregister(&key()).await,
            ] {
                assert!(
                    matches!(result, Err(ActorError::Stopped)),
                    "retained native context cannot publish after lifecycle closure"
                );
            }
            assert_eq!(
                gateway.subscribe(key()).await.unwrap().try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            );
            assert_eq!(
                events.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            );
            f.shutdown_runtime().await.unwrap();
        },
    );
}

struct CallbackGateway {
    inner: Arc<ReceptionistFacade>,
    retire: Box<dyn Fn() + Send + Sync>,
}
impl Drop for CallbackGateway {
    fn drop(&mut self) {
        (self.retire)();
    }
}
#[async_trait::async_trait]
impl ReceptionistGateway for CallbackGateway {
    async fn register(&self, key: ReceptionistKey, path: String) -> ActorResult<()> {
        self.inner.register(key, path).await
    }
    async fn deregister(&self, key: ReceptionistKey, path: String) -> ActorResult<()> {
        self.inner.deregister(key, path).await
    }
    async fn subscribe(&self, key: ReceptionistKey) -> ActorResult<ReceptionistStream> {
        self.inner.subscribe(key).await
    }
}
async fn assert_publication(
    f: &ParrotActorSystem,
    actor: &BoxedActorRef,
    gateway: &Arc<ReceptionistFacade>,
) {
    let check: Arc<dyn ReceptionistGateway> = gateway.clone();
    assert!(Arc::ptr_eq(&f.receptionist_gateway().unwrap(), &check));
    let mut observed = gateway.subscribe(key()).await.unwrap();
    write(actor, true).await;
    event(&mut observed, actor, true).await;
    write(actor, false).await;
    event(&mut observed, actor, false).await;
    assert!(matches!(
        observed.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}
#[test]
fn reentrant_retirement_keeps_newer_publication() {
    isolated("reentrant_retirement_keeps_newer_publication", async {
        let f = facade().await;
        let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let (actor, drops) = spawn(&system).await;
        f.register_actix_system("actix".into(), system, true)
            .await
            .unwrap();
        let newest = registry();
        let nested = newest.clone();
        let weak = Arc::downgrade(&f);
        let count = Arc::new(AtomicUsize::new(0));
        let retired = count.clone();
        f.register_receptionist_gateway(Arc::new(CallbackGateway {
            inner: registry(),
            retire: Box::new(move || {
                let Some(f) = weak.upgrade() else {
                    return;
                };
                assert_eq!(f.list_registered_systems().unwrap(), ["actix"]);
                retired.fetch_add(1, Ordering::SeqCst);
                futures::executor::block_on(f.register_receptionist_gateway(nested.clone()))
                    .unwrap();
            }),
        }))
        .await
        .unwrap();
        f.register_receptionist_gateway(registry()).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_publication(&f, &actor, &newest).await;
        stop(&actor, &drops).await;
        f.shutdown_runtime().await.unwrap();
    });
}
#[test]
fn inherited_gateway_retires_outside_registry_and_keeps_reentrant_update() {
    isolated(
        "inherited_gateway_retires_outside_registry_and_keeps_reentrant_update",
        inherited_retirement(false),
    );
}
#[test]
fn neutral_inheritance_retires_outside_registry_and_keeps_reentrant_update() {
    isolated(
        "neutral_inheritance_retires_outside_registry_and_keeps_reentrant_update",
        inherited_retirement(true),
    );
}
async fn inherited_retirement(neutral: bool) {
    let f = facade().await;
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let (actor, drops) = spawn(&system).await;
    let newest = registry();
    let nested = newest.clone();
    let weak = Arc::downgrade(&f);
    let count = Arc::new(AtomicUsize::new(0));
    let retired = count.clone();
    // Public APIs shared with the old implementation seed the engine's slot.
    let seed = facade().await;
    seed.register_receptionist_gateway(Arc::new(CallbackGateway {
        inner: registry(),
        retire: Box::new(move || {
            let Some(f) = weak.upgrade() else {
                return;
            };
            assert_eq!(f.list_registered_systems().unwrap(), ["actix"]);
            retired.fetch_add(1, Ordering::SeqCst);
            futures::executor::block_on(f.register_receptionist_gateway(nested.clone())).unwrap();
        }),
    }))
    .await
    .unwrap();
    seed.register_actix_system("seed".into(), system.clone(), true)
        .await
        .unwrap();
    drop(seed);
    f.register_receptionist_gateway(registry()).await.unwrap();
    if neutral {
        f.register_runtime("actix".into(), Arc::new(system), true)
            .unwrap();
    } else {
        f.register_actix_system("actix".into(), system, true)
            .await
            .unwrap();
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "inheritance must retire the displaced engine gateway before returning"
    );
    assert_publication(&f, &actor, &newest).await;
    stop(&actor, &drops).await;
    f.shutdown_runtime().await.unwrap();
}
#[test]
fn panicking_retirement_preserves_both_engines_and_late_registration() {
    isolated(
        "panicking_retirement_preserves_both_engines_and_late_registration",
        async {
            let f = facade().await;
            let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
            let (actor, drops) = spawn(&system).await;
            f.register_actix_system("actix".into(), system, true)
                .await
                .unwrap();
            let thread = ThreadActorSystem::shared(Default::default());
            f.register_thread_system("thread".into(), thread.clone(), false)
                .await
                .unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let retired = count.clone();
            f.register_receptionist_gateway(Arc::new(CallbackGateway {
                inner: registry(),
                retire: Box::new(move || {
                    retired.fetch_add(1, Ordering::SeqCst);
                    panic!("retiring receptionist");
                }),
            }))
            .await
            .unwrap();
            let newest = registry();
            let error = f
                .register_receptionist_gateway(newest.clone())
                .await
                .unwrap_err();
            assert!(error.to_string().contains("registration committed"));
            assert_eq!(count.load(Ordering::SeqCst), 1);
            let check: Arc<dyn ReceptionistGateway> = newest.clone();
            assert!(Arc::ptr_eq(&thread.receptionist_gateway().unwrap(), &check));
            assert_publication(&f, &actor, &newest).await;
            let late = ActixActorSystem::with_arbiter_count(1).await.unwrap();
            let (late_actor, late_drops) = spawn(&late).await;
            f.register_actix_system("late".into(), late, false)
                .await
                .unwrap();
            assert_publication(&f, &late_actor, &newest).await;
            stop(&actor, &drops).await;
            stop(&late_actor, &late_drops).await;
            f.shutdown_runtime().await.unwrap();
        },
    );
}
#[test]
fn concurrent_registration_and_updates_converge_on_actual_actor_calls() {
    isolated(
        "concurrent_registration_and_updates_converge_on_actual_actor_calls",
        async {
            let f = facade().await;
            let mut systems = Vec::new();
            let mut actors = Vec::new();
            for _ in 0..4 {
                let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
                actors.push(spawn(&system).await);
                systems.push(system);
            }
            let barrier = Arc::new(std::sync::Barrier::new(6));
            let mut joins = Vec::new();
            for (i, system) in systems.into_iter().enumerate() {
                let f = f.clone();
                let barrier = barrier.clone();
                joins.push(std::thread::spawn(move || {
                    barrier.wait();
                    if i % 2 == 0 {
                        f.register_runtime(format!("engine-{i}"), Arc::new(system), false)
                            .unwrap();
                    } else {
                        futures::executor::block_on(f.register_actix_system(
                            format!("engine-{i}"),
                            system,
                            false,
                        ))
                        .unwrap();
                    }
                }));
            }
            for _ in 0..2 {
                let f = f.clone();
                let barrier = barrier.clone();
                joins.push(std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..32 {
                        futures::executor::block_on(f.register_receptionist_gateway(registry()))
                            .unwrap();
                    }
                }));
            }
            for join in joins {
                join.join().unwrap();
            }
            let mut observed = f
                .receptionist_gateway()
                .unwrap()
                .subscribe(key())
                .await
                .unwrap();
            for (actor, drops) in &actors {
                write(actor, true).await;
                event(&mut observed, actor, true).await;
                write(actor, false).await;
                event(&mut observed, actor, false).await;
                stop(actor, drops).await;
            }
            assert!(matches!(
                observed.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            f.shutdown_runtime().await.unwrap();
        },
    );
}
