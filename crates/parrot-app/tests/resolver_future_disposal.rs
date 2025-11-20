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
    manifest::{AppManifest, ComponentSpec},
    planner::{plan, LocalTopology},
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

#[derive(Default, Debug)]
struct State {
    fault_enabled: AtomicBool,
    future_drops: AtomicUsize,
    payload_drops: AtomicUsize,
    actor_drops: AtomicUsize,
    resolving: tokio::sync::Notify,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FaultSite {
    Lookup,
    Dispatch,
    Liveness,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FaultMode {
    Construct,
    Poll,
    Ready,
}
struct Fault<T> {
    state: Arc<State>,
    mode: FaultMode,
    output: Option<T>,
}
impl<T: Unpin> Future for Fault<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        if this.mode == FaultMode::Poll {
            std::panic::panic_any(Payload(this.state.clone()));
        }
        Poll::Ready(this.output.take().unwrap())
    }
}
impl<T> Drop for Fault<T> {
    fn drop(&mut self) {
        self.state.future_drops.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Payload(self.state.clone()));
    }
}
fn fault<T: Send + Unpin + 'static>(
    state: &Arc<State>,
    mode: FaultMode,
    output: T,
) -> BoxedFuture<'static, T> {
    if mode == FaultMode::Construct {
        std::panic::panic_any(Payload(state.clone()));
    }
    Box::pin(Fault {
        state: state.clone(),
        mode,
        output: Some(output),
    })
}
struct Payload(Arc<State>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.payload_drops.fetch_add(1, Ordering::SeqCst);
        panic!("resolver destructor panic payload destructor");
    }
}
struct PendingOwner(Arc<State>);
impl Drop for PendingOwner {
    fn drop(&mut self) {
        self.0.future_drops.fetch_add(1, Ordering::SeqCst);
        std::panic::panic_any(Payload(self.0.clone()));
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
        self.0.actor_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Host {
    engine: Arc<ThreadActorSystem>,
    state: Arc<State>,
    originals: Mutex<Vec<BoxedActorRef>>,
    dispatch: bool,
    fault: Option<(FaultSite, FaultMode)>,
}
#[derive(Debug)]
struct PendingHookRef {
    target: BoxedActorRef,
    state: Arc<State>,
    fault: Option<(FaultSite, FaultMode)>,
}
impl ActorRef for PendingHookRef {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        if self.state.fault_enabled.load(Ordering::SeqCst) {
            if let Some((FaultSite::Dispatch, mode)) = self.fault {
                return fault(&self.state, mode, Ok(message));
            }
        }
        if self.fault.is_some() {
            return self.target.send(message);
        }
        Box::pin(async move {
            if message.is::<parrot_app::ParrotAppHook>() {
                let _owner = PendingOwner(self.state.clone());
                self.state.resolving.notify_one();
                std::future::pending().await
            } else {
                self.target.send(message).await
            }
        })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.target.deliver(message)
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        self.target.stop()
    }
    fn retirement_status(&self) -> RetirementStatus {
        self.target.retirement_status()
    }
    fn path(&self) -> String {
        self.target.path()
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        if self.state.fault_enabled.load(Ordering::SeqCst) {
            if let Some((FaultSite::Liveness, mode)) = self.fault {
                return fault(&self.state, mode, true);
            }
        }
        self.target.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            target: self.target.clone_boxed(),
            state: self.state.clone(),
            fault: self.fault,
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
impl Host {
    fn new(dispatch: bool) -> Self {
        Self {
            engine: ThreadActorSystem::shared(Default::default()),
            state: Arc::new(State::default()),
            originals: Mutex::new(Vec::new()),
            dispatch,
            fault: None,
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
        let mut actors = Vec::new();
        for path in paths {
            let actor = self
                .engine
                .spawn_at(Echo(self.state.clone()), path, None, Default::default())
                .await
                .unwrap();
            actor.send(Box::new(7u64)).await.unwrap();
            self.originals.lock().unwrap().push(actor.clone_boxed());
            if self.dispatch && path == "/user/pending" {
                actors.push(Box::new(PendingHookRef {
                    target: Box::new(actor),
                    state: self.state.clone(),
                    fault: self.fault,
                }) as BoxedActorRef);
            } else {
                actors.push(Box::new(actor) as BoxedActorRef);
            }
        }
        Ok(actors)
    }
    async fn pending_target(&self) -> Result<BoxedActorRef, DeployError> {
        let _owner = PendingOwner(self.state.clone());
        self.state.resolving.notify_one();
        std::future::pending().await
    }
    fn target(&self) -> BoxedFuture<'_, Result<BoxedActorRef, DeployError>> {
        if self.state.fault_enabled.load(Ordering::SeqCst) {
            if let Some((FaultSite::Lookup, mode)) = self.fault {
                let target = self.originals.lock().unwrap()[1].clone_boxed();
                return fault(&self.state, mode, Ok(target));
            }
        }
        Box::pin(self.pending_target())
    }
    fn retired(&self) -> bool {
        let actors = self.originals.lock().unwrap();
        actors.len() == 2
            && actors
                .iter()
                .all(|actor| matches!(actor.retirement_status(), RetirementStatus::Retired(Ok(()))))
    }
    async fn close(&self, context: &mut AssemblingContext) {
        context.teardown(&self.deployer()).await.unwrap();
        self.engine.shutdown_internal().await.unwrap();
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
    fn hook_target<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _: &'life1 str,
    ) -> BoxedFuture<'async_trait, Result<BoxedActorRef, DeployError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        self.target()
    }
    async fn stop_paths(&self, _: &[String]) -> Result<(), DeployError> {
        panic!("cleanup must stop the original identities")
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
    fn hook_target<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        _: &'life1 ComponentSpec,
        _: &'life2 str,
    ) -> BoxedFuture<'async_trait, Result<BoxedActorRef, DeployError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        self.target()
    }
    async fn stop_gateway(&self, _: &ComponentSpec) -> Result<(), DeployError> {
        let actor = self.originals.lock().unwrap()[1].clone_boxed();
        actor
            .stop()
            .await
            .map_err(|error| DeployError::Engine(error.to_string()))
    }
}
fn manifest(gateway: bool, hook: bool) -> AppManifest {
    let mut text = String::from("name='resolver-disposal'\nversion='1.0.0'\n[[components]]\nname='healthy'\nengine='parrot'\nartifact={Props={factory='echo'}}\n[[components]]\nname='pending'\ndeps=['healthy']\n");
    text.push_str(if gateway {
        "engine='erlang'\nartifact={Beam={app='echo'}}\n"
    } else {
        "engine='parrot'\nartifact={Props={factory='echo'}}\n"
    });
    if hook {
        text.push_str("[components.hooks]\non_start='child'\n");
    } else {
        text.push_str("[[wiring]]\nfrom='healthy:/user/out'\nto='pending:/user/pending/child'\n");
    }
    AppManifest::from_toml(&text).unwrap()
}
async fn timeout_rolls_back(gateway: bool, hook: bool) {
    timeout_case(gateway, hook, false).await;
}
async fn timeout_case(gateway: bool, hook: bool, dispatch: bool) {
    let host = Host::new(dispatch);
    let mut manifest = manifest(gateway, hook);
    if dispatch {
        manifest.components[1].hooks.on_start = Some(".".into());
    }
    let mut context = AssemblingContext::new();
    context.set_hooks_timeout(Duration::from_millis(20));
    let result = std::panic::AssertUnwindSafe(context.assemble(
        &plan(&manifest, &LocalTopology).unwrap(),
        &host.deployer(),
        &manifest,
        &Default::default(),
    ))
    .catch_unwind()
    .await;
    let result = match result {
        Ok(result) => Some(result),
        Err(payload) => {
            if let Err(nested) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                drop(nested);
            }
            None
        }
    };
    let retired_before_fallback = host.retired();
    let disposed_before_fallback = host.state.actor_drops.load(Ordering::SeqCst);
    let cleared_before_fallback = context.started().is_empty();
    host.close(&mut context).await;
    let error = result
        .expect("resolver future Drop escaped timeout")
        .unwrap_err();
    assert!(error.to_string().contains("timeout") || error.to_string().contains("timed out"));
    assert!(
        retired_before_fallback,
        "automatic rollback skipped original actors"
    );
    assert_eq!(
        disposed_before_fallback, 2,
        "fallback cleanup hid skipped rollback"
    );
    assert!(cleared_before_fallback);
    assert_eq!(host.state.future_drops.load(Ordering::SeqCst), 1);
    assert_eq!(host.state.payload_drops.load(Ordering::SeqCst), 1);
}
async fn cancellation_retains_cleanup(gateway: bool) {
    cancellation_case(gateway, false).await;
}
async fn cancellation_case(gateway: bool, dispatch: bool) {
    let host = Host::new(dispatch);
    let mut manifest = manifest(gateway, dispatch);
    if dispatch {
        manifest.components[1].hooks.on_start = Some(".".into());
    }
    let plan = plan(&manifest, &LocalTopology).unwrap();
    let deployer = host.deployer();
    let cfg = Default::default();
    let mut context = AssemblingContext::new();
    let mut assembly = Box::pin(context.assemble(&plan, &deployer, &manifest, &cfg));
    tokio::select! {
        result = &mut assembly => panic!("unexpected assembly result: {result:?}"),
        ready = tokio::time::timeout(Duration::from_secs(3), host.state.resolving.notified()) => ready.unwrap(),
    }
    let cancelled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(assembly)));
    let payload = cancelled.err();
    // The baseline panic payload itself throws on Drop; isolate that too so
    // a failing regression remains an ordinary assertion, not a test abort.
    if let Some(payload) = payload.as_ref() {
        assert!(payload.is::<Payload>());
    }
    let escaped = payload.is_some();
    if let Err(nested) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload))) {
        drop(nested);
    }
    assert_eq!(context.started().len(), 2);
    assert!(context.wired_ref("healthy:/user/out").is_none());
    assert!(!host.retired());
    let original = host.originals.lock().unwrap()[0].clone_boxed();
    assert_eq!(
        *original
            .send(Box::new(19u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        19
    );
    host.close(&mut context).await;
    assert!(host.retired());
    assert_eq!(host.state.actor_drops.load(Ordering::SeqCst), 2);
    assert_eq!(host.state.future_drops.load(Ordering::SeqCst), 1);
    assert_eq!(host.state.payload_drops.load(Ordering::SeqCst), 1);
    assert!(
        !escaped,
        "resolver future Drop escaped assembly cancellation"
    );
}

#[tokio::test]
async fn local_hook_timeout_disposes_resolver_and_rolls_back() {
    timeout_rolls_back(false, true).await;
}
#[tokio::test]
async fn gateway_hook_timeout_disposes_resolver_and_rolls_back() {
    timeout_rolls_back(true, true).await;
}
#[tokio::test]
async fn local_wire_timeout_disposes_resolver_and_rolls_back() {
    timeout_rolls_back(false, false).await;
}
#[tokio::test]
async fn gateway_wire_timeout_disposes_resolver_and_rolls_back() {
    timeout_rolls_back(true, false).await;
}
#[tokio::test]
async fn local_wire_cancellation_isolates_resolver_and_retains_owned_cleanup() {
    cancellation_retains_cleanup(false).await;
}
#[tokio::test]
async fn gateway_wire_cancellation_isolates_resolver_and_retains_owned_cleanup() {
    cancellation_retains_cleanup(true).await;
}

#[tokio::test]
async fn local_hook_dispatch_timeout_disposes_waiter_and_rolls_back() {
    timeout_case(false, true, true).await;
}
#[tokio::test]
async fn gateway_hook_dispatch_timeout_disposes_waiter_and_rolls_back() {
    timeout_case(true, true, true).await;
}
#[tokio::test]
async fn local_hook_dispatch_cancellation_retains_owned_cleanup() {
    cancellation_case(false, true).await;
}
#[tokio::test]
async fn gateway_hook_dispatch_cancellation_retains_owned_cleanup() {
    cancellation_case(true, true).await;
}

async fn nested_case(gateway: bool, hook: bool, site: FaultSite, mode: FaultMode) {
    let mut host = Host::new(site != FaultSite::Lookup);
    host.fault = Some((site, mode));
    host.state.fault_enabled.store(true, Ordering::SeqCst);
    let mut manifest = manifest(gateway, hook);
    if site == FaultSite::Dispatch {
        manifest.components[1].hooks.on_start = Some(".".into());
    } else if site == FaultSite::Liveness {
        manifest.components[1].hooks.on_start = None;
        manifest.components[1].hooks.on_drain = Some(".".into());
    }
    let mut context = AssemblingContext::new();
    let plan = plan(&manifest, &LocalTopology).unwrap();
    let deployer = host.deployer();
    let config = Default::default();
    let result = if site == FaultSite::Liveness {
        context
            .assemble(&plan, &deployer, &manifest, &config)
            .await
            .unwrap();
        std::panic::AssertUnwindSafe(context.teardown(&deployer))
            .catch_unwind()
            .await
    } else {
        std::panic::AssertUnwindSafe(context.assemble(&plan, &deployer, &manifest, &config))
            .catch_unwind()
            .await
    };
    let retired = host.retired();
    let disposed = host.state.actor_drops.load(Ordering::SeqCst);
    let cleared = context.started().is_empty();
    host.state.fault_enabled.store(false, Ordering::SeqCst);
    host.close(&mut context).await;
    let result = result.unwrap_or_else(|payload| {
        if let Err(nested) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
        {
            drop(nested);
        }
        panic!("nested host callback panic escaped the application boundary");
    });
    let error = result.expect_err("failed callback disposal must not publish success");
    assert!(error.to_string().contains("panic"), "{error:?}");
    assert!(
        retired,
        "rollback/teardown skipped original actor retirement"
    );
    assert_eq!(disposed, 2, "fallback hid skipped actor destruction");
    assert!(cleared);
    assert_eq!(
        host.state.future_drops.load(Ordering::SeqCst),
        usize::from(mode != FaultMode::Construct)
    );
    assert_eq!(
        host.state.payload_drops.load(Ordering::SeqCst),
        if mode == FaultMode::Poll { 2 } else { 1 }
    );
}

fn isolated_nested(name: &str, gateway: bool, hook: bool, site: FaultSite, mode: FaultMode) {
    const FLAG: &str = "PARROT_NESTED_RESOLVER_CHILD";
    if std::env::var(FLAG).as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(nested_case(gateway, hook, site, mode));
        return;
    }
    let mut log = tempfile::tempfile().unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(FLAG, name)
            .env("RUST_BACKTRACE", "0")
            .stdout(log.try_clone().unwrap())
            .stderr(log.try_clone().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            use std::io::{Read, Seek};
            log.rewind().unwrap();
            let mut output = String::new();
            log.read_to_string(&mut output).unwrap();
            assert!(status.success(), "{name}: {status}\n{output}");
            assert!(
                output.contains("1 passed; 0 failed"),
                "child did not execute the contract: {output}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "nested callback subprocess exceeded deadline"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

macro_rules! nested_contract {
    ($name:ident, $gateway:expr, $hook:expr, $site:ident, $mode:ident) => {
        #[test]
        fn $name() {
            isolated_nested(
                stringify!($name),
                $gateway,
                $hook,
                FaultSite::$site,
                FaultMode::$mode,
            );
        }
    };
}
nested_contract!(nested_local_hook_construct, false, true, Lookup, Construct);
nested_contract!(nested_local_hook_poll_and_drop, false, true, Lookup, Poll);
nested_contract!(nested_local_hook_ready_drop, false, true, Lookup, Ready);
nested_contract!(nested_gateway_hook_construct, true, true, Lookup, Construct);
nested_contract!(nested_gateway_hook_poll_and_drop, true, true, Lookup, Poll);
nested_contract!(nested_gateway_hook_ready_drop, true, true, Lookup, Ready);
nested_contract!(nested_local_wire_construct, false, false, Lookup, Construct);
nested_contract!(nested_local_wire_poll_and_drop, false, false, Lookup, Poll);
nested_contract!(nested_local_wire_ready_drop, false, false, Lookup, Ready);
nested_contract!(
    nested_gateway_wire_construct,
    true,
    false,
    Lookup,
    Construct
);
nested_contract!(nested_gateway_wire_poll_and_drop, true, false, Lookup, Poll);
nested_contract!(nested_gateway_wire_ready_drop, true, false, Lookup, Ready);
nested_contract!(
    nested_local_dispatch_construct,
    false,
    true,
    Dispatch,
    Construct
);
nested_contract!(
    nested_local_dispatch_poll_and_drop,
    false,
    true,
    Dispatch,
    Poll
);
nested_contract!(
    nested_local_dispatch_ready_drop,
    false,
    true,
    Dispatch,
    Ready
);
nested_contract!(
    nested_gateway_dispatch_construct,
    true,
    true,
    Dispatch,
    Construct
);
nested_contract!(
    nested_gateway_dispatch_poll_and_drop,
    true,
    true,
    Dispatch,
    Poll
);
nested_contract!(
    nested_gateway_dispatch_ready_drop,
    true,
    true,
    Dispatch,
    Ready
);
nested_contract!(
    nested_local_liveness_construct,
    false,
    true,
    Liveness,
    Construct
);
nested_contract!(
    nested_local_liveness_poll_and_drop,
    false,
    true,
    Liveness,
    Poll
);
nested_contract!(
    nested_local_liveness_ready_drop,
    false,
    true,
    Liveness,
    Ready
);
