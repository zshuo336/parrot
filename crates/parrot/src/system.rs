mod runtime;
mod shutdown;
mod topology;
use crate::actix::ActixActorSystem;
use crate::thread::system::ThreadActorSystem;
use anyhow;
use async_trait::async_trait;
use parrot_api::{
    actor::Actor,
    address::{ActorPath, ActorRef},
    context::ActorContext,
    message::Message,
    system::{
        ActorSystem, ActorSystemConfig, SystemError, SystemResources, SystemState, SystemStatus,
    },
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Instant;

// Commit first, release every displaced registration outside all locks, then
// report cleanup errors. One destructor panic must not skip later retirements.
fn retire_registrations<T>(values: impl IntoIterator<Item = T>) -> Result<(), SystemError> {
    let mut failed = false;
    for value in values {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value)))
        {
            failed = true;
            if let Err(secondary) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                std::mem::forget(secondary);
            }
        }
    }
    if failed {
        Err(SystemError::Other(anyhow::anyhow!(
            "registration committed but displaced resource cleanup panicked"
        )))
    } else {
        Ok(())
    }
}

fn reject_spawn<A: Send + 'static, C: Send + 'static>(
    actor: crate::owned_payload::OwnedPayload<A>,
    config: crate::owned_payload::OwnedPayload<C>,
    error: SystemError,
) -> parrot_api::types::BoxedFuture<'static, Result<Box<dyn ActorRef>, SystemError>> {
    Box::pin(async move {
        let (actor, config) = tokio::join!(actor.retire(), config.retire());
        actor.and(config).map_err(SystemError::ActorError)?;
        Err(error)
    })
}

/// Supported ActorSystem implementation types
#[derive(Clone)]
#[allow(clippy::large_enum_variant)] // 克隆热路径优先：避免 Box 间接层
pub enum ActorSystemImpl {
    /// Actix implementation
    Actix(ActixActorSystem),
    /// Thread engine implementation (native shared-pool / dedicated-thread)
    Thread(Arc<ThreadActorSystem>),
    /// Any production asynchronous runtime, without a new enum arm per backend.
    Runtime(Arc<dyn parrot_api::engine::AsyncEngineRuntime>),
}

impl ActorSystemImpl {
    fn identity(&self) -> (u8, usize) {
        match self {
            Self::Actix(system) => (0, Arc::as_ptr(&system.registry) as usize),
            Self::Thread(system) => (1, Arc::as_ptr(&system.registry) as usize),
            Self::Runtime(system) => (2, Arc::as_ptr(system) as *const () as usize),
        }
    }

    /// Spawn a type-erased actor on either built-in engine. Each engine adapts
    /// the public `dyn ActorContext` contract to its concrete native context.
    pub fn spawn_root_boxed<'a>(
        &'a self,
        actor: Box<dyn Actor<Config = Box<dyn std::any::Any + Send>, Context = dyn ActorContext>>,
        config: Box<dyn std::any::Any + Send>,
    ) -> parrot_api::types::BoxedFuture<'a, Result<Box<dyn ActorRef>, SystemError>> {
        // Construct the native owner now, before returning any outer future.
        match self {
            Self::Thread(sys) => {
                <ThreadActorSystem as ActorSystem>::spawn_root_boxed(&**sys, actor, config)
            }
            Self::Actix(sys) => Box::pin(sys.spawn_root_boxed(actor, config)),
            Self::Runtime(sys) => sys.spawn(actor, config, Default::default()),
        }
    }

    /// Forward spawn_root_typed method; generic version returns an error
    pub fn spawn_root_typed<'a, A: Actor + 'static>(
        &'a self,
        actor: A,
        config: A::Config,
    ) -> parrot_api::types::BoxedFuture<'a, Result<Box<dyn ActorRef>, SystemError>> {
        let actor = crate::owned_payload::OwnedPayload::unbound(actor);
        let config = crate::owned_payload::OwnedPayload::unbound(config);
        let message = match self {
            Self::Actix(_) => {
                "Generic actor creation not supported directly. Use spawn_root_typed_actix for Actix system."
            }
            Self::Thread(_) => {
                "Generic actor creation not supported directly. Use spawn_root_thread for the thread system."
            }
            Self::Runtime(_) => "Use the asynchronous runtime interface for engine-neutral actors",
        };
        reject_spawn(
            actor,
            config,
            SystemError::ActorCreationError(message.into()),
        )
    }

    /// Actix-specific spawn method with necessary type constraints
    pub fn spawn_root_typed_actix<'a, A>(
        &'a self,
        actor: A,
        config: A::Config,
    ) -> parrot_api::types::BoxedFuture<'a, Result<Box<dyn ActorRef>, SystemError>>
    where
        A: Actor<Context = crate::actix::context::ActixContext<crate::actix::actor::ActixActor<A>>>
            + parrot_api::actor::ActixEngineExt<
                Context = crate::actix::context::ActixContext<crate::actix::actor::ActixActor<A>>,
            > + Unpin
            + 'static,
    {
        match self {
            Self::Actix(sys) => Box::pin(sys.spawn_root_typed(actor, config)),
            _ => reject_spawn(
                crate::owned_payload::OwnedPayload::unbound(actor),
                crate::owned_payload::OwnedPayload::unbound(config),
                SystemError::ActorCreationError("Not an Actix system".into()),
            ),
        }
    }

    /// Forward get_actor method
    pub async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        // Clone path to avoid borrowing the original in the async block
        let path_copy = ActorPath {
            path: path.path.clone(),
            target: path.target.clone(),
        };
        let self_clone = self.clone();

        async move {
            match self_clone {
                ActorSystemImpl::Actix(sys) => {
                    // ActixActorSystem's get_actor method expects a &String
                    sys.get_actor(&path_copy.path).await
                }
                ActorSystemImpl::Thread(sys) => {
                    use parrot_api::system::ActorSystem as _;
                    sys.get_actor(&path_copy).await
                }
                ActorSystemImpl::Runtime(sys) => sys.get_actor(&path_copy.path).await,
            }
        }
        .await
    }

    /// Forward broadcast method
    pub async fn broadcast<M: Message + Clone + 'static>(&self, msg: M) -> Result<(), SystemError> {
        // Clone the message to avoid borrowing the original in the async block
        let msg_copy = msg.clone();
        let self_clone = self.clone();

        async move {
            match self_clone {
                ActorSystemImpl::Actix(sys) => sys.broadcast(msg_copy).await,
                ActorSystemImpl::Thread(sys) => {
                    use parrot_api::system::ActorSystem as _;
                    sys.broadcast(msg_copy).await
                }
                ActorSystemImpl::Runtime(_) => Err(SystemError::Other(anyhow::anyhow!(
                    "registered asynchronous runtime does not expose broadcast"
                ))),
            }
        }
        .await
    }

    /// Forward shutdown method
    pub async fn shutdown(self) -> Result<(), SystemError> {
        match self {
            ActorSystemImpl::Actix(sys) => sys.shutdown().await,
            ActorSystemImpl::Thread(sys) => sys
                .shutdown_internal()
                .await
                .map_err(|e| SystemError::Other(anyhow::anyhow!(e.to_string()))),
            ActorSystemImpl::Runtime(sys) => sys.shutdown().await,
        }
    }
}

/// 远程网关（trait 倒置——parrot 只认接口，parrot-remote 在应用层注入）。
///
/// 为什么 trait 而非直接依赖：parrot import parrot-remote 会引入可选依赖
/// 耦合（不用 remote 的用户也被拖编译）。倒置后桥/远程件只依赖 parrot-api
/// （E5.2 分层铁律，与网关模式同构）。
pub trait RemoteGateway: Send + Sync + 'static {
    /// parrot:// 前缀路径 → 远程 ActorRef（本地未命中时的三级路由出口）
    fn lookup(&self, path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>>;
}

/// Receptionist 网关（K2）——trait 定义在 parrot-api::receptionist（分层：
/// parrot 与 parrot-remote 共见，同 RemoteGateway 的倒置模式）。
pub use parrot_api::receptionist::ReceptionistGateway;

type PrefixHandlers = RwLock<Vec<(String, Arc<dyn ActorRef>)>>;

/// ParrotActorSystem manages multiple ActorSystem implementations
pub struct ParrotActorSystem {
    started_at: Instant,
    // Main system configuration
    config: ActorSystemConfig,
    // Store registered ActorSystem implementations, using name as key
    systems: Arc<RwLock<HashMap<String, ActorSystemImpl>>>,
    // Default system name
    default_system: Arc<RwLock<Option<String>>>,
    // Remote gateway (optional; DEV_01 §3.7 facade 三级路由的远程出口)
    remote: RwLock<Option<std::sync::Arc<dyn RemoteGateway>>>,
    // Receptionist gateway（K2——双引擎 context 三方法转发出口）
    receptionist: RwLock<Option<std::sync::Arc<dyn ReceptionistGateway>>>,
    // 前缀通配处理器（DEV_04 §7.1——ShardRouter 的 `/user/entity-*` 注册面；
    // 具体本地actor未命中之后，按最长前缀兜底）
    prefix_handlers: Arc<PrefixHandlers>,
    // 镜像策略（F2 DEV_09 §3.6）：src_prefix 命中的目标引用被包成双写
    // MirrorRef——tell/ask 原路 + 镜像副本（fire-and-forget，失败不回传）
    mirrors: Arc<crate::mirror::MirrorRegistry>,
    lifecycle: Arc<shutdown::Lifecycle>,
    topology: Arc<topology::Node>,
    runtime: Option<tokio::runtime::Handle>,
    cleanup_slots: Arc<tokio::sync::Semaphore>,
}

// Workers resolve the current shadow generation for each copy. Weak registry
// handles avoid retaining the facade/engines through old source references.
struct LocalMirrorLookup {
    systems: std::sync::Weak<RwLock<HashMap<String, ActorSystemImpl>>>,
    default_system: std::sync::Weak<RwLock<Option<String>>>,
    prefixes: std::sync::Weak<PrefixHandlers>,
}
impl crate::mirror::MirrorLookup for LocalMirrorLookup {
    fn lookup<'a>(
        &'a self,
        path: &'a str,
    ) -> parrot_api::types::BoxedFuture<'a, Option<parrot_api::types::BoxedActorRef>> {
        Box::pin(async move {
            let default = self.default_system.upgrade()?.read().ok()?.clone();
            let systems = {
                let registry = self.systems.upgrade()?;
                let guard = registry.read().ok()?;
                let mut systems: Vec<_> = guard
                    .iter()
                    .map(|(name, system)| (name.clone(), system.clone()))
                    .collect();
                systems.sort_by_key(|(name, _)| (Some(name) != default.as_ref(), name.clone()));
                systems
            };
            let actor_path = ActorPath::placeholder(path);
            for (_, system) in systems {
                if let Some(actor) = system.get_actor(&actor_path).await {
                    return Some(actor);
                }
            }
            let registry = self.prefixes.upgrade()?;
            let handler = registry
                .read()
                .ok()?
                .iter()
                .find(|(prefix, _)| path.starts_with(prefix))
                .map(|(_, actor)| actor.clone());
            handler.map(|actor| actor.clone_boxed())
        })
    }
}

impl ParrotActorSystem {
    /// Create new ParrotActorSystem
    pub async fn new(config: ActorSystemConfig) -> Result<Self, SystemError> {
        Ok(Self {
            started_at: Instant::now(),
            config,
            systems: Arc::new(RwLock::new(HashMap::new())),
            default_system: Arc::new(RwLock::new(None)),
            remote: RwLock::new(None),
            receptionist: RwLock::new(None),
            prefix_handlers: Arc::new(RwLock::new(Vec::new())),
            mirrors: Arc::new(crate::mirror::MirrorRegistry::default()),
            lifecycle: Arc::default(),
            topology: Arc::default(),
            runtime: tokio::runtime::Handle::try_current().ok(),
            cleanup_slots: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    /// Register a receptionist gateway（K2 注入面——context 三方法出口）。
    ///
    /// Publish to registered Thread and Actix systems. Systems registered later
    /// inherit the current gateway before they become visible in this facade.
    pub async fn register_receptionist_gateway(
        &self,
        gw: std::sync::Arc<dyn ReceptionistGateway>,
    ) -> Result<(), SystemError> {
        // Serialize publication, propagation and late engine registration in
        // systems -> facade gateway -> engine gateway lock order. Arc swaps do
        // not call user code. Retain every displaced object until ALL locks end.
        let retired = {
            let systems = self
                .systems
                .read()
                .map_err(|_| SystemError::Other(anyhow::anyhow!("systems lock poisoned")))?;
            let mut current = self
                .receptionist
                .write()
                .map_err(|_| SystemError::Other(anyhow::anyhow!("receptionist lock poisoned")))?;
            let mut retired = vec![current.replace(gw.clone())];
            for system in systems.values() {
                retired.push(match system {
                    ActorSystemImpl::Thread(thread) => thread.swap_receptionist_gateway(gw.clone()),
                    ActorSystemImpl::Actix(actix) => actix.swap_receptionist_gateway(gw.clone()),
                    ActorSystemImpl::Runtime(_) => None,
                });
            }
            retired
        };
        retire_registrations(retired)
    }

    /// 全局 receptionist 句柄（双引擎 context 转发用——Weak 防循环持有）。
    pub fn receptionist_gateway(&self) -> Option<std::sync::Arc<dyn ReceptionistGateway>> {
        self.receptionist.read().ok().and_then(|g| g.clone())
    }

    /// Register a remote gateway（与 register_thread_system 同族命名/风格）。
    ///
    /// `parrot://` 显式远程路径优先交给网关，其结果不会回退到本地。
    /// 其它路径仍依次查询默认引擎、其它引擎、最长前缀处理器。
    pub async fn register_remote_gateway(
        &self,
        gw: std::sync::Arc<dyn RemoteGateway>,
    ) -> Result<(), SystemError> {
        let old = self
            .remote
            .write()
            .map_err(|_| SystemError::Other(anyhow::anyhow!("remote gateway lock poisoned")))?
            .replace(gw);
        retire_registrations([old])
    }

    /// 配置切面一站式入口：单进程纯本地引擎（thread）从折叠配置构建。
    ///
    /// 业务代码零部署参数——物理参数全部来自 `ParrotConfig`（三层：
    /// 代码 > 文件 > 默认）。远程接入见 parrot-remote 的
    /// `RemoteConfig::from_resolved`。
    pub async fn from_config(
        r: &parrot_config::Resolved,
    ) -> Result<Arc<crate::thread::system::ThreadActorSystem>, SystemError> {
        Ok(crate::thread::system::ThreadActorSystem::shared(
            crate::thread::config::ThreadActorSystemConfig::from_resolved(r),
        ))
    }

    /// Register an Actix system
    ///
    /// Names are opaque keys, including the empty string. The first registered
    /// engine becomes the default even when `set_as_default` is false.
    pub async fn register_actix_system(
        &self,
        name: String,
        system: ActixActorSystem,
        set_as_default: bool,
    ) -> Result<(), SystemError> {
        let topology = topology::mutation();
        let lifecycle = self.lifecycle.receipt.lock().unwrap();
        if lifecycle.is_some() {
            return Err(SystemError::ShuttingDown);
        }
        let mut systems = self
            .systems
            .write()
            .map_err(|_| SystemError::Other(anyhow::anyhow!("Failed to acquire write lock")))?;

        let retired_gateway = self
            .receptionist_gateway()
            .and_then(|gw| system.swap_receptionist_gateway(gw));
        let old = systems.insert(name.clone(), ActorSystemImpl::Actix(system));

        // Set as default if needed
        if set_as_default || self.default_system.read().unwrap().is_none() {
            *self.default_system.write().unwrap() = Some(name.clone());
        }

        self.topology.set(&name, None);
        drop(systems);
        drop(lifecycle);
        drop(topology);
        let gateway_cleanup = retire_registrations([retired_gateway]);
        let system_cleanup = retire_registrations([old]);
        gateway_cleanup.and(system_cleanup)
    }

    /// Register a thread engine system
    ///
    /// Names are opaque keys, including the empty string. The first registered
    /// engine becomes the default even when `set_as_default` is false.
    pub async fn register_thread_system(
        &self,
        name: String,
        system: Arc<ThreadActorSystem>,
        set_as_default: bool,
    ) -> Result<(), SystemError> {
        let topology = topology::mutation();
        let lifecycle = self.lifecycle.receipt.lock().unwrap();
        if lifecycle.is_some() {
            return Err(SystemError::ShuttingDown);
        }
        let mut systems = self
            .systems
            .write()
            .map_err(|_| SystemError::Other(anyhow::anyhow!("Failed to acquire write lock")))?;

        // No user destructor may run while this facade's registry is locked.
        let retired_gateway = self
            .receptionist_gateway()
            .and_then(|gw| system.swap_receptionist_gateway(gw));
        let old = systems.insert(name.clone(), ActorSystemImpl::Thread(system));
        if set_as_default || self.default_system.read().unwrap().is_none() {
            *self.default_system.write().unwrap() = Some(name.clone());
        }
        self.topology.set(&name, None);
        drop(systems);
        drop(lifecycle);
        drop(topology);
        // Retire both even if one user destructor panics.
        let gateway_cleanup = retire_registrations([retired_gateway]);
        let system_cleanup = retire_registrations([old]);
        gateway_cleanup.and(system_cleanup)
    }

    /// Set default system
    pub fn set_default_system(&self, name: &str) -> Result<(), SystemError> {
        let lifecycle = self.lifecycle.receipt.lock().unwrap();
        if lifecycle.is_some() {
            return Err(SystemError::ShuttingDown);
        }
        // Verify system exists
        let systems = self
            .systems
            .read()
            .map_err(|_| SystemError::Other(anyhow::anyhow!("Failed to acquire read lock")))?;

        if !systems.contains_key(name) {
            return Err(SystemError::Other(anyhow::anyhow!(format!(
                "System not found: {}",
                name
            ))));
        }

        // Set as default
        *self.default_system.write().unwrap() = Some(name.to_string());
        Ok(())
    }

    /// Get default system
    #[allow(dead_code)] // 公共 API 委托入口保留
    fn get_default_system_impl(&self) -> Result<ActorSystemImpl, SystemError> {
        // Release the default-name guard before taking systems: registration
        // uses systems -> default and must never encounter the reverse order.
        let default_name = self.get_default_system_name()?;
        self.get_system_impl(&default_name)
    }

    /// Get system by name
    fn get_system_impl(&self, name: &str) -> Result<ActorSystemImpl, SystemError> {
        let systems = self
            .systems
            .read()
            .map_err(|_| SystemError::Other(anyhow::anyhow!("Failed to acquire read lock")))?;

        // Clone the system instance to release the read lock
        match systems.get(name) {
            Some(system) => Ok(system.clone()),
            None => Err(SystemError::Other(anyhow::anyhow!(format!(
                "System not found: {}",
                name
            )))),
        }
    }

    /// List all registered system names
    pub fn list_registered_systems(&self) -> Result<Vec<String>, SystemError> {
        let systems = self
            .systems
            .read()
            .map_err(|_| SystemError::Other(anyhow::anyhow!("Failed to acquire read lock")))?;

        Ok(systems.keys().cloned().collect())
    }

    /// Create actor in default system
    pub fn internal_spawn_actor<'a, A: Actor + 'static>(
        &'a self,
        actor: A,
        config: A::Config,
    ) -> parrot_api::types::BoxedFuture<'a, Result<Box<dyn ActorRef>, SystemError>> {
        let actor = self.own(actor);
        let config = self.own(config);
        Box::pin(async move {
            let system = match self.get_creation_system(None) {
                Ok(system) => system,
                Err(error) => return reject_spawn(actor, config, error).await,
            };
            system.spawn_root_typed(actor.take(), config.take()).await
        })
    }

    /// Create actor in specified system
    pub fn spawn_actor_in_system<'a, A: Actor + 'static>(
        &'a self,
        system_name: &'a str,
        actor: A,
        config: A::Config,
    ) -> parrot_api::types::BoxedFuture<'a, Result<Box<dyn ActorRef>, SystemError>> {
        let actor = self.own(actor);
        let config = self.own(config);
        Box::pin(async move {
            let system = match self.get_creation_system(Some(system_name)) {
                Ok(system) => system,
                Err(error) => return reject_spawn(actor, config, error).await,
            };
            system.spawn_root_typed(actor.take(), config.take()).await
        })
    }

    /// Query actor by path
    pub async fn internal_get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        // 远程分支（一级）：parrot:// 前缀且网关注册 → 远程 ref。
        // 置于本地解析之前——parrot:// 是显式远程意图（05 §3.2 解析管线：
        // 显式协议前缀优先于本地命中；本地系统注册的是 /user/... 本地路径）
        if path.path.starts_with("parrot://") {
            let gateway = self.remote.read().ok().and_then(|g| g.clone());
            if let Some(gateway) = gateway {
                return gateway.lookup(&path.path);
            }
        }

        // Try to get the actor from the default system
        // An empty name is a valid registry key, not an absent default.
        let default_system = self.get_default_system_name().ok();
        if let Some(default_name) = default_system.as_ref()
            && let Ok(system) = self.get_system_impl(default_name)
            && let Some(actor) = system.get_actor(path).await
        {
            return Some(actor);
        }

        // If not found in the default system, try all registered systems
        if let Ok(system_names) = self.list_registered_systems() {
            for name in system_names {
                // Skip the default system which has already been checked
                if default_system.as_deref() == Some(name.as_str()) {
                    continue;
                }

                if let Ok(system) = self.get_system_impl(&name)
                    && let Some(actor) = system.get_actor(path).await
                {
                    return Some(actor);
                }
            }
        }

        // 前缀通配兜底（DEV_04 §7.1）：显式 actor 未命中 → 最长前缀
        // handler（ShardRouter 惰性激活入口）。位于本地查找之后——
        // 显式 spawn 的 actor 优先于通配。
        let handler = self.prefix_handlers.read().ok().and_then(|handlers| {
            handlers
                .iter()
                .find(|(p, _)| path.path.starts_with(p.as_str()))
                .map(|(_, handler)| handler.clone())
        });
        handler.map(|handler| handler.clone_boxed())
    }

    /// get_actor 镜像包装出口（F2）：命中策略的引用包 MirrorRef 双写。
    pub(crate) async fn get_actor_mirrored(
        &self,
        path: &parrot_api::address::ActorPath,
    ) -> Option<Box<dyn ActorRef>> {
        let actor = self.internal_get_actor(path).await?;
        if let Some(handle) = self.mirrors.find(&path.path) {
            let lookup = Arc::new(LocalMirrorLookup {
                systems: Arc::downgrade(&self.systems),
                default_system: Arc::downgrade(&self.default_system),
                prefixes: Arc::downgrade(&self.prefix_handlers),
            });
            return Some(Box::new(crate::mirror::MirrorRef::new(
                std::sync::Arc::from(actor),
                lookup,
                handle,
            )));
        }
        Some(actor)
    }

    /// Get the default system name
    fn get_default_system_name(&self) -> Result<String, SystemError> {
        let guard = self
            .default_system
            .read()
            .map_err(|_| SystemError::Other(anyhow::anyhow!("Failed to acquire read lock")))?;

        guard
            .clone()
            .ok_or_else(|| SystemError::Other(anyhow::anyhow!("No default system registered")))
    }

    /// Broadcast message to all systems
    pub async fn internal_broadcast<M: Message + Clone + 'static>(
        &self,
        msg: M,
    ) -> Result<(), SystemError> {
        // Capture one stable selection before invoking any user Clone or engine.
        let systems: Vec<_> = self.systems.read().unwrap().values().cloned().collect();
        if systems
            .iter()
            .any(|system| matches!(system, ActorSystemImpl::Runtime(_)))
        {
            return Err(SystemError::Other(anyhow::anyhow!(
                "registered asynchronous runtime does not expose broadcast"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for system in systems {
            if seen.insert(system.identity()) {
                system.broadcast(msg.clone()).await?;
            }
        }

        Ok(())
    }

    /// Shutdown all systems
    pub async fn internal_shutdown(self) -> Result<(), SystemError> {
        self.shutdown_runtime().await
    }

    /// Spawn an actor specific to the Actix system
    ///
    /// This method is an Actix-specific version of spawn_root_typed,
    /// designed for actors compatible with the Actix backend
    pub fn spawn_root_actix<'a, A>(
        &'a self,
        actor: A,
        config: A::Config,
    ) -> parrot_api::types::BoxedFuture<'a, Result<Box<dyn ActorRef>, SystemError>>
    where
        A: Actor<Context = crate::actix::context::ActixContext<crate::actix::actor::ActixActor<A>>>
            + parrot_api::actor::ActixEngineExt<
                Context = crate::actix::context::ActixContext<crate::actix::actor::ActixActor<A>>,
            > + Unpin
            + 'static,
    {
        let actor = self.own(actor);
        let config = self.own(config);
        Box::pin(async move {
            let system = match self.get_creation_system(None) {
                Ok(system) => system,
                Err(error) => return reject_spawn(actor, config, error).await,
            };
            match system {
                ActorSystemImpl::Actix(_) => {
                    system
                        .spawn_root_typed_actix(actor.take(), config.take())
                        .await
                }
                _ => {
                    reject_spawn(
                        actor,
                        config,
                        SystemError::ActorCreationError(
                            "Default system is not an Actix system".into(),
                        ),
                    )
                    .await
                }
            }
        })
    }

    /// Spawn an actor specific to the thread engine system
    ///
    /// This method is a thread-engine-specific version of spawn_root_typed,
    /// designed for actors whose `Context` is `ThreadContext<A>`.
    pub fn spawn_root_thread<'a, A>(
        &'a self,
        actor: A,
        config: A::Config,
    ) -> parrot_api::types::BoxedFuture<'a, Result<Box<dyn ActorRef>, SystemError>>
    where
        A: Actor<Context = crate::thread::context::ThreadContext<A>> + Send + Sync + 'static,
    {
        let actor = self.own(actor);
        let config = self.own(config);
        Box::pin(async move {
            let system = match self.get_creation_system(None) {
                Ok(system) => system,
                Err(error) => return reject_spawn(actor, config, error).await,
            };
            match system {
                ActorSystemImpl::Thread(sys) => {
                    let reference = sys
                        .spawn_root_typed_thread(actor.take(), config.take())
                        .await
                        .map_err(|error| SystemError::ActorCreationError(error.to_string()))?;
                    Ok(Box::new(reference) as Box<dyn ActorRef>)
                }
                _ => {
                    reject_spawn(
                        actor,
                        config,
                        SystemError::ActorCreationError(
                            "Default system is not a thread engine system".to_string(),
                        ),
                    )
                    .await
                }
            }
        })
    }

    /// Spawn an actor specific to the thread engine system in a named system
    pub fn spawn_root_thread_in_system<'a, A>(
        &'a self,
        system_name: &'a str,
        actor: A,
        config: A::Config,
    ) -> parrot_api::types::BoxedFuture<'a, Result<Box<dyn ActorRef>, SystemError>>
    where
        A: Actor<Context = crate::thread::context::ThreadContext<A>> + Send + Sync + 'static,
    {
        let actor = self.own(actor);
        let config = self.own(config);
        Box::pin(async move {
            let system = match self.get_creation_system(Some(system_name)) {
                Ok(system) => system,
                Err(error) => return reject_spawn(actor, config, error).await,
            };
            match system {
                ActorSystemImpl::Thread(sys) => {
                    let reference = sys
                        .spawn_root_typed_thread(actor.take(), config.take())
                        .await
                        .map_err(|error| SystemError::ActorCreationError(error.to_string()))?;
                    Ok(Box::new(reference) as Box<dyn ActorRef>)
                }
                _ => {
                    reject_spawn(
                        actor,
                        config,
                        SystemError::ActorCreationError(format!(
                            "System '{}' is not a thread engine system",
                            system_name
                        )),
                    )
                    .await
                }
            }
        })
    }

    /// 注册前缀通配处理器（DEV_04 §7.1——ShardRouter 的 facade 注册面）。
    ///
    /// `prefix` 如 "/user/entity-"：任何以之前缀开头的路径未命中显式
    /// actor 时路由到 `handler`（最长前缀优先）。用于 cluster sharding
    /// 的实体惰性激活——`/user/entity-{key}` → ShardRouter → ring 放置。
    ///
    /// 显式parrot://远程意图优先，其次本地actor，最后最长前缀。
    /// 同一prefix再次注册替换旧handler；已捕获的引用不被撤销。
    pub fn register_prefix_handler(
        &self,
        prefix: impl Into<String>,
        handler: std::sync::Arc<dyn ActorRef>,
    ) -> Result<(), SystemError> {
        // Into<String> and a rejected handler's Drop are user code too.
        let prefix = prefix.into();
        if !prefix.ends_with('-') && !prefix.ends_with('/') {
            return Err(SystemError::ActorCreationError(format!(
                "prefix handler must end with '-' or '/' (got {prefix:?})——通配语义锚点"
            )));
        }
        let old = {
            let mut handlers = self
                .prefix_handlers
                .write()
                .map_err(|_| SystemError::Other(anyhow::anyhow!("prefix handler lock poisoned")))?;
            if let Some((_, existing)) = handlers.iter_mut().find(|(p, _)| p == &prefix) {
                Some(std::mem::replace(existing, handler))
            } else {
                let pos = handlers
                    .iter()
                    .position(|(p, _)| p.len() < prefix.len())
                    .unwrap_or(handlers.len());
                handlers.insert(pos, (prefix, handler));
                None
            }
        };
        retire_registrations([old])
    }

    /// Remove only this exact prefix. Captured references remain valid; future
    /// lookups use the remaining longest prefix or explicit local actor.
    pub fn unregister_prefix_handler(&self, prefix: &str) -> Result<bool, SystemError> {
        let old = {
            let mut handlers = self
                .prefix_handlers
                .write()
                .map_err(|_| SystemError::Other(anyhow::anyhow!("prefix handler lock poisoned")))?;
            handlers
                .iter()
                .position(|(p, _)| p == prefix)
                .map(|index| handlers.remove(index))
        };
        let existed = old.is_some();
        retire_registrations([old])?;
        Ok(existed)
    }

    /// Register an explicit message copying/size-accounting contract.
    pub fn register_mirror_copier(
        &self,
        copier: crate::mirror::MirrorCopier,
    ) -> Result<(), crate::mirror::MirrorError> {
        self.mirrors.register_copier(copier)
    }

    /// Register bounded best-effort shadow delivery for facade lookups.
    /// At least one message copier must be registered first. The returned
    /// handle exposes failures and permits explicit cancellation/join.
    pub fn register_mirror_policy(
        &self,
        policy: crate::mirror::MirrorPolicy,
    ) -> Result<crate::mirror::MirrorHandle, crate::mirror::MirrorError> {
        self.register_mirror_policy_with_options(policy, crate::mirror::MirrorOptions::default())
    }
    pub fn register_mirror_policy_with_options(
        &self,
        policy: crate::mirror::MirrorPolicy,
        options: crate::mirror::MirrorOptions,
    ) -> Result<crate::mirror::MirrorHandle, crate::mirror::MirrorError> {
        self.mirrors.register(policy, options)
    }

    /// Cancel this policy, including copies from previously obtained refs.
    /// Await the returned handle's shutdown to join its worker.
    pub fn remove_mirror_policy(&self, src_prefix: &str) -> Option<crate::mirror::MirrorHandle> {
        self.mirrors.remove(src_prefix)
    }

    /// Longest matching source prefix; shadow target itself is excluded.
    pub fn mirror_policy_for(&self, path: &str) -> Option<crate::mirror::MirrorPolicy> {
        self.mirrors
            .find(path)
            .map(|handle| handle.policy().clone())
    }

    /// Get a thread engine system by name
    pub fn get_thread_system(&self, name: &str) -> Result<Arc<ThreadActorSystem>, SystemError> {
        match self.get_system_impl(name)? {
            ActorSystemImpl::Thread(sys) => Ok(sys),
            _ => Err(SystemError::Other(anyhow::anyhow!(format!(
                "System '{}' is not a thread engine system",
                name
            )))),
        }
    }
}

#[async_trait]
impl ActorSystem for ParrotActorSystem {
    async fn start(config: ActorSystemConfig) -> Result<Self, SystemError> {
        Self::new(config).await
    }

    fn spawn_root_typed<'life0, 'async_trait, A>(
        &'life0 self,
        actor: A,
        config: A::Config,
    ) -> parrot_api::types::BoxedFuture<'async_trait, Result<Box<dyn ActorRef>, SystemError>>
    where
        A: Actor + 'static + 'async_trait,
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        self.internal_spawn_actor(actor, config)
    }

    fn spawn_root_boxed<'life0, 'async_trait>(
        &'life0 self,
        actor: Box<dyn Actor<Config = Box<dyn std::any::Any + Send>, Context = dyn ActorContext>>,
        config: Box<dyn std::any::Any + Send>,
    ) -> parrot_api::types::BoxedFuture<'async_trait, Result<Box<dyn ActorRef>, SystemError>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let actor = self.own(actor);
        let config = self.own(config);
        Box::pin(async move {
            parrot_api::engine::AsyncEngineRuntime::spawn(
                self,
                actor.take(),
                config.take(),
                Default::default(),
            )
            .await
        })
    }

    async fn get_actor(&self, path: &ActorPath) -> Option<Box<dyn ActorRef>> {
        // Clone the path to avoid referencing the original variable in the new async block
        let path_copy = ActorPath {
            path: path.path.clone(),
            target: path.target.clone(),
        };

        async move { self.get_actor_mirrored(&path_copy).await }.await
    }

    /// Sends a message to all actors in the system.
    ///
    /// This method provides a way to notify all actors of
    /// system-wide events or state changes.
    ///
    /// # Type Parameters
    /// * `M` - Message type implementing Message trait
    ///
    /// # Parameters
    /// * `msg` - Message to broadcast
    ///
    /// # Returns
    /// Success or failure of the broadcast operation
    async fn broadcast<M: Message + Clone + 'static>(&self, msg: M) -> Result<(), SystemError> {
        // Clone the message to avoid capturing the original in the new async block
        let msg_copy = msg.clone();

        async move { self.internal_broadcast(msg_copy).await }.await
    }

    /// Snapshot unique registered engines, sampling each outside the facade
    /// lock. Aliases and Actix clones share registry identity. This is not an
    /// atomic cross-engine snapshot. Uptime belongs to the facade itself.
    /// Empty registries are ready for registration (Running); otherwise state
    /// preference is Running, Starting, ShuttingDown, then all Stopped.
    fn status(&self) -> SystemStatus {
        let systems: Vec<_> = self.systems.read().unwrap().values().cloned().collect();
        let mut seen = HashSet::new();
        let mut state_rank = 0;
        let mut result = SystemStatus {
            state: SystemState::Running,
            active_actors: 0,
            uptime: self.started_at.elapsed(),
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: 0,
            },
        };
        for system in &systems {
            let identity = system.identity();
            if !seen.insert(identity) {
                continue;
            }
            let status = match system {
                ActorSystemImpl::Actix(s) => s.status(),
                ActorSystemImpl::Thread(s) => s.status(),
                ActorSystemImpl::Runtime(s) => s.status(),
            };
            let rank = match status.state {
                SystemState::Stopped => 0,
                SystemState::ShuttingDown => 1,
                SystemState::Starting => 2,
                SystemState::Running => 3,
            };
            state_rank = state_rank.max(rank);
            result.active_actors = result.active_actors.saturating_add(status.active_actors);
            result.resources.thread_count = result
                .resources
                .thread_count
                .saturating_add(status.resources.thread_count);
            result.resources.memory_usage = result
                .resources
                .memory_usage
                .saturating_add(status.resources.memory_usage);
            result.resources.cpu_usage += status.resources.cpu_usage;
        }
        if !seen.is_empty() {
            result.state = match state_rank {
                3 => SystemState::Running,
                2 => SystemState::Starting,
                1 => SystemState::ShuttingDown,
                _ => SystemState::Stopped,
            };
        }
        if self.lifecycle.started() {
            result.state = if self.lifecycle.finished() && state_rank == 0 {
                SystemState::Stopped
            } else {
                SystemState::ShuttingDown
            };
        }
        result
    }

    async fn shutdown(self) -> Result<(), SystemError> {
        self.internal_shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::context::ThreadContext;
    use parrot_api::actor::{Actor, ActorState, EmptyConfig};
    use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

    /// Simple echo actor for the thread engine.
    #[derive(Debug, Default)]
    struct EchoActor {
        received: std::sync::Mutex<Vec<u64>>,
    }

    impl Actor for EchoActor {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(
            &'a mut self,
            msg: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if let Some(v) = msg.downcast_ref::<u64>() {
                    self.received.lock().unwrap().push(*v);
                }
                Ok(msg)
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    #[tokio::test]
    async fn test_parrot_system_with_no_registrations() {
        let system = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();

        assert!(system.list_registered_systems().unwrap().is_empty());

        // No default system: spawn and broadcast must fail cleanly.
        let spawn = system
            .spawn_root_thread(EchoActor::default(), EmptyConfig)
            .await;
        assert!(spawn.is_err());

        system.internal_shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_register_thread_system_and_spawn() {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();

        let thread_system = crate::thread::system::ThreadActorSystem::shared(
            crate::thread::config::ThreadActorSystemConfig::default(),
        );

        parrot
            .register_thread_system("thread-main".into(), thread_system, true)
            .await
            .unwrap();

        // Registered and set as default (first registration wins by default).
        let names = parrot.list_registered_systems().unwrap();
        assert_eq!(names, vec!["thread-main".to_string()]);

        // Spawn a thread-engine actor through the Parrot facade.
        let actor_ref = parrot
            .spawn_root_thread(EchoActor::default(), EmptyConfig)
            .await
            .expect("spawn through ParrotActorSystem");
        assert!(actor_ref.path().starts_with("/user/"));

        // The actor lives in the thread system registry.
        let ts = parrot.get_thread_system("thread-main").unwrap();
        assert_eq!(ts.actor_count(), 1);

        // get_thread_system rejects non-thread systems.
        assert!(parrot.get_thread_system("missing").is_err());

        parrot.internal_shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_facade_forwards_boxed_spawn_to_default_thread_system() {
        struct ErasedEcho;
        impl Actor for ErasedEcho {
            type Config = Box<dyn std::any::Any + Send>;
            type Context = dyn ActorContext;

            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move { Ok(msg) })
            }

            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }

        let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();
        parrot
            .register_thread_system(
                "boxed-default".into(),
                crate::thread::system::ThreadActorSystem::shared(
                    crate::thread::config::ThreadActorSystemConfig::default(),
                ),
                true,
            )
            .await
            .unwrap();

        let actor_ref = <ParrotActorSystem as ActorSystem>::spawn_root_boxed(
            &parrot,
            Box::new(ErasedEcho),
            Box::new("facade-config") as Box<dyn std::any::Any + Send>,
        )
        .await
        .expect("facade should forward boxed spawn");
        let response = actor_ref
            .send(Box::new(9_u64) as BoxedMessage)
            .await
            .expect("facade-spawned actor should answer");
        assert_eq!(response.downcast::<u64>().unwrap().as_ref(), &9);

        parrot.internal_shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_named_system_spawn_and_default_selection() {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();

        let ts = crate::thread::system::ThreadActorSystem::shared(
            crate::thread::config::ThreadActorSystemConfig::default(),
        );
        parrot
            .register_thread_system("engine-a".into(), ts, false)
            .await
            .unwrap();

        // First registration becomes the default even when set_as_default=false.
        let spawned = parrot
            .spawn_root_thread(EchoActor::default(), EmptyConfig)
            .await;
        assert!(spawned.is_ok());

        // Explicit named spawn works too.
        let named = parrot
            .spawn_root_thread_in_system("engine-a", EchoActor::default(), EmptyConfig)
            .await;
        assert!(named.is_ok());

        // Unknown system name fails.
        let missing = parrot
            .spawn_root_thread_in_system("nope", EchoActor::default(), EmptyConfig)
            .await;
        assert!(missing.is_err());

        // set_default_system validates the name.
        assert!(parrot.set_default_system("engine-a").is_ok());
        assert!(parrot.set_default_system("missing").is_err());

        parrot.internal_shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_get_actor_through_facade() {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();

        let ts = crate::thread::system::ThreadActorSystem::shared(
            crate::thread::config::ThreadActorSystemConfig::default(),
        );
        parrot
            .register_thread_system("lookup".into(), ts, true)
            .await
            .unwrap();

        let actor_ref = parrot
            .spawn_root_thread(EchoActor::default(), EmptyConfig)
            .await
            .unwrap();
        let path_str = actor_ref.path();

        // Look up by path via the ActorSystem trait method.
        let found = parrot.get_actor(&ActorPath::placeholder(&path_str)).await;
        assert!(found.is_some(), "spawned actor must be discoverable");

        let missing = parrot
            .get_actor(&ActorPath::placeholder("/user/nonexistent"))
            .await;
        assert!(missing.is_none());

        parrot.internal_shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_status_reports_running() {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();
        let status = parrot.status();
        assert_eq!(status.state, SystemState::Running);
        parrot.internal_shutdown().await.unwrap();
    }
}
