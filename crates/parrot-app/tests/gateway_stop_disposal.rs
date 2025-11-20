#![cfg(feature = "host")]

use futures::FutureExt;
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorRef, RetirementStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::{AssemblingContext, DeployError, GatewayFactory, LocalDeployer, ParrotSpawner},
    plan, AppManifest, ComponentSpec, LocalTopology,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Clean,
    Pending,
    Ready,
    PollPanic,
    PayloadPanic,
}
struct State {
    enabled: AtomicBool,
    futures: AtomicUsize,
    payloads: AtomicUsize,
    actors: AtomicUsize,
    polls: AtomicUsize,
}
struct Payload(Arc<State>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("gateway stop payload disposal");
    }
}
struct Echo(Arc<State>);
impl Actor for Echo {
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
        Box::pin(async move { Ok(message) })
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        self.0.actors.fetch_add(1, Ordering::SeqCst);
    }
}
// This is the future directly returned by the host, with separate poll and Drop
// failures. No nested async value unwinds inside its own poll implementation.
struct Stop {
    state: Arc<State>,
    mode: Mode,
    inner: Option<BoxedFuture<'static, Result<(), DeployError>>>,
}
impl Future for Stop {
    type Output = Result<(), DeployError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.state.polls.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            Mode::Clean => self.inner.as_mut().unwrap().as_mut().poll(cx),
            Mode::Pending => Poll::Pending,
            Mode::Ready => Poll::Ready(Ok(())),
            Mode::PollPanic => panic!("gateway stop poll failure"),
            Mode::PayloadPanic => std::panic::panic_any(Payload(self.state.clone())),
        }
    }
}
impl Drop for Stop {
    fn drop(&mut self) {
        self.state.futures.fetch_add(1, Ordering::SeqCst);
        if self.mode != Mode::Clean {
            std::panic::panic_any(Payload(self.state.clone()));
        }
    }
}
struct Host {
    engine: Arc<ThreadActorSystem>,
    state: Arc<State>,
    originals: Mutex<Vec<BoxedActorRef>>,
    mode: Mode,
}
impl Host {
    fn new(mode: Mode) -> Self {
        Self {
            engine: ThreadActorSystem::shared(Default::default()),
            state: Arc::new(State {
                enabled: AtomicBool::new(true),
                futures: AtomicUsize::new(0),
                payloads: AtomicUsize::new(0),
                actors: AtomicUsize::new(0),
                polls: AtomicUsize::new(0),
            }),
            originals: Mutex::new(Vec::new()),
            mode,
        }
    }
    fn deployer(&self) -> LocalDeployer<'_> {
        LocalDeployer {
            topology: &LocalTopology,
            gateway_factory: self,
            parrot_spawner: self,
        }
    }
    async fn spawn(&self, paths: &[String]) -> Result<Vec<BoxedActorRef>, DeployError> {
        let mut refs = Vec::new();
        for path in paths {
            let actor = self
                .engine
                .spawn_at(Echo(self.state.clone()), path, None, Default::default())
                .await
                .unwrap();
            actor.send(Box::new(7u64)).await.unwrap();
            self.originals.lock().unwrap().push(Box::new(actor.clone()));
            refs.push(Box::new(actor) as BoxedActorRef);
        }
        Ok(refs)
    }
    fn retired(&self, index: usize) -> bool {
        matches!(
            self.originals.lock().unwrap()[index].retirement_status(),
            RetirementStatus::Retired(Ok(()))
        )
    }
}
#[async_trait::async_trait]
impl ParrotSpawner for Host {
    fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        Ok(())
    }
    async fn spawn_props(
        &self,
        _: &str,
        paths: &[String],
        _: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        self.spawn(paths).await
    }
    async fn stop_paths(&self, _: &[String]) -> Result<(), DeployError> {
        panic!("must stop original local identities")
    }
}
#[async_trait::async_trait]
impl GatewayFactory for Host {
    async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
        Ok(())
    }
    async fn start_gateway(
        &self,
        _: &ComponentSpec,
        paths: &[String],
        _: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        self.spawn(paths).await
    }
    fn stop_gateway<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _: &'life1 ComponentSpec,
    ) -> Pin<Box<dyn Future<Output = Result<(), DeployError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        let mode = if self.state.enabled.load(Ordering::SeqCst) {
            self.mode
        } else {
            Mode::Clean
        };
        let inner = if mode == Mode::Clean {
            let actor = self.originals.lock().unwrap()[1].clone_boxed();
            Some(Box::pin(async move {
                parrot_api::address::stop_for_owned_cleanup(&*actor)
                    .await
                    .map_err(|error| DeployError::Engine(error.to_string()))
            })
                as BoxedFuture<'static, Result<(), DeployError>>)
        } else {
            None
        };
        Box::pin(Stop {
            state: self.state.clone(),
            mode,
            inner,
        })
    }
}
async fn scenario(mode: Mode, cancel: bool) {
    let host = Host::new(mode);
    let manifest = AppManifest::from_toml("name='stop-disposal'\nversion='1.0.0'\n[[components]]\nname='healthy'\nengine='parrot'\nartifact={Props={factory='echo'}}\n[[components]]\nname='gateway'\ndeps=['healthy']\nengine='erlang'\nartifact={Beam={app='echo'}}\n").unwrap();
    let deployer = host.deployer();
    let mut context = AssemblingContext::new();
    context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &deployer,
            &manifest,
            &parrot_config::ParrotConfig::builder().thread_shutdown_timeout_ms(15),
        )
        .await
        .unwrap();
    let mut escaped = false;
    if cancel {
        let mut cleanup = Box::pin(context.teardown(&deployer));
        assert!(cleanup.as_mut().now_or_never().is_none());
        assert!(host.state.polls.load(Ordering::SeqCst) > 0);
        let cancellation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(cleanup)));
        escaped = cancellation.is_err();
        if let Err(payload) = cancellation {
            assert!(payload.is::<Payload>());
            // Dispose the baseline's throwing payload outside its unwind so the
            // regression reports an assertion after actual fallback cleanup.
            if let Err(nested) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                drop(nested);
            }
        }
        assert_eq!(context.started(), &["healthy", "gateway"]);
        assert!(!host.retired(0) && !host.retired(1));
    } else if mode == Mode::Clean {
        context.teardown(&deployer).await.unwrap();
        assert!(context.started().is_empty());
        assert!(host.retired(0) && host.retired(1));
    } else {
        let error = context.teardown(&deployer).await.unwrap_err();
        assert!(
            error.to_string().contains(if mode == Mode::Pending {
                "deadline"
            } else {
                "panicked"
            }),
            "{error}"
        );
        assert_eq!(context.started(), &["gateway"]);
        assert!(
            host.retired(0),
            "one gateway failure skipped healthy cleanup"
        );
        assert!(!host.retired(1), "failure incorrectly claimed retirement");
    }
    assert_eq!(host.state.futures.load(Ordering::SeqCst), 1);
    assert_eq!(
        host.state.payloads.load(Ordering::SeqCst),
        if mode == Mode::Clean {
            0
        } else if mode == Mode::PayloadPanic {
            2
        } else {
            1
        }
    );
    if mode != Mode::Clean {
        let original = host.originals.lock().unwrap()[1].clone_boxed();
        assert_eq!(
            *original
                .send(Box::new(19u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            19,
            "retained gateway business must remain observable"
        );
    }
    host.state.enabled.store(false, Ordering::SeqCst);
    context.teardown(&deployer).await.unwrap();
    assert!(context.started().is_empty());
    assert!(host.retired(0) && host.retired(1));
    assert_eq!(host.state.actors.load(Ordering::SeqCst), 2);
    assert_eq!(
        host.state.futures.load(Ordering::SeqCst),
        if mode == Mode::Clean { 1 } else { 2 }
    );
    host.engine.shutdown_internal().await.unwrap();
    assert!(!escaped, "gateway stop disposal escaped cancellation");
}
fn isolated(name: &str, mode: Mode, cancel: bool) {
    const FLAG: &str = "PARROT_GATEWAY_STOP_DISPOSAL_CHILD";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(scenario(mode, cancel));
        return;
    }
    let log = tempfile::tempfile().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(FLAG, name)
        .env("RUST_BACKTRACE", "0")
        .stdout(log.try_clone().unwrap())
        .stderr(log.try_clone().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            use std::io::{Read, Seek};
            let mut log = log;
            log.rewind().unwrap();
            let mut output = String::new();
            log.read_to_string(&mut output).unwrap();
            assert!(status.success(), "child {name}: {status}\n{output}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("gateway cleanup contract exceeded process deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn gateway_stop_cancellation_isolates_disposal_and_retains_originals() {
    isolated(
        "gateway_stop_cancellation_isolates_disposal_and_retains_originals",
        Mode::Pending,
        true,
    );
}
#[test]
fn gateway_stop_poll_and_disposal_panics_do_not_abort() {
    isolated(
        "gateway_stop_poll_and_disposal_panics_do_not_abort",
        Mode::PollPanic,
        false,
    );
}
#[test]
fn gateway_stop_throwing_poll_payload_and_disposal_are_independent() {
    isolated(
        "gateway_stop_throwing_poll_payload_and_disposal_are_independent",
        Mode::PayloadPanic,
        false,
    );
}
#[test]
fn gateway_stop_timeout_continues_cleanup_and_retains_retry() {
    isolated(
        "gateway_stop_timeout_continues_cleanup_and_retains_retry",
        Mode::Pending,
        false,
    );
}
#[test]
fn gateway_stop_success_with_disposal_failure_is_not_retirement() {
    isolated(
        "gateway_stop_success_with_disposal_failure_is_not_retirement",
        Mode::Ready,
        false,
    );
}
#[test]
fn gateway_stop_clean_completion_retires_real_actors_once() {
    isolated(
        "gateway_stop_clean_completion_retires_real_actors_once",
        Mode::Clean,
        false,
    );
}
