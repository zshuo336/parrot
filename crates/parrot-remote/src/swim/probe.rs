//! Correlated membership control messages and bounded, cancellation-safe requests.
use crate::{
    admin::sys_event_tag,
    error::RemoteError,
    frame::{frame_type, Frame},
    transport::FrameSender,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{oneshot, watch, Semaphore};

pub(crate) const MAX_PROBE_MS: u32 = 10_000;
const MAX_BODY: usize = 65_535 + 64;
const MAX_PENDING: usize = 128;
const MAX_HELPERS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeRequest {
    pub request_id: u64,
    pub target: String,
    pub budget_ms: u32,
}

/// Unavailable means the observer could not perform a probe, not target death.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeOutcome {
    Reachable,
    Failed,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReply {
    pub request_id: u64,
    pub target: String,
    pub outcome: ProbeOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeEvent {
    Ping(ProbeRequest),
    PingReq(ProbeRequest),
    Pong(ProbeReply),
    PingReqAck(ProbeReply),
}

impl ProbeEvent {
    pub fn request_id(&self) -> u64 {
        match self {
            Self::Ping(r) | Self::PingReq(r) => r.request_id,
            Self::Pong(r) | Self::PingReqAck(r) => r.request_id,
        }
    }
    fn validate(&self) -> Result<(), RemoteError> {
        let invalid = || RemoteError::Codec("invalid membership probe".into());
        let target = match self {
            Self::Ping(r) | Self::PingReq(r) => {
                if !(1..=MAX_PROBE_MS).contains(&r.budget_ms) {
                    return Err(invalid());
                }
                &r.target
            }
            Self::Pong(r) | Self::PingReqAck(r) => &r.target,
        };
        if self.request_id() == 0 || target.len() > 65_535 {
            return Err(invalid());
        }
        parrot_config::endpoint::validate_node_id(target).map_err(|_| invalid())
    }
    pub fn encode(&self) -> Result<bytes::Bytes, RemoteError> {
        self.validate()?;
        let (tag, body) = match self {
            Self::Ping(r) => (
                sys_event_tag::MEMBERSHIP_PING,
                bincode::serde::encode_to_vec(r, bincode::config::standard()),
            ),
            Self::PingReq(r) => (
                sys_event_tag::MEMBERSHIP_PING_REQ,
                bincode::serde::encode_to_vec(r, bincode::config::standard()),
            ),
            Self::Pong(r) => (
                sys_event_tag::MEMBERSHIP_PONG,
                bincode::serde::encode_to_vec(r, bincode::config::standard()),
            ),
            Self::PingReqAck(r) => (
                sys_event_tag::MEMBERSHIP_PING_REQ_ACK,
                bincode::serde::encode_to_vec(r, bincode::config::standard()),
            ),
        };
        let body = body.map_err(|e| RemoteError::Codec(format!("membership probe encode: {e}")))?;
        let mut bytes = Vec::with_capacity(body.len() + 1);
        bytes.push(tag);
        bytes.extend_from_slice(&body);
        Ok(bytes.into())
    }
    pub(crate) fn decode(tag: u8, body: &[u8]) -> Result<Self, RemoteError> {
        fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, RemoteError> {
            parrot_api::message::serde_remote_deserialize(body)
                .map_err(|e| RemoteError::Codec(format!("membership probe decode: {e}")))
        }
        if body.len() > MAX_BODY {
            return Err(RemoteError::Codec("membership probe too large".into()));
        }
        let event = match tag {
            sys_event_tag::MEMBERSHIP_PING => Self::Ping(decode(body)?),
            sys_event_tag::MEMBERSHIP_PING_REQ => Self::PingReq(decode(body)?),
            sys_event_tag::MEMBERSHIP_PONG => Self::Pong(decode(body)?),
            sys_event_tag::MEMBERSHIP_PING_REQ_ACK => Self::PingReqAck(decode(body)?),
            _ => return Err(RemoteError::Codec("unknown membership probe tag".into())),
        };
        event.validate()?;
        Ok(event)
    }
    pub(crate) fn frame(&self) -> Result<Frame, RemoteError> {
        let mut frame = Frame::tell("", "", self.encode()?);
        frame.header.frame_type = frame_type::SYSTEM_EVENT;
        frame.header.correlation_id = self.request_id();
        Ok(frame)
    }
}

struct Slot {
    source: FrameSender,
    target: String,
    indirect: bool,
    tx: oneshot::Sender<ProbeOutcome>,
}
struct Inner {
    next: u64,
    closed: bool,
    pending: HashMap<u64, Slot>,
}

pub(crate) struct ProbeState {
    inner: Mutex<Inner>,
    pub(crate) helpers: Arc<Semaphore>,
}
impl Default for ProbeState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                next: 1,
                closed: false,
                pending: HashMap::new(),
            }),
            helpers: Arc::new(Semaphore::new(MAX_HELPERS)),
        }
    }
}
struct RequestGuard {
    state: Arc<ProbeState>,
    id: u64,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.state.inner.lock().unwrap().pending.remove(&self.id);
    }
}
impl ProbeState {
    fn register(
        self: &Arc<Self>,
        source: &FrameSender,
        target: &str,
        indirect: bool,
    ) -> Option<(RequestGuard, oneshot::Receiver<ProbeOutcome>)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed || inner.pending.len() >= MAX_PENDING || source.is_closed() {
            return None;
        }
        let id = inner.next;
        inner.next = id.checked_add(1)?;
        let (tx, rx) = oneshot::channel();
        inner.pending.insert(
            id,
            Slot {
                source: source.clone(),
                target: target.into(),
                indirect,
                tx,
            },
        );
        Some((
            RequestGuard {
                state: self.clone(),
                id,
            },
            rx,
        ))
    }
    pub(crate) fn complete(&self, reply: ProbeReply, indirect: bool, source: &FrameSender) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let valid = inner.pending.get(&reply.request_id).is_some_and(|slot| {
            slot.indirect == indirect
                && slot.target == reply.target
                && slot.source.same_connection(source)
        });
        if !valid {
            return false;
        }
        let slot = inner
            .pending
            .remove(&reply.request_id)
            .expect("validated slot");
        let _ = slot.tx.send(reply.outcome);
        true
    }
    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.pending.clear();
        self.helpers.close();
    }
    pub(crate) async fn exchange(
        self: Arc<Self>,
        source: FrameSender,
        target: String,
        indirect: bool,
        budget: Duration,
        mut shutdown: watch::Receiver<bool>,
    ) -> ProbeOutcome {
        if *shutdown.borrow() || budget.is_zero() {
            return ProbeOutcome::Unavailable;
        }
        let Some((guard, rx)) = self.register(&source, &target, indirect) else {
            return ProbeOutcome::Unavailable;
        };
        // The helper's target observation must finish before our own wait.
        // Reserve half the remaining time for request delivery and the reply;
        // otherwise a failed target can consistently arrive after our timeout.
        let remote_budget = if indirect { budget / 2 } else { budget };
        let request = ProbeRequest {
            request_id: guard.id,
            target,
            budget_ms: remote_budget.as_millis().clamp(1, u128::from(MAX_PROBE_MS)) as u32,
        };
        let event = if indirect {
            ProbeEvent::PingReq(request)
        } else {
            ProbeEvent::Ping(request)
        };
        let Ok(frame) = event.frame() else {
            return ProbeOutcome::Unavailable;
        };
        // Never queue an unbounded waiter behind a busy business writer.
        if source.send_frame_sync(frame).is_err() {
            return ProbeOutcome::Unavailable;
        }
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stopped| *stopped) => ProbeOutcome::Unavailable,
            result = rx => result.unwrap_or(ProbeOutcome::Unavailable),
            _ = source.closed() => if indirect { ProbeOutcome::Unavailable } else { ProbeOutcome::Failed },
            _ = tokio::time::sleep(budget.min(Duration::from_millis(u64::from(MAX_PROBE_MS)))) => if indirect { ProbeOutcome::Unavailable } else { ProbeOutcome::Failed },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    fn source() -> (FrameSender, mpsc::Receiver<Frame>) {
        let (tx, rx) = mpsc::channel(4);
        (FrameSender::new(tx, "peer"), rx)
    }
    #[tokio::test(start_paused = true)]
    async fn indirect_execution_budget_reserves_time_for_the_failure_receipt() {
        let state = Arc::new(ProbeState::default());
        let (sender, mut peer) = source();
        let (_stop, stopped) = watch::channel(false);
        let task = tokio::spawn(state.clone().exchange(
            sender.clone(),
            "target".into(),
            true,
            Duration::from_millis(100),
            stopped,
        ));
        let frame = peer.recv().await.unwrap();
        let crate::admin::SysEvent::MembershipProbe(ProbeEvent::PingReq(request)) =
            crate::admin::decode_sys_event(&frame.payload).unwrap()
        else {
            panic!("wrong request")
        };
        assert_eq!(request.budget_ms, 50);
        // Target execution consumes its entire budget, plus return-path latency.
        tokio::time::sleep(Duration::from_millis(u64::from(request.budget_ms) + 10)).await;
        assert!(!task.is_finished());
        assert!(state.complete(
            ProbeReply {
                request_id: request.request_id,
                target: request.target,
                outcome: ProbeOutcome::Failed
            },
            true,
            &sender
        ));
        assert_eq!(task.await.unwrap(), ProbeOutcome::Failed);
        assert!(state.inner.lock().unwrap().pending.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn network_observer_timeout_preserves_unknown_target_state() {
        use crate::swim::{
            Member, MemberEvent, MemberStatus, Membership, NodeAddrWire, Swim, SwimConfig,
        };
        let membership = Arc::new(tokio::sync::Mutex::new(Membership::new(0)));
        membership
            .lock()
            .await
            .merge_event(&MemberEvent::Upsert(Member {
                node_id: "unknown".into(),
                addr: NodeAddrWire {
                    node_id: "unknown".into(),
                    scheme: "tcp".into(),
                    host: "".into(),
                    port: 0,
                },
                status: MemberStatus::Alive,
                incarnation: 1,
                status_until_ms: 0,
                metadata: bytes::Bytes::new(),
            }));
        let swim = Arc::new(Swim {
            config: SwimConfig {
                probe_timeout: Duration::from_millis(10),
                probe_interval: Duration::from_secs(60),
                gossip_interval: Duration::from_millis(50),
                ..Default::default()
            },
            membership: membership.clone(),
            self_node: "observer".into(),
        });
        let (gossip, mut receipts) = mpsc::channel(1);
        let (stop, stopped) = watch::channel(false);
        let driver = tokio::spawn(swim.run_observer(
            Box::new(|_| Box::pin(std::future::pending())),
            gossip,
            stopped,
        ));
        // This real driver tick happens after its first, never-completing probe expires.
        let (snapshot, _) = receipts.recv().await.unwrap();
        assert!(snapshot
            .events
            .iter()
            .any(|event| matches!(event, MemberEvent::Upsert(member)
            if member.node_id == "unknown" && member.status == MemberStatus::Alive)));
        stop.send(true).unwrap();
        driver.await.unwrap();
        assert_eq!(
            membership.lock().await.members["unknown"].status,
            MemberStatus::Alive
        );
    }
    #[tokio::test]
    async fn reply_requires_source_target_kind_and_request_identity() {
        let state = Arc::new(ProbeState::default());
        let (sender, _rx) = source();
        let (wrong, _rx2) = source();
        let (guard, mut result) = state.register(&sender, "target", true).unwrap();
        let reply = ProbeReply {
            request_id: guard.id,
            target: "target".into(),
            outcome: ProbeOutcome::Reachable,
        };
        assert!(!state.complete(reply.clone(), true, &wrong));
        assert!(!state.complete(reply.clone(), false, &sender));
        assert!(!state.complete(
            ProbeReply {
                target: "other".into(),
                ..reply.clone()
            },
            true,
            &sender
        ));
        assert!(!state.complete(
            ProbeReply {
                request_id: guard.id + 1,
                ..reply.clone()
            },
            true,
            &sender
        ));
        assert!(matches!(
            result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(state.complete(reply.clone(), true, &sender));
        assert_eq!(result.await.unwrap(), ProbeOutcome::Reachable);
        assert!(!state.complete(reply, true, &sender));
    }
    #[test]
    fn pending_capacity_cancellation_and_id_exhaustion_are_bounded() {
        let state = Arc::new(ProbeState::default());
        let (sender, _rx) = source();
        let mut requests = (0..MAX_PENDING)
            .map(|_| state.register(&sender, "target", false).unwrap())
            .collect::<Vec<_>>();
        assert!(state.register(&sender, "target", false).is_none());
        requests.pop();
        let replacement = state.register(&sender, "target", false).unwrap();
        assert_eq!(replacement.0.id, MAX_PENDING as u64 + 1);
        drop(requests);
        drop(replacement);
        assert!(state.inner.lock().unwrap().pending.is_empty());
        state.inner.lock().unwrap().next = u64::MAX;
        assert!(state.register(&sender, "target", false).is_none());
        assert_eq!(state.inner.lock().unwrap().next, u64::MAX);
    }
    #[tokio::test]
    async fn cancellation_after_actual_send_removes_pending_slot() {
        let state = Arc::new(ProbeState::default());
        let (sender, mut peer) = source();
        let (_stop, stopped) = watch::channel(false);
        let task = tokio::spawn(state.clone().exchange(
            sender,
            "target".into(),
            true,
            Duration::from_secs(5),
            stopped,
        ));
        let frame = peer.recv().await.unwrap();
        assert_eq!(state.inner.lock().unwrap().pending.len(), 1);
        assert_ne!(frame.header.correlation_id, 0);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(state.inner.lock().unwrap().pending.is_empty());
    }
    #[tokio::test(start_paused = true)]
    async fn helper_timeout_is_not_target_failure_and_all_timeouts_retire_slots() {
        for indirect in [false, true] {
            let state = Arc::new(ProbeState::default());
            let (sender, _peer) = source();
            let (_stop, stopped) = watch::channel(false);
            let result = state
                .clone()
                .exchange(
                    sender,
                    "target".into(),
                    indirect,
                    Duration::from_millis(20),
                    stopped,
                )
                .await;
            assert_eq!(
                result,
                if indirect {
                    ProbeOutcome::Unavailable
                } else {
                    ProbeOutcome::Failed
                }
            );
            assert!(state.inner.lock().unwrap().pending.is_empty());
        }
    }
    #[tokio::test]
    async fn registry_shutdown_fails_waiters_and_fences_requests_and_helpers() {
        let state = Arc::new(ProbeState::default());
        let (sender, _peer) = source();
        let (_guard, receiver) = state.register(&sender, "target", true).unwrap();
        state.close();
        assert!(receiver.await.is_err());
        assert!(state.register(&sender, "target", true).is_none());
        assert!(state.helpers.clone().try_acquire_owned().is_err());
    }
    #[tokio::test]
    async fn full_output_queue_does_not_create_a_waiter_or_close_connection() {
        let state = Arc::new(ProbeState::default());
        let (sender, mut peer) = source();
        for _ in 0..4 {
            sender
                .send_frame_sync(Frame::tell("", "", bytes::Bytes::new()))
                .unwrap();
        }
        let (_stop, stopped) = watch::channel(false);
        assert_eq!(
            state
                .clone()
                .exchange(
                    sender.clone(),
                    "target".into(),
                    false,
                    Duration::from_secs(1),
                    stopped
                )
                .await,
            ProbeOutcome::Unavailable
        );
        assert!(!sender.is_closed());
        assert!(state.inner.lock().unwrap().pending.is_empty());
        for _ in 0..4 {
            assert_eq!(
                peer.recv().await.unwrap().header.frame_type,
                frame_type::TELL
            );
        }
        assert!(peer.try_recv().is_err());
    }
}
