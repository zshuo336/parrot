//! 职责：入站帧分发——ASK/TELL/STOP → 本地 actor；REPLY → 回调表（05 §6.1）。
//!
//! P1 入站串行：每连接单任务顺序处理（HOL 是已知特性，P2 拆 worker 池
//! ——06 I2，文档明示不修补）。本地解析经 LocalLookup trait 倒置
//! （parrot-remote 不依赖 parrot crate，E5.2 分层铁律）。

use std::sync::Arc;

use crate::codec_registry::CodecRegistry;
use crate::error::{decode_err_payload, ErrCode};
use crate::frame::{frame_type, Frame};
use crate::node::NodeState;
use crate::registry::{CallbackRegistry, ReplyPayload};
use crate::transport::FrameSender;

/// 本地 actor 解析出口（trait 倒置：parrot facade 在应用层注入实现）。
#[async_trait::async_trait]
pub trait LocalLookup: Send + Sync + 'static {
    /// 路径 → 本地 ActorRef（miss 返回 None → REPLY_ERR ActorNotFound）。
    async fn lookup(&self, path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>>;
}

/// 死信计数（TELL miss 目标——无回程信道，只记 metric + debug 日志）。
pub static DEAD_TELL_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub struct Ingress {
    pub local: Arc<dyn LocalLookup>,
    pub callbacks: Arc<CallbackRegistry>,
    /// P2：SYSTEM_EVENT 分流钩子（admin/gossip/receptionist——system.rs 注入；
    /// None 时吞帧防御式丢弃）。
    sys_event: std::sync::RwLock<Option<Arc<dyn SysEventHook>>>,
}

impl Ingress {
    /// 构造（P1 兼容：无钩子）。
    pub fn new(local: Arc<dyn LocalLookup>, callbacks: Arc<CallbackRegistry>) -> Self {
        Self {
            local,
            callbacks,
            sys_event: std::sync::RwLock::new(None),
        }
    }

    /// 钩子注入/替换（system.rs install_admin_hook 用）。
    pub fn sys_event_slot(&self) -> std::sync::RwLockWriteGuard<'_, Option<Arc<dyn SysEventHook>>> {
        self.sys_event.write().unwrap()
    }
}

/// SYSTEM_EVENT 入站钩子（返回 false = 未处理，ingress 记日志）。
#[async_trait::async_trait]
pub trait SysEventHook: Send + Sync + 'static {
    async fn on_event(&self, event: crate::admin::SysEvent, back: &FrameSender, from: &str);
}

/// 帧路径 → 本地查找路径：剥 parrot://{node}/ 前缀（节点寻址已在连接层完成，
/// 本地表只存 /system/... 形态——05 §3.1 一文法两形态）。
fn local_path(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("parrot://") {
        if let Some(idx) = rest.find('/') {
            return &rest[idx..];
        }
    }
    path
}

impl Ingress {
    /// 处理一帧入站（来自 ConnectionTask 的 inbound 队列）。
    /// `back` 是该连接的回程发送端（REPLY 用）。
    pub async fn dispatch(&self, frame: Frame, back: &FrameSender, from_node: &str) {
        match frame.header.frame_type {
            frame_type::ASK => self.on_ask(frame, back).await,
            frame_type::TELL => self.on_tell(frame).await,
            frame_type::STOP => self.on_stop(frame).await,
            frame_type::REPLY | frame_type::REPLY_ERR => self.on_reply(frame),
            frame_type::SYSTEM_EVENT => {
                // P2 管理通道：admin/gossip/receptionist 同帧不同标签
                match crate::admin::decode_sys_event(&frame.payload) {
                    Ok(event) => {
                        let hook = self.sys_event.read().unwrap().clone();
                        if let Some(hook) = hook {
                            hook.on_event(event, back, from_node).await;
                        } else {
                            tracing::debug!("SYSTEM_EVENT ignored (no hook installed)");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(err = %e, "malformed SYSTEM_EVENT dropped");
                    }
                }
            }
            frame_type::ERROR => {
                let (code, detail) = decode_err_payload(&frame.payload)
                    .unwrap_or((ErrCode::ProtocolViolation, "<undecodable>".into()));
                tracing::warn!(node = %from_node, code = code.name(), %detail, "remote ERROR frame");
            }
            // 心跳/握手由 ConnectionTask 处理；集群帧在连接层已断连——防御式吞掉
            _ => {
                tracing::debug!(ft = frame.header.frame_type, "ingress: frame handled at conn layer");
            }
        }
    }

    async fn on_ask(&self, frame: Frame, back: &FrameSender) {
        let cid = frame.header.correlation_id;
        // 剥 reply_to 前缀（DEV_01 §3.1 约定）
        let (reply_to, payload) = match frame.split_reply_to() {
            Ok(v) => v,
            Err(e) => {
                let _ = back
                    .send(Frame::reply_err(
                        cid,
                        &frame.path,
                        ErrCode::ProtocolViolation,
                        &e.to_string(),
                    ))
                    .await;
                return;
            }
        };
        // 解码
        let decoded = match CodecRegistry::global().decode_incoming(&frame.type_key, &payload) {
            Ok(m) => m,
            Err(code) => {
                let _ = back
                    .send(Frame::reply_err(
                        cid,
                        &frame.path,
                        code,
                        &format!("type_key={}", frame.type_key),
                    ))
                    .await;
                return;
            }
        };
        // 本地解析（facade 注入）
        let local_ref = match self.local.lookup(local_path(&frame.path)).await {
            Some(r) => r,
            None => {
                let _ = back
                    .send(Frame::reply_err(
                        cid,
                        &frame.path,
                        ErrCode::ActorNotFound,
                        &frame.path,
                    ))
                    .await;
                return;
            }
        };
        // 远端不二次超时（07 §7 双超时竞态规避）：send() 无界。
        // panic 隔离：本地引擎 panic 不能杀分发循环（连接存活优先）
        let send_fut = local_ref.send(decoded);
        let outcome = std::panic::AssertUnwindSafe(send_fut);
        let outcome = futures::FutureExt::catch_unwind(outcome).await;
        let outcome: Result<parrot_api::types::BoxedMessage, parrot_api::errors::ActorError> = match outcome {
            Ok(r) => r,
            Err(p) => Err(parrot_api::errors::ActorError::Panic(
                p.downcast_ref::<&str>().map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "local actor panicked".into()),
            )),
        };
        match outcome {
            Ok(reply) => {
                // 回复类型编码（回复也必须 RemoteMessage）
                let (key, payload) = match CodecRegistry::global().encode_outgoing(&reply) {
                    Ok(v) => v,
                    Err(code) => {
                        let _ = back
                            .send(Frame::reply_err(
                                cid,
                                &frame.path,
                                code,
                                "reply type not remotable",
                            ))
                            .await;
                        return;
                    }
                };
                // REPLY.path = reply_to 回程路径（cid 配对走回调表）
                let path = reply_to.unwrap_or_default();
                let _ = back
                    .send(Frame::reply(cid, &path, &key, bytes::Bytes::from(payload)))
                    .await;
            }
            Err(e) => {
                let (code, detail) = ErrCode::from_actor_error(&e);
                let _ = back
                    .send(Frame::reply_err(
                        cid,
                        &frame.path,
                        code,
                        &detail,
                    ))
                    .await;
            }
        }
    }

    async fn on_tell(&self, frame: Frame) {
        let decoded = match CodecRegistry::global().decode_incoming(&frame.type_key, &frame.payload)
        {
            Ok(m) => m,
            Err(_) => {
                DEAD_TELL_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        if let Some(r) = self.local.lookup(local_path(&frame.path)).await {
            // deliver 挂起 = 读循环挂起 = 对端背压贯通（05 §6.3，RC8）
            let _ = r.deliver(decoded).await;
        } else {
            DEAD_TELL_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!(path = %frame.path, "TELL to unknown actor (dead letter)");
        }
    }

    async fn on_stop(&self, frame: Frame) {
        if let Some(r) = self.local.lookup(local_path(&frame.path)).await {
            let _ = r.stop().await;
        }
    }

    fn on_reply(&self, frame: Frame) {
        let cid = frame.header.correlation_id;
        match frame.header.frame_type {
            frame_type::REPLY => {
                self.callbacks
                    .complete(cid, ReplyPayload::Ok(frame.payload, frame.type_key));
            }
            _ => {
                let (code, detail) = decode_err_payload(&frame.payload)
                    .unwrap_or((ErrCode::ProtocolViolation, "<undecodable>".into()));
                self.callbacks.complete(cid, ReplyPayload::Err(code, detail));
            }
        }
    }
}

/// 断连清理钩子（RemoteActorSystem 注入 ConnectionTask）：
/// callbacks.fail_node(ConnectionLost)（单链路隔离）+ NodeTable → Disconnected。
pub fn make_on_disconnect(
    callbacks: Arc<CallbackRegistry>,
    statuses: Vec<Arc<crate::node::NodeStatus>>,
) -> Arc<dyn Fn(&str) + Send + Sync> {
    Arc::new(move |node_id: &str| {
        let n = callbacks.fail_node(node_id, ErrCode::ConnectionLost, "connection lost");
        if n > 0 {
            tracing::debug!(node = %node_id, pending = n, "callbacks failed on disconnect");
        }
        for st in &statuses {
            st.set(NodeState::Disconnected);
        }
    })
}

/// 测试消息（ingress 单测注册——全局 CodecRegistry 走 inventory）。
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TestAsk(pub u64);
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TestReply(pub u64);

#[cfg(test)]
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot_remote::TestAsk#v1",
        type_id: std::any::TypeId::of::<TestAsk>(),
        encode: |msg: &parrot_api::types::BoxedMessage| {
            let m = msg.downcast_ref::<TestAsk>().ok_or("downcast TestAsk")?;
            parrot_api::message::serde_remote_serialize(&m)
        },
        decode: |b: &[u8]| {
            let v: TestAsk = parrot_api::message::serde_remote_deserialize(b)?;
            Ok(Box::new(v) as parrot_api::types::BoxedMessage)
        },
    }
}

#[cfg(test)]
parrot_api::message::inventory::submit! {
    parrot_api::message::CodecRegistration {
        type_key: "bin:parrot_remote::TestReply#v1",
        type_id: std::any::TypeId::of::<TestReply>(),
        encode: |msg: &parrot_api::types::BoxedMessage| {
            let m = msg.downcast_ref::<TestReply>().ok_or("downcast TestReply")?;
            parrot_api::message::serde_remote_serialize(&m)
        },
        decode: |b: &[u8]| {
            let v: TestReply = parrot_api::message::serde_remote_deserialize(b)?;
            Ok(Box::new(v) as parrot_api::types::BoxedMessage)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::CallbackRegistry;
    use async_trait::async_trait;
    use bytes::Bytes;
    use parrot_api::address::ActorRef;
    use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
    use std::any::Any;

    /// 回声 ActorRef 桩（LocalLookup 测试实现）。
    #[derive(Debug)]
    struct EchoRef;

    #[async_trait]
    impl ActorRef for EchoRef {
        fn send<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.send_with_timeout(msg, None)
        }

        fn send_with_timeout<'a>(
            &'a self,
            msg: BoxedMessage,
            _timeout: Option<std::time::Duration>,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move { Ok(msg) })
        }
        fn deliver<'a>(&'a self, _msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move { Ok(()) })
        }
        fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move { Ok(()) })
        }
        fn path(&self) -> String {
            "/user/echo".into()
        }
        fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
            Box::pin(async move { true })
        }
        fn clone_boxed(&self) -> BoxedActorRef {
            Box::new(Self)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct TestLookup;

    #[async_trait]
    impl LocalLookup for TestLookup {
        async fn lookup(&self, path: &str) -> Option<Box<dyn ActorRef>> {
            if path == "/user/echo" {
                Some(Box::new(EchoRef))
            } else {
                None
            }
        }
    }

    #[test]
    fn local_path_strips_node_prefix() {
        assert_eq!(local_path("parrot://node-b/user/echo"), "/user/echo");
        assert_eq!(local_path("/user/echo"), "/user/echo");
    }

    fn ingress() -> (Arc<Ingress>, Arc<CallbackRegistry>) {
        let callbacks = Arc::new(CallbackRegistry::new(64));
        (
            Arc::new(Ingress::new(Arc::new(TestLookup), callbacks.clone())),
            callbacks,
        )
    }

    // ingress_dispatch_matrix：ASK 命中/ASK miss/REPLY 回调/REPLY_ERR/TELL miss 死信
    #[tokio::test]
    async fn ingress_dispatch_matrix() {
        let (ig, ch) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(16);
        let back = FrameSender { tx };

        // ① ASK miss → REPLY_ERR(ActorNotFound)（消息键已注册，decode 通过后 lookup miss）
        let ask_payload = {
            let (k, p) = crate::codec_registry::CodecRegistry::global()
                .encode_outgoing(&(Box::new(TestAsk(1)) as BoxedMessage))
                .unwrap();
            assert_eq!(k, "bin:parrot_remote::TestAsk#v1");
            Bytes::from(p)
        };
        ig.dispatch(
            Frame::ask(1, "/user/ghost", "bin:parrot_remote::TestAsk#v1", ask_payload.clone(), None),
            &back,
            "n1",
        )
        .await;
        let f = rx.recv().await.unwrap();
        assert_eq!(f.header.frame_type, frame_type::REPLY_ERR);
        let (code, detail) = decode_err_payload(&f.payload).unwrap();
        assert_eq!(code, ErrCode::ActorNotFound);
        assert_eq!(detail, "/user/ghost");

        // ② TELL miss → 死信计数
        let before = DEAD_TELL_DROPPED.load(std::sync::atomic::Ordering::Relaxed);
        ig.dispatch(
            Frame::tell("/user/ghost", "bin:parrot_remote::TestAsk#v1", ask_payload),
            &back,
            "n1",
        )
        .await;
        assert_eq!(
            DEAD_TELL_DROPPED.load(std::sync::atomic::Ordering::Relaxed),
            before + 1
        );

        // ③ REPLY → 回调表完成
        let (tx_cb, rx_cb) = tokio::sync::oneshot::channel();
        ch.insert(42, "n1", tx_cb).unwrap();
        ig.dispatch(
            Frame::reply(42, "", "bin:t::R", Bytes::from_static(b"ok")),
            &back,
            "n1",
        )
        .await;
        let got = rx_cb.await.unwrap();
        assert!(matches!(got, ReplyPayload::Ok(_, _)));

        // ④ REPLY_ERR → 回调表 Err
        let (tx_cb2, rx_cb2) = tokio::sync::oneshot::channel();
        ch.insert(43, "n1", tx_cb2).unwrap();
        ig.dispatch(
            Frame::reply_err(43, "", ErrCode::Stopped, "stopped"),
            &back,
            "n1",
        )
        .await;
        let got2 = rx_cb2.await.unwrap();
        assert!(matches!(got2, ReplyPayload::Err(ErrCode::Stopped, _)));
    }

    // make_on_disconnect：fail_all + 状态置 Disconnected
    #[tokio::test]
    async fn on_disconnect_hook_fails_all_and_flags_nodes() {
        let callbacks = Arc::new(CallbackRegistry::new(64));
        let (tx, rx) = tokio::sync::oneshot::channel();
        callbacks.insert(9, "n1", tx).unwrap();
        let st = Arc::new(crate::node::NodeStatus::default());
        st.set(crate::node::NodeState::Connected);
        let hook = make_on_disconnect(callbacks.clone(), vec![st.clone()]);
        hook("n1");
        let got = rx.await.unwrap();
        assert!(matches!(got, ReplyPayload::Err(ErrCode::ConnectionLost, _)));
        assert_eq!(st.get(), crate::node::NodeState::Disconnected);
    }

    // ERROR 帧入站 → 仅日志（不 panic、不回帧）
    #[tokio::test]
    async fn ingress_error_frame_swallowed() {
        let (ig, _cb) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender { tx };
        ig.dispatch(
            Frame::error_frame(ErrCode::Overloaded, "busy"),
            &back,
            "n1",
        )
        .await;
        // 无回帧
        assert!(rx.try_recv().is_err());
    }

    // 握手帧直入 ingress（防御式吞掉——正常路径连接层拦截）
    #[tokio::test]
    async fn ingress_handshake_frame_swallowed() {
        let (ig, _cb) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender { tx };
        ig.dispatch(Frame::heartbeat(), &back, "n1").await;
        assert!(rx.try_recv().is_err());
    }

    // ASK payload 畸形（reply_to 前缀坏）→ REPLY_ERR(ProtocolViolation)
    #[tokio::test]
    async fn ingress_ask_malformed_prefix() {
        // 直接验证 split 失败分支（Frame::ask 总是合法前置——构造裸 payload）
        let mut f = Frame::ask(3, "/user/echo", "bin:parrot_remote::TestAsk#v1", Bytes::new(), None);
        f.payload = Bytes::from_static(b"ab"); // < 4B → MalformedLengths
        assert!(f.split_reply_to().is_err());
        // dispatch 走同分支：回 REPLY_ERR(ProtocolViolation)
        let (ig, _cb) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(4);
        let back = FrameSender { tx };
        ig.dispatch(f, &back, "n1").await;
        let got = rx.recv().await.unwrap();
        assert_eq!(got.header.frame_type, frame_type::REPLY_ERR);
        let (code, _) = decode_err_payload(&got.payload).unwrap();
        assert_eq!(code, ErrCode::ProtocolViolation);
    }

    // local_path 边界：parrot:// 无斜杠尾 → 原样返回
    #[test]
    fn local_path_edge_cases() {
        assert_eq!(local_path("parrot://node-only"), "parrot://node-only");
        assert_eq!(local_path(""), "");
    }

    // ASK 解码失败（未知键）→ REPLY_ERR(UnknownTypeKey)
    #[tokio::test]
    async fn ingress_ask_unknown_key() {
        let (ig, _) = ingress();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(16);
        let back = FrameSender { tx };
        ig.dispatch(
            Frame::ask(7, "/user/echo", "bin:nowhere::Z#v1", Bytes::new(), None),
            &back,
            "n1",
        )
        .await;
        let f = rx.recv().await.unwrap();
        let (code, _) = decode_err_payload(&f.payload).unwrap();
        assert_eq!(code, ErrCode::UnknownTypeKey);
    }
}
