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
use parrot_api::message::MessageOptions;
use parrot_api::types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage};
use tracing::Instrument;

use crate::codec_registry::CodecRegistry;
use crate::durable::timing::{Phase, TARGET};
use crate::error::ErrCode;
use crate::frame::Frame;
use crate::node::NodeStatus;
use crate::registry::{CallbackRegistry, ReplyPayload};
use crate::transport::FrameSender;

/// Own the caller's message before the future can be cancelled. A borrowed
/// codec may unwind while this independent owner retires; do not unwind twice.
struct OutboundInput(Option<BoxedMessage>);
impl OutboundInput {
    fn value(&self) -> &BoxedMessage {
        self.0.as_ref().expect("live remote input")
    }
    fn retire(&mut self) -> Result<(), ()> {
        crate::ingress::unwind::Cleanup::default().call(|| drop(self.0.take()))
    }
    fn finish<T>(mut self, result: ActorResult<T>) -> ActorResult<T> {
        let cleanup = self.retire();
        match (result, cleanup) {
            (Err(error), _) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
            (Ok(value), Err(())) => {
                // A decoded reply can itself have a throwing destructor.
                crate::ingress::unwind::Cleanup::default().dispose(value);
                Err(ActorError::Panic(
                    "remote outgoing input destructor panicked".into(),
                ))
            }
        }
    }
}
impl Drop for OutboundInput {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

/// 远程 ref 共享内核。
pub struct RemoteInner {
    pub(crate) system: Option<std::sync::Weak<crate::system::RemoteActorSystem>>,
    pub nodes: Vec<(String, FrameSender, Arc<NodeStatus>)>,
    pub callbacks: Arc<CallbackRegistry>,
    pub self_node: String,
    /// 星型拓扑默认路由（07 §6）：目标节点不在直连表时经 hub 中转。
    /// None = 无 uplink（纯直连模式——miss 即 fail）。
    pub uplink: Option<FrameSender>,
    /// 端到端 TELL 序号分配（per 目标节点单调递增，从 1 起；跨路径切换
    /// 连续——接收端据此重排。外层 Mutex 简单正确：临界区仅查表+自增，
    /// 无 await；高并发 TELL 热点可换 sharded，当前压测未见瓶颈）。
    pub(crate) seq_counters: Arc<std::sync::Mutex<std::collections::HashMap<String, u32>>>,
}

impl RemoteInner {
    fn outgoing(&self, mut frame: Frame) -> Frame {
        frame.header.hop_limit = self
            .system
            .as_ref()
            .and_then(|system| system.upgrade())
            .map(|system| system.config.knobs.unwrap_or_default().default_hop_limit)
            .unwrap_or(crate::frame::DEFAULT_HOP_LIMIT);
        frame
    }

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
    fn sender_of(&self, node_id: &str) -> Option<FrameSender> {
        if let Some(system) = &self.system {
            return system.upgrade()?.sender_of(node_id);
        }
        self.nodes
            .iter()
            .find(|(n, sender, _)| n == node_id && !sender.is_closed())
            .map(|(_, s, _)| s.clone())
            .or_else(|| self.uplink.clone().filter(|s| !s.is_closed()))
    }
    async fn ensure_sender(&self, node_id: &str) -> Option<FrameSender> {
        if let Some(system) = &self.system {
            crate::system::RemoteActorSystem::sender_for(system, node_id).await
        } else {
            self.sender_of(node_id)
        }
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

struct ReceiptExpectation<'a> {
    required: u32,
    key: &'a str,
    payload: &'a [u8],
    shadow: bool,
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

    /// Wait for the remote handler to complete successfully, without requiring a
    /// serializable business reply. Each hop must advertise TELL_COMPLETION.
    /// The deadline covers queue admission and the return path. Cancellation,
    /// timeout or disconnection leave execution uncertain; no retry, WAL write,
    /// deduplication, cumulative ACK or exactly-once guarantee is implied.
    /// Captures the caller's explicit trace context when this future is created,
    /// including None. Encoding and owned input cleanup retain that identity.
    pub fn deliver_with_ack(
        &self,
        msg: BoxedMessage,
        timeout: Duration,
    ) -> impl std::future::Future<Output = ActorResult<()>> + Send + '_ {
        let mut input = OutboundInput(Some(msg));
        parrot_api::trace::capture(async move {
            let result = async {
                let input = &mut input;
                let deadline = tokio::time::Instant::now()
                    .checked_add(timeout)
                    .filter(|_| !timeout.is_zero())
                    .ok_or_else(|| {
                        ActorError::InternalError("invalid completion deadline".into())
                    })?;
                let (key, payload) = self.encode_outgoing(input.value())?;
                self.deliver_encoded_before(&key, payload, deadline).await
            }
            .await;
            input.finish(result)
        })
    }

    // WAL replay must preserve original bytes and must not require a local decoder.
    pub(crate) async fn deliver_encoded_before(
        &self,
        key: &str,
        payload: bytes::Bytes,
        deadline: tokio::time::Instant,
    ) -> ActorResult<()> {
        self.deliver_receipted_before(
            key,
            payload,
            deadline,
            ReceiptExpectation {
                required: crate::handshake::caps::TELL_COMPLETION,
                key: crate::frame::TELL_COMPLETION_KEY,
                payload: &[],
                shadow: false,
            },
        )
        .await
    }

    /// One best-effort direct shadow attempt. The distinct receipt proves raw
    /// mailbox admission. Cancellation/timeout cannot retract bytes already sent.
    pub(crate) async fn deliver_shadow_before(
        &self,
        key: &str,
        payload: &[u8],
        deadline: tokio::time::Instant,
    ) -> ActorResult<()> {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(ActorError::Timeout)?;
        let millis = remaining.as_millis().clamp(1, 60_000) as u32;
        let mut encoded = bytes::BytesMut::with_capacity(4 + payload.len());
        encoded.extend_from_slice(&millis.to_le_bytes());
        encoded.extend_from_slice(payload);
        self.deliver_receipted_before(
            key,
            encoded.freeze(),
            deadline,
            ReceiptExpectation {
                required: crate::handshake::caps::TELL_COMPLETION
                    | crate::handshake::caps::MIRROR_SHADOW,
                key: crate::frame::MIRROR_ADMISSION_KEY,
                payload: &[],
                shadow: true,
            },
        )
        .await
    }

    pub(crate) async fn deliver_durable_before(
        &self,
        envelope: &crate::durable::DurableEnvelope,
        deadline: tokio::time::Instant,
    ) -> ActorResult<()> {
        let payload = envelope
            .encode()
            .map_err(ActorError::MessageHandlingError)?;
        let span = tracing::trace_span!(target: TARGET, "durable.request",
            source = %envelope.source, session = %envelope.session,
            receiver = %envelope.receiver, path = %envelope.path, sequence = envelope.sequence);
        self.deliver_receipted_before(
            crate::durable::DURABLE_KEY,
            payload,
            deadline,
            ReceiptExpectation {
                required: crate::handshake::caps::TELL_COMPLETION
                    | crate::handshake::caps::DURABLE_DELIVERY,
                key: crate::durable::DURABLE_RECEIPT_KEY,
                payload: &envelope.sequence.to_le_bytes(),
                shadow: false,
            },
        )
        .instrument(span)
        .await
    }

    async fn deliver_receipted_before(
        &self,
        key: &str,
        payload: bytes::Bytes,
        deadline: tokio::time::Instant,
        receipt: ReceiptExpectation<'_>,
    ) -> ActorResult<()> {
        if tokio::time::Instant::now() >= deadline {
            return Err(ActorError::Timeout);
        }
        let sequence = (receipt.key == crate::durable::DURABLE_RECEIPT_KEY)
            .then(|| receipt.payload.try_into().ok().map(u64::from_le_bytes))
            .flatten();
        let span = tracing::trace_span!(target: TARGET, "completion.request",
            source = %self.inner.self_node, receiver = %self.node_id, path = %self.path,
            sequence, cid = tracing::field::Empty);
        let total = Phase::new(span.clone(), "completion.total");
        let request = async {
            let connection = Phase::new(span.clone(), "completion.connection");
            let sender = if receipt.shadow {
                if let Some(system) = &self.inner.system {
                    crate::system::RemoteActorSystem::resolve_sender(system, &self.node_id, false)
                        .await
                } else {
                    self.inner
                        .nodes
                        .iter()
                        .find(|(node, sender, _)| node == &self.node_id && !sender.is_closed())
                        .map(|(_, sender, _)| sender.clone())
                }
            } else {
                self.inner.ensure_sender(&self.node_id).await
            };
            connection.finish(sender.is_some());
            let sender = sender.ok_or_else(|| {
                ErrCode::ConnectionLost.to_actor_error("completion link unavailable".into())
            })?;
            if sender.peer_capabilities() & receipt.required != receipt.required {
                return Err(ErrCode::ProtocolViolation
                    .to_actor_error("peer does not support completion TELL".into()));
            }
            let cid = self.inner.callbacks.next_cid();
            span.record("cid", cid);
            let (tx, rx) = tokio::sync::oneshot::channel::<ReplyPayload>();
            self.inner
                .callbacks
                .insert_on(cid, self.node_id.clone(), tx, &sender)
                .map_err(|e| e.to_actor_error())?;
            let _pending = self.inner.callbacks.guard(cid);
            let sending = Phase::new(span.clone(), "completion.queue_admission");
            let mut frame = self
                .inner
                .outgoing(Frame::tell_with_ack(cid, &self.path, key, payload));
            if receipt.shadow {
                frame.header.flags |= crate::frame::flags::MIRROR_SHADOW;
            }
            let frame = sender
                .trace_request(frame)
                .map_err(|e| e.to_actor_error())?;
            let sent = sender.send(frame).await;
            sending.finish(sent.is_ok());
            sent.map_err(|e| e.to_actor_error())?;
            let waiting = Phase::new(span.clone(), "completion.reply_wait");
            let reply = tokio::select! {
                biased;
                reply = rx => reply.map_err(|_| ErrCode::ConnectionLost.to_actor_error("connection lost".into())),
                _ = sender.closed() => Err(ErrCode::ConnectionLost.to_actor_error("connection lost".into())),
            };
            waiting.finish(reply.is_ok());
            match reply? {
                ReplyPayload::Ok(bytes, key)
                    if key == receipt.key && bytes.as_ref() == receipt.payload =>
                {
                    Ok(())
                }
                ReplyPayload::Ok(_, _) => {
                    Err(ErrCode::ProtocolViolation
                        .to_actor_error("invalid completion receipt".into()))
                }
                ReplyPayload::Err(code, detail) => Err(code.to_actor_error(detail)),
            }
        };
        let result = tokio::time::timeout_at(deadline, request)
            .await
            .unwrap_or_else(|_| {
                Err(ActorError::TimeoutDetail(format!(
                    "completion TELL {} deadline exceeded; execution may have occurred",
                    self.path
                )))
            });
        total.finish(result.is_ok());
        result
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
        self.send_with_options(
            msg,
            MessageOptions {
                timeout,
                ..Default::default()
            },
        )
    }

    fn send_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let mut input = OutboundInput(Some(msg));
        Box::pin(parrot_api::trace::capture(async move {
            let result = async {
                let input = &mut input;
            options.validate_single_attempt()?;
            let started = tokio::time::Instant::now();
            let timeout = options.timeout;
            let urgent = options.priority.value() >= 70;
            let (key, payload) = self.encode_outgoing(input.value())?;
            // Encoding is synchronous. Once it returns, an exhausted budget
            // must fail before callback allocation or transport admission.
            let timeout = remaining_budget(started, timeout)?;
            let request = async {
                let sender = self
                    .inner
                    .ensure_sender(&self.node_id)
                    .await
                    .ok_or_else(|| {
                        ActorError::InternalError(format!(
                            "remote link to {} unavailable",
                            self.node_id
                        ))
                    })?;
                let cid = self.inner.callbacks.next_cid();
                let (tx, rx) = tokio::sync::oneshot::channel::<ReplyPayload>();
                self.inner
                    .callbacks
                    .insert_on(cid, self.node_id.clone(), tx, &sender)
                    .map_err(|e| e.to_actor_error())?;
                let _pending = self.inner.callbacks.guard(cid);
                let reply_to = format!("parrot://{}/_remote/reply", self.inner.self_node);
                let mut frame = self.inner.outgoing(Frame::ask(
                    cid,
                    &self.path,
                    &key,
                    payload,
                    Some(&reply_to),
                ));
                if urgent {
                    frame.header.flags |= crate::frame::flags::URGENT;
                }
                let frame = sender.trace_request(frame).map_err(|e| e.to_actor_error())?;
                sender.send(frame).await.map_err(|e| e.to_actor_error())?;
                let reply = tokio::select! {
                    biased;
                    reply = rx => reply.map_err(|_| ErrCode::ConnectionLost.to_actor_error("connection lost".into()))?,
                    _ = sender.closed() => return Err(ErrCode::ConnectionLost.to_actor_error("connection lost".into())),
                };
                self.reply_to_result(reply)
            };
            match timeout {
                None => request.await,
                Some(d) => tokio::time::timeout(d, request).await.unwrap_or_else(|_| {
                    Err(ActorError::TimeoutDetail(format!(
                        "remote ask {} after {:?}",
                        self.path, d
                    )))
                }),
            }
            }.await;
            input.finish(result)
        }))
    }

    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.deliver_with_options(msg, MessageOptions::default())
    }

    fn deliver_with_options<'a>(
        &'a self,
        msg: BoxedMessage,
        options: MessageOptions,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let mut input = OutboundInput(Some(msg));
        Box::pin(parrot_api::trace::capture(async move {
            let result = async {
                let input = &mut input;
                options.validate_single_attempt()?;
                let started = tokio::time::Instant::now();
                let (key, payload) = self.encode_outgoing(input.value())?;
                let timeout = remaining_budget(started, options.timeout)?;
                let delivery = async move {
                    let sender =
                        self.inner
                            .ensure_sender(&self.node_id)
                            .await
                            .ok_or_else(|| {
                                ActorError::InternalError(format!(
                                    "remote link to {} unavailable",
                                    self.node_id
                                ))
                            })?;
                    let mut frame = self
                        .inner
                        .outgoing(Frame::tell_with_seq(&self.path, &key, payload, 0));
                    frame = sender
                        .trace_request(frame)
                        .map_err(|e| e.to_actor_error())?;
                    // Allocate a sequence only after size validation and queue admission: cancellation
                    // cannot create a gap, and urgent traffic still obeys wire order.
                    let permit = sender
                        .reserve(&frame)
                        .await
                        .map_err(|e| e.to_actor_error())?;
                    frame.header.seq = self.inner.next_seq(self.node_id.clone());
                    if options.priority.value() >= 70 {
                        frame.header.flags |= crate::frame::flags::URGENT;
                    }
                    permit.send(frame);
                    Ok(())
                };
                match timeout {
                    Some(duration) => tokio::time::timeout(duration, delivery)
                        .await
                        .map_err(|_| ActorError::Timeout)?,
                    None => delivery.await,
                }
            }
            .await;
            input.finish(result)
        }))
    }

    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let sender = self
                .inner
                .ensure_sender(&self.node_id)
                .await
                .ok_or_else(|| {
                    ActorError::InternalError(format!(
                        "remote link to {} unavailable",
                        self.node_id
                    ))
                })?;
            sender
                .send(self.inner.outgoing(Frame::stop(&self.path)))
                .await
                .map_err(|e| e.to_actor_error())?;
            Ok(())
        })
    }

    fn path(&self) -> String {
        self.path.clone()
    }

    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move { self.inner.sender_of(&self.node_id).is_some() })
    }

    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn remaining_budget(
    started: tokio::time::Instant,
    timeout: Option<Duration>,
) -> ActorResult<Option<Duration>> {
    timeout
        .map(|budget| {
            budget
                .checked_sub(started.elapsed())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(ActorError::Timeout)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeState;
    #[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
    struct CompletionPayload(u64);
    impl parrot_api::Message for CompletionPayload {
        type Result = ();
    }

    #[derive(serde::Serialize, serde::Deserialize, parrot_api_derive::RemoteMessage)]
    struct SizedPayload(Vec<u8>);
    impl parrot_api::Message for SizedPayload {
        type Result = ();
    }

    #[tokio::test]
    async fn negotiated_admission_rejects_before_capacity_and_preserves_tell_sequence() {
        use crate::{
            handshake::{caps, HandshakeBody},
            transport::{run_connection, ConnParams, ConnSide},
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let (left, right) = tokio::io::duplex(4096);
            let (shutdown, _) = tokio::sync::watch::channel(false);
            let (in_a, _rx_a) = tokio::sync::mpsc::channel(8);
            let (in_b, mut rx_b) = tokio::sync::mpsc::channel(8);
            let params = |side, id: &str, inbound| ConnParams {
                side,
                local_addr: None,
                peer_addr: None,
                scheme: "mem",
                local_handshake: HandshakeBody {
                    node_id: id.into(),
                    max_frame_len: 256,
                    capabilities: caps::BIN | caps::TELL_COMPLETION,
                    ..Default::default()
                },
                inbound,
                on_disconnect: Arc::new(|_| {}),
                shutdown: shutdown.subscribe(),
                knobs: None,
            };
            let (a, b) = tokio::try_join!(
                run_connection(left, params(ConnSide::Connect, "a", in_a)),
                run_connection(right, params(ConnSide::Accept, "b", in_b))
            )
            .unwrap();
            let inner = Arc::new(RemoteInner {
                system: None,
                nodes: vec![(
                    "b".into(),
                    a.sender.clone(),
                    Arc::new(NodeStatus::default()),
                )],
                callbacks: Arc::new(CallbackRegistry::new(8)),
                self_node: "a".into(),
                uplink: None,
                seq_counters: Default::default(),
            });
            let reference = RemoteActorRef::new("/target", "b", inner.clone());
            // Occupy the actual outbound queue; invalid work must not await it.
            let reservations = a
                .sender
                .tx
                .reserve_many(a.sender.tx.max_capacity())
                .await
                .unwrap();
            for operation in 0..3 {
                let message = Box::new(SizedPayload(vec![0; 256])) as BoxedMessage;
                let result = tokio::time::timeout(Duration::from_millis(100), async {
                    match operation {
                        0 => reference.deliver(message).await,
                        1 => reference.send(message).await.map(|_| ()),
                        _ => {
                            reference
                                .deliver_with_ack(message, Duration::from_secs(1))
                                .await
                        }
                    }
                })
                .await
                .expect("invalid frame waited for queue capacity");
                let error = result.unwrap_err().to_string();
                assert!(
                    error.contains("protocol violation") && error.contains("256"),
                    "{error}"
                );
                assert_eq!(
                    inner.callbacks.len(),
                    0,
                    "rejected request leaked its callback"
                );
                assert!(inner.seq_counters.lock().unwrap().is_empty());
            }
            assert!(rx_b.try_recv().is_err());
            drop(reservations);
            reference
                .deliver(Box::new(SizedPayload(vec![1])))
                .await
                .unwrap();
            let (received, _, _) = rx_b.recv().await.unwrap();
            assert_eq!(received.header.seq, 1, "rejected TELL consumed a sequence");
            assert_eq!(received.path, "/target");
            assert!(!a.sender.is_closed());
            shutdown.send_replace(true);
            a.closed.await.unwrap();
            b.closed.await.unwrap();
        })
        .await
        .expect("connection admission/retirement deadline");
    }

    #[tokio::test]
    async fn completion_deadline_covers_reserved_writer_queue_and_releases_callback() {
        use crate::{
            handshake::{caps, HandshakeBody},
            transport::{run_connection, ConnParams, ConnSide},
        };
        let (left, right) = tokio::io::duplex(4096);
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let (in_a, _rx_a) = tokio::sync::mpsc::channel(8);
        let (in_b, mut rx_b) = tokio::sync::mpsc::channel(8);
        let params = |side, id: &str, inbound| ConnParams {
            side,
            local_addr: None,
            peer_addr: None,
            scheme: "mem",
            local_handshake: HandshakeBody {
                node_id: id.into(),
                capabilities: caps::BIN | caps::TELL_COMPLETION,
                ..Default::default()
            },
            inbound,
            on_disconnect: Arc::new(|_| {}),
            shutdown: shutdown.subscribe(),
            knobs: None,
        };
        let (a, b) = tokio::try_join!(
            run_connection(left, params(ConnSide::Connect, "a", in_a)),
            run_connection(right, params(ConnSide::Accept, "b", in_b))
        )
        .unwrap();
        let callbacks = Arc::new(CallbackRegistry::new(8));
        let inner = Arc::new(RemoteInner {
            system: None,
            nodes: vec![(
                "b".into(),
                a.sender.clone(),
                Arc::new(NodeStatus::default()),
            )],
            callbacks: callbacks.clone(),
            self_node: "a".into(),
            uplink: None,
            seq_counters: Default::default(),
        });
        let reference = RemoteActorRef::new("/target", "b", inner);
        // Hold actual queue capacity without depending on socket speed or sleeps.
        let reservations = a
            .sender
            .tx
            .reserve_many(a.sender.tx.max_capacity())
            .await
            .unwrap();
        let message = Box::new(CompletionPayload(7)) as BoxedMessage;
        let error = reference
            .deliver_with_ack(message, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ActorError::Timeout | ActorError::TimeoutDetail(_)
        ));
        assert_eq!(callbacks.len(), 0);
        assert!(rx_b.try_recv().is_err());
        drop(reservations);
        shutdown.send_replace(true);
        a.closed.await.unwrap();
        b.closed.await.unwrap();
    }

    fn test_inner_with_sender() -> (Arc<RemoteInner>, tokio::sync::mpsc::Receiver<Frame>) {
        let (tx, rx) = tokio::sync::mpsc::channel::<Frame>(16);
        let status = Arc::new(NodeStatus::default());
        status.set(NodeState::Connected);
        (
            Arc::new(RemoteInner {
                system: None,
                nodes: vec![("n1".into(), crate::transport::FrameSender::anon(tx), status)],
                callbacks: Arc::new(CallbackRegistry::new(8)),
                self_node: "self".into(),
                uplink: None,
                seq_counters: Default::default(),
            }),
            rx,
        )
    }

    struct SlowEncoding;
    parrot_api::message::inventory::submit! {
        parrot_api::message::CodecRegistration {
            type_key: "bin:parrot_remote::SlowEncoding#v1",
            type_id: std::any::TypeId::of::<SlowEncoding>(),
            encode: |_| {
                // Synchronous codecs cannot be preempted, but they must not
                // acquire a fresh send budget after exhausting the deadline.
                std::thread::sleep(Duration::from_millis(25));
                Ok(Vec::new())
            },
            decode: |_| Ok(Box::new(SlowEncoding) as BoxedMessage),
        }
    }

    #[tokio::test]
    async fn expired_encoding_budget_prevents_ask_and_tell_admission() {
        let (inner, mut rx) = test_inner_with_sender();
        let reference = RemoteActorRef::new("/x", "n1", inner.clone());
        let options = MessageOptions {
            timeout: Some(Duration::from_millis(1)),
            ..Default::default()
        };
        let ask = reference
            .send_with_options(Box::new(SlowEncoding), options.clone())
            .await;
        let tell = reference
            .deliver_with_options(Box::new(SlowEncoding), options)
            .await;
        assert!(matches!(
            ask,
            Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
        ));
        assert!(matches!(
            tell,
            Err(ActorError::Timeout | ActorError::TimeoutDetail(_))
        ));
        assert!(
            rx.try_recv().is_err(),
            "expired encoding must not emit a frame"
        );
        assert!(inner.callbacks.is_empty());
        // A rejected TELL must not consume a sequence and leave a wire gap.
        assert!(inner.seq_counters.lock().unwrap().is_empty());
    }

    // 纯本地单测：NotRemotable 不发帧（RC6 语义的 registry 侧半边）
    #[tokio::test]
    async fn ref_notremotable_local() {
        let inner = Arc::new(RemoteInner {
            system: None,
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
            system: None,
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

    #[tokio::test]
    async fn typed_priority_options_set_remote_urgent_flag() {
        let (inner, mut rx) = test_inner_with_sender();
        let r = RemoteActorRef::new("/x", "n1", inner);
        r.deliver_with_options(
            Box::new(crate::ingress::TestAsk(9)),
            parrot_api::message::MessageOptions {
                priority: parrot_api::message::MessagePriority::HIGH,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let f = rx.recv().await.unwrap();
        assert_eq!(
            f.header.flags & crate::frame::flags::URGENT,
            crate::frame::flags::URGENT
        );
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

    #[tokio::test]
    async fn cancelled_ask_cleans_callback_at_both_awaits() {
        for full in [false, true] {
            let (inner, _rx) = test_inner_with_sender();
            let r = RemoteActorRef::new("/x", "n1", inner.clone());
            if full {
                for _ in 0..16 {
                    inner.nodes[0].1.send(Frame::heartbeat()).await.unwrap();
                }
            }
            assert!(tokio::time::timeout(
                Duration::from_millis(10),
                r.send(Box::new(crate::ingress::TestAsk(1)))
            )
            .await
            .is_err());
            assert!(
                inner.callbacks.is_empty(),
                "external cancellation leaked a slot, full={full}"
            );
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                r.send_with_timeout(
                    Box::new(crate::ingress::TestAsk(2)),
                    Some(Duration::from_millis(10)),
                ),
            )
            .await
            .expect("queue admission must respect ask deadline");
            assert!(matches!(result, Err(ActorError::TimeoutDetail(_))));
            assert!(inner.callbacks.is_empty());
        }
    }

    #[tokio::test]
    async fn cancelled_tell_does_not_allocate_a_sequence() {
        let (inner, mut rx) = test_inner_with_sender();
        let reference = RemoteActorRef::new("/x", "n1", inner.clone());
        for _ in 0..16 {
            inner.nodes[0].1.send(Frame::heartbeat()).await.unwrap();
        }
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            reference.deliver(Box::new(crate::ingress::TestAsk(1)))
        )
        .await
        .is_err());
        for _ in 0..16 {
            rx.recv().await.unwrap();
        }
        reference
            .deliver(Box::new(crate::ingress::TestAsk(2)))
            .await
            .unwrap();
        assert_eq!(rx.recv().await.unwrap().header.seq, 1);
    }

    // 回调表容量满 → insert 失败 → InternalError（capacity=1 预占满）
    #[tokio::test]
    async fn ref_ask_capacity_reject() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(16);
        let status = Arc::new(NodeStatus::default());
        let inner = Arc::new(RemoteInner {
            system: None,
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
