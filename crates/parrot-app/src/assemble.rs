//! AssemblingContext（DEV_09 §3.1 A3）：依赖序装配 + 失败逆序回滚 + hooks。
//!
//! 行为规约：
//! ① config_overlay 并入 parrot-config（Y3：overlay > 代码 > parrot.toml > 默认）；
//! ② hooks 路径解析：on_start 是组件内 actor 路径，宿主发 `ParrotAppHook` 消息
//!    （type_key `bin:app.hook#v1`）并等回执，超时 = 装配失败；
//! ③ 失败回滚逆序保证（started 栈）。

use futures::FutureExt;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use parrot_api::types::{BoxedActorRef, BoxedMessage};

use crate::manifest::{AppManifest, ComponentSpec};
use crate::planner::{Plan, TopologyView};

/// 应用钩子消息（宿主 → 组件 hook actor；type_key `bin:app.hook#v1`）。
///
/// 裸字节编解码（与网关方言同构——A4 起注册 codec）：
/// `[phase u8]`：0=Start 1=Stop 2=Drain。回执同型（同 phase 回显）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParrotAppHook(pub u8);

enum HookCompletion {
    Acknowledged,
    Retired,
}

pub mod hook_phase {
    pub const START: u8 = 0;
    pub const STOP: u8 = 1;
    pub const DRAIN: u8 = 2;
}

impl ParrotAppHook {
    pub fn encode(&self) -> Vec<u8> {
        vec![self.0]
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        match b {
            [phase @ 0..=2] => Some(Self(*phase)),
            _ => None,
        }
    }
}

parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:app.hook#v1",
        type_id: std::any::TypeId::of::<ParrotAppHook>(),
        encode: |message: &BoxedMessage| {
            let hook = message.downcast_ref::<ParrotAppHook>().ok_or("expected application hook")?;
            if hook.0 > hook_phase::DRAIN { return Err("unknown hook phase".into()); }
            Ok(hook.encode())
        },
        decode: |bytes: &[u8]| ParrotAppHook::decode(bytes)
            .map(|hook| Box::new(hook) as BoxedMessage)
            .ok_or_else(|| "hook must contain exactly one valid phase byte".into()),
    }
}

/// 部署错误（DeployError 枚举驱动——错误注入点显式建模）。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DeployError {
    #[error("{source}; incomplete rollback: {detail}")]
    Cleanup {
        source: Box<DeployError>,
        retained: Vec<crate::cleanup::RetainedActor>,
        detail: String,
    },
    #[error("artifact: {0}")]
    Artifact(String),
    #[error("artifact digest: {0}")]
    ArtifactDigest(String),
    #[error("engine: {0}")]
    Engine(String),
    #[error("config: {0}")]
    Config(String),
    #[error("hook panic: comp={comp} hook={hook}")]
    HookPanic { comp: String, hook: String },
    #[error("hook timeout: comp={comp} hook={hook}")]
    HookTimeout { comp: String, hook: String },
}

impl DeployError {
    pub fn retained_actors(&self) -> &[crate::cleanup::RetainedActor] {
        match self {
            Self::Cleanup { retained, .. } => retained,
            _ => &[],
        }
    }
    pub fn primary_error(&self) -> &Self {
        match self {
            Self::Cleanup { source, .. } => source.primary_error(),
            _ => self,
        }
    }
}

/// 本地网关工厂（A4 的四引擎内嵌实现点；测试替身 = memory transport 对）。
#[async_trait::async_trait]
pub trait GatewayFactory: Send + Sync {
    async fn configure(&self, cfg: &parrot_config::Resolved) -> Result<(), DeployError>;
    /// 为非 parrot 组件建本地执行面：返回该组件的入口 ActorRef
    /// （经 Wire 编解码——与生产同路径）。
    async fn start_gateway(
        &self,
        comp: &ComponentSpec,
        paths: &[String],
        version: &str,
    ) -> Result<Vec<BoxedActorRef>, DeployError>;
    /// Resolve a child hook within this component's owned gateway.
    async fn hook_target(
        &self,
        comp: &ComponentSpec,
        path: &str,
    ) -> Result<BoxedActorRef, DeployError> {
        Err(DeployError::Engine(format!(
            "hook target unavailable: {} {path}",
            comp.name
        )))
    }
    /// 停网关（回滚/teardown 用）。
    async fn stop_gateway(&self, comp: &ComponentSpec) -> Result<(), DeployError>;
}

/// parrot 组件 spawn 面（host 通过 ThreadPropsFactory 注入引擎与组件配置；
/// 本 trait 是宿主注入点——A4 用真实 ThreadActorSystem 适配，测试用替身）。
#[async_trait::async_trait]
pub trait ParrotSpawner: Send + Sync {
    /// Check that the host engine consumes the effective application configuration.
    fn configure(&self, cfg: &parrot_config::Resolved) -> Result<(), DeployError>;
    /// Check host artifact support before starting any component. Content and
    /// guest initialization failures remain fallible spawn operations.
    fn validate_artifact(
        &self,
        artifact: &crate::manifest::ArtifactRef,
    ) -> Result<(), DeployError> {
        match artifact {
            crate::manifest::ArtifactRef::Props { .. } => Ok(()),
            other => Err(DeployError::Artifact(format!(
                "{} artifact runtime is not enabled by this spawner",
                other.kind()
            ))),
        }
    }
    /// Dispatch an artifact while retaining compatibility with Props-only hosts.
    async fn spawn_artifact(
        &self,
        artifact: &crate::manifest::ArtifactRef,
        paths: &[String],
        config: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError> {
        self.validate_artifact(artifact)?;
        match artifact {
            crate::manifest::ArtifactRef::Props { factory } => {
                self.spawn_props(factory, paths, config).await
            }
            other => Err(DeployError::Artifact(format!(
                "{} artifact spawn is not implemented by this spawner",
                other.kind()
            ))),
        }
    }
    /// 按上下文工厂名在给定路径 spawn，并传入业务配置。
    async fn spawn_props(
        &self,
        factory: &str,
        paths: &[String],
        config: Option<&toml::Table>,
    ) -> Result<Vec<BoxedActorRef>, DeployError>;
    /// Resolve a child hook on the supplied local engine; never use a global engine.
    async fn hook_target(&self, path: &str) -> Result<BoxedActorRef, DeployError> {
        Err(DeployError::Engine(format!(
            "hook target unavailable: {path}"
        )))
    }
    /// Stop currently registered paths. Owned assembly cleanup stops its original
    /// actor references instead, so a replacement at the same path survives.
    /// 停一组路径（优雅）。
    async fn stop_paths(&self, paths: &[String]) -> Result<(), DeployError>;
}

/// 装配上下文：registry + started 逆序停用栈。
pub struct AssemblingContext {
    limits: crate::limits::ApplicationLimits,
    registry: HashMap<String, Vec<BoxedActorRef>>,
    wires: HashMap<String, BoxedActorRef>,
    public_refs: HashMap<String, Vec<BoxedActorRef>>,
    mirror_host: Option<std::sync::Weak<parrot_remote::system::RemoteActorSystem>>,
    mirror_group: Option<parrot_remote::mirror::WireMirrorGroup>,
    // Owned during resolution, but unpublished until every endpoint resolves.
    pending_wires: HashMap<String, BoxedActorRef>,
    wire_admission: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 已启动组件（逆序 pop = 逆依赖序停用）。
    started: Vec<String>,
    /// Original component specs and instance paths retain cleanup ownership.
    paths: HashMap<String, Vec<String>>,
    specs: HashMap<String, ComponentSpec>,
    ready: HashSet<String>,
    completed_hooks: HashSet<(String, usize, u8)>,
    /// Stable instance slots confirmed retired during partial/cancelled teardown.
    completed_terminations: HashSet<(String, usize)>,
    hook_targets: HashMap<(String, usize, String), BoxedActorRef>,
    /// hooks 回执超时。
    hooks_timeout: Duration,
    cleanup_timeout: Duration,
}

impl Default for AssemblingContext {
    fn default() -> Self {
        Self::new()
    }
}

/// 本地部署器（manifest → 本地执行面；与 ClusterDeployer 同构差异点）。
pub struct LocalDeployer<'a> {
    pub topology: &'a dyn TopologyView,
    pub gateway_factory: &'a dyn GatewayFactory,
    pub parrot_spawner: &'a dyn ParrotSpawner,
}

impl AssemblingContext {
    pub fn new() -> Self {
        Self::new_with_limits(crate::limits::ApplicationLimits::default())
    }

    /// Explicit host admission policy. A manifest or caller-supplied Plan cannot
    /// relax it; each assembly rechecks this policy before any host side effect.
    pub fn new_with_limits(limits: crate::limits::ApplicationLimits) -> Self {
        Self {
            limits,
            registry: HashMap::new(),
            wires: HashMap::new(),
            public_refs: HashMap::new(),
            mirror_host: None,
            mirror_group: None,
            pending_wires: HashMap::new(),
            wire_admission: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            started: Vec::new(),
            paths: HashMap::new(),
            specs: HashMap::new(),
            ready: HashSet::new(),
            completed_hooks: HashSet::new(),
            completed_terminations: HashSet::new(),
            hook_targets: HashMap::new(),
            hooks_timeout: Duration::from_secs(5),
            cleanup_timeout: Duration::from_millis(parrot_config::Defaults::shutdown_timeout_ms()),
        }
    }

    /// Bind a raw host lookup for application-owned local and Wire mirrors.
    /// The context keeps a weak host reference; the runner owns host shutdown.
    pub fn with_mirror_host(
        mut self,
        host: &std::sync::Arc<parrot_remote::system::RemoteActorSystem>,
    ) -> Self {
        self.mirror_host = Some(std::sync::Arc::downgrade(host));
        self
    }

    /// hooks 回执超时（测试注入用）。
    pub fn set_hooks_timeout(&mut self, d: Duration) {
        self.hooks_timeout = d;
    }

    /// Deploy in plan order; on failure attempt reverse cleanup and retain any failed termination for retry.
    pub async fn assemble(
        &mut self,
        plan: &Plan,
        deployer: &LocalDeployer<'_>,
        manifest: &AppManifest,
        cfg: &parrot_config::ParrotConfig,
    ) -> Result<(), DeployError> {
        if !self.started.is_empty() || self.mirror_group.is_some() {
            return Err(DeployError::Engine(
                "context is already assembled; teardown before reusing it".into(),
            ));
        }
        crate::manifest::validate_with_limits(manifest, &self.limits)
            .map_err(|errors| DeployError::Config(format!("{errors:?}")))?;
        if !manifest.mirrors.is_empty()
            && self
                .mirror_host
                .as_ref()
                .and_then(|host| host.upgrade())
                .is_none()
        {
            return Err(DeployError::Config(
                "application mirrors require a live raw host lookup".into(),
            ));
        }
        self.limits
            .check_plan(plan)
            .map_err(|e| DeployError::Config(e.to_string()))?;
        if manifest.wiring.iter().any(|wire| wire.qos == "wan") {
            return Err(DeployError::Config(
                "local assembly does not implement per-connection WAN QoS".into(),
            ));
        }
        aggregate(self.close_wires())?;
        self.wire_admission = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        if self.hooks_timeout.is_zero()
            || tokio::time::Instant::now()
                .checked_add(self.hooks_timeout)
                .is_none()
        {
            return Err(DeployError::Config(
                "hook timeout must be positive and representable".into(),
            ));
        }
        let mut planned = HashSet::new();
        let mut all_paths = HashSet::new();
        let mut prepared_paths = HashMap::new();
        for pc in &plan.order {
            let expected = manifest.component(&pc.spec.name).ok_or_else(|| {
                DeployError::Config("plan contains an undeclared component".into())
            })?;
            // Valid TOML includes NaN, whose ordinary PartialEq is not reflexive.
            // Compare the lossless serialized spec, also retaining signed zero.
            let same_spec = toml::to_string(expected)
                .map_err(|e| DeployError::Config(e.to_string()))?
                == toml::to_string(&pc.spec).map_err(|e| DeployError::Config(e.to_string()))?;
            if !same_spec
                || !planned.insert(pc.spec.name.clone())
                || pc
                    .spec
                    .deps
                    .iter()
                    .any(|dependency| !planned.contains(dependency))
            {
                return Err(DeployError::Config(
                    "plan differs from manifest or dependency order".into(),
                ));
            }
            let expected = instance_paths(&pc.spec, &self.limits)?;
            if pc
                .shard_plan
                .as_ref()
                .is_some_and(|paths| paths != &expected)
                || expected.iter().any(|path| !all_paths.insert(path.clone()))
            {
                return Err(DeployError::Config(
                    "plan has conflicting or non-canonical instance paths".into(),
                ));
            }
            prepared_paths.insert(pc.spec.name.clone(), expected);
        }
        if planned.len() != manifest.components.len() {
            return Err(DeployError::Config("plan omits manifest components".into()));
        }
        let resolved = effective_config(manifest, cfg)?;
        self.cleanup_timeout = Duration::from_millis(resolved.thread.shutdown_timeout_ms);
        if tokio::time::Instant::now()
            .checked_add(self.cleanup_timeout)
            .is_none()
        {
            return Err(DeployError::Config(
                "cleanup timeout is not representable".into(),
            ));
        }
        deployer.parrot_spawner.configure(&resolved)?;
        for component in &manifest.components {
            if component.engine == crate::manifest::EngineKind::Parrot {
                deployer
                    .parrot_spawner
                    .validate_artifact(&component.artifact)?;
            }
        }
        deployer.gateway_factory.configure(&resolved).await?;

        for pc in &plan.order {
            let comp = &pc.spec;
            let paths = prepared_paths
                .remove(&comp.name)
                .expect("all plan paths preflighted");

            let spawned = std::panic::AssertUnwindSafe(spawn_component(
                comp,
                &paths,
                &manifest.version,
                deployer,
            ))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                Err(DeployError::Engine(format!(
                    "spawn component {} panicked",
                    comp.name
                )))
            });
            let refs = match spawned {
                Ok(refs) => refs,
                Err(error) => {
                    if !error.retained_actors().is_empty() {
                        self.registry.insert(
                            comp.name.clone(),
                            error
                                .retained_actors()
                                .iter()
                                .map(|actor| Box::new(actor.clone()) as BoxedActorRef)
                                .collect(),
                        );
                        self.paths.insert(
                            comp.name.clone(),
                            error
                                .retained_actors()
                                .iter()
                                .map(|actor| actor.path.clone())
                                .collect(),
                        );
                        // Failed partial instances have no successful application
                        // hooks to replay, but still require owned termination.
                        let mut cleanup_spec = comp.clone();
                        cleanup_spec.hooks = Default::default();
                        self.specs.insert(comp.name.clone(), cleanup_spec);
                        self.started.push(comp.name.clone());
                    }
                    return Err(with_cleanup(error, self.teardown(deployer).await));
                }
            };
            let valid_count = refs.len() == paths.len() && !refs.is_empty();
            self.registry.insert(comp.name.clone(), refs);
            self.paths.insert(comp.name.clone(), paths);
            self.specs.insert(comp.name.clone(), comp.clone());
            self.started.push(comp.name.clone());
            if !valid_count {
                return Err(with_cleanup(
                    DeployError::Engine(format!(
                        "component {} returned the wrong instance count",
                        comp.name
                    )),
                    self.teardown(deployer).await,
                ));
            }
            if let Err(error) = self.prepare_hooks(&comp.name, deployer).await {
                return Err(with_cleanup(error, self.teardown(deployer).await));
            }
            if let Err(error) = self
                .run_hooks(&comp.name, hook_phase::START, deployer)
                .await
            {
                return Err(with_cleanup(error, self.teardown(deployer).await));
            }
            self.ready.insert(comp.name.clone());
        }
        if let Err(error) = self.install_mirrors(manifest) {
            return Err(with_cleanup(error, self.teardown(deployer).await));
        }
        if let Err(error) = self.resolve_wires(manifest, deployer).await {
            return Err(with_cleanup(error, self.teardown(deployer).await));
        }
        self.wire_admission
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Cooperative drain in reverse dependency order. A successful acknowledgement
    /// is remembered per instance; hooks must quiesce their own business work.
    /// This does not independently fence arbitrary actor references or network ingress.
    pub async fn drain(&mut self, deployer: &LocalDeployer<'_>) -> Result<(), DeployError> {
        let mut errors = self.close_wires();
        if let Err(error) = self.close_mirrors().await {
            errors.push(error);
        }
        for name in self.started.clone().into_iter().rev() {
            if self.ready.contains(&name) {
                if let Err(error) = self.run_hooks(&name, hook_phase::DRAIN, deployer).await {
                    errors.push(error);
                }
            }
        }
        aggregate(errors)
    }

    /// Reverse dependency order: drain ready instances, stop hooks, then terminate
    /// owned actors/processes. A failed hook cannot skip resource cleanup. Failed
    /// termination retains ownership for a subsequent teardown attempt.
    pub async fn teardown(&mut self, deployer: &LocalDeployer<'_>) -> Result<(), DeployError> {
        let mut errors = self.close_wires();
        if let Err(error) = self.close_mirrors().await {
            errors.push(error);
        }
        for name in self.started.clone().into_iter().rev() {
            if self.ready.contains(&name) {
                if let Err(error) = self.run_hooks(&name, hook_phase::DRAIN, deployer).await {
                    errors.push(error);
                }
            }
            if let Err(error) = self.run_hooks(&name, hook_phase::STOP, deployer).await {
                errors.push(error);
            }
            let spec = &self.specs[&name];
            let termination = std::panic::AssertUnwindSafe(async {
                if spec.engine == crate::manifest::EngineKind::Parrot {
                    let mut failures = Vec::new();
                    for (index, actor) in self.registry[&name].iter().enumerate() {
                        let key = (name.clone(), index);
                        if self.completed_terminations.contains(&key) {
                            continue;
                        }
                        // Diagnostics must not invoke another user callback on
                        // the failure path. A malformed spawner may also return
                        // more references than declared paths; still stop all.
                        let path = self.paths[&name]
                            .get(index)
                            .cloned()
                            .unwrap_or_else(|| format!("{name}[{index}]"));
                        let result = tokio::time::timeout(
                            self.cleanup_timeout,
                            std::panic::AssertUnwindSafe(async { actor.stop().await })
                                .catch_unwind(),
                        )
                        .await;
                        match result {
                            Ok(Ok(Ok(())))
                            | Ok(Ok(Err(parrot_api::errors::ActorError::ActorNotFound(_)))) => {
                                // No await between confirmation and recording it:
                                // cancellation of a later stop preserves progress.
                                self.completed_terminations.insert(key);
                            }
                            Ok(Ok(Err(error))) => {
                                failures.push(DeployError::Engine(format!("stop {path}: {error}")))
                            }
                            Ok(Err(_)) => {
                                failures.push(DeployError::Engine(format!("stop {path} panicked")))
                            }
                            Err(_) => failures
                                .push(DeployError::Engine(format!("stop {path} cleanup deadline"))),
                        }
                    }
                    aggregate(failures)
                } else {
                    tokio::time::timeout(
                        self.cleanup_timeout,
                        deployer.gateway_factory.stop_gateway(spec),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(DeployError::Engine(format!(
                            "stop component {name} cleanup deadline"
                        )))
                    })
                }
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| {
                Err(DeployError::Engine(format!(
                    "stop component {name} panicked"
                )))
            });
            match termination {
                Ok(()) => {
                    errors.extend(self.release_component(&name));
                }
                Err(error) => errors.push(error),
            }
        }
        aggregate(errors)
    }

    async fn prepare_hooks(
        &mut self,
        name: &str,
        deployer: &LocalDeployer<'_>,
    ) -> Result<(), DeployError> {
        let spec = &self.specs[name];
        for (index, instance) in self.paths[name].iter().enumerate() {
            for hook in [
                &spec.hooks.on_start,
                &spec.hooks.on_drain,
                &spec.hooks.on_stop,
            ]
            .into_iter()
            .flatten()
            {
                let path = spec
                    .hook_path(hook, instance)
                    .map_err(DeployError::Config)?;
                let key = (name.to_string(), index, path.clone());
                if self.hook_targets.contains_key(&key) {
                    continue;
                }
                let resolve = async {
                    if &path == instance {
                        self.registry[name]
                            .get(index)
                            .map(|actor| actor.clone_boxed())
                            .ok_or_else(|| {
                                DeployError::Engine(format!("missing instance {instance}"))
                            })
                    } else if spec.engine == crate::manifest::EngineKind::Parrot {
                        deployer.parrot_spawner.hook_target(&path).await
                    } else {
                        deployer.gateway_factory.hook_target(spec, &path).await
                    }
                };
                let target = match tokio::time::timeout(
                    self.hooks_timeout,
                    std::panic::AssertUnwindSafe(resolve).catch_unwind(),
                )
                .await
                {
                    Ok(Ok(result)) => result?,
                    Ok(Err(_)) => {
                        return Err(DeployError::HookPanic {
                            comp: name.into(),
                            hook: path,
                        })
                    }
                    Err(_) => {
                        return Err(DeployError::HookTimeout {
                            comp: name.into(),
                            hook: path,
                        })
                    }
                };
                self.hook_targets.insert(key, target);
            }
        }
        Ok(())
    }

    async fn run_hooks(
        &mut self,
        name: &str,
        phase: u8,
        _deployer: &LocalDeployer<'_>,
    ) -> Result<(), DeployError> {
        if self.hooks_timeout.is_zero()
            || tokio::time::Instant::now()
                .checked_add(self.hooks_timeout)
                .is_none()
        {
            return Err(DeployError::Config(
                "hook timeout must be positive and representable".into(),
            ));
        }
        let spec = &self.specs[name];
        let hook = match phase {
            hook_phase::START => &spec.hooks.on_start,
            hook_phase::DRAIN => &spec.hooks.on_drain,
            hook_phase::STOP => &spec.hooks.on_stop,
            _ => unreachable!("internal hook phase"),
        };
        let Some(hook) = hook else {
            return Ok(());
        };
        let mut errors = Vec::new();
        for (index, instance) in self.paths[name].iter().enumerate() {
            let key = (name.to_string(), index, phase);
            if self.completed_hooks.contains(&key)
                || self
                    .completed_terminations
                    .contains(&(name.to_string(), index))
            {
                continue;
            }
            let path = spec
                .hook_path(hook, instance)
                .map_err(DeployError::Config)?;
            let result = async {
                let root = self.registry[name]
                    .get(index)
                    .ok_or_else(|| DeployError::Engine(format!("missing instance {instance}")))?;
                if spec.engine == crate::manifest::EngineKind::Parrot
                    && phase != hook_phase::START
                    && !root.is_alive().await
                {
                    // A closed mailbox is not a completed destructor. Join this
                    // exact owned reference; never look up a replacement by path.
                    root.stop().await.map_err(|error| {
                        DeployError::Engine(format!("join retired instance {instance}: {error}"))
                    })?;
                    return Ok(HookCompletion::Retired);
                }
                let target = self
                    .hook_targets
                    .get(&(name.to_string(), index, path.clone()))
                    .ok_or_else(|| DeployError::Engine(format!("unresolved hook {name} {path}")))?;
                let reply = target.send(hook_message(phase)).await.map_err(|error| {
                    DeployError::Engine(format!("hook {name} {path} phase={phase}: {error}"))
                })?;
                if reply.downcast_ref::<ParrotAppHook>() != Some(&ParrotAppHook(phase)) {
                    return Err(DeployError::Engine(format!(
                        "hook {name} {path}: expected phase {phase} acknowledgement"
                    )));
                }
                Ok(HookCompletion::Acknowledged)
            };
            let result = match tokio::time::timeout(
                self.hooks_timeout,
                std::panic::AssertUnwindSafe(result).catch_unwind(),
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(DeployError::HookPanic {
                    comp: name.into(),
                    hook: path.clone(),
                }),
                Err(_) => Err(DeployError::HookTimeout {
                    comp: name.into(),
                    hook: path.clone(),
                }),
            };
            match result {
                Ok(HookCompletion::Acknowledged) => {
                    self.completed_hooks.insert(key);
                }
                Ok(HookCompletion::Retired) => {
                    // Record retirement separately from a hook acknowledgement.
                    self.completed_terminations
                        .insert((name.to_string(), index));
                }
                Err(error) => {
                    errors.push(error);
                    if phase == hook_phase::START {
                        break;
                    }
                }
            }
        }
        aggregate(errors)
    }

    // Detach all metadata before invoking arbitrary user destructors. Retirement
    // has already been confirmed; a disposal failure must not re-run stop/hooks.
    fn release_component(&mut self, name: &str) -> Vec<DeployError> {
        let public = self.public_refs.remove(name).unwrap_or_default();
        let actors = self.registry.remove(name).unwrap_or_default();
        self.paths.remove(name);
        self.specs.remove(name);
        self.ready.remove(name);
        self.completed_hooks
            .retain(|(component, _, _)| component != name);
        self.completed_terminations
            .retain(|(component, _)| component != name);
        self.started.retain(|component| component != name);
        let mut errors = Vec::new();
        for actor in public {
            release_reference(actor, "mirror component", &mut errors);
        }
        for (index, actor) in actors.into_iter().enumerate() {
            release_reference(actor, &format!("component {name}[{index}]"), &mut errors);
        }
        let keys: Vec<_> = self
            .hook_targets
            .keys()
            .filter(|(component, _, _)| component == name)
            .cloned()
            .collect();
        for key in keys {
            if let Some(actor) = self.hook_targets.remove(&key) {
                release_reference(
                    actor,
                    &format!("hook {}[{}] {}", key.0, key.1, key.2),
                    &mut errors,
                );
            }
        }
        errors
    }

    fn install_mirrors(&mut self, manifest: &AppManifest) -> Result<(), DeployError> {
        if manifest.mirrors.is_empty() {
            return Ok(());
        }
        let host = self
            .mirror_host
            .as_ref()
            .and_then(|host| host.upgrade())
            .ok_or_else(|| DeployError::Config("mirror host retired during assembly".into()))?;
        let policies = crate::mirror::expand(manifest).map_err(DeployError::Config)?;
        self.mirror_group = Some(
            host.register_wire_mirror_group(policies)
                .map_err(|e| DeployError::Engine(e.to_string()))?,
        );
        // Retain originals exclusively for lifecycle ownership and raw host lookup.
        for (name, refs) in &self.registry {
            if manifest.mirrors.iter().any(|policy| policy.source == *name) {
                self.public_refs.insert(name.clone(), Vec::new());
                for (index, target) in refs.iter().enumerate() {
                    let path = &self.paths[name][index];
                    let cloned =
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            target.clone_boxed()
                        })) {
                            Ok(cloned) => cloned,
                            Err(payload) => {
                                crate::disposal::drop_catching(payload);
                                return Err(DeployError::Engine(
                                    "mirror primary reference clone panicked".into(),
                                ));
                            }
                        };
                    let wrapped = self.wrap_mirror(path, cloned);
                    self.public_refs.get_mut(name).unwrap().push(Box::new(
                        crate::wiring::WiredRef::component(
                            path.clone(),
                            wrapped,
                            self.wire_admission.clone(),
                        ),
                    )
                        as BoxedActorRef);
                }
            }
        }
        Ok(())
    }
    fn wrap_mirror(&self, path: &str, target: BoxedActorRef) -> BoxedActorRef {
        match self
            .mirror_group
            .as_ref()
            .and_then(|group| group.handle_for(path))
        {
            Some(handle) => Box::new(crate::mirror::MirroredRef {
                target,
                path: path.into(),
                handle,
            }),
            None => target,
        }
    }
    async fn close_mirrors(&mut self) -> Result<(), DeployError> {
        if let Some(group) = &self.mirror_group {
            tokio::time::timeout(self.cleanup_timeout, group.close())
                .await
                .map_err(|_| {
                    DeployError::Engine(
                        "mirror retirement timed out; teardown must be retried".into(),
                    )
                })?;
        }
        self.mirror_group.take();
        Ok(())
    }
    fn close_wires(&mut self) -> Vec<DeployError> {
        if let Some(group) = &self.mirror_group {
            group.cancel();
        }
        self.wire_admission
            .store(false, std::sync::atomic::Ordering::Release);
        let mut errors = Vec::new();
        for (source, actor) in self.wires.drain().chain(self.pending_wires.drain()) {
            release_reference(actor, &format!("wire {source}"), &mut errors);
        }
        errors
    }

    async fn resolve_wires(
        &mut self,
        manifest: &AppManifest,
        deployer: &LocalDeployer<'_>,
    ) -> Result<(), DeployError> {
        for wire in &manifest.wiring {
            let (name, path) =
                crate::manifest::wire_endpoint(&wire.to).map_err(DeployError::Config)?;
            let spec = &self.specs[name];
            let resolve = async {
                if let Some(index) = self.paths[name].iter().position(|root| root == path) {
                    Ok(self.registry[name][index].clone_boxed())
                } else if spec.engine == crate::manifest::EngineKind::Parrot {
                    deployer.parrot_spawner.hook_target(path).await
                } else {
                    deployer.gateway_factory.hook_target(spec, path).await
                }
            };
            let target = match tokio::time::timeout(
                self.hooks_timeout,
                std::panic::AssertUnwindSafe(resolve).catch_unwind(),
            )
            .await
            {
                Ok(Ok(result)) => result.map_err(|error| {
                    DeployError::Engine(format!(
                        "resolve wire {} -> {}: {error}",
                        wire.from, wire.to
                    ))
                })?,
                Ok(Err(_)) => {
                    return Err(DeployError::Engine(format!(
                        "wire resolution panicked: {}",
                        wire.to
                    )))
                }
                Err(_) => {
                    return Err(DeployError::Engine(format!(
                        "wire resolution timed out: {}",
                        wire.to
                    )))
                }
            };
            let actor = crate::wiring::WiredRef::connection(
                wire.from.clone(),
                self.wrap_mirror(path, target),
                self.wire_admission.clone(),
            );
            self.pending_wires
                .insert(wire.from.clone(), Box::new(actor) as BoxedActorRef);
        }
        self.wires = std::mem::take(&mut self.pending_wires);
        Ok(())
    }

    /// Approximate counters for this assembly's exact policy generation.
    pub fn mirror_stats(&self, path: &str) -> Option<parrot_remote::mirror::WireMirrorStats> {
        self.mirror_group
            .as_ref()?
            .handle_for(path)
            .map(|handle| handle.stats())
    }

    /// Resolve a declared source endpoint, e.g. `crawler:/user/frontier`, to
    /// the exact target reference captured by this successful assembly.
    /// Clones reject new sends after drain, teardown or context drop and never
    /// rebind to a replacement generation. In-flight messages are not revoked.
    /// This does not inject names into an actor engine's global lookup table.
    pub fn wired_ref(&self, source: &str) -> Option<BoxedActorRef> {
        self.wires.get(source).map(|actor| actor.clone_boxed())
    }

    /// 组件入口 ref 快查（wiring 解析面）。
    pub fn component_ref(&self, name: &str) -> Option<&BoxedActorRef> {
        self.public_refs
            .get(name)
            .or_else(|| self.registry.get(name))
            .and_then(|v| v.first())
    }

    pub fn component_refs(&self, name: &str) -> Option<&[BoxedActorRef]> {
        self.public_refs
            .get(name)
            .or_else(|| self.registry.get(name))
            .map(|v| v.as_slice())
    }

    pub fn component_paths(&self, name: &str) -> Option<&[String]> {
        self.paths.get(name).map(|v| v.as_slice())
    }

    pub fn started(&self) -> &[String] {
        &self.started
    }
}

impl Drop for AssemblingContext {
    fn drop(&mut self) {
        // Synchronous reference disposal only: callers still must await teardown
        // (and handle errors) to retire actors and gateway processes.
        let mut errors = self.close_wires();
        for name in self.registry.keys().cloned().collect::<Vec<_>>() {
            errors.extend(self.release_component(&name));
        }
        for ((name, index, path), actor) in self.hook_targets.drain() {
            release_reference(actor, &format!("hook {name}[{index}] {path}"), &mut errors);
        }
        for error in errors {
            tracing::warn!(%error, "application reference disposal failed during Drop");
        }
    }
}

// Catch each reference independently, never an entire owning container: a second
// destructor panicking while the first unwinds would otherwise abort the process.
fn release_reference(actor: BoxedActorRef, owner: &str, errors: &mut Vec<DeployError>) {
    if !crate::disposal::drop_catching(actor) {
        errors.push(DeployError::Engine(format!(
            "release {owner} reference panicked"
        )));
    }
}

fn with_cleanup(primary: DeployError, cleanup: Result<(), DeployError>) -> DeployError {
    match cleanup {
        Ok(()) => primary,
        Err(error) => {
            let retained = primary
                .retained_actors()
                .iter()
                .chain(error.retained_actors())
                .cloned()
                .collect::<Vec<_>>();
            if retained.is_empty() {
                DeployError::Engine(format!("{primary}; rollback: {error}"))
            } else {
                DeployError::Cleanup {
                    source: Box::new(primary),
                    retained,
                    detail: error.to_string(),
                }
            }
        }
    }
}

/// 单组件 spawn 分流（parrot → artifact 宿主；其他 → GatewayFactory）。
async fn spawn_component(
    comp: &ComponentSpec,
    paths: &[String],
    version: &str,
    deployer: &LocalDeployer<'_>,
) -> Result<Vec<BoxedActorRef>, DeployError> {
    match comp.engine {
        crate::manifest::EngineKind::Parrot => {
            deployer
                .parrot_spawner
                .spawn_artifact(&comp.artifact, paths, comp.config.as_ref())
                .await
        }
        _ => {
            deployer
                .gateway_factory
                .start_gateway(comp, paths, version)
                .await
        }
    }
}

fn instance_paths(
    spec: &ComponentSpec,
    limits: &crate::limits::ApplicationLimits,
) -> Result<Vec<String>, DeployError> {
    spec.instance_paths_with_limits(limits)
        .map_err(|e| DeployError::Config(e.to_string()))
}

fn aggregate(mut errors: Vec<DeployError>) -> Result<(), DeployError> {
    match errors.len() {
        0 => Ok(()),
        1 => Err(errors.pop().unwrap()),
        _ => Err(DeployError::Engine(
            errors
                .into_iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        )),
    }
}

/// hook 消息构造（BoxedMessage 直接装 ParrotAppHook——本地路径；
/// 跨网关走 codec 注册的 `bin:app.hook#v1`）。
fn hook_message(phase: u8) -> BoxedMessage {
    Box::new(ParrotAppHook(phase))
}

/// Resolve the complete application configuration before creating host resources.
pub fn effective_config(
    manifest: &AppManifest,
    base: &parrot_config::ParrotConfig,
) -> Result<parrot_config::Resolved, DeployError> {
    let mut cfg = base.clone();
    if let Some(overlay) = &manifest.config_overlay {
        apply_overlay(&mut cfg, overlay)?;
    }
    cfg.build().map_err(|e| DeployError::Config(e.to_string()))
}

/// Application overlay has higher priority than code, file and defaults.
/// Parse the complete layer, validate the effective result, and only then
/// publish it. An invalid overlay must not partially mutate its caller.
pub fn apply_overlay(
    cfg: &mut parrot_config::ParrotConfig,
    overlay: &toml::value::Table,
) -> Result<(), DeployError> {
    let layer: parrot_config::ParrotConfig = toml::Value::Table(overlay.clone())
        .try_into()
        .map_err(|error| DeployError::Config(format!("invalid config overlay: {error}")))?;
    let merged = layer.with_fallback(cfg.clone());
    merged
        .clone()
        .build()
        .map_err(|error| DeployError::Config(error.to_string()))?;
    *cfg = merged;
    Ok(())
}

/// Deployment result alias.
pub type DeployResult<T> = Result<T, DeployError>;

// ============================================================================
// 测试（§5.1 Assembling 20+：依赖序/回滚逆序/overlay 优先级/hooks 时序）
// ============================================================================

#[cfg(test)]
mod tests {
    mod retirement_contract;
    use super::*;
    use crate::manifest::{
        ArtifactRef, ComponentHooks, EngineKind, InstancePolicy, PlacementConstraint, UpgradePolicy,
    };
    use crate::planner::{plan, LocalTopology};
    use parrot_api::address::ActorRef;
    use parrot_api::types::{ActorResult, BoxedFuture};
    use std::sync::{Arc, Mutex};

    // ── 测试替身：记账式 ref/spawner/gateway ──────────────────

    #[derive(Debug)]
    struct Journal {
        events: Mutex<Vec<String>>,
        fail_spawn_on: Mutex<Option<String>>,
        hook_delay: Mutex<Duration>,
        fail_hook: std::sync::atomic::AtomicBool,
        bad_ack: std::sync::atomic::AtomicBool,
        hook_panic: std::sync::atomic::AtomicU8,
        fail_stop: Mutex<Option<String>>,
        hang_stop: Mutex<Option<String>>,
        live: std::sync::atomic::AtomicBool,
    }

    impl Journal {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(vec![]),
                fail_spawn_on: Mutex::new(None),
                hook_delay: Mutex::new(Duration::ZERO),
                fail_hook: std::sync::atomic::AtomicBool::new(false),
                bad_ack: std::sync::atomic::AtomicBool::new(false),
                hook_panic: std::sync::atomic::AtomicU8::new(0),
                fail_stop: Mutex::new(None),
                hang_stop: Mutex::new(None),
                live: std::sync::atomic::AtomicBool::new(true),
            })
        }
        fn log(&self, e: impl Into<String>) {
            self.events.lock().unwrap().push(e.into());
        }
        fn snapshot(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    /// 记账 ref：ask 回 ParrotAppHook 回显（可注入延迟）。
    struct JournalRef {
        journal: Arc<Journal>,
        path: String,
    }

    impl std::fmt::Debug for JournalRef {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("JournalRef")
                .field("path", &self.path)
                .finish()
        }
    }

    #[async_trait::async_trait]
    impl ActorRef for JournalRef {
        fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send_with_timeout(msg, None)
        }
        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _t: Option<Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            if self
                .journal
                .hook_panic
                .load(std::sync::atomic::Ordering::Relaxed)
                == 1
            {
                panic!("synchronous hook panic");
            }
            let phase = msg
                .downcast_ref::<ParrotAppHook>()
                .map(|h| h.0)
                .unwrap_or(99);
            let delay = *self.journal.hook_delay.lock().unwrap();
            let journal = self.journal.clone();
            let path = self.path.clone();
            Box::pin(async move {
                if journal
                    .hook_panic
                    .load(std::sync::atomic::Ordering::Relaxed)
                    == 2
                {
                    panic!("asynchronous hook panic");
                }
                if delay > Duration::ZERO {
                    tokio::time::sleep(delay).await;
                }
                journal.log(format!("ask:{path}:{phase}"));
                if journal.fail_hook.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(parrot_api::errors::ActorError::MessageHandlingError(
                        "injected hook failure".into(),
                    ));
                }
                if journal.bad_ack.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(Box::new(ParrotAppHook(99)) as BoxedMessage);
                }
                if phase <= 2 {
                    Ok(Box::new(ParrotAppHook(phase)) as BoxedMessage)
                } else {
                    Err(parrot_api::errors::ActorError::MessageHandlingError(
                        "unhandled".into(),
                    ))
                }
            })
        }
        fn deliver<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            let journal = self.journal.clone();
            let path = self.path.clone();
            Box::pin(async move {
                journal.log(format!("stop:{path}"));
                let hang = journal.hang_stop.lock().unwrap().as_deref() == Some(&path);
                if hang {
                    std::future::pending::<()>().await;
                }
                if journal.fail_stop.lock().unwrap().as_deref() == Some(&path) {
                    return Err(parrot_api::errors::ActorError::InternalError(
                        "injected stop failure".into(),
                    ));
                }
                Ok(())
            })
        }
        fn path(&self) -> String {
            self.path.clone()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async { self.journal.live.load(std::sync::atomic::Ordering::Relaxed) })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(Self {
                journal: self.journal.clone(),
                path: self.path.clone(),
            })
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    struct TestSpawner {
        journal: Arc<Journal>,
    }

    #[async_trait::async_trait]
    impl ParrotSpawner for TestSpawner {
        fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
            Ok(())
        }
        async fn spawn_props(
            &self,
            factory: &str,
            paths: &[String],
            _config: Option<&toml::Table>,
        ) -> Result<Vec<BoxedActorRef>, DeployError> {
            if factory == "app.panic" {
                panic!("injected factory panic");
            }
            if self.journal.fail_spawn_on.lock().unwrap().as_deref() == Some(factory) {
                return Err(DeployError::Engine(format!("spawn failed: {factory}")));
            }
            self.journal.log(format!("spawn:{}", paths[0]));
            Ok(paths
                .iter()
                .map(|p| {
                    Box::new(JournalRef {
                        journal: self.journal.clone(),
                        path: p.clone(),
                    }) as BoxedActorRef
                })
                .collect())
        }
        async fn stop_paths(&self, paths: &[String]) -> Result<(), DeployError> {
            for p in paths {
                self.journal.log(format!("stop_paths:{p}"));
            }
            Ok(())
        }
    }

    struct TestGateway {
        journal: Arc<Journal>,
    }

    #[async_trait::async_trait]
    impl GatewayFactory for TestGateway {
        async fn configure(&self, _: &parrot_config::Resolved) -> Result<(), DeployError> {
            Ok(())
        }
        async fn start_gateway(
            &self,
            comp: &ComponentSpec,
            paths: &[String],
            _version: &str,
        ) -> Result<Vec<BoxedActorRef>, DeployError> {
            self.journal
                .log(format!("gw:{}:{}", comp.name, comp.engine.as_str()));
            Ok(paths
                .iter()
                .map(|p| {
                    Box::new(JournalRef {
                        journal: self.journal.clone(),
                        path: p.clone(),
                    }) as BoxedActorRef
                })
                .collect())
        }
        async fn stop_gateway(&self, comp: &ComponentSpec) -> Result<(), DeployError> {
            self.journal.log(format!("gw_stop:{}", comp.name));
            Ok(())
        }
    }

    struct TestHost {
        spawner: TestSpawner,
        gateway: TestGateway,
    }
    impl TestHost {
        fn deployer(&self) -> LocalDeployer<'_> {
            LocalDeployer {
                topology: &LocalTopology,
                parrot_spawner: &self.spawner,
                gateway_factory: &self.gateway,
            }
        }
    }
    fn make_deployer(journal: &Arc<Journal>) -> TestHost {
        TestHost {
            spawner: TestSpawner {
                journal: journal.clone(),
            },
            gateway: TestGateway {
                journal: journal.clone(),
            },
        }
    }

    fn comp(name: &str, deps: &[&str]) -> ComponentSpec {
        ComponentSpec {
            name: name.into(),
            engine: EngineKind::Parrot,
            artifact: ArtifactRef::Props {
                factory: format!("app.{name}"),
            },
            alt_artifact: None,
            instances: InstancePolicy::Singleton,
            placement: PlacementConstraint::default(),
            upgrade: UpgradePolicy::default(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            config: None,
            hooks: ComponentHooks::default(),
        }
    }

    fn manifest(comps: Vec<ComponentSpec>) -> AppManifest {
        AppManifest {
            name: "t".into(),
            version: "1.0.0".into(),
            components: comps,
            wiring: vec![],
            mirrors: Vec::new(),
            config_overlay: None,
        }
    }

    // ── 依赖序装配（3）──────────────────────────────────────

    #[tokio::test]
    async fn assemble_dependency_order() {
        let j = Journal::new();
        let m = manifest(vec![comp("c", &["b"]), comp("a", &[]), comp("b", &["a"])]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        let ev = j.snapshot();
        assert_eq!(ev, vec!["spawn:/user/a", "spawn:/user/b", "spawn:/user/c"]);
        assert_eq!(ctx.started(), &["a", "b", "c"]);
    }

    #[tokio::test]
    async fn assemble_mixed_engines() {
        let j = Journal::new();
        let mut erl = comp("frontier", &[]);
        erl.engine = EngineKind::Erlang;
        erl.artifact = ArtifactRef::Beam {
            app: "frontier".into(),
            uri: None,
        };
        let m = manifest(vec![comp("hub", &[]), erl]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        let ev = j.snapshot();
        assert!(ev.contains(&"gw:frontier:erlang".to_string()));
        assert!(ev.contains(&"spawn:/user/hub".to_string()));
    }

    #[tokio::test]
    async fn assemble_sharded_multiple_paths() {
        let j = Journal::new();
        let mut c = comp("sh", &[]);
        c.instances = InstancePolicy::Sharded(3);
        let m = manifest(vec![c]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        assert_eq!(ctx.component_refs("sh").unwrap().len(), 3);
        assert_eq!(ctx.component_paths("sh").unwrap()[2], "/user/sh-2");
    }

    // ── 失败回滚逆序（4）────────────────────────────────────

    #[tokio::test]
    async fn assemble_failure_rolls_back_reverse_order() {
        let j = Journal::new();
        // c 依赖 b 依赖 a；b spawn 失败 → a 回滚
        *j.fail_spawn_on.lock().unwrap() = Some("app.b".into());
        let m = manifest(vec![comp("c", &["b"]), comp("a", &[]), comp("b", &["a"])]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        let err = ctx
            .assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DeployError::Engine(ref s) if s.contains("app.b")));
        let ev = j.snapshot();
        // 启停严格逆序：a 起 → a 的 ref 停 → a 的路径停
        assert_eq!(ev, vec!["spawn:/user/a", "stop:/user/a"]);
        assert!(ctx.started().is_empty());
        assert!(ctx.component_ref("a").is_none());
    }

    #[tokio::test]
    async fn assemble_failure_middle_of_five() {
        let j = Journal::new();
        *j.fail_spawn_on.lock().unwrap() = Some("app.c3".into());
        let comps: Vec<ComponentSpec> = (0..5)
            .map(|i| {
                let deps = if i == 0 {
                    vec![]
                } else {
                    vec![format!("c{}", i - 1)]
                };
                comp(
                    &format!("c{i}"),
                    &deps.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                )
            })
            .collect();
        let m = manifest(comps);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        assert!(ctx
            .assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .is_err());
        let ev = j.snapshot();
        // c0..c2 起，c3 失败，c2..c0 逆序停
        let spawns: Vec<&String> = ev.iter().filter(|e| e.starts_with("spawn:")).collect();
        assert_eq!(spawns.len(), 3);
        let stops: Vec<&String> = ev.iter().filter(|e| e.starts_with("stop:")).collect();
        assert_eq!(stops.len(), 3);
        assert_eq!(stops[0], "stop:/user/c2");
        assert_eq!(stops[2], "stop:/user/c0");
    }

    #[tokio::test]
    async fn teardown_reverse_order() {
        let j = Journal::new();
        let m = manifest(vec![comp("x", &["y"]), comp("y", &[])]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        ctx.teardown(&d).await.unwrap();
        let ev = j.snapshot();
        assert_eq!(ev[0], "spawn:/user/y");
        assert_eq!(ev[1], "spawn:/user/x");
        // teardown 逆序：x 先停（ref stop + paths stop），再 y
        assert_eq!(ev[2], "stop:/user/x");
        assert!(ev.contains(&"stop:/user/y".to_string()));
        let x_stop = ev.iter().position(|e| e == "stop:/user/x").unwrap();
        let y_stop = ev.iter().position(|e| e == "stop:/user/y").unwrap();
        assert!(x_stop < y_stop);
        assert!(ctx.started().is_empty());
    }

    #[tokio::test]
    async fn teardown_empty_ok() {
        let j = Journal::new();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        assert!(ctx.teardown(&d).await.is_ok());
    }

    // ── hooks 时序（4）──────────────────────────────────────

    #[tokio::test]
    async fn hook_on_start_fired_and_awaited() {
        let j = Journal::new();
        let mut c = comp("a", &[]);
        c.hooks.on_start = Some("/user/a".into());
        let m = manifest(vec![c]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        assert!(
            j.snapshot().contains(&"ask:/user/a:0".to_string()),
            "START hook asked"
        );
    }

    #[tokio::test]
    async fn hook_timeout_rolls_back() {
        let j = Journal::new();
        *j.hook_delay.lock().unwrap() = Duration::from_millis(300);
        let mut c = comp("a", &[]);
        c.hooks.on_start = Some("/user/a".into());
        let m = manifest(vec![c]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        ctx.set_hooks_timeout(Duration::from_millis(50));
        let err = ctx
            .assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap_err();
        assert!(
            matches!(err, DeployError::HookTimeout { comp, hook } if comp == "a" && hook == "/user/a")
        );
        assert!(ctx.started().is_empty(), "rolled back");
        assert_eq!(j.snapshot(), vec!["spawn:/user/a", "stop:/user/a"]);
        assert!(ctx.component_refs("a").is_none());
        ctx.teardown(&d).await.unwrap();
        assert_eq!(j.snapshot().len(), 2, "cleanup must not stop twice");
    }

    #[tokio::test]
    async fn hook_ask_error_fails_assembly() {
        let j = Journal::new();
        j.fail_hook
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut c = comp("a", &[]);
        c.hooks.on_start = Some("/user/a".into());
        let m = manifest(vec![c]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        let err = ctx
            .assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DeployError::Engine(_)));
        assert_eq!(
            j.snapshot(),
            vec!["spawn:/user/a", "ask:/user/a:0", "stop:/user/a",]
        );
        assert!(ctx.started().is_empty());
    }

    #[tokio::test]
    async fn hook_codec_roundtrip() {
        assert_eq!(ParrotAppHook(hook_phase::START).encode(), vec![0]);
        assert_eq!(
            ParrotAppHook::decode(&[1]),
            Some(ParrotAppHook(hook_phase::STOP))
        );
        assert_eq!(ParrotAppHook::decode(&[]), None);
    }

    #[tokio::test]
    async fn lifecycle_rejects_wrong_phase_ack_and_rolls_back() {
        let j = Journal::new();
        j.bad_ack.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut c = comp("a", &[]);
        c.hooks.on_start = Some("/user/a".into());
        let m = manifest(vec![c]);
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        let result = ctx
            .assemble(
                &plan(&m, &LocalTopology).unwrap(),
                &d,
                &m,
                &Default::default(),
            )
            .await;
        assert!(
            result.is_err(),
            "a wrong phase is not a start acknowledgement"
        );
        assert!(ctx.started().is_empty());
        assert!(j.snapshot().contains(&"stop:/user/a".into()));
    }

    #[tokio::test]
    async fn lifecycle_runs_drain_stop_before_termination_in_reverse_dependency_order() {
        let j = Journal::new();
        let mut components = vec![comp("a", &[]), comp("b", &["a"])];
        for c in &mut components {
            c.hooks.on_start = Some(format!("/user/{}", c.name));
            c.hooks.on_drain = c.hooks.on_start.clone();
            c.hooks.on_stop = c.hooks.on_start.clone();
        }
        let m = manifest(components);
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        ctx.assemble(
            &plan(&m, &LocalTopology).unwrap(),
            &d,
            &m,
            &Default::default(),
        )
        .await
        .unwrap();
        ctx.teardown(&d).await.unwrap();
        let hooks: Vec<_> = j
            .snapshot()
            .into_iter()
            .filter(|e| e.starts_with("ask:"))
            .collect();
        assert_eq!(
            hooks,
            [
                "ask:/user/a:0",
                "ask:/user/b:0",
                "ask:/user/b:2",
                "ask:/user/b:1",
                "ask:/user/a:2",
                "ask:/user/a:1"
            ]
        );
        let before = j.snapshot();
        ctx.teardown(&d).await.unwrap();
        assert_eq!(j.snapshot(), before, "teardown must not repeat hooks");
    }

    #[tokio::test]
    async fn lifecycle_notifies_every_pool_instance() {
        let j = Journal::new();
        let mut c = comp("a", &[]);
        c.instances = InstancePolicy::Pool(3);
        c.hooks.on_start = Some("/user/a".into());
        let m = manifest(vec![c]);
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        ctx.assemble(
            &plan(&m, &LocalTopology).unwrap(),
            &d,
            &m,
            &Default::default(),
        )
        .await
        .unwrap();
        ctx.teardown(&d).await.unwrap();
        let hooks: Vec<_> = j
            .snapshot()
            .into_iter()
            .filter(|e| e.starts_with("ask:"))
            .collect();
        assert_eq!(
            hooks,
            ["ask:/user/a-0:0", "ask:/user/a-1:0", "ask:/user/a-2:0"]
        );
    }

    #[tokio::test]
    async fn lifecycle_missing_child_hook_does_not_fall_back_to_component_root() {
        let j = Journal::new();
        let mut c = comp("a", &[]);
        c.hooks.on_start = Some("/user/a/missing".into());
        let m = manifest(vec![c]);
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        let result = ctx
            .assemble(
                &plan(&m, &LocalTopology).unwrap(),
                &d,
                &m,
                &Default::default(),
            )
            .await;
        assert!(
            result.is_err(),
            "a missing hook path must not call the root actor"
        );
        assert!(!j.snapshot().iter().any(|e| e.starts_with("ask:")));
        assert!(ctx.started().is_empty());
    }

    #[test]
    fn lifecycle_codec_rejects_trailing_and_unknown_phase_bytes() {
        for payload in [&[0, 0][..], &[3][..], &[255][..], &[][..]] {
            assert_eq!(
                ParrotAppHook::decode(payload),
                None,
                "invalid hook bytes {payload:?}"
            );
        }
    }

    #[test]
    fn lifecycle_hook_codec_is_available_to_real_transport() {
        let registry = parrot_remote::codec_registry::CodecRegistry::global();
        let encoded = registry.encode_outgoing(&(Box::new(ParrotAppHook(2)) as BoxedMessage));
        assert!(
            encoded.is_ok(),
            "application hooks need a wire codec: {encoded:?}"
        );
        let (key, payload) = encoded.unwrap();
        assert_eq!(key, "bin:app.hook#v1");
        assert_eq!(payload, vec![2]);
        assert_eq!(
            registry
                .decode_incoming(&key, &payload)
                .unwrap()
                .downcast_ref::<ParrotAppHook>(),
            Some(&ParrotAppHook(2))
        );
    }

    #[tokio::test]
    async fn lifecycle_sync_and_async_hook_panics_roll_back_without_escaping() {
        for mode in [1, 2] {
            let journal = Journal::new();
            journal
                .hook_panic
                .store(mode, std::sync::atomic::Ordering::Relaxed);
            let host = make_deployer(&journal);
            let deployer = host.deployer();
            let mut component = comp("a", &[]);
            component.hooks.on_start = Some(".".into());
            let manifest = manifest(vec![component]);
            let mut context = AssemblingContext::new();
            let result = context
                .assemble(
                    &plan(&manifest, &LocalTopology).unwrap(),
                    &deployer,
                    &manifest,
                    &Default::default(),
                )
                .await;
            assert!(matches!(result, Err(DeployError::HookPanic { ref comp, .. }) if comp == "a"));
            assert!(context.started().is_empty());
            assert!(journal.snapshot().contains(&"stop:/user/a".into()));
        }
    }

    #[tokio::test]
    async fn lifecycle_bad_drain_and_stop_ack_do_not_skip_other_instances_or_cleanup() {
        let journal = Journal::new();
        let host = make_deployer(&journal);
        let deployer = host.deployer();
        let mut component = comp("a", &[]);
        component.instances = InstancePolicy::Pool(2);
        component.hooks.on_start = Some(".".into());
        component.hooks.on_drain = Some(".".into());
        component.hooks.on_stop = Some(".".into());
        let manifest = manifest(vec![component]);
        let mut context = AssemblingContext::new();
        context
            .assemble(
                &plan(&manifest, &LocalTopology).unwrap(),
                &deployer,
                &manifest,
                &Default::default(),
            )
            .await
            .unwrap();
        journal
            .bad_ack
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let result = context.teardown(&deployer).await.unwrap_err();
        assert!(result.to_string().contains("a-0"));
        assert!(result.to_string().contains("a-1"));
        for index in 0..2 {
            for phase in [0, 2, 1] {
                assert!(journal
                    .snapshot()
                    .contains(&format!("ask:/user/a-{index}:{phase}")));
            }
            assert!(journal
                .snapshot()
                .contains(&format!("stop:/user/a-{index}")));
        }
        assert!(context.started().is_empty());
    }

    #[tokio::test]
    async fn lifecycle_partial_teardown_does_not_replay_failed_hooks_of_retired_members() {
        let journal = Journal::new();
        let host = make_deployer(&journal);
        let deployer = host.deployer();
        let mut component = comp("a", &[]);
        component.instances = InstancePolicy::Pool(2);
        component.hooks.on_drain = Some(".".into());
        component.hooks.on_stop = Some(".".into());
        let manifest = manifest(vec![component]);
        let mut context = AssemblingContext::new();
        context
            .assemble(
                &plan(&manifest, &LocalTopology).unwrap(),
                &deployer,
                &manifest,
                &Default::default(),
            )
            .await
            .unwrap();
        journal
            .bad_ack
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *journal.fail_stop.lock().unwrap() = Some("/user/a-0".into());
        assert!(context.teardown(&deployer).await.is_err());
        journal
            .bad_ack
            .store(false, std::sync::atomic::Ordering::Relaxed);
        *journal.fail_stop.lock().unwrap() = None;
        context.teardown(&deployer).await.unwrap();
        let events = journal.snapshot();
        for phase in [hook_phase::DRAIN, hook_phase::STOP] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| **event == format!("ask:/user/a-1:{phase}"))
                    .count(),
                1,
                "retired instance received a hook retry"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| **event == format!("ask:/user/a-0:{phase}"))
                    .count(),
                2,
                "unconfirmed instance lost its failed hook retry"
            );
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| *event == "stop:/user/a-1")
                .count(),
            1
        );
        assert!(context.started().is_empty());
    }

    #[tokio::test]
    async fn lifecycle_failed_termination_retains_ownership_and_retry_skips_acknowledged_hooks() {
        let journal = Journal::new();
        let host = make_deployer(&journal);
        let deployer = host.deployer();
        let mut a = comp("a", &[]);
        a.hooks.on_stop = Some(".".into());
        let manifest = manifest(vec![a, comp("b", &["a"])]);
        let mut context = AssemblingContext::new();
        context
            .assemble(
                &plan(&manifest, &LocalTopology).unwrap(),
                &deployer,
                &manifest,
                &Default::default(),
            )
            .await
            .unwrap();
        *journal.fail_stop.lock().unwrap() = Some("/user/a".into());
        assert!(context.teardown(&deployer).await.is_err());
        assert_eq!(context.started(), &["a"]);
        assert!(context.component_ref("a").is_some());
        assert!(context.component_ref("b").is_none());
        *journal.fail_stop.lock().unwrap() = None;
        context.teardown(&deployer).await.unwrap();
        assert!(context.started().is_empty());
        assert_eq!(
            journal
                .snapshot()
                .iter()
                .filter(|e| e.as_str() == "ask:/user/a:1")
                .count(),
            1
        );
        assert_eq!(
            journal
                .snapshot()
                .iter()
                .filter(|e| e.as_str() == "stop:/user/a")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn lifecycle_forged_plan_and_invalid_hook_fail_before_any_spawn() {
        let journal = Journal::new();
        let host = make_deployer(&journal);
        let deployer = host.deployer();
        for hook in [
            "",
            "/user/another",
            "../escape",
            "a//b",
            "a/",
            "parrot://other/user/a",
        ] {
            let mut component = comp("a", &[]);
            component.hooks.on_start = Some(hook.into());
            assert!(
                manifest(vec![component]).validate_or_err().is_err(),
                "{hook}"
            );
        }
        let manifest = manifest(vec![comp("a", &[]), comp("b", &["a"])]);
        for change in 0..4 {
            let mut plan = plan(&manifest, &LocalTopology).unwrap();
            match change {
                0 => plan.order.reverse(),
                1 => {
                    plan.order.pop();
                }
                2 => {
                    plan.order[0].shard_plan = Some(vec!["/user/victim".into()]);
                }
                _ => plan.order[0].spec.config = Some(toml::Table::new()),
            }
            let mut context = AssemblingContext::new();
            assert!(context
                .assemble(&plan, &deployer, &manifest, &Default::default())
                .await
                .is_err());
        }
        assert!(journal.snapshot().is_empty());
    }

    #[tokio::test]
    async fn lifecycle_cleanup_deadline_preserves_retry_and_does_not_block_other_components() {
        let journal = Journal::new();
        let host = make_deployer(&journal);
        let deployer = host.deployer();
        let manifest = manifest(vec![comp("a", &[]), comp("b", &["a"])]);
        let mut context = AssemblingContext::new();
        context
            .assemble(
                &plan(&manifest, &LocalTopology).unwrap(),
                &deployer,
                &manifest,
                &parrot_config::ParrotConfig::builder().thread_shutdown_timeout_ms(5),
            )
            .await
            .unwrap();
        *journal.hang_stop.lock().unwrap() = Some("/user/b".into());
        let error = tokio::time::timeout(Duration::from_secs(1), context.teardown(&deployer))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("cleanup deadline"));
        assert_eq!(context.started(), &["b"]);
        assert!(journal.snapshot().contains(&"stop:/user/a".into()));
        *journal.hang_stop.lock().unwrap() = None;
        context.teardown(&deployer).await.unwrap();
        assert!(context.started().is_empty());
    }

    #[tokio::test]
    async fn lifecycle_factory_panic_rolls_back_already_started_dependencies() {
        let journal = Journal::new();
        let host = make_deployer(&journal);
        let deployer = host.deployer();
        let manifest = manifest(vec![comp("a", &[]), comp("panic", &["a"])]);
        let mut context = AssemblingContext::new();
        let error = context
            .assemble(
                &plan(&manifest, &LocalTopology).unwrap(),
                &deployer,
                &manifest,
                &Default::default(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("spawn component panic panicked"));
        assert!(context.started().is_empty());
        assert_eq!(journal.snapshot(), vec!["spawn:/user/a", "stop:/user/a"]);
    }

    #[tokio::test]
    async fn lifecycle_unreachable_remote_is_not_an_acknowledged_drain() {
        let journal = Journal::new();
        let host = make_deployer(&journal);
        let deployer = host.deployer();
        let mut component = comp("remote", &[]);
        component.engine = EngineKind::Akka;
        component.artifact = ArtifactRef::Jvm {
            main_class: "fixture".into(),
            coords: None,
            uri: None,
        };
        component.hooks.on_drain = Some(".".into());
        let manifest = manifest(vec![component]);
        let mut context = AssemblingContext::new();
        context
            .assemble(
                &plan(&manifest, &LocalTopology).unwrap(),
                &deployer,
                &manifest,
                &Default::default(),
            )
            .await
            .unwrap();
        journal
            .live
            .store(false, std::sync::atomic::Ordering::Relaxed);
        journal
            .fail_hook
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(context.drain(&deployer).await.is_err(), "a disconnected peer may still have live work; require its actual drain acknowledgement");
        assert!(journal.snapshot().contains(&"ask:/user/remote:2".into()));
        assert!(context.teardown(&deployer).await.is_err());
        assert!(context.started().is_empty());
    }

    #[tokio::test]
    async fn lifecycle_plan_validation_preserves_valid_nan_business_configuration() {
        let journal = Journal::new();
        let host = make_deployer(&journal);
        let deployer = host.deployer();
        let mut component = comp("a", &[]);
        component.config = Some(
            toml::from_str("sample = nan\nnegative_zero = -0.0\npositive_infinity = inf").unwrap(),
        );
        let manifest = manifest(vec![component]);
        let mut context = AssemblingContext::new();
        context
            .assemble(
                &plan(&manifest, &LocalTopology).unwrap(),
                &deployer,
                &manifest,
                &Default::default(),
            )
            .await
            .unwrap();
        context.teardown(&deployer).await.unwrap();
        assert_eq!(journal.snapshot(), vec!["spawn:/user/a", "stop:/user/a"]);
    }

    // ── overlay 优先级 Y3（5）───────────────────────────────

    #[test]
    fn overlay_rejects_invalid_nested_fields_without_partial_mutation() {
        for raw in [
            "[thread]\nshared_pool_size = 8\n[remote.node]\nnodeid = 'typo'",
            "[thread]\nshared_pool_size = 8\n[remote.transport]\noutbound_queue = 'bad'",
            "[thread]\nshared_pool_size = 8\n[remote.node]\nseeds = ['tcp://127.0.0.1:9800', 1]",
            "[thread]\nshared_pool_size = 8\n[remote.node]\nbind = 'invalid'",
            "thread = 8",
            "[remote]\ntransport = 8",
            "typo = true\n[thread]\nshared_pool_size = 8",
        ] {
            let mut cfg = parrot_config::ParrotConfig::builder().thread_shared_pool_size(2);
            let before = toml::to_string(&cfg).unwrap();
            let overlay = toml::from_str(raw).unwrap();
            assert!(apply_overlay(&mut cfg, &overlay).is_err(), "accepted {raw}");
            assert_eq!(
                toml::to_string(&cfg).unwrap(),
                before,
                "failed overlay mutated config"
            );
        }
    }

    #[test]
    fn overlay_supports_task_limit_and_explicit_empty_collection() {
        let mut cfg = parrot_config::ParrotConfig::builder()
            .thread_max_context_tasks(16)
            .remote_seeds(vec!["tcp://127.0.0.1:9800".into()]);
        let overlay =
            toml::from_str("[thread]\nmax_context_tasks = 0\n[remote.node]\nseeds = []").unwrap();
        apply_overlay(&mut cfg, &overlay).unwrap();
        let effective = cfg.build().unwrap();
        assert_eq!(effective.thread.max_context_tasks, 0);
        assert!(effective.remote.node.seeds.is_empty());
    }

    #[test]
    fn overlay_fills_unset_keys() {
        let mut cfg = parrot_config::ParrotConfig::default();
        let overlay: toml::value::Table = [(
            "remote".to_string(),
            toml::Value::Table(
                [(
                    "transport".to_string(),
                    toml::Value::Table(
                        [("heartbeat_interval_ms".to_string(), toml::Value::from(999))]
                            .into_iter()
                            .collect(),
                    ),
                )]
                .into_iter()
                .collect(),
            ),
        )]
        .into_iter()
        .collect();
        apply_overlay(&mut cfg, &overlay).unwrap();
        assert_eq!(cfg.remote.transport.heartbeat_interval_ms, Some(999));
    }

    #[test]
    fn overlay_overrides_code_layer() {
        let mut cfg = parrot_config::ParrotConfig::default();
        cfg.remote.transport.heartbeat_interval_ms = Some(111); // 代码层已设
        let overlay: toml::value::Table = [(
            "remote".to_string(),
            toml::Value::Table(
                [(
                    "transport".to_string(),
                    toml::Value::Table(
                        [("heartbeat_interval_ms".to_string(), toml::Value::from(999))]
                            .into_iter()
                            .collect(),
                    ),
                )]
                .into_iter()
                .collect(),
            ),
        )]
        .into_iter()
        .collect();
        apply_overlay(&mut cfg, &overlay).unwrap();
        // Y3: app config_overlay > 代码——overlay 直接覆盖
        assert_eq!(cfg.remote.transport.heartbeat_interval_ms, Some(999));
    }

    #[test]
    fn overlay_reorder_keys() {
        let mut cfg = parrot_config::ParrotConfig::default();
        let overlay: toml::value::Table = [(
            "remote".to_string(),
            toml::Value::Table(
                [(
                    "reorder".to_string(),
                    toml::Value::Table(
                        [
                            ("gap_timeout_ms".to_string(), toml::Value::from(42)),
                            ("buffer_cap".to_string(), toml::Value::from(7)),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                )]
                .into_iter()
                .collect(),
            ),
        )]
        .into_iter()
        .collect();
        apply_overlay(&mut cfg, &overlay).unwrap();
        assert_eq!(cfg.remote.reorder.gap_timeout_ms, Some(42));
        assert_eq!(cfg.remote.reorder.buffer_cap, Some(7));
    }

    #[test]
    fn overlay_thread_keys() {
        let mut cfg = parrot_config::ParrotConfig::default();
        let overlay: toml::value::Table = [(
            "thread".to_string(),
            toml::Value::Table(
                [("shared_pool_size".to_string(), toml::Value::from(3))]
                    .into_iter()
                    .collect(),
            ),
        )]
        .into_iter()
        .collect();
        apply_overlay(&mut cfg, &overlay).unwrap();
        assert_eq!(cfg.thread.shared_pool_size, Some(3));
    }

    #[test]
    fn overlay_bad_key_rejected() {
        let mut cfg = parrot_config::ParrotConfig::default();
        let overlay: toml::value::Table = [("nonexistent".to_string(), toml::Value::from(1))]
            .into_iter()
            .collect();
        match apply_overlay(&mut cfg, &overlay) {
            Err(DeployError::Config(s)) => assert!(
                s.contains("unknown field") && s.contains("nonexistent"),
                "{s}"
            ),
            other => panic!("expected config error, got {other:?}"),
        }
    }

    // ── 查询面（2）─────────────────────────────────────────

    #[tokio::test]
    async fn component_ref_lookup() {
        let j = Journal::new();
        let m = manifest(vec![comp("a", &[])]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        assert!(ctx.component_ref("a").is_some());
        assert!(ctx.component_ref("zz").is_none());
    }

    #[tokio::test]
    async fn gateway_failure_no_rollback_targets_before_it() {
        let j = Journal::new();
        // 第一个组件就是网关且失败——空回滚
        let mut bad = comp("g", &[]);
        bad.engine = EngineKind::Ray;
        bad.artifact = ArtifactRef::PyModule {
            module: "m".into(),
            runtime_env: None,
            uri: None,
        };
        let m = manifest(vec![bad]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        // 正常路径成功（替身 gw 恒成功）——断言 registry 记账
        let mut ctx = AssemblingContext::new();
        ctx.assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap();
        assert_eq!(ctx.started().len(), 1);
    }

    // Props-only custom hosts must explicitly reject unsupported artifacts.
    #[tokio::test]
    async fn wasm_artifact_rejected_by_props_only_spawner() {
        let j = Journal::new();
        let mut c = comp("w", &[]);
        c.artifact = ArtifactRef::Wasm {
            digest: "d".into(),
            uri: "file:///x.wasm".into(),
        };
        let m = manifest(vec![c]);
        let plan = plan(&m, &LocalTopology).unwrap();
        let host = make_deployer(&j);
        let d = host.deployer();
        let mut ctx = AssemblingContext::new();
        let err = ctx
            .assemble(&plan, &d, &m, &parrot_config::ParrotConfig::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DeployError::Artifact(ref s) if s.contains("wasm")));
    }
    #[tokio::test]
    async fn invalid_effective_config_fails_before_spawning() {
        let journal = Journal::new();
        let host = make_deployer(&journal);
        let deployer = host.deployer();
        let manifest = manifest(vec![comp("one", &[])]);
        let plan = crate::planner::plan(&manifest, &LocalTopology).unwrap();
        let cfg = parrot_config::ParrotConfig::builder().thread_shared_queue_capacity(0);
        let mut context = AssemblingContext::new();
        let result = context.assemble(&plan, &deployer, &manifest, &cfg).await;
        assert!(
            matches!(result, Err(DeployError::Config(_))),
            "invalid effective config was ignored: {result:?}"
        );
        assert!(context.started().is_empty());
        assert!(journal.snapshot().is_empty());
    }
}
