//! Segments carry one encoded snapshot body, never independently merged entries.
use crate::{error::RemoteError, transport::FrameSender};
use bytes::{Bytes, BytesMut};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

#[cfg(test)]
mod notification_contracts;

const HEADER: usize = 16;
pub(super) const MAX_SNAPSHOT: usize = 16 * 1024 * 1024;
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 1024;
const ASSEMBLY_TTL: Duration = Duration::from_secs(10);
const FLIGHT_IDLE_TTL: Duration = Duration::from_secs(10);
const FRAMES_PER_TICK: usize = 16;

fn invalid(reason: &str) -> RemoteError {
    RemoteError::Codec(format!("receptionist segment: {reason}"))
}

struct Pending {
    data: Vec<u8>,
    total: usize,
    touched: Instant,
}

struct Assembly {
    back: FrameSender,
    id: u64,
    routed: bool,
    pending: Option<Pending>,
}

#[derive(Default)]
pub(crate) struct Assemblies {
    states: HashMap<usize, Assembly>,
    reserved: usize,
}

#[derive(Default)]
pub(super) struct RetiredAssemblies(Vec<Assembly>);
impl RetiredAssemblies {
    pub(super) fn extend(&mut self, mut other: Self) {
        self.0.append(&mut other.0);
    }
}
impl Drop for RetiredAssemblies {
    fn drop(&mut self) {
        let cleanup = crate::ingress::unwind::Cleanup::default();
        for Assembly { back, pending, .. } in self.0.drain(..) {
            cleanup.dispose(pending);
            cleanup.dispose(back);
        }
    }
}
impl Drop for Assemblies {
    fn drop(&mut self) {
        self.reserved = 0;
        drop(RetiredAssemblies(
            std::mem::take(&mut self.states).into_values().collect(),
        ));
    }
}

impl Assemblies {
    pub(super) fn prune(&mut self, now: Instant) -> RetiredAssemblies {
        let retired = self
            .states
            .extract_if(|_, state| {
                let closed = state.back.is_closed();
                if closed
                    || state
                        .pending
                        .as_ref()
                        .is_some_and(|p| now.duration_since(p.touched) >= ASSEMBLY_TTL)
                {
                    if let Some(pending) = state.pending.take() {
                        self.reserved -= pending.total;
                    }
                }
                closed
            })
            .map(|(_, state)| state)
            .collect();
        RetiredAssemblies(retired)
    }

    pub(super) fn discard(&mut self, back: &FrameSender) {
        if let Some(state) = self.states.get_mut(&back.dispatch_key()) {
            if let Some(pending) = state.pending.take() {
                self.reserved -= pending.total;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn push(
        &mut self,
        back: &FrameSender,
        body: &[u8],
        now: Instant,
    ) -> Result<Option<Bytes>, RemoteError> {
        self.push_deferred(back, body, now, false, &mut RetiredAssemblies::default())
    }

    pub(super) fn push_deferred(
        &mut self,
        back: &FrameSender,
        body: &[u8],
        now: Instant,
        routed: bool,
        retired: &mut RetiredAssemblies,
    ) -> Result<Option<Bytes>, RemoteError> {
        retired.extend(self.prune(now));
        if body.len() <= HEADER {
            return Err(invalid("missing header or segment bytes"));
        }
        let id = u64::from_le_bytes(body[..8].try_into().unwrap());
        let total = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
        let data = &body[HEADER..];
        if id == 0
            || total == 0
            || total > MAX_SNAPSHOT
            || offset.checked_add(data.len()).is_none_or(|end| end > total)
        {
            return Err(invalid("invalid identity, length or offset"));
        }
        let key = back.dispatch_key();
        if let Some(state) = self.states.get(&key) {
            if id == state.id && routed != state.routed {
                return Err(invalid("segment protocol changed within snapshot"));
            }
            if id < state.id || (id == state.id && state.pending.is_none()) {
                return Ok(None);
            }
        }
        let new = self.states.get(&key).is_none_or(|state| state.id < id);
        if new {
            if offset != 0 {
                return Err(invalid("first segment must start at zero"));
            }
            self.discard(back);
            if (!self.states.contains_key(&key) && self.states.len() >= MAX_CONNECTIONS)
                || total > MAX_PENDING_BYTES.saturating_sub(self.reserved)
            {
                return Err(invalid("assembly budget exhausted"));
            }
            let mut buffer = Vec::new();
            buffer
                .try_reserve_exact(total)
                .map_err(|_| invalid("assembly allocation failed"))?;
            self.states.insert(
                key,
                Assembly {
                    back: back.clone(),
                    id,
                    routed,
                    pending: Some(Pending {
                        data: buffer,
                        total,
                        touched: now,
                    }),
                },
            );
            self.reserved += total;
        }
        let state = self.states.get_mut(&key).expect("admitted assembly");
        let pending = state.pending.as_mut().expect("live assembly");
        if pending.total != total || pending.data.len() != offset {
            return Err(invalid("segment changed total or skipped/repeated bytes"));
        }
        pending.data.extend_from_slice(data);
        pending.touched = now;
        if pending.data.len() != total {
            return Ok(None);
        }
        self.reserved -= total;
        Ok(Some(Bytes::from(state.pending.take().unwrap().data)))
    }
}

struct Flight {
    back: FrameSender,
    id: u64,
    body: Bytes,
    offset: usize,
    progressed: tokio::time::Instant,
    routed: bool,
}

#[derive(Default)]
pub(super) struct Outgoing {
    flights: HashMap<usize, Flight>,
    next_id: u64,
    reserved: usize,
}

pub(super) struct RetiredFlights(Vec<Flight>);
impl Drop for RetiredFlights {
    fn drop(&mut self) {
        let cleanup = crate::ingress::unwind::Cleanup::default();
        for Flight { back, body, .. } in self.0.drain(..) {
            cleanup.dispose(body);
            cleanup.dispose(back);
        }
    }
}
impl Drop for Outgoing {
    fn drop(&mut self) {
        self.reserved = 0;
        drop(RetiredFlights(
            std::mem::take(&mut self.flights).into_values().collect(),
        ));
    }
}

impl Outgoing {
    pub(super) fn prune(&mut self, current: &HashSet<usize>) -> RetiredFlights {
        let now = tokio::time::Instant::now();
        let retired = self
            .flights
            .extract_if(|key, flight| {
                let keep = current.contains(key)
                    && !flight.back.is_closed()
                    && now.duration_since(flight.progressed) < FLIGHT_IDLE_TTL;
                if !keep {
                    self.reserved -= flight.body.len();
                }
                !keep
            })
            .map(|(_, flight)| flight)
            .collect();
        RetiredFlights(retired)
    }

    /// Keep the same snapshot and cursor through bounded queue backpressure.
    pub(super) fn send(&mut self, back: &FrameSender, payload: &Bytes) -> Result<(), RemoteError> {
        let key = back.dispatch_key();
        if !self.flights.contains_key(&key) {
            if super::frame(payload.clone())
                .validate_size(back.max_frame_len())
                .is_ok()
            {
                return back.send_frame_sync(super::frame(payload.clone()));
            }
            if !back.supports_receptionist_segments() {
                return Err(invalid("peer cannot receive an oversized full snapshot"));
            }
            let body = payload.slice(1..);
            if body.len() > MAX_SNAPSHOT
                || body.len() > MAX_PENDING_BYTES.saturating_sub(self.reserved)
            {
                return Err(invalid("outgoing snapshot budget exhausted"));
            }
            if back.max_frame_len() as usize <= crate::frame::BODY_FIXED_OVERHEAD + 1 + HEADER {
                return Err(invalid("negotiated frame cannot hold a segment"));
            }
            self.next_id = self
                .next_id
                .checked_add(1)
                .ok_or_else(|| invalid("snapshot identity exhausted"))?;
            self.reserved += body.len();
            self.flights.insert(
                key,
                Flight {
                    back: back.clone(),
                    id: self.next_id,
                    body,
                    offset: 0,
                    progressed: tokio::time::Instant::now(),
                    routed: payload.first()
                        == Some(&crate::admin::sys_event_tag::RECEPTIONIST_ROUTES),
                },
            );
        }
        let flight = self
            .flights
            .get_mut(&key)
            .expect("admitted outgoing snapshot");
        let capacity =
            back.max_frame_len() as usize - crate::frame::BODY_FIXED_OVERHEAD - 1 - HEADER;
        for _ in 0..FRAMES_PER_TICK {
            let end = flight
                .body
                .len()
                .min(flight.offset.saturating_add(capacity));
            let mut payload = BytesMut::with_capacity(1 + HEADER + end - flight.offset);
            payload.extend_from_slice(&[if flight.routed {
                crate::admin::sys_event_tag::RECEPTIONIST_ROUTES_SEGMENT
            } else {
                crate::admin::sys_event_tag::RECEPTIONIST_SEGMENT
            }]);
            payload.extend_from_slice(&flight.id.to_le_bytes());
            payload.extend_from_slice(&(flight.body.len() as u32).to_le_bytes());
            payload.extend_from_slice(&(flight.offset as u32).to_le_bytes());
            payload.extend_from_slice(&flight.body[flight.offset..end]);
            if back
                .send_frame_sync(super::frame(payload.freeze()))
                .is_err()
            {
                return Ok(());
            }
            flight.offset = end;
            flight.progressed = tokio::time::Instant::now();
            if end == flight.body.len() {
                self.reserved -= flight.body.len();
                if let Some(Flight { back, body, .. }) = self.flights.remove(&key) {
                    let cleanup = crate::ingress::unwind::Cleanup::default();
                    cleanup.dispose(body);
                    cleanup.dispose(back);
                }
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod shutdown_assembly_contracts {
    use super::*;
    use crate::{LocalLookup, RemoteActorSystem, RemoteConfig};
    use parrot_api::receptionist::{ReceptionistGateway, ReceptionistKey};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    struct Empty;

    #[async_trait::async_trait]
    impl LocalLookup for Empty {
        async fn lookup(&self, _: &str) -> Option<parrot_api::types::BoxedActorRef> {
            None
        }
    }

    fn system() -> Arc<RemoteActorSystem> {
        RemoteActorSystem::new(
            RemoteConfig::tcp("assembly-owner", Some("127.0.0.1:0".parse().unwrap()))
                .with_insecure_development()
                .with_receptionist_acl(crate::acl::AclRules::permissive()),
            Arc::new(Empty),
        )
        .unwrap()
    }

    fn retain_assembly(
        system: &RemoteActorSystem,
        total: usize,
    ) -> mpsc::Receiver<crate::frame::Frame> {
        let (tx, rx) = mpsc::channel(1);
        let sender = FrameSender::new(tx, "retained-peer");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&(total as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(42);
        system
            .receptionist_assemblies
            .lock()
            .unwrap()
            .push(&sender, &bytes, Instant::now())
            .unwrap();
        // Only the assembly now owns this sender. Channel closure proves that
        // cleanup actually drops the retained state, rather than resetting counters.
        rx
    }

    fn assert_released(
        system: &RemoteActorSystem,
        receivers: &mut [mpsc::Receiver<crate::frame::Frame>],
    ) {
        let assemblies = system.receptionist_assemblies.lock().unwrap();
        assert!(
            assemblies.states.is_empty() && assemblies.reserved == 0,
            "closed system must release all assembly buffers and identities while retained"
        );
        drop(assemblies);
        for receiver in receivers {
            assert!(
                matches!(
                    receiver.try_recv(),
                    Err(mpsc::error::TryRecvError::Disconnected)
                ),
                "closed system must drop assembly-owned sender allocations"
            );
        }
    }

    #[test]
    fn synchronous_close_reclaims_partial_and_completed_assemblies_without_a_runtime() {
        let system = system();
        let mut receivers = [
            retain_assembly(&system, MAX_SNAPSHOT),
            retain_assembly(&system, 1),
        ];
        {
            let assemblies = system.receptionist_assemblies.lock().unwrap();
            assert_eq!(assemblies.states.len(), 2);
            assert_eq!(assemblies.reserved, MAX_SNAPSHOT);
        }
        assert!(receivers
            .iter_mut()
            .all(|r| matches!(r.try_recv(), Err(mpsc::error::TryRecvError::Empty))));
        system.begin_close();
        assert_released(&system, &mut receivers);
        system.begin_close();
        assert_released(&system, &mut receivers);
    }

    #[tokio::test]
    async fn async_shutdown_reclaims_assemblies_while_system_and_gateway_handles_survive() {
        let system = system();
        let retained = system.clone();
        let gateway = system.receptionist_gateway();
        let mut stream = gateway
            .subscribe(ReceptionistKey::new("assembly/service").unwrap())
            .await
            .unwrap();
        system.start().await.unwrap();
        let mut receivers = (0..4)
            .map(|_| retain_assembly(&system, MAX_SNAPSHOT))
            .collect::<Vec<_>>();
        assert_eq!(
            system.receptionist_assemblies.lock().unwrap().reserved,
            MAX_PENDING_BYTES
        );
        system.shutdown().await.unwrap();
        assert_released(&retained, &mut receivers);
        assert_eq!(
            stream.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        );
        assert!(matches!(
            gateway
                .register(
                    ReceptionistKey::new("assembly/service").unwrap(),
                    "/user/late".into()
                )
                .await,
            Err(parrot_api::errors::ActorError::Stopped)
        ));
        retained.shutdown().await.unwrap();
        assert_released(&retained, &mut receivers);
    }
}

#[cfg(test)]
mod outgoing_liveness_contracts {
    use super::*;
    use crate::frame::Frame;
    use tokio::sync::mpsc;

    fn peer(capacity: usize) -> (FrameSender, mpsc::Receiver<Frame>) {
        let (tx, rx) = mpsc::channel(capacity);
        (FrameSender::segmented_test_sender(tx, 512), rx)
    }

    fn payload(bytes: usize) -> Bytes {
        let mut data = vec![7; bytes + 1];
        data[0] = crate::admin::sys_event_tag::RECEPTIONIST_SYNC;
        Bytes::from(data)
    }

    #[derive(Default)]
    struct Received {
        id: Option<u64>,
        bytes: Vec<u8>,
    }

    impl Received {
        fn accept(&mut self, frame: Frame, expected: &Bytes) {
            frame.validate_size(512).unwrap();
            let p = &frame.payload;
            assert_eq!(p[0], crate::admin::sys_event_tag::RECEPTIONIST_SEGMENT);
            let id = u64::from_le_bytes(p[1..9].try_into().unwrap());
            assert_ne!(id, 0);
            assert_eq!(*self.id.get_or_insert(id), id, "active snapshot changed");
            assert_eq!(
                u32::from_le_bytes(p[9..13].try_into().unwrap()) as usize,
                expected.len() - 1
            );
            assert_eq!(
                u32::from_le_bytes(p[13..17].try_into().unwrap()) as usize,
                self.bytes.len(),
                "segment cursor skipped or repeated bytes"
            );
            self.bytes.extend_from_slice(&p[17..]);
            assert_eq!(self.bytes, expected[1..=self.bytes.len()]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_flights_release_global_budget_for_a_healthy_peer() {
        let mut outgoing = Outgoing::default();
        let blocked: Vec<_> = (0..4).map(|_| peer(1)).collect();
        let (healthy, mut receiver) = peer(16);
        let current = blocked
            .iter()
            .map(|(p, _)| p.dispatch_key())
            .chain([healthy.dispatch_key()])
            .collect();
        for (p, _) in &blocked {
            p.send_frame_sync(super::super::frame(Bytes::from_static(b"occupied")))
                .unwrap();
            outgoing.send(p, &payload(MAX_SNAPSHOT)).unwrap();
        }
        let snapshot = payload(1024);
        assert!(outgoing.send(&healthy, &snapshot).is_err());
        tokio::time::advance(Duration::from_secs(9)).await;
        // Retrying a full queue is not progress and must not extend its lease.
        for (p, _) in &blocked {
            outgoing.send(p, &payload(MAX_SNAPSHOT)).unwrap();
        }
        outgoing.prune(&current);
        assert!(outgoing.send(&healthy, &snapshot).is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        outgoing.prune(&current);
        assert!(
            outgoing.send(&healthy, &snapshot).is_ok(),
            "stalled flights must release budget for the healthy peer"
        );
        let mut received = Received::default();
        while let Ok(frame) = receiver.try_recv() {
            received.accept(frame, &snapshot);
        }
        assert_eq!(received.bytes, snapshot[1..]);
        assert!(
            blocked.iter().all(|(p, _)| !p.is_closed()),
            "budget reclamation must not close live business connections"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn progressing_snapshot_keeps_its_cursor_beyond_the_idle_timeout() {
        let mut outgoing = Outgoing::default();
        let (p, mut receiver) = peer(1);
        let current = [p.dispatch_key()].into_iter().collect();
        let snapshot = payload(8192);
        let newer = payload(1024);
        let mut received = Received::default();
        outgoing.send(&p, &snapshot).unwrap();
        for _ in 0..64 {
            received.accept(receiver.try_recv().unwrap(), &snapshot);
            if received.bytes.len() == snapshot.len() - 1 {
                break;
            }
            tokio::time::advance(Duration::from_secs(9)).await;
            outgoing.prune(&current);
            outgoing.send(&p, &newer).unwrap();
        }
        assert_eq!(
            received.bytes,
            snapshot[1..],
            "a progressing snapshot must finish even after its total age exceeds the idle timeout"
        );
        let previous_id = received.id.unwrap();
        outgoing.send(&p, &newer).unwrap();
        let next = receiver.try_recv().unwrap();
        assert!(u64::from_le_bytes(next.payload[1..9].try_into().unwrap()) > previous_id);
        assert_eq!(
            u32::from_le_bytes(next.payload[13..17].try_into().unwrap()),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn expired_snapshot_restarts_with_a_new_identity_and_current_content() {
        let mut outgoing = Outgoing::default();
        let (p, mut receiver) = peer(1);
        let current = [p.dispatch_key()].into_iter().collect();
        let original = payload(8192);
        outgoing.send(&p, &original).unwrap();
        let first = receiver.try_recv().unwrap();
        let old_id = u64::from_le_bytes(first.payload[1..9].try_into().unwrap());
        // Refill the actual bounded writer queue, leaving no admission capacity.
        p.send_frame_sync(super::super::frame(Bytes::from_static(b"occupied")))
            .unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        outgoing.prune(&current);
        receiver.try_recv().unwrap();
        let latest = payload(1024);
        outgoing.send(&p, &latest).unwrap();
        let first = receiver.try_recv().unwrap();
        assert!(
            u64::from_le_bytes(first.payload[1..9].try_into().unwrap()) > old_id,
            "expired snapshot must restart with a fresh identity"
        );
        let mut received = Received::default();
        received.accept(first, &latest);
        while received.bytes.len() < latest.len() - 1 {
            outgoing.send(&p, &latest).unwrap();
            received.accept(receiver.try_recv().unwrap(), &latest);
        }
        assert_eq!(received.bytes, latest[1..]);
    }

    #[tokio::test(start_paused = true)]
    async fn closed_and_replaced_connections_release_reservations_exactly_once() {
        let mut outgoing = Outgoing::default();
        let mut peers: Vec<_> = (0..4).map(|_| peer(1)).collect();
        for (p, _) in &peers {
            outgoing.send(p, &payload(MAX_SNAPSHOT)).unwrap();
        }
        peers[0].0.close();
        let current = peers[2..].iter().map(|(p, _)| p.dispatch_key()).collect();
        outgoing.prune(&current);
        outgoing.prune(&current);
        for (p, receiver) in &mut peers[2..] {
            let snapshot = payload(1024);
            receiver.try_recv().unwrap();
            // The original flight is still authoritative until idle expiry.
            outgoing.send(p, &snapshot).unwrap();
        }
        tokio::time::advance(Duration::from_secs(10)).await;
        outgoing.prune(&current);
        outgoing.prune(&current);
        let replacements: Vec<_> = (0..4).map(|_| peer(1)).collect();
        for (p, _) in &replacements {
            assert!(
                outgoing.send(p, &payload(MAX_SNAPSHOT)).is_ok(),
                "closed, replaced and expired flights must release each reservation once"
            );
        }
        let (extra, _receiver) = peer(1);
        assert!(
            outgoing.send(&extra, &payload(1024)).is_err(),
            "released capacity must still respect the global byte limit"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender() -> (
        FrameSender,
        tokio::sync::mpsc::Receiver<crate::frame::Frame>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        (FrameSender::new(tx, "same-logical-node"), rx)
    }
    fn body(id: u64, total: usize, offset: usize, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        out.extend_from_slice(bytes);
        out
    }

    #[test]
    fn completed_and_retired_ids_cannot_replay_or_mix_physical_connections() {
        let mut pool = Assemblies::default();
        let (a, _a_rx) = sender();
        let (b, _b_rx) = sender();
        let now = Instant::now();
        assert!(pool.push(&a, &body(1, 3, 0, b"a"), now).unwrap().is_none());
        assert!(pool.push(&b, &body(1, 3, 1, b"bc"), now).is_err());
        assert_eq!(
            pool.push(&a, &body(1, 3, 1, b"bc"), now).unwrap(),
            Some(Bytes::from_static(b"abc"))
        );
        assert_eq!(pool.reserved, 0);
        assert!(pool
            .push(&a, &body(1, 3, 0, b"old"), now)
            .unwrap()
            .is_none());
        assert_eq!(
            pool.push(&b, &body(1, 3, 0, b"new"), now).unwrap(),
            Some(Bytes::from_static(b"new"))
        );
        assert!(pool.push(&a, &body(2, 3, 0, b"x"), now).unwrap().is_none());
        pool.discard(&a);
        assert_eq!(pool.reserved, 0);
        assert!(pool.push(&a, &body(2, 3, 1, b"yz"), now).unwrap().is_none());
        assert_eq!(
            pool.push(&a, &body(3, 3, 0, b"end"), now).unwrap(),
            Some(Bytes::from_static(b"end"))
        );
    }

    #[test]
    fn declared_bytes_are_charged_globally_and_expiry_releases_without_replay() {
        let mut pool = Assemblies::default();
        let peers: Vec<_> = (0..5).map(|_| sender()).collect();
        let now = Instant::now();
        for (peer, _) in &peers[..4] {
            assert!(pool
                .push(peer, &body(1, MAX_SNAPSHOT, 0, &[1]), now)
                .unwrap()
                .is_none());
        }
        assert_eq!(pool.reserved, MAX_PENDING_BYTES);
        assert!(
            pool.push(&peers[4].0, &body(1, 2, 0, &[1]), now).is_err(),
            "global reservation must refuse a fifth incomplete snapshot"
        );
        pool.prune(now + ASSEMBLY_TTL);
        assert_eq!(pool.reserved, 0);
        assert!(pool
            .push(
                &peers[0].0,
                &body(1, MAX_SNAPSHOT, 1, &[2]),
                now + ASSEMBLY_TTL
            )
            .unwrap()
            .is_none());
        assert_eq!(
            pool.push(&peers[4].0, &body(2, 2, 0, &[1, 2]), now + ASSEMBLY_TTL)
                .unwrap(),
            Some(Bytes::from_static(&[1, 2]))
        );
        assert_eq!(pool.reserved, 0);
    }

    #[test]
    fn malformed_offsets_lengths_and_repeated_bytes_never_complete() {
        let now = Instant::now();
        for invalid in [
            vec![],
            vec![0; HEADER],
            body(0, 1, 0, &[1]),
            body(1, 0, 0, &[1]),
            body(1, MAX_SNAPSHOT + 1, 0, &[1]),
            body(1, 4, u32::MAX as usize, &[1]),
            body(1, 1, 0, &[1, 2]),
            body(1, 4, 1, &[1]),
        ] {
            let (peer, _rx) = sender();
            let mut pool = Assemblies::default();
            assert!(pool.push(&peer, &invalid, now).is_err());
            assert_eq!(pool.reserved, 0);
        }
        for invalid in [
            body(1, 4, 0, &[1]),
            body(1, 5, 1, &[2]),
            body(1, 4, 2, &[2]),
        ] {
            let (peer, _rx) = sender();
            let mut pool = Assemblies::default();
            pool.push(&peer, &body(1, 4, 0, &[1]), now).unwrap();
            assert!(pool.push(&peer, &invalid, now).is_err());
            peer.close();
            pool.prune(now);
            assert_eq!((pool.reserved, pool.states.len()), (0, 0));
        }
    }

    #[test]
    fn connection_budget_includes_completed_ids_and_closed_connections_release_it() {
        let now = Instant::now();
        let peers: Vec<_> = (0..MAX_CONNECTIONS + 1).map(|_| sender()).collect();
        let mut pool = Assemblies::default();
        for (peer, _) in &peers[..MAX_CONNECTIONS] {
            pool.push(peer, &body(1, 1, 0, &[1]), now).unwrap();
        }
        assert_eq!(pool.reserved, 0);
        assert!(pool
            .push(&peers[MAX_CONNECTIONS].0, &body(1, 1, 0, &[1]), now)
            .is_err());
        peers[0].0.close();
        assert_eq!(
            pool.push(&peers[MAX_CONNECTIONS].0, &body(1, 1, 0, &[2]), now)
                .unwrap(),
            Some(Bytes::from_static(&[2]))
        );
        assert_eq!(pool.states.len(), MAX_CONNECTIONS);
    }
}
