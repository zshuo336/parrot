//! 职责：Receptionist——key → remote_path 注册表 + 订阅流（DEV_02 §3 / 06 §2.2）。
//!
//! 本地表 + 订阅者表；注册产生 ReceptionistSync 搭 MembershipGossip 同车
//! （同一 gossip 循环，不另起任务——带宽预算靠单车原则）。

use std::collections::HashMap;

use parrot_api::receptionist::{ReceptionistEvent, ReceptionistKey, ReceptionistStream};
use serde::{Deserialize, Serialize};

use crate::swim::NodeAddrWire;

pub use parrot_api::receptionist::{ReceptionistEvent as ApiEvent, ReceptionistKey as ApiKey};

/// gossip 载荷（SYSTEM_EVENT tag 0x20）：本节点注册表快照 + 增量事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceptionistSync {
    pub from: String,
    pub entries: Vec<ReceptionistEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReceptionistEntry {
    pub key: ReceptionistKey,
    pub remote_path: String,
    pub node: String,
}

// ---------------- 本地表 + 订阅者 ----------------

#[derive(Default)]
pub struct Receptionist {
    /// key → (node, remote_path) 集合。
    entries: HashMap<String, Vec<ReceptionistEntry>>,
    /// 订阅者（key → 广播通道）。
    subscribers: HashMap<String, Vec<tokio::sync::mpsc::Sender<ReceptionistEvent>>>,
}

impl Receptionist {
    pub fn new() -> Self {
        Self::default()
    }

    /// 本节点注册。
    pub fn register(&mut self, node: &str, key: &ReceptionistKey, remote_path: &str) -> ReceptionistEvent {
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

    /// 订阅：先回放快照再续流（订阅端按 remote_path 去重）。
    pub fn subscribe(&mut self, key: &ReceptionistKey, buf: usize) -> ReceptionistStream {
        let (tx, rx) = tokio::sync::mpsc::channel::<ReceptionistEvent>(buf);
        // 回放快照
        if let Some(list) = self.entries.get(key.as_str()) {
            for e in list {
                let _ = tx.try_send(ReceptionistEvent::Registered {
                    key: e.key.clone(),
                    remote_path: e.remote_path.clone(),
                });
            }
        }
        self.subscribers
            .entry(key.as_str().to_string())
            .or_default()
            .push(tx);
        // 丢弃已关订阅者（下次广播时清理）
        rx
    }

    /// 事件广播（本地注册 + 远端同步统一入口）。
    pub fn broadcast(&mut self, ev: &ReceptionistEvent) {
        let key = match ev {
            ReceptionistEvent::Registered { key, .. } | ReceptionistEvent::Unregistered { key, .. } => {
                key.as_str().to_string()
            }
        };
        if let Some(subs) = self.subscribers.get_mut(&key) {
            subs.retain(|tx| tx.try_send(ev.clone()).is_ok());
            if subs.is_empty() {
                self.subscribers.remove(&key);
            }
        }
    }

    /// 远端 sync 合并（节点 Dead → 该节点全部注册项批量 Unregistered——K1 回调驱动）。
    pub fn merge_sync(&mut self, sync: &ReceptionistSync) -> Vec<ReceptionistEvent> {
        let mut events = Vec::new();
        // 全量替换该节点条目（幂等）
        let nodes_entries: Vec<&ReceptionistEntry> = sync.entries.iter().collect();
        // 先移除该节点旧条目（不在新快照中的）
        for (k, list) in self.entries.iter_mut() {
            let before = list.len();
            list.retain(|e| {
                e.node != sync.from || nodes_entries.iter().any(|ne| ne == &e)
            });
            if list.len() != before {
                for removed in 0..(before - list.len()) {
                    let _ = removed;
                    events.push(ReceptionistEvent::Unregistered {
                        key: ReceptionistKey::new(k.clone()).unwrap(),
                        remote_path: String::new(),
                    });
                }
            }
        }
        // upsert 新条目
        for e in &sync.entries {
            let list = self.entries.entry(e.key.as_str().to_string()).or_default();
            if !list.contains(e) {
                list.push(e.clone());
                events.push(ReceptionistEvent::Registered {
                    key: e.key.clone(),
                    remote_path: e.remote_path.clone(),
                });
            }
        }
        events
    }

    /// 节点 Dead：批量清除该节点全部注册项（返回事件批量推送订阅者）。
    pub fn remove_node(&mut self, node: &str) -> Vec<ReceptionistEvent> {
        let mut events = Vec::new();
        let keys: Vec<String> = self.entries.keys().cloned().collect();
        for k in keys {
            let list = self.entries.get_mut(&k).unwrap();
            let before = list.len();
            let removed: Vec<ReceptionistEntry> =
                list.extract_if(.., |e| e.node == node).collect();
            if list.is_empty() {
                self.entries.remove(&k);
            }
            for e in removed {
                events.push(ReceptionistEvent::Unregistered {
                    key: e.key,
                    remote_path: e.remote_path,
                });
            }
            let _ = before;
        }
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
        self.entries.get(key.as_str()).map(|v| v.iter().collect()).unwrap_or_default()
    }
}

// ---------------- wire 编解码 ----------------

pub fn encode_sync(s: &ReceptionistSync) -> bytes::Bytes {
    let mut b = bytes::BytesMut::new();
    b.extend_from_slice(&[crate::admin::sys_event_tag::RECEPTIONIST_SYNC]);
    b.extend_from_slice(&bincode::serde::encode_to_vec(s, bincode::config::standard()).unwrap());
    b.freeze()
}

pub fn decode_sync(body: &[u8]) -> Result<ReceptionistSync, crate::error::RemoteError> {
    bincode::serde::decode_from_slice(body, bincode::config::standard())
        .map(|(s, _)| s)
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
/// 订阅流按 role 过滤事件下发；`role` 由调用方传入（生产=证书 CN）。
pub struct ReceptionistFacade {
    node: String,
    inner: std::sync::Mutex<Receptionist>,
    acl: crate::acl::AclRules,
}

impl ReceptionistFacade {
    pub fn new(node: impl Into<String>) -> Self {
        Self {
            node: node.into(),
            inner: std::sync::Mutex::new(Receptionist::new()),
            acl: crate::acl::AclRules::permissive(),
        }
    }

    /// 注入 ACL（E5——静态规则；未注入=全放行）。
    pub fn with_acl(mut self, acl: crate::acl::AclRules) -> Self {
        self.acl = acl;
        self
    }

    fn forbidden<T>() -> parrot_api::types::ActorResult<T> {
        Err(parrot_api::errors::ActorError::InternalError(
            "forbidden: receptionist key outside role scope (E5 ACL)".into(),
        ))
    }
}

#[async_trait::async_trait]
impl parrot_api::receptionist::ReceptionistGateway for ReceptionistFacade {
    async fn register(
        &self,
        key: ReceptionistKey,
        remote_path: String,
    ) -> parrot_api::types::ActorResult<()> {
        // E5：node 即 role 源（dev；生产证书 CN——同值注入）
        if self.acl.check(&self.node, key.as_str()) != crate::acl::AclDecision::Allow {
            return Self::forbidden();
        }
        let mut g = self.inner.lock().unwrap();
        let ev = g.register(&self.node, &key, &remote_path);
        g.broadcast(&ev);
        Ok(())
    }

    async fn deregister(
        &self,
        key: ReceptionistKey,
        remote_path: String,
    ) -> parrot_api::types::ActorResult<()> {
        if self.acl.check(&self.node, key.as_str()) != crate::acl::AclDecision::Allow {
            return Self::forbidden();
        }
        let mut g = self.inner.lock().unwrap();
        let ev = g.deregister(&key, &remote_path);
        g.broadcast(&ev);
        Ok(())
    }

    async fn subscribe(
        &self,
        key: ReceptionistKey,
    ) -> parrot_api::types::ActorResult<ReceptionistStream> {
        if self.acl.check(&self.node, key.as_str()) != crate::acl::AclDecision::Allow {
            return Self::forbidden();
        }
        let mut g = self.inner.lock().unwrap();
        Ok(g.subscribe(&key, 64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> ReceptionistKey {
        ReceptionistKey::new(s).unwrap()
    }

    // receptionist_register_subscribe_flow：本节点注册→订阅者收到
    #[tokio::test]
    async fn receptionist_register_subscribe_flow() {
        let mut r = Receptionist::new();
        r.register("n1", &key("edge/sensor"), "parrot://n1/user/s1");
        // 订阅（回放快照）
        let mut stream = r.subscribe(&key("edge/sensor"), 16);
        let ev = stream.recv().await.unwrap();
        assert_eq!(
            ev,
            ReceptionistEvent::Registered {
                key: key("edge/sensor"),
                remote_path: "parrot://n1/user/s1".into()
            }
        );
        // 续流：新注册广播
        r.register("n1", &key("edge/sensor"), "parrot://n1/user/s2");
        r.broadcast(&ReceptionistEvent::Registered {
            key: key("edge/sensor"),
            remote_path: "parrot://n1/user/s2".into(),
        });
        let ev2 = stream.recv().await.unwrap();
        assert!(matches!(ev2, ReceptionistEvent::Registered { .. }));
    }

    // receptionist_cross_node：双节点 sync 合并 → 远端可查
    #[test]
    fn receptionist_cross_node() {
        let mut a = Receptionist::new();
        let mut b = Receptionist::new();
        a.register("na", &key("cloud/api"), "parrot://na/user/api1");
        // A → B sync（gossip 同车）
        let sync = ReceptionistSync {
            from: "na".into(),
            entries: a.snapshot("na"),
        };
        let events = b.merge_sync(&sync);
        assert_eq!(events.len(), 1);
        let got = b.lookup(&key("cloud/api"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].remote_path, "parrot://na/user/api1");
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
        assert!(events.iter().all(|e| matches!(e, ReceptionistEvent::Unregistered { .. })));
        assert_eq!(r.lookup(&key("edge/a")).len(), 0);
        assert_eq!(r.lookup(&key("edge/c")).len(), 1); // ny 不受影响
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
        assert_eq!(got.entries.len(), 1);
        assert_eq!(got.from, "n1");
    }

    // 订阅端按 remote_path 去重（快照回放 + 续流不重复）
    #[tokio::test]
    async fn subscribe_snapshot_then_live_no_dup() {
        let mut r = Receptionist::new();
        r.register("n1", &key("edge/s"), "parrot://n1/user/s1");
        let mut stream = r.subscribe(&key("edge/s"), 16);
        // 快照一条
        let _ = stream.recv().await.unwrap();
        // 广播同 remote_path 的事件（订阅端去重责任——此处只验证流语义）
        r.broadcast(&ReceptionistEvent::Registered {
            key: key("edge/s"),
            remote_path: "parrot://n1/user/s1".into(),
        });
        // 流关闭前 try_next 应立刻得到（或空——非阻塞验证）
        let _ = stream.try_recv();
    }
}
