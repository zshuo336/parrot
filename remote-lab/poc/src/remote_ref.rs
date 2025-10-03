//! RemoteActorRef（05 §5）：ActorRef trait 的远程实现 = 位置透明核心。
//! ask = cid + oneshot 挂起 + REPLY 回程；deliver = TELL 帧 at-most-once。

use crate::codec::CodecRegistry;
use crate::frame::{frame_type, Frame};
use crate::transport::FrameSender;
use async_trait::async_trait;
use parrot_api::address::ActorRef;
use parrot_api::errors::ActorError;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// cid → oneshot 回调表（05 §6.2）。
pub struct CallbackRegistry {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Frame>>>,
}

impl CallbackRegistry {
    pub fn new() -> Self {
        Self { next: AtomicU64::new(1), pending: Mutex::new(HashMap::new()) }
    }
    pub fn next_cid(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
    pub fn insert(&self, cid: u64, tx: tokio::sync::oneshot::Sender<Frame>) {
        self.pending.lock().unwrap().insert(cid, tx);
    }
    pub fn remove(&self, cid: u64) {
        self.pending.lock().unwrap().remove(&cid);
    }
    /// REPLY 到达：完成挂起的 ask。返回 false = 迟到回复（调用方已超时放弃）。
    pub fn complete(&self, f: Frame) -> bool {
        if let Some(tx) = self.pending.lock().unwrap().remove(&f.correlation_id) {
            tx.send(f).is_ok()
        } else {
            false
        }
    }
    pub fn fail_all(&self, reason: &str) -> usize {
        let mut n = 0;
        let mut g = self.pending.lock().unwrap();
        for (_, tx) in g.drain() {
            let _ = tx.send(Frame::reply_err(0, reason));
            n += 1;
        }
        n
    }
}

impl std::fmt::Debug for RemoteActorRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteActorRef").field("path", &self.path).finish()
    }
}

/// 远程 ref：持有目标路径 + 出站帧通道 + 回调表。
pub struct RemoteActorRef {
    path: String,
    sender: FrameSender,
    callbacks: Arc<CallbackRegistry>,
}

impl RemoteActorRef {
    pub fn new(path: impl Into<String>, sender: FrameSender, callbacks: Arc<CallbackRegistry>) -> Self {
        Self { path: path.into(), sender, callbacks }
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
            // 1. 编码出口（NotRemotable 在此立即失败，不发帧）
            let (key, payload) = CodecRegistry::encode_outgoing(&msg)
                .map_err(|e| ActorError::MessageHandlingError(e))?;
            // 2. 注册回调 + 发帧
            let cid = self.callbacks.next_cid();
            let (tx, rx) = tokio::sync::oneshot::channel::<Frame>();
            self.callbacks.insert(cid, tx);
            self.sender
                .send(Frame::ask(cid, &self.path, key, payload))
                .await
                .map_err(|e| {
                    self.callbacks.remove(cid);
                    ActorError::InternalError(format!("remote send: {e}"))
                })?;
            // 3. 等回包（无界 / 带超时——ADR-10 语义）
            match timeout {
                None => match rx.await {
                    Ok(f) => reply_to_result(f),
                    Err(_) => Err(ActorError::InternalError("connection lost".into())),
                },
                Some(d) => match tokio::time::timeout(d, rx).await {
                    Ok(Ok(f)) => reply_to_result(f),
                    Ok(Err(_)) => Err(ActorError::InternalError("connection lost".into())),
                    Err(_) => {
                        // 调用方放弃：清回调；迟到 REPLY 将查表 miss 被 drop
                        self.callbacks.remove(cid);
                        Err(ActorError::TimeoutDetail(format!("remote ask {} after {:?}", self.path, d)))
                    }
                },
            }
        })
    }

    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let (key, payload) = CodecRegistry::encode_outgoing(&msg)
                .map_err(ActorError::MessageHandlingError)?;
            self.sender
                .send(Frame::tell(&self.path, key, payload))
                .await
                .map_err(|e| ActorError::InternalError(format!("remote deliver: {e}")))
        })
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.sender
                .send(Frame { frame_type: frame_type::STOP, correlation_id: 0, path: self.path.clone(), type_key: String::new(), payload: bytes::Bytes::new() })
                .await
                .map_err(|e| ActorError::InternalError(format!("remote stop: {e}")))
        })
    }

    fn path(&self) -> String {
        self.path.clone()
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        // POC：连接可发送即视为活（弱一致，文档语义）
        Box::pin(async move { !self.sender.tx.is_closed() })
    }

    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self {
            path: self.path.clone(),
            sender: self.sender.clone(),
            callbacks: self.callbacks.clone(),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// REPLY/REPLY_ERR → ActorResult。
fn reply_to_result(f: Frame) -> ActorResult<BoxedMessage> {
    match f.frame_type {
        frame_type::REPLY => {
            let msg = CodecRegistry::decode_incoming(&f.type_key, &f.payload)
                .map_err(ActorError::MessageHandlingError)?;
            Ok(msg)
        }
        frame_type::REPLY_ERR => {
            let text = String::from_utf8_lossy(&f.payload).to_string();
            // 映射 05 §9 错误表
            if text.contains("ActorNotFound") || text.contains("not found") {
                Err(ActorError::ActorNotFound(text))
            } else if text.contains("Timeout") {
                Err(ActorError::TimeoutDetail(text))
            } else if text.contains("stopped") {
                Err(ActorError::Stopped)
            } else {
                Err(ActorError::MessageHandlingError(text))
            }
        }
        other => Err(ActorError::InternalError(format!("expected REPLY, got {other:#x}"))),
    }
}
