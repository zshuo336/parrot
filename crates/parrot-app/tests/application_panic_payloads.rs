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
    hook_phase, plan, AppManifest, ComponentSpec, LocalTopology, ParrotAppHook,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Spawn,
    Clone,
    Lookup,
    Send(u8),
    Liveness,
    GatewayStop,
}
#[derive(Debug)]
struct Probe {
    failure: Failure,
    enabled: AtomicBool,
    payload_drops: AtomicUsize,
    retired: AtomicUsize,
    hooks: Mutex<Vec<(String, u8)>>,
}
struct Payload(Arc<Probe>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.payload_drops.fetch_add(1, Ordering::SeqCst);
        panic!("application callback panic payload destructor");
    }
}
impl Probe {
    fn inject(self: &Arc<Self>, failure: Failure, path: &str) {
        if self.enabled.load(Ordering::SeqCst)
            && self.failure == failure
            && path.starts_with("/user/bad")
        {
            std::panic::panic_any(Payload(self.clone()));
        }
    }
}
struct Echo {
    path: String,
    probe: Arc<Probe>,
}
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(hook) = message.downcast_ref::<ParrotAppHook>() {
                self.probe
                    .hooks
                    .lock()
                    .unwrap()
                    .push((self.path.clone(), hook.0));
            }
            Ok(message)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        self.probe.retired.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Debug)]
struct Reference {
    target: BoxedActorRef,
    path: String,
    probe: Arc<Probe>,
}
impl ActorRef for Reference {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(hook) = message.downcast_ref::<ParrotAppHook>() {
                self.probe.inject(Failure::Send(hook.0), &self.path);
            }
            self.target.send(message).await
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
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        Box::pin(async move {
            self.probe.inject(Failure::Liveness, &self.path);
            self.target.is_alive().await
        })
    }
    fn path(&self) -> String {
        self.path.clone()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        self.probe.inject(Failure::Clone, &self.path);
        Box::new(Self {
            target: self.target.clone_boxed(),
            path: self.path.clone(),
            probe: self.probe.clone(),
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Host {
    engine: Arc<ThreadActorSystem>,
    probe: Arc<Probe>,
    originals: Mutex<HashMap<String, BoxedActorRef>>,
}
impl Host {
    fn new(failure: Failure) -> Self {
        Self {
            engine: ThreadActorSystem::shared(Default::default()),
            probe: Arc::new(Probe {
                failure,
                enabled: AtomicBool::new(true),
                payload_drops: AtomicUsize::new(0),
                retired: AtomicUsize::new(0),
                hooks: Mutex::new(Vec::new()),
            }),
            originals: Mutex::new(HashMap::new()),
        }
    }
    fn deployer(&self) -> LocalDeployer<'_> {
        LocalDeployer {
            topology: &LocalTopology,
            parrot_spawner: self,
            gateway_factory: self,
        }
    }
    async fn spawn(&self, paths: &[String]) -> Result<Vec<BoxedActorRef>, DeployError> {
        let mut refs = Vec::new();
        for path in paths {
            self.probe.inject(Failure::Spawn, path);
            let actor = self
                .engine
                .spawn_at(
                    Echo {
                        path: path.clone(),
                        probe: self.probe.clone(),
                    },
                    path,
                    None,
                    Default::default(),
                )
                .await
                .map_err(|error| DeployError::Engine(error.to_string()))?;
            self.originals
                .lock()
                .unwrap()
                .insert(path.clone(), Box::new(actor.clone()));
            refs.push(Box::new(Reference {
                target: Box::new(actor),
                path: path.clone(),
                probe: self.probe.clone(),
            }) as BoxedActorRef);
        }
        Ok(refs)
    }
    async fn lookup(&self, path: &str) -> Result<BoxedActorRef, DeployError> {
        self.probe.inject(Failure::Lookup, path);
        Err(DeployError::Engine(format!("no child {path}")))
    }
    async fn finish(&self, context: &mut AssemblingContext) {
        self.probe.enabled.store(false, Ordering::SeqCst);
        context.teardown(&self.deployer()).await.unwrap();
        self.engine.shutdown_internal().await.unwrap();
    }
    fn retired(&self, path: &str) -> bool {
        self.originals
            .lock()
            .unwrap()
            .get(path)
            .is_some_and(|actor| {
                matches!(actor.retirement_status(), RetirementStatus::Retired(Ok(())))
            })
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
    async fn hook_target(&self, path: &str) -> Result<BoxedActorRef, DeployError> {
        self.lookup(path).await
    }
    async fn stop_paths(&self, _: &[String]) -> Result<(), DeployError> {
        panic!("cleanup must stop original references")
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
    async fn hook_target(
        &self,
        _: &ComponentSpec,
        path: &str,
    ) -> Result<BoxedActorRef, DeployError> {
        self.lookup(path).await
    }
    async fn stop_gateway(&self, spec: &ComponentSpec) -> Result<(), DeployError> {
        let path = format!("/user/{}", spec.name);
        self.probe.inject(Failure::GatewayStop, &path);
        let actor = self.originals.lock().unwrap()[&path].clone_boxed();
        actor
            .stop()
            .await
            .map_err(|error| DeployError::Engine(error.to_string()))
    }
}

fn manifest(gateway: bool, hook: Option<(&str, &str)>, wire: bool) -> AppManifest {
    let mut text = String::from("name='panic-payload'\nversion='1.0.0'\n[[components]]\nname='healthy'\nengine='parrot'\nartifact={Props={factory='echo'}}\n[[components]]\nname='bad'\ndeps=['healthy']\n");
    if gateway {
        text.push_str("engine='erlang'\nartifact={Beam={app='echo'}}\n");
    } else {
        text.push_str("engine='parrot'\nartifact={Props={factory='echo'}}\n");
    }
    if let Some((phase, path)) = hook {
        text.push_str(&format!("[components.hooks]\n{phase}='{path}'\n"));
    }
    if wire {
        text.push_str("[[wiring]]\nfrom='healthy:/user/healthy'\nto='bad:/user/bad'\n");
    }
    AppManifest::from_toml(&text).unwrap()
}

async fn assembly_failure(failure: Failure, gateway: bool, hook: Option<(&str, &str)>, wire: bool) {
    let host = Host::new(failure);
    let manifest = manifest(gateway, hook, wire);
    let mut context = AssemblingContext::new();
    let result = std::panic::AssertUnwindSafe(context.assemble(
        &plan(&manifest, &LocalTopology).unwrap(),
        &host.deployer(),
        &manifest,
        &Default::default(),
    ))
    .catch_unwind()
    .await;
    let retired_before_fallback = host
        .originals
        .lock()
        .unwrap()
        .values()
        .all(|actor| matches!(actor.retirement_status(), RetirementStatus::Retired(Ok(()))));
    let drops_before_fallback = host.probe.retired.load(Ordering::SeqCst);
    let context_cleared = context.started().is_empty();
    let never_published = context.wired_ref("healthy:/user/healthy").is_none();
    let created = host.originals.lock().unwrap().len();
    host.finish(&mut context).await;
    let error = result
        .expect("panic payload destructor escaped assembly")
        .unwrap_err();
    assert!(error.to_string().contains("panic"), "{error}");
    assert!(retired_before_fallback, "prior actor rollback was skipped");
    assert_eq!(
        drops_before_fallback, created,
        "owned rollback required fallback cleanup"
    );
    assert!(
        context_cleared,
        "rollback left already-owned actors registered"
    );
    assert!(never_published);
    assert_eq!(host.probe.retired.load(Ordering::SeqCst), created);
    assert_eq!(host.probe.payload_drops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn spawner_panic_payload_cannot_skip_prior_actor_rollback() {
    assembly_failure(Failure::Spawn, false, None, false).await;
}
#[tokio::test]
async fn gateway_start_panic_payload_cannot_skip_prior_actor_rollback() {
    assembly_failure(Failure::Spawn, true, None, false).await;
}
#[tokio::test]
async fn hook_clone_panic_payload_cannot_skip_owned_actor_rollback() {
    assembly_failure(Failure::Clone, false, Some(("on_start", ".")), false).await;
}
#[tokio::test]
async fn local_hook_lookup_panic_payload_cannot_skip_owned_actor_rollback() {
    assembly_failure(Failure::Lookup, false, Some(("on_start", "child")), false).await;
}
#[tokio::test]
async fn gateway_hook_lookup_panic_payload_cannot_skip_owned_actor_rollback() {
    assembly_failure(Failure::Lookup, true, Some(("on_start", "child")), false).await;
}
#[tokio::test]
async fn wire_clone_panic_payload_cannot_skip_owned_actor_rollback() {
    assembly_failure(Failure::Clone, false, None, true).await;
}
#[tokio::test]
async fn start_hook_panic_payload_cannot_skip_owned_actor_rollback() {
    assembly_failure(
        Failure::Send(hook_phase::START),
        false,
        Some(("on_start", ".")),
        false,
    )
    .await;
}

async fn post_start_failure(failure: Failure, drain: bool, gateway: bool) {
    let host = Host::new(failure);
    host.probe.enabled.store(false, Ordering::SeqCst);
    let mut manifest = manifest(gateway, Some(("on_drain", ".")), false);
    manifest.components[0].hooks.on_drain = Some(".".into());
    if failure == Failure::Send(hook_phase::STOP) {
        manifest.components[1].hooks.on_stop = Some(".".into());
    }
    let mut context = AssemblingContext::new();
    context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &host.deployer(),
            &manifest,
            &Default::default(),
        )
        .await
        .unwrap();
    host.probe.enabled.store(true, Ordering::SeqCst);
    let result = std::panic::AssertUnwindSafe(async {
        if drain {
            context.drain(&host.deployer()).await
        } else {
            context.teardown(&host.deployer()).await
        }
    })
    .catch_unwind()
    .await;
    let healthy_retired = host.retired("/user/healthy");
    let bad_retired = host.retired("/user/bad");
    let healthy_drained = host
        .probe
        .hooks
        .lock()
        .unwrap()
        .contains(&("/user/healthy".into(), hook_phase::DRAIN));
    let remaining = context.started().to_vec();
    let retry_owned = if drain || gateway {
        let actor = host.originals.lock().unwrap()["/user/bad"].clone_boxed();
        *actor
            .send(Box::new(71u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap()
            == 71
    } else {
        true
    };
    host.finish(&mut context).await;
    let error = result
        .expect("panic payload destructor escaped lifecycle")
        .unwrap_err();
    assert!(error.to_string().contains("panic"), "{error}");
    assert!(healthy_drained, "healthy sibling hook was skipped");
    assert_eq!(healthy_retired, !drain);
    assert_eq!(bad_retired, !drain && !gateway);
    assert_eq!(
        remaining,
        if drain {
            vec!["healthy", "bad"]
        } else if gateway {
            vec!["bad"]
        } else {
            vec![]
        }
    );
    assert!(retry_owned);
    assert_eq!(host.probe.retired.load(Ordering::SeqCst), 2);
    assert_eq!(host.probe.payload_drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        host.probe
            .hooks
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, phase)| path == "/user/healthy" && *phase == hook_phase::DRAIN)
            .count(),
        1,
        "successful sibling hook was replayed during cleanup retry"
    );
}

#[tokio::test]
async fn drain_hook_panic_payload_cannot_skip_healthy_sibling_or_retry() {
    post_start_failure(Failure::Send(hook_phase::DRAIN), true, false).await;
}
#[tokio::test]
async fn stop_hook_panic_payload_cannot_skip_owned_termination() {
    post_start_failure(Failure::Send(hook_phase::STOP), false, false).await;
}
#[tokio::test]
async fn liveness_panic_payload_cannot_skip_healthy_sibling_or_retry() {
    post_start_failure(Failure::Liveness, true, false).await;
}
#[tokio::test]
async fn gateway_stop_panic_payload_preserves_failure_and_healthy_sibling_cleanup() {
    post_start_failure(Failure::GatewayStop, false, true).await;
}

#[tokio::test]
async fn context_can_reassemble_after_panic_payload_rollback() {
    let host = Host::new(Failure::Send(hook_phase::START));
    let mut context = AssemblingContext::new();
    let failing = manifest(false, Some(("on_start", ".")), false);
    let result = std::panic::AssertUnwindSafe(context.assemble(
        &plan(&failing, &LocalTopology).unwrap(),
        &host.deployer(),
        &failing,
        &Default::default(),
    ))
    .catch_unwind()
    .await;
    let rolled_back =
        context.started().is_empty() && host.retired("/user/healthy") && host.retired("/user/bad");
    host.finish(&mut context).await;
    assert!(result
        .expect("panic payload destructor escaped rollback")
        .is_err());
    assert!(rolled_back);
    // Reuse the context with a fresh engine, so shutdown of the old engine
    // cannot account for the new assembly's business replies or retirement.
    let replacement = Host::new(Failure::Spawn);
    replacement.probe.enabled.store(false, Ordering::SeqCst);
    let healthy = manifest(false, Some(("on_start", ".")), true);
    context
        .assemble(
            &plan(&healthy, &LocalTopology).unwrap(),
            &replacement.deployer(),
            &healthy,
            &Default::default(),
        )
        .await
        .unwrap();
    let wire = context.wired_ref("healthy:/user/healthy").unwrap();
    assert_eq!(
        *wire
            .send(Box::new(83u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        83
    );
    replacement.finish(&mut context).await;
    assert_eq!(host.probe.payload_drops.load(Ordering::SeqCst), 1);
    assert_eq!(host.probe.retired.load(Ordering::SeqCst), 2);
    assert_eq!(replacement.probe.retired.load(Ordering::SeqCst), 2);
    assert_eq!(replacement.probe.payload_drops.load(Ordering::SeqCst), 0);
    assert!(context.started().is_empty());
}

async fn successful_lifecycle(gateway: bool) {
    let host = Host::new(Failure::Spawn);
    host.probe.enabled.store(false, Ordering::SeqCst);
    let mut manifest = manifest(gateway, Some(("on_start", ".")), true);
    for spec in &mut manifest.components {
        spec.hooks.on_start = Some(".".into());
        spec.hooks.on_drain = Some(".".into());
        spec.hooks.on_stop = Some(".".into());
    }
    let mut context = AssemblingContext::new();
    context
        .assemble(
            &plan(&manifest, &LocalTopology).unwrap(),
            &host.deployer(),
            &manifest,
            &Default::default(),
        )
        .await
        .unwrap();
    let wire = context.wired_ref("healthy:/user/healthy").unwrap();
    assert_eq!(
        *wire
            .send(Box::new(97u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        97
    );
    context.drain(&host.deployer()).await.unwrap();
    context.drain(&host.deployer()).await.unwrap();
    host.finish(&mut context).await;
    assert!(host.retired("/user/healthy") && host.retired("/user/bad"));
    let hooks = host.probe.hooks.lock().unwrap();
    for path in ["/user/healthy", "/user/bad"] {
        for phase in [hook_phase::START, hook_phase::DRAIN, hook_phase::STOP] {
            assert_eq!(
                hooks
                    .iter()
                    .filter(|(p, h)| p == path && *h == phase)
                    .count(),
                1
            );
        }
    }
    assert_eq!(host.probe.payload_drops.load(Ordering::SeqCst), 0);
    assert_eq!(host.probe.retired.load(Ordering::SeqCst), 2);
    assert!(context.started().is_empty());
}

#[tokio::test]
async fn local_control_delivers_wire_replies_and_runs_each_hook_once() {
    successful_lifecycle(false).await;
}

#[tokio::test]
async fn gateway_adapter_control_delivers_wire_replies_and_runs_each_hook_once() {
    successful_lifecycle(true).await;
}
