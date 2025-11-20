//! Bounded, path-aware shard hosting for an explicitly configured Node cluster.
//!
//! Only absent/released ownership is claimed. Ordinary messages are never
//! replayed. Graceful handoff drains this router, retires the actual child,
//! then releases its exact generation. Crash takeover and arbitrary external
//! effects require a separate fencing policy; reachability is not retirement.
mod host;
pub(crate) mod owned;

use host::Host;
use owned::{call, resolve, Owned};
use parrot::system::{ParrotActorSystem, PrefixResolver};
use parrot::thread::system::ThreadActorSystem;
use parrot_api::{
    actor::{Actor, ActorState},
    address::{stop_for_owned_cleanup, ActorRef},
    context::ActorContext,
    engine::{AsyncEngineRuntime, RuntimeActor, SpawnOptions},
    errors::ActorError,
    trace,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_remote::{
    roles::directory::ownership::{
        OwnershipClusterClient, OwnershipCommand, OwnershipOutcome, OwnershipRecord,
        OwnershipStatus, OwnershipValue,
    },
    sharding::HashRing,
    NodeAddr, RemoteActorSystem,
};
use sha2::{Digest, Sha256};
use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};
use tokio::sync::{oneshot, Mutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};

fn error(text: impl Into<String>) -> ActorError {
    ActorError::MessageHandlingError(text.into())
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn segment(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text != "."
        && text != ".."
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// Operator-owned routing and resource limits. Members are exact authenticated
/// routes, independent of the Directory voting configuration and SWIM suspicion.
#[derive(Clone, Debug)]
pub struct ShardConfig {
    pub name: String,
    pub members: Vec<NodeAddr>,
    pub directory_group: String,
    pub directory_routes: Vec<NodeAddr>,
    pub max_entities: usize,
    /// Includes queued and executing local work and outgoing exchanges. Excess
    /// calls fail immediately; no unbounded waiting queue is created here.
    pub max_inflight: usize,
    /// Bounds authority requests and connection establishment, not actor work.
    pub authority_timeout: Duration,
}
impl ShardConfig {
    pub fn new(
        name: impl Into<String>,
        members: Vec<NodeAddr>,
        directory_group: impl Into<String>,
        directory_routes: Vec<NodeAddr>,
    ) -> Self {
        Self {
            name: name.into(),
            members,
            directory_group: directory_group.into(),
            directory_routes,
            max_entities: 1024,
            max_inflight: 256,
            authority_timeout: Duration::from_secs(5),
        }
    }
}

/// The factory receives the committed generation and latest externalized state.
/// Keep business replies successful for domain rejections. This capability
/// fences the Directory register only; it cannot fence arbitrary external I/O.
#[derive(Clone)]
pub struct ShardEntity {
    pub key: String,
    pub resource: String,
    pub generation: u64,
    pub value: Option<OwnershipValue>,
    authority: OwnershipClusterClient,
}
impl ShardEntity {
    /// A fresh committed state observation. Inspect its ownership before using
    /// the value: an operator may have fenced this generation since activation.
    pub async fn read_value(&self, timeout: Duration) -> ActorResult<OwnershipStatus> {
        self.authority
            .read_value(&self.resource, timeout)
            .await
            .map_err(|e| error(e.to_string()))
    }
    /// An explicit, immutable application write. Reuse an ID only with identical
    /// generation/revision/value bytes. Timeout does not undo a committed write.
    pub async fn write_value(
        &self,
        request_id: &str,
        expected_revision: Option<u64>,
        value: Vec<u8>,
        timeout: Duration,
    ) -> ActorResult<OwnershipStatus> {
        if request_id.is_empty()
            || request_id.len() > 128
            || request_id.chars().any(char::is_control)
        {
            return Err(error("invalid shard write identity"));
        }
        let mut digest = Sha256::new();
        digest.update((self.resource.len() as u64).to_be_bytes());
        digest.update(self.resource.as_bytes());
        digest.update(request_id.as_bytes());
        let request_id = format!("shard-value-v1:{}", hex::encode(digest.finalize()));
        self.authority
            .submit_and_wait(
                &request_id,
                &OwnershipCommand::WriteValue {
                    resource: self.resource.clone(),
                    generation: self.generation,
                    expected_revision,
                    value,
                },
                timeout,
            )
            .await
            .map_err(|e| error(e.to_string()))
    }
}
/// Called only after a committed claim and coherent state read. The returned
/// instance is a real runtime child; no detached actor task is created.
pub type ShardFactory = Arc<dyn Fn(ShardEntity) -> ActorResult<RuntimeActor> + Send + Sync>;

struct Placement {
    ring: HashRing,
    routes: BTreeMap<String, NodeAddr>,
}
impl Placement {
    fn new(members: Vec<NodeAddr>, scheme: &str) -> ActorResult<Self> {
        if members.len() > 64 {
            return Err(error("at most 64 shard members are supported"));
        }
        let mut routes = BTreeMap::new();
        for member in members {
            if !member.identity_is_explicit
                || member.scheme != scheme
                || parrot_config::endpoint::validate_node_id(&member.node_id).is_err()
                || routes.insert(member.node_id.clone(), member).is_some()
            {
                return Err(error("invalid or duplicate shard member"));
            }
        }
        let ring = HashRing::try_new(&routes.keys().cloned().collect::<Vec<_>>(), 256)
            .map_err(|e| error(e.to_string()))?;
        Ok(Self { ring, routes })
    }
    fn route(&self, key: &str) -> Option<NodeAddr> {
        self.routes.get(self.ring.node(key)?).cloned()
    }
}

struct Reply {
    sender: oneshot::Sender<Owned<ActorResult<BoxedMessage>>>,
    // Accepted control work retains serialization after its waiter is cancelled.
    _control: Option<OwnedMutexGuard<()>>,
}
impl Reply {
    fn complete(self, result: ActorResult<BoxedMessage>) {
        drop(Owned::new(self.sender.send(Owned::new(result))));
    }
}
struct Work {
    family: String,
    key: String,
    message: Owned<BoxedMessage>,
    reply: Option<Reply>,
    _activity: Option<host::Activity>,
    _permit: OwnedSemaphorePermit,
}
impl Work {
    fn complete(self, result: ActorResult<BoxedMessage>) {
        let result = self.message.finish(result);
        if let Some(reply) = self.reply {
            reply.complete(result);
        } else {
            if let Err(error) = &result {
                let _ = call(
                    || tracing::warn!(family = %self.family, key = %self.key, %error, "shard TELL processing failed"),
                );
            }
            drop(Owned::new(result));
        }
    }
}
enum Command {
    Work(Work),
    Passivate(String, Reply),
    Rebalance(Placement, Reply),
    Close(Reply),
}

struct Shared {
    remote: Arc<RemoteActorSystem>,
    placement: Arc<RwLock<Placement>>,
    parent: Owned<BoxedActorRef>,
    permits: Arc<Semaphore>,
    gate: Mutex<()>,
    control: Arc<Mutex<()>>,
    closed: AtomicBool,
    drained: Arc<AtomicBool>,
    retired: Mutex<bool>,
    name: String,
    timeout: Duration,
}
impl Shared {
    fn prefix(&self, direct: bool) -> String {
        format!(
            "/{}/{}/",
            if direct { "_shards" } else { "shards" },
            self.name
        )
    }
    async fn enqueue(&self, command: Command, closing: bool) -> ActorResult<()> {
        let mut command = Owned::new(command);
        let _gate = self.gate.lock().await;
        if closing {
            self.closed.store(true, Ordering::Release);
        } else if self.closed.load(Ordering::Acquire) {
            return command.finish(Err(ActorError::Stopped));
        }
        let delivery = call(|| self.parent.get().deliver(Box::new(command.take())))?;
        command.finish(resolve(delivery).await)
    }
    async fn control(
        &self,
        build: impl FnOnce(Reply) -> Command,
        closing: bool,
    ) -> ActorResult<()> {
        let control = self.control.clone().lock_owned().await;
        let (tx, rx) = oneshot::channel();
        self.enqueue(
            build(Reply {
                sender: tx,
                _control: Some(control),
            }),
            closing,
        )
        .await?;
        let mut reply = rx.await.map_err(|_| ActorError::Stopped)?;
        let result = Owned::new(reply.take()?);
        if !result.get().is::<()>() {
            return result.finish(Err(error("invalid shard control reply")));
        }
        result.finish(Ok(()))
    }
}

/// Owned by RunningNode even when the install/close waiter is cancelled. Prefix
/// references remain bound to this host and reject work after close. Keep the
/// runtime alive and retry close after an interrupted control operation.
#[derive(Clone)]
pub struct ShardRouter(Arc<Shared>);
impl ShardRouter {
    pub fn path(&self, key: &str) -> ActorResult<String> {
        if !segment(key, 256) {
            return Err(error("invalid shard entity key"));
        }
        Ok(format!("{}{key}", self.0.prefix(false)))
    }
    /// Ordered retirement of one entity, including accepted TELL processing.
    /// On uncertain authority/cleanup, retain the generation for a later retry.
    pub async fn passivate(&self, key: &str) -> ActorResult<()> {
        self.path(key)?;
        self.0
            .control(|tx| Command::Passivate(key.into(), tx), false)
            .await
    }
    /// Explicit graceful placement change. Install matching membership on every
    /// host; requests may fail during convergence and require application retry.
    /// Empty membership drains all local entities. No automatic crash takeover.
    pub async fn update_members(&self, members: Vec<NodeAddr>) -> ActorResult<()> {
        let replacement = Placement::new(members, self.0.remote.config.scheme)?;
        self.0
            .control(|tx| Command::Rebalance(replacement, tx), false)
            .await
    }
    /// Close admission, drain accepted local work, retire children and release
    /// generations before retiring the parent. Cancellation after enqueue does
    /// not cancel the actor's command. Repeat to join/retry an interrupted close.
    pub async fn close(&self) -> ActorResult<()> {
        let mut retired = self.0.retired.lock().await;
        if *retired {
            return Ok(());
        }
        if !self.0.drained.load(Ordering::Acquire) {
            self.0.control(Command::Close, true).await?;
        }
        stop_for_owned_cleanup(self.0.parent.get().as_ref()).await?;
        *retired = true;
        Ok(())
    }
    pub(crate) fn name(&self) -> &str {
        &self.0.name
    }
    pub(crate) fn reject(&self) {
        self.0.closed.store(true, Ordering::Release);
    }
    pub(crate) fn resolve_local(&self, path: &str) -> Option<BoxedActorRef> {
        for direct in [true, false] {
            if path.starts_with(&self.0.prefix(direct)) {
                return Resolver {
                    shared: self.0.clone(),
                    direct,
                }
                .resolve(path);
            }
        }
        None
    }
    pub(crate) async fn start(
        thread: &Arc<ThreadActorSystem>,
        remote: Arc<RemoteActorSystem>,
        config: ShardConfig,
        factory: owned::Owned<ShardFactory>,
    ) -> ActorResult<Self> {
        if !segment(&config.name, 64)
            || !(1..=4096).contains(&config.max_entities)
            || !(1..=4096).contains(&config.max_inflight)
            || config.authority_timeout.is_zero()
            || config.authority_timeout > Duration::from_secs(60)
        {
            return Err(error("invalid shard configuration"));
        }
        let placement = Placement::new(config.members, remote.config.scheme)?;
        let authority = OwnershipClusterClient::new(
            remote.clone(),
            config.directory_group,
            config.directory_routes,
        )
        .map_err(|e| error(e.to_string()))?;
        let shared_placement = Arc::new(RwLock::new(placement));
        let drained = Arc::new(AtomicBool::new(false));
        let parent = thread
            .spawn(
                Box::new(Host {
                    node: remote.config.node_id.clone(),
                    prefix: format!("/shards/{}/", config.name),
                    authority,
                    factory,
                    placement: shared_placement.clone(),
                    entities: HashMap::new(),
                    limit: config.max_entities,
                    timeout: config.authority_timeout,
                    drained: drained.clone(),
                    queue_capacity: config.max_inflight + 1,
                    last_control: None,
                    buckets: Vec::new(),
                    bucket_loads: Vec::new(),
                    authority_slots: Arc::new(Semaphore::new(16)),
                }),
                Box::new(()),
                SpawnOptions::at(format!("/_shard_hosts/{}", config.name)),
            )
            .await
            .map_err(|e| error(e.to_string()))?;
        // The parent and proxy must see the same placement, including updates.
        Ok(Self(Arc::new(Shared {
            remote,
            placement: shared_placement,
            parent: Owned::new(parent),
            permits: Arc::new(Semaphore::new(config.max_inflight)),
            gate: Mutex::new(()),
            control: Arc::new(Mutex::new(())),
            closed: AtomicBool::new(false),
            retired: Mutex::new(false),
            drained,
            name: config.name,
            timeout: config.authority_timeout,
        })))
    }
    pub(crate) fn publish(&self, facade: &ParrotActorSystem) -> ActorResult<()> {
        for direct in [true, false] {
            facade
                .register_prefix_resolver(
                    self.0.prefix(direct),
                    Arc::new(Resolver {
                        shared: self.0.clone(),
                        direct,
                    }),
                )
                .map_err(|e| error(e.to_string()))?;
        }
        Ok(())
    }
}

struct Resolver {
    shared: Arc<Shared>,
    direct: bool,
}
impl PrefixResolver for Resolver {
    fn resolve(&self, path: &str) -> Option<BoxedActorRef> {
        let key = path.strip_prefix(&self.shared.prefix(self.direct))?;
        segment(key, 256).then(|| {
            Box::new(RoutedRef {
                shared: self.shared.clone(),
                key: key.into(),
                path: path.into(),
                direct: self.direct,
            }) as BoxedActorRef
        })
    }
}
#[derive(Clone)]
struct RoutedRef {
    shared: Arc<Shared>,
    key: String,
    path: String,
    direct: bool,
}
impl std::fmt::Debug for RoutedRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ShardRef").field(&self.path).finish()
    }
}
impl RoutedRef {
    fn exchange(
        &self,
        message: BoxedMessage,
        ask: bool,
    ) -> BoxedFuture<'_, ActorResult<BoxedMessage>> {
        let mut message = Owned::new(message);
        Box::pin(trace::capture(async move {
            let result = async {
                if self.shared.closed.load(Ordering::Acquire) {
                    return Err(ActorError::Stopped);
                }
                let permit = self
                    .shared
                    .permits
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| error("shard admission capacity exhausted"))?;
                let target = self
                    .shared
                    .placement
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .route(&self.key)
                    .ok_or_else(|| error("no shard destination"))?;
                if target.node_id != self.shared.remote.config.node_id {
                    if self.direct {
                        return Err(error("stale shard destination"));
                    }
                    tokio::time::timeout(self.shared.timeout, self.shared.remote.connect(&target))
                        .await
                        .map_err(|_| ActorError::Timeout)?
                        .map_err(|e| error(e.to_string()))?;
                    let remote = self
                        .shared
                        .remote
                        .remote_ref(&format!(
                            "parrot://{}{}{key}",
                            target.node_id,
                            self.shared.prefix(true),
                            key = self.key
                        ))
                        .map_err(|e| error(e.to_string()))?;
                    let result = if ask {
                        resolve(call(|| remote.send(message.take()))?).await
                    } else {
                        resolve(call(|| remote.deliver(message.take()))?)
                            .await
                            .map(|()| Box::new(()) as BoxedMessage)
                    };
                    drop(permit);
                    return result;
                }
                let (tx, rx) = oneshot::channel();
                self.shared
                    .enqueue(
                        Command::Work(Work {
                            family: self.shared.prefix(false),
                            key: self.key.clone(),
                            message: Owned::new(message.take()),
                            reply: ask.then_some(Reply {
                                sender: tx,
                                _control: None,
                            }),
                            _activity: None,
                            _permit: permit,
                        }),
                        false,
                    )
                    .await?;
                if ask {
                    rx.await.map_err(|_| ActorError::Stopped)?.take()
                } else {
                    Ok(Box::new(()) as BoxedMessage)
                }
            }
            .await;
            message.finish(result)
        }))
    }
}
#[async_trait::async_trait]
impl ActorRef for RoutedRef {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.exchange(message, true)
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let future = self.send(message);
        Box::pin(trace::capture(async move {
            match timeout {
                Some(timeout) => tokio::time::timeout(timeout, future)
                    .await
                    .map_err(|_| ActorError::Timeout)?,
                None => future.await,
            }
        }))
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        let future = self.exchange(message, false);
        Box::pin(async move { Owned::new(future.await?).finish(Ok(())) })
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Err(error("use ShardRouter::passivate for owned retirement")) })
    }
    fn path(&self) -> String {
        self.path.clone()
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        Box::pin(async move { !self.shared.closed.load(Ordering::Acquire) })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

enum Entity {
    Claim {
        request: String,
        expected: Option<u64>,
    },
    Owned(OwnershipRecord),
    Active {
        record: OwnershipRecord,
        actor: Owned<BoxedActorRef>,
    },
    Retiring {
        record: OwnershipRecord,
        actor: Option<Owned<BoxedActorRef>>,
        request: String,
    },
}
struct EntityHost {
    node: String,
    prefix: String,
    authority: OwnershipClusterClient,
    factory: Owned<ShardFactory>,
    entities: HashMap<String, Entity>,
    limit: usize,
    timeout: Duration,
    authority_slots: Arc<Semaphore>,
}
impl EntityHost {
    async fn claim(&mut self, key: &str) -> ActorResult<()> {
        let _authority = if matches!(self.entities.get(key), None | Some(Entity::Claim { .. })) {
            Some(
                self.authority_slots
                    .acquire()
                    .await
                    .map_err(|_| ActorError::Stopped)?,
            )
        } else {
            None
        };
        let resource = format!("{}{key}", self.prefix);
        if !self.entities.contains_key(key) {
            if self.entities.len() >= self.limit {
                return Err(error("shard entity capacity exhausted"));
            }
            let view = self
                .authority
                .read_value(&resource, self.timeout)
                .await
                .map_err(|e| error(e.to_string()))?;
            let expected = match view {
                OwnershipStatus::Applied {
                    outcome: OwnershipOutcome::ValueObserved { ownership, .. },
                    ..
                } => match ownership {
                    None => None,
                    Some(record) if record.holder.is_none() => Some(record.generation),
                    Some(_) => {
                        return Err(error("shard generation still held; retirement unconfirmed"))
                    }
                },
                _ => return Err(error("invalid shard ownership observation")),
            };
            self.entities.insert(
                key.into(),
                Entity::Claim {
                    request: id(),
                    expected,
                },
            );
        }
        if let Some(Entity::Claim { request, expected }) = self.entities.get(key) {
            let result = self
                .authority
                .submit_and_wait(
                    request,
                    &OwnershipCommand::Claim {
                        resource,
                        expected: *expected,
                    },
                    self.timeout,
                )
                .await
                .map_err(|e| error(e.to_string()))?;
            match result {
                OwnershipStatus::Applied {
                    outcome: OwnershipOutcome::Changed(record),
                    ..
                } if record.holder.as_deref() == Some(&self.node) => {
                    self.entities.insert(key.into(), Entity::Owned(record));
                }
                OwnershipStatus::Applied { .. } | OwnershipStatus::Conflict => {
                    self.entities.remove(key);
                    return Err(error("shard claim rejected"));
                }
                _ => return Err(error("shard claim is unresolved")),
            }
        }
        Ok(())
    }
    async fn work(
        &mut self,
        key: &str,
        message: &mut Owned<BoxedMessage>,
        ctx: &dyn ActorContext,
    ) -> ActorResult<BoxedMessage> {
        self.claim(key).await?;
        if let Some(Entity::Owned(record)) = self.entities.get(key) {
            let record = record.clone();
            let resource = record.resource.clone();
            let status = {
                let _authority = self
                    .authority_slots
                    .acquire()
                    .await
                    .map_err(|_| ActorError::Stopped)?;
                self.authority
                    .read_value(&resource, self.timeout)
                    .await
                    .map_err(|e| error(e.to_string()))?
            };
            let value = match status {
                OwnershipStatus::Applied {
                    outcome:
                        OwnershipOutcome::ValueObserved {
                            ownership: Some(current),
                            value,
                        },
                    ..
                } if current == record => value,
                _ => return Err(error("shard generation changed before activation")),
            };
            let entity = ShardEntity {
                key: key.into(),
                resource,
                generation: record.generation,
                value,
                authority: self.authority.clone(),
            };
            let mut actor = Owned::new(call(|| (self.factory.get())(entity))??);
            let spawner = ctx
                .async_spawner()
                .ok_or_else(|| error("shard child capability unavailable"))?;
            let child = resolve(call(|| spawner.spawn(actor.take(), Box::new(()), None))?).await?;
            self.entities.insert(
                key.into(),
                Entity::Active {
                    record,
                    actor: Owned::new(child),
                },
            );
        }
        match self.entities.get(key) {
            Some(Entity::Active { actor, .. }) => {
                resolve(call(|| actor.get().send(message.take()))?).await
            }
            _ => Err(error("shard generation is retiring")),
        }
    }
    async fn retire(&mut self, key: &str) -> ActorResult<()> {
        if !self.entities.contains_key(key) {
            return Ok(());
        }
        // Reconcile an ambiguous claim using exactly its original invocation.
        self.claim(key).await?;
        let slot = self
            .entities
            .remove(key)
            .expect("reconciled shard generation");
        let retiring = match slot {
            Entity::Owned(record) => Entity::Retiring {
                record,
                actor: None,
                request: id(),
            },
            Entity::Active { record, actor } => Entity::Retiring {
                record,
                actor: Some(actor),
                request: id(),
            },
            slot => slot,
        };
        self.entities.insert(key.into(), retiring);
        if let Some(Entity::Retiring {
            actor: Some(actor), ..
        }) = self.entities.get(key)
        {
            stop_for_owned_cleanup(actor.get().as_ref()).await?;
        }
        if let Some(Entity::Retiring {
            record, request, ..
        }) = self.entities.get(key)
        {
            let _authority = self
                .authority_slots
                .acquire()
                .await
                .map_err(|_| ActorError::Stopped)?;
            let result = self
                .authority
                .submit_and_wait(
                    request,
                    &OwnershipCommand::Release {
                        resource: record.resource.clone(),
                        expected: record.generation,
                    },
                    self.timeout,
                )
                .await
                .map_err(|e| error(e.to_string()))?;
            match result {
                OwnershipStatus::Applied {
                    outcome: OwnershipOutcome::Changed(released),
                    ..
                } if released.holder.is_none()
                    && released.resource == record.resource
                    && released.generation > record.generation => {}
                _ => return Err(error("shard release not confirmed")),
            }
        } else {
            return Err(error("unresolved shard retirement"));
        }
        Owned::new(self.entities.remove(key)).finish(Ok(()))
    }
}
