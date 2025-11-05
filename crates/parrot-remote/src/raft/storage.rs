//! Synchronous durable host for the in-memory Raft core.
//!
//! Every persistent transition is synced before RPCs or committed entries can
//! escape. Run on a dedicated owner thread, not a Tokio IO worker. The private
//! directory and lock inode must remain stable; hostile filesystem writers are
//! outside this cooperative ownership contract.
use super::{Clock, LogEntry, LogIndex, NodeId, RaftNode, RaftRpc, Role, Term};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    version: u8,
    cluster: String,
    id: NodeId,
    peers: Vec<NodeId>,
    term: Term,
    vote: Option<NodeId>,
    log: Vec<LogEntry>,
    commit: LogIndex,
}

/// Opt-in disk-backed Raft owner with fixed membership and a bounded full log.
///
/// `open` requires an existing, private directory. The snapshot binds its cluster,
/// node and canonical member set. Reconfiguration by editing these is rejected.
/// A fresh application state machine must replay `committed()` after every open;
/// replay is not an exactly-once contract for external effects. Propose returns a
/// durable local log index, **not** quorum commitment. Drop does not flush work:
/// successful operations have already synced their state.
pub struct DurableRaft {
    node: RaftNode,
    saved: Saved,
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
        Self::open_validated(directory, cluster, id, peers, clock, |_| Ok(()))
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
        let saved = if exists {
            let mut bytes = Vec::new();
            regular(&main, Kind::Read)?
                .take(MAX_SNAPSHOT as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(storage)?;
            if bytes.len() < 40
                || bytes.len() > MAX_SNAPSHOT
                || &bytes[..8] != MAGIC
                || Sha256::digest(&bytes[40..])[..] != bytes[8..40]
            {
                return Err(invalid("corrupt snapshot"));
            }
            let saved: Saved = parrot_api::message::serde_remote_deserialize_with_limits(
                &bytes[40..],
                parrot_api::message::RemoteDecodeLimits {
                    max_payload_bytes: MAX_SNAPSHOT,
                    // Vec<u8> visits each byte. Bound total visits for payload
                    // bytes plus log metadata, without trusting declared lengths.
                    max_values: MAX_SNAPSHOT + MAX_ENTRIES * 8,
                    ..Default::default()
                },
            )
            .map_err(RaftStorageError::Invalid)?;
            if saved.version != 1
                || saved.cluster != cluster
                || saved.id != node.id
                || saved.peers != node.peers
            {
                return Err(invalid("snapshot cluster/node/membership mismatch"));
            }
            validate(&saved)?;
            for entry in &saved.log {
                validate_command(&entry.cmd).map_err(RaftStorageError::Invalid)?;
            }
            node.current_term = saved.term;
            node.voted_for = saved.vote.clone();
            node.log = saved.log.clone();
            node.commit_index = saved.commit;
            node.last_applied = saved.commit;
            node.pending_apply = saved.log[..saved.commit as usize].to_vec();
            saved
        } else {
            // Never initialize over an interrupted first publication.
            match std::fs::symlink_metadata(&next) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(storage(e)),
                Ok(_) => return Err(invalid("incomplete initial snapshot; refusing reset")),
            }
            snapshot(&node, cluster)
        };
        let this = Self {
            node,
            saved,
            directory,
            failed: false,
            pid: std::process::id(),
            _owner: owner,
        };
        if exists {
            // Validate committed state before removing any abandoned temporary.
            match std::fs::symlink_metadata(&next) {
                Ok(_) => {
                    drop(regular(&next, Kind::Read)?);
                    std::fs::remove_file(&next).map_err(storage)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(storage(e)),
            }
            regular(&main, Kind::Read)?.sync_all().map_err(storage)?;
            this.sync_directory()?;
        } else {
            this.publish(&this.saved)?;
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
        self.finish()
    }
    pub fn step(&mut self, from: NodeId, rpc: RaftRpc) -> Result<(), RaftStorageError> {
        self.check()?;
        // Invalid protocol metadata is a caller error, not a failed disk
        // publication. Reject it before mutating the core or fencing its owner.
        rpc.validate_shape().map_err(invalid)?;
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
                        self.check_log_capacity(&self.node.log[..keep], &entries[skip..])?;
                    }
                }
            }
        }
        self.node.step(from, rpc);
        self.finish()
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
            index: self.node.log.len() as u64 + 1,
            term: self.node.current_term,
            cmd,
        };
        self.check_log_capacity(&self.node.log, std::slice::from_ref(&entry))?;
        let index = self
            .node
            .propose(entry.cmd)
            .map_err(|e| RaftStorageError::NotLeader(e.0))?;
        self.finish()?;
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
    /// Count the v1 snapshot representation without copying the existing log.
    /// Reserve the largest possible vote and integer metadata, so a newly
    /// admitted log cannot make a later election/commit exceed the byte limit.
    fn check_log_capacity(
        &self,
        prefix: &[LogEntry],
        suffix: &[LogEntry],
    ) -> Result<(), RaftStorageError> {
        if prefix.len().saturating_add(suffix.len()) > MAX_ENTRIES {
            return Err(RaftStorageError::Capacity);
        }
        let longest_voter = std::iter::once(&self.saved.id)
            .chain(&self.saved.peers)
            .max_by_key(|id| id.len());
        // Field order is the Saved v1 schema; borrowed strings/sequences have
        // the same encoding as their owned counterparts.
        let reserved = (
            self.saved.version,
            &self.saved.cluster,
            &self.saved.id,
            &self.saved.peers,
            u64::MAX,
            longest_voter,
            LogParts(prefix, suffix),
            u64::MAX,
        );
        bincode::serde::encode_into_std_write(
            reserved,
            &mut Counter(40),
            bincode::config::standard(),
        )
        .map_err(|_| RaftStorageError::Capacity)?;
        Ok(())
    }

    fn finish(&mut self) -> Result<(), RaftStorageError> {
        if self.saved.term == self.node.current_term
            && self.saved.vote == self.node.voted_for
            && self.saved.log == self.node.log
            && self.saved.commit == self.node.commit_index
        {
            return Ok(());
        }
        let saved = snapshot(&self.node, self.saved.cluster.clone());
        let result = validate(&saved).and_then(|()| self.publish(&saved));
        if result.is_err() {
            // Mutation may already have happened, and rename may have succeeded.
            // Do not expose any messages or apply work until explicit recovery.
            self.failed = true;
            self.node.outbox.clear();
            self.node.pending_apply.clear();
        } else {
            self.saved = saved;
        }
        result
    }
    fn sync_directory(&self) -> Result<(), RaftStorageError> {
        File::open(&self.directory)
            .map_err(storage)?
            .sync_all()
            .map_err(storage)
    }
    fn publish(&self, saved: &Saved) -> Result<(), RaftStorageError> {
        let mut counter = Counter(40);
        bincode::serde::encode_into_std_write(saved, &mut counter, bincode::config::standard())
            .map_err(|_| RaftStorageError::Capacity)?;
        let body = bincode::serde::encode_to_vec(saved, bincode::config::standard())
            .map_err(|e| RaftStorageError::Storage(e.to_string()))?;
        let next = self.directory.join("raft.next");
        let mut file = regular(&next, Kind::New)?;
        file.write_all(MAGIC)
            .and_then(|()| file.write_all(&Sha256::digest(&body)))
            .and_then(|()| file.write_all(&body))
            .and_then(|()| file.sync_all())
            .map_err(storage)?;
        std::fs::rename(next, self.directory.join("raft.state")).map_err(storage)?;
        self.sync_directory()
    }
}
fn snapshot(node: &RaftNode, cluster: String) -> Saved {
    Saved {
        version: 1,
        cluster,
        id: node.id.clone(),
        peers: node.peers.clone(),
        term: node.current_term,
        vote: node.voted_for.clone(),
        log: node.log.clone(),
        commit: node.commit_index,
    }
}
fn validate(s: &Saved) -> Result<(), RaftStorageError> {
    if s.log.len() > MAX_ENTRIES || s.log.iter().any(|e| e.cmd.len() > MAX_COMMAND) {
        return Err(RaftStorageError::Capacity);
    }
    if s.commit > s.log.len() as u64
        || (s.term == 0 && s.vote.is_some())
        || s.vote
            .as_ref()
            .is_some_and(|v| v != &s.id && !s.peers.contains(v))
    {
        return Err(invalid("invalid commit index or vote"));
    }
    let mut previous_term = 0;
    for (i, entry) in s.log.iter().enumerate() {
        if entry.index != i as u64 + 1
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
struct LogParts<'a>(&'a [LogEntry], &'a [LogEntry]);
impl serde::Serialize for LogParts<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(Some(self.0.len() + self.1.len()))?;
        for entry in self.0.iter().chain(self.1) {
            sequence.serialize_element(entry)?;
        }
        sequence.end()
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
