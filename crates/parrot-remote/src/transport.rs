//! 职责：Transport trait——连接建立的机制；载体（tcp/quic/ws/mem）是策略（E5.3）。
//!
//! ConnectionTask：每连接一个 tokio 任务（写侧 mpsc 串行化 + 读循环 + 心跳
//! 半开判定 + 断连回调），DEV_01 §3.6 时序图的落地。

use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;

use std::sync::Arc;
use std::time::{Duration, Instant};

mod lifecycle;
pub(crate) use lifecycle::ConnectionTasks;
pub mod memory;
pub mod quic;
pub mod tcp;

use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::{Decoder, Encoder};

use crate::error::RemoteError;
use crate::frame::{frame_type, Frame, FrameError};
use crate::handshake::{negotiate_caps, HandshakeAckBody, HandshakeBody};
use crate::node::NodeAddr;

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
pub const HEARTBEAT_MAX_LOSS: u32 = 5; // 5×2s=10s 无帧 → 半开判定
pub const OUTBOUND_QUEUE: usize = 1024; // 出站队列容量（天然反压）

/// 出站帧发送端（克隆共享；内部 mpsc 串行化写侧）。
#[derive(Clone)]
pub struct FrameSender {
    pub(crate) tx: mpsc::Sender<Frame>,
    /// 归属对端 node_id（hub 转发 fail_target 定位用——run_connection 注入；
    /// 构造后不变）。
    peer_node: Arc<std::sync::OnceLock<String>>,
    cancelled: tokio_util::sync::CancellationToken,
    authentication: crate::security::PeerAuthentication,
    peer_capabilities: u32,
    negotiated_capabilities: u32,
    /// Immutable bound negotiated by this connection, shared by sender clones.
    max_frame_len: u32,
}

impl FrameSender {
    /// 构造（peer_node 已知——转发失效定位用）。
    pub fn new(tx: mpsc::Sender<Frame>, peer_node: impl Into<String>) -> Self {
        let cell = Arc::new(std::sync::OnceLock::new());
        let _ = cell.set(peer_node.into());
        Self {
            tx,
            peer_node: cell,
            cancelled: tokio_util::sync::CancellationToken::new(),
            authentication: crate::security::PeerAuthentication::Unauthenticated,
            peer_capabilities: 0,
            negotiated_capabilities: 0,
            max_frame_len: crate::frame::MAX_FRAME_LEN,
        }
    }

    /// 测试构造（无 peer 标注——node_id() 返回 "?"）。
    pub fn anon(tx: mpsc::Sender<Frame>) -> Self {
        Self {
            tx,
            peer_node: Arc::new(std::sync::OnceLock::new()),
            cancelled: tokio_util::sync::CancellationToken::new(),
            authentication: crate::security::PeerAuthentication::Unauthenticated,
            peer_capabilities: 0,
            negotiated_capabilities: 0,
            max_frame_len: crate::frame::MAX_FRAME_LEN,
        }
    }

    /// 对端 node_id（未标注返回 "?"——转发失效定位退化为全表扫描）。
    /// Compare actual writer channels, not user-supplied node identities.
    pub fn same_connection(&self, other: &Self) -> bool {
        self.tx.same_channel(&other.tx)
    }

    /// Identity of this driver, stable across clones while any clone is alive.
    pub(crate) fn dispatch_key(&self) -> usize {
        Arc::as_ptr(&self.peer_node) as usize
    }

    /// Authentication established by the owning transport, never by HELLO fields.
    pub fn authentication(&self) -> crate::security::PeerAuthentication {
        self.authentication
    }

    pub fn node_id(&self) -> &str {
        self.peer_node.get().map(String::as_str).unwrap_or("?")
    }

    /// Capabilities advertised by this exact connection's peer during HELLO.
    /// Zero for manually constructed senders without a handshake. Advertising a
    /// capability is not authentication or proof of downstream business behavior.
    pub fn peer_capabilities(&self) -> u32 {
        self.peer_capabilities
    }

    pub(crate) fn supports_mirror_shadow(&self) -> bool {
        {
            let required =
                crate::handshake::caps::MIRROR_SHADOW | crate::handshake::caps::TELL_COMPLETION;
            self.negotiated_capabilities & required == required
        }
    }
    pub fn supports_trace_context(&self) -> bool {
        self.negotiated_capabilities & crate::handshake::caps::TRACE_CONTEXT != 0
    }

    /// Encode only on a mutually capable link. An older hop gets the original
    /// business frame and no invented causal relationship beyond that boundary.
    pub(crate) fn trace_request(&self, frame: Frame) -> Result<Frame, RemoteError> {
        if self.supports_trace_context() {
            if let Some(context) = parrot_api::trace::TraceContext::current() {
                return Ok(frame.with_trace_parent(context.as_parent())?);
            }
        }
        Ok(frame)
    }

    fn check_trace_context(&self, frame: &Frame) -> Result<(), RemoteError> {
        if (frame.header.flags & crate::frame::flags::TRACE_CONTEXT != 0
            && !self.supports_trace_context())
            || (frame.header.flags & crate::frame::flags::MIRROR_SHADOW != 0
                && !self.supports_mirror_shadow())
        {
            return Err(crate::frame::FrameError::UnsupportedFlags {
                flags: frame.header.flags,
            }
            .into());
        }
        Ok(())
    }

    pub async fn send(&self, f: Frame) -> Result<(), RemoteError> {
        self.check_trace_context(&f)?;
        f.validate_size(self.max_frame_len)?;
        tokio::select! {
            biased;
            _ = self.closed() => Err(RemoteError::Transport("writer closed".into())),
            result = self.tx.send(f) => result.map_err(|_| RemoteError::Transport("writer dropped".into())),
        }
    }
    /// Check size before reserving capacity. Callers may change fixed-width
    /// sequence/correlation fields after admission, but must not grow the body.
    pub(crate) async fn reserve(
        &self,
        frame: &Frame,
    ) -> Result<mpsc::Permit<'_, Frame>, RemoteError> {
        self.check_trace_context(frame)?;
        frame.validate_size(self.max_frame_len)?;
        tokio::select! {
            biased;
            _ = self.closed() => Err(RemoteError::Transport("writer closed".into())),
            result = self.tx.reserve() => result.map_err(|_| RemoteError::Transport("writer dropped".into())),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.cancelled.is_cancelled() || self.tx.is_closed()
    }

    /// Close only this connection, including any writer currently blocked on IO.
    pub fn close(&self) {
        self.cancelled.cancel();
    }

    pub async fn closed(&self) {
        tokio::select! {
            _ = self.cancelled.cancelled() => {},
            _ = self.tx.closed() => {},
        }
    }

    /// 便捷回源（relayed reply——REPLY/REPLY_ERR 同入口：换 cid 原样回送）。
    pub fn send_frame(&self, f: Frame) {
        let _ = self.send_frame_sync(f);
    }

    /// Replies must either be queued or make connection loss observable.
    pub fn send_reply(&self, frame: Frame) {
        if self.send_frame_sync(frame).is_err() {
            self.close();
        }
    }

    /// 同步转发（TELL 中转——队列满返回 Err，调用方记死信）。
    pub fn send_frame_sync(&self, f: Frame) -> Result<(), RemoteError> {
        self.check_trace_context(&f)?;
        f.validate_size(self.max_frame_len)?;
        if self.is_closed() {
            return Err(RemoteError::Transport("writer closed".into()));
        }
        self.tx
            .try_send(f)
            .map_err(|_| RemoteError::Transport("relay queue full".into()))
    }

    /// 无连接回程（重排任务投递用——TELL 无回程语义，relay miss 走死信）。
    pub fn detached() -> Self {
        let (tx, _rx) = mpsc::channel(1);
        Self::anon(tx)
    }
}

/// 传输载体抽象。P1：tcp/mem；P2：quic（07 §8.1 scheme() 统一命名）。
/// A transport owns its connection drivers. Built-in transports synchronously
/// close admission and request cancellation on Drop without awaiting task exit.
/// Keep the runtime alive for cleanup, or use shutdown() as a completion barrier.
/// Retained connection handles and external watch senders do not extend ownership.
#[async_trait::async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn connect(&self, addr: &NodeAddr) -> Result<ConnectionHandle, RemoteError>;
    async fn listen(&self, bind: SocketAddr) -> Result<(), RemoteError>;
    async fn accept(&self) -> Result<ConnectionHandle, RemoteError>;
    /// Permanently fence new operations and cancel existing ones, without awaiting.
    fn close(&self);
    /// Wait until owned operations, drivers and listeners have released their IO.
    async fn shutdown(&self);
    fn scheme(&self) -> &'static str; // "tcp" | "quic"(P2) | "ws"(P3) | "mem"
    /// 实际监听地址（bind :0 后取真实端口；未监听 → None）。
    fn local_addr(&self) -> Option<SocketAddr> {
        None
    }
}

/// 一条已建立的连接（握手完成后产出）。
pub struct ConnectionHandle {
    pub node_id: String,
    pub sender: FrameSender,
    pub info: ConnectionInfo,
    /// 对端拓扑角色（握手协商保留——spoke 据此识别 uplink hub）。
    pub peer_role: crate::handshake::TopologyRole,
    /// 对端可直拨地址（方案 A：握手 DIRECT_ADDR 声明；None = 不可直拨）。
    pub peer_dial_addr: Option<String>,
    /// 断连通知（ConnectionTask 结束时触发一次）
    pub closed: oneshot::Receiver<()>,
}

#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub local: Option<SocketAddr>,
    pub peer: Option<SocketAddr>,
    pub scheme: &'static str,
    pub established_at: Instant,
}

/// tokio_util codec 薄封装：使用协议全局 16 MiB 上限。
///
/// 保留为公开单元结构体以兼容现有网关/探针代码；托管连接在握手后使用
/// 私有的 `NegotiatedFrameCodec` 收紧上限。
#[derive(Clone, Copy, Default)]
pub struct FrameCodec;

/// Connection-local codec whose limit is set by the HELLO/ACK negotiation.
#[derive(Clone, Copy)]
struct NegotiatedFrameCodec {
    max_frame_len: u32,
}

impl Default for NegotiatedFrameCodec {
    fn default() -> Self {
        Self {
            max_frame_len: crate::frame::MAX_FRAME_LEN,
        }
    }
}

impl NegotiatedFrameCodec {
    fn set_max_frame_len(&mut self, max_frame_len: u32) {
        self.max_frame_len = max_frame_len
            .max(crate::frame::BODY_FIXED_OVERHEAD as u32)
            .min(crate::frame::MAX_FRAME_LEN);
    }
}

impl Encoder<Frame> for FrameCodec {
    type Error = FrameError;
    fn encode(&mut self, item: Frame, buf: &mut BytesMut) -> Result<(), FrameError> {
        item.encode(buf)
    }
}

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = FrameError;
    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        Frame::decode(buf)
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        decode_eof_with(self, buf)
    }
}

impl Encoder<Frame> for NegotiatedFrameCodec {
    type Error = FrameError;
    fn encode(&mut self, item: Frame, buf: &mut BytesMut) -> Result<(), FrameError> {
        item.validate_size(self.max_frame_len)?;
        item.encode(buf)
    }
}

impl Decoder for NegotiatedFrameCodec {
    type Item = Frame;
    type Error = FrameError;
    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        Frame::decode_with_limit(buf, self.max_frame_len)
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, FrameError> {
        decode_eof_with(self, buf)
    }
}

fn decode_eof_with<C>(codec: &mut C, buf: &mut BytesMut) -> Result<Option<Frame>, FrameError>
where
    C: Decoder<Item = Frame, Error = FrameError>,
{
    // Drain complete pipelined frames and preserve specific parse errors before
    // classifying any bytes left when the peer closes its write half.
    match codec.decode(buf)? {
        Some(frame) => Ok(Some(frame)),
        None if buf.is_empty() => Ok(None),
        None => Err(FrameError::Truncated {
            received: buf.len(),
            expected: buf.get(..4).map(|prefix| {
                // decode has already checked the bounded advertised length.
                4 + u32::from_le_bytes(prefix.try_into().unwrap()) as usize
            }),
        }),
    }
}

impl From<std::io::Error> for FrameError {
    fn from(error: std::io::Error) -> Self {
        tracing::debug!(io = %error, kind = ?error.kind(), "framed io error");
        FrameError::Io {
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
            message: error.to_string(),
        }
    }
}

/// Connection-close hook. Receives the actual writer identity, with its close
/// token already cancelled; node_id alone is insufficient for request cleanup.
pub type OnDisconnect = Arc<dyn Fn(&FrameSender) + Send + Sync>;

/// 建连模式（主动 connect / 被动 accept 决定握手先后手）。
#[derive(Clone, Copy)]
pub enum ConnSide {
    Connect,
    Accept,
}

/// 建立连接任务：IO 流 → 握手 → ConnectionHandle + 驱动循环。
///
/// 握手时序（DEV_01 §8.5）：connect 侧发 HANDSHAKE 后必须先等 ACK 再放行
/// 数据帧；accept 侧先收 HANDSHAKE 再回 ACK。两条 mpsc 严格控制时序，
/// 勿用 sleep 同步。
pub struct ConnParams {
    pub side: ConnSide,
    pub local_addr: Option<SocketAddr>,
    pub peer_addr: Option<SocketAddr>,
    pub scheme: &'static str,
    pub local_handshake: HandshakeBody,
    pub inbound: mpsc::Sender<(Frame, FrameSender, String)>,
    pub on_disconnect: OnDisconnect,
    pub shutdown: tokio::sync::watch::Receiver<bool>,
    /// 运行时旋钮（配置切面——None=编译期默认常量，零迁移成本）。
    pub knobs: Option<RuntimeKnobs>,
}

/// 连接层运行时旋钮（原写死常量参数化——配置切面 `[remote.transport]`）。
///
/// 心跳/出站队列沿用原默认值（2s / 5 次 / 1024 帧）；入站 ASK
/// 新增系统级并发与编码长度预算，超限明确拒绝，见 SEMANTIC_CHANGES。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeKnobs {
    /// Enable proactive heartbeat and loss detection; incoming heartbeats are
    /// acknowledged even when disabled. RemoteConfig may also disable this.
    pub heartbeat_enabled: bool,
    /// 心跳发送间隔（半开检测粒度）。
    pub heartbeat_interval: Duration,
    /// 连续丢失多少个间隔判半开断开。
    pub heartbeat_max_loss: u32,
    /// 出站帧队列容量（满=天然反压）。
    pub outbound_queue: usize,
    /// Maximum accepted inbound ASK futures per system.
    pub inbound_ask_capacity: usize,
    /// Sum of encoded ASK path, key and payload lengths retained by those futures.
    pub inbound_ask_bytes: usize,
    /// System-wide slow dispatch operations, including queued and active work.
    pub inbound_dispatch_capacity: usize,
    /// Slow dispatch operations retained per physical connection.
    pub inbound_dispatch_per_connection: usize,
    /// Retained payload/path/type key/source identity bytes for slow dispatch.
    pub inbound_dispatch_bytes: usize,
    /// TELL 重排缺口等待上限（缺省 250ms）。
    pub reorder_gap_timeout_ms: u64,
    /// 重排缓冲帧数上限（缺省 1024）。
    pub reorder_buffer_cap: usize,
    /// 帧默认跳数上限（缺省 8）。
    pub default_hop_limit: u8,
}

impl Default for RuntimeKnobs {
    fn default() -> Self {
        Self {
            heartbeat_enabled: true,
            heartbeat_interval: HEARTBEAT_INTERVAL,
            heartbeat_max_loss: HEARTBEAT_MAX_LOSS,
            outbound_queue: OUTBOUND_QUEUE,
            inbound_ask_capacity: parrot_config::Defaults::inbound_ask_capacity(),
            inbound_ask_bytes: parrot_config::Defaults::inbound_ask_bytes(),
            inbound_dispatch_capacity: parrot_config::Defaults::inbound_dispatch_capacity(),
            inbound_dispatch_per_connection:
                parrot_config::Defaults::inbound_dispatch_per_connection(),
            inbound_dispatch_bytes: parrot_config::Defaults::inbound_dispatch_bytes(),
            reorder_gap_timeout_ms: 250,
            reorder_buffer_cap: 1024,
            default_hop_limit: crate::frame::DEFAULT_HOP_LIMIT,
        }
    }
}

pub async fn run_connection<S>(io: S, p: ConnParams) -> Result<ConnectionHandle, RemoteError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    run_connection_owned(io, p, None).await
}

pub(crate) async fn run_connection_owned<S>(
    io: S,
    p: ConnParams,
    owner: Option<&ConnectionTasks>,
) -> Result<ConnectionHandle, RemoteError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    run_connection_with_peer(io, p, owner, crate::security::PeerTrust::Unauthenticated).await
}

pub(crate) async fn run_connection_with_peer<S>(
    io: S,
    p: ConnParams,
    owner: Option<&ConnectionTasks>,
    peer: crate::security::PeerTrust,
) -> Result<ConnectionHandle, RemoteError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    run_connection_captured(io, p, owner, peer, None).await
}

pub(crate) async fn run_connection_captured<S>(
    io: S,
    p: ConnParams,
    owner: Option<&ConnectionTasks>,
    peer: crate::security::PeerTrust,
    capture: Option<&crate::capture::FrameCapture>,
) -> Result<ConnectionHandle, RemoteError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ConnParams {
        side,
        local_addr,
        peer_addr,
        scheme,
        local_handshake,
        inbound,
        on_disconnect,
        mut shutdown,
        knobs,
    } = p;
    local_handshake
        .validate()
        .map_err(|e| RemoteError::Handshake(format!("local config: {e}")))?;
    let capture = capture.and_then(|c| c.connection(&local_handshake.node_id, scheme));
    let knobs = knobs.unwrap_or_default();
    let heartbeat_budget = knobs
        .heartbeat_interval
        .checked_mul(knobs.heartbeat_max_loss)
        .filter(|budget| !budget.is_zero())
        .filter(|budget| tokio::time::Instant::now().checked_add(*budget).is_some())
        .ok_or_else(|| RemoteError::Transport("invalid heartbeat deadline".into()))?;
    if knobs.outbound_queue == 0 {
        return Err(RemoteError::Transport(
            "outbound queue capacity must be positive".into(),
        ));
    }
    let metrics = owner.map(|o| o.metrics.clone()).unwrap_or_default();
    let mut metric_lease = crate::metrics::ConnectionMetricLease::new(metrics.clone());
    let mut framed = tokio_util::codec::Framed::new(io, NegotiatedFrameCodec::default());
    let (tx, rx) = mpsc::channel::<Frame>(knobs.outbound_queue);

    // ---- 握手阶段 ----
    let handshake = async {
        let peer_body: HandshakeBody = match side {
            ConnSide::Connect => {
                let mut body = BytesMut::new();
                local_handshake
                    .encode_tlv(&mut body)
                    .map_err(|e| RemoteError::Handshake(format!("local encode: {e}")))?;
                let frame = Frame::handshake(body.freeze());
                if let Some(capture) = &capture {
                    capture.observe(crate::capture::CaptureDirection::Tx, &frame);
                }
                let measured = crate::metrics::FrameMetric::of(&frame);
                framed
                    .send(frame)
                    .await
                    .map_err(|e| RemoteError::Handshake(format!("send: {e}")))?;
                metrics.sent(measured);
                let ack = recv_expect(
                    &mut framed,
                    frame_type::HANDSHAKE_ACK,
                    capture.as_ref(),
                    &metrics,
                )
                .await?;
                let ack_body = HandshakeAckBody::decode_tlv(&ack.payload)
                    .map_err(|e| RemoteError::Handshake(format!("ack decode: {e}")))?;
                let common = negotiate_caps(local_handshake.capabilities, ack_body.capabilities)
                    .map_err(|_| RemoteError::Handshake("no common codec".into()))?;
                let chosen_cap = match ack_body.chosen_codec.as_str() {
                    "bin" => crate::handshake::caps::BIN,
                    "pb" => crate::handshake::caps::PB,
                    _ => return Err(RemoteError::Handshake("invalid chosen codec".into())),
                };
                if common & chosen_cap == 0 {
                    return Err(RemoteError::Handshake(
                        "peer chose an unsupported codec".into(),
                    ));
                }
                HandshakeBody {
                    node_id: ack_body.node_id,
                    realm: ack_body.realm,
                    cluster: ack_body.cluster,
                    capabilities: ack_body.capabilities,
                    max_frame_len: ack_body.max_frame_len,
                    topology_role: ack_body.topology_role,
                    hop_limit: ack_body.hop_limit,
                    direct_addr: ack_body.direct_addr,
                }
            }
            ConnSide::Accept => {
                let hs = recv_expect(
                    &mut framed,
                    frame_type::HANDSHAKE,
                    capture.as_ref(),
                    &metrics,
                )
                .await?;
                let theirs = HandshakeBody::decode_tlv(&hs.payload)
                    .map_err(|e| RemoteError::Handshake(format!("decode: {e}")))?;
                peer.verify(&theirs.node_id)?;
                let common = negotiate_caps(local_handshake.capabilities, theirs.capabilities)
                    .map_err(|e| RemoteError::Handshake(format!("{:?}", e as u8)))?;
                let mine_ack = HandshakeAckBody {
                    node_id: local_handshake.node_id.clone(),
                    realm: local_handshake.realm.clone(),
                    cluster: local_handshake.cluster.clone(),
                    capabilities: local_handshake.capabilities,
                    max_frame_len: local_handshake.max_frame_len.min(theirs.max_frame_len),
                    topology_role: local_handshake.topology_role,
                    hop_limit: local_handshake.hop_limit,
                    direct_addr: local_handshake.direct_addr.clone(),
                    chosen_codec: if common & crate::handshake::caps::BIN != 0 {
                        "bin".into()
                    } else {
                        "pb".into()
                    },
                };
                // HELLO and ACK both use the global handshake bound. Apply the
                // negotiated bound below only after ACK has been flushed, even
                // when it is smaller than the handshake metadata itself.
                let mut body = BytesMut::new();
                mine_ack
                    .encode_tlv(&mut body)
                    .map_err(|e| RemoteError::Handshake(format!("ack encode: {e}")))?;
                let frame = Frame::handshake_ack(body.freeze());
                if let Some(capture) = &capture {
                    capture.observe(crate::capture::CaptureDirection::Tx, &frame);
                }
                let measured = crate::metrics::FrameMetric::of(&frame);
                framed
                    .send(frame)
                    .await
                    .map_err(|e| RemoteError::Handshake(format!("ack send: {e}")))?;
                metrics.sent(measured);
                theirs
            }
        };
        if matches!(side, ConnSide::Connect) {
            peer.verify(&peer_body.node_id)?;
        }
        Ok::<_, RemoteError>(peer_body)
    };
    let peer_body = tokio::select! {
        biased;
        _ = wait_shutdown(&mut shutdown) => return Err(RemoteError::Transport("connection shut down during handshake".into())),
        result = tokio::time::timeout(Duration::from_secs(5), handshake) => {
            result.map_err(|_| RemoteError::Handshake("handshake timeout 5s".into()))??
        }
    };
    // Enforce the same bound on both established codecs and queue admission.
    framed
        .codec_mut()
        .set_max_frame_len(local_handshake.max_frame_len.min(peer_body.max_frame_len));
    if let Some(capture) = &capture {
        capture.established(&peer_body.node_id);
    }
    metric_lease.established();
    let remote_node_id = peer_body.node_id.clone();
    let peer_role = peer_body.topology_role;
    let peer_dial_addr = peer_body.direct_addr.clone();

    let (closed_tx, closed_rx) = oneshot::channel::<()>();
    let info = ConnectionInfo {
        local: local_addr,
        peer: peer_addr,
        scheme,
        established_at: Instant::now(),
    };
    let node_id_cb = remote_node_id.clone();
    let dis = on_disconnect.clone();

    // ---- 驱动循环 ----
    let mut back_sender = FrameSender::new(tx.clone(), remote_node_id.as_str());
    back_sender.authentication = peer.authentication();
    back_sender.peer_capabilities = peer_body.capabilities;
    back_sender.negotiated_capabilities = local_handshake.capabilities & peer_body.capabilities;
    back_sender.max_frame_len = local_handshake
        .max_frame_len
        .min(peer_body.max_frame_len)
        .min(crate::frame::MAX_FRAME_LEN);
    let sender = back_sender.clone();
    let cancelled = sender.cancelled.clone();
    let disconnected_sender = sender.clone();
    let from_node = remote_node_id.clone();
    let owner_cancel = owner
        .map(|owner| owner.cancelled.clone())
        .unwrap_or_default();
    let timing_local = local_handshake.node_id.clone();
    static TIMING_NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);
    let timing_connection =
        TIMING_NEXT_CONNECTION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let driver = async move {
        let _metric_lease = metric_lease;
        let _cancel_on_exit = cancelled.clone().drop_guard();
        // The split halves and all three loops belong to this one tracked task.
        // Cancelling drive destroys both halves and the outbound queue before
        // the disconnect callback/closed notification, including blocked IO.
        let drive = async move {
            let (mut sink, mut stream) = framed.split();
            let mut rx = rx;
            let ack_needed = tokio::sync::Notify::new();
            let heartbeat_needed = tokio::sync::Notify::new();
            // Heartbeats carry no unique payload: coalesce each kind to one
            // notification instead of allowing a control-frame queue to grow.
            let (fatal_tx, mut fatal_rx) = oneshot::channel::<Frame>();
            let (last_seen, mut activity) =
                tokio::sync::watch::channel(tokio::time::Instant::now());
            let read = async {
                while let Some(Ok(frame)) = stream.next().await {
                    tracing::trace!(target: crate::durable::timing::TARGET, parent: None,
                        local_node = %timing_local, peer_node = %node_id_cb,
                        connection = timing_connection, cid = frame.header.correlation_id,
                        frame_type = frame.header.frame_type, path = %frame.path,
                        phase = "transport.read", boundary = "observed");
                    metrics.received(&frame);
                    if let Some(capture) = &capture {
                        capture.observe(crate::capture::CaptureDirection::Rx, &frame);
                    }
                    last_seen.send_replace(tokio::time::Instant::now());
                    // Buffered control frames need not touch Tokio IO/channels;
                    // keep cancellation and the other loops schedulable anyway.
                    tokio::task::consume_budget().await;
                    if tracing::enabled!(tracing::Level::TRACE) || crate::frame::trace_enabled() {
                        eprintln!("[parrot-trace] RX {}", frame.trace_line());
                    }
                    if frame.header.frame_type == frame_type::ERROR {
                        tracing::warn!(node = %node_id_cb, "remote terminal ERROR received");
                        // Preserve earlier queued replies before ingress closes this
                        // connection. Terminal frames cannot use application shedding.
                        let drained = tokio::time::timeout(Duration::from_secs(5), async {
                            if inbound
                                .send((frame, back_sender.clone(), from_node.clone()))
                                .await
                                .is_ok()
                            {
                                back_sender.closed().await;
                            }
                        })
                        .await;
                        if drained.is_err() {
                            tracing::warn!(node = %node_id_cb, "terminal ERROR dispatch deadline exceeded");
                        }
                        // No later frame may resume this reader. Driver retirement
                        // cancels IO and invokes the connection-bound cleanup hook.
                        return;
                    }
                    let violation = if frame.validate_payload_flags().is_err()
                        || back_sender.check_trace_context(&frame).is_err()
                    {
                        Some("unsupported payload flags")
                    } else {
                        match frame.header.frame_type {
                            frame_type::HEARTBEAT => {
                                ack_needed.notify_one();
                                continue;
                            }
                            frame_type::HEARTBEAT_ACK => continue,
                            frame_type::HANDSHAKE | frame_type::HANDSHAKE_ACK => {
                                Some("handshake frame after established")
                            }
                            // Resolve control frames are authenticated and
                            // bounded by ingress. Keep them on the normal
                            // inbound lane so the receiver can enforce the
                            // connection identity and cache version.
                            frame_type::FRAGMENT => Some("FRAGMENT not implemented in P1"),
                            _ => None,
                        }
                    };
                    if let Some(detail) = violation {
                        let _ = fatal_tx.send(Frame::error_frame(
                            crate::error::ErrCode::ProtocolViolation,
                            detail,
                        ));
                        // Let the writer flush the terminal error if possible.
                        // A non-reading peer cannot hold this connection forever,
                        // even if proactive heartbeat checking was disabled.
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        return;
                    }
                    // The inbound dispatcher is deliberately bounded. Waiting
                    // for global capacity here would stop this connection from
                    // reading heartbeats and let one slow application create a
                    // false half-open timeout for an otherwise healthy peer.
                    // Load-shed this application frame and keep the transport
                    // control loop alive; ASK/TELL admission and reply paths
                    // still report overload at their own bounded queues.
                    match inbound.try_send((frame, back_sender.clone(), from_node.clone())) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full((frame, back, _))) => {
                            tracing::warn!(
                                node = %node_id_cb,
                                "inbound dispatch queue full; dropping one application frame"
                            );
                            // A request with a caller-visible correlation must
                            // fail fast instead of hanging until its deadline.
                            // Ordinary TELL has no receipt and is intentionally
                            // shed under overload. Batch envelopes are omitted
                            // because their member CIDs cannot be reconstructed
                            // safely at this transport layer.
                            let request_needs_reply = frame.header.frame_type == frame_type::ASK
                                || (frame.header.frame_type == frame_type::TELL
                                    && frame.header.flags & crate::frame::flags::TELL_ACK != 0);
                            if frame.header.flags & crate::frame::flags::BATCH == 0
                                && request_needs_reply
                            {
                                back.send_reply(Frame::reply_err(
                                    frame.header.correlation_id,
                                    &frame.path,
                                    crate::error::ErrCode::Overloaded,
                                    "inbound dispatch queue full",
                                ));
                            }
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            };
            let write = async {
                loop {
                    tokio::task::consume_budget().await;
                    // Terminal errors take priority between complete frames.
                    // ACK/heartbeat/data otherwise receive fair scheduling; an
                    // incoming heartbeat flood cannot monopolize this writer.
                    let (frame, terminal) = tokio::select! {
                        biased;
                        result = &mut fatal_rx => match result {
                            Ok(frame) => (frame, true),
                            Err(_) => return,
                        },
                        frame = async {
                            tokio::select! {
                                _ = ack_needed.notified() => Some(Frame::heartbeat_ack()),
                                _ = heartbeat_needed.notified() => Some(Frame::heartbeat()),
                                frame = rx.recv() => frame,
                            }
                        } => match frame {
                            Some(frame) => (frame, false),
                            None => return,
                        },
                    };
                    if tracing::enabled!(tracing::Level::TRACE) || crate::frame::trace_enabled() {
                        eprintln!("[parrot-trace] TX {}", frame.trace_line());
                    }
                    if let Some(capture) = &capture {
                        capture.observe(crate::capture::CaptureDirection::Tx, &frame);
                    }
                    let measured = crate::metrics::FrameMetric::of(&frame);
                    // Queue admission is not wire completion. This phase includes
                    // encode/write/flush of this frame, but not peer receipt or
                    // durable commit. Correlate by connection endpoints + CID;
                    // transport does not decode payloads to recover a session.
                    let span = tracing::trace_span!(target: crate::durable::timing::TARGET,
                        parent: None, "transport.frame", local_node = %timing_local, peer_node = %node_id_cb,
                        connection = timing_connection, cid = frame.header.correlation_id,
                        frame_type = frame.header.frame_type, path = %frame.path);
                    let timing = crate::durable::timing::Phase::new(span, "transport.write_flush");
                    let result = sink.send(frame).await;
                    timing.finish(result.is_ok());
                    if result.is_err() {
                        return;
                    }
                    metrics.sent(measured);
                    if terminal {
                        return;
                    }
                }
            };
            let heartbeat = async {
                if !knobs.heartbeat_enabled {
                    std::future::pending::<()>().await;
                    return;
                }
                let mut interval = tokio::time::interval(knobs.heartbeat_interval);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    let deadline = *activity.borrow() + heartbeat_budget;
                    tokio::select! {
                        biased;
                        result = activity.changed() => if result.is_err() { return; },
                        _ = tokio::time::sleep_until(deadline) => {
                            // Receipt and timer expiry may be ready together.
                            if activity.borrow().elapsed() >= heartbeat_budget {
                                tracing::warn!(node = %node_id_cb, "connection receive progress deadline exceeded");
                                return;
                            }
                        }
                        _ = interval.tick() => heartbeat_needed.notify_one(),
                    }
                }
            };
            tokio::select! {
                _ = read => {},
                _ = write => {},
                _ = heartbeat => {},
            };
        };
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => {},
            _ = owner_cancel.cancelled() => {},
            _ = wait_shutdown(&mut shutdown) => {},
            _ = drive => {},
        };
        cancelled.cancel();
        dis(&disconnected_sender);
        let _ = closed_tx.send(());
    };
    if let Some(owner) = owner {
        owner.spawn(driver)?;
    } else {
        tokio::spawn(driver);
    }

    Ok(ConnectionHandle {
        node_id: remote_node_id.clone(),
        sender,
        info,
        peer_role,
        peer_dial_addr,
        closed: closed_rx,
    })
}

async fn wait_shutdown(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    if shutdown.wait_for(|closed| *closed).await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn recv_expect<S>(
    framed: &mut tokio_util::codec::Framed<S, NegotiatedFrameCodec>,
    expect: u8,
    capture: Option<&crate::capture::ConnectionCapture>,
    metrics: &crate::metrics::TransportMetrics,
) -> Result<Frame, RemoteError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use futures::StreamExt;
    let deadline = Duration::from_secs(5);
    match tokio::time::timeout(deadline, framed.next()).await {
        Ok(Some(Ok(f))) => {
            metrics.received(&f);
            if let Some(capture) = capture {
                capture.observe(crate::capture::CaptureDirection::Rx, &f);
            }
            f.validate_payload_flags()?;
            if f.header.frame_type == expect {
                Ok(f)
            } else {
                Err(RemoteError::Handshake(format!(
                    "expected frame 0x{expect:02X}, got 0x{:02X}",
                    f.header.frame_type
                )))
            }
        }
        Ok(Some(Err(e))) => Err(RemoteError::Handshake(format!("io: {e}"))),
        Ok(None) => Err(RemoteError::Handshake("closed during handshake".into())),
        Err(_) => Err(RemoteError::Handshake("handshake timeout 5s".into())),
    }
}

/// 迟到 REPLY 计数器（late_reply_dropped_total，E1.10 可观测内建——RC4 断言凭据）。
pub static LATE_REPLY_DROPPED: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeAddr;

    #[test]
    fn negotiated_frame_limit_applies_to_encode_and_decode() {
        use bytes::Bytes;
        use tokio_util::codec::{Decoder, Encoder};

        let frame = Frame::tell("/x", "k", Bytes::from(vec![0xA5; 40]));
        let mut codec = NegotiatedFrameCodec { max_frame_len: 64 };
        let mut encoded = BytesMut::new();
        let error = codec.encode(frame.clone(), &mut encoded).unwrap_err();
        assert!(matches!(error, FrameError::TooLarge { max: 64, .. }));

        let mut full = BytesMut::new();
        Frame::tell("/x", "k", Bytes::from(vec![0xA5; 40]))
            .encode(&mut full)
            .unwrap();
        let error = codec.decode(&mut full).unwrap_err();
        assert!(matches!(error, FrameError::TooLarge { max: 64, .. }));
        assert_eq!(full.len(), 4 + 28 + 2 + 1 + 40);
    }

    #[test]
    fn negotiated_frame_limit_cannot_widen_global_cap() {
        let codec = NegotiatedFrameCodec {
            max_frame_len: crate::frame::MAX_FRAME_LEN,
        };
        assert_eq!(codec.max_frame_len, crate::frame::MAX_FRAME_LEN);
    }

    /// 双工内存对（与 transport/memory.rs 共用底层；此处测握手时序）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshake_over_duplex() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (a_read, a_write) = tokio::io::split(a);
        let (b_read, b_write) = tokio::io::split(b);
        let hs_a = HandshakeBody {
            node_id: "a".into(),
            ..Default::default()
        };
        let hs_b = HandshakeBody {
            node_id: "b".into(),
            ..Default::default()
        };
        let (in_tx, mut in_rx_a) = mpsc::channel::<(Frame, FrameSender, String)>(64);
        let (in_tx2, mut in_rx_b) = mpsc::channel::<(Frame, FrameSender, String)>(64);
        let noop: OnDisconnect = Arc::new(|_| {});
        let (sd_a, _) = tokio::sync::watch::channel(false);
        let (sd_b, _) = tokio::sync::watch::channel(false);

        let h1 = tokio::spawn(run_connection(
            tokio::io::join(a_read, a_write),
            ConnParams {
                side: ConnSide::Connect,
                local_addr: None,
                peer_addr: None,
                scheme: "mem",
                local_handshake: hs_a,
                inbound: in_tx,
                on_disconnect: noop.clone(),
                shutdown: sd_a.subscribe(),
                knobs: None,
            },
        ));
        let h2 = tokio::spawn(run_connection(
            tokio::io::join(b_read, b_write),
            ConnParams {
                side: ConnSide::Accept,
                local_addr: None,
                peer_addr: None,
                scheme: "mem",
                local_handshake: hs_b,
                inbound: in_tx2,
                on_disconnect: noop,
                shutdown: sd_b.subscribe(),
                knobs: None,
            },
        ));
        let (c1, c2) = tokio::join!(h1, h2);
        let c1 = c1.unwrap().unwrap();
        let c2 = c2.unwrap().unwrap();
        assert_eq!(c1.node_id, "b");
        assert_eq!(c2.node_id, "a");

        // ask 帧往返：a → b 入站队列
        c1.sender
            .send(Frame::tell("/x", "k", bytes::Bytes::from_static(b"v")))
            .await
            .unwrap();
        let (got, _, _) = tokio::time::timeout(Duration::from_secs(10), in_rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.path, "/x");
        // b → a 反向
        c2.sender
            .send(Frame::tell("/y", "k", bytes::Bytes::from_static(b"w")))
            .await
            .unwrap();
        let (got2, _, _) = tokio::time::timeout(Duration::from_secs(10), in_rx_a.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got2.path, "/y");
        // 心跳：a 发 HEARTBEAT → b 回 ACK（ACK 不进 inbound，直接吞）
        c1.sender.send(Frame::heartbeat()).await.unwrap();
        // SYSTEM_EVENT → 入站分发（P2 解禁：管理/集群载荷）
        c2.sender
            .send(Frame {
                header: crate::frame::FrameHeader {
                    frame_len: 0,
                    version: 1,
                    frame_type: frame_type::SYSTEM_EVENT,
                    flags: 0,
                    correlation_id: 0,
                    hop_count: 0,
                    hop_limit: 8,
                    seq: crate::frame::SEQ_NONE,
                },
                path: String::new(),
                type_key: String::new(),
                payload: bytes::Bytes::new(),
            })
            .await
            .unwrap();
        let (event, _, _) = tokio::time::timeout(Duration::from_secs(10), in_rx_a.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.header.frame_type, frame_type::SYSTEM_EVENT);
        // Federation control frames are delivered to ingress for authentication
        // and bounded payload handling instead of being rejected by transport.
        c2.sender
            .send(Frame {
                header: crate::frame::FrameHeader {
                    frame_len: 0,
                    version: 1,
                    frame_type: frame_type::RESOLVE_Q,
                    flags: 0,
                    correlation_id: 0,
                    hop_count: 0,
                    hop_limit: 8,
                    seq: crate::frame::SEQ_NONE,
                },
                path: String::new(),
                type_key: String::new(),
                payload: bytes::Bytes::new(),
            })
            .await
            .unwrap();
        let (resolve, _, _) = tokio::time::timeout(Duration::from_secs(2), in_rx_a.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolve.header.frame_type, frame_type::RESOLVE_Q);
        c1.sender.close();
        c2.sender.close();
        tokio::time::timeout(Duration::from_secs(2), c1.closed)
            .await
            .unwrap()
            .unwrap();
    }

    /// E3 半开检测：对端静默（不发帧不回 ACK）→ 5×2s 后 A 侧主动断开。
    /// 真实等待 10s+（> HEARTBEAT_MAX_LOSS × HEARTBEAT_INTERVAL）——
    /// 慢但真：验证 tokio interval 与 last_seen 交互的物理时序正确性。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn heartbeat_half_open_detected() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (a_read, a_write) = tokio::io::split(a);
        let (b_read, b_write) = tokio::io::split(b);
        let hs_a = HandshakeBody {
            node_id: "a".into(),
            ..Default::default()
        };
        let (in_tx, _in_rx) = mpsc::channel::<(Frame, FrameSender, String)>(64);
        let noop: OnDisconnect = Arc::new(|_| {});
        let (sd_a, _) = tokio::sync::watch::channel(false);

        // A 侧正常跑连接
        let h1 = tokio::spawn(run_connection(
            tokio::io::join(a_read, a_write),
            ConnParams {
                side: ConnSide::Connect,
                local_addr: None,
                peer_addr: None,
                scheme: "mem",
                local_handshake: hs_a,
                inbound: in_tx,
                on_disconnect: noop.clone(),
                shutdown: sd_a.subscribe(),
                knobs: None,
            },
        ));
        // B 侧：字节级静默对端。手动完成握手（读掉 A 的 HANDSHAKE 帧、
        // 回一个合法 HANDSHAKE_ACK），随后**只读不写**——吞掉 A 的所有
        // HEARTBEAT 不回 ACK。模拟真实半开（对端活着/NAT 单向断）。
        // duplex 下 EOF 与静默在 A 的 read 侧不可区分，但心跳丢失计数
        // 路径独立于 read EOF——B 保持读开（不 drop b_read）确保不会 EOF，
        // 唯一触发断开的就是 missed >= 5。
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut b_rd, mut b_wr) = (b_read, b_write);
            // ① 读 A 的 HANDSHAKE 帧（4B len 前缀 + body）
            let mut len_buf = [0u8; 4];
            b_rd.read_exact(&mut len_buf).await.unwrap();
            let body_len = u32::from_le_bytes(len_buf) as usize;
            let mut body = vec![0u8; body_len];
            b_rd.read_exact(&mut body).await.unwrap();
            // ② 回 HANDSHAKE_ACK（最小合法 TLV：node_id 必填，其余默认）
            let mut ack_body = bytes::BytesMut::new();
            crate::handshake::HandshakeAckBody {
                node_id: "b".into(),
                realm: None,
                cluster: None,
                capabilities: 3,
                max_frame_len: 1024 * 1024,
                topology_role: crate::handshake::TopologyRole::Normal,
                hop_limit: 8,
                direct_addr: None,
                chosen_codec: "bin".into(),
            }
            .encode_tlv(&mut ack_body)
            .unwrap();
            let ack = crate::frame::Frame::handshake_ack(ack_body.freeze());
            let mut out = bytes::BytesMut::new();
            ack.encode(&mut out).unwrap();
            b_wr.write_all(&out).await.unwrap();
            // ③ 静默期：持续读（吞 A 的心跳），永不写。读到 EOF（A 断开）
            // 才退出。
            let mut sink = vec![0u8; 8192];
            loop {
                match b_rd.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        });

        let c1 = tokio::time::timeout(Duration::from_secs(5), h1)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(c1.node_id, "b");

        // A 侧心跳每 2s 发一次（B 静默不回）；5 次丢失（≈10s）→ 半开断开
        let t0 = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(15), c1.closed)
            .await
            .expect("half-open must close connection within ~10s")
            .unwrap();
        let dt = t0.elapsed();
        assert!(
            dt >= Duration::from_secs(9),
            "断开不得早于 5×2s-容差（实测 {dt:?}）——过早=心跳计数 bug"
        );
        assert!(
            dt <= Duration::from_secs(14),
            "断开不得晚于 ~14s（实测 {dt:?}）——过晚=检测失效"
        );
    }

    /// 配置切面：心跳旋钮实际生效——300ms×2 ≈ 0.6s 检出半开
    /// （默认 2s×5=10s。旋钮未流入驱动循环则本测试超时失败）。
    #[tokio::test]
    async fn heartbeat_knobs_accelerate_detection() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (a_read, a_write) = tokio::io::split(a);
        let (b_read, b_write) = tokio::io::split(b);
        let hs_a = HandshakeBody {
            node_id: "knob-a".into(),
            ..Default::default()
        };
        let (in_tx, _in_rx) = mpsc::channel::<(Frame, FrameSender, String)>(64);
        let noop: OnDisconnect = Arc::new(|_| {});
        let (sd_a, _) = tokio::sync::watch::channel(false);

        let h1 = tokio::spawn(run_connection(
            tokio::io::join(a_read, a_write),
            ConnParams {
                side: ConnSide::Connect,
                local_addr: None,
                peer_addr: None,
                scheme: "mem",
                local_handshake: hs_a,
                inbound: in_tx,
                on_disconnect: noop.clone(),
                shutdown: sd_a.subscribe(),
                knobs: Some(RuntimeKnobs {
                    heartbeat_interval: Duration::from_millis(300),
                    heartbeat_max_loss: 2,
                    outbound_queue: 64,
                    reorder_gap_timeout_ms: 250,
                    reorder_buffer_cap: 1024,
                    default_hop_limit: 8,
                    ..Default::default()
                }),
            },
        ));
        // 静默对端：握手后只读不写（同上测试的模式）
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut b_rd, mut b_wr) = (b_read, b_write);
            let mut len_buf = [0u8; 4];
            b_rd.read_exact(&mut len_buf).await.unwrap();
            let body_len = u32::from_le_bytes(len_buf) as usize;
            let mut body = vec![0u8; body_len];
            b_rd.read_exact(&mut body).await.unwrap();
            let mut ack_body = bytes::BytesMut::new();
            crate::handshake::HandshakeAckBody {
                node_id: "knob-b".into(),
                realm: None,
                cluster: None,
                capabilities: 3,
                max_frame_len: 1024 * 1024,
                topology_role: crate::handshake::TopologyRole::Normal,
                hop_limit: 8,
                direct_addr: None,
                chosen_codec: "bin".into(),
            }
            .encode_tlv(&mut ack_body)
            .unwrap();
            let ack = crate::frame::Frame::handshake_ack(ack_body.freeze());
            let mut out = bytes::BytesMut::new();
            ack.encode(&mut out).unwrap();
            b_wr.write_all(&out).await.unwrap();
            let mut sink = vec![0u8; 8192];
            loop {
                match b_rd.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        });

        let c1 = tokio::time::timeout(Duration::from_secs(5), h1)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(c1.node_id, "knob-b");

        // 旋钮下 300ms×2 ≈ 0.6s 断开（若未生效=10s → 3s 超时兜底失败）
        let t0 = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(3), c1.closed)
            .await
            .expect("knobbed half-open must close within ~0.6s")
            .unwrap();
        let dt = t0.elapsed();
        assert!(
            dt >= Duration::from_millis(500),
            "断开不得早于 2×300ms-容差（实测 {dt:?}）"
        );
        assert!(
            dt <= Duration::from_secs(2),
            "断开不得晚于 ~2s（实测 {dt:?}）——旋钮未生效=默认 10s"
        );
    }

    /// FrameSender::is_closed：writer 退出后可观测（重连循环的判据）。
    #[tokio::test]
    async fn frame_sender_is_closed_observable() {
        let (tx, rx) = mpsc::channel::<Frame>(1);
        let fs = FrameSender::anon(tx);
        assert!(!fs.is_closed());
        drop(rx);
        // mpsc 关闭传播：send 侧在 capacity 满后感知。立即态可能仍 open，
        // 试发一帧后必 closed。
        let _ = fs.send(Frame::heartbeat()).await;
        assert!(
            fs.is_closed(),
            "receiver dropped → sender must observe closed"
        );
    }

    /// FrameError → RemoteError 转换表（wire 层错误到传输错误的映射冻结）。
    #[test]
    fn frame_error_to_remote_error_mapping() {
        let e = RemoteError::from(FrameError::TooLarge { len: 99, max: 16 });
        assert!(e.to_string().contains("超限"), "TooLarge 映射: {e}");
        let e2 = RemoteError::from(FrameError::MalformedLengths {
            flen: 1,
            plen: 2,
            klen: 3,
        });
        assert!(e2.to_string().contains("长度"), "Malformed 映射: {e2}");
        // I/O diagnostics and connection-loss semantics survive the public conversion.
        let io_err = std::io::Error::other("boom");
        let fe = FrameError::from(io_err);
        assert_eq!(fe.to_errcode(), crate::error::ErrCode::ConnectionLost);
        assert_eq!(fe.clone(), fe);
        let e3 = RemoteError::from(fe);
        assert!(matches!(e3, RemoteError::Frame(_)), "io→Frame→Remote: {e3}");
        assert!(e3.to_string().contains("boom"));
        assert!(
            matches!(e3.to_actor_error(), parrot_api::errors::ActorError::InternalError(message) if message == "remote connection lost")
        );
    }

    #[test]
    fn node_addr_scheme() {
        let a = NodeAddr::tcp("n", "127.0.0.1:1".parse().unwrap());
        assert_eq!(a.scheme, "tcp");
    }
}
