//! The same application contract uses only parrot-api runtime/ref types on both engines.
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineCatalog, AsyncEngineRuntime, InstanceConfig, RuntimeActor, SpawnOptions},
    errors::ActorError,
    system::{SystemError, SystemState, SystemStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

#[derive(Default)]
struct Counts {
    init: AtomicUsize,
    stop: AtomicUsize,
    actor: AtomicUsize,
    config: AtomicUsize,
}
type AsyncGate = (oneshot::Sender<()>, oneshot::Receiver<()>);
struct Probe {
    counts: Arc<Counts>,
    init_gate: Option<AsyncGate>,
    stop_gate: Option<AsyncGate>,
    fail_stop: bool,
    panic_drop: bool,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.actor.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "runtime actor disposal failed");
    }
}
impl Actor for Probe {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.init.fetch_add(1, Ordering::SeqCst);
            if let Some((entered, release)) = self.init_gate.take() {
                let _ = entered.send(());
                let _ = release.await;
            }
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            if message.is::<()>() {
                return Ok(Box::new(ctx.get_self_ref().path()) as BoxedMessage);
            }
            if message.is::<bool>() {
                return Err(ActorError::InternalError("runtime handler failed".into()));
            }
            Ok(message)
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.counts.stop.fetch_add(1, Ordering::SeqCst);
            if let Some((entered, release)) = self.stop_gate.take() {
                let _ = entered.send(());
                let _ = release.await;
            }
            if self.fail_stop {
                Err(ActorError::InternalError("runtime stop failed".into()))
            } else {
                Ok(())
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
struct Config {
    counts: Arc<Counts>,
    gate: Option<(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    panic: bool,
}
impl Drop for Config {
    fn drop(&mut self) {
        if let Some((entered, release)) = self.gate.take() {
            let _ = entered.send(());
            release.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        self.counts.config.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "runtime config disposal failed");
    }
}
fn actor(c: &Arc<Counts>) -> Probe {
    Probe {
        counts: c.clone(),
        init_gate: None,
        stop_gate: None,
        fail_stop: false,
        panic_drop: false,
    }
}
fn config(c: &Arc<Counts>) -> Config {
    Config {
        counts: c.clone(),
        gate: None,
        panic: false,
    }
}
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("runtime contract deadline")
}
async fn engine(actix: bool) -> Arc<dyn AsyncEngineRuntime> {
    if actix {
        Arc::new(
            parrot::actix::ActixActorSystem::with_arbiter_count(1)
                .await
                .unwrap(),
        )
    } else {
        parrot::thread::ThreadActorSystem::shared(Default::default())
    }
}
async fn drained(runtime: &dyn AsyncEngineRuntime) {
    bounded(async {
        while runtime.status().active_actors != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
}
async fn named_lifecycle(runtime: Arc<dyn AsyncEngineRuntime>) {
    let c = Arc::new(Counts::default());
    let (entered, observed) = oneshot::channel();
    let (release, held) = oneshot::channel();
    let mut a = actor(&c);
    a.init_gate = Some((entered, held));
    let reference = bounded(runtime.spawn(
        Box::new(a),
        Box::new(config(&c)),
        SpawnOptions::at("/user/neutral"),
    ))
    .await
    .unwrap();
    assert_eq!(reference.path(), "/user/neutral");
    bounded(observed).await.unwrap();
    assert_eq!(c.config.load(Ordering::SeqCst), 1);
    let found = runtime.get_actor("/user/neutral").await.unwrap();
    assert_eq!(
        found.death_watch().unwrap().id(),
        reference.death_watch().unwrap().id()
    );
    let reply = reference.send(Box::new(()));
    tokio::pin!(reply);
    assert!(
        futures::poll!(&mut reply).is_pending(),
        "message passed suspended init"
    );
    release.send(()).unwrap();
    assert_eq!(
        *bounded(reply).await.unwrap().downcast::<String>().unwrap(),
        "/user/neutral"
    );
    assert_eq!(
        *found
            .send(Box::new(42_u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    bounded(reference.stop()).await.unwrap();
    assert!(!reference.is_alive().await);
    assert!(runtime.get_actor("/user/neutral").await.is_none());
    assert_eq!(c.stop.load(Ordering::SeqCst), 1);
    assert_eq!(c.actor.load(Ordering::SeqCst), 1);
    bounded(runtime.shutdown()).await.unwrap();
}
async fn pending_reservation(runtime: Arc<dyn AsyncEngineRuntime>) {
    let c = Arc::new(Counts::default());
    let (entered, observed) = oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let mut cfg = config(&c);
    cfg.gate = Some((entered, held));
    let r = runtime.clone();
    let a = actor(&c);
    let task = tokio::spawn(async move {
        r.spawn(
            Box::new(a),
            Box::new(cfg),
            SpawnOptions::at("/user/reserved"),
        )
        .await
    });
    bounded(observed).await.unwrap();
    let old = runtime
        .get_actor("/user/reserved")
        .await
        .expect("pending path is real registry admission");
    let rejected = Arc::new(Counts::default());
    // A second creation must reject the exact pending identity, not create an alias.
    let duplicate = runtime.spawn(
        Box::new(actor(&rejected)),
        Box::new(config(&rejected)),
        SpawnOptions::at("/user/reserved"),
    );
    tokio::pin!(duplicate);
    // Poll admission while the original config is held. Spawning a task and
    // releasing the gate would race duplicate admission against path retirement.
    let early = futures::poll!(&mut duplicate);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(runtime.status().active_actors, 1);
    release.send(()).unwrap();
    assert!(
        match early {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => bounded(duplicate).await,
        }
        .is_err()
    );
    drained(&*runtime).await;
    assert_eq!(c.init.load(Ordering::SeqCst), 0);
    assert_eq!(c.actor.load(Ordering::SeqCst), 1);
    assert_eq!(rejected.init.load(Ordering::SeqCst), 0);
    assert_eq!(rejected.actor.load(Ordering::SeqCst), 1);
    let next = Arc::new(Counts::default());
    let replacement = runtime
        .spawn(
            Box::new(actor(&next)),
            Box::new(config(&next)),
            SpawnOptions::at("/user/reserved"),
        )
        .await
        .unwrap();
    assert_ne!(
        old.death_watch().unwrap().id(),
        replacement.death_watch().unwrap().id()
    );
    let _ = old.stop().await;
    assert_eq!(
        *replacement
            .send(Box::new(7_u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        7
    );
    bounded(runtime.shutdown()).await.unwrap();
}
async fn rejected_inputs(runtime: Arc<dyn AsyncEngineRuntime>) {
    for path in [String::new(), "/user/\n".into(), "x".repeat(4097)] {
        let c = Arc::new(Counts::default());
        assert!(
            runtime
                .spawn(
                    Box::new(actor(&c)),
                    Box::new(config(&c)),
                    SpawnOptions::at(path)
                )
                .await
                .is_err()
        );
        assert_eq!(c.init.load(Ordering::SeqCst), 0);
        assert_eq!(c.actor.load(Ordering::SeqCst), 1);
        assert_eq!(c.config.load(Ordering::SeqCst), 1);
    }
    let c = Arc::new(Counts::default());
    let mut a = actor(&c);
    a.panic_drop = true;
    let mut cfg = config(&c);
    cfg.panic = true;
    // Dropping before first poll cannot synchronously unwind through both inputs.
    drop(runtime.spawn(Box::new(a), Box::new(cfg), SpawnOptions::default()));
    bounded(async {
        while c.actor.load(Ordering::SeqCst) != 1 || c.config.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(c.init.load(Ordering::SeqCst), 0);
    let c = Arc::new(Counts::default());
    let mut cfg = config(&c);
    cfg.panic = true;
    let error = runtime
        .spawn(
            Box::new(actor(&c)),
            Box::new(cfg),
            SpawnOptions::at("/user/bad-config"),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("runtime config disposal failed"),
        "{error}"
    );
    drained(&*runtime).await;
    assert_eq!(c.init.load(Ordering::SeqCst), 0);
    assert_eq!(c.actor.load(Ordering::SeqCst), 1);
    runtime.shutdown().await.unwrap();
}
async fn cancelled_shutdown(runtime: Arc<dyn AsyncEngineRuntime>) {
    let c = Arc::new(Counts::default());
    let (entered, observed) = oneshot::channel();
    let (release, held) = oneshot::channel();
    let mut a = actor(&c);
    a.stop_gate = Some((entered, held));
    let reference = runtime
        .spawn(Box::new(a), Box::new(config(&c)), SpawnOptions::default())
        .await
        .unwrap();
    reference.send(Box::new(1_u64)).await.unwrap();
    let r = runtime.clone();
    let task = tokio::spawn(async move { r.shutdown().await });
    bounded(observed).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let second = runtime.shutdown();
    tokio::pin!(second);
    assert!(futures::poll!(&mut second).is_pending());
    assert_eq!(c.actor.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    bounded(second).await.unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(runtime.status().state, SystemState::Stopped);
    assert_eq!(c.actor.load(Ordering::SeqCst), 1);
    assert_eq!(c.stop.load(Ordering::SeqCst), 1);
    let late = Arc::new(Counts::default());
    assert!(
        runtime
            .spawn(
                Box::new(actor(&late)),
                Box::new(config(&late)),
                SpawnOptions::default()
            )
            .await
            .is_err()
    );
    assert_eq!(late.init.load(Ordering::SeqCst), 0);
    assert_eq!(late.actor.load(Ordering::SeqCst), 1);
    assert_eq!(late.config.load(Ordering::SeqCst), 1);
}
async fn failures(runtime: Arc<dyn AsyncEngineRuntime>, stop_failure: bool) {
    let c = Arc::new(Counts::default());
    let mut a = actor(&c);
    a.fail_stop = stop_failure;
    let reference = runtime
        .spawn(Box::new(a), Box::new(config(&c)), SpawnOptions::default())
        .await
        .unwrap();
    reference.send(Box::new(1_u64)).await.unwrap();
    if stop_failure {
        let first = runtime.shutdown().await.unwrap_err().to_string();
        assert!(first.contains("runtime stop failed"), "{first}");
        assert_eq!(runtime.shutdown().await.unwrap_err().to_string(), first);
    } else {
        let error = reference.send(Box::new(true)).await.unwrap_err();
        assert!(
            error.to_string().contains("runtime handler failed"),
            "{error}"
        );
        drained(&*runtime).await;
        assert!(!reference.is_alive().await);
        runtime.shutdown().await.unwrap();
    }
    assert_eq!(c.actor.load(Ordering::SeqCst), 1);
    // Fatal dispatch retires the failed instance; it does not run a graceful hook.
    assert_eq!(c.stop.load(Ordering::SeqCst), usize::from(stop_failure));
}

#[actix::test]
async fn thread_named_async_lifecycle() {
    named_lifecycle(engine(false).await).await;
}
#[actix::test]
async fn actix_named_async_lifecycle() {
    named_lifecycle(engine(true).await).await;
}
#[actix::test]
async fn thread_pending_path_and_cancelled_creation() {
    pending_reservation(engine(false).await).await;
}
#[actix::test]
async fn actix_pending_path_and_cancelled_creation() {
    pending_reservation(engine(true).await).await;
}
#[actix::test]
async fn thread_rejected_and_unpolled_inputs() {
    rejected_inputs(engine(false).await).await;
}
#[actix::test]
async fn actix_rejected_and_unpolled_inputs() {
    rejected_inputs(engine(true).await).await;
}
#[actix::test]
async fn thread_cancelled_and_repeated_shutdown() {
    cancelled_shutdown(engine(false).await).await;
}
#[actix::test]
async fn actix_cancelled_and_repeated_shutdown() {
    cancelled_shutdown(engine(true).await).await;
}
#[actix::test]
async fn thread_shutdown_failure_is_stable() {
    failures(engine(false).await, true).await;
}
#[actix::test]
async fn actix_shutdown_failure_is_stable() {
    failures(engine(true).await, true).await;
}
#[actix::test]
async fn thread_handler_failure_retires_reference() {
    failures(engine(false).await, false).await;
}
#[actix::test]
async fn actix_handler_failure_retires_reference() {
    failures(engine(true).await, false).await;
}

// A runtime outside the built-in enum, routing actual actor work to its owner.
struct Extension {
    native: Arc<dyn AsyncEngineRuntime>,
    calls: Arc<AtomicUsize>,
}
impl AsyncEngineRuntime for Extension {
    fn spawn<'a>(
        &'a self,
        a: RuntimeActor,
        c: InstanceConfig,
        o: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.native.spawn(a, c, o)
    }
    fn get_actor<'a>(&'a self, p: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        self.native.get_actor(p)
    }
    fn status(&self) -> SystemStatus {
        self.native.status()
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        self.native.shutdown()
    }
}
#[actix::test]
async fn open_catalog_uses_real_extension_and_stale_tokens_cannot_remove_replacements() {
    let catalog = AsyncEngineCatalog::new();
    let other = AsyncEngineCatalog::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime: Arc<dyn AsyncEngineRuntime> = Arc::new(Extension {
        native: engine(false).await,
        calls: calls.clone(),
    });
    let token = catalog.register("third-party", runtime.clone()).unwrap();
    assert!(catalog.register("third-party", runtime.clone()).is_err());
    assert!(other.lookup("third-party").is_none());
    let captured = catalog.lookup("third-party").unwrap();
    assert!(catalog.insert("third-party", runtime.clone()).is_some());
    drop(token);
    assert_eq!(catalog.list(), vec!["third-party"]);
    assert!(catalog.remove("third-party").is_some());
    assert!(catalog.list().is_empty());
    named_lifecycle(captured).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

pub struct Read;
impl parrot_api::message::Message for Read {
    type Result = u64;
}
#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(Read))]
struct StaticCounter {
    value: u64,
}
impl parrot_api::typed::TypedReceive<Read> for StaticCounter {
    fn receive_typed<'a>(&'a mut self, _: Read) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move { Ok(self.value) })
    }
}
async fn replace_static(runtime: &dyn AsyncEngineRuntime) -> BoxedActorRef {
    let original = runtime.get_actor("/shared-namespace").await.unwrap();
    assert_eq!(
        *original
            .send(Box::new(Read))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    let c = Arc::new(Counts::default());
    assert!(
        runtime
            .spawn(
                Box::new(actor(&c)),
                Box::new(config(&c)),
                SpawnOptions::at("/shared-namespace")
            )
            .await
            .is_err()
    );
    assert_eq!(c.actor.load(Ordering::SeqCst), 1);
    assert_eq!(c.init.load(Ordering::SeqCst), 0);
    original.stop().await.unwrap();
    runtime
        .spawn(
            Box::new(actor(&c)),
            Box::new(config(&c)),
            SpawnOptions::at("/shared-namespace"),
        )
        .await
        .unwrap()
}
#[actix::test]
async fn actix_neutral_and_native_typed_paths_share_actual_admission() {
    let native = Arc::new(
        parrot::actix::ActixActorSystem::with_arbiter_count(1)
            .await
            .unwrap(),
    );
    let original = native
        .spawn_typed::<StaticCounter, Read>(StaticCounter { value: 42 }, "/shared-namespace")
        .await
        .unwrap();
    let reference = replace_static(&*native).await;
    assert!(!original.is_alive());
    assert!(
        native
            .spawn_typed::<StaticCounter, Read>(StaticCounter { value: 99 }, "/shared-namespace")
            .await
            .is_err()
    );
    assert_eq!(
        *reference
            .send(Box::new(7_u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        7
    );
    AsyncEngineRuntime::shutdown(&*native).await.unwrap();
}
#[actix::test]
async fn thread_neutral_and_native_typed_paths_share_actual_admission() {
    let native = parrot::thread::ThreadActorSystem::shared(Default::default());
    let original = native
        .spawn_typed::<StaticCounter, Read>(StaticCounter { value: 42 }, "/shared-namespace")
        .await
        .unwrap();
    let reference = replace_static(&*native).await;
    assert!(!original.is_alive());
    assert!(
        native
            .spawn_typed::<StaticCounter, Read>(StaticCounter { value: 99 }, "/shared-namespace")
            .await
            .is_err()
    );
    assert_eq!(
        *reference
            .send(Box::new(7_u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        7
    );
    native.shutdown_internal().await.unwrap();
}
