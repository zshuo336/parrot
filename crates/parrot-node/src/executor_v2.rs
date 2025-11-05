//! Node-owned Admin V2 deployments. Accepted mutations remain owned on the
//! engine runtime when an RPC waiter disappears. Status/Stop address recorded
//! component identities, never neighboring or replacement actors by prefix.
use futures::FutureExt;
use parrot::thread::system::ThreadActorSystem;
#[cfg(test)]
use parrot_api::address::ActorRef;
use parrot_api::errors::ActorError;
use parrot_app::{assemble::ParrotSpawner, host::ThreadPropsSpawner};
use parrot_remote::admin_v2::{
    failed_v2, v2_err, AdminArtifactRef, AdminCommandV2, AdminInstancePolicy, AdminReplyV2 as R,
    ComponentDeploy, ComponentExecutor, ComponentStateReport, MetricsSnapshot,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::Duration,
};

mod artifact;
mod reference;
pub use artifact::ArtifactChannel;
use reference::{drop_catching, ReferenceOwner};

/// Admission bounds apply before path expansion or configuration parsing.
pub const MAX_COMPONENT_INSTANCES: usize = 1024;
pub const MAX_OWNED_INSTANCES: usize = 4096;
pub const MAX_COMPONENTS: usize = 1024;
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;
pub const MAX_PENDING_OPERATIONS: usize = 64;
type Failure = (u16, String);
static NODE_OWNER: OnceLock<Arc<Owner>> = OnceLock::new();

/// Expand validated component names/policies. Callers handling untrusted
/// requests must validate count before calling this allocation helper.
pub fn instance_paths(name: &str, policy: AdminInstancePolicy) -> Vec<String> {
    match policy {
        AdminInstancePolicy::Singleton => vec![format!("/user/{name}")],
        AdminInstancePolicy::Pool { count } | AdminInstancePolicy::Sharded { count } => (0..count)
            .map(|index| format!("/user/{name}-{index}"))
            .collect(),
    }
}

#[derive(Clone)]
struct Instance {
    path: String,
    actor: Arc<ReferenceOwner>,
    cleanup_error: Option<String>,
}
impl Instance {
    fn release(self) -> Result<(), String> {
        if ReferenceOwner::release(self.actor) {
            Ok(())
        } else {
            Err(format!("release {} reference panicked", self.path))
        }
    }
}
struct Deployment {
    command: ComponentDeploy,
    // One reservation through commit/failed cleanup; a redeploy gets a new token.
    generation: Arc<()>,
    committed: bool,
    instances: Vec<Instance>,
}
type Selection = BTreeMap<String, Arc<()>>;
struct Owner {
    records: Mutex<BTreeMap<String, Deployment>>,
    mutations: Mutex<BTreeMap<String, Weak<tokio::sync::Mutex<()>>>>,
    admission: Arc<tokio::sync::Semaphore>,
    spawner: ThreadPropsSpawner,
    policy: HostPolicy,
}
#[derive(Clone, PartialEq, Eq)]
struct HostPolicy {
    epoch_ticks: u32,
    #[cfg(feature = "dylib")]
    native: crate::NativeTrust,
}
impl Default for HostPolicy {
    fn default() -> Self {
        Self {
            epoch_ticks: 1,
            #[cfg(feature = "dylib")]
            native: Default::default(),
        }
    }
}
impl Owner {
    fn new(engine: Arc<ThreadActorSystem>) -> Self {
        Self::with_spawner(ThreadPropsSpawner::new(engine), HostPolicy::default())
    }
    fn with_spawner(spawner: ThreadPropsSpawner, policy: HostPolicy) -> Self {
        Self {
            records: Mutex::default(),
            mutations: Mutex::default(),
            admission: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_OPERATIONS)),
            spawner,
            policy,
        }
    }
}

/// One management owner per host. All constructors targeting the process NODE
/// share its registry, including convenience functions. Independent engines
/// should retain and clone one executor. Legacy Props factories require NODE;
/// contextual app factories, Wasm and explicitly trusted native artifacts use
/// the supplied engine. Retain/clone one executor to share ownership and quotas.
#[derive(Clone)]
pub struct NodeComponentExecutor {
    ts: Arc<ThreadActorSystem>,
    owner: Arc<Owner>,
}
impl NodeComponentExecutor {
    pub fn new(ts: Arc<ThreadActorSystem>) -> Self {
        let owner = if is_node_engine(&ts) {
            NODE_OWNER
                .get_or_init(|| Arc::new(Owner::new(ts.clone())))
                .clone()
        } else {
            Arc::new(Owner::new(ts.clone()))
        };
        Self { ts, owner }
    }

    /// Install explicit process limits before any deployment owner is used.
    /// An existing owner must never be silently replaced or reconfigured.
    pub(crate) fn for_running_node(
        ts: Arc<ThreadActorSystem>,
        epoch_ticks: Option<u32>,
        #[cfg(feature = "dylib")] native: &crate::NativeTrust,
    ) -> Result<Self, String> {
        if !is_node_engine(&ts) {
            return Err("invalid Node component engine".into());
        }
        Self::configured(
            ts,
            epoch_ticks,
            #[cfg(feature = "dylib")]
            native,
        )
    }

    /// Configure an independent executor using previously granted native trust.
    /// NODE constructors share its existing owner only if policy matches; an
    /// initialized owner can never be silently reconfigured or lose its records.
    /// This constructor requests the default Wasm budget (one epoch tick).
    /// To reuse a running NODE with a custom budget, use `new` instead.
    #[cfg(feature = "dylib")]
    pub fn with_native_trust(
        ts: Arc<ThreadActorSystem>,
        native: &crate::NativeTrust,
    ) -> Result<Self, String> {
        Self::configured(ts, None, native)
    }

    fn configured(
        ts: Arc<ThreadActorSystem>,
        epoch_ticks: Option<u32>,
        #[cfg(feature = "dylib")] native: &crate::NativeTrust,
    ) -> Result<Self, String> {
        if epoch_ticks == Some(0) {
            return Err("invalid Node Wasm budget".into());
        }
        #[cfg(not(feature = "wasm"))]
        if epoch_ticks.is_some() {
            return Err("Node Wasm budget requires the wasm feature".into());
        }
        let policy = HostPolicy {
            epoch_ticks: epoch_ticks.unwrap_or(1),
            #[cfg(feature = "dylib")]
            native: native.clone(),
        };
        let spawner = ThreadPropsSpawner::new(ts.clone());
        #[cfg(feature = "wasm")]
        let spawner = if let Some(epoch_ticks) = epoch_ticks {
            spawner.with_wasm_limits(
                parrot_wasm::WasmConfig {
                    epoch_deadline: epoch_ticks.into(),
                    ..Default::default()
                },
                Default::default(),
            )
        } else {
            spawner
        };
        #[cfg(feature = "dylib")]
        let spawner = native.configure(spawner)?;
        let candidate = Arc::new(Owner::with_spawner(spawner, policy.clone()));
        let owner = if is_node_engine(&ts) {
            NODE_OWNER.get_or_init(|| candidate).clone()
        } else {
            candidate
        };
        if owner.policy != policy {
            return Err(
                "Node component owner was already initialized with a different host policy".into(),
            );
        }
        Ok(Self { ts, owner })
    }

    async fn owned(
        &self,
        req_id: u64,
        work: impl std::future::Future<Output = R> + Send + 'static,
    ) -> R {
        let permit = match self.owner.admission.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return failed_v2(
                    req_id,
                    v2_err::SPAWN_FAILED,
                    "node management operation limit reached",
                )
            }
        };
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.ts.runtime_handle().spawn(async move {
            let _permit = permit;
            let reply = std::panic::AssertUnwindSafe(work)
                .catch_unwind()
                .await
                .unwrap_or_else(|payload| {
                    drop_catching(payload);
                    failed_v2(
                        req_id,
                        v2_err::SPAWN_FAILED,
                        "node management operation panicked",
                    )
                });
            let _ = sender.send(reply);
        });
        receiver.await.unwrap_or_else(|_| {
            failed_v2(
                req_id,
                v2_err::SPAWN_FAILED,
                "node management owner runtime stopped",
            )
        })
    }

    fn operation_locks(&self, mut names: Vec<String>) -> Vec<Arc<tokio::sync::Mutex<()>>> {
        names.sort();
        names.dedup();
        let mut locks = self.owner.mutations.lock().unwrap();
        locks.retain(|_, lock| lock.strong_count() != 0);
        names
            .into_iter()
            .map(|name| {
                if let Some(lock) = locks.get(&name).and_then(Weak::upgrade) {
                    return lock;
                }
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(name, Arc::downgrade(&lock));
                lock
            })
            .collect()
    }
    async fn lock_selection(
        &self,
        prefix: &str,
    ) -> (Vec<tokio::sync::OwnedMutexGuard<()>>, Selection) {
        let selection: Selection = {
            let records = self.owner.records.lock().unwrap();
            let all = matches!(prefix, "/" | "/user" | "/user/");
            let exact = prefix
                .strip_prefix("/user/")
                .filter(|name| records.contains_key(*name));
            records
                .iter()
                .filter(|(name, entry)| {
                    all || exact.map_or_else(
                        || {
                            entry.instances.iter().any(|i| i.path == prefix)
                                || (!entry.committed
                                    && instance_paths(name, entry.command.instances)
                                        .iter()
                                        .any(|path| path == prefix))
                        },
                        |selected| selected == name.as_str(),
                    )
                })
                .map(|(name, entry)| (name.clone(), entry.generation.clone()))
                .collect()
        };
        let locks = self.operation_locks(selection.keys().cloned().collect());
        let mut guards = Vec::new();
        for lock in locks {
            guards.push(lock.lock_owned().await);
        }
        (guards, selection)
    }

    fn selected(&self, prefix: &str, selection: &Selection) -> Vec<(String, Instance)> {
        let records = self.owner.records.lock().unwrap();
        let all = matches!(prefix, "/" | "/user" | "/user/");
        let exact_component = prefix
            .strip_prefix("/user/")
            .filter(|name| records.contains_key(*name));
        records
            .iter()
            .filter(|(name, entry)| {
                selection
                    .get(*name)
                    .is_some_and(|generation| Arc::ptr_eq(generation, &entry.generation))
            })
            .flat_map(|(name, entry)| {
                entry
                    .instances
                    .iter()
                    .filter(move |instance| {
                        all || exact_component
                            .map_or(instance.path == prefix, |selected| selected == name)
                    })
                    .map(move |instance| (name.clone(), instance.clone()))
            })
            .collect()
    }

    fn retire_record(&self, component: &str, instance: &Instance, error: Option<String>) {
        let mut records = self.owner.records.lock().unwrap();
        if let Some(entry) = records.get_mut(component) {
            if let Some(error) = error {
                if let Some(owned) = entry
                    .instances
                    .iter_mut()
                    .find(|i| Arc::ptr_eq(&i.actor, &instance.actor))
                {
                    owned.cleanup_error = Some(error);
                }
            } else {
                entry
                    .instances
                    .retain(|i| !Arc::ptr_eq(&i.actor, &instance.actor));
            }
            if entry.instances.is_empty() {
                records.remove(component);
            }
        }
    }

    async fn deploy_owned(&self, req_id: u64, command: ComponentDeploy) -> R {
        let _mutation = self
            .operation_locks(vec![command.name.clone()])
            .remove(0)
            .lock_owned()
            .await;
        let bytes = command.config.clone();
        let config = match self
            .ts
            .runtime_handle()
            .spawn_blocking(move || parse_config(bytes.as_deref()))
            .await
        {
            Ok(Ok(config)) => config,
            Ok(Err((code, detail))) => return failed_v2(req_id, code, detail),
            Err(error) => {
                return failed_v2(
                    req_id,
                    v2_err::SPAWN_FAILED,
                    format!("config worker: {error}"),
                )
            }
        };
        let existing = {
            let records = self.owner.records.lock().unwrap();
            records.get(&command.name).map(|entry| {
                (
                    entry.committed && entry.command == command,
                    entry.instances.clone(),
                )
            })
        };
        if let Some((same, instances)) = existing {
            let mut live = same && instances.len() == command.instances.instance_count();
            for instance in &instances {
                live &= instance.cleanup_error.is_none() && instance_alive(instance).await;
            }
            return if live {
                R::Deployed {
                    req_id,
                    instances: instances.into_iter().map(|i| i.path).collect(),
                }
            } else {
                failed_v2(req_id, v2_err::SPAWN_FAILED, "component already owned with a different specification or unavailable instances; stop it before redeploying")
            };
        }
        let paths = instance_paths(&command.name, command.instances);
        let generation = Arc::new(());
        {
            let mut records = self.owner.records.lock().unwrap();
            let root = format!("/user/{}", command.name);
            let reserved: BTreeSet<_> = paths.iter().chain(std::iter::once(&root)).collect();
            if records.iter().any(|(name, entry)| {
                let other_root = format!("/user/{name}");
                let other_paths = instance_paths(name, entry.command.instances);
                other_paths
                    .iter()
                    .chain(std::iter::once(&other_root))
                    .any(|path| reserved.contains(path))
            }) {
                return failed_v2(
                    req_id,
                    v2_err::SPAWN_FAILED,
                    "component root or instance path conflicts with an owned deployment",
                );
            }
            if records.len() >= MAX_COMPONENTS
                || records
                    .values()
                    .map(|entry| entry.command.instances.instance_count())
                    .sum::<usize>()
                    + command.instances.instance_count()
                    > MAX_OWNED_INSTANCES
            {
                return failed_v2(
                    req_id,
                    v2_err::SPAWN_FAILED,
                    "node component/instance ownership limit reached",
                );
            }
            records.insert(
                command.name.clone(),
                Deployment {
                    command: command.clone(),
                    generation: generation.clone(),
                    committed: false,
                    instances: vec![],
                },
            );
        }
        if paths
            .iter()
            .any(|path| self.ts.get_actor_ref(path).is_some())
        {
            self.owner.records.lock().unwrap().remove(&command.name);
            return failed_v2(
                req_id,
                v2_err::SPAWN_FAILED,
                "component path already belongs to another actor",
            );
        }
        match self.spawn(&command, &paths, config.as_ref()).await {
            Ok(instances) => {
                self.owner.records.lock().unwrap().insert(
                    command.name.clone(),
                    Deployment {
                        command,
                        generation,
                        committed: true,
                        instances,
                    },
                );
                R::Deployed {
                    req_id,
                    instances: paths,
                }
            }
            Err((failure, retained)) => {
                let mut records = self.owner.records.lock().unwrap();
                records.remove(&command.name);
                if !retained.is_empty() {
                    records.insert(
                        command.name.clone(),
                        Deployment {
                            command,
                            generation,
                            committed: false,
                            instances: retained,
                        },
                    );
                }
                failed_v2(req_id, failure.0, failure.1)
            }
        }
    }

    async fn spawn(
        &self,
        command: &ComponentDeploy,
        paths: &[String],
        config: Option<&toml::Table>,
    ) -> Result<Vec<Instance>, (Failure, Vec<Instance>)> {
        let contextual = match &command.artifact {
            AdminArtifactRef::Props { factory } => {
                parrot_api::message::inventory::iter::<parrot_app::host::ThreadPropsFactory>()
                    .any(|entry| entry.name == factory)
            }
            _ => true,
        };
        if contextual {
            let artifact = match &command.artifact {
                AdminArtifactRef::Props { factory } => parrot_app::manifest::ArtifactRef::Props {
                    factory: factory.clone(),
                },
                #[cfg(feature = "wasm")]
                AdminArtifactRef::Wasm { digest, uri } => parrot_app::manifest::ArtifactRef::Wasm {
                    digest: digest.clone(),
                    uri: uri.clone(),
                },
                #[cfg(feature = "dylib")]
                AdminArtifactRef::Dylib { digest, uri, abi } => {
                    parrot_app::manifest::ArtifactRef::Dylib {
                        digest: digest.clone(),
                        uri: uri.clone(),
                        abi: *abi,
                    }
                }
                _ => {
                    return Err((
                        (
                            v2_err::DIALECT_MISMATCH,
                            "artifact runtime not enabled on this node".into(),
                        ),
                        vec![],
                    ))
                }
            };
            return self
                .owner
                .spawner
                .spawn_artifact(&artifact, paths, config)
                .await
                .map(|actors| {
                    paths
                        .iter()
                        .cloned()
                        .zip(actors)
                        .map(|(path, actor)| Instance {
                            path: path.clone(),
                            actor: ReferenceOwner::new(path, Arc::from(actor)),
                            cleanup_error: None,
                        })
                        .collect()
                })
                .map_err(|error| {
                    let retained = error
                        .retained_actors()
                        .iter()
                        .map(|actor| Instance {
                            path: actor.path.clone(),
                            actor: ReferenceOwner::new(actor.path.clone(), actor.actor.clone()),
                            cleanup_error: Some(actor.error.clone()),
                        })
                        .collect();
                    (map_app_error(error), retained)
                });
        }
        let AdminArtifactRef::Props { factory } = &command.artifact else {
            unreachable!()
        };
        if !is_node_engine(&self.ts) {
            return Err((
                (
                    v2_err::SPAWN_FAILED,
                    "engine mismatch: legacy PropsFactory spawns on NODE singleton".into(),
                ),
                vec![],
            ));
        }
        if config.is_some_and(|table| !table.is_empty()) {
            return Err(((v2_err::SPAWN_FAILED, "legacy PropsFactory cannot consume component config; register a ThreadPropsFactory".into()), vec![]));
        }
        let Some(factory) = parrot_remote::admin::find_factory(factory) else {
            return Err((
                (
                    v2_err::FACTORY_NOT_FOUND,
                    format!("PropsFactory '{factory}' not registered"),
                ),
                vec![],
            ));
        };
        let mut instances = Vec::new();
        for path in paths {
            let result = std::panic::AssertUnwindSafe(async { (factory.spawn)(path).await })
                .catch_unwind()
                .await
                .unwrap_or_else(|payload| {
                    drop_catching(payload);
                    Err(ActorError::Panic(format!(
                        "Props factory {} panicked",
                        factory.name
                    )))
                });
            match result {
                Ok(actor) => instances.push(Instance {
                    path: path.clone(),
                    actor: ReferenceOwner::new(path.clone(), Arc::from(actor)),
                    cleanup_error: None,
                }),
                Err(error) => {
                    let (retained, cleanup) = rollback(instances).await;
                    return Err((
                        (
                            v2_err::SPAWN_FAILED,
                            format!("spawn {path}: {error}{cleanup}"),
                        ),
                        retained,
                    ));
                }
            }
        }
        Ok(instances)
    }

    async fn stop_owned(&self, req_id: u64, prefix: String) -> R {
        let (_mutation, selection) = self.lock_selection(&prefix).await;
        let selected = self.selected(&prefix, &selection);
        if selected.is_empty() {
            return not_found(req_id, &prefix);
        }
        let mut errors = Vec::new();
        for (component, instance) in selected.into_iter().rev() {
            let error = stop_instance(&instance).await.err();
            self.retire_record(&component, &instance, error.clone());
            if let Some(error) = error {
                errors.push(format!("{}: {error}", instance.path));
            }
            if let Err(error) = instance.release() {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            R::Stopped { req_id }
        } else {
            failed_v2(req_id, v2_err::SPAWN_FAILED, errors.join("; "))
        }
    }

    async fn drain_owned(&self, req_id: u64, prefix: String, deadline: tokio::time::Instant) -> R {
        let Ok((_mutation, selection)) =
            tokio::time::timeout_at(deadline, self.lock_selection(&prefix)).await
        else {
            return failed_v2(
                req_id,
                v2_err::DRAIN_TIMEOUT,
                "drain deadline expired while another management operation was running",
            );
        };
        let selected = self.selected(&prefix, &selection);
        if selected.is_empty() {
            return not_found(req_id, &prefix);
        }
        let (mut drained, mut aborted) = (0, 0);
        let mut disposal_errors = Vec::new();
        for (component, instance) in selected.into_iter().rev() {
            // A real actor control message acknowledges preceding queued work
            // AND its currently executing handler. Queue length cannot do this.
            let outcome = if tokio::time::Instant::now() >= deadline {
                None
            } else {
                match std::panic::AssertUnwindSafe(async {
                    let operation = async {
                        instance
                            .actor
                            .send(Box::new(
                                parrot::thread::envelope::ControlMessage::HealthCheck,
                            ))
                            .await
                            .map_err(|e| e.to_string())?;
                        stop_instance(&instance).await
                    };
                    tokio::time::timeout_at(deadline, operation).await.ok()
                })
                .catch_unwind()
                .await
                {
                    Ok(outcome) => outcome,
                    Err(payload) => {
                        drop_catching(payload);
                        Some(Err("actor drain callback panicked".into()))
                    }
                }
            };
            match outcome {
                Some(Ok(())) => {
                    drained += 1;
                    self.retire_record(&component, &instance, None);
                }
                Some(Err(error)) => {
                    aborted += 1;
                    self.retire_record(&component, &instance, Some(error));
                }
                None => {
                    aborted += 1;
                }
            }
            if let Err(error) = instance.release() {
                disposal_errors.push(error);
            }
        }
        if !disposal_errors.is_empty() {
            return failed_v2(
                req_id,
                v2_err::SPAWN_FAILED,
                format!(
                    "drained={drained} aborted={aborted}; {}",
                    disposal_errors.join("; ")
                ),
            );
        }
        R::Drained {
            req_id,
            drained,
            aborted,
        }
    }

    async fn states(&self, prefix: &str) -> Vec<ComponentStateReport> {
        // Snapshot identity and version under one lock; a later redeploy must
        // not label an old instance with the replacement's version.
        let rows = {
            let records = self.owner.records.lock().unwrap();
            let all = matches!(prefix, "/" | "/user" | "/user/");
            let exact = prefix
                .strip_prefix("/user/")
                .filter(|name| records.contains_key(*name));
            let mut rows = Vec::new();
            for (name, entry) in records.iter() {
                let selected =
                    |path: &str| all || exact.map_or(path == prefix, |component| component == name);
                if !entry.committed && entry.instances.is_empty() {
                    for path in instance_paths(name, entry.command.instances) {
                        if selected(&path) {
                            rows.push((path, entry.command.version.clone(), None));
                        }
                    }
                } else {
                    for instance in &entry.instances {
                        if selected(&instance.path) {
                            rows.push((
                                instance.path.clone(),
                                if entry.committed {
                                    entry.command.version.clone()
                                } else {
                                    String::new()
                                },
                                Some(instance.clone()),
                            ));
                        }
                    }
                }
            }
            rows
        };
        let mut result = Vec::new();
        for (path, version, instance) in rows {
            let state = match instance {
                None => "deploying",
                Some(instance) if instance.cleanup_error.is_some() => "cleanup_failed",
                Some(instance) if instance_alive(&instance).await => "running",
                Some(_) => "unavailable",
            };
            result.push(ComponentStateReport {
                path,
                version,
                state: state.into(),
            });
        }
        result
    }
}

fn is_node_engine(ts: &Arc<ThreadActorSystem>) -> bool {
    crate::NODE
        .get()
        .is_some_and(|node| Arc::ptr_eq(&node.ts, ts))
}
fn not_found(req_id: u64, prefix: &str) -> R {
    failed_v2(
        req_id,
        v2_err::COMPONENT_NOT_FOUND,
        format!("no owned instances selected by {prefix}"),
    )
}
fn map_app_error(error: parrot_app::DeployError) -> Failure {
    let code = match error.primary_error() {
        parrot_app::DeployError::ArtifactDigest(_) => v2_err::ARTIFACT_DIGEST,
        parrot_app::DeployError::Artifact(_) => v2_err::ARTIFACT_FETCH,
        _ => v2_err::SPAWN_FAILED,
    };
    (code, error.to_string())
}
fn validate(command: &ComponentDeploy) -> Result<(), Failure> {
    let bad = |detail: &str| (v2_err::SPAWN_FAILED, detail.into());
    let name = &command.name;
    if name.is_empty()
        || name.len() > 1024
        || matches!(name.as_str(), "." | "..")
        || !name
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(bad("invalid component path segment"));
    }
    if !matches!(
        command.artifact,
        AdminArtifactRef::Props { .. }
            | AdminArtifactRef::Wasm { .. }
            | AdminArtifactRef::Dylib { .. }
    ) {
        return Err((
            v2_err::DIALECT_MISMATCH,
            "unsupported artifact dialect".into(),
        ));
    }
    if command.version.len() > 256 || !parrot_app::manifest::is_valid_semver(&command.version) {
        return Err(bad("invalid component semantic version"));
    }
    if !(1..=MAX_COMPONENT_INSTANCES).contains(&command.instances.instance_count()) {
        return Err(bad("instance count must be in 1..=1024"));
    }
    if command
        .config
        .as_ref()
        .is_some_and(|bytes| bytes.len() > MAX_CONFIG_BYTES)
    {
        return Err(bad("component config exceeds byte limit"));
    }
    match &command.artifact {
        AdminArtifactRef::Props { factory } if factory.is_empty() || factory.len() > 1024 => {
            return Err(bad("invalid Props factory name length"))
        }
        AdminArtifactRef::Wasm { digest, uri } | AdminArtifactRef::Dylib { digest, uri, .. }
            if digest.len() > 71 || uri.len() > 8192 =>
        {
            return Err(bad("artifact metadata exceeds byte limit"))
        }
        _ => {}
    }
    Ok(())
}
fn parse_config(bytes: Option<&[u8]>) -> Result<Option<toml::Table>, Failure> {
    bytes
        .map(|bytes| {
            let text = std::str::from_utf8(bytes).map_err(|_| {
                (
                    v2_err::SPAWN_FAILED,
                    "component config must be UTF-8 TOML".into(),
                )
            })?;
            toml::from_str(text)
                .map_err(|e| (v2_err::SPAWN_FAILED, format!("component config: {e}")))
        })
        .transpose()
}

async fn instance_alive(instance: &Instance) -> bool {
    match std::panic::AssertUnwindSafe(async { instance.actor.is_alive().await })
        .catch_unwind()
        .await
    {
        Ok(alive) => alive,
        Err(payload) => {
            drop_catching(payload);
            tracing::warn!(path = %instance.path, "Node liveness callback panicked");
            false
        }
    }
}

async fn stop_instance(instance: &Instance) -> Result<(), String> {
    match std::panic::AssertUnwindSafe(async { instance.actor.stop().await })
        .catch_unwind()
        .await
    {
        Ok(Ok(())) | Ok(Err(ActorError::ActorNotFound(_))) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(payload) => {
            drop_catching(payload);
            Err("actor stop panicked".into())
        }
    }
}
async fn rollback(instances: Vec<Instance>) -> (Vec<Instance>, String) {
    let mut retained = Vec::new();
    let mut errors = String::new();
    for mut instance in instances.into_iter().rev() {
        if let Err(error) = stop_instance(&instance).await {
            errors.push_str(&format!("; rollback {}: {error}", instance.path));
            instance.cleanup_error = Some(error);
            retained.push(instance);
        } else if let Err(error) = instance.release() {
            errors.push_str(&format!("; rollback {error}"));
        }
    }
    retained.reverse();
    (retained, errors)
}

#[async_trait::async_trait]
impl ComponentExecutor for NodeComponentExecutor {
    async fn deploy(&self, req_id: u64, command: &ComponentDeploy) -> R {
        // Reject oversized requests before duplicating their storage into a task.
        if let Err((code, detail)) = validate(command) {
            return failed_v2(req_id, code, detail);
        }
        let owner = self.clone();
        let command = command.clone();
        self.owned(
            req_id,
            async move { owner.deploy_owned(req_id, command).await },
        )
        .await
    }
    async fn stop(&self, req_id: u64, prefix: &str) -> R {
        let owner = self.clone();
        let prefix = prefix.to_owned();
        self.owned(
            req_id,
            async move { owner.stop_owned(req_id, prefix).await },
        )
        .await
    }
    async fn drain(&self, req_id: u64, prefix: &str, timeout_ms: u64) -> R {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms.min(60_000));
        let owner = self.clone();
        let prefix = prefix.to_owned();
        self.owned(req_id, async move {
            owner.drain_owned(req_id, prefix, deadline).await
        })
        .await
    }
    async fn status(&self, req_id: u64, prefix: &str) -> R {
        let states = self.states(prefix).await;
        if states.is_empty() {
            not_found(req_id, prefix)
        } else {
            R::Status { req_id, states }
        }
    }
    async fn metrics(&self, req_id: u64) -> R {
        let states = self.states("/user/").await;
        let components = self.owner.records.lock().unwrap().len() as u64;
        R::Metrics {
            req_id,
            snapshot: MetricsSnapshot {
                ts_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or_default(),
                runtime: format!("parrot/{}", env!("CARGO_PKG_VERSION")),
                components,
                component_states: states,
                processes: self.ts.actor_count() as u64,
                ..Default::default()
            },
        }
    }
    async fn measured_metrics(&self) -> parrot_remote::metrics::MeasuredMetrics {
        use parrot_remote::metrics::{field, MeasuredMetrics};
        let R::Metrics { snapshot, .. } = self.metrics(0).await else {
            unreachable!()
        };
        MeasuredMetrics {
            snapshot,
            available: field::TS_MS
                | field::RUNTIME
                | field::COMPONENTS
                | field::COMPONENT_STATES
                | field::PROCESSES,
            epoch: String::new(),
            process_kind: "parrot_actor_entries".into(),
        }
    }
}

/// Convenience deploy for the process NODE. The process-owned executor retains
/// metadata and original instance references across subsequent helper calls.
pub async fn exec_deploy_v2(
    ts: &Arc<ThreadActorSystem>,
    command: ComponentDeploy,
) -> Result<Vec<String>, Failure> {
    if !is_node_engine(ts) {
        return Err((v2_err::SPAWN_FAILED, "engine mismatch: convenience deploy requires NODE; use a retained NodeComponentExecutor for an independent engine".into()));
    }
    match NodeComponentExecutor::new(ts.clone())
        .deploy(0, &command)
        .await
    {
        R::Deployed { instances, .. } => Ok(instances),
        R::Failed { code, detail, .. } => Err((code, detail)),
        _ => unreachable!(),
    }
}
/// Execute commands on the process NODE's persistent management owner.
pub async fn exec_command_v2(ts: &Arc<ThreadActorSystem>, command: AdminCommandV2) -> R {
    if !is_node_engine(ts) {
        return failed_v2(command.req_id(), v2_err::SPAWN_FAILED, "engine mismatch: convenience command requires NODE; retain a NodeComponentExecutor for an independent engine");
    }
    let executor = NodeComponentExecutor::new(ts.clone());
    match command {
        AdminCommandV2::DeployComponent { req_id, component } => {
            executor.deploy(req_id, &component).await
        }
        AdminCommandV2::DrainComponent {
            req_id,
            path_prefix,
            timeout_ms,
        } => executor.drain(req_id, &path_prefix, timeout_ms).await,
        AdminCommandV2::StopComponent {
            req_id,
            path_prefix,
        } => executor.stop(req_id, &path_prefix).await,
        AdminCommandV2::ComponentStatus {
            req_id,
            path_prefix,
        } => executor.status(req_id, &path_prefix).await,
        AdminCommandV2::MetricsReport { req_id } => executor.metrics(req_id).await,
        AdminCommandV2::MetricsReportV2 { req_id } => R::MeasuredMetrics {
            req_id,
            metrics: executor.measured_metrics().await,
        },
        AdminCommandV2::MirrorControl { req_id, .. } => failed_v2(
            req_id,
            v2_err::DIALECT_MISMATCH,
            "mirror control requires the owning RemoteActorSystem",
        ),
    }
}

#[cfg(test)]
mod selection_tests;
