//! 职责：Receptionist——key → remote_path 注册表 + 订阅流（DEV_02 §3 / 06 §2.2）。
//!
//! Local table and bounded subscriptions. A system-owned facade additionally
//! synchronizes authorized direct cluster links and retires dead-node entries.
//! Standalone facades keep a separate local table without automatic networking.

use std::collections::{HashMap, HashSet};

use parrot_api::receptionist::{ReceptionistEvent, ReceptionistKey, ReceptionistStream};
use serde::{Deserialize, Serialize};

use crate::swim::NodeAddrWire;

pub use parrot_api::receptionist::{ReceptionistEvent as ApiEvent, ReceptionistKey as ApiKey};

/// SYSTEM_EVENT 注册表快照载荷：仅包含 from 节点自己的完整注册表。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceptionistSync {
    pub from: String,
    pub entries: Vec<ReceptionistEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReceptionistEntry {
    pub key: ReceptionistKey,
    pub remote_path: String,
    pub node: String,
}

#[derive(Default)]
pub(crate) struct Notifications {
    wakes: Vec<tokio::sync::mpsc::DeferredSendWakes>,
    retired: Vec<tokio::sync::mpsc::Sender<ReceptionistEvent>>,
}

impl Notifications {
    pub(crate) fn extend(&mut self, mut other: Self) {
        self.wakes.append(&mut other.wakes);
        self.retired.append(&mut other.retired);
    }

    /// Run after releasing all table, membership and connection locks.
    pub(crate) fn dispatch(self) -> parrot_api::types::ActorResult<()> {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self)))
        {
            if let Err(secondary) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                std::mem::forget(secondary);
            }
            return Err(parrot_api::errors::ActorError::Panic(
                "receptionist notification callback panicked after state committed".into(),
            ));
        }
        Ok(())
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        let mut failure = None;
        let mut remember = |result| {
            if let Err(payload) = result {
                if failure.is_none() {
                    failure = Some(payload);
                } else {
                    std::mem::forget(payload);
                }
            }
        };
        for wake in self.wakes.drain(..) {
            remember(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || drop(wake),
            )));
        }
        // Last-sender drop can also invoke a receiver callback.
        for sender in self.retired.drain(..) {
            remember(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || drop(sender),
            )));
        }
        if let Some(payload) = failure {
            if std::thread::panicking() {
                std::mem::forget(payload);
            } else {
                std::panic::resume_unwind(payload);
            }
        }
    }
}

// ---------------- 本地表 + 订阅者 ----------------

#[derive(Default)]
pub struct Receptionist {
    /// key → (node, remote_path) 集合。
    entries: HashMap<String, Vec<ReceptionistEntry>>,
    /// 订阅者（key → 广播通道）。
    subscribers: HashMap<String, Vec<tokio::sync::mpsc::Sender<ReceptionistEvent>>>,
    owners: HashMap<
        (ReceptionistKey, String),
        std::sync::Weak<parrot_api::receptionist::RegistrationScope>,
    >,
}

impl Drop for Receptionist {
    fn drop(&mut self) {
        let retired = std::mem::take(&mut self.subscribers)
            .into_values()
            .flatten()
            .collect();
        let _ = Notifications {
            wakes: Vec::new(),
            retired,
        }
        .dispatch();
    }
}

impl Receptionist {
    pub fn new() -> Self {
        Self::default()
    }

    /// 本节点注册。
    pub fn register(
        &mut self,
        node: &str,
        key: &ReceptionistKey,
        remote_path: &str,
    ) -> ReceptionistEvent {
        self.owners.remove(&(key.clone(), remote_path.to_owned()));
        let e = ReceptionistEntry {
            key: key.clone(),
            remote_path: remote_path.to_string(),
            node: node.to_string(),
        };
        let list = self.entries.entry(key.as_str().to_string()).or_default();
        if !list.contains(&e) {
            list.push(e);
        }
        ReceptionistEvent::Registered {
            key: key.clone(),
            remote_path: remote_path.to_string(),
        }
    }

    /// 本节点去注册。
    pub fn deregister(&mut self, key: &ReceptionistKey, remote_path: &str) -> ReceptionistEvent {
        self.owners.remove(&(key.clone(), remote_path.to_owned()));
        if let Some(list) = self.entries.get_mut(key.as_str()) {
            list.retain(|e| e.remote_path != remote_path);
            if list.is_empty() {
                self.entries.remove(key.as_str());
            }
        }
        ReceptionistEvent::Unregistered {
            key: key.clone(),
            remote_path: remote_path.to_string(),
        }
    }

    /// 订阅：先回放完整快照，再续流（订阅端按 remote_path 去重）。
    /// 容量为快照条数加 buf 个实时事件，buf 为 0 时按 1 处理。
    /// 超大容量限制在 Tokio 队列上限以内，避免合法 usize 预算触发 panic。
    /// 消费者过慢导致队列满时关闭该流；消费完已排队前缀后应重新订阅。
    pub fn subscribe(&mut self, key: &ReceptionistKey, buf: usize) -> ReceptionistStream {
        let (stream, notifications) = self.subscribe_deferred(key, buf);
        drop(notifications);
        stream
    }

    pub(crate) fn subscribe_deferred(
        &mut self,
        key: &ReceptionistKey,
        buf: usize,
    ) -> (ReceptionistStream, Notifications) {
        let mut notifications = Notifications::default();
        self.subscribers.retain(|_, subscribers| {
            notifications
                .retired
                .extend(subscribers.extract_if(.., |subscriber| subscriber.is_closed()));
            !subscribers.is_empty()
        });
        let snapshot = self.entries.get(key.as_str());
        let capacity = snapshot
            .map_or(0, Vec::len)
            .saturating_add(buf.max(1))
            .min(tokio::sync::Semaphore::MAX_PERMITS);
        let (tx, rx) = tokio::sync::mpsc::channel::<ReceptionistEvent>(capacity);
        if let Some(list) = snapshot {
            for e in list {
                tx.try_send(ReceptionistEvent::Registered {
                    key: e.key.clone(),
                    remote_path: e.remote_path.clone(),
                })
                .expect("snapshot fits reserved capacity and receiver is alive");
            }
        }
        self.subscribers
            .entry(key.as_str().to_string())
            .or_default()
            .push(tx);
        (rx, notifications)
    }

    /// 事件广播（本地注册 + 远端同步统一入口）。
    pub fn broadcast(&mut self, ev: &ReceptionistEvent) {
        drop(self.broadcast_deferred(ev));
    }

    pub(crate) fn broadcast_deferred(&mut self, ev: &ReceptionistEvent) -> Notifications {
        let mut notifications = Notifications::default();
        let key = match ev {
            ReceptionistEvent::Registered { key, .. }
            | ReceptionistEvent::Unregistered { key, .. } => key.as_str().to_string(),
        };
        if let Some(subs) = self.subscribers.get_mut(&key) {
            notifications.retired.extend(subs.extract_if(.., |tx| {
                let (result, wake) = tx.try_send_deferred(ev.clone());
                notifications.wakes.push(wake);
                result.is_err()
            }));
            if subs.is_empty() {
                self.subscribers.remove(&key);
            }
        }
        notifications
    }

    /// 全量替换 from 节点的快照并返回精确差量；调用方负责广播返回的事件。
    /// 混入其他节点的快照整体拒绝，不改变表或产生事件。
    /// Checks entry ownership; network callers must additionally authenticate from.
    /// RemoteActorSystem validates its current physical connection and key ACL first.
    pub fn merge_sync(&mut self, sync: &ReceptionistSync) -> Vec<ReceptionistEvent> {
        if sync.entries.iter().any(|entry| entry.node != sync.from) {
            return Vec::new();
        }
        let mut events = Vec::new();
        // Consume retained identities, then insert each remaining input once.
        // Per-key Vec scans would make a complete snapshot quadratic.
        let mut pending: HashSet<&ReceptionistEntry> = sync.entries.iter().collect();
        self.entries.retain(|_, list| {
            list.retain(|entry| {
                let keep = entry.node != sync.from || pending.remove(entry);
                if !keep {
                    events.push(ReceptionistEvent::Unregistered {
                        key: entry.key.clone(),
                        remote_path: entry.remote_path.clone(),
                    });
                }
                keep
            });
            !list.is_empty()
        });
        // upsert 新条目
        for e in &sync.entries {
            if pending.remove(e) {
                self.entries
                    .entry(e.key.as_str().to_string())
                    .or_default()
                    .push(e.clone());
                events.push(ReceptionistEvent::Registered {
                    key: e.key.clone(),
                    remote_path: e.remote_path.clone(),
                });
            }
        }
        self.forget_removed_owners(&events);
        events
    }

    /// 节点 Dead：批量清除该节点全部注册项（返回事件批量推送订阅者）。
    pub fn remove_node(&mut self, node: &str) -> Vec<ReceptionistEvent> {
        let mut events = Vec::new();
        let keys: Vec<String> = self.entries.keys().cloned().collect();
        for k in keys {
            let list = self.entries.get_mut(&k).unwrap();
            let removed: Vec<ReceptionistEntry> = list.extract_if(.., |e| e.node == node).collect();
            if list.is_empty() {
                self.entries.remove(&k);
            }
            for e in removed {
                events.push(ReceptionistEvent::Unregistered {
                    key: e.key,
                    remote_path: e.remote_path,
                });
            }
        }
        self.forget_removed_owners(&events);
        events
    }

    /// 本节点全部条目（gossip 同车快照）。
    pub fn snapshot(&self, node: &str) -> Vec<ReceptionistEntry> {
        self.entries
            .values()
            .flatten()
            .filter(|e| e.node == node)
            .cloned()
            .collect()
    }

    pub fn lookup(&self, key: &ReceptionistKey) -> Vec<&ReceptionistEntry> {
        self.entries
            .get(key.as_str())
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    pub(crate) fn remove_unavailable(
        &mut self,
        local: &str,
        available: &HashSet<String>,
    ) -> Vec<ReceptionistEvent> {
        let mut events = Vec::new();
        self.entries.retain(|_, entries| {
            events.extend(
                entries
                    .extract_if(.., |entry| {
                        entry.node != local && !available.contains(&entry.node)
                    })
                    .map(|entry| ReceptionistEvent::Unregistered {
                        key: entry.key,
                        remote_path: entry.remote_path,
                    }),
            );
            !entries.is_empty()
        });
        self.forget_removed_owners(&events);
        events
    }

    fn forget_removed_owners(&mut self, events: &[ReceptionistEvent]) {
        for event in events {
            if let ReceptionistEvent::Unregistered { key, remote_path } = event {
                self.owners.remove(&(key.clone(), remote_path.clone()));
            }
        }
    }
}

// ---------------- wire 编解码 ----------------

pub fn encode_sync(s: &ReceptionistSync) -> bytes::Bytes {
    let mut b = bytes::BytesMut::new();
    b.extend_from_slice(&[crate::admin::sys_event_tag::RECEPTIONIST_SYNC]);
    b.extend_from_slice(&bincode::serde::encode_to_vec(s, bincode::config::standard()).unwrap());
    b.freeze()
}

/// Decode exactly one complete body with the default remote byte/value/depth budgets.
/// Trailing data and excessive declared lengths are rejected before allocating fields.
pub fn decode_sync(body: &[u8]) -> Result<ReceptionistSync, crate::error::RemoteError> {
    parrot_api::message::serde_remote_deserialize(body)
        .map_err(|e| crate::error::RemoteError::Codec(format!("receptionist sync decode: {e}")))
}

/// 未使用占位（NodeAddrWire 在全量 gossip 时承载地址——P6 digest 扩展点）。
#[allow(dead_code)]
fn _addr_wire_keep(_: &NodeAddrWire) {}

// ---------------- facade（parrot::system::ReceptionistGateway 注入实现） ----------------

/// `ReceptionistFacade`：把 `Receptionist` 适配为 parrot facade 的网关 trait
/// （K2 注入面——`ParrotActorSystem::register_receptionist_gateway` 接收）。
///
/// 并发模型：内表 Mutex 串行化（注册/订阅均为低频控制面操作）。
/// E5 ACL：注入 `AclRules` 后 register/subscribe 前置校验（Forbidden），
/// 每次操作校验注入的principal；认证该身份仍是调用方的责任，构造函数
/// 本身不建立证书绑定。默认拒绝，须通过with_acl显式授权。
/// RemoteActorSystem::receptionist_gateway instead shares its owned network table,
/// uses the configured ACL and qualifies paths with that system's node identity.
pub struct ReceptionistFacade {
    node: String,
    inner: std::sync::Arc<std::sync::Mutex<Receptionist>>,
    acl: crate::acl::AclRules,
    system: Option<std::sync::Weak<crate::system::RemoteActorSystem>>,
}

impl ReceptionistFacade {
    pub fn new(node: impl Into<String>) -> Self {
        Self {
            node: node.into(),
            inner: std::sync::Arc::new(std::sync::Mutex::new(Receptionist::new())),
            acl: crate::acl::AclRules::default(),
            system: None,
        }
    }

    /// 注入静态授权规则；显式permissive只适合调用方控制的开放场景。
    pub fn with_acl(mut self, acl: crate::acl::AclRules) -> Self {
        self.acl = acl;
        self
    }

    pub(crate) fn for_system(system: &std::sync::Arc<crate::system::RemoteActorSystem>) -> Self {
        Self {
            node: system.config.node_id.clone(),
            inner: system.receptionist.clone(),
            acl: system.config.receptionist_acl.clone(),
            system: Some(std::sync::Arc::downgrade(system)),
        }
    }

    fn owner(
        &self,
    ) -> parrot_api::types::ActorResult<Option<std::sync::Arc<crate::system::RemoteActorSystem>>>
    {
        self.system
            .as_ref()
            .map(|system| {
                system
                    .upgrade()
                    .ok_or(parrot_api::errors::ActorError::Stopped)
            })
            .transpose()
    }

    fn live(
        owner: &Option<std::sync::Arc<crate::system::RemoteActorSystem>>,
    ) -> parrot_api::types::ActorResult<()> {
        if let Some(system) = owner {
            if *system.shutdown_signal().borrow() {
                return Err(parrot_api::errors::ActorError::Stopped);
            }
        }
        Ok(())
    }

    fn registration_path(&self, path: String) -> parrot_api::types::ActorResult<String> {
        if self.system.is_none() {
            return Ok(path);
        }
        crate::actor_path::qualify(&self.node, &path).ok_or_else(|| {
            parrot_api::errors::ActorError::InternalError(
                "receptionist path must belong to its owning remote node".into(),
            )
        })
    }

    fn forbidden<T>() -> parrot_api::types::ActorResult<T> {
        Err(parrot_api::errors::ActorError::InternalError(
            "forbidden: receptionist key outside role scope (E5 ACL)".into(),
        ))
    }
}

#[async_trait::async_trait]
impl parrot_api::receptionist::ReceptionistGateway for ReceptionistFacade {
    async fn register_scoped(
        &self,
        key: ReceptionistKey,
        remote_path: String,
        scope: std::sync::Arc<parrot_api::receptionist::RegistrationScope>,
    ) -> parrot_api::types::ActorResult<()> {
        if self.acl.check(&self.node, key.as_str()) != crate::acl::AclDecision::Allow {
            return Self::forbidden();
        }
        let remote_path = self.registration_path(remote_path)?;
        let owner = self.owner()?;
        let binding = (
            std::sync::Arc::as_ptr(&self.inner) as usize,
            key.clone(),
            remote_path.clone(),
        );
        let table = std::sync::Arc::downgrade(&self.inner);
        let generation = std::sync::Arc::downgrade(&scope);
        let cleanup_key = key.clone();
        let cleanup_path = remote_path.clone();
        let cleanup_generation = generation.clone();
        let notifications = {
            let mut table_guard = self.inner.lock().unwrap();
            Self::live(&owner)?;
            scope.publish(
                binding,
                move || {
                    let Some(table) = table.upgrade() else {
                        return Ok(());
                    };
                    let notifications = {
                        let mut table = table.lock().unwrap();
                        if table
                            .owners
                            .get(&(cleanup_key.clone(), cleanup_path.clone()))
                            .is_some_and(|current| current.ptr_eq(&cleanup_generation))
                        {
                            let event = table.deregister(&cleanup_key, &cleanup_path);
                            table.broadcast_deferred(&event)
                        } else {
                            Notifications::default()
                        }
                    };
                    notifications.dispatch()
                },
                || {
                    let event = table_guard.register(&self.node, &key, &remote_path);
                    table_guard
                        .owners
                        .insert((key.clone(), remote_path.clone()), generation.clone());
                    table_guard.broadcast_deferred(&event)
                },
            )?
        };
        notifications.dispatch()
    }

    async fn deregister_scoped(
        &self,
        key: ReceptionistKey,
        remote_path: String,
        scope: std::sync::Arc<parrot_api::receptionist::RegistrationScope>,
    ) -> parrot_api::types::ActorResult<()> {
        if self.acl.check(&self.node, key.as_str()) != crate::acl::AclDecision::Allow {
            return Self::forbidden();
        }
        let remote_path = self.registration_path(remote_path)?;
        let owner = self.owner()?;
        let identity = (key.clone(), remote_path.clone());
        let generation = std::sync::Arc::downgrade(&scope);
        let notifications = {
            let mut table = self.inner.lock().unwrap();
            Self::live(&owner)?;
            scope.withdraw(
                &(
                    std::sync::Arc::as_ptr(&self.inner) as usize,
                    key.clone(),
                    remote_path.clone(),
                ),
                || {
                    if table
                        .owners
                        .get(&identity)
                        .is_some_and(|current| current.ptr_eq(&generation))
                    {
                        let event = table.deregister(&key, &remote_path);
                        table.broadcast_deferred(&event)
                    } else {
                        Notifications::default()
                    }
                },
            )?
        };
        notifications.dispatch()
    }

    async fn register(
        &self,
        key: ReceptionistKey,
        remote_path: String,
    ) -> parrot_api::types::ActorResult<()> {
        // This facade's principal is injected by its owner, not authenticated here.
        if self.acl.check(&self.node, key.as_str()) != crate::acl::AclDecision::Allow {
            return Self::forbidden();
        }
        let remote_path = self.registration_path(remote_path)?;
        let owner = self.owner()?;
        let notifications = {
            let mut g = self.inner.lock().unwrap();
            Self::live(&owner)?;
            let ev = g.register(&self.node, &key, &remote_path);
            g.broadcast_deferred(&ev)
        };
        notifications.dispatch()
    }

    async fn deregister(
        &self,
        key: ReceptionistKey,
        remote_path: String,
    ) -> parrot_api::types::ActorResult<()> {
        if self.acl.check(&self.node, key.as_str()) != crate::acl::AclDecision::Allow {
            return Self::forbidden();
        }
        let remote_path = self.registration_path(remote_path)?;
        let owner = self.owner()?;
        let notifications = {
            let mut g = self.inner.lock().unwrap();
            Self::live(&owner)?;
            let ev = g.deregister(&key, &remote_path);
            g.broadcast_deferred(&ev)
        };
        notifications.dispatch()
    }

    async fn subscribe(
        &self,
        key: ReceptionistKey,
    ) -> parrot_api::types::ActorResult<ReceptionistStream> {
        if self.acl.check(&self.node, key.as_str()) != crate::acl::AclDecision::Allow {
            return Self::forbidden();
        }
        let owner = self.owner()?;
        let (stream, notifications) = {
            let mut g = self.inner.lock().unwrap();
            Self::live(&owner)?;
            g.subscribe_deferred(&key, 64)
        };
        notifications.dispatch()?;
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> ReceptionistKey {
        ReceptionistKey::new(s).unwrap()
    }

    #[test]
    fn membership_retirement_matches_an_independent_owner_model() {
        use std::collections::BTreeSet;
        let nodes = ["local", "a", "b", "c", "d", "e"];
        for registered in 0u8..64 {
            for present in 0u8..64 {
                let mut table = Receptionist::new();
                let mut original = Vec::new();
                for (index, node) in nodes.iter().enumerate() {
                    if registered & (1 << index) == 0 {
                        continue;
                    }
                    for service in 0..2 {
                        let entry = ReceptionistEntry {
                            key: key(&format!("edge/service-{service}")),
                            remote_path: format!("parrot://{node}/user/{service}"),
                            node: (*node).into(),
                        };
                        table.register(node, &entry.key, &entry.remote_path);
                        original.push(entry);
                    }
                }
                let available: HashSet<_> = nodes
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| present & (1 << index) != 0)
                    .map(|(_, node)| (*node).to_owned())
                    .collect();
                let retained_nodes: BTreeSet<_> = std::iter::once("local")
                    .chain(
                        nodes
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| present & (1 << index) != 0)
                            .map(|(_, node)| *node),
                    )
                    .collect();
                let expected: BTreeSet<_> = original
                    .iter()
                    .filter(|entry| !retained_nodes.contains(entry.node.as_str()))
                    .map(|entry| (entry.key.as_str().to_owned(), entry.remote_path.clone()))
                    .collect();
                let events = table.remove_unavailable("local", &available);
                let actual: BTreeSet<_> = events
                    .iter()
                    .map(|event| match event {
                        ReceptionistEvent::Unregistered { key, remote_path } => {
                            (key.as_str().to_owned(), remote_path.clone())
                        }
                        other => panic!("retirement must only withdraw services: {other:?}"),
                    })
                    .collect();
                assert_eq!(actual.len(), events.len(), "duplicate retirement events");
                assert_eq!(
                    actual, expected,
                    "registered={registered} present={present}"
                );
                for node in nodes {
                    let expected: BTreeSet<_> = original
                        .iter()
                        .filter(|entry| entry.node == node && retained_nodes.contains(node))
                        .map(|entry| (entry.key.as_str().to_owned(), entry.remote_path.clone()))
                        .collect();
                    let snapshot = table.snapshot(node);
                    let actual: BTreeSet<_> = snapshot
                        .iter()
                        .map(|entry| (entry.key.as_str().to_owned(), entry.remote_path.clone()))
                        .collect();
                    assert_eq!(snapshot.len(), actual.len());
                    assert_eq!(actual, expected);
                }
                assert!(table.remove_unavailable("local", &available).is_empty());
            }
        }
    }

    #[test]
    fn simultaneous_node_retirement_preserves_live_services_and_reports_cost() {
        use std::{collections::BTreeSet, time::Instant};
        let service = key("edge/many-owners");
        for size in [1024, 4096, 16384] {
            let mut table = Receptionist::new();
            table.register("local", &service, "parrot://local/user/keep");
            table.register("live", &service, "parrot://live/user/keep");
            let mut expected = BTreeSet::new();
            // Populate fixture state without measuring local register's scan.
            for index in 0..size {
                let node = format!("departed-{index}");
                let path = format!("parrot://{node}/user/service");
                table
                    .entries
                    .get_mut(service.as_str())
                    .unwrap()
                    .push(ReceptionistEntry {
                        key: service.clone(),
                        node,
                        remote_path: path.clone(),
                    });
                expected.insert(path);
            }
            let available = HashSet::from(["live".to_owned()]);
            let started = Instant::now();
            let events = table.remove_unavailable("local", &available);
            eprintln!(
                "RETIRE_SAMPLE nodes={size} nanoseconds={}",
                started.elapsed().as_nanos()
            );
            let actual: BTreeSet<_> = events
                .iter()
                .map(|event| match event {
                    ReceptionistEvent::Unregistered { key, remote_path } => {
                        assert_eq!(key, &service);
                        remote_path.clone()
                    }
                    other => panic!("retirement must only withdraw services: {other:?}"),
                })
                .collect();
            assert_eq!(events.len(), size);
            assert_eq!(actual, expected);
            assert_eq!(table.lookup(&service).len(), 2);
            for node in ["local", "live"] {
                let entries = table.snapshot(node);
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].remote_path, format!("parrot://{node}/user/keep"));
            }
            assert!(table.remove_unavailable("local", &available).is_empty());
        }
    }

    #[test]
    fn closed_subscriptions_for_distinct_keys_do_not_accumulate_without_broadcast() {
        let mut table = Receptionist::new();
        let active_key = key("edge/active");
        let mut active = table.subscribe(&active_key, 1);
        for index in 0..128 {
            drop(table.subscribe(&key(&format!("edge/abandoned-{index}")), 1));
            let retained = table.subscribers.len();
            assert!(
                retained <= 2,
                "retained {retained} subscriber keys; expected at most 2"
            );
        }
        let new_key = key("edge/new");
        let mut new = table.subscribe(&new_key, 1);
        assert_eq!(table.subscribers.len(), 2);
        assert!(!active.is_closed());
        for (service, stream, path) in [
            (&active_key, &mut active, "parrot://n/user/active"),
            (&new_key, &mut new, "parrot://n/user/new"),
        ] {
            let event = table.register("n", service, path);
            table.broadcast(&event);
            assert_eq!(stream.try_recv(), Ok(event));
        }
    }

    #[test]
    fn closed_subscriptions_for_one_key_do_not_accumulate_without_broadcast() {
        let mut table = Receptionist::new();
        let service = key("edge/active");
        let mut active = table.subscribe(&service, 1);
        for _ in 0..128 {
            drop(table.subscribe(&service, 1));
            let retained = table.subscribers[service.as_str()].len();
            assert!(
                retained <= 2,
                "retained {retained} senders; expected at most 2"
            );
        }
        let mut new = table.subscribe(&service, 1);
        assert_eq!(table.subscribers.len(), 1);
        assert_eq!(table.subscribers[service.as_str()].len(), 2);
        let event = table.register("n", &service, "parrot://n/user/active");
        table.broadcast(&event);
        assert_eq!(active.try_recv(), Ok(event.clone()));
        assert_eq!(new.try_recv(), Ok(event));
    }

    // receptionist_register_subscribe_flow：本节点注册→订阅者收到
    #[test]
    fn receptionist_register_subscribe_flow() {
        let mut r = Receptionist::new();
        r.register("n1", &key("edge/sensor"), "parrot://n1/user/s1");
        // 订阅（回放快照）
        let mut stream = r.subscribe(&key("edge/sensor"), 16);
        let ev = stream.try_recv().unwrap();
        assert_eq!(
            ev,
            ReceptionistEvent::Registered {
                key: key("edge/sensor"),
                remote_path: "parrot://n1/user/s1".into()
            }
        );
        // 续流：新注册广播
        let registered = r.register("n1", &key("edge/sensor"), "parrot://n1/user/s2");
        r.broadcast(&registered);
        let ev2 = stream.try_recv().unwrap();
        assert_eq!(
            ev2,
            ReceptionistEvent::Registered {
                key: key("edge/sensor"),
                remote_path: "parrot://n1/user/s2".into(),
            }
        );
    }

    // receptionist_cross_node：双节点 sync 合并 → 远端可查
    #[test]
    fn receptionist_cross_node() {
        let mut a = Receptionist::new();
        let mut b = Receptionist::new();
        a.register("na", &key("cloud/api"), "parrot://na/user/api1");
        // 本地双表合并；此用例不启动网络 gossip。
        let sync = ReceptionistSync {
            from: "na".into(),
            entries: a.snapshot("na"),
        };
        let events = b.merge_sync(&sync);
        assert_eq!(
            events,
            vec![ReceptionistEvent::Registered {
                key: key("cloud/api"),
                remote_path: "parrot://na/user/api1".into(),
            }]
        );
        let got = b.lookup(&key("cloud/api"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].remote_path, "parrot://na/user/api1");
        assert_eq!(b.snapshot("na"), sync.entries);
        // 幂等重放
        let events2 = b.merge_sync(&sync);
        assert!(events2.is_empty());
    }

    // receptionist_dead_node_cleanup：节点 Dead → Unregistered 批量
    #[test]
    fn receptionist_dead_node_cleanup() {
        let mut r = Receptionist::new();
        r.register("nx", &key("edge/a"), "parrot://nx/user/a");
        r.register("nx", &key("edge/b"), "parrot://nx/user/b");
        r.register("ny", &key("edge/c"), "parrot://ny/user/c");
        let events = r.remove_node("nx");
        assert_eq!(events.len(), 2);
        for (service, path) in [
            ("edge/a", "parrot://nx/user/a"),
            ("edge/b", "parrot://nx/user/b"),
        ] {
            assert!(events.contains(&ReceptionistEvent::Unregistered {
                key: key(service),
                remote_path: path.into(),
            }));
        }
        assert_eq!(r.lookup(&key("edge/a")).len(), 0);
        assert_eq!(r.lookup(&key("edge/b")).len(), 0);
        assert_eq!(r.lookup(&key("edge/c")).len(), 1); // ny 不受影响
        assert!(r.snapshot("nx").is_empty());
        assert_eq!(
            r.snapshot("ny"),
            vec![ReceptionistEntry {
                key: key("edge/c"),
                remote_path: "parrot://ny/user/c".into(),
                node: "ny".into(),
            }]
        );
        assert!(r.remove_node("nx").is_empty());
    }

    // key 校验：非法字符启动即拒
    #[test]
    fn receptionist_key_validation() {
        assert!(ReceptionistKey::new("edge/ok key").is_err());
        assert!(ReceptionistKey::new("edge/*").is_err());
        assert!(ReceptionistKey::new("edge/#x").is_err());
        assert!(ReceptionistKey::new("edge/fine").is_ok());
    }

    // sync wire 往返
    #[test]
    fn sync_wire_roundtrip() {
        let s = ReceptionistSync {
            from: "n1".into(),
            entries: vec![ReceptionistEntry {
                key: key("edge/x"),
                remote_path: "parrot://n1/user/x".into(),
                node: "n1".into(),
            }],
        };
        let b = encode_sync(&s);
        assert_eq!(b[0], crate::admin::sys_event_tag::RECEPTIONIST_SYNC);
        let got = decode_sync(&b[1..]).unwrap();
        assert_eq!(got.entries, s.entries);
        assert_eq!(got.from, s.from);
    }

    // 流保留相同路径的事件，去重由订阅端负责。
    #[tokio::test]
    async fn subscribe_snapshot_then_live_preserves_repeated_events() {
        let mut r = Receptionist::new();
        r.register("n1", &key("edge/s"), "parrot://n1/user/s1");
        let mut stream = r.subscribe(&key("edge/s"), 16);
        // 快照一条
        let expected = ReceptionistEvent::Registered {
            key: key("edge/s"),
            remote_path: "parrot://n1/user/s1".into(),
        };
        assert_eq!(stream.try_recv(), Ok(expected.clone()));
        // 广播同 remote_path 的事件（订阅端去重责任——此处只验证流语义）
        r.broadcast(&ReceptionistEvent::Registered {
            key: key("edge/s"),
            remote_path: "parrot://n1/user/s1".into(),
        });
        assert_eq!(stream.try_recv(), Ok(expected));
        assert_eq!(
            stream.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
    }
}

#[cfg(test)]
mod wake_boundary_reproduction {
    use super::*;
    use parrot_api::receptionist::ReceptionistGateway;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    };
    use std::task::{Context, Wake, Waker};

    struct Probe {
        table: Weak<Mutex<Receptionist>>,
        observed_locked: AtomicBool,
        wakes: AtomicUsize,
        panic_on_wake: bool,
    }

    impl Wake for Probe {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
            if let Some(table) = self.table.upgrade() {
                if table.try_lock().is_err() {
                    self.observed_locked.store(true, Ordering::SeqCst);
                }
            }
            assert!(!self.panic_on_wake, "controlled subscriber wake panic");
        }
    }

    fn key() -> ReceptionistKey {
        ReceptionistKey::new("edge/wake-boundary").unwrap()
    }

    fn probe(facade: &ReceptionistFacade, panic_on_wake: bool) -> Arc<Probe> {
        Arc::new(Probe {
            table: Arc::downgrade(&facade.inner),
            observed_locked: AtomicBool::new(false),
            wakes: AtomicUsize::new(0),
            panic_on_wake,
        })
    }

    fn park(stream: &mut ReceptionistStream, probe: &Arc<Probe>) {
        let waker = Waker::from(probe.clone());
        let mut context = Context::from_waker(&waker);
        assert!(stream.poll_recv(&mut context).is_pending());
    }

    #[test]
    fn facade_registration_releases_table_before_receiver_wake() {
        futures::executor::block_on(async {
            let facade = ReceptionistFacade::new("n").with_acl(crate::acl::AclRules::permissive());
            let mut stream = facade.subscribe(key()).await.unwrap();
            let probe = probe(&facade, false);
            park(&mut stream, &probe);
            facade
                .register(key(), "/user/service".into())
                .await
                .unwrap();
            assert_eq!(probe.wakes.load(Ordering::SeqCst), 1);
            assert!(matches!(
                stream.try_recv(),
                Ok(ReceptionistEvent::Registered { .. })
            ));
            assert!(
                !probe.observed_locked.load(Ordering::SeqCst),
                "subscriber wake ran under the receptionist table lock"
            );
        });
    }

    #[test]
    fn facade_withdrawal_releases_table_before_receiver_wake() {
        futures::executor::block_on(async {
            let facade = ReceptionistFacade::new("n").with_acl(crate::acl::AclRules::permissive());
            facade
                .register(key(), "/user/service".into())
                .await
                .unwrap();
            let mut stream = facade.subscribe(key()).await.unwrap();
            assert!(matches!(
                stream.try_recv(),
                Ok(ReceptionistEvent::Registered { .. })
            ));
            let probe = probe(&facade, false);
            park(&mut stream, &probe);
            facade
                .deregister(key(), "/user/service".into())
                .await
                .unwrap();
            assert_eq!(probe.wakes.load(Ordering::SeqCst), 1);
            assert!(matches!(
                stream.try_recv(),
                Ok(ReceptionistEvent::Unregistered { .. })
            ));
            assert!(
                !probe.observed_locked.load(Ordering::SeqCst),
                "subscriber withdrawal wake ran under the receptionist table lock"
            );
        });
    }

    #[test]
    fn receiver_wake_panic_cannot_poison_committed_registration() {
        let facade = ReceptionistFacade::new("n").with_acl(crate::acl::AclRules::permissive());
        let mut stream = futures::executor::block_on(facade.subscribe(key())).unwrap();
        let probe = probe(&facade, true);
        park(&mut stream, &probe);
        let _result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            futures::executor::block_on(facade.register(key(), "/user/service".into()))
        }));
        assert_eq!(probe.wakes.load(Ordering::SeqCst), 1);
        assert!(
            !facade.inner.is_poisoned(),
            "a receiver wake panic poisoned the shared receptionist table"
        );
        let mut next = futures::executor::block_on(facade.subscribe(key())).unwrap();
        assert_eq!(
            next.try_recv().unwrap(),
            ReceptionistEvent::Registered {
                key: key(),
                remote_path: "/user/service".into(),
            }
        );
    }
}

#[cfg(test)]
mod wake_boundary_contracts {
    use super::*;
    use parrot_api::receptionist::ReceptionistGateway;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    };
    use std::task::{Context, Poll, Wake, Waker};

    struct Callback {
        table: Weak<Mutex<Receptionist>>,
        calls: AtomicUsize,
        locked: AtomicBool,
        panic: bool,
        reenter: Option<Arc<ReceptionistFacade>>,
    }

    impl Wake for Callback {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let unlocked = self
                .table
                .upgrade()
                .is_none_or(|table| table.try_lock().is_ok());
            self.locked.fetch_or(!unlocked, Ordering::SeqCst);
            if let Some(facade) = &self.reenter {
                assert!(unlocked, "reentry would deadlock on the shared table");
                // Poll directly: nesting a futures executor would test the executor.
                let mut operation = facade.register(key(), "/user/reentrant".into());
                let waker = futures::task::noop_waker();
                assert!(matches!(
                    operation.as_mut().poll(&mut Context::from_waker(&waker)),
                    Poll::Ready(Ok(()))
                ));
            }
            assert!(!self.panic, "controlled notification panic");
        }
    }

    fn key() -> ReceptionistKey {
        ReceptionistKey::new("edge/wake-contracts").unwrap()
    }

    fn facade() -> Arc<ReceptionistFacade> {
        Arc::new(
            ReceptionistFacade::new("wake-contracts").with_acl(crate::acl::AclRules::permissive()),
        )
    }

    fn callback(facade: &Arc<ReceptionistFacade>, panic: bool, reenter: bool) -> Arc<Callback> {
        Arc::new(Callback {
            table: Arc::downgrade(&facade.inner),
            calls: AtomicUsize::new(0),
            locked: AtomicBool::new(false),
            panic,
            reenter: reenter.then(|| facade.clone()),
        })
    }

    fn park(stream: &mut ReceptionistStream, callback: &Arc<Callback>) {
        let waker = Waker::from(callback.clone());
        assert!(stream
            .poll_recv(&mut Context::from_waker(&waker))
            .is_pending());
    }

    fn registered(path: &str) -> ReceptionistEvent {
        ReceptionistEvent::Registered {
            key: key(),
            remote_path: path.into(),
        }
    }

    #[test]
    fn reentrant_registration_preserves_original_before_nested_event_for_every_subscriber() {
        futures::executor::block_on(async {
            let facade = facade();
            let mut first = facade.subscribe(key()).await.unwrap();
            let mut second = facade.subscribe(key()).await.unwrap();
            let nested = callback(&facade, false, true);
            let sibling = callback(&facade, false, false);
            park(&mut first, &nested);
            park(&mut second, &sibling);
            facade
                .register(key(), "/user/original".into())
                .await
                .unwrap();
            assert_eq!(nested.calls.load(Ordering::SeqCst), 1);
            assert_eq!(sibling.calls.load(Ordering::SeqCst), 1);
            assert!(!nested.locked.load(Ordering::SeqCst));
            assert!(!sibling.locked.load(Ordering::SeqCst));
            for stream in [&mut first, &mut second] {
                assert_eq!(stream.try_recv().unwrap(), registered("/user/original"));
                assert_eq!(stream.try_recv().unwrap(), registered("/user/reentrant"));
                assert_eq!(
                    stream.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                );
            }
        });
    }

    #[test]
    fn first_wake_panic_still_notifies_all_siblings_and_reports_committed_state() {
        futures::executor::block_on(async {
            let facade = facade();
            let mut first = facade.subscribe(key()).await.unwrap();
            let mut second = facade.subscribe(key()).await.unwrap();
            let mut third = facade.subscribe(key()).await.unwrap();
            let throwing = callback(&facade, true, false);
            let healthy = callback(&facade, false, false);
            let also_throwing = callback(&facade, true, false);
            park(&mut first, &throwing);
            park(&mut second, &healthy);
            park(&mut third, &also_throwing);
            let outcome = facade.register(key(), "/user/committed".into()).await;
            assert!(matches!(
                outcome,
                Err(parrot_api::errors::ActorError::Panic(_))
            ));
            assert!(!facade.inner.is_poisoned());
            for probe in [&throwing, &healthy, &also_throwing] {
                assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
                assert!(!probe.locked.load(Ordering::SeqCst));
            }
            for stream in [&mut first, &mut second, &mut third] {
                assert_eq!(stream.try_recv().unwrap(), registered("/user/committed"));
            }
            let mut replay = facade.subscribe(key()).await.unwrap();
            assert_eq!(replay.try_recv().unwrap(), registered("/user/committed"));
            park(&mut second, &healthy);
            facade
                .deregister(key(), "/user/committed".into())
                .await
                .unwrap();
            assert_eq!(healthy.calls.load(Ordering::SeqCst), 2);
            assert!(matches!(
                second.try_recv(),
                Ok(ReceptionistEvent::Unregistered { .. })
            ));
        });
    }

    #[test]
    fn explicitly_closed_receiver_cleanup_wakes_after_table_unlock() {
        futures::executor::block_on(async {
            let facade = facade();
            let mut closed = facade.subscribe(key()).await.unwrap();
            let probe = callback(&facade, false, false);
            park(&mut closed, &probe);
            closed.close();
            let live = facade.subscribe(key()).await.unwrap();
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
            assert!(!probe.locked.load(Ordering::SeqCst));
            assert_eq!(
                closed.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            );
            assert!(!live.is_closed());
            assert_eq!(
                facade.inner.lock().unwrap().subscribers[key().as_str()].len(),
                1
            );
        });
    }

    #[test]
    fn overflow_preserves_queued_prefix_and_resubscription_replays_committed_state() {
        futures::executor::block_on(async {
            let facade = facade();
            let mut stream = facade.inner.lock().unwrap().subscribe(&key(), 1);
            let probe = callback(&facade, false, false);
            park(&mut stream, &probe);
            facade.register(key(), "/user/first".into()).await.unwrap();
            facade
                .register(key(), "/user/overflow".into())
                .await
                .unwrap();
            assert!(!probe.locked.load(Ordering::SeqCst));
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
            assert_eq!(stream.try_recv().unwrap(), registered("/user/first"));
            assert_eq!(
                stream.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            );
            let mut replay = facade.subscribe(key()).await.unwrap();
            assert_eq!(replay.try_recv().unwrap(), registered("/user/first"));
            assert_eq!(replay.try_recv().unwrap(), registered("/user/overflow"));
        });
    }

    #[test]
    fn table_destruction_finishes_sibling_closure_after_a_panicking_wake() {
        let facade = facade();
        let mut first = futures::executor::block_on(facade.subscribe(key())).unwrap();
        let mut second = futures::executor::block_on(facade.subscribe(key())).unwrap();
        let throwing = callback(&facade, true, false);
        let healthy = callback(&facade, false, false);
        park(&mut first, &throwing);
        park(&mut second, &healthy);
        let retired = std::mem::take(&mut *facade.inner.lock().unwrap());
        drop(retired);
        assert!(!facade.inner.is_poisoned());
        for probe in [&throwing, &healthy] {
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
            assert!(!probe.locked.load(Ordering::SeqCst));
        }
        assert_eq!(
            first.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        );
        assert_eq!(
            second.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        );
    }
}
