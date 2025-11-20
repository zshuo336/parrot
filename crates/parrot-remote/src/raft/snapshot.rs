use super::{LogEntry, LogIndex, NodeId, RaftNode, RaftRpc, Role, Term};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};

pub(super) const MAX_BYTES: usize = 16 * 1024 * 1024;
pub(super) const CHUNK_BYTES: usize = 64 * 1024;

/// Application state after applying exactly `index`, whose log term is `term`.
/// The caller owns the application codec and must preserve request deduplication.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RaftSnapshot {
    pub index: LogIndex,
    pub term: Term,
    pub data: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::ManualClock;

    #[test]
    fn compacted_current_term_commit_still_confirms_a_fresh_read() {
        let clock = Arc::new(ManualClock::new());
        let mut node = RaftNode::new("a", vec![], clock.clone());
        clock.advance(1400);
        node.tick();
        node.propose(vec![1]).unwrap();
        node.committed();
        node.compact(1, vec![1]).unwrap();
        let mut ticket = node.begin_read().unwrap();
        assert!(node.poll_read(&mut ticket, 0).unwrap().is_none());
        node.snapshots.pending_apply = true;
        assert!(
            node.poll_read(&mut ticket, 1).unwrap().is_none(),
            "even a confirmed read must wait for snapshot application"
        );
        node.snapshot_applied(1, 1).unwrap();
        assert_eq!(node.poll_read(&mut ticket, 1).unwrap(), Some(1));
    }

    #[test]
    fn application_validation_precedes_snapshot_adoption_and_tail_release() {
        let clock = Arc::new(ManualClock::new());
        let mut node = RaftNode::new("b", vec!["a".into()], clock);
        let state = b"invalid application schema";
        let rpc = RaftRpc::InstallSnapshot {
            term: 3,
            leader: "a".into(),
            index: 10,
            snapshot_term: 2,
            digest: digest(10, 2, state),
            offset: 0,
            total: state.len() as u64,
            data: state.to_vec(),
        };
        assert_eq!(
            node.step_snapshot("a".into(), rpc, |_| Err(SnapshotError::Invalid)),
            Err(SnapshotError::Invalid)
        );
        assert_eq!(node.snapshot_index(), 0);
        assert_eq!(node.commit_index, 0);
        assert!(node.pending_snapshot().is_none());
        assert!(node.committed().is_empty());
        assert!(matches!(
            node.outbox().as_slice(),
            [(
                _,
                RaftRpc::InstallSnapshotResp {
                    installed: false,
                    next_offset: 0,
                    ..
                }
            )]
        ));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot capacity exceeded")]
    Capacity,
    #[error("snapshot must cover a retained committed index after application")]
    NotApplied,
    #[error("snapshot metadata, content or application acknowledgment is invalid")]
    Invalid,
}

#[derive(Default)]
pub(super) struct Snapshots {
    current: Option<Arc<RaftSnapshot>>,
    digest: [u8; 32],
    pub(super) pending_apply: bool,
    incoming: Option<Incoming>,
    outgoing: HashMap<NodeId, Sending>,
}
struct Incoming {
    leader: NodeId,
    term: Term,
    index: LogIndex,
    snapshot_term: Term,
    digest: [u8; 32],
    total: usize,
    data: Vec<u8>,
}
struct Sending {
    digest: [u8; 32],
    offset: usize,
    sent_end: Option<usize>,
}
impl Snapshots {
    pub(super) fn reset_transfer(&mut self) {
        self.incoming = None;
        self.outgoing.clear();
    }
}

fn digest(index: LogIndex, term: Term, data: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"parrot:raft:snapshot#v1");
    hash.update(index.to_le_bytes());
    hash.update(term.to_le_bytes());
    hash.update(data);
    hash.finalize().into()
}

impl RaftNode {
    pub(super) fn restore_snapshot(&mut self, snapshot: Arc<RaftSnapshot>) {
        self.snapshots.digest = digest(snapshot.index, snapshot.term, &snapshot.data);
        self.snapshots.current = Some(snapshot);
        self.snapshots.pending_apply = true;
    }

    pub fn snapshot_index(&self) -> LogIndex {
        self.snapshots.current.as_ref().map_or(0, |s| s.index)
    }
    pub fn snapshot_term(&self) -> Term {
        self.snapshots.current.as_ref().map_or(0, |s| s.term)
    }
    pub fn snapshot(&self) -> Option<&RaftSnapshot> {
        self.snapshots.current.as_deref()
    }
    /// A received snapshot must be applied before consuming subsequent entries.
    /// Hosts must durably publish it before exposing the installation response.
    pub fn pending_snapshot(&self) -> Option<&RaftSnapshot> {
        if self.snapshots.pending_apply {
            self.snapshot()
        } else {
            None
        }
    }
    pub fn snapshot_applied(&mut self, index: LogIndex, term: Term) -> Result<(), SnapshotError> {
        if !self.snapshots.pending_apply
            || self.snapshot_index() != index
            || self.snapshot_term() != term
        {
            return Err(SnapshotError::Invalid);
        }
        self.snapshots.pending_apply = false;
        Ok(())
    }
    /// Replace an already applied prefix, retaining absolute indices and terms.
    /// The application must serialize its complete state at this exact position.
    /// Bare Raft performs no IO; this is not a durable-host compaction API.
    pub fn compact(&mut self, index: LogIndex, data: Vec<u8>) -> Result<(), SnapshotError> {
        if data.len() > MAX_BYTES {
            return Err(SnapshotError::Capacity);
        }
        if index <= self.snapshot_index()
            || index > self.commit_index
            || !self.pending_apply.is_empty()
            || self.snapshots.pending_apply
        {
            return Err(SnapshotError::NotApplied);
        }
        let term = self.term_at(index).ok_or(SnapshotError::Invalid)?;
        let keep_from =
            usize::try_from(index - self.snapshot_index()).map_err(|_| SnapshotError::Invalid)?;
        let suffix = self.log.split_off(keep_from);
        self.log = suffix;
        self.snapshots.digest = digest(index, term, &data);
        self.snapshots.current = Some(Arc::new(RaftSnapshot { index, term, data }));
        self.snapshots.reset_transfer();
        Ok(())
    }

    pub(super) fn term_at(&self, index: LogIndex) -> Option<Term> {
        if index == self.snapshot_index() {
            return Some(self.snapshot_term());
        }
        let slot =
            usize::try_from(index.checked_sub(self.snapshot_index())?.checked_sub(1)?).ok()?;
        self.log.get(slot).map(|entry| entry.term)
    }
    pub(super) fn entries_after(&self, index: LogIndex) -> Option<&[LogEntry]> {
        let slot = usize::try_from(index.checked_sub(self.snapshot_index())?).ok()?;
        self.log.get(slot..)
    }
    pub(super) fn entries_between(&self, after: LogIndex, through: LogIndex) -> &[LogEntry] {
        let start =
            usize::try_from(after - self.snapshot_index()).expect("applied index follows snapshot");
        let end = usize::try_from(through - self.snapshot_index())
            .expect("committed index follows snapshot");
        &self.log[start..end]
    }

    pub(super) fn send_snapshot(&mut self, peer: NodeId) {
        let Some(snapshot) = self.snapshots.current.clone() else {
            return;
        };
        let identity = self.snapshots.digest;
        let sending = self
            .snapshots
            .outgoing
            .entry(peer.clone())
            .or_insert(Sending {
                digest: identity,
                offset: 0,
                sent_end: None,
            });
        if sending.digest != identity {
            *sending = Sending {
                digest: identity,
                offset: 0,
                sent_end: None,
            };
        }
        let end = (sending.offset + CHUNK_BYTES).min(snapshot.data.len());
        sending.sent_end = Some(end);
        self.outbox.push((
            peer,
            RaftRpc::InstallSnapshot {
                term: self.current_term,
                leader: self.id.clone(),
                index: snapshot.index,
                snapshot_term: snapshot.term,
                digest: identity,
                offset: sending.offset as u64,
                total: snapshot.data.len() as u64,
                data: snapshot.data[sending.offset..end].to_vec(),
            },
        ));
    }

    pub(super) fn step_snapshot(
        &mut self,
        from: NodeId,
        rpc: RaftRpc,
        validate: impl FnOnce(&RaftSnapshot) -> Result<(), SnapshotError>,
    ) -> Result<(), SnapshotError> {
        let RaftRpc::InstallSnapshot {
            term,
            leader,
            index,
            snapshot_term,
            digest: identity,
            offset,
            total,
            data,
        } = rpc
        else {
            return Err(SnapshotError::Invalid);
        };
        if from != leader || !self.peers.contains(&from) {
            return Ok(());
        }
        if term < self.current_term {
            self.snapshot_response(from, index, identity, 0, false);
            return Ok(());
        }
        self.observe_term(term);
        self.role = Role::Follower;
        self.leader_hint = Some(leader.clone());
        self.reads.pending.clear();
        self.last_heartbeat_ms = self.clock.now_ms();
        if index > self.commit_index
            && self
                .term_at(self.commit_index)
                .is_some_and(|committed_term| snapshot_term < committed_term)
        {
            self.snapshot_response(from, index, identity, 0, false);
            return Err(SnapshotError::Invalid);
        }
        if index <= self.commit_index {
            if self
                .term_at(index)
                .is_some_and(|term| term != snapshot_term)
            {
                self.snapshot_response(from, index, identity, 0, false);
                return Err(SnapshotError::Invalid);
            }
            self.snapshot_response(from, index, identity, total, true);
            return Ok(());
        }
        let matches = self.snapshots.incoming.as_ref().is_some_and(|incoming| {
            incoming.leader == leader
                && incoming.term == term
                && incoming.index == index
                && incoming.snapshot_term == snapshot_term
                && incoming.digest == identity
                && incoming.total as u64 == total
        });
        if !matches {
            if offset != 0 {
                self.snapshot_response(from, index, identity, 0, false);
                return Ok(());
            }
            self.snapshots.incoming = Some(Incoming {
                leader,
                term,
                index,
                snapshot_term,
                digest: identity,
                total: total as usize,
                data: Vec::new(),
            });
        }
        let incoming = self
            .snapshots
            .incoming
            .as_mut()
            .expect("selected incoming snapshot");
        let offset = offset as usize;
        if offset > incoming.data.len() {
            self.snapshot_response(from, index, identity, 0, false);
            return Ok(());
        }
        if offset < incoming.data.len() {
            if incoming.data.get(offset..offset + data.len()) != Some(data.as_slice()) {
                self.snapshots.incoming = None;
                self.snapshot_response(from, index, identity, 0, false);
                return Err(SnapshotError::Invalid);
            }
        } else {
            incoming.data.extend_from_slice(&data);
        }
        let acknowledged = offset + data.len();
        let next = incoming.data.len();
        if next != incoming.total {
            self.snapshot_response(from, index, identity, acknowledged as u64, false);
            return Ok(());
        }
        let incoming = self
            .snapshots
            .incoming
            .take()
            .expect("completed incoming snapshot");
        if digest(index, snapshot_term, &incoming.data) != identity {
            self.snapshot_response(from, index, identity, 0, false);
            return Err(SnapshotError::Invalid);
        }
        let snapshot = RaftSnapshot {
            index,
            term: snapshot_term,
            data: incoming.data,
        };
        if let Err(error) = validate(&snapshot) {
            self.snapshot_response(from, index, identity, 0, false);
            return Err(error);
        }
        self.log = if self.term_at(index) == Some(snapshot_term) {
            let count = usize::try_from(index - self.snapshot_index())
                .map_err(|_| SnapshotError::Invalid)?;
            self.log.split_off(count)
        } else {
            Vec::new()
        };
        self.snapshots.current = Some(Arc::new(snapshot));
        self.snapshots.digest = identity;
        self.snapshots.pending_apply = true;
        self.pending_apply.clear();
        self.commit_index = index;
        self.last_applied = index;
        self.snapshot_response(from, index, identity, total, true);
        Ok(())
    }

    fn snapshot_response(
        &mut self,
        peer: NodeId,
        index: LogIndex,
        digest: [u8; 32],
        next_offset: u64,
        installed: bool,
    ) {
        self.outbox.push((
            peer,
            RaftRpc::InstallSnapshotResp {
                term: self.current_term,
                index,
                digest,
                next_offset,
                installed,
            },
        ));
    }

    pub(super) fn step_snapshot_response(
        &mut self,
        from: NodeId,
        term: Term,
        index: LogIndex,
        identity: [u8; 32],
        next_offset: u64,
        installed: bool,
    ) {
        if !self.peers.contains(&from) {
            return;
        }
        if term > self.current_term {
            self.observe_term(term);
            return;
        }
        if self.role != Role::Leader
            || term != self.current_term
            || index != self.snapshot_index()
            || identity != self.snapshots.digest
        {
            return;
        }
        let Some(sending) = self.snapshots.outgoing.get_mut(&from) else {
            return;
        };
        if sending.digest != identity {
            return;
        }
        let total = self
            .snapshots
            .current
            .as_ref()
            .expect("current snapshot identity")
            .data
            .len();
        if next_offset == 0 && !installed && total != 0 {
            sending.offset = 0;
            sending.sent_end = None;
            return;
        }
        if installed {
            // A follower may already have installed this committed prefix.
            // Its matching response can complete a restarted transfer without
            // retransmitting the entire application state.
            if next_offset != total as u64 || sending.sent_end.is_none() {
                return;
            }
            self.snapshots.outgoing.remove(&from);
            let matched = self.match_index.entry(from.clone()).or_default();
            *matched = (*matched).max(index);
            let next = self.next_index.entry(from.clone()).or_insert(1);
            *next = (*next).max(index.saturating_add(1));
            self.advance_commit();
            self.send_append(from);
        } else if next_offset < total as u64
            && sending.sent_end == usize::try_from(next_offset).ok()
        {
            sending.offset = next_offset as usize;
            self.send_snapshot(from);
        }
    }
}
