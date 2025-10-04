//! 职责：CallbackRegistry——cid → oneshot 映射；ask 的等待端在这里挂起（05 §6.2）。
//!
//! 资源边界（E2.2）：capacity 默认 65536，满则新 ask 拒绝（Overloaded 快速失败）。
//! cid 计数器随系统生命周期单调（重连不重置——RC7 断言防新旧连接撞车）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::error::RemoteError;
use crate::transport::LATE_REPLY_DROPPED;

/// REPLY 载荷语义（成功带 type_key；失败带结构化错误）。
#[derive(Debug, Clone)]
pub enum ReplyPayload {
    Ok(bytes::Bytes, String),
    Err(crate::error::ErrCode, String),
}

pub struct CallbackRegistry {
    next_cid: AtomicU64,
    /// cid → (oneshot, 目标 node)——单链路断连只 fail 该 node 的挂起（K6）。
    slots: Mutex<HashMap<u64, (tokio::sync::oneshot::Sender<ReplyPayload>, String)>>,
    capacity: usize,
}

impl Default for CallbackRegistry {
    fn default() -> Self {
        Self::new(65536)
    }
}

impl CallbackRegistry {
    pub fn new(capacity: usize) -> Self {
        Self {
            next_cid: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    pub fn next_cid(&self) -> u64 {
        self.next_cid.fetch_add(1, Ordering::Relaxed)
    }

    /// 满 → RemoteError::CallbacksFull（Overloaded 语义）。
    /// `node`：目标节点（单链路断连精准 fail 用）。
    pub fn insert(
        &self,
        cid: u64,
        node: impl Into<String>,
        tx: tokio::sync::oneshot::Sender<ReplyPayload>,
    ) -> Result<(), RemoteError> {
        let mut g = self.slots.lock().unwrap();
        if g.len() >= self.capacity {
            return Err(RemoteError::CallbacksFull(self.capacity));
        }
        g.insert(cid, (tx, node.into()));
        Ok(())
    }

    pub fn remove(&self, cid: u64) -> bool {
        self.slots.lock().unwrap().remove(&cid).is_some()
    }

    /// REPLY 到达：完成挂起 ask。false = 迟到回复（调用方已超时放弃）
    /// → metric late_reply_dropped +1（ADR-10）。
    pub fn complete(&self, cid: u64, payload: ReplyPayload) -> bool {
        if let Some((tx, _)) = self.slots.lock().unwrap().remove(&cid) {
            let _ = tx.send(payload);
            true
        } else {
            LATE_REPLY_DROPPED.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// 断连清空：所有挂起 ask 得 ConnectionLost（POC 实证路径——系统级关闭用）。
    pub fn fail_all(&self, code: crate::error::ErrCode, detail: &str) -> usize {
        let mut n = 0;
        let mut g = self.slots.lock().unwrap();
        for (_, (tx, _)) in g.drain() {
            let _ = tx.send(ReplyPayload::Err(code, detail.to_string()));
            n += 1;
        }
        n
    }

    /// 单链路断连：只 fail 发往 `node` 的挂起 ask（其它链路不受牵连——K6 多联语义）。
    pub fn fail_node(&self, node: &str, code: crate::error::ErrCode, detail: &str) -> usize {
        let mut n = 0;
        let mut g = self.slots.lock().unwrap();
        let cids: Vec<u64> = g
            .iter()
            .filter(|(_, (_, nd))| nd == node)
            .map(|(cid, _)| *cid)
            .collect();
        for cid in cids {
            if let Some((tx, _)) = g.remove(&cid) {
                let _ = tx.send(ReplyPayload::Err(code, detail.to_string()));
                n += 1;
            }
        }
        n
    }

    pub fn len(&self) -> usize {
        self.slots.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.lock().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_insert_remove_complete() {
        let r = CallbackRegistry::new(8);
        let cid = r.next_cid();
        assert_eq!(cid, 1);
        let (tx, rx) = tokio::sync::oneshot::channel();
        r.insert(cid, "peer", tx).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r.complete(cid, ReplyPayload::Ok(bytes::Bytes::new(), "k".into())));
        let got = futures::executor::block_on(rx).unwrap();
        assert!(matches!(got, ReplyPayload::Ok(_, _)));
        assert!(r.is_empty());
        // remove 已不在的 → false
        assert!(!r.remove(cid));
    }

    #[test]
    fn callback_capacity_rejects() {
        let r = CallbackRegistry::new(2);
        let (tx1, _rx1) = tokio::sync::oneshot::channel();
        let (tx2, _rx2) = tokio::sync::oneshot::channel();
        r.insert(1, "peer", tx1).unwrap();
        r.insert(2, "peer", tx2).unwrap();
        let (tx3, _rx3) = tokio::sync::oneshot::channel();
        assert!(matches!(r.insert(3, "peer", tx3), Err(RemoteError::CallbacksFull(2))));
    }

    #[test]
    fn callback_fail_all() {
        let r = CallbackRegistry::new(8);
        let (tx1, rx1) = tokio::sync::oneshot::channel();
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        r.insert(1, "x", tx1).unwrap();
        r.insert(2, "y", tx2).unwrap();
        let n = r.fail_all(crate::error::ErrCode::ConnectionLost, "link down");
        assert_eq!(n, 2);
        for rx in [rx1, rx2] {
            let got = futures::executor::block_on(rx).unwrap();
            assert!(matches!(got, ReplyPayload::Err(crate::error::ErrCode::ConnectionLost, _)));
        }
        assert!(r.is_empty());
    }

    /// fail_node：只清目标 node 的挂起（K6 多联隔离）。
    #[test]
    fn callback_fail_node_isolated() {
        let r = CallbackRegistry::new(8);
        let (tx1, rx1) = tokio::sync::oneshot::channel();
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        r.insert(1, "node-b", tx1).unwrap();
        r.insert(2, "node-c", tx2).unwrap();
        // 链路 c 断：只 fail node-c
        let n = r.fail_node("node-c", crate::error::ErrCode::ConnectionLost, "link down");
        assert_eq!(n, 1);
        assert!(matches!(
            futures::executor::block_on(rx2).unwrap(),
            ReplyPayload::Err(crate::error::ErrCode::ConnectionLost, _)
        ));
        // node-b 的挂起不受影响
        assert_eq!(r.len(), 1);
        assert!(r.complete(1, ReplyPayload::Ok(bytes::Bytes::new(), "k".into())));
        drop(rx1);
    }

    #[test]
    fn callback_late_reply_dropped() {
        let before = LATE_REPLY_DROPPED.load(Ordering::Relaxed);
        let r = CallbackRegistry::new(8);
        // 未注册 cid 的 complete → false + metric+1
        assert!(!r.complete(999, ReplyPayload::Err(crate::error::ErrCode::Stopped, "late".into())));
        let after = LATE_REPLY_DROPPED.load(Ordering::Relaxed);
        assert_eq!(after, before + 1, "late_reply_dropped_total must increment (E1.10)");
    }

    #[test]
    fn cid_monotonic_no_reset() {
        let r = CallbackRegistry::new(8);
        let a = r.next_cid();
        let b = r.next_cid();
        assert!(b > a);
        // fail_all 不重置计数器（RC7：重连后 cid 单调不回绕）
        r.fail_all(crate::error::ErrCode::ConnectionLost, "x");
        let c = r.next_cid();
        assert!(c > b);
    }
}
