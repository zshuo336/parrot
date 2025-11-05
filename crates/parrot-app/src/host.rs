//! 宿主层（feature `host`——09 §2.2 分层：宿主 Runner 依赖引擎）。
//!
//! A4 交付：
//! - [`ThreadPropsSpawner`]：显式引擎与组件配置传入 [`ThreadPropsFactory`]；
//! - [`cloud_proxy::CloudProxy`]：内置持久发送组件，manifest 工厂为 `parrot.cloud-proxy`；
//! - [`ProcessGatewayFactory`]：akka/ray/erlang 组件 → 真实子进程网关
//!   （本机 spawn + registry 模式注册 + 回环 TCP——Wire 序列化路径与生产一致）。

use futures::FutureExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use parrot::thread::system::ThreadActorSystem;
use parrot_api::types::BoxedActorRef;
use parrot_remote::system::RemoteActorSystem;
use parrot_remote::LocalLookup;

use crate::assemble::{DeployError, GatewayFactory, ParrotSpawner};
use crate::manifest::{ArtifactRef, ComponentSpec, EngineKind};

mod cleanup;
mod references;
use references::OwnedReferences;
mod gateway_admission;
mod gateway_artifacts;
mod gateway_process;
#[cfg(unix)]
#[cfg(test)]
mod group_liveness;
pub use gateway_admission::GatewayProcessBudget;
use gateway_artifacts::PrivateArtifacts;
use gateway_process::OwnedGateway;
pub mod cloud_proxy;
#[cfg(feature = "dylib")]
pub mod dylib;
#[cfg(all(test, unix))]
mod gateway_process_tests;
#[cfg(feature = "wasm")]
pub mod wasm;

// ============================================================================
// parrot 组件 spawn 面（ThreadActorSystem + PropsFactory）
// ============================================================================

/// A local Props factory receives its owning engine and component configuration.
/// It must return an actor registered at `path`, or clean up its own failed spawn.
/// Once invoked, its future remains owned until completion even if the public
/// spawn waiter is cancelled. A permanently pending factory retains that work.
pub type ThreadPropsSpawn =
    fn(
        Arc<ThreadActorSystem>,
        String,
        Option<toml::Table>,
    )
        -> parrot_api::types::BoxedFuture<'static, parrot_api::types::ActorResult<BoxedActorRef>>;
pub struct ThreadPropsFactory {
    pub name: &'static str,
    pub spawn: ThreadPropsSpawn,
}
parrot_api::message::inventory::collect!(ThreadPropsFactory);

/// Cancellation-safe ownership for local Props/Wasm/trusted-native components and partial pools.
///
/// Cancelling a polled spawn does not drop a running factory. Once it returns,
/// the host stops its unclaimed instances in reverse order without starting
/// further pool members. A result is transferred only when the caller receives
/// it. Cleanup uses the original actor identities and survives cancellation of
/// its waiter. Work runs on the engine's configured runtime, independently of
/// the caller's executor. Keep that runtime alive until cleanup can finish;
/// this does not provide an asynchronous drain of a destroyed runtime.
#[derive(Clone)]
pub struct ThreadPropsSpawner {
    engine: Arc<ThreadActorSystem>,
    remote: Option<Arc<RemoteActorSystem>>,
    cleanup: Arc<cleanup::Registry>,
    #[cfg(feature = "wasm")]
    wasm: Arc<wasm::WasmHost>,
    #[cfg(feature = "dylib")]
    dylib: Arc<dylib::DylibHost>,
}

struct HostRemoteGateway {
    remote: std::sync::Weak<RemoteActorSystem>,
    engine: std::sync::Weak<ThreadActorSystem>,
}

impl parrot::system::RemoteGateway for HostRemoteGateway {
    fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        let remote = self.remote.upgrade()?;
        if let Some(local) = remote.local_actor_path(path) {
            return self.engine.upgrade()?.resolve_actor(local);
        }
        remote.gateway().lookup(path)
    }
}

impl ThreadPropsSpawner {
    /// Authorize native artifacts with this exact digest for this spawner.
    /// Feature enablement alone never authorizes native execution. Clones made
    /// before this call retain their original policy; all share worker capacity.
    ///
    /// # Safety
    /// The caller guarantees the code, dependencies, metadata and hooks obey
    /// `parrot_abi::DylibLoader::load`'s full trust contract for every permitted
    /// load. A digest is an integrity identifier, not proof of those guarantees.
    /// Instances must tolerate independent drain/destruction while sibling
    /// instances or library handles remain live; shared globals are not isolated.
    ///
    /// ```compile_fail
    /// use parrot_app::host::ThreadPropsSpawner;
    /// fn authorize(spawner: ThreadPropsSpawner, digest: &str) {
    ///     let _ = spawner.with_trusted_dylib(digest);
    /// }
    /// ```
    #[cfg(feature = "dylib")]
    pub unsafe fn with_trusted_dylib(mut self, digest: &str) -> Result<Self, DeployError> {
        self.dylib = Arc::new(self.dylib.authorize(digest)?);
        Ok(self)
    }
    /// Failed original identities retained by this spawner and all its clones.
    pub fn pending_cleanup(&self) -> Vec<crate::cleanup::RetainedActor> {
        self.cleanup.pending()
    }
    /// Retry retained cleanup. Started retries remain on the engine runtime if
    /// this waiter is cancelled; unattempted entries remain in the journal.
    pub async fn retry_pending_cleanup(&self) -> Result<(), DeployError> {
        self.cleanup.retry().await
    }
    /// Set sandbox and artifact/cache budgets for subsequent Wasm spawns.
    /// Runtime construction is lazy and occurs on a blocking worker. A fresh
    /// preparation/cache owner is created; existing clones retain their limits.
    #[cfg(feature = "wasm")]
    pub fn with_wasm_limits(
        mut self,
        config: parrot_wasm::WasmConfig,
        limits: parrot_wasm::runtime::CompilationLimits,
    ) -> Self {
        self.wasm = Arc::new(wasm::WasmHost::new(config, limits));
        self
    }
    pub fn new(engine: Arc<ThreadActorSystem>) -> Self {
        Self {
            cleanup: cleanup::Registry::new(engine.runtime_handle().clone()),
            engine,
            remote: None,
            #[cfg(feature = "wasm")]
            wasm: Arc::new(wasm::WasmHost::default()),
            #[cfg(feature = "dylib")]
            dylib: Arc::new(dylib::DylibHost::default()),
        }
    }
    /// Enable host factories which own network-backed application components.
    /// Component contexts use this remote system's service registry, ACL and
    /// message routes. Both injected gateways weakly reference their remote owner.
    pub fn with_remote(engine: Arc<ThreadActorSystem>, remote: Arc<RemoteActorSystem>) -> Self {
        engine.set_receptionist_gateway(Arc::new(remote.receptionist_gateway()));
        engine.set_remote_gateway(Arc::new(HostRemoteGateway {
            remote: Arc::downgrade(&remote),
            engine: Arc::downgrade(&engine),
        }));
        Self {
            cleanup: cleanup::Registry::new(engine.runtime_handle().clone()),
            engine,
            remote: Some(remote),
            #[cfg(feature = "wasm")]
            wasm: Arc::new(wasm::WasmHost::default()),
            #[cfg(feature = "dylib")]
            dylib: Arc::new(dylib::DylibHost::default()),
        }
    }
}
#[async_trait::async_trait]
impl ParrotSpawner for ThreadPropsSpawner {
    async fn hook_target(&self, path: &str) -> Result<BoxedActorRef, DeployError> {
        self.engine
            .get_actor_ref(path)
            .map(|actor| Box::new(ArcActorRef(actor)) as BoxedActorRef)
            .ok_or_else(|| DeployError::Engine(format!("hook actor not found: {path}")))
    }
    fn configure(&self, resolved: &parrot_config::Resolved) -> Result<(), DeployError> {
        let actual = self.engine.config();
        let expected = parrot::thread::config::ThreadActorSystemConfig::from_resolved(resolved);
        macro_rules! check { ($($field:ident),+ $(,)?) => {$(
            if actual.$field != expected.$field {
                return Err(DeployError::Config(format!("thread.{} differs from the running engine; construct the engine from effective_config before assembly", stringify!($field))));
            }
        )+}; }
        if resolved.remote.node.node_id.is_some() && actual.name != expected.name {
            return Err(DeployError::Config(
                "thread system identity differs from effective config".into(),
            ));
        }
        check!(
            shared_pool_size,
            shared_burst_workers_max,
            shared_burst_backlog_threshold_ms,
            shared_burst_idle_timeout_ms,
            shared_queue_capacity,
            max_dedicated_threads,
            default_mailbox_capacity,
            max_context_tasks,
            default_ask_timeout,
            shutdown_timeout
        );
        Ok(())
    }
    async fn spawn_props(
        &self,
        factory: &str,
        paths: &[String],
        config: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        self.spawn_artifact(
            &ArtifactRef::Props {
                factory: factory.into(),
            },
            paths,
            config,
        )
        .await
    }
    fn validate_artifact(&self, artifact: &ArtifactRef) -> Result<(), DeployError> {
        match artifact {
            ArtifactRef::Props { .. } => Ok(()),
            #[cfg(feature = "wasm")]
            ArtifactRef::Wasm { digest, uri } => wasm::validate_artifact(digest, uri).map(|_| ()),
            #[cfg(feature = "dylib")]
            ArtifactRef::Dylib { digest, uri, abi } => {
                self.dylib.validate(digest, uri, *abi).map(|_| ())
            }
            other => Err(DeployError::Artifact(format!(
                "{} artifact runtime is not enabled by this spawner",
                other.kind()
            ))),
        }
    }
    async fn spawn_artifact(
        &self,
        artifact: &ArtifactRef,
        paths: &[String],
        config: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        self.validate_artifact(artifact)?;
        let cleanup = self.cleanup.reserve(paths)?;
        let runtime = self.engine.runtime_handle().clone();
        let owner = self.clone();
        let artifact = artifact.clone();
        let config = config.cloned();
        let (reply, result) = tokio::sync::oneshot::channel();
        let (claimed, claim) = tokio::sync::oneshot::channel();
        // Keep the running factory and partial pool owned if the caller goes away.
        // The extra acknowledgement also covers cancellation after send succeeds
        // but before the result is polled: a buffered result is not a transfer.
        runtime.spawn(async move {
            match owner
                .spawn_owned_artifact(&reply, &artifact, &cleanup, config.as_ref())
                .await
            {
                Ok(owned) => {
                    // Factory references are application code too. Preserve the
                    // originals if preparing the caller's views panics, so the
                    // owner can still retire every successfully created actor.
                    let references = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut references = OwnedReferences::with_capacity(owned.len());
                        for actor in owned.iter() { references.push(actor.clone_boxed()); }
                        references
                    }));
                    let references = match references {
                        Ok(references) => references,
                        Err(payload) => {
                            crate::disposal::drop_catching(payload);
                            let error = cleanup.rollback(owned, DeployError::Engine("artifact reference clone panicked".into())).await;
                            if let Err(error) = reply.send(Err(error)) {
                                tracing::warn!(?error, "cancelled artifact handoff failed");
                            }
                            return;
                        }
                    };
                    if reply.send(Ok(references)).is_ok() && claim.await.is_ok() {
                        return;
                    }
                    let error = cleanup.rollback(owned, DeployError::Engine("artifact result was not claimed".into())).await;
                    if !error.retained_actors().is_empty() {
                        tracing::error!(%error, "cancelled artifact spawn cleanup retained for retry");
                    }
                }
                Err(error) => {
                    if let Err(error) = reply.send(Err(error)) {
                        tracing::warn!(?error, "cancelled artifact spawn completed with an error");
                    }
                }
            }
        });
        let references = result.await.map_err(|_| {
            DeployError::Engine("artifact owner exited before delivering its result".into())
        })??;
        // No suspension between receiving the references and claiming ownership.
        let _ = claimed.send(());
        Ok(references.into_inner())
    }
    async fn stop_paths(&self, paths: &[String]) -> Result<(), DeployError> {
        let mut errors = Vec::new();
        for path in paths {
            if let Some(actor) = self.engine.get_actor_ref(path) {
                if let Err(error) = actor.stop().await {
                    errors.push(format!("stop {path}: {error}"));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(DeployError::Engine(errors.join("; ")))
        }
    }
}

impl ThreadPropsSpawner {
    async fn spawn_owned_artifact(
        &self,
        reply: &tokio::sync::oneshot::Sender<Result<OwnedReferences, DeployError>>,
        artifact: &ArtifactRef,
        cleanup: &cleanup::Scope,
        config: Option<&toml::Table>,
    ) -> Result<OwnedReferences, DeployError> {
        match artifact {
            ArtifactRef::Props { factory } => {
                self.spawn_owned_props(reply, factory, cleanup, config)
                    .await
            }
            #[cfg(feature = "wasm")]
            ArtifactRef::Wasm { digest, uri } => {
                self.wasm
                    .spawn(&self.engine, reply, digest, uri, config, cleanup)
                    .await
            }
            #[cfg(feature = "dylib")]
            ArtifactRef::Dylib { digest, uri, abi } => {
                self.dylib
                    .spawn(&self.engine, reply, (digest, uri, *abi), config, cleanup)
                    .await
            }
            other => Err(DeployError::Artifact(format!(
                "unsupported {} artifact",
                other.kind()
            ))),
        }
    }
    async fn spawn_owned_props(
        &self,
        reply: &tokio::sync::oneshot::Sender<Result<OwnedReferences, DeployError>>,
        factory: &str,
        cleanup: &cleanup::Scope,
        config: Option<&toml::Table>,
    ) -> Result<OwnedReferences, DeployError> {
        let paths = &cleanup.paths;
        if reply.is_closed() {
            return Err(DeployError::Engine("props caller cancelled".into()));
        }
        if factory == crate::durable::CLOUD_PROXY_FACTORY {
            if paths.len() != 1 {
                return Err(DeployError::Config("one cloud proxy owns exactly one durable stream; use separate components for separate streams".into()));
            }
            let remote = self.remote.as_ref().ok_or_else(|| {
                DeployError::Config("cloud proxy requires ThreadPropsSpawner::with_remote".into())
            })?;
            let config = crate::durable::CloudProxyConfig::from_table(config)
                .map_err(DeployError::Config)?;
            let actor =
                cloud_proxy::CloudProxy::spawn(&self.engine, remote, &paths[0], config).await?;
            let mut owned = OwnedReferences::with_capacity(1);
            owned.push(actor);
            return Ok(owned);
        }
        let factory = parrot_api::message::inventory::iter::<ThreadPropsFactory>()
            .find(|f| f.name == factory)
            .ok_or_else(|| {
                DeployError::Artifact(format!(
                    "contextual thread props factory '{factory}' not registered"
                ))
            })?;
        let mut owned = OwnedReferences::with_capacity(paths.len());
        for path in paths {
            if reply.is_closed() {
                return Err(cleanup
                    .rollback(owned, DeployError::Engine("props caller cancelled".into()))
                    .await);
            }
            let result = std::panic::AssertUnwindSafe(async {
                (factory.spawn)(self.engine.clone(), path.clone(), config.cloned()).await
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|payload| {
                crate::disposal::drop_catching(payload);
                Err(parrot_api::errors::ActorError::Panic(format!(
                    "props factory {} panicked at {path}",
                    factory.name
                )))
            });
            match result {
                Ok(actor) => owned.push(actor),
                Err(error) => {
                    return Err(cleanup
                        .rollback(owned, DeployError::Engine(format!("spawn {path}: {error}")))
                        .await);
                }
            }
        }
        Ok(owned)
    }
}

// ============================================================================
// 网关组件：真实子进程 + registry 模式注册
// ============================================================================

/// LocalLookup 适配：宿主 thread 引擎本地查找。
pub struct HostLookup {
    pub ts: Arc<ThreadActorSystem>,
}

#[async_trait::async_trait]
impl LocalLookup for HostLookup {
    async fn lookup(&self, path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        // registry 直查（thread 引擎本地段）
        self.ts
            .get_actor_ref(path)
            .map(|r| Box::new(ArcActorRef(r)) as Box<dyn parrot_api::address::ActorRef>)
    }
}

/// Exact application paths published by successfully deployed gateways.
#[derive(Default)]
pub struct GatewayRoutes(std::sync::RwLock<std::collections::HashMap<String, String>>);
impl GatewayRoutes {
    fn remove_node(&self, node: &str) {
        let prefix = format!("parrot://{node}/");
        self.0
            .write()
            .unwrap()
            .retain(|_, target| !target.starts_with(&prefix));
    }
}

/// Host-local actors and opaque gateway routes share the application's address.
pub struct ApplicationLookup {
    pub ts: Arc<ThreadActorSystem>,
    pub routes: Arc<GatewayRoutes>,
}
#[async_trait::async_trait]
impl LocalLookup for ApplicationLookup {
    async fn lookup(&self, path: &str) -> Option<BoxedActorRef> {
        HostLookup {
            ts: self.ts.clone(),
        }
        .lookup(path)
        .await
    }
    fn forwarding_target(&self, path: &str) -> Option<String> {
        self.routes.0.read().unwrap().get(path).cloned()
    }
}

/// Arc<dyn ActorRef> → Box<dyn ActorRef> 适配（clone_boxed 保共享）。
#[derive(Debug)]
struct ArcActorRef(Arc<dyn parrot_api::address::ActorRef>);

#[async_trait::async_trait]
impl parrot_api::address::ActorRef for ArcActorRef {
    fn send<'a>(
        &'a self,
        msg: parrot_api::types::BoxedMessage,
    ) -> parrot_api::types::BoxedFuture<
        'a,
        parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
    > {
        self.0.send(msg)
    }
    fn send_with_timeout<'a>(
        &'a self,
        msg: parrot_api::types::BoxedMessage,
        t: Option<std::time::Duration>,
    ) -> parrot_api::types::BoxedFuture<
        'a,
        parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
    > {
        self.0.send_with_timeout(msg, t)
    }
    fn deliver<'a>(
        &'a self,
        msg: parrot_api::types::BoxedMessage,
    ) -> parrot_api::types::BoxedFuture<'a, parrot_api::types::ActorResult<()>> {
        self.0.deliver(msg)
    }
    fn send_with_options<'a>(
        &'a self,
        msg: parrot_api::types::BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> parrot_api::types::BoxedFuture<
        'a,
        parrot_api::types::ActorResult<parrot_api::types::BoxedMessage>,
    > {
        self.0.send_with_options(msg, options)
    }
    fn deliver_with_options<'a>(
        &'a self,
        msg: parrot_api::types::BoxedMessage,
        options: parrot_api::message::MessageOptions,
    ) -> parrot_api::types::BoxedFuture<'a, parrot_api::types::ActorResult<()>> {
        self.0.deliver_with_options(msg, options)
    }
    fn stop<'a>(
        &'a self,
    ) -> parrot_api::types::BoxedFuture<'a, parrot_api::types::ActorResult<()>> {
        self.0.stop()
    }
    fn path(&self) -> String {
        self.0.path()
    }
    fn is_alive<'a>(&'a self) -> parrot_api::types::BoxedFuture<'a, bool> {
        self.0.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self(self.0.clone()))
    }
    fn death_watch(&self) -> Option<parrot_api::death_watch::DeathWatch> {
        self.0.death_watch()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Guardians own gateway groups from spawn through actual retirement.
/// Cleanup continues independently when a caller or its Tokio runtime is dropped.
#[derive(Default)]
pub struct GatewayProcesses {
    budget: Arc<GatewayProcessBudget>,
    procs: std::sync::Mutex<std::collections::HashMap<String, (String, OwnedGateway)>>,
    retirements: std::sync::Mutex<Vec<(String, gateway_process::Completion)>>,
    operation: tokio::sync::Mutex<()>,
    closed: std::sync::atomic::AtomicBool,
    routes: Arc<GatewayRoutes>,
}
impl GatewayProcesses {
    pub fn budget(&self) -> Arc<GatewayProcessBudget> {
        self.budget.clone()
    }
    pub async fn kill_all(&self) -> Result<(), DeployError> {
        let _operation = self.operation.lock().await;
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        let children = std::mem::take(&mut *self.procs.lock().unwrap());
        self.routes.0.write().unwrap().clear();
        // Drop all senders before awaiting any completion. Cancellation cannot
        // strand peers, and a later kill_all also waits for earlier retirements.
        drop(children);
        let completions = self.retirements.lock().unwrap().clone();
        let mut errors = Vec::new();
        for (name, mut completion) in completions {
            if let Err(error) = gateway_process::wait_for_retirement(&mut completion).await {
                errors.push(format!("{name}: {error}"));
            }
        }
        self.retirements.lock().unwrap().clear();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(DeployError::Engine(errors.join("; ")))
        }
    }
    fn take_component(&self, name: &str) -> Option<(String, OwnedGateway)> {
        let child = self.procs.lock().unwrap().remove(name);
        if let Some((node, _)) = &child {
            self.routes.remove_node(node);
        }
        child
    }
    async fn wait_component(&self, name: &str) -> Result<(), DeployError> {
        let completions: Vec<_> = self
            .retirements
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .collect();
        let mut errors = Vec::new();
        for mut completion in completions {
            if let Err(error) = gateway_process::wait_for_retirement(&mut completion).await {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(DeployError::Engine(errors.join("; ")))
        }
    }
    /// Published component handles, not a count of live OS processes.
    /// Await `kill_all` to drain pending, cancelled and published ownership.
    pub fn child_count(&self) -> usize {
        self.procs.lock().unwrap().len()
    }
}
async fn reap(child: &mut OwnedGateway) -> Result<(), DeployError> {
    child.retire().await
}

/// One owned process per component; stopping one component preserves its peers.
#[derive(Clone)]
pub struct ProcessGatewayFactory {
    pub repo_root: PathBuf,
    pub remote: Arc<RemoteActorSystem>,
    pub procs: Arc<GatewayProcesses>,
    startup_timeout: std::time::Duration,
    jvm_classpath: Option<std::ffi::OsString>,
    beam_sources: std::collections::HashMap<String, PathBuf>,
}
impl ProcessGatewayFactory {
    pub fn new(repo_root: PathBuf, remote: Arc<RemoteActorSystem>) -> Self {
        Self::new_with_routes(repo_root, remote, Arc::default())
    }
    /// Use the same table supplied to `ApplicationLookup` to publish gateway paths.
    pub fn new_with_routes(
        repo_root: PathBuf,
        remote: Arc<RemoteActorSystem>,
        routes: Arc<GatewayRoutes>,
    ) -> Self {
        Self::new_with_routes_and_budget(repo_root, remote, routes, Arc::default())
    }
    /// Share one admission domain across all factories owned by this host.
    /// Every compiler and gateway group consumes a slot until actual retirement.
    pub fn new_with_routes_and_budget(
        repo_root: PathBuf,
        remote: Arc<RemoteActorSystem>,
        routes: Arc<GatewayRoutes>,
        budget: Arc<GatewayProcessBudget>,
    ) -> Self {
        Self {
            repo_root,
            remote,
            procs: Arc::new(GatewayProcesses {
                routes,
                budget,
                ..Default::default()
            }),
            startup_timeout: std::time::Duration::from_secs(90),
            jvm_classpath: std::env::var_os("PARROT_JVM_CLASSPATH"),
            beam_sources: Default::default(),
        }
    }
    pub fn with_startup_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }
    pub fn with_jvm_classpath(mut self, classpath: impl Into<std::ffi::OsString>) -> Self {
        self.jvm_classpath = Some(classpath.into());
        self
    }
    /// Compile an explicitly selected component source with the target OTP.
    /// Outputs remain private and owned until actual process retirement.
    pub fn with_beam_sources(
        mut self,
        sources: std::collections::HashMap<String, PathBuf>,
    ) -> Self {
        self.beam_sources = sources;
        self
    }
    pub fn procs(&self) -> Arc<GatewayProcesses> {
        self.procs.clone()
    }
    pub fn component_node(&self, name: &str) -> Option<String> {
        self.procs
            .procs
            .lock()
            .unwrap()
            .get(name)
            .map(|(node, _)| node.clone())
    }

    fn spawn_owned(
        &self,
        name: &str,
        command: &tokio::process::Command,
        inline: bool,
        artifacts: Option<Arc<PrivateArtifacts>>,
    ) -> Result<OwnedGateway, DeployError> {
        let reservation = self.procs.budget.reserve()?;
        let child = OwnedGateway::spawn_admitted(
            command,
            &self.repo_root,
            inline,
            artifacts,
            Some(reservation),
        )?;
        let mut retirements = self.procs.retirements.lock().unwrap();
        retirements.retain(|(_, receipt)| !matches!(&*receipt.borrow(), Some(Ok(()))));
        retirements.push((name.into(), child.completion()));
        Ok(child)
    }

    async fn spawn_gateway_process(
        &self,
        comp: &ComponentSpec,
        node_id: &str,
    ) -> Result<(OwnedGateway, Option<String>), DeployError> {
        if self.remote.config.scheme != "tcp" {
            return Err(DeployError::Config(
                "local JVM/Ray/Erlang gateways currently require a TCP host".into(),
            ));
        }
        let mut address = self
            .remote
            .local_addr()
            .ok_or_else(|| DeployError::Engine("app not listening".into()))?;
        if address.ip().is_unspecified() {
            address.set_ip(if address.is_ipv6() {
                std::net::Ipv6Addr::LOCALHOST.into()
            } else {
                std::net::Ipv4Addr::LOCALHOST.into()
            });
        }
        let registration = format!("parrot={address}");
        let deadline = tokio::time::Instant::now()
            .checked_add(self.startup_timeout)
            .filter(|_| !self.startup_timeout.is_zero())
            .ok_or_else(|| {
                DeployError::Config(
                    "gateway startup deadline must be positive and representable".into(),
                )
            })?;
        let mut compilation = None;
        let mut compiled_component_uri = None;
        let mut command = match comp.engine {
            EngineKind::Akka => {
                let classpath = match &self.jvm_classpath {
                    Some(path) => path.clone(),
                    None => {
                        let target = self.repo_root.join("interop/jvm/target");
                        let dependencies =
                            std::fs::read_to_string(target.join("cp.txt")).map_err(|e| {
                                DeployError::Artifact(format!(
                                    "JVM classpath: {e}; build the gateway first"
                                ))
                            })?;
                        let paths = std::iter::once(target.join("parrot-protocol-jvm-0.1.0.jar"))
                            .chain(std::env::split_paths(dependencies.trim()));
                        std::env::join_paths(paths)
                            .map_err(|e| DeployError::Artifact(e.to_string()))?
                    }
                };
                let mut cmd = tokio::process::Command::new("java");
                cmd.arg("-cp")
                    .arg(classpath)
                    .arg("parrot.protocol.jvm.ParrotGatewayMain")
                    .arg("0")
                    .arg(&registration)
                    .arg(format!("node={node_id}"))
                    .arg("7200");
                cmd
            }
            EngineKind::Ray => {
                let mut cmd = tokio::process::Command::new("python3");
                cmd.current_dir(self.repo_root.join("interop/python"))
                    .env("PYTHONPATH", self.repo_root.join("interop/python"))
                    .env("RAY_ADDRESS", "local")
                    .env("RAY_USAGE_STATS_ENABLED", "0")
                    .arg(self.repo_root.join("tools/parrot-obs/ray_gateway.py"))
                    .args(["0", &registration]);
                cmd
            }
            EngineKind::Erlang => {
                let directory = self.repo_root.join("interop/erlang");
                let output = Arc::new(PrivateArtifacts::new(
                    tempfile::Builder::new()
                        .prefix("parrot-host-beam-")
                        .tempdir()
                        .map_err(|e| {
                            DeployError::Artifact(format!("Erlang output directory: {e}"))
                        })?,
                ));
                let mut compiler = tokio::process::Command::new("erlc");
                compiler
                    .current_dir(&directory)
                    .arg("-o")
                    .arg(output.path())
                    .args([
                        "parrot_gw.erl",
                        "parrot_trace.erl",
                        "parrot_component_runtime.erl",
                        "parrot_beam_snapshot.erl",
                        "parrot_beam_guest.erl",
                        "parrot_beam_host.erl",
                    ])
                    .kill_on_drop(true);
                let mut compiler =
                    self.spawn_owned(&comp.name, &compiler, false, Some(output.clone()))?;
                let compiled = tokio::time::timeout_at(deadline, compiler.wait_command()).await;
                let cleanup = compiler.retire().await;
                cleanup?;
                compiled.map_err(|_| DeployError::Engine("Erlang compile deadline".into()))??;
                if let Some(source) = self.beam_sources.get(&comp.name) {
                    let component_output = output.path().join("component");
                    std::fs::create_dir(&component_output).map_err(|e| {
                        DeployError::Artifact(format!("Erlang component output: {e}"))
                    })?;
                    let mut command = tokio::process::Command::new("erlc");
                    command
                        .current_dir(source.parent().ok_or_else(|| {
                            DeployError::Artifact("Erlang source has no directory".into())
                        })?)
                        .arg("-o")
                        .arg(&component_output)
                        .arg(source)
                        .kill_on_drop(true);
                    let mut compiler =
                        self.spawn_owned(&comp.name, &command, false, Some(output.clone()))?;
                    let compiled = tokio::time::timeout_at(deadline, compiler.wait_command()).await;
                    compiler.retire().await?;
                    compiled.map_err(|_| {
                        DeployError::Engine("Erlang component compile deadline".into())
                    })??;
                    compiled_component_uri = Some(
                        url::Url::from_directory_path(&component_output)
                            .map_err(|_| {
                                DeployError::Artifact("invalid Erlang output path".into())
                            })?
                            .to_string(),
                    );
                }
                let mut cmd = tokio::process::Command::new("erl");
                cmd.args(["-noshell", "-pa"])
                    .arg(output.path())
                    .arg("-eval")
                    .arg(format!("parrot_gw:main([0, \"{registration}\"] )"));
                compilation = Some(output);
                cmd
            }
            engine => {
                return Err(DeployError::Engine(format!(
                    "engine {engine:?} has no local gateway process form"
                )))
            }
        };
        command
            .env("PARROT_NODE_ID", node_id)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = self.spawn_owned(
            &comp.name,
            &command,
            comp.engine == EngineKind::Ray,
            compilation,
        )?;
        let started = async {
            loop {
                child.check_running()?;
                if self
                    .remote
                    .links_snapshot()
                    .await
                    .iter()
                    .any(|(id, _, status)| {
                        id == node_id && status.get() == parrot_remote::node::NodeState::Connected
                    })
                {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        let result = tokio::time::timeout_at(deadline, started)
            .await
            .unwrap_or_else(|_| {
                Err(DeployError::Engine(format!(
                    "gateway {node_id} registration deadline"
                )))
            });
        if let Err(error) = result {
            let cleanup = reap(&mut child).await;
            return Err(match cleanup {
                Ok(()) => error,
                Err(e) => DeployError::Engine(format!("{error}; cleanup: {e}")),
            });
        }
        Ok((child, compiled_component_uri))
    }
}
#[async_trait::async_trait]
impl GatewayFactory for ProcessGatewayFactory {
    async fn hook_target(
        &self,
        comp: &ComponentSpec,
        path: &str,
    ) -> Result<BoxedActorRef, DeployError> {
        let node = self.component_node(&comp.name).ok_or_else(|| {
            DeployError::Engine(format!("component {} has no owned gateway", comp.name))
        })?;
        self.remote
            .remote_ref(&format!("parrot://{node}{path}"))
            .map(|actor| Box::new(actor) as BoxedActorRef)
            .map_err(|error| DeployError::Engine(error.to_string()))
    }
    async fn configure(&self, resolved: &parrot_config::Resolved) -> Result<(), DeployError> {
        let expected = parrot_remote::RemoteConfig::from_resolved_async(
            resolved,
            std::time::Duration::from_secs(5),
        )
        .await
        .map_err(|e| DeployError::Config(e.to_string()))?;
        let actual = &self.remote.config;
        let node = &resolved.remote.node;
        let seeds_match = actual
            .seeds
            .iter()
            .collect::<std::collections::HashSet<_>>()
            == expected
                .seeds
                .iter()
                .collect::<std::collections::HashSet<_>>();
        if (node.node_id.is_some() && actual.node_id != expected.node_id)
            || (node.bind.is_some() && actual.bind != expected.bind)
            || actual.scheme != expected.scheme
            || actual.topology_role != expected.topology_role
            || actual.direct_addr != expected.direct_addr
            || actual.extra_caps != expected.extra_caps
            || actual.knobs.unwrap_or_default() != expected.knobs.unwrap_or_default()
            || actual.receptionist_acl.default_deny != expected.receptionist_acl.default_deny
            || actual.receptionist_acl.roles != expected.receptionist_acl.roles
            || !seeds_match
        {
            return Err(DeployError::Config("remote host differs from effective_config; construct the host before assembly using that configuration".into()));
        }
        Ok(())
    }

    async fn start_gateway(
        &self,
        comp: &ComponentSpec,
        paths: &[String],
        version: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        let _operation = self.procs.operation.lock().await;
        if self.procs.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(DeployError::Engine("gateway factory is shut down".into()));
        }
        if self.procs.procs.lock().unwrap().contains_key(&comp.name) {
            return Err(DeployError::Engine(format!(
                "component {} is already deployed",
                comp.name
            )));
        }
        self.procs.wait_component(&comp.name).await?;
        let mut payload = comp.deployment(version).map_err(DeployError::Config)?;
        if paths
            .iter()
            .any(|path| self.procs.routes.0.read().unwrap().contains_key(path))
        {
            return Err(DeployError::Config(
                "gateway application path already deployed".into(),
            ));
        }
        static NEXT_NODE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let node_id = format!(
            "{}-{}-{}",
            comp.engine.as_str(),
            std::process::id(),
            NEXT_NODE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let (mut child, compiled_uri) = self.spawn_gateway_process(comp, &node_id).await?;
        if let Some(selected) = compiled_uri {
            if let parrot_remote::admin_v2::AdminArtifactRef::Beam { uri, .. } =
                &mut payload.artifact
            {
                *uri = Some(selected);
            }
        }
        let result = async {
            let deployed = self
                .remote
                .deploy_component(&node_id, payload)
                .await
                .map_err(|e| DeployError::Engine(format!("deploy {}: {e}", comp.name)))?;
            if deployed != paths {
                return Err(DeployError::Engine(format!(
                    "deployed paths {deployed:?} differ from planned {paths:?}"
                )));
            }
            child.check_running()?;
            paths
                .iter()
                .map(|path| {
                    self.remote
                        .remote_ref(&format!("parrot://{node_id}{path}"))
                        .map(|actor| Box::new(actor) as BoxedActorRef)
                        .map_err(|e| DeployError::Engine(e.to_string()))
                })
                .collect()
        }
        .await;
        match result {
            Ok(refs) => {
                let mut routes = self.procs.routes.0.write().unwrap();
                for path in paths {
                    routes.insert(path.clone(), format!("parrot://{node_id}{path}"));
                }
                self.procs
                    .procs
                    .lock()
                    .unwrap()
                    .insert(comp.name.clone(), (node_id, child));
                Ok(refs)
            }
            Err(error) => {
                let cleanup = reap(&mut child).await;
                Err(match cleanup {
                    Ok(()) => error,
                    Err(e) => DeployError::Engine(format!("{error}; cleanup: {e}")),
                })
            }
        }
    }
    async fn stop_gateway(&self, comp: &ComponentSpec) -> Result<(), DeployError> {
        let _operation = self.procs.operation.lock().await;
        // Take ownership before the first RPC await: cancellation drops this
        // handle and requests retirement even if the remote stop never replies.
        let Some((node, mut child)) = self.procs.take_component(&comp.name) else {
            return self.procs.wait_component(&comp.name).await;
        };
        let stopped = self
            .remote
            .stop_component(&node, &format!("/user/{}", comp.name))
            .await
            .map(|_| ())
            .map_err(|e| DeployError::Engine(e.to_string()));
        let reaped = reap(&mut child).await;
        match (stopped, reaped) {
            (Err(stop), Err(reap)) => Err(DeployError::Engine(format!("{stop}; cleanup: {reap}"))),
            (Err(error), _) | (_, Err(error)) => Err(error),
            _ => Ok(()),
        }
    }
}
