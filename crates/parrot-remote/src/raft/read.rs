use super::{LogIndex, NodeId, RaftNode, RaftRpc, Role, Term};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Weak,
    },
};

const MAX_PENDING: usize = 64;

#[derive(Default)]
pub(super) struct Reads {
    next_context: u64,
    identity: Arc<()>,
    pub(super) pending: HashMap<u64, Pending>,
}

pub(super) struct Pending {
    state: Weak<State>,
    voters: HashSet<NodeId>,
}
struct State {
    // Zero means waiting for a current-term commit before sending any probe.
    index: AtomicU64,
    quorum: AtomicBool,
}
pub(crate) struct ReadTicket {
    identity: Arc<()>,
    state: Arc<State>,
    term: Term,
    complete: bool,
}
impl std::fmt::Debug for ReadTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadTicket")
            .field("term", &self.term)
            .field("complete", &self.complete)
            .finish_non_exhaustive()
    }
}
impl ReadTicket {
    pub(crate) fn term(&self) -> Term {
        self.term
    }
}
#[derive(Debug)]
pub(super) enum ReadError {
    NotLeader,
    Capacity,
    Invalid,
}

impl RaftNode {
    pub(super) fn begin_read(&mut self) -> Result<ReadTicket, ReadError> {
        if self.role != Role::Leader || self.current_term == 0 {
            return Err(ReadError::NotLeader);
        }
        self.reads
            .pending
            .retain(|_, pending| pending.state.strong_count() != 0);
        if self.reads.pending.len() >= MAX_PENDING {
            return Err(ReadError::Capacity);
        }
        let context = self
            .reads
            .next_context
            .checked_add(1)
            .ok_or(ReadError::Capacity)?;
        self.reads.next_context = context;
        let state = Arc::new(State {
            index: AtomicU64::new(0),
            quorum: AtomicBool::new(false),
        });
        self.reads.pending.insert(
            context,
            Pending {
                state: Arc::downgrade(&state),
                voters: HashSet::from([self.id.clone()]),
            },
        );
        self.send_read_probes(false);
        Ok(ReadTicket {
            identity: self.reads.identity.clone(),
            state,
            term: self.current_term,
            complete: false,
        })
    }

    pub(super) fn poll_read(
        &self,
        ticket: &mut ReadTicket,
        applied: LogIndex,
    ) -> Result<Option<LogIndex>, ReadError> {
        if ticket.complete || !Arc::ptr_eq(&ticket.identity, &self.reads.identity) {
            return Err(ReadError::Invalid);
        }
        if self.role != Role::Leader || self.current_term != ticket.term {
            ticket.complete = true;
            return Err(ReadError::NotLeader);
        }
        let index = ticket.state.index.load(Ordering::Acquire);
        if ticket.state.quorum.load(Ordering::Acquire)
            && index != 0
            && applied >= index
            && !self.snapshots.pending_apply
        {
            ticket.complete = true;
            Ok(Some(index))
        } else {
            Ok(None)
        }
    }

    pub(super) fn send_read_probes(&mut self, retry: bool) {
        if self.role != Role::Leader
            || self.commit_index == 0
            || self.term_at(self.commit_index) != Some(self.current_term)
        {
            self.reads
                .pending
                .retain(|_, pending| pending.state.strong_count() != 0);
            return;
        }
        let quorum = self.quorum();
        self.reads.pending.retain(|context, pending| {
            let Some(state) = pending.state.upgrade() else {
                return false;
            };
            let index = state.index.load(Ordering::Acquire);
            if index == 0 {
                state.index.store(self.commit_index, Ordering::Release);
            } else if !retry {
                return true;
            }
            if quorum == 1 {
                state.quorum.store(true, Ordering::Release);
                return false;
            }
            for peer in &self.peers {
                self.outbox.push((
                    peer.clone(),
                    RaftRpc::ReadIndex {
                        term: self.current_term,
                        leader: self.id.clone(),
                        context: *context,
                    },
                ));
            }
            true
        });
    }

    pub(super) fn step_read_probe(
        &mut self,
        from: NodeId,
        term: Term,
        leader: NodeId,
        context: u64,
    ) {
        if from != leader || !self.peers.contains(&from) {
            return;
        }
        if term >= self.current_term {
            self.observe_term(term);
            self.role = Role::Follower;
            self.leader_hint = Some(leader);
            self.reads.pending.clear();
            self.last_heartbeat_ms = self.clock.now_ms();
        }
        self.outbox.push((
            from,
            RaftRpc::ReadIndexResp {
                term: self.current_term,
                context,
            },
        ));
    }

    pub(super) fn step_read_response(&mut self, from: NodeId, term: Term, context: u64) {
        if !self.peers.contains(&from) {
            return;
        }
        if term > self.current_term {
            self.observe_term(term);
            return;
        }
        if self.role != Role::Leader || term != self.current_term {
            return;
        }
        let quorum = self.quorum();
        let Some(pending) = self.reads.pending.get_mut(&context) else {
            return;
        };
        let Some(state) = pending.state.upgrade() else {
            self.reads.pending.remove(&context);
            return;
        };
        if state.index.load(Ordering::Acquire) == 0 {
            return;
        }
        pending.voters.insert(from);
        if pending.voters.len() >= quorum {
            state.quorum.store(true, Ordering::Release);
            self.reads.pending.remove(&context);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::{LogEntry, ManualClock};

    fn leader() -> RaftNode {
        let mut node = RaftNode::new("a", vec!["b".into()], Arc::new(ManualClock::new()));
        node.role = Role::Leader;
        node.current_term = 3;
        node.log.push(LogEntry {
            term: 3,
            index: 1,
            cmd: vec![],
        });
        node.commit_index = 1;
        node
    }

    #[test]
    fn quorum_does_not_complete_before_application_and_foreign_owner_cannot_consume() {
        let mut node = leader();
        let mut read = node.begin_read().unwrap();
        node.step_read_response("b".into(), 3, 1);
        assert!(
            node.poll_read(&mut read, 0).unwrap().is_none(),
            "a quorum is not an applied state machine"
        );
        assert!(matches!(
            leader().poll_read(&mut read, 1),
            Err(ReadError::Invalid)
        ));
        assert_eq!(node.poll_read(&mut read, 1).unwrap(), Some(1));
        assert!(matches!(
            node.poll_read(&mut read, 1),
            Err(ReadError::Invalid)
        ));
    }

    #[test]
    fn exhausted_contexts_are_rejected_without_wrapping_or_emitting_a_probe() {
        let mut node = leader();
        node.reads.next_context = u64::MAX;
        assert!(matches!(node.begin_read(), Err(ReadError::Capacity)));
        assert!(node.outbox().is_empty());
        assert!(node.reads.pending.is_empty());
    }
}
