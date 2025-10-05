//! 职责：RemoteActorRef——ActorRef trait 的远程实现（位置透明核心，05 §5）。
//!
//! ask = cid + oneshot 挂起 + REPLY 回程；deliver = TELL 帧 at-most-once；
//! stop = STOP 帧火忘 + 本地 5s 兜底（P1 简化，DEV_01 §8.9）。

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parrot_api::address::ActorRef;
use parrot_api::errors::ActorError;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};

use crate::codec_registry::CodecRegistry;
use crate::error::ErrCode;
use crate::frame::Frame;
use crate::node::{NodeState, NodeStatus};
use crate::registry::{CallbackRegistry, ReplyPayload};
use crate::transport::FrameSender;

/// 远程 ref 共享内核。
pub struct RemoteInner {
    pub nodes: Vec<(String, FrameSender, Arc<NodeStatus>)>,
    pub callbacks: Arc<CallbackRegistry>,
    pub self_node: String,
    /// 星型拓扑默认路由（07 §6）：目标节点不在直连表时经 hub 中转。
    /// None = 无 uplink（纯直连模式——miss 即 fail）。
    pub uplink: Option<FrameSender>,
    /// 端到端 TELL 序号分配（per 目标节点单调递增，从 1 起；跨路径切换
    /// 连续——接收端据此重排。外层 Mutex 简单正确：临界区仅查表+自增，
    /// 无 await；高并发 TELL 热点可换 sharded，当前压测未见瓶颈）。
    pub(crate) seq_counters: std::sync::Mutex<std::collections::HashMap<String, u32>>,
}

impl RemoteInner {
    /// 取目标节点下一序号（首帧 =1；u32 回绕在 2^32 帧后——接收端
    /// expected 同步回绕，语义仍单调）。
    pub fn next_seq(&self, node: String) -> u32 {
        let mut g = self.seq_counters.lock().unwrap();
        let c = g.entry(node).or_insert(0u32);
        *c = c.wrapping_add(1);
        if *c == 0 {
            *c = 1; // 回绕跳过 0 哨兵
        }
        *c
    }
    /// 出站链路：按 node_id 查（P1 单链路；fail-over 是 P2 重连任务职责）。
    fn sender_of(&self, node_id: &str) -> Option<&FrameSender> {
        self.nodes
            .iter()
            .find(|(n, _, _)| n == node_id)
            .map(|(_, s, _)| s)
            .or(self.uplink.as_ref())
    }
}

#[derive(Clone)]
pub struct RemoteActorRef {
    path: String,
    node_id: String,
    inner: Arc<RemoteInner>,
}

impl std::fmt::Debug for RemoteActorRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteActorRef")
            .field("path", &self.path)
            .field("node", &self.node_id)
            .finish()
    }
}

impl RemoteActorRef {
    pub fn new(
        path: impl Into<String>,
        node_id: impl Into<String>,
        inner: Arc<RemoteInner>,
    ) -> Self {
        Self {
            path: path.into(),
            node_id: node_id.into(),
            inner,
        }
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    fn encode_outgoing(&self, msg: &BoxedMessage) -> Result<(String, bytes::Bytes), ActorError> {
        let (key, payload) = CodecRegistry::global()
            .encode_outgoing(msg)
            .map_err(|c| c.to_actor_error(format!("type_id={:?}", (**msg).type_id())))?;
        Ok((key, bytes::Bytes::from(payload)))
    }

    fn reply_to_result(&self, payload: ReplyPayload) -> ActorResult<BoxedMessage> {
        match payload {
            ReplyPayload::Ok(bytes, key) => {
                let msg = CodecRegistry::global()
                    .decode_incoming(&key, &bytes)
                    .map_err(|c| c.to_actor_error(key.clone()))?;
                Ok(msg)
            }
            ReplyPayload::Err(code, detail) => Err(code.to_actor_error(detail)),
        }
    }
}

#[async_trait]
impl ActorRef for RemoteActorRef {
    fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send_with_timeout(msg, None)
    }

    fn send_with_timeout<'a>(
        &'a self,
        msg: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            // 1. 编码出口（NotRemotable 在此立即失败，不发帧——RC6）
            let (key, payload) = self.encode_outgoing(&msg)?;
            // 2. 回调注册（记录目标 node——单链路断连精准 fail）
            let cid = self.inner.callbacks.next_cid();
            let (tx, rx) = tokio::sync::oneshot::channel::<ReplyPayload>();
            self.inner
                .callbacks
                .insert(cid, self.node_id.clone(), tx)
                .map_err(|e| e.to_actor_error())?;
            // 3. 发帧（reply_to = 系统回程路径，05 §5.1）
            let sender = self
                .inner
                .sender_of(&self.node_id)
                .ok_or_else(|| {
                    self.inner.callbacks.remove(cid);
                    ActorError::InternalError(format!(
                        "remote link to {} unavailable",
                        self.node_id
                    ))
                })?
                .clone();
            let reply_to = format!("parrot://{}/_remote/reply", self.inner.self_node);
            let frame = Frame::ask(cid, &self.path, &key, payload, Some(&reply_to));
            if let Err(e) = sender.send(frame).await {
                self.inner.callbacks.remove(cid);
                return Err(ErrCode::ConnectionLost.to_actor_error(e.to_string()));
            }
            // 4. 等回包（无界 / 带超时——ADR-10）
            match timeout {
                None => match rx.await {
                    Ok(p) => self.reply_to_result(p),
                    Err(_) => Err(ErrCode::ConnectionLost.to_actor_error("connection lost".into())),
                },
                Some(d) => match tokio::time::timeout(d, rx).await {
                    Ok(Ok(p)) => self.reply_to_result(p),
                    Ok(Err(_)) => {
                        Err(ErrCode::ConnectionLost.to_actor_error("connection lost".into()))
                    }
                    Err(_) => {
                        // 调用方放弃：清回调；迟到 REPLY 查表 miss → metric drop（RC4）
                        self.inner.callbacks.remove(cid);
                        Err(ActorError::TimeoutDetail(format!(
                            "remote ask {} after {:?}",
                            self.path, d
                        )))
                    }
                },
            }
        })
    }

    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let (key, payload) = self.encode_outgoing(&msg)?;
            let sender = self.inner.sender_of(&self.node_id).ok_or_else(|| {
                ActorError::InternalError(format!("remote link to {} unavailable", self.node_id))
            })?;
            // 端到端 seq（per 目标节点单调递增）：接收端 TELL 重排用。
            // 分配与入队同线程紧邻（先取号再 send），原子计数保证唯一性；
            // 极端并发下 33 先于 32 入队由接收端重排兜底。
            let seq = self.inner.next_seq(self.node_id.clone());
            sender
                .send(Frame::tell_with_seq(&self.path, &key, payload, seq))
                .await
                .map_err(|e| ErrCode::ConnectionLost.to_actor_error(e.to_string()))?;
            Ok(())
        })
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let sender = self.inner.sender_of(&self.node_id).ok_or_else(|| {
                ActorError::InternalError(format!("remote link to {} unavailable", self.node_id))
            })?;
            sender
                .send(Frame::stop(&self.path))
                .await
                .map_err(|e| ErrCode::ConnectionLost.to_actor_error(e.to_string()))?;
            Ok(())
        })
    }

    fn path(&self) -> String {
        self.path.clone()
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        // 弱一致：NodeTable 状态 == Connected（DEV_01 §8.9 明示）
        Box::pin(async move {
            self.inner
                .nodes
                .iter()
                .find(|(n, _, _)| n == &self.node_id)
                .map(|(_, _, st)| st.get() == NodeState::Connected)
                .unwrap_or(false)
        })
    }

    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_inner_with_sender() -> (Arc<RemoteInner>, tokio::sync::mpsc::Receiver<Frame>) {
        let (tx, rx) = tokio::sync::mpsc::channel::<Frame>(16);
        let status = Arc::new(NodeStatus::default());
        status.set(NodeState::Connected);
        (
            Arc::new(RemoteInner {
                nodes: vec![("n1".into(), crate::transport::FrameSender::anon(tx), status)],
                callbacks: Arc::new(CallbackRegistry::new(8)),
                self_node: "self".into(),
                uplink: None,
                seq_counters: Default::default(),
            }),
            rx,
        )
    }

    // 纯本地单测：NotRemotable 不发帧（RC6 语义的 registry 侧半边）
    #[tokio::test]
    async fn ref_notremotable_local() {
        let inner = Arc::new(RemoteInner {
            nodes: Vec::new(),
            callbacks: Arc::new(CallbackRegistry::new(8)),
            self_node: "test".into(),
            uplink: None,
            seq_counters: Default::default(),
        });
        let r = RemoteActorRef::new("/x", "n", inner);
        #[derive(Debug)]
        struct Secret;
        let err = r.send(Box::new(Secret)).await.unwrap_err();
        assert!(err.to_string().contains("not remotable"), "got {err:?}");
        // 回调表零残留（未发帧）
        assert!(r.inner.callbacks.is_empty());
    }

    // ASK 无链路 → InternalError（send/deliver/stop 三入口）
    #[tokio::test]
    async fn ref_no_link_errors() {
        let inner = Arc::new(RemoteInner {
            nodes: Vec::new(),
            callbacks: Arc::new(CallbackRegistry::new(8)),
            self_node: "test".into(),
            uplink: None,
            seq_counters: Default::default(),
        });
        let r = RemoteActorRef::new("/x", "ghost", inner.clone());
        #[derive(Debug, serde::Serialize, serde::Deserialize)]
        struct M(u32);
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: "bin:parrot_remote::M#v1",
                type_id: std::any::TypeId::of::<M>(),
                encode: |msg: &BoxedMessage| {
                    let m = msg.downcast_ref::<M>().ok_or("downcast M")?;
                    parrot_api::message::serde_remote_serialize(&m)
                },
                decode: |b: &[u8]| {
                    let v: M = parrot_api::message::serde_remote_deserialize(b)?;
                    Ok(Box::new(v) as BoxedMessage)
                },
            }
        }
        let err = r.send(Box::new(M(1))).await.unwrap_err();
        assert!(matches!(err, ActorError::InternalError(_)), "got {err:?}");
        let err = r.deliver(Box::new(M(2))).await.unwrap_err();
        assert!(matches!(err, ActorError::InternalError(_)));
        let err = r.stop().await.unwrap_err();
        assert!(matches!(err, ActorError::InternalError(_)));
        assert!(r.inner.callbacks.is_empty());
    }

    // TELL 成功出帧 + STOP 出帧 + is_alive 弱一致
    #[tokio::test]
    async fn ref_tell_stop_wire() {
        let (inner, mut rx) = test_inner_with_sender();
        let r = RemoteActorRef::new("/x", "n1", inner);
        assert!(r.is_alive().await);
        r.deliver(Box::new(crate::ingress::TestAsk(5)))
            .await
            .unwrap();
        let f = rx.recv().await.unwrap();
        assert_eq!(f.header.frame_type, crate::frame::frame_type::TELL);
        assert_eq!(f.path, "/x");
        r.stop().await.unwrap();
        let f = rx.recv().await.unwrap();
        assert_eq!(f.header.frame_type, crate::frame::frame_type::STOP);
    }

    // ASK 带超时：超时 → TimeoutDetail + 回调清除 + 迟到 REPLY 计数不炸
    #[tokio::test]
    async fn ref_ask_timeout_cleans_callback() {
        let (inner, mut rx) = test_inner_with_sender();
        let r = RemoteActorRef::new("/x", "n1", inner.clone());
        let err = r
            .send_with_timeout(
                Box::new(crate::ingress::TestAsk(1)),
                Some(Duration::from_millis(50)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ActorError::TimeoutDetail(_)), "got {err:?}");
        assert!(
            inner.callbacks.is_empty(),
            "callback not cleaned after timeout"
        );
        // 帧已发出
        assert_eq!(
            rx.recv().await.unwrap().header.frame_type,
            crate::frame::frame_type::ASK
        );
    }

    // 回调表容量满 → insert 失败 → InternalError（capacity=1 预占满）
    #[tokio::test]
    async fn ref_ask_capacity_reject() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(16);
        let status = Arc::new(NodeStatus::default());
        let inner = Arc::new(RemoteInner {
            nodes: vec![("n1".into(), crate::transport::FrameSender::anon(tx), status)],
            callbacks: Arc::new(CallbackRegistry::new(1)),
            self_node: "self".into(),
            uplink: None,
            seq_counters: Default::default(),
        });
        // 预占满（cid=1000）
        let (otx, _orx) = tokio::sync::oneshot::channel();
        inner.callbacks.insert(1000, "n1", otx).unwrap();
        let r = RemoteActorRef::new("/x", "n1", inner);
        let err = r
            .send(Box::new(crate::ingress::TestAsk(1)))
            .await
            .unwrap_err();
        assert!(matches!(err, ActorError::InternalError(_)), "got {err:?}");
        // 不应有帧发出（insert 失败在 send 帧之前）
        assert!(rx.try_recv().is_err());
    }
}
