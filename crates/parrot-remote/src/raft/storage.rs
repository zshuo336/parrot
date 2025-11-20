//! Synchronous durable host for the in-memory Raft core.
//!
//! Every persistent transition is synced before RPCs or committed entries can
//! escape. Run on a dedicated owner thread, not a Tokio IO worker. The private
//! directory and lock inode must remain stable; hostile filesystem writers are
//! outside this cooperative ownership contract.
use super::{
    Clock, LogEntry, LogIndex, NodeId, RaftNode, RaftRpc, RaftSnapshot, Role, SnapshotError, Term,
};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod capacity;
mod journal;
use capacity::Capacity;

const MAX_SNAPSHOT: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 65_536;
const MAX_COMMAND: usize = 1024 * 1024;
const MAGIC: &[u8; 8] = b"PRAFT01\0";

#[derive(Debug, thiserror::Error)]
pub enum RaftStorageError {
    #[error("raft storage: {0}")]
    Storage(String),
    #[error("raft invalid state or binding: {0}")]
    Invalid(String),
    #[error("raft snapshot capacity exceeded")]
    Capacity,
    #[error("raft owner failed or was inherited across fork; reopen required")]
    Closed,
    #[error("raft not leader (hint: {0:?})")]
    NotLeader(Option<NodeId>),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Saved {
    version: u8,
    cluster: String,
    id: NodeId,
    peers: Vec<NodeId>,
    term: Term,
    vote: Option<NodeId>,
    #[serde(deserialize_with = "journal::deserialize_log")]
    log: Vec<LogEntry>,
    commit: LogIndex,
    #[serde(skip)]
    snapshot: Option<Arc<RaftSnapshot>>,
}
impl Saved {
    fn base(&self) -> LogIndex {
        self.snapshot.as_ref().map_or(0, |snapshot| snapshot.index)
    }
}

/// Opt-in disk-backed Raft owner with fixed membership and bounded retained state.
///
/// `open` requires an existing, private directory. The snapshot binds its cluster,
/// node and canonical member set. Reconfiguration by editing these is rejected.
/// A fresh application must apply `pending_snapshot()` and acknowledge it before
/// replaying `committed()` after every open. The caller owns the snapshot codec;
/// replay is not an exactly-once contract for external effects. Propose returns a
/// durable local log index, **not** quorum commitment. Drop does not flush work:
/// successful operations have already synced their state.
pub struct DurableRaft {
    node: RaftNode,
    saved: Saved,
    capacity: Capacity,
    journal: journal::Journal,
    directory: PathBuf,
    failed: bool,
    pid: u32,
    _owner: OwnerLock,
}

// Acquired before snapshot parsing, replay validation or user clock callbacks.
// Closing alone does not release flock while a concurrently forked child retains
// the descriptor. Explicitly unlock every parent-side exit, including failed open.
struct OwnerLock {
    file: File,
    pid: u32,
}
impl Drop for OwnerLock {
    fn drop(&mut self) {
        if self.pid == std::process::id() {
            let _ = self.file.unlock();
        }
    }
}

impl DurableRaft {
    pub fn open(
        directory: impl AsRef<Path>,
        cluster: impl Into<String>,
        id: impl Into<String>,
        peers: Vec<NodeId>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, RaftStorageError> {
        Self::open_validated(
            directory,
            cluster,
            id,
            peers,
            clock,
            |_| Ok(()),
            |_, _| Ok(()),
        )
    }

    // Application replay validation must run before recovery removes temporary
    // files or rewrites anything. Validate the uncommitted suffix as well.
    pub(crate) fn open_validated(
        directory: impl AsRef<Path>,
        cluster: impl Into<String>,
        id: impl Into<String>,
        peers: Vec<NodeId>,
        clock: Arc<dyn Clock>,
        validate_command: impl Fn(&[u8]) -> Result<(), String>,
        validate_replay: impl Fn(Option<&RaftSnapshot>, &[LogEntry]) -> Result<(), String>,
    ) -> Result<Self, RaftStorageError> {
        let directory = directory.as_ref().to_path_buf();
        let cluster = cluster.into();
        let id = id.into();
        if !valid_id(&cluster)
            || !valid_id(&id)
            || peers.len() > 255
            || peers.iter().any(|p| !valid_id(p))
        {
            return Err(invalid(
                "invalid cluster/member identifiers or too many peers",
            ));
        }
        if !std::fs::symlink_metadata(&directory)
            .map_err(storage)?
            .is_dir()
        {
            return Err(invalid("existing real directory required"));
        }
        let owner = regular(&directory.join(".parrot-raft.lock"), Kind::Lock)?;
        owner
            .try_lock()
            .map_err(|e| RaftStorageError::Storage(format!("owner lock: {e}")))?;
        let owner = OwnerLock {
            file: owner,
            pid: std::process::id(),
        };
        let mut node = RaftNode::new(id, peers, clock);
        let main = directory.join("raft.state");
        let next = directory.join("raft.next");
        let exists = match std::fs::symlink_metadata(&main) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(storage(e)),
        };
        let (saved, journal) = if exists {
            let (saved, journal) =
                journal::read(&main, &cluster, &node.id, &node.peers, &validate_command)?;
            validate_replay(
                saved.snapshot.as_deref(),
                &saved.log[..(saved.commit - saved.base()) as usize],
            )
            .map_err(RaftStorageError::Invalid)?;
            if let Some(snapshot) = &saved.snapshot {
                node.restore_snapshot(snapshot.clone());
            }
            node.current_term = saved.term;
            node.voted_for = saved.vote.clone();
            node.log = saved.log.clone();
            node.commit_index = saved.commit;
            node.last_applied = saved.commit;
            node.pending_apply = saved.log[..(saved.commit - saved.base()) as usize].to_vec();
            (saved, journal)
        } else {
            // Never initialize over an interrupted first publication.
            match std::fs::symlink_metadata(&next) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(storage(e)),
                Ok(_) => return Err(invalid("incomplete initial snapshot; refusing reset")),
            }
            (snapshot(&node, cluster), journal::Journal::initial())
        };
        let capacity = Capacity::new(&saved)?;
        let mut this = Self {
            node,
            saved,
            capacity,
            journal,
            directory,
            failed: false,
            pid: owner.pid,
            _owner: owner,
        };
        // Clock or replay callbacks may have crossed fork after locking.
        // An inherited owner must not initialize, truncate or clean up files.
        this.check()?;
        if exists {
            // Validate committed state before removing any abandoned temporary.
            match std::fs::symlink_metadata(&next) {
                Ok(_) => {
                    drop(regular(&next, Kind::Read)?);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(storage(e)),
            }
            // Replay and application validation completed before any recovery IO.
            this.journal.recover(&main)?;
            if next.exists() {
                std::fs::remove_file(&next).map_err(storage)?;
            }
            regular(&main, Kind::Read)?.sync_all().map_err(storage)?;
            this.sync_directory()?;
        } else {
            this.journal.checkpoint(&this.directory, &this.saved)?;
        }
        // Persist the existing directory's name before returning success.
        // Caller must durably create any further missing ancestors beforehand.
        File::open(
            this.directory
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?
        .sync_all()
        .map_err(storage)?;
        Ok(this)
    }

    pub fn tick(&mut self) -> Result<(), RaftStorageError> {
        self.check()?;
        self.node.tick();
        self.finish(None)
    }
    pub fn step(&mut self, from: NodeId, rpc: RaftRpc) -> Result<(), RaftStorageError> {
        self.step_validated(from, rpc, |_| Ok(()))
    }
    pub(crate) fn step_validated(
        &mut self,
        from: NodeId,
        rpc: RaftRpc,
        validate_snapshot: impl FnOnce(&RaftSnapshot) -> Result<(), String>,
    ) -> Result<(), RaftStorageError> {
        self.check()?;
        // Invalid protocol metadata is a caller error, not a failed disk
        // publication. Reject it before mutating the core or fencing its owner.
        rpc.validate_shape().map_err(invalid)?;
        if let RaftRpc::InstallSnapshot { term, .. } = &rpc {
            let term = *term;
            let mut rejected = None;
            let saved = &self.saved;
            let result = self.node.step_snapshot(from, rpc, |snapshot| {
                let candidate = installed(saved, snapshot, term);
                let checked = validate_snapshot(snapshot)
                    .map_err(|error| invalid(&error))
                    .and_then(|()| preflight(&candidate));
                if let Err(error) = checked {
                    rejected = Some(error);
                    return Err(SnapshotError::Invalid);
                }
                Ok(())
            });
            // Even a rejected application payload can carry a newer term.
            // Publish that hard state before exposing any rejection response.
            self.finish(None)?;
            return match (rejected, result) {
                (Some(error), _) => Err(error),
                (_, result) => result.map_err(Into::into),
            };
        }
        let mut log_change = None;
        if let RaftRpc::AppendEntries {
            term,
            leader,
            prev_log_index,
            prev_log_term,
            entries,
            ..
        } = &rpc
        {
            if entries.len() > MAX_ENTRIES
                || entries.iter().any(|e| e.cmd.len() > MAX_COMMAND)
                || entries
                    .iter()
                    .try_fold(0usize, |n, e| n.checked_add(e.cmd.len()))
                    .is_none_or(|n| n > MAX_SNAPSHOT)
            {
                return Err(RaftStorageError::Capacity);
            }
            if *term >= self.node.current_term && *leader == from && self.node.peers.contains(&from)
            {
                if let Some((keep, skip)) =
                    self.node
                        .append_plan(*prev_log_index, *prev_log_term, entries)
                {
                    if skip < entries.len() {
                        self.capacity.check(keep, &entries[skip..])?;
                        log_change = Some(keep);
                    }
                }
            }
        }
        self.node.step(from, rpc);
        self.finish(log_change)
    }
    pub fn propose(&mut self, cmd: Vec<u8>) -> Result<LogIndex, RaftStorageError> {
        self.check()?;
        if cmd.len() > MAX_COMMAND || self.node.log.len() >= MAX_ENTRIES {
            return Err(RaftStorageError::Capacity);
        }
        if self.node.role != Role::Leader {
            return Err(RaftStorageError::NotLeader(self.node.leader_hint.clone()));
        }
        let entry = LogEntry {
            index: self
                .node
                .last_log_index()
                .checked_add(1)
                .ok_or_else(|| invalid("log index exhausted"))?,
            term: self.node.current_term,
            cmd,
        };
        self.capacity
            .check(self.node.log.len(), std::slice::from_ref(&entry))?;
        let keep = self.node.log.len();
        let index = self
            .node
            .propose(entry.cmd)
            .map_err(|e| RaftStorageError::NotLeader(e.0))?;
        self.finish(Some(keep))?;
        Ok(index)
    }
    pub fn outbox(&mut self) -> Result<Vec<(NodeId, RaftRpc)>, RaftStorageError> {
        self.check()?;
        Ok(self.node.outbox())
    }
    pub(crate) fn pending_committed(&self) -> Result<&[LogEntry], RaftStorageError> {
        self.check()?;
        Ok(self.node.pending_committed())
    }
    pub fn committed(&mut self) -> Result<Vec<LogEntry>, RaftStorageError> {
        self.check()?;
        Ok(self.node.committed())
    }
    pub fn snapshot(&self) -> Option<&RaftSnapshot> {
        self.saved.snapshot.as_deref()
    }
    pub fn pending_snapshot(&self) -> Result<Option<&RaftSnapshot>, RaftStorageError> {
        self.check()?;
        Ok(self.node.pending_snapshot())
    }
    pub fn snapshot_applied(
        &mut self,
        index: LogIndex,
        term: Term,
    ) -> Result<(), RaftStorageError> {
        self.check()?;
        self.node.snapshot_applied(index, term)?;
        Ok(())
    }
    /// Durably replace an applied committed prefix. Data must encode the exact
    /// application state at `index`, including any persistent deduplication.
    pub fn compact(&mut self, index: LogIndex, data: Vec<u8>) -> Result<(), RaftStorageError> {
        self.check()?;
        if index <= self.saved.base()
            || index > self.saved.commit
            || !self.node.pending_apply.is_empty()
            || self.node.pending_snapshot().is_some()
        {
            return Err(SnapshotError::NotApplied.into());
        }
        let term = self.node.term_at(index).ok_or(SnapshotError::Invalid)?;
        let snapshot = RaftSnapshot { index, term, data };
        let candidate = installed(&self.saved, &snapshot, self.saved.term);
        preflight(&candidate)?;
        self.node.compact(index, snapshot.data)?;
        self.finish(None)
    }
    pub fn snapshot_index(&self) -> LogIndex {
        self.saved.base()
    }
    pub fn last_log_term(&self) -> Term {
        self.saved.log.last().map_or_else(
            || {
                self.saved
                    .snapshot
                    .as_ref()
                    .map_or(0, |snapshot| snapshot.term)
            },
            |entry| entry.term,
        )
    }
    pub(crate) fn begin_read(&mut self) -> Result<super::ReadTicket, RaftStorageError> {
        self.check()?;
        self.node.begin_read().map_err(|e| self.read_error(e))
    }
    pub(crate) fn poll_read(
        &self,
        ticket: &mut super::ReadTicket,
        applied: LogIndex,
    ) -> Result<Option<LogIndex>, RaftStorageError> {
        self.check()?;
        self.node
            .poll_read(ticket, applied)
            .map_err(|e| self.read_error(e))
    }
    fn read_error(&self, error: super::read::ReadError) -> RaftStorageError {
        match error {
            super::read::ReadError::NotLeader => {
                RaftStorageError::NotLeader(self.node.leader_hint.clone())
            }
            super::read::ReadError::Capacity => RaftStorageError::Capacity,
            super::read::ReadError::Invalid => invalid("foreign or completed read ticket"),
        }
    }
    /// Persistent observations reflect the last successfully published state.
    pub fn current_term(&self) -> Term {
        self.saved.term
    }
    pub fn voted_for(&self) -> Option<&str> {
        self.saved.vote.as_deref()
    }
    /// A closed owner can no longer advertise leadership.
    pub fn role(&self) -> Role {
        if self.is_closed() {
            Role::Follower
        } else {
            self.node.role
        }
    }
    pub fn commit_index(&self) -> LogIndex {
        self.saved.commit
    }
    pub fn log(&self) -> &[LogEntry] {
        &self.saved.log
    }
    pub fn leader_hint(&self) -> Option<&str> {
        if self.is_closed() {
            None
        } else {
            self.node.leader_hint.as_deref()
        }
    }

    pub fn is_closed(&self) -> bool {
        self.failed || self.pid != std::process::id()
    }

    fn check(&self) -> Result<(), RaftStorageError> {
        if self.is_closed() {
            Err(RaftStorageError::Closed)
        } else {
            Ok(())
        }
    }
    fn finish(&mut self, log_change: Option<usize>) -> Result<(), RaftStorageError> {
        // Recheck after core clock callbacks, including no-change transitions.
        self.check()?;
        if self.saved.base() != self.node.snapshot_index() {
            let mut saved = snapshot(&self.node, self.saved.cluster.clone());
            // Local compaction can retain committed entries after its boundary.
            saved.commit = self.node.commit_index;
            let result = Capacity::new(&saved).and_then(|capacity| {
                self.journal.checkpoint(&self.directory, &saved)?;
                Ok(capacity)
            });
            return match result {
                Ok(capacity) => {
                    self.capacity = capacity;
                    self.saved = saved;
                    Ok(())
                }
                Err(error) => self.fence(error),
            };
        }
        if self.saved.term == self.node.current_term
            && self.saved.vote == self.node.voted_for
            && log_change.is_none()
            && self.saved.commit == self.node.commit_index
        {
            return Ok(());
        }
        let keep = log_change.unwrap_or(self.node.log.len());
        let suffix = &self.node.log[keep..];
        let result: Result<Vec<usize>, RaftStorageError> = (|| {
            let sizes = self.capacity.project(keep, suffix)?;
            self.capacity.check_actual(&self.node, keep, &sizes)?;
            let delta = (
                self.node.current_term,
                &self.node.voted_for,
                keep as u64,
                suffix,
                self.node.commit_index,
            );
            self.journal
                .publish(&self.directory, &self.saved, &self.node, &delta)?;
            Ok(sizes)
        })();
        match result {
            Ok(sizes) => {
                self.capacity.replace(keep, &sizes);
                self.saved.log.truncate(keep);
                self.saved.log.extend_from_slice(suffix);
                self.saved.term = self.node.current_term;
                self.saved.vote = self.node.voted_for.clone();
                self.saved.commit = self.node.commit_index;
                Ok(())
            }
            Err(error) => self.fence(error),
        }
    }
    fn fence(&mut self, error: RaftStorageError) -> Result<(), RaftStorageError> {
        self.failed = true;
        self.node.outbox.clear();
        self.node.pending_apply.clear();
        Err(error)
    }
    fn sync_directory(&self) -> Result<(), RaftStorageError> {
        File::open(&self.directory)
            .map_err(storage)?
            .sync_all()
            .map_err(storage)
    }
}
fn snapshot(node: &RaftNode, cluster: String) -> Saved {
    Saved {
        version: if node.snapshot().is_some() { 3 } else { 1 },
        cluster,
        id: node.id.clone(),
        peers: node.peers.clone(),
        term: node.current_term,
        vote: node.voted_for.clone(),
        log: node.log.clone(),
        commit: node.commit_index,
        snapshot: node.snapshot().cloned().map(Arc::new),
    }
}
fn validate(s: &Saved) -> Result<(), RaftStorageError> {
    if s.log.len() > MAX_ENTRIES || s.log.iter().any(|e| e.cmd.len() > MAX_COMMAND) {
        return Err(RaftStorageError::Capacity);
    }
    let base = s.base();
    let last = base
        .checked_add(s.log.len() as u64)
        .ok_or_else(|| invalid("log index overflow"))?;
    if s.snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.index == 0
            || snapshot.term == 0
            || snapshot.term > s.term
            || snapshot.data.len() > super::snapshot::MAX_BYTES
    }) || s.commit < base
        || s.commit > last
        || (s.term == 0 && s.vote.is_some())
        || s.vote
            .as_ref()
            .is_some_and(|v| v != &s.id && !s.peers.contains(v))
    {
        return Err(invalid("invalid commit index or vote"));
    }
    let mut previous_term = s.snapshot.as_ref().map_or(0, |snapshot| snapshot.term);
    for (i, entry) in s.log.iter().enumerate() {
        if entry.index != base + i as u64 + 1
            || entry.term == 0
            || entry.term > s.term
            || entry.term < previous_term
        {
            return Err(invalid("invalid log index/term ordering"));
        }
        previous_term = entry.term;
    }
    Ok(())
}
fn installed(saved: &Saved, snapshot: &RaftSnapshot, term: Term) -> Saved {
    let mut next = saved.clone();
    next.version = 3;
    next.term = term;
    if term > saved.term {
        next.vote = None;
    }
    let slot = snapshot
        .index
        .checked_sub(saved.base())
        .and_then(|n| usize::try_from(n).ok());
    next.log = match slot {
        Some(slot)
            if slot > 0
                && saved
                    .log
                    .get(slot - 1)
                    .is_some_and(|e| e.term == snapshot.term) =>
        {
            saved.log[slot..].to_vec()
        }
        _ => Vec::new(),
    };
    next.commit = saved.commit.max(snapshot.index);
    next.snapshot = Some(Arc::new(snapshot.clone()));
    next
}
fn preflight(saved: &Saved) -> Result<(), RaftStorageError> {
    validate(saved)?;
    let capacity = Capacity::new(saved)?;
    capacity.check(saved.log.len(), &[])
}
fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
}
fn invalid(s: &str) -> RaftStorageError {
    RaftStorageError::Invalid(s.into())
}
fn storage(e: std::io::Error) -> RaftStorageError {
    RaftStorageError::Storage(e.to_string())
}
impl From<std::io::Error> for RaftStorageError {
    fn from(e: std::io::Error) -> Self {
        storage(e)
    }
}
struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if b.len() > MAX_SNAPSHOT - self.0 {
            return Err(std::io::Error::other("snapshot limit"));
        }
        self.0 += b.len();
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
enum Kind {
    Read,
    Lock,
    New,
    Append,
    Recover,
}
fn regular(path: &Path, kind: Kind) -> Result<File, RaftStorageError> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    match kind {
        Kind::Read => {}
        Kind::Lock => {
            opts.write(true).create(true);
        }
        Kind::New => {
            opts.write(true).create_new(true);
        }
        Kind::Append => {
            opts.append(true);
        }
        Kind::Recover => {
            opts.write(true);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(invalid("symlink entry forbidden"));
    }
    let file = opts.open(path).map_err(storage)?;
    if !file.metadata().map_err(storage)?.is_file() {
        return Err(invalid("regular file required"));
    }
    Ok(file)
}
