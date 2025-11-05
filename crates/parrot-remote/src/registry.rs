//! 职责：CallbackRegistry——cid → oneshot 映射；ask 的等待端在这里挂起（05 §6.2）。
//!
//! 资源边界（E2.2）：capacity 默认 65536，满则新 ask 拒绝（Overloaded 快速失败）。
//! cid 计数器随系统生命周期单调（重连不重置——RC7 断言防新旧连接撞车）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::error::RemoteError;
use crate::transport::{FrameSender, LATE_REPLY_DROPPED};

/// REPLY 载荷语义（成功带 type_key；失败带结构化错误）。
#[derive(Debug, Clone)]
pub enum ReplyPayload {
    Ok(bytes::Bytes, String),
    Err(crate::error::ErrCode, String),
}

pub struct CallbackRegistry {
    next_cid: AtomicU64,
    /// Logical destination and actual connection are distinct for hub routes.
    slots: Mutex<HashMap<u64, CallbackSlot>>,
    capacity: usize,
}

struct CallbackSlot {
    tx: tokio::sync::oneshot::Sender<ReplyPayload>,
    node: String,
    connection: Option<FrameSender>,
}

/// Network replies rejected because they do not belong to the recorded connection.
pub static WRONG_REPLY_SOURCE: AtomicU64 = AtomicU64::new(0);

/// Removes an outstanding callback on success, error, timeout, or cancellation.
pub(crate) struct PendingCallback<'a> {
    registry: &'a CallbackRegistry,
    cid: u64,
}

impl Drop for PendingCallback<'_> {
    fn drop(&mut self) {
        self.registry.remove(self.cid);
    }
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

    /// Register a locally completed callback. Network dispatch cannot complete
    /// unbound slots; use insert_on for actual remote requests.
    pub fn insert(
        &self,
        cid: u64,
        node: impl Into<String>,
        tx: tokio::sync::oneshot::Sender<ReplyPayload>,
    ) -> Result<(), RemoteError> {
        self.insert_slot(cid, node.into(), tx, None)
    }

    pub fn insert_on(
        &self,
        cid: u64,
        node: impl Into<String>,
        tx: tokio::sync::oneshot::Sender<ReplyPayload>,
        connection: &FrameSender,
    ) -> Result<(), RemoteError> {
        self.insert_slot(cid, node.into(), tx, Some(connection.clone()))
    }

    fn insert_slot(
        &self,
        cid: u64,
        node: String,
        tx: tokio::sync::oneshot::Sender<ReplyPayload>,
        connection: Option<FrameSender>,
    ) -> Result<(), RemoteError> {
        let mut slots = self.slots.lock().unwrap();
        if slots.contains_key(&cid) {
            return Err(RemoteError::Transport("duplicate correlation id".into()));
        }
        if slots.len() >= self.capacity {
            return Err(RemoteError::CallbacksFull(self.capacity));
        }
        // Under the same lock as fail_connection: no registration can slip in
        // after disconnect cleanup has already visited the table.
        if connection.as_ref().is_some_and(FrameSender::is_closed) {
            return Err(RemoteError::Transport("connection already closed".into()));
        }
        slots.insert(
            cid,
            CallbackSlot {
                tx,
                node,
                connection,
            },
        );
        Ok(())
    }

    pub(crate) fn guard(&self, cid: u64) -> PendingCallback<'_> {
        PendingCallback {
            registry: self,
            cid,
        }
    }

    pub fn remove(&self, cid: u64) -> bool {
        self.slots.lock().unwrap().remove(&cid).is_some()
    }

    /// Trusted local completion. Network ingress must use complete_from.
    pub fn complete(&self, cid: u64, payload: ReplyPayload) -> bool {
        if let Some(slot) = self.slots.lock().unwrap().remove(&cid) {
            let _ = slot.tx.send(payload);
            true
        } else {
            LATE_REPLY_DROPPED.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// Validate and consume atomically. A forged reply cannot steal a real slot.
    pub fn complete_from(&self, cid: u64, payload: ReplyPayload, from: &FrameSender) -> bool {
        let mut slots = self.slots.lock().unwrap();
        let Some(slot) = slots.get(&cid) else {
            LATE_REPLY_DROPPED.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if !slot
            .connection
            .as_ref()
            .is_some_and(|owner| owner.same_connection(from))
        {
            WRONG_REPLY_SOURCE.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let slot = slots.remove(&cid).expect("slot checked under lock");
        let _ = slot.tx.send(payload);
        true
    }

    pub fn fail_connection(
        &self,
        connection: &FrameSender,
        code: crate::error::ErrCode,
        detail: &str,
    ) -> usize {
        let mut slots = self.slots.lock().unwrap();
        let before = slots.len();
        let ids: Vec<_> = slots
            .iter()
            .filter(|(_, slot)| {
                slot.connection
                    .as_ref()
                    .is_some_and(|owner| owner.same_connection(connection))
            })
            .map(|(cid, _)| *cid)
            .collect();
        for cid in ids {
            let slot = slots.remove(&cid).expect("slot checked under lock");
            let _ = slot.tx.send(ReplyPayload::Err(code, detail.to_owned()));
        }
        before - slots.len()
    }

    /// 断连清空：所有挂起 ask 得 ConnectionLost（POC 实证路径——系统级关闭用）。
    pub fn fail_all(&self, code: crate::error::ErrCode, detail: &str) -> usize {
        let mut n = 0;
        let mut g = self.slots.lock().unwrap();
        for (_, slot) in g.drain() {
            let _ = slot.tx.send(ReplyPayload::Err(code, detail.to_string()));
            n += 1;
        }
        n
    }

    /// Explicit node-wide administrative failure. Not a connection-close hook.
    pub fn fail_node(&self, node: &str, code: crate::error::ErrCode, detail: &str) -> usize {
        let mut n = 0;
        let mut g = self.slots.lock().unwrap();
        let cids: Vec<u64> = g
            .iter()
            .filter(|(_, slot)| slot.node == node)
            .map(|(cid, _)| *cid)
            .collect();
        for cid in cids {
            if let Some(slot) = g.remove(&cid) {
                let _ = slot.tx.send(ReplyPayload::Err(code, detail.to_string()));
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
        assert!(matches!(
            r.insert(3, "peer", tx3),
            Err(RemoteError::CallbacksFull(2))
        ));
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
            assert!(matches!(
                got,
                ReplyPayload::Err(crate::error::ErrCode::ConnectionLost, _)
            ));
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
        assert!(!r.complete(
            999,
            ReplyPayload::Err(crate::error::ErrCode::Stopped, "late".into())
        ));
        let after = LATE_REPLY_DROPPED.load(Ordering::Relaxed);
        assert_eq!(
            after,
            before + 1,
            "late_reply_dropped_total must increment (E1.10)"
        );
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

    fn connection(
        name: &str,
    ) -> (
        FrameSender,
        tokio::sync::mpsc::Receiver<crate::frame::Frame>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        (FrameSender::new(tx, name), rx)
    }
    #[test]
    fn bound_slots_use_physical_writer_for_completion_and_failure() {
        let r = CallbackRegistry::new(8);
        let (a, _a_rx) = connection("same-hub");
        let (b, _b_rx) = connection("same-hub");
        let (tx, rx) = tokio::sync::oneshot::channel();
        r.insert_on(1, "logical-destination", tx, &a).unwrap();
        let reply = || ReplyPayload::Ok(bytes::Bytes::from_static(b"value"), "key".into());
        assert!(!r.complete_from(1, reply(), &b));
        assert_eq!(r.len(), 1);
        assert_eq!(
            r.fail_connection(&b, crate::ErrCode::ConnectionLost, "wrong"),
            0
        );
        assert!(r.complete_from(1, reply(), &a));
        r.fail_connection(&a, crate::ErrCode::ConnectionLost, "later close");
        assert!(matches!(
            futures::executor::block_on(rx).unwrap(),
            ReplyPayload::Ok(_, _)
        ));
    }

    #[test]
    fn duplicate_id_cannot_replace_pending_callback() {
        let r = CallbackRegistry::new(2);
        let (a, _a_rx) = connection("peer");
        let (tx, rx) = tokio::sync::oneshot::channel();
        r.insert_on(1, "dest", tx, &a).unwrap();
        let (replacement, _) = tokio::sync::oneshot::channel();
        assert!(r.insert_on(1, "other", replacement, &a).is_err());
        assert_eq!(r.len(), 1);
        r.fail_connection(&a, crate::ErrCode::ConnectionLost, "closed");
        assert!(matches!(
            futures::executor::block_on(rx).unwrap(),
            ReplyPayload::Err(crate::ErrCode::ConnectionLost, _)
        ));
    }

    #[test]
    fn network_cannot_complete_unbound_local_slot() {
        let r = CallbackRegistry::new(2);
        let (connection, _rx) = connection("peer");
        let (tx, rx) = tokio::sync::oneshot::channel();
        r.insert(1, "peer", tx).unwrap();
        let reply = || ReplyPayload::Err(crate::ErrCode::Stopped, "local".into());
        assert!(!r.complete_from(1, reply(), &connection));
        assert_eq!(r.len(), 1);
        assert!(r.complete(1, reply()));
        assert!(futures::executor::block_on(rx).is_ok());
    }

    #[test]
    fn closed_writer_cannot_be_registered_after_disconnect_cleanup() {
        let r = CallbackRegistry::new(2);
        let (connection, _rx) = connection("peer");
        let (tx, rx) = tokio::sync::oneshot::channel();
        r.insert_on(1, "dest", tx, &connection).unwrap();
        connection.close();
        assert_eq!(
            r.fail_connection(&connection, crate::ErrCode::ConnectionLost, "closed"),
            1
        );
        assert!(matches!(
            futures::executor::block_on(rx).unwrap(),
            ReplyPayload::Err(crate::ErrCode::ConnectionLost, _)
        ));
        let (late, _) = tokio::sync::oneshot::channel();
        assert!(r.insert_on(2, "dest", late, &connection).is_err());
        assert!(r.is_empty());
    }
}
