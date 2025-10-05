//! 职责：Transport trait——连接建立的机制；载体（tcp/quic/ws/mem）是策略（E5.3）。
//!
//! ConnectionTask：每连接一个 tokio 任务（写侧 mpsc 串行化 + 读循环 + 心跳
//! 半开判定 + 断连回调），DEV_01 §3.6 时序图的落地。

use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;

use std::sync::Arc;
use std::time::{Duration, Instant};

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
}

impl FrameSender {
    /// 构造（peer_node 已知——转发失效定位用）。
    pub fn new(tx: mpsc::Sender<Frame>, peer_node: impl Into<String>) -> Self {
        let cell = Arc::new(std::sync::OnceLock::new());
        let _ = cell.set(peer_node.into());
        Self { tx, peer_node: cell }
    }

    /// 测试构造（无 peer 标注——node_id() 返回 "?"）。
    pub fn anon(tx: mpsc::Sender<Frame>) -> Self {
        Self {
            tx,
            peer_node: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// 对端 node_id（未标注返回 "?"——转发失效定位退化为全表扫描）。
    pub fn node_id(&self) -> &str {
        self.peer_node.get().map(String::as_str).unwrap_or("?")
    }

    pub async fn send(&self, f: Frame) -> Result<(), RemoteError> {
        self.tx
            .send(f)
            .await
            .map_err(|_| RemoteError::Transport("writer dropped".into()))
    }
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// 便捷回源（relayed reply——REPLY/REPLY_ERR 同入口：换 cid 原样回送）。
    pub fn send_frame(&self, f: Frame) {
        let _ = self.tx.try_send(f);
    }

    /// 同步转发（TELL 中转——队列满返回 Err，调用方记死信）。
    pub fn send_frame_sync(&self, f: Frame) -> Result<(), RemoteError> {
        self.tx.try_send(f).map_err(|_| RemoteError::Transport("relay queue full".into()))
    }
}

/// 传输载体抽象。P1：tcp/mem；P2：quic（07 §8.1 scheme() 统一命名）。
#[async_trait::async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn connect(&self, addr: &NodeAddr) -> Result<ConnectionHandle, RemoteError>;
    async fn listen(&self, bind: SocketAddr) -> Result<(), RemoteError>;
    async fn accept(&self) -> Result<ConnectionHandle, RemoteError>;
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

/// tokio_util codec 薄封装：encode → Frame::encode；decode → Frame::decode。
#[derive(Clone, Default)]
pub struct FrameCodec;

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
}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        // IO 层错误（对端关闭/复位）——统一映射协议违规类（连接层断连处理）
        FrameError::MalformedLengths {
            flen: 0,
            plen: 0,
            klen: 0,
        }
        .tap_io(e)
    }
}

trait TapIo {
    fn tap_io(self, e: std::io::Error) -> Self;
}
impl TapIo for FrameError {
    fn tap_io(self, e: std::io::Error) -> Self {
        tracing::debug!(io = %e, "framed io error");
        self
    }
}

/// 断连时的清理回调（RemoteActorSystem 注入：fail_all + NodeTable 更新）。
pub type OnDisconnect = Arc<dyn Fn(&str) + Send + Sync>;

/// 建连模式（主动 connect / 被动 accept 决定握手先后手）。
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
}

pub async fn run_connection<S>(io: S, p: ConnParams) -> Result<ConnectionHandle, RemoteError>
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
    } = p;
    let mut framed = tokio_util::codec::Framed::new(io, FrameCodec);
    let (tx, rx) = mpsc::channel::<Frame>(OUTBOUND_QUEUE);

    // ---- 握手阶段 ----
    let peer_body: HandshakeBody = match side {
        ConnSide::Connect => {
            let mut body = BytesMut::new();
            local_handshake.encode_tlv(&mut body);
            framed
                .send(Frame::handshake(body.freeze()))
                .await
                .map_err(|e| RemoteError::Handshake(format!("send: {e}")))?;
            let ack = recv_expect(&mut framed, frame_type::HANDSHAKE_ACK).await?;
            let ack_body = HandshakeAckBody::decode_tlv(&ack.payload)
                .map_err(|e| RemoteError::Handshake(format!("ack decode: {e}")))?;
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
            let hs = recv_expect(&mut framed, frame_type::HANDSHAKE).await?;
            let theirs = HandshakeBody::decode_tlv(&hs.payload)
                .map_err(|e| RemoteError::Handshake(format!("decode: {e}")))?;
            negotiate_caps(local_handshake.capabilities, theirs.capabilities)
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
                chosen_codec: "bin".into(),
            };
            let mut body = BytesMut::new();
            mine_ack.encode_tlv(&mut body);
            framed
                .send(Frame::handshake_ack(body.freeze()))
                .await
                .map_err(|e| RemoteError::Handshake(format!("ack send: {e}")))?;
            theirs
        }
    };
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
    let back_sender = FrameSender::new(tx.clone(), remote_node_id.as_str());
    let from_node = remote_node_id.clone();
    tokio::spawn(async move {
        let mut rx = rx;
        let mut missed: u32 = 0;
        let mut last_seen = Instant::now();
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        if *shutdown.borrow() {
            dis(&node_id_cb);
            let _ = closed_tx.send(());
            return;
        }
        // watch sender 消亡 ≠ 关连接（只有显式 true 或 IO 断才关）；
        // sender 消亡后 changed() 恒 Ready(Err)——必须停 poll 该臂防 biased 饿死。
        let mut shutdown_alive = true;
        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed(), if shutdown_alive => match changed {
                    Ok(_) => {
                        if *shutdown.borrow() {
                            break;
                        }
                    }
                    Err(_) => shutdown_alive = false,
                },
                out = rx.recv() => {
                    match out {
                        Some(frame) => {
                            if tracing::enabled!(tracing::Level::TRACE)
                                || crate::frame::trace_enabled()
                            {
                                eprintln!("[parrot-trace] TX {}", frame.trace_line());
                            }
                            if framed.send(frame).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                r = framed.next() => {
                    match r {
                        Some(Ok(frame)) => {
                            last_seen = Instant::now();
                            if tracing::enabled!(tracing::Level::TRACE)
                                || crate::frame::trace_enabled()
                            {
                                eprintln!("[parrot-trace] RX {}", frame.trace_line());
                            }
                            missed = 0;
                            match frame.header.frame_type {
                                frame_type::HEARTBEAT => {
                                    let _ = framed.send(Frame::heartbeat_ack()).await;
                                }
                                frame_type::HANDSHAKE | frame_type::HANDSHAKE_ACK => {
                                    // 握手后重发 = 协议违规（07 §2.2 规则）
                                    let _ = framed
                                        .send(Frame::error_frame(
                                            crate::error::ErrCode::ProtocolViolation,
                                            "handshake frame after established",
                                        ))
                                        .await;
                                    break;
                                }
                                frame_type::SYSTEM_EVENT => {
                                    // P2 解禁：管理/集群载荷入站分发（admin/gossip/receptionist
                                    // 同帧不同标签——ingress 按 sys_event_tag 分流）
                                    let _ = inbound
                                        .send((frame, back_sender.clone(), from_node.clone()))
                                        .await;
                                }
                                frame_type::RESOLVE_Q
                                | frame_type::RESOLVE_R
                                | frame_type::INVALIDATE => {
                                    // P5 联邦帧——收到即断连（协议违规）
                                    let _ = framed
                                        .send(Frame::error_frame(
                                            crate::error::ErrCode::ProtocolViolation,
                                            "federation frame not supported before P5",
                                        ))
                                        .await;
                                    break;
                                }
                                frame_type::FRAGMENT => {
                                    let _ = framed
                                        .send(Frame::error_frame(
                                            crate::error::ErrCode::ProtocolViolation,
                                            "FRAGMENT not implemented in P1",
                                        ))
                                        .await;
                                    break;
                                }
                                _ => {
                                    // ASK/TELL/STOP/REPLY/REPLY_ERR/ERROR → 入站分发
                                    let _ = inbound
                                        .send((frame, back_sender.clone(), from_node.clone()))
                                        .await;
                                }
                            }
                        }
                        Some(Err(_)) | None => break,
                    }
                }
                _ = interval.tick() => {
                    if last_seen.elapsed() >= HEARTBEAT_INTERVAL {
                        missed = missed.saturating_add(1);
                    }
                    if missed >= HEARTBEAT_MAX_LOSS {
                        // 半开判定（E3：≤10s 检出）
                        tracing::warn!(node = %node_id_cb, "heartbeat lost x{HEARTBEAT_MAX_LOSS}, half-open detected");
                        break;
                    }
                    if framed.send(Frame::heartbeat()).await.is_err() {
                        break;
                    }
                }
            }
        }
        dis(&node_id_cb);
        let _ = closed_tx.send(());
    });

    Ok(ConnectionHandle {
        node_id: remote_node_id.clone(),
        sender: FrameSender::new(tx, remote_node_id),
        info,
        peer_role,
        peer_dial_addr,
        closed: closed_rx,
    })
}

async fn recv_expect<S>(
    framed: &mut tokio_util::codec::Framed<S, FrameCodec>,
    expect: u8,
) -> Result<Frame, RemoteError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use futures::StreamExt;
    let deadline = Duration::from_secs(5);
    match tokio::time::timeout(deadline, framed.next()).await {
        Ok(Some(Ok(f))) if f.header.frame_type == expect => Ok(f),
        Ok(Some(Ok(f))) => Err(RemoteError::Handshake(format!(
            "expected frame 0x{expect:02X}, got 0x{:02X}",
            f.header.frame_type
        ))),
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
        tokio::time::sleep(Duration::from_millis(100)).await;
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
                },
                path: String::new(),
                type_key: String::new(),
                payload: bytes::Bytes::new(),
            })
            .await
            .unwrap();
        // 过滤心跳 ACK，等 SYSTEM_EVENT 到 a 侧 inbound
        let mut got_sys = None;
        for _ in 0..10 {
            let (f, _, _) = tokio::time::timeout(Duration::from_secs(10), in_rx_a.recv())
                .await
                .unwrap()
                .unwrap();
            if f.header.frame_type == frame_type::SYSTEM_EVENT {
                got_sys = Some(f);
                break;
            }
        }
        assert!(got_sys.is_some(), "SYSTEM_EVENT not dispatched to inbound");
        // RESOLVE_Q → 断连（P5 前协议违规）
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
                },
                path: String::new(),
                type_key: String::new(),
                payload: bytes::Bytes::new(),
            })
            .await
            .unwrap();
        // 连接应关闭（closed 信号）
        tokio::time::timeout(Duration::from_secs(2), c1.closed)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn node_addr_scheme() {
        let a = NodeAddr::tcp("n", "127.0.0.1:1".parse().unwrap());
        assert_eq!(a.scheme, "tcp");
    }
}
