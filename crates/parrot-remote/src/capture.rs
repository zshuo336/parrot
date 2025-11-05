//! Opt-in, bounded capture of decoded frames and outbound send attempts.
//! The transport never performs file I/O or waits for queue capacity.
use crate::frame::Frame;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
#[derive(Debug, Clone, Copy)]
pub struct CaptureLimits {
    pub capacity: usize,
    pub max_event_bytes: usize,
    pub max_pending_bytes: usize,
}
impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            capacity: 256,
            max_event_bytes: 20 * 1024 * 1024,
            max_pending_bytes: 64 * 1024 * 1024,
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureStats {
    pub observed: u64,
    pub queued: u64,
    pub lost: u64,
}
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("invalid capture capacity or byte limits")]
    InvalidLimits,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureDirection {
    Rx,
    Tx,
}
impl CaptureDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rx => "rx",
            Self::Tx => "tx",
        }
    }
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapturedWire {
    pub connection: u64,
    pub local_node: String,
    pub peer_node: Option<String>,
    pub scheme: String,
    pub elapsed_micros: u64,
    pub version: u8,
    pub flags: u16,
    pub hop_count: u8,
    pub hop_limit: u8,
    pub wire_seq: u32,
}
/// Permits remain held while a consumer processes this event, not just while queued.
pub struct CapturedFrame {
    pub seq: u64,
    pub direction: CaptureDirection,
    pub wire: CapturedWire,
    pub frame: Frame,
    _slot: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}
struct Shared {
    stopped: AtomicBool,
    active: AtomicUsize,
    observed: AtomicU64,
    queued: AtomicU64,
    lost: AtomicU64,
    waiter: OnceLock<std::thread::Thread>,
    started: Instant,
}
fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        Some(n.saturating_add(1))
    });
}
impl Shared {
    fn stats(&self) -> CaptureStats {
        CaptureStats {
            observed: self.observed.load(Ordering::Acquire),
            queued: self.queued.load(Ordering::Acquire),
            lost: self.lost.load(Ordering::Acquire),
        }
    }
    fn wait_quiescent(&self) {
        let _ = self.waiter.set(std::thread::current());
        while self.active.load(Ordering::SeqCst) != 0 {
            std::thread::park_timeout(Duration::from_millis(10));
        }
    }
}
struct Active(Arc<Shared>);
impl Drop for Active {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Some(t) = self.0.waiter.get() {
                t.unpark();
            }
        }
    }
}
struct State {
    sender: Option<mpsc::Sender<CapturedFrame>>,
    next_seq: Option<u64>,
}
struct Producer {
    state: Mutex<State>,
    shared: Arc<Shared>,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    limits: CaptureLimits,
    next_connection: AtomicU64,
}
#[derive(Clone)]
pub struct FrameCapture(Arc<Producer>);
pub struct CaptureReceiver {
    receiver: mpsc::Receiver<CapturedFrame>,
    producer: Weak<Producer>,
    shared: Arc<Shared>,
}
pub struct ConnectionCapture {
    capture: FrameCapture,
    id: u64,
    local_node: String,
    peer_node: OnceLock<String>,
    scheme: &'static str,
}
impl FrameCapture {
    pub fn channel(limits: CaptureLimits) -> Result<(Self, CaptureReceiver), CaptureError> {
        if limits.capacity == 0
            || limits.capacity > 65_536
            || limits.max_event_bytes == 0
            || limits.max_event_bytes > limits.max_pending_bytes
            || limits.max_pending_bytes > 64 * 1024 * 1024
        {
            return Err(CaptureError::InvalidLimits);
        }
        let (sender, receiver) = mpsc::channel(limits.capacity);
        let shared = Arc::new(Shared {
            stopped: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            observed: AtomicU64::new(0),
            queued: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            waiter: OnceLock::new(),
            started: Instant::now(),
        });
        let inner = Arc::new(Producer {
            state: Mutex::new(State {
                sender: Some(sender),
                next_seq: Some(0),
            }),
            shared: shared.clone(),
            slots: Arc::new(Semaphore::new(limits.capacity)),
            bytes: Arc::new(Semaphore::new(limits.max_pending_bytes)),
            limits,
            next_connection: AtomicU64::new(1),
        });
        Ok((
            Self(inner.clone()),
            CaptureReceiver {
                receiver,
                producer: Arc::downgrade(&inner),
                shared,
            },
        ))
    }
    pub fn stats(&self) -> CaptureStats {
        self.0.shared.stats()
    }
    /// End the capture interval and drain in-progress memory-copy offers.
    /// This is not a file sync or a timed shutdown; call outside latency-sensitive code.
    pub fn close(&self) {
        self.0.shared.stopped.store(true, Ordering::SeqCst);
        self.0.shared.wait_quiescent();
        self.0.state.lock().unwrap().sender.take();
    }
    pub fn connection(
        &self,
        local_node: impl Into<String>,
        scheme: &'static str,
    ) -> Option<ConnectionCapture> {
        if self.0.shared.stopped.load(Ordering::SeqCst) {
            return None;
        }
        self.0.shared.active.fetch_add(1, Ordering::SeqCst);
        let _active = Active(self.0.shared.clone());
        if self.0.shared.stopped.load(Ordering::SeqCst) {
            return None;
        }
        let id =
            match self
                .0
                .next_connection
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            {
                Ok(id) => id,
                Err(_) => {
                    increment(&self.0.shared.lost);
                    return None;
                }
            };
        Some(ConnectionCapture {
            capture: self.clone(),
            id,
            local_node: local_node.into(),
            peer_node: OnceLock::new(),
            scheme,
        })
    }
}
impl ConnectionCapture {
    /// Set only after the connection driver has completed its identity checks.
    pub(crate) fn established(&self, peer_node: &str) {
        let _ = self.peer_node.set(peer_node.into());
    }
    pub fn observe(&self, direction: CaptureDirection, frame: &Frame) {
        let p = &self.capture.0;
        if p.shared.stopped.load(Ordering::SeqCst) {
            return;
        }
        p.shared.active.fetch_add(1, Ordering::SeqCst);
        let _active = Active(p.shared.clone());
        if p.shared.stopped.load(Ordering::SeqCst) {
            return;
        }
        increment(&p.shared.observed);
        let offered = (|| {
            let slot = p.slots.clone().try_acquire_owned().ok()?;
            let size = [
                frame.payload.len(),
                frame.path.len(),
                frame.type_key.len(),
                self.local_node.len(),
                self.peer_node.get().map_or(0, String::len),
                self.scheme.len(),
                512,
            ]
            .into_iter()
            .try_fold(0usize, usize::checked_add)?;
            if size > p.limits.max_event_bytes {
                return None;
            }
            let bytes = p.bytes.clone().try_acquire_many_owned(size as u32).ok()?;
            // Copy tightly: a small Bytes slice may retain a much larger input buffer.
            let frame = Frame {
                header: frame.header.clone(),
                path: frame.path.clone(),
                type_key: frame.type_key.clone(),
                payload: bytes::Bytes::copy_from_slice(&frame.payload),
            };
            let wire = CapturedWire {
                connection: self.id,
                local_node: self.local_node.clone(),
                peer_node: self.peer_node.get().cloned(),
                scheme: self.scheme.into(),
                elapsed_micros: p.shared.started.elapsed().as_micros().min(u64::MAX as u128) as u64,
                version: frame.header.version,
                flags: frame.header.flags,
                hop_count: frame.header.hop_count,
                hop_limit: frame.header.hop_limit,
                wire_seq: frame.header.seq,
            };
            // Serialize only reservation/sequence/publication. Temporary mutex
            // contention is not capacity exhaustion and must not lose a frame.
            // Copying, user code, I/O and queue waits stay outside this section.
            let mut state = p.state.lock().ok()?;
            let permit = state.sender.as_ref()?.clone().try_reserve_owned().ok()?;
            let seq = state.next_seq?;
            state.next_seq = seq.checked_add(1);
            permit.send(CapturedFrame {
                seq,
                direction,
                wire,
                frame,
                _slot: slot,
                _bytes: bytes,
            });
            increment(&p.shared.queued);
            Some(())
        })();
        if offered.is_none() {
            increment(&p.shared.lost);
        }
    }
}
impl CaptureReceiver {
    /// For a dedicated recording thread. Calling this inside a Tokio task panics.
    pub fn blocking_recv(&mut self) -> Option<CapturedFrame> {
        self.receiver.blocking_recv()
    }
    pub fn close(&self) {
        if let Some(p) = self.producer.upgrade() {
            FrameCapture(p).close();
        }
    }
    /// Read a settled receipt after close and after the receiver is drained.
    pub fn final_stats(&self) -> CaptureStats {
        self.close();
        self.shared.wait_quiescent();
        self.shared.stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame() -> Frame {
        Frame::tell("/actor", "test", bytes::Bytes::from_static(b"abc"))
    }
    #[test]
    fn consumer_holds_slot_and_close_drains_without_sender_drop() {
        let (capture, mut receiver) = FrameCapture::channel(CaptureLimits {
            capacity: 1,
            ..Default::default()
        })
        .unwrap();
        let conn = capture.connection("local", "mem").unwrap();
        conn.observe(CaptureDirection::Rx, &frame());
        let first = receiver.blocking_recv().unwrap();
        conn.observe(CaptureDirection::Tx, &frame());
        assert_eq!(
            capture.stats(),
            CaptureStats {
                observed: 2,
                queued: 1,
                lost: 1
            }
        );
        drop(first);
        conn.observe(CaptureDirection::Tx, &frame());
        capture.close();
        let second = receiver.blocking_recv().unwrap();
        assert_eq!(second.seq, 1);
        assert_eq!(&second.frame.payload[..], b"abc");
        conn.observe(CaptureDirection::Tx, &frame());
        assert!(receiver.blocking_recv().is_none());
        assert_eq!(
            receiver.final_stats(),
            CaptureStats {
                observed: 3,
                queued: 2,
                lost: 1
            }
        );
    }
    #[test]
    fn byte_budget_applies_to_inflight_copies_and_rejects_large_events() {
        let (capture, mut receiver) = FrameCapture::channel(CaptureLimits {
            capacity: 10,
            max_event_bytes: 600,
            max_pending_bytes: 600,
        })
        .unwrap();
        let conn = capture.connection("local", "mem").unwrap();
        conn.observe(CaptureDirection::Rx, &frame());
        let held = receiver.blocking_recv().unwrap();
        conn.observe(CaptureDirection::Rx, &frame());
        drop(held);
        let mut huge = frame();
        huge.payload = vec![0; 601].into();
        conn.observe(CaptureDirection::Rx, &huge);
        conn.observe(CaptureDirection::Rx, &frame());
        capture.close();
        assert!(receiver.blocking_recv().is_some());
        assert!(receiver.blocking_recv().is_none());
        assert_eq!(
            receiver.final_stats(),
            CaptureStats {
                observed: 4,
                queued: 2,
                lost: 2
            }
        );
    }
    #[test]
    fn dropped_receiver_and_sequence_exhaustion_are_losses() {
        let (capture, receiver) = FrameCapture::channel(Default::default()).unwrap();
        drop(receiver);
        capture
            .connection("node", "mem")
            .unwrap()
            .observe(CaptureDirection::Rx, &frame());
        assert_eq!(capture.stats().lost, 1);
        let (capture, mut receiver) = FrameCapture::channel(Default::default()).unwrap();
        capture.0.state.lock().unwrap().next_seq = Some(u64::MAX);
        let conn = capture.connection("node", "mem").unwrap();
        conn.observe(CaptureDirection::Rx, &frame());
        conn.observe(CaptureDirection::Rx, &frame());
        capture.0.next_connection.store(u64::MAX, Ordering::Relaxed);
        assert!(capture.connection("node", "mem").is_none());
        capture.close();
        assert_eq!(receiver.blocking_recv().unwrap().seq, u64::MAX);
        assert!(receiver.blocking_recv().is_none());
        assert_eq!(receiver.final_stats().lost, 2);
    }
    #[test]
    fn concurrent_publishers_and_close_produce_settled_receipt() {
        let (capture, mut receiver) = FrameCapture::channel(Default::default()).unwrap();
        let consumer = std::thread::spawn(move || {
            let mut seq = 0;
            while let Some(frame) = receiver.blocking_recv() {
                assert_eq!(frame.seq, seq);
                seq += 1;
            }
            (seq, receiver.final_stats())
        });
        std::thread::scope(|scope| {
            let barrier = Arc::new(std::sync::Barrier::new(9));
            for _ in 0..8 {
                let capture = capture.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    let conn = capture.connection("node", "mem").unwrap();
                    barrier.wait();
                    for _ in 0..1000 {
                        conn.observe(CaptureDirection::Rx, &frame());
                    }
                });
            }
            barrier.wait();
            capture.close();
        });
        let (count, stats) = consumer.join().unwrap();
        assert_eq!(stats.queued, count);
        assert_eq!(stats.observed, stats.queued + stats.lost);
        assert_eq!(capture.stats(), stats);
    }
}
