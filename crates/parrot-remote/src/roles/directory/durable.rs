use super::{
    request::{self, StoredCommand},
    DirectoryRequestStatus,
};
use super::{DirCmd, DirEntry, DirQuery, DirectoryApplyError, DirectorySm};
use crate::raft::{Clock, DurableRaft, LogIndex, NodeId, RaftRpc, RaftStorageError, Role, Term};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

#[derive(Debug, thiserror::Error)]
pub enum DurableDirectoryError {
    #[error(transparent)]
    Storage(#[from] RaftStorageError),
    #[error(transparent)]
    Command(#[from] DirectoryApplyError),
    #[error("directory owner closed; reopen required")]
    Closed,
}

/// Local acceptance receipt; scoped to this live host, not a durable request ID.
#[derive(Debug, Clone)]
pub struct DurableDirectoryProposal {
    index: LogIndex,
    term: Term,
    digest: [u8; 32],
    identity: Arc<()>,
}
impl DurableDirectoryProposal {
    pub fn index(&self) -> LogIndex {
        self.index
    }
    pub fn term(&self) -> Term {
        self.term
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableDirectoryStatus {
    Pending,
    Applied,
    Conflict,
    Foreign,
}

/// One read invocation, bound to its owner, term, log position and query.
/// Poll until completion; a completed ticket cannot be reused as a new read.
#[derive(Debug)]
pub struct DirectoryReadTicket {
    receipt: DurableDirectoryProposal,
    query: DirQuery,
    complete: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectoryRead {
    pub entry: Option<DirEntry>,
    pub barrier_index: LogIndex,
    pub applied_index: LogIndex,
    pub term: Term,
}

/// A synchronous, disk-backed Directory group, with one owner for Raft and SM.
///
/// Successful drives durably save Raft state then apply only its committed prefix
/// before exposing outbound RPCs. Open reconstructs the SM from that prefix. Run
/// on an owner thread: these methods perform blocking disk IO. Authentication,
/// network delivery is not installed by this type. Linear reads require a fresh
/// begin_linear_read ticket followed by polling while driving Raft to quorum.
/// Explicit submit_request identities rebuild deduplication with business state;
/// the original propose API retains its instance-local receipt semantics.
pub struct DurableDirectory {
    raft: DurableRaft,
    sm: DirectorySm,
    applied: LogIndex,
    failed: bool,
    peers: Vec<NodeId>,
    identity: Arc<()>,
    requests: HashMap<(String, String), ([u8; 32], DirectoryRequestStatus)>,
}
impl DurableDirectory {
    pub fn open(
        directory: impl AsRef<Path>,
        cluster: impl Into<String>,
        id: impl Into<String>,
        peers: Vec<NodeId>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, DurableDirectoryError> {
        let cluster = cluster.into();
        if cluster.is_empty() || cluster.chars().any(char::is_control) {
            return Err(DirectoryApplyError::Invalid("invalid cluster identifier".into()).into());
        }
        let id = id.into();
        let mut members = peers.clone();
        members.retain(|p| p != &id);
        members.sort();
        members.dedup();
        // Bind the stored application schema separately from a bare Raft group.
        // The mapping is injective; no automatic adoption of a generic snapshot.
        let raft = DurableRaft::open_validated(
            directory,
            format!("directory-v1:{cluster}"),
            id,
            peers,
            clock,
            |bytes| {
                request::decode(bytes)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            },
        )?;
        let mut this = Self {
            raft,
            sm: DirectorySm::default(),
            applied: 0,
            failed: false,
            peers: members,
            identity: Arc::new(()),
            requests: HashMap::new(),
        };
        this.apply_pending()?;
        Ok(this)
    }
    pub fn tick(&mut self) -> Result<(), DurableDirectoryError> {
        self.check()?;
        self.raft.tick()?;
        self.apply_pending()
    }
    /// `from` must already be bound to an authenticated transport identity.
    /// Invalid commands reject the complete RPC before any state is persisted.
    pub fn step(&mut self, from: NodeId, rpc: RaftRpc) -> Result<(), DurableDirectoryError> {
        self.check()?;
        if !self.peers.contains(&from) {
            return Ok(());
        }
        if let RaftRpc::AppendEntries { entries, .. } = &rpc {
            if entries.len() > 65_536
                || entries
                    .iter()
                    .try_fold(0usize, |n, e| n.checked_add(e.cmd.len()))
                    .is_none_or(|n| n > 16 * 1024 * 1024)
            {
                return Err(DirectoryApplyError::Capacity.into());
            }
            for e in entries {
                request::decode(&e.cmd)?;
            }
        }
        self.raft.step(from, rpc)?;
        self.apply_pending()
    }
    pub fn propose(
        &mut self,
        cmd: &DirCmd,
    ) -> Result<DurableDirectoryProposal, DurableDirectoryError> {
        self.check()?;
        let bytes = cmd.encode()?;
        let digest = Sha256::digest(&bytes).into();
        let index = self.raft.propose(bytes)?;
        self.apply_pending()?;
        Ok(DurableDirectoryProposal {
            index,
            term: self.raft.current_term(),
            digest,
            identity: self.identity.clone(),
        })
    }
    pub fn outbox(&mut self) -> Result<Vec<(NodeId, RaftRpc)>, DurableDirectoryError> {
        self.check()?;
        Ok(self.raft.outbox()?)
    }
    /// Idempotent within this fixed group and authenticated client namespace.
    /// Pending acceptance is not a majority commit. IDs are retained with the log.
    pub fn submit_request(
        &mut self,
        client: &str,
        id: &str,
        command: &DirCmd,
    ) -> Result<DirectoryRequestStatus, DurableDirectoryError> {
        self.check()?;
        let bytes = request::encode_request(client, id, command)?;
        let digest = super::directory_command_digest(command)?;
        let status = self.request_status(client, id, digest)?;
        match &status {
            DirectoryRequestStatus::Applied { .. } | DirectoryRequestStatus::Conflict => {
                return Ok(status)
            }
            DirectoryRequestStatus::Pending { term, .. } => {
                if self.role() != Role::Leader {
                    return Err(
                        RaftStorageError::NotLeader(self.leader_hint().map(str::to_owned)).into(),
                    );
                }
                if *term == self.current_term() {
                    return Ok(status);
                }
                // A new leader cannot commit old-term entries by counting their
                // replicas alone. Append the same identity in its current term;
                // application deduplicates both entries into one business change.
            }
            DirectoryRequestStatus::Unknown => {}
        }
        let index = self.raft.propose(bytes)?;
        self.apply_pending()?;
        let status = self.request_status(client, id, digest)?;
        debug_assert_ne!(
            status,
            DirectoryRequestStatus::Unknown,
            "accepted index {index}"
        );
        Ok(status)
    }
    /// A local durable observation. Applied is a positive commit proof; Unknown
    /// or Pending on a stale replica does not establish the request's global fate.
    pub fn request_status(
        &self,
        client: &str,
        id: &str,
        digest: [u8; 32],
    ) -> Result<DirectoryRequestStatus, DurableDirectoryError> {
        self.check()?;
        request::identity(client, id)?;
        if let Some((saved, status)) = self.requests.get(&(client.into(), id.into())) {
            return Ok(if *saved == digest {
                status.clone()
            } else {
                DirectoryRequestStatus::Conflict
            });
        }
        let mut pending = DirectoryRequestStatus::Unknown;
        for entry in &self.raft.log()[self.applied as usize..] {
            if let StoredCommand::Request(saved) = request::decode(&entry.cmd)? {
                if saved.client == client && saved.id == id {
                    if super::directory_command_digest(&saved.command)? == digest {
                        pending = DirectoryRequestStatus::Pending {
                            index: entry.index,
                            term: entry.term,
                        };
                    } else if pending == DirectoryRequestStatus::Unknown {
                        return Ok(DirectoryRequestStatus::Conflict);
                    }
                }
            }
        }
        Ok(pending)
    }
    /// A coherent local applied view, including on followers. Not a quorum read.
    pub fn query_local(&self, query: &DirQuery) -> Result<Option<DirEntry>, DurableDirectoryError> {
        self.check()?;
        Ok(self.sm.query(query))
    }
    pub fn begin_linear_read(
        &mut self,
        query: &DirQuery,
    ) -> Result<DirectoryReadTicket, DurableDirectoryError> {
        self.check()?;
        super::validate_query(query)?;
        let bytes = request::READ_BARRIER.to_vec();
        let digest = Sha256::digest(&bytes).into();
        let index = self.raft.propose(bytes)?;
        self.apply_pending()?;
        Ok(DirectoryReadTicket {
            receipt: DurableDirectoryProposal {
                index,
                term: self.current_term(),
                digest,
                identity: self.identity.clone(),
            },
            query: query.clone(),
            complete: false,
        })
    }
    /// None means pending, not a successful missing-key read. Some(entry=None)
    /// is a quorum-confirmed absent result. Term/role loss fails this invocation.
    pub fn poll_linear_read(
        &self,
        ticket: &mut DirectoryReadTicket,
    ) -> Result<Option<DirectoryRead>, DurableDirectoryError> {
        self.check()?;
        if ticket.complete || !Arc::ptr_eq(&self.identity, &ticket.receipt.identity) {
            return Err(
                DirectoryApplyError::Invalid("foreign or completed read ticket".into()).into(),
            );
        }
        if self.role() != Role::Leader || self.current_term() != ticket.receipt.term {
            ticket.complete = true;
            return Err(RaftStorageError::NotLeader(self.leader_hint().map(str::to_owned)).into());
        }
        match self.proposal_status(&ticket.receipt)? {
            DurableDirectoryStatus::Applied => {
                ticket.complete = true;
                Ok(Some(DirectoryRead {
                    entry: self.sm.query(&ticket.query),
                    barrier_index: ticket.receipt.index,
                    applied_index: self.applied,
                    term: ticket.receipt.term,
                }))
            }
            DurableDirectoryStatus::Pending => Ok(None),
            _ => {
                ticket.complete = true;
                Err(DirectoryApplyError::Invalid("read barrier replaced".into()).into())
            }
        }
    }
    pub fn state_local(&self) -> Result<&DirectorySm, DurableDirectoryError> {
        self.check()?;
        Ok(&self.sm)
    }
    pub fn proposal_status(
        &self,
        receipt: &DurableDirectoryProposal,
    ) -> Result<DurableDirectoryStatus, DurableDirectoryError> {
        self.check()?;
        if !Arc::ptr_eq(&self.identity, &receipt.identity) {
            return Ok(DurableDirectoryStatus::Foreign);
        }
        let Some(entry) = self.raft.log().get((receipt.index - 1) as usize) else {
            // This live owner issued the receipt only after appending the
            // entry. Without log compaction, absence means its uncommitted
            // suffix was truncated, not that replication is still pending.
            return Ok(DurableDirectoryStatus::Conflict);
        };
        if entry.term != receipt.term || Sha256::digest(&entry.cmd)[..] != receipt.digest {
            return Ok(DurableDirectoryStatus::Conflict);
        }
        Ok(if self.applied >= receipt.index {
            DurableDirectoryStatus::Applied
        } else {
            DurableDirectoryStatus::Pending
        })
    }
    pub fn role(&self) -> Role {
        if self.is_closed() {
            Role::Follower
        } else {
            self.raft.role()
        }
    }
    pub fn current_term(&self) -> Term {
        self.raft.current_term()
    }
    pub fn leader_hint(&self) -> Option<&str> {
        if self.is_closed() {
            None
        } else {
            self.raft.leader_hint()
        }
    }
    pub fn applied_index(&self) -> LogIndex {
        self.applied
    }
    pub fn is_closed(&self) -> bool {
        self.failed || self.raft.is_closed()
    }
    fn check(&self) -> Result<(), DurableDirectoryError> {
        if self.is_closed() {
            Err(DurableDirectoryError::Closed)
        } else {
            Ok(())
        }
    }
    fn apply_pending(&mut self) -> Result<(), DurableDirectoryError> {
        let commands = match self.prepare_requests() {
            Ok(commands) => commands,
            Err(e) => {
                self.failed = true;
                return Err(e);
            }
        };
        let entries = self.raft.committed()?;
        for (entry, cmd) in entries.iter().zip(commands) {
            match cmd {
                StoredCommand::ReadBarrier => {}
                StoredCommand::Plain(command) => self.sm.apply_validated(command),
                StoredCommand::Request(request) => {
                    let key = (request.client, request.id);
                    if let std::collections::hash_map::Entry::Vacant(slot) =
                        self.requests.entry(key)
                    {
                        let digest = super::directory_command_digest(&request.command)?;
                        self.sm.apply_validated(request.command);
                        slot.insert((
                            digest,
                            DirectoryRequestStatus::Applied {
                                index: entry.index,
                                term: entry.term,
                                revision: self.sm.version,
                            },
                        ));
                    }
                    // Duplicate/conflicting IDs never run business logic twice.
                    // Status remains bound to the first committed command.
                }
            }
        }
        if let Some(last) = entries.last() {
            self.applied = last.index;
        }
        Ok(())
    }
    fn prepare_requests(&self) -> Result<Vec<StoredCommand>, DurableDirectoryError> {
        let commands: Vec<_> = self
            .raft
            .pending_committed()?
            .iter()
            .map(|e| request::decode(&e.cmd))
            .collect::<Result<_, _>>()?;
        let mut new = HashSet::new();
        let mut revisions = 0u64;
        for command in &commands {
            let changes = match command {
                StoredCommand::ReadBarrier => false,
                StoredCommand::Plain(_) => true,
                StoredCommand::Request(request) => {
                    let key = (request.client.clone(), request.id.clone());
                    !self.requests.contains_key(&key) && new.insert(key)
                }
            };
            revisions += u64::from(changes);
        }
        self.sm
            .version
            .checked_add(revisions)
            .ok_or(DirectoryApplyError::RevisionExhausted)?;
        Ok(commands)
    }
}
