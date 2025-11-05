//! Owned, bounded, at-least-once WAL delivery. See docs/DURABLE_OUTBOX.md.
use super::timing::{Phase, TARGET};
use super::{
    ack_record, crc32, encoded_size, open_regular, sync_dir, OpenKind, Wal, WalLimits, WalRecord,
};
use crate::{NodeAddr, RemoteActorSystem};
use bytes::Bytes;
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

mod group;

#[derive(Debug, Clone, thiserror::Error)]
pub enum OutboxError {
    #[error("invalid outbox configuration or input: {0}")]
    Invalid(String),
    #[error("outbox command capacity exhausted")]
    Capacity,
    #[error("outbox admission rejected: {0}")]
    Rejected(String),
    #[error("outbox storage failure: {0}")]
    Storage(String),
    #[error("outbox closed")]
    Closed,
    #[error("outbox worker failed: {0}")]
    Worker(String),
}

#[derive(Debug, Clone)]
pub struct OutboxConfig {
    /// Enable durable submission IDs (directory binding v4; ordinary uses v3).
    /// Existing v1/v2 directories upgrade atomically without changing this mode.
    pub idempotent_submissions: bool,
    /// Require authenticated durable identity and a transactional receiver.
    /// Persisted as part of the directory binding; cannot change on reopen.
    pub transactional: bool,
    pub wal_limits: WalLimits,
    pub segment_bytes: u64,
    /// Queued commands, in addition to one bounded group being persisted.
    pub command_capacity: usize,
    /// Includes type key, payload and optional causal-prefix bytes of commands.
    pub command_bytes: u32,
    /// Maximum commands collected for one acceptance sync (1 disables grouping).
    pub group_commit_max_records: usize,
    /// Maximum collection delay from the first command, at most one second.
    /// Zero collects only commands already queued; it does not delay admission.
    pub group_commit_interval: Duration,
    /// Includes dialing, writer admission and the completion receipt.
    pub attempt_timeout: Duration,
    pub retry_min: Duration,
    pub retry_max: Duration,
}
impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            idempotent_submissions: false,
            transactional: false,
            wal_limits: WalLimits::default(),
            segment_bytes: 64 * 1024 * 1024,
            command_capacity: 256,
            command_bytes: 16 * 1024 * 1024,
            group_commit_max_records: 64,
            group_commit_interval: Duration::from_millis(10),
            attempt_timeout: Duration::from_secs(10),
            retry_min: Duration::from_millis(100),
            retry_max: Duration::from_secs(30),
        }
    }
}
impl OutboxConfig {
    fn validate(&self) -> Result<(), OutboxError> {
        self.wal_limits
            .validate(self.segment_bytes)
            .map_err(OutboxError::Invalid)?;
        if self.wal_limits.max_segments < 2
            || self.command_capacity == 0
            || self.command_capacity > Semaphore::MAX_PERMITS
            || self.command_bytes == 0
            || self.command_bytes as usize > Semaphore::MAX_PERMITS
            || !(1..=65536).contains(&self.group_commit_max_records)
            || self.group_commit_interval > Duration::from_secs(1)
            || self.attempt_timeout.is_zero()
            || self.attempt_timeout > Duration::from_secs(86400)
            || self.retry_min.is_zero()
            || self.retry_min > self.retry_max
            || self.retry_max > Duration::from_secs(86400)
        {
            return Err(OutboxError::Invalid(
                "invalid queue/deadline/backoff limits".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct OutboxStatus {
    /// Retained submission IDs, including completed work. Never evicted silently.
    pub submission_receipts: usize,
    pub pending: usize,
    /// Persisted acceptance, not remote business completion.
    pub last_enqueued: u64,
    /// Completion receipt received and cumulative WAL ACK synced.
    pub last_acked: u64,
    pub attempts: u64,
    /// Successful acceptance group syncs since open (excludes ACK, rotation and
    /// identity syncs). Not a count of all filesystem sync system calls.
    pub acceptance_syncs: u64,
    pub last_group_records: usize,
    /// Bounded diagnostic for the last unsuccessful network attempt.
    pub last_error: Option<String>,
    pub closed: bool,
    pub terminal_error: Option<OutboxError>,
}
struct Command {
    parent: Option<parrot_api::trace::TraceParent>,
    request_id: Option<String>,
    key: String,
    payload: Bytes,
    _bytes: OwnedSemaphorePermit,
    reply: oneshot::Sender<Result<u64, OutboxError>>,
}

/// Clones do not own the worker. Dropping/closing DurableOutbox closes all handles.
#[derive(Clone)]
pub struct OutboxHandle {
    tx: mpsc::Sender<Command>,
    bytes: Arc<Semaphore>,
    status: watch::Receiver<OutboxStatus>,
    stop: CancellationToken,
    pid: u32,
    identity: Arc<Identity>,
}
impl OutboxHandle {
    /// Persist opaque encoded business bytes before acknowledging acceptance.
    /// Admission is fail-fast and bounded; it does not wait for queue capacity.
    /// Cancellation after command publication is ambiguous: the record may be
    /// persisted and delivered. Do not blindly resubmit non-idempotent work.
    pub fn enqueue<'a>(
        &'a self,
        type_key: &'a str,
        payload: &'a [u8],
    ) -> impl std::future::Future<Output = Result<u64, OutboxError>> + 'a {
        parrot_api::trace::capture(async move { self.enqueue_inner(None, type_key, payload).await })
    }
    /// Persist a caller-chosen ID and business bytes atomically. A matching
    /// retry keeps the first accepted sequence and parent, including on reopen.
    /// Causal metadata is not part of the business-content fingerprint.
    pub fn enqueue_idempotent<'a>(
        &'a self,
        request_id: &'a str,
        type_key: &'a str,
        payload: &'a [u8],
    ) -> impl std::future::Future<Output = Result<u64, OutboxError>> + 'a {
        parrot_api::trace::capture(async move {
            if !self.identity.idempotent() {
                return Err(OutboxError::Invalid(
                    "idempotent submissions are not enabled".into(),
                ));
            }
            super::submission::validate_id(request_id).map_err(OutboxError::Invalid)?;
            self.enqueue_inner(Some(request_id), type_key, payload)
                .await
        })
    }
    async fn enqueue_inner(
        &self,
        request_id: Option<&str>,
        type_key: &str,
        payload: &[u8],
    ) -> Result<u64, OutboxError> {
        if std::process::id() != self.pid || self.stop.is_cancelled() {
            return Err(OutboxError::Closed);
        }
        let parent = parrot_api::trace::TraceContext::current().map(|context| context.as_parent());
        let size = self
            .identity
            .validate_message(type_key, payload.len(), parent.is_some())?
            .checked_add(request_id.map_or(0, |id| id.len() as u32))
            .ok_or(OutboxError::Capacity)?;
        let permit = self.tx.try_reserve().map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => OutboxError::Capacity,
            mpsc::error::TrySendError::Closed(_) => OutboxError::Closed,
        })?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(size)
            .map_err(|_| OutboxError::Capacity)?;
        let (tx, rx) = oneshot::channel();
        permit.send(Command {
            parent,
            request_id: request_id.map(str::to_owned),
            key: type_key.into(),
            payload: Bytes::copy_from_slice(payload),
            _bytes: bytes,
            reply: tx,
        });
        rx.await.map_err(|_| {
            self.status
                .borrow()
                .terminal_error
                .clone()
                .unwrap_or(OutboxError::Closed)
        })?
    }
    pub fn status(&self) -> OutboxStatus {
        self.status.borrow().clone()
    }

    /// Wait for a previously accepted sequence's persisted completion ACK.
    /// Cancellation only stops this wait. It does not remove durable work.
    pub async fn wait_acked(&self, seq: u64) -> Result<(), OutboxError> {
        let mut rx = self.status.clone();
        if std::process::id() != self.pid {
            return Err(OutboxError::Closed);
        }
        loop {
            let state = rx.borrow_and_update().clone();
            if seq == 0 || seq > state.last_enqueued {
                return Err(OutboxError::Invalid(
                    "sequence was not accepted by this outbox".into(),
                ));
            }
            if seq <= state.last_acked {
                return Ok(());
            }
            if state.closed {
                return Err(state.terminal_error.unwrap_or(OutboxError::Closed));
            }
            rx.changed().await.map_err(|_| OutboxError::Closed)?;
        }
    }
}

/// One directory, one immutable sender/receiver/path binding and one ordered
/// in-flight request. File I/O runs on an owned OS thread, never a Tokio worker.
/// Drop cancels network work and joins storage; slow filesystem calls can delay
/// Drop. Prefer close().await. Closing retains unacknowledged work for reopen.
pub struct DurableOutbox {
    handle: OutboxHandle,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl DurableOutbox {
    /// `sender` is a stable, caller-chosen stream/session ID in this local WAL.
    /// In transactional mode it is transmitted and bound to the authenticated
    /// source node and receiver path; ordinary completion mode keeps it local.
    /// `path` is a local actor path; receiver.node_id is explicitly bound.
    /// The supplied runtime and RemoteActorSystem must stay live until close.
    pub async fn open(
        directory: impl Into<PathBuf>,
        system: &Arc<RemoteActorSystem>,
        receiver: NodeAddr,
        path: &str,
        sender: &str,
        config: OutboxConfig,
    ) -> Result<Self, OutboxError> {
        config.validate()?;
        // Fail before creating a directory or binding work to a destination
        // which this host's transport/security configuration can never dial.
        system
            .config
            .validate_peer_address(&receiver)
            .map_err(|e| OutboxError::Invalid(e.to_string()))?;
        if !receiver.identity_is_explicit
            || receiver.node_id.is_empty()
            || receiver.node_id.len() > 1024
            || receiver.node_id.contains(['/', '\0'])
            || !path.starts_with('/')
            || path.len() > 4096
            || path.contains('\0')
            || sender.is_empty()
            || sender.len() > 1024
            || sender.contains('\0')
            || system.config.node_id.len() > 1024
        {
            return Err(OutboxError::Invalid(
                "explicit receiver, local path and bounded stream ID required".into(),
            ));
        }
        parrot_config::endpoint::validate_node_id(&receiver.node_id)
            .map_err(OutboxError::Invalid)?;
        let identity = Identity {
            version: if config.idempotent_submissions { 4 } else { 3 },
            transactional: config.transactional,
            source: system.config.node_id.clone(),
            receiver: receiver.node_id.clone(),
            path: path.into(),
            sender: sender.into(),
        };
        super::reserve_watermark(0, &identity.receiver, &identity.sender, config.wal_limits)
            .map_err(OutboxError::Invalid)?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|e| OutboxError::Invalid(e.to_string()))?;
        let (tx, rx) = mpsc::channel(config.command_capacity);
        let (status_tx, status) = watch::channel(OutboxStatus::default());
        let stop = CancellationToken::new();
        let handle = OutboxHandle {
            tx,
            bytes: Arc::new(Semaphore::new(config.command_bytes as usize)),
            status,
            stop: stop.clone(),
            pid: std::process::id(),
            identity: Arc::new(identity.clone()),
        };
        let (ready_tx, ready_rx) = oneshot::channel();
        let weak = Arc::downgrade(system);
        let directory = directory.into();
        let worker = std::thread::Builder::new()
            .name("parrot-outbox".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let wal =
                        Wal::open_with_limits(directory, config.segment_bytes, config.wal_limits)
                            .map_err(OutboxError::Storage)?;
                    let state = bind_and_recover(&wal, &identity)?;
                    status_tx.send_replace(state);
                    let _ = ready_tx.send(());
                    // The original runtime owns any transport tasks spawned by connect.
                    // This future (including disk I/O) is polled on this dedicated thread.
                    runtime.block_on(run(
                        wal, identity, weak, receiver, config, rx, &stop, &status_tx,
                    ))
                }))
                .unwrap_or_else(|_| Err(OutboxError::Worker("worker panicked".into())));
                stop.cancel();
                status_tx.send_modify(|s| {
                    s.closed = true;
                    s.terminal_error = result.err();
                });
            })
            .map_err(|e| OutboxError::Worker(e.to_string()))?;
        let mut owner = Self {
            handle,
            worker: Some(worker),
        };
        if ready_rx.await.is_err() {
            // Closed is published only after storage ownership has been released.
            owner.wait_closed().await;
            let error = owner
                .handle
                .status()
                .terminal_error
                .unwrap_or(OutboxError::Closed);
            owner.join();
            return Err(error);
        }
        Ok(owner)
    }
    pub fn handle(&self) -> OutboxHandle {
        self.handle.clone()
    }
    pub async fn close(&mut self) -> Result<(), OutboxError> {
        if std::process::id() != self.handle.pid {
            return Err(OutboxError::Closed);
        }
        self.handle.stop.cancel();
        self.wait_closed().await;
        self.join();
        self.handle.status().terminal_error.map_or(Ok(()), Err)
    }
    async fn wait_closed(&self) {
        let mut rx = self.handle.status.clone();
        while !rx.borrow_and_update().closed {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            if std::process::id() == self.handle.pid {
                let _ = worker.join();
            }
        }
    }
}
impl Drop for DurableOutbox {
    fn drop(&mut self) {
        if std::process::id() != self.handle.pid {
            return;
        }
        self.handle.stop.cancel();
        self.join();
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Identity {
    version: u32,
    #[serde(default)]
    transactional: bool,
    source: String,
    receiver: String,
    path: String,
    sender: String,
}

impl Identity {
    fn idempotent(&self) -> bool {
        matches!(self.version, 2 | 4)
    }
    // One predicate for fresh acceptance and recovered records. Persisting a
    // request that this mode can never put on the wire poisons the ordered queue.
    fn validate_message(
        &self,
        type_key: &str,
        payload_len: usize,
        traced: bool,
    ) -> Result<u32, OutboxError> {
        let trace_bytes = if traced {
            crate::frame::TRACE_CONTEXT_BYTES
        } else {
            0
        };
        let path_bytes = "parrot://".len() + self.receiver.len() + self.path.len();
        if type_key.is_empty()
            || super::submission::reserved(type_key)
            || super::causal::reserved(type_key)
            || matches!(
                type_key,
                "#ack" | super::DURABLE_KEY | super::DURABLE_RECEIPT_KEY
            )
        {
            return Err(OutboxError::Invalid("empty/reserved type key".into()));
        }
        let size = type_key
            .len()
            .checked_add(payload_len)
            .and_then(|n| n.checked_add(trace_bytes))
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(OutboxError::Capacity)?;
        if path_bytes
            .checked_add(size as usize)
            .and_then(|n| n.checked_add(crate::frame::BODY_FIXED_OVERHEAD))
            .is_none_or(|n| n > crate::frame::MAX_FRAME_LEN as usize)
        {
            return Err(OutboxError::Invalid("wire frame exceeds limit".into()));
        }
        if self.transactional {
            let probe = super::DurableEnvelope {
                source: self.source.clone(),
                session: self.sender.clone(),
                receiver: self.receiver.clone(),
                path: self.path.clone(),
                sequence: 1,
                type_key: type_key.into(),
                payload: bytes::Bytes::new(),
            };
            probe.validate().map_err(OutboxError::Invalid)?;
            if probe
                .encoded_len()
                .map_err(OutboxError::Invalid)?
                .checked_add(payload_len)
                .and_then(|n| {
                    n.checked_add(
                        path_bytes
                            + super::DURABLE_KEY.len()
                            + crate::frame::BODY_FIXED_OVERHEAD
                            + trace_bytes,
                    )
                })
                .is_none_or(|n| n > crate::frame::MAX_FRAME_LEN as usize)
            {
                return Err(OutboxError::Invalid("durable frame exceeds limit".into()));
            }
        }
        Ok(size)
    }
}

fn bind_and_recover(wal: &Wal, identity: &Identity) -> Result<OutboxStatus, OutboxError> {
    const LIMIT: u64 = 16 * 1024;
    let state = wal.state().map_err(OutboxError::Storage)?;
    let binding = wal.dir.join("outbox.identity");
    let mut upgrade = false;
    match std::fs::symlink_metadata(&binding) {
        Ok(_) => {
            let file = open_regular(&binding, OpenKind::Read).map_err(OutboxError::Storage)?;
            let mut bytes = Vec::new();
            file.take(LIMIT + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| OutboxError::Storage(e.to_string()))?;
            if bytes.len() < 4
                || bytes.len() as u64 > LIMIT
                || crc32(&bytes[4..]) != u32::from_le_bytes(bytes[..4].try_into().unwrap())
            {
                return Err(OutboxError::Storage("corrupt outbox identity".into()));
            }
            let saved: Identity = serde_json::from_slice(&bytes[4..])
                .map_err(|e| OutboxError::Storage(e.to_string()))?;
            if &saved != identity {
                let mut compatible = saved.clone();
                compatible.version = identity.version;
                if compatible != *identity
                    || !matches!((saved.version, identity.version), (1, 3) | (2, 4))
                {
                    return Err(OutboxError::Invalid(
                        "outbox identity does not match saved binding".into(),
                    ));
                }
                if state.pending.iter().any(|record| record.parent.is_some()) {
                    return Err(OutboxError::Storage(
                        "causal WAL data requires an upgraded directory binding".into(),
                    ));
                }
                upgrade = true;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !state.pending.is_empty() || !state.acked.is_empty() || state.receipts.len() != 0 {
                return Err(OutboxError::Storage(
                    "missing identity for nonempty outbox WAL".into(),
                ));
            }
            let bytes =
                serde_json::to_vec(identity).map_err(|e| OutboxError::Invalid(e.to_string()))?;
            if bytes.len() + 4 > LIMIT as usize {
                return Err(OutboxError::Invalid("identity exceeds byte budget".into()));
            }
            // No requests can precede this durable binding. Interrupted creation
            // fails closed on reopen; it never silently selects a new destination.
            let mut file = open_regular(&binding, OpenKind::New).map_err(OutboxError::Storage)?;
            file.write_all(&crc32(&bytes).to_le_bytes())
                .and_then(|_| file.write_all(&bytes))
                .and_then(|_| file.sync_all())
                .map_err(|e| OutboxError::Storage(e.to_string()))?;
            sync_dir(&wal.dir).map_err(OutboxError::Storage)?;
        }
        Err(e) => return Err(OutboxError::Storage(e.to_string())),
    }
    if state
        .acked
        .keys()
        .any(|(e, s)| e != &identity.receiver || s != &identity.sender)
    {
        return Err(OutboxError::Storage("foreign ACK stream in outbox".into()));
    }
    let acked = state
        .acked
        .get(&(identity.receiver.clone(), identity.sender.clone()))
        .copied()
        .unwrap_or(0);
    let mut last = acked;
    for rec in &state.pending {
        if rec.endpoint != identity.receiver
            || rec.sender != identity.sender
            || rec.path != identity.path
            || last.checked_add(1) != Some(rec.seq)
        {
            return Err(OutboxError::Storage(
                "foreign, duplicate or noncontiguous outbox record".into(),
            ));
        }
        identity
            .validate_message(&rec.type_key, rec.payload.len(), rec.parent.is_some())
            .map_err(|e| OutboxError::Storage(format!("invalid saved request: {e}")))?;
        last = rec.seq;
    }
    if !identity.idempotent() && state.receipts.len() != 0 {
        return Err(OutboxError::Storage(
            "submission receipts require v2 directory binding".into(),
        ));
    }
    state
        .receipts
        .validate_binding(&identity.receiver, &identity.sender, last)
        .map_err(OutboxError::Storage)?;
    // No migration until all recovered bindings, sequences and budgets pass.
    if upgrade {
        binding::upgrade(wal, identity)?;
    }
    // Re-establish rename durability after an interrupted migration before any
    // newly accepted work can depend on the version that protects old readers.
    open_regular(&binding, OpenKind::Read)
        .map_err(OutboxError::Storage)?
        .sync_all()
        .map_err(|error| OutboxError::Storage(error.to_string()))?;
    sync_dir(&wal.dir).map_err(OutboxError::Storage)?;
    Ok(OutboxStatus {
        submission_receipts: state.receipts.len(),
        pending: state.pending.len(),
        last_enqueued: last,
        last_acked: acked,
        ..Default::default()
    })
}

fn append_for_group(
    wal: &Wal,
    identity: &Identity,
    cmd: &Command,
    seq: u64,
) -> Result<(u64, bool), String> {
    let rec = WalRecord {
        endpoint: identity.receiver.clone(),
        sender: identity.sender.clone(),
        seq,
        path: identity.path.clone(),
        type_key: cmd.key.clone(),
        payload: cmd.payload.clone(),
    };
    {
        let state = wal.state()?;
        if let Some(id) = &cmd.request_id {
            if let Some(seq) = state.receipts.lookup(&rec, id)? {
                return Ok((seq, false));
            }
        }
        if seq == 0 {
            return Err("sequence exhausted".into());
        }
        let (stored, extra_checkpoint) = if let Some(id) = &cmd.request_id {
            let (stored, receipt) = super::submission::wrap(&rec, id)?;
            state.receipts.check(&receipt, wal.limits)?;
            (stored, encoded_size(&receipt.record()?, wal.limits)?)
        } else {
            (rec.clone(), 0)
        };
        let stored = super::causal::wrap(stored, cmd.parent)?;
        // Reserve room to ACK every accepted record, including the final
        // checkpoint. Otherwise a full disk budget could deadlock delivery.
        let record_size = encoded_size(&stored, wal.limits)?;
        let ack_size = encoded_size(
            &ack_record(&identity.receiver, &identity.sender, u64::MAX),
            wal.limits,
        )?;
        let reserve = state
            .pending
            .len()
            .checked_add(1)
            .and_then(|n| n.checked_mul(ack_size))
            .and_then(|n| n.checked_add(record_size))
            .and_then(|n| n.checked_add(state.receipts.checkpoint_bytes))
            .and_then(|n| n.checked_add(extra_checkpoint))
            .ok_or("ACK reservation overflow")?;
        super::ensure_disk(&state, reserve, wal.limits)?;
        let rotate = state.seg_bytes + record_size as u64 >= wal.segment_max;
        if state.segments.saturating_add(usize::from(rotate)) >= wal.limits.max_segments
            || state.seg_id.checked_add(1 + u64::from(rotate)).is_none()
        {
            return Err("outbox must reserve a checkpoint segment".into());
        }
    }
    wal.append_submission_with_parent(rec, cmd.request_id.as_deref(), cmd.parent)
}

#[allow(clippy::too_many_arguments)]
async fn run(
    wal: Wal,
    identity: Identity,
    system: Weak<RemoteActorSystem>,
    receiver: NodeAddr,
    config: OutboxConfig,
    mut rx: mpsc::Receiver<Command>,
    stop: &CancellationToken,
    status: &watch::Sender<OutboxStatus>,
) -> Result<(), OutboxError> {
    let mut backoff = config.retry_min;
    let mut next_attempt = tokio::time::Instant::now();
    loop {
        if stop.is_cancelled() {
            break;
        }
        let record = wal
            .state()
            .map_err(OutboxError::Storage)?
            .pending
            .first()
            .cloned();
        let Some(record) = record else {
            tokio::select! { biased;
                _ = stop.cancelled() => break,
                cmd = rx.recv() => match cmd { Some(c) => group::accept(&wal, &identity, c, &mut rx, stop, status, &config).await?, None => break },
            }
            continue;
        };
        let span = tracing::trace_span!(target: TARGET, "outbox.request",
            source = %identity.source, session = %identity.sender,
            receiver = %identity.receiver, path = %identity.path, sequence = record.seq,
            attempt = status.borrow().attempts.saturating_add(1));
        let attempt_at = next_attempt;
        let attempt = parrot_api::trace::scope(
            record
                .parent
                .map(parrot_api::trace::TraceContext::remote_parent),
            async {
                tokio::time::sleep_until(attempt_at).await;
                status.send_modify(|s| s.attempts = s.attempts.saturating_add(1));
                let deadline = tokio::time::Instant::now() + config.attempt_timeout;
                let timing = Phase::new(span.clone(), "outbox.attempt");
                let result = tokio::time::timeout_at(deadline, async {
                    let system = system
                        .upgrade()
                        .ok_or("remote system dropped".to_string())?;
                    if system.sender_of(&receiver.node_id).is_none() {
                        let connection = Phase::new(span.clone(), "outbox.connect");
                        let result = system.connect(&receiver).await;
                        connection.finish(result.is_ok());
                        result.map_err(|e| e.to_string())?;
                    }
                    let reference = system
                        .remote_ref(&format!("parrot://{}{}", identity.receiver, identity.path))
                        .map_err(|e| e.to_string())?;
                    if identity.transactional {
                        reference
                            .deliver_durable_before(
                                &super::DurableEnvelope {
                                    source: identity.source.clone(),
                                    session: identity.sender.clone(),
                                    receiver: identity.receiver.clone(),
                                    path: identity.path.clone(),
                                    sequence: record.seq,
                                    type_key: record.type_key.clone(),
                                    payload: record.payload.clone(),
                                },
                                deadline,
                            )
                            .await
                            .map_err(|e| e.to_string())
                    } else {
                        reference
                            .deliver_encoded_before(
                                &record.type_key,
                                record.payload.clone(),
                                deadline,
                            )
                            .await
                            .map_err(|e| e.to_string())
                    }
                })
                .await
                .unwrap_or_else(|_| {
                    Err("outbox attempt deadline exceeded; execution uncertain".into())
                });
                timing.finish(result.is_ok());
                result
            },
        )
        .instrument(span.clone());
        tokio::pin!(attempt);
        loop {
            tokio::select! { biased;
                _ = stop.cancelled() => return wal.sync().map_err(OutboxError::Storage),
                result = &mut attempt => {
                    match result {
                        Ok(()) => {
                            let timing = Phase::new(span.clone(), "outbox.ack_sync");
                            let result = wal.ack(&identity.receiver, &identity.sender, record.seq)
                                .and_then(|()| wal.sync()).map_err(OutboxError::Storage);
                            timing.finish(result.is_ok());
                            result?;
                            status.send_modify(|s| { s.last_acked = record.seq; s.pending -= 1; s.last_error = None; });
                            backoff = config.retry_min;
                            next_attempt = tokio::time::Instant::now();
                        }
                        Err(error) => {
                            status.send_modify(|s| s.last_error = Some(error.chars().take(1024).collect()));
                            next_attempt = tokio::time::Instant::now() + backoff;
                            backoff = backoff.saturating_mul(2).min(config.retry_max);
                        }
                    }
                    break;
                }
                cmd = rx.recv() => match cmd {
                    Some(c) => group::accept(&wal, &identity, c, &mut rx, stop, status, &config).await?,
                    None => return wal.sync().map_err(OutboxError::Storage),
                },
            }
        }
    }
    wal.sync().map_err(OutboxError::Storage)
}

mod binding;
