//! Availability-aware metrics. Legacy admin-v2 snapshots keep their exact bytes.
//! New samples use complete valid Wire frames including the four-byte length
//! prefix, across all frame types. TX counts only successful complete flushes;
//! a failed partial write is excluded. These are transport events, not business
//! completion receipts. Independent atomic fields are not a transactional view.
use crate::{
    admin_v2::MetricsSnapshot,
    frame::{frame_type, Frame},
};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

/// Stable field mask for the measured-metrics wire extension.
pub mod field {
    pub const TS_MS: u64 = 1 << 0;
    pub const RUNTIME: u64 = 1 << 1;
    pub const CONNECTIONS: u64 = 1 << 2;
    pub const HANDSHAKES_OK: u64 = 1 << 3;
    pub const HANDSHAKES_FAILED: u64 = 1 << 4;
    pub const ASKS_RX: u64 = 1 << 5;
    pub const TELLS_RX: u64 = 1 << 6;
    pub const REPLIES_TX: u64 = 1 << 7;
    pub const REPLY_ERRS: u64 = 1 << 8;
    pub const BYTES_RX: u64 = 1 << 9;
    pub const BYTES_TX: u64 = 1 << 10;
    pub const HEARTBEATS_RX: u64 = 1 << 11;
    pub const COMPONENTS: u64 = 1 << 12;
    pub const COMPONENT_STATES: u64 = 1 << 13;
    pub const PROCESSES: u64 = 1 << 14;
    pub const MEMORY_RSS: u64 = 1 << 15;
    pub const UPTIME_START_MS: u64 = 1 << 16;
    pub const ALL: u64 = (1 << 17) - 1;
    pub const TRANSPORT: u64 = CONNECTIONS
        | HANDSHAKES_OK
        | HANDSHAKES_FAILED
        | ASKS_RX
        | TELLS_RX
        | REPLIES_TX
        | REPLY_ERRS
        | BYTES_RX
        | BYTES_TX
        | HEARTBEATS_RX;
}

/// An unset field bit means unavailable, even if its legacy numeric slot is zero
/// or nonzero. Epoch identifies one counter owner, not a wall-clock timestamp.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MeasuredMetrics {
    pub snapshot: MetricsSnapshot,
    pub available: u64,
    pub epoch: String,
    /// Unit of `processes`, e.g. `parrot_actors`; never compare different units.
    pub process_kind: String,
}
impl std::ops::Deref for MeasuredMetrics {
    type Target = MetricsSnapshot;
    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}
impl MeasuredMetrics {
    pub fn has(&self, fields: u64) -> bool {
        self.available & fields == fields
    }
    pub fn value(&self, field: u64) -> Option<u64> {
        if !self.has(field) {
            return None;
        }
        Some(match field {
            field::TS_MS => self.snapshot.ts_ms,
            field::CONNECTIONS => self.snapshot.connections,
            field::HANDSHAKES_OK => self.snapshot.handshakes_ok,
            field::HANDSHAKES_FAILED => self.snapshot.handshakes_failed,
            field::ASKS_RX => self.snapshot.asks_rx,
            field::TELLS_RX => self.snapshot.tells_rx,
            field::REPLIES_TX => self.snapshot.replies_tx,
            field::REPLY_ERRS => self.snapshot.reply_errs,
            field::BYTES_RX => self.snapshot.bytes_rx,
            field::BYTES_TX => self.snapshot.bytes_tx,
            field::HEARTBEATS_RX => self.snapshot.heartbeats_rx,
            field::COMPONENTS => self.snapshot.components,
            field::PROCESSES => self.snapshot.processes,
            field::MEMORY_RSS => self.snapshot.memory_rss,
            field::UPTIME_START_MS => self.snapshot.uptime_start_ms,
            _ => return None,
        })
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.available & !field::ALL != 0 {
            return Err("unknown metrics availability bit");
        }
        if self.epoch.len() > 128 || self.process_kind.len() > 128 {
            return Err("metrics identity too long");
        }
        if self.has(field::PROCESSES) && self.process_kind.is_empty() {
            return Err("missing process metric unit");
        }
        Ok(())
    }
    /// Component executors may contribute only their owned runtime fields.
    pub(crate) fn merge_runtime(&mut self, other: Self) {
        for bit in [field::COMPONENTS, field::COMPONENT_STATES, field::PROCESSES] {
            if other.has(bit) {
                match bit {
                    field::COMPONENTS => self.snapshot.components = other.components,
                    field::COMPONENT_STATES => {
                        self.snapshot.component_states = other.component_states.clone()
                    }
                    field::PROCESSES => {
                        self.snapshot.processes = other.processes;
                        self.process_kind = other.process_kind.clone();
                    }
                    _ => unreachable!(),
                }
                self.available |= bit;
            }
        }
    }
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn add(counter: &AtomicU64, count: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        Some(n.saturating_add(count))
    });
}

/// Shared by every transport driver of one RemoteActorSystem, including direct
/// in-memory pairs. Dropping one connection cannot reset the aggregate counters.
pub(crate) struct TransportMetrics {
    epoch: String,
    started: u64,
    connections: AtomicU64,
    handshakes_ok: AtomicU64,
    handshakes_failed: AtomicU64,
    asks_rx: AtomicU64,
    tells_rx: AtomicU64,
    replies_tx: AtomicU64,
    reply_errs: AtomicU64,
    bytes_rx: AtomicU64,
    bytes_tx: AtomicU64,
    heartbeats_rx: AtomicU64,
}
impl Default for TransportMetrics {
    fn default() -> Self {
        Self {
            epoch: uuid::Uuid::new_v4().to_string(),
            started: unix_ms(),
            connections: 0.into(),
            handshakes_ok: 0.into(),
            handshakes_failed: 0.into(),
            asks_rx: 0.into(),
            tells_rx: 0.into(),
            replies_tx: 0.into(),
            reply_errs: 0.into(),
            bytes_rx: 0.into(),
            bytes_tx: 0.into(),
            heartbeats_rx: 0.into(),
        }
    }
}
#[derive(Clone, Copy)]
pub(crate) struct FrameMetric {
    kind: u8,
    bytes: u64,
}
impl FrameMetric {
    pub(crate) fn of(frame: &Frame) -> Self {
        Self {
            kind: frame.header.frame_type,
            bytes: (4
                + crate::frame::BODY_FIXED_OVERHEAD
                + frame.path.len()
                + frame.type_key.len()
                + frame.payload.len()) as u64,
        }
    }
}
impl TransportMetrics {
    pub(crate) fn received(&self, frame: &Frame) {
        let frame = FrameMetric::of(frame);
        add(&self.bytes_rx, frame.bytes);
        match frame.kind {
            frame_type::ASK => add(&self.asks_rx, 1),
            frame_type::TELL => add(&self.tells_rx, 1),
            frame_type::HEARTBEAT => add(&self.heartbeats_rx, 1),
            _ => {}
        }
    }
    pub(crate) fn sent(&self, frame: FrameMetric) {
        add(&self.bytes_tx, frame.bytes);
        if matches!(frame.kind, frame_type::REPLY | frame_type::REPLY_ERR) {
            add(&self.replies_tx, 1);
        }
        if frame.kind == frame_type::REPLY_ERR {
            add(&self.reply_errs, 1);
        }
    }
    pub(crate) fn snapshot(&self) -> MeasuredMetrics {
        let get = |v: &AtomicU64| v.load(Ordering::Relaxed);
        let mut result = MeasuredMetrics {
            snapshot: MetricsSnapshot {
                ts_ms: unix_ms(),
                runtime: format!("parrot-remote/{}", env!("CARGO_PKG_VERSION")),
                connections: get(&self.connections),
                handshakes_ok: get(&self.handshakes_ok),
                handshakes_failed: get(&self.handshakes_failed),
                asks_rx: get(&self.asks_rx),
                tells_rx: get(&self.tells_rx),
                replies_tx: get(&self.replies_tx),
                reply_errs: get(&self.reply_errs),
                bytes_rx: get(&self.bytes_rx),
                bytes_tx: get(&self.bytes_tx),
                heartbeats_rx: get(&self.heartbeats_rx),
                uptime_start_ms: self.started,
                ..Default::default()
            },
            available: field::TRANSPORT | field::TS_MS | field::RUNTIME | field::UPTIME_START_MS,
            epoch: self.epoch.clone(),
            process_kind: String::new(),
        };
        if let Some(rss) = resident_bytes() {
            result.snapshot.memory_rss = rss;
            result.available |= field::MEMORY_RSS;
        }
        result
    }
}

pub(crate) struct ConnectionMetricLease {
    metrics: Arc<TransportMetrics>,
    established: bool,
}
impl ConnectionMetricLease {
    pub(crate) fn new(metrics: Arc<TransportMetrics>) -> Self {
        Self {
            metrics,
            established: false,
        }
    }
    pub(crate) fn established(&mut self) {
        assert!(!self.established);
        self.established = true;
        add(&self.metrics.handshakes_ok, 1);
        add(&self.metrics.connections, 1);
    }
}
impl Drop for ConnectionMetricLease {
    fn drop(&mut self) {
        if self.established {
            self.metrics.connections.fetch_sub(1, Ordering::Relaxed);
        } else {
            add(&self.metrics.handshakes_failed, 1);
        }
    }
}

#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<u64> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open("/proc/self/status")
        .ok()?
        .take(65536)
        .read_to_string(&mut text)
        .ok()?;
    text.lines().find_map(|line| {
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        if fields.len() == 3 && fields[0] == "VmRSS:" && fields[2] == "kB" {
            fields[1].parse::<u64>().ok()?.checked_mul(1024)
        } else {
            None
        }
    })
}

#[cfg(not(target_os = "linux"))]
fn resident_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{run_connection_owned, ConnParams, ConnSide, ConnectionTasks};
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::{
        io::{AsyncRead, AsyncWrite, ReadBuf},
        sync::{mpsc, watch},
    };

    #[test]
    fn complete_wire_lengths_include_utf8_paths_keys_reply_prefix_and_frame_header() {
        for frame in [
            Frame::ask(
                1,
                "/用户/actor",
                "bin:消息",
                bytes::Bytes::from_static(b"body"),
                Some("/return"),
            ),
            Frame::reply_err(2, "/路径", crate::error::ErrCode::Timeout, "failure"),
            Frame::heartbeat(),
        ] {
            let mut wire = bytes::BytesMut::new();
            frame.encode(&mut wire).unwrap();
            assert_eq!(FrameMetric::of(&frame).bytes, wire.len() as u64);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actual_driver_counts_exact_complete_frames_and_replies() {
        let a = ConnectionTasks::default();
        let b = ConnectionTasks::default();
        let (io_a, io_b) = tokio::io::duplex(65536);
        let (tx_a, mut rx_a) = mpsc::channel(4);
        let (tx_b, mut rx_b) = mpsc::channel(4);
        let (stop, _) = watch::channel(false);
        let params = |name: &str, side, inbound| ConnParams {
            side,
            local_addr: None,
            peer_addr: None,
            scheme: "mem",
            local_handshake: crate::handshake::HandshakeBody {
                node_id: name.into(),
                ..Default::default()
            },
            inbound,
            on_disconnect: Arc::new(|_| {}),
            shutdown: stop.subscribe(),
            knobs: Some(crate::transport::RuntimeKnobs {
                heartbeat_enabled: false,
                ..Default::default()
            }),
        };
        let (ca, cb) = tokio::try_join!(
            run_connection_owned(io_a, params("a", ConnSide::Connect, tx_a), Some(&a)),
            run_connection_owned(io_b, params("b", ConnSide::Accept, tx_b), Some(&b))
        )
        .unwrap();
        let before_a = a.metrics.snapshot();
        let before_b = b.metrics.snapshot();
        assert_eq!(before_a.handshakes_ok, 1);
        assert_eq!(before_b.handshakes_ok, 1);
        let frame = Frame::tell("/用户", "bin:消息", bytes::Bytes::from_static(b"body"));
        let mut wire = bytes::BytesMut::new();
        frame.encode(&mut wire).unwrap();
        let sent_len = wire.len() as u64;
        ca.sender.send(frame.clone()).await.unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), rx_b.recv())
            .await
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(received.payload, frame.payload);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while a.metrics.bytes_tx.load(Ordering::Relaxed) != before_a.bytes_tx + sent_len {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(b.metrics.snapshot().bytes_rx - before_b.bytes_rx, sent_len);
        assert_eq!(b.metrics.snapshot().tells_rx, 1);
        let reply = Frame::reply_err(7, "/return", crate::error::ErrCode::Timeout, "failed");
        let mut wire = bytes::BytesMut::new();
        reply.encode(&mut wire).unwrap();
        let reply_len = wire.len() as u64;
        cb.sender.send(reply).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), rx_a.recv())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while b.metrics.bytes_tx.load(Ordering::Relaxed) != before_b.bytes_tx + reply_len {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let after = b.metrics.snapshot();
        assert_eq!(after.replies_tx, 1);
        assert_eq!(after.reply_errs, 1);
        assert_eq!(a.metrics.snapshot().bytes_rx - before_a.bytes_rx, reply_len);
        a.close();
        b.close();
        a.wait().await;
        b.wait().await;
        assert_eq!(a.metrics.snapshot().connections, 0);
        assert_eq!(b.metrics.snapshot().connections, 0);
    }

    struct PartialWrite {
        remaining: usize,
        written: Arc<AtomicU64>,
    }
    impl AsyncRead for PartialWrite {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }
    impl AsyncWrite for PartialWrite {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.remaining == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected partial write",
                )));
            }
            let n = self.remaining.min(buf.len());
            self.remaining -= n;
            self.written.fetch_add(n as u64, Ordering::Relaxed);
            Poll::Ready(Ok(n))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    #[tokio::test]
    async fn failed_partial_frame_write_is_not_reported_as_successful_tx() {
        let owner = ConnectionTasks::default();
        let (tx, _rx) = mpsc::channel(1);
        let (stop, _) = watch::channel(false);
        let written = Arc::new(AtomicU64::new(0));
        let result = run_connection_owned(
            PartialWrite {
                remaining: 7,
                written: written.clone(),
            },
            ConnParams {
                side: ConnSide::Connect,
                local_addr: None,
                peer_addr: None,
                scheme: "mem",
                local_handshake: crate::handshake::HandshakeBody {
                    node_id: "metrics-test".into(),
                    ..Default::default()
                },
                inbound: tx,
                on_disconnect: Arc::new(|_| {}),
                shutdown: stop.subscribe(),
                knobs: None,
            },
            Some(&owner),
        )
        .await;
        assert!(
            matches!(result, Err(crate::RemoteError::Handshake(ref error)) if error.starts_with("send:"))
        );
        assert_eq!(
            written.load(Ordering::Relaxed),
            7,
            "must exercise a partial network write"
        );
        let sample = owner.metrics.snapshot();
        assert_eq!(sample.bytes_tx, 0);
        assert_eq!(sample.handshakes_ok, 0);
        assert_eq!(sample.handshakes_failed, 1);
        assert_eq!(sample.connections, 0);
    }

    #[tokio::test]
    async fn cancelled_handshake_retires_the_attempt_without_an_active_connection() {
        let owner = ConnectionTasks::default();
        let (io, _peer) = tokio::io::duplex(4096);
        let (tx, _rx) = mpsc::channel(1);
        let (stop, _) = watch::channel(false);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            run_connection_owned(
                io,
                ConnParams {
                    side: ConnSide::Accept,
                    local_addr: None,
                    peer_addr: None,
                    scheme: "mem",
                    local_handshake: crate::handshake::HandshakeBody {
                        node_id: "metrics-test".into(),
                        ..Default::default()
                    },
                    inbound: tx,
                    on_disconnect: Arc::new(|_| {}),
                    shutdown: stop.subscribe(),
                    knobs: None,
                },
                Some(&owner),
            ),
        )
        .await;
        assert!(result.is_err());
        let sample = owner.metrics.snapshot();
        assert_eq!(sample.connections, 0);
        assert_eq!(sample.handshakes_failed, 1);
        assert_eq!(sample.handshakes_ok, 0);
        assert_eq!(sample.bytes_tx, 0);
        assert_eq!(sample.bytes_rx, 0);
    }
}
