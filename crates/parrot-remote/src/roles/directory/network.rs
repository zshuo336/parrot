//! An explicitly assembled, fixed-membership Directory over authenticated Wire
//! ASK/REPLY. One disk thread owns Raft and the applied state; one bounded async
//! sender per peer reconnects and delivers its outbox. Queries are local reads.
use super::{
    DirCmd, DirEntry, DirQuery, DurableDirectory, DurableDirectoryError, DurableDirectoryProposal,
    DurableDirectoryStatus,
};
use crate::{
    raft::{RaftRpc, RealClock, Role},
    ErrCode, Frame, LocalLookup, NodeAddr, PeerAuthentication, RemoteActorSystem, RemoteConfig,
    RemoteSecurity, ReplyPayload,
};
use bytes::Bytes;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{mpsc as sync, Arc},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub const RAFT_KEY: &str = "parrot:directory:raft#v1";
pub const RAFT_ACK_KEY: &str = "parrot:directory:raft-accepted#v1";
pub const RAFT_PATH: &str = "/_directory/raft/v1";
const MAX_RPC: usize = 4 * 1024 * 1024;
const COMMAND_BYTES: usize = 8 * 1024 * 1024;
const MAX_PEERS: usize = 16;
const TICK: Duration = Duration::from_millis(20);

/// Shared timing configuration; its jitter window scales with election_ms.
pub use parrot_config::directory::DirectoryTiming;

#[derive(Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize)]
pub enum NetworkDirectoryError {
    #[error("invalid Directory request/configuration: {0}")]
    Invalid(String),
    #[error("Directory is not leader (hint: {0:?})")]
    NotLeader(Option<String>),
    #[error("Directory peer not authorized")]
    Forbidden,
    #[error("Directory admission at capacity")]
    Capacity,
    #[error("Directory owner closed")]
    Closed,
    #[error("Directory operation failed: {0}")]
    Operation(String),
    #[error("Directory deadline elapsed; request may have committed")]
    Timeout,
}
impl NetworkDirectoryError {
    pub(crate) fn code(&self) -> ErrCode {
        match self {
            Self::Invalid(_) => ErrCode::ProtocolViolation,
            Self::Forbidden => ErrCode::Forbidden,
            Self::Capacity => ErrCode::Overloaded,
            Self::Closed => ErrCode::Stopped,
            Self::Operation(_) => ErrCode::CodecError,
            Self::NotLeader(_) => ErrCode::RouteUnreachable,
            Self::Timeout => ErrCode::Timeout,
        }
    }
}
type Result<T> = std::result::Result<T, NetworkDirectoryError>;
fn invalid(e: impl ToString) -> NetworkDirectoryError {
    NetworkDirectoryError::Invalid(e.to_string())
}
fn durable_error(e: DurableDirectoryError) -> NetworkDirectoryError {
    use crate::raft::RaftStorageError;
    match e {
        DurableDirectoryError::Storage(RaftStorageError::NotLeader(hint)) => {
            NetworkDirectoryError::NotLeader(hint)
        }
        DurableDirectoryError::Closed
        | DurableDirectoryError::Storage(RaftStorageError::Closed) => NetworkDirectoryError::Closed,
        DurableDirectoryError::Storage(RaftStorageError::Capacity)
        | DurableDirectoryError::Command(super::DirectoryApplyError::Capacity) => {
            NetworkDirectoryError::Capacity
        }
        DurableDirectoryError::Storage(RaftStorageError::Invalid(reason)) => invalid(reason),
        DurableDirectoryError::Command(e) => invalid(e),
        other => operation(other),
    }
}
fn operation(e: impl ToString) -> NetworkDirectoryError {
    NetworkDirectoryError::Operation(e.to_string())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct WireRpc {
    group: String,
    rpc: RaftRpc,
}
fn decode(bytes: &[u8], group: &str, peer: &str) -> Result<RaftRpc> {
    let wire: WireRpc = parrot_api::message::serde_remote_deserialize_with_limits(
        bytes,
        parrot_api::message::RemoteDecodeLimits {
            max_payload_bytes: MAX_RPC,
            // Vec<u8> commands visit one value per byte. Bound both bytes and
            // aggregate visits so declared hostile lengths cannot allocate freely.
            max_values: MAX_RPC + 16_384,
            max_depth: 12,
        },
    )
    .map_err(invalid)?;
    if wire.group != group {
        return Err(NetworkDirectoryError::Forbidden);
    }
    let claimed = match &wire.rpc {
        RaftRpc::RequestVote { candidate, .. } => Some(candidate),
        RaftRpc::AppendEntries { leader, .. } => Some(leader),
        _ => None,
    };
    if claimed.is_some_and(|id| id != peer) {
        return Err(NetworkDirectoryError::Forbidden);
    }
    wire.rpc.validate_shape().map_err(invalid)?;
    Ok(wire.rpc)
}

/// A local observation, never a lease or proof of current quorum leadership.
#[derive(Debug, Clone)]
pub struct DirectoryObservation {
    pub role: Role,
    pub term: u64,
    pub applied_index: u64,
    pub leader: Option<String>,
    pub closed: bool,
    pub terminal_error: Option<NetworkDirectoryError>,
}
impl Default for DirectoryObservation {
    fn default() -> Self {
        Self {
            role: Role::Follower,
            term: 0,
            applied_index: 0,
            leader: None,
            closed: false,
            terminal_error: None,
        }
    }
}
enum Request {
    Read(
        DirQuery,
        Instant,
        OwnedSemaphorePermit,
        oneshot::Sender<Result<super::DirectoryRead>>,
    ),
    Client(
        String,
        Bytes,
        oneshot::Sender<Result<super::DirectoryRequestStatus>>,
    ),
    Propose(Vec<u8>, oneshot::Sender<Result<DurableDirectoryProposal>>),
    Receive(String, Bytes, oneshot::Sender<Result<()>>),
    Query(DirQuery, oneshot::Sender<Result<Option<DirEntry>>>),
    Status(
        DurableDirectoryProposal,
        oneshot::Sender<Result<DurableDirectoryStatus>>,
    ),
}
struct Command {
    request: Request,
    _budget: OwnedSemaphorePermit,
}
struct PendingRead {
    ticket: super::DirectoryReadTicket,
    deadline: Instant,
    reply: oneshot::Sender<Result<super::DirectoryRead>>,
    _slot: OwnedSemaphorePermit,
    _budget: OwnedSemaphorePermit,
}

/// Clones do not own the disk thread or network lifetime. Drop/close of the
/// NetworkDirectory fences all handles. Cancelling a submitted write may leave
/// it committed; use its receipt if available, not a blind non-idempotent retry.
#[derive(Clone)]
pub struct DirectoryHandle {
    group: Arc<str>,
    reads: Arc<Semaphore>,
    tx: sync::SyncSender<Command>,
    budget: Arc<Semaphore>,
    stop: CancellationToken,
    observation: watch::Receiver<DirectoryObservation>,
    peers: Arc<Vec<String>>,
    pid: u32,
}
impl DirectoryHandle {
    /// Admitted linear reads, including queued commands and pending quorum waits.
    /// Released after completion, cancellation cleanup or owner shutdown.
    pub fn pending_linear_reads(&self) -> usize {
        64 - self.reads.available_permits()
    }
    pub(crate) fn group(&self) -> &str {
        &self.group
    }
    /// A fresh log barrier per invocation. The deadline covers admission and
    /// quorum wait; timeout/cancellation can leave a harmless committed barrier.
    pub async fn query_linearizable(
        &self,
        query: &DirQuery,
        timeout: Duration,
    ) -> Result<super::DirectoryRead> {
        self.check()?;
        let size = super::validate_query(query).map_err(invalid)?;
        if timeout.is_zero() || timeout > Duration::from_secs(60) {
            return Err(invalid("read timeout must be in (0,60s]"));
        }
        let slot = self
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| NetworkDirectoryError::Capacity)?;
        let (tx, rx) = oneshot::channel();
        self.submit(
            Request::Read(query.clone(), Instant::now() + timeout, slot, tx),
            size,
        )?;
        tokio::time::timeout(timeout, self.response(rx))
            .await
            .map_err(|_| NetworkDirectoryError::Timeout)?
    }
    pub(crate) async fn client_request(
        &self,
        client: &str,
        payload: Bytes,
    ) -> Result<super::DirectoryRequestStatus> {
        self.check()?;
        let size = payload.len() + client.len();
        let (tx, rx) = oneshot::channel();
        self.submit(Request::Client(client.into(), payload, tx), size)?;
        self.response(rx).await
    }
    fn check(&self) -> Result<()> {
        if self.pid != std::process::id() || self.stop.is_cancelled() {
            Err(NetworkDirectoryError::Closed)
        } else {
            Ok(())
        }
    }
    fn submit(&self, request: Request, bytes: usize) -> Result<()> {
        self.check()?;
        let budget = self
            .budget
            .clone()
            .try_acquire_many_owned(
                u32::try_from(bytes.max(1)).map_err(|_| NetworkDirectoryError::Capacity)?,
            )
            .map_err(|_| NetworkDirectoryError::Capacity)?;
        self.tx
            .try_send(Command {
                request,
                _budget: budget,
            })
            .map_err(|e| match e {
                sync::TrySendError::Full(_) => NetworkDirectoryError::Capacity,
                sync::TrySendError::Disconnected(_) => NetworkDirectoryError::Closed,
            })
    }
    async fn response<T>(&self, rx: oneshot::Receiver<Result<T>>) -> Result<T> {
        tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err(NetworkDirectoryError::Closed),
            result = rx => result.map_err(|_| NetworkDirectoryError::Closed)?,
        }
    }
    /// Wait for the disk worker's terminal state, including its failure reason.
    pub async fn wait_closed(&self) -> DirectoryObservation {
        if self.pid != std::process::id() {
            return self.observation();
        }
        let mut status = self.observation.clone();
        while !status.borrow_and_update().closed {
            if status.changed().await.is_err() {
                break;
            }
        }
        self.observation()
    }
    pub fn observation(&self) -> DirectoryObservation {
        let mut view = self.observation.borrow().clone();
        if self.check().is_err() {
            view.closed = true;
            view.role = Role::Follower;
            view.leader = None;
        }
        view
    }
    pub async fn propose(&self, cmd: &DirCmd) -> Result<DurableDirectoryProposal> {
        self.check()?;
        let bytes = cmd.encode().map_err(invalid)?;
        let size = bytes.len();
        let (tx, rx) = oneshot::channel();
        self.submit(Request::Propose(bytes, tx), size)?;
        self.response(rx).await
    }
    pub async fn proposal_status(
        &self,
        receipt: &DurableDirectoryProposal,
    ) -> Result<DurableDirectoryStatus> {
        let (tx, rx) = oneshot::channel();
        self.submit(Request::Status(receipt.clone(), tx), 1)?;
        self.response(rx).await
    }
    /// Read only the locally applied state. A partitioned former leader may be stale.
    pub async fn query_local(&self, query: &DirQuery) -> Result<Option<DirEntry>> {
        let text = match query {
            DirQuery::Resolve { prefix_or_node } => prefix_or_node,
            DirQuery::BorderFor { prefix } => prefix,
            DirQuery::NodesForKey { key } => key,
        };
        if text.len() > 4096 {
            return Err(NetworkDirectoryError::Capacity);
        }
        let (tx, rx) = oneshot::channel();
        self.submit(Request::Query(query.clone(), tx), text.len())?;
        self.response(rx).await
    }
    // Only ingress may call this with its authenticated transport identity.
    pub(crate) async fn receive(&self, peer: &str, bytes: Bytes) -> Result<()> {
        self.check()?;
        if !self.peers.iter().any(|p| p == peer) {
            return Err(NetworkDirectoryError::Forbidden);
        }
        if bytes.len() > MAX_RPC {
            return Err(NetworkDirectoryError::Capacity);
        }
        let (tx, rx) = oneshot::channel();
        let size = bytes.len();
        self.submit(Request::Receive(peer.into(), bytes, tx), size)?;
        self.response(rx).await
    }
}

struct Lookup {
    handle: DirectoryHandle,
    fallback: Arc<dyn LocalLookup>,
}
#[async_trait::async_trait]
impl LocalLookup for Lookup {
    async fn lookup(&self, path: &str) -> Option<Box<dyn parrot_api::address::ActorRef>> {
        self.fallback.lookup(path).await
    }
    fn directory_receiver(&self) -> Option<DirectoryHandle> {
        Some(self.handle.clone())
    }
    fn durable_receiver(&self, path: &str) -> Option<Arc<dyn crate::durable::DurableReceiver>> {
        self.fallback.durable_receiver(path)
    }
    fn forwarding_target(&self, path: &str) -> Option<String> {
        self.fallback.forwarding_target(path)
    }
}

type PendingPeer = (Vec<NodeAddr>, mpsc::Receiver<Bytes>);

/// Owns a single Directory group and its RemoteActorSystem. Membership is fixed
/// on disk. Alternate addresses identify the same voter. Standard parrot-node
/// installs this owner when its Directory role and persistent group are configured.
pub struct NetworkDirectory {
    handle: DirectoryHandle,
    worker: Option<std::thread::JoinHandle<()>>,
    remote: Option<Arc<RemoteActorSystem>>,
    tasks: TaskTracker,
    pending: Option<Vec<PendingPeer>>,
    attempt_timeout: Duration,
}
impl NetworkDirectory {
    pub async fn start(
        directory: impl Into<PathBuf>,
        group: impl Into<String>,
        config: RemoteConfig,
        peers: Vec<NodeAddr>,
        fallback: Arc<dyn LocalLookup>,
    ) -> Result<Self> {
        Self::start_with_timing(
            directory,
            group,
            config,
            peers,
            fallback,
            DirectoryTiming::default(),
        )
        .await
    }
    pub async fn start_with_timing(
        directory: impl Into<PathBuf>,
        group: impl Into<String>,
        config: RemoteConfig,
        peers: Vec<NodeAddr>,
        fallback: Arc<dyn LocalLookup>,
        timing: DirectoryTiming,
    ) -> Result<Self> {
        let mut owner =
            Self::prepare_with_timing(directory, group, config, peers, fallback, timing).await?;
        owner.start_transport().await?;
        Ok(owner)
    }
    /// Open and validate storage without accepting or dialing network traffic.
    /// This allows an application to finish actor/gateway assembly before start.
    pub async fn prepare_with_timing(
        directory: impl Into<PathBuf>,
        group: impl Into<String>,
        config: RemoteConfig,
        peers: Vec<NodeAddr>,
        fallback: Arc<dyn LocalLookup>,
        timing: DirectoryTiming,
    ) -> Result<Self> {
        timing.validate().map_err(invalid)?;
        tokio::runtime::Handle::try_current().map_err(invalid)?;
        let directory = directory.into();
        let group = group.into();
        if group.is_empty() || group.len() > 1011 || group.chars().any(char::is_control) {
            return Err(invalid("invalid group identifier"));
        }
        if !(matches!(config.security, RemoteSecurity::MutualTls { .. })
            || (config.scheme == "mem" && matches!(config.security, RemoteSecurity::Local)))
        {
            return Err(invalid(
                "Directory requires mTLS, or process-local memory security",
            ));
        }
        let mut routes: HashMap<String, Vec<NodeAddr>> = HashMap::new();
        if peers.len() > 64 {
            return Err(NetworkDirectoryError::Capacity);
        }
        for peer in peers {
            parrot_config::endpoint::validate_node_id(&peer.node_id).map_err(invalid)?;
            if peer.node_id.len() > 1024
                || !peer.identity_is_explicit
                || peer.scheme != config.scheme
                || peer.node_id == config.node_id
            {
                return Err(invalid("invalid or self peer route"));
            }
            let candidates = routes.entry(peer.node_id.clone()).or_default();
            if candidates.contains(&peer) {
                return Err(invalid("duplicate peer route"));
            }
            candidates.push(peer);
        }
        if routes.len() > MAX_PEERS {
            return Err(NetworkDirectoryError::Capacity);
        }
        let members: Vec<_> = routes.keys().cloned().collect();
        let id = config.node_id.clone();
        let (tx, rx) = sync::sync_channel(64);
        let (status, observation) = watch::channel(DirectoryObservation::default());
        let stop = CancellationToken::new();
        let handle = DirectoryHandle {
            group: Arc::from(group.as_str()),
            reads: Arc::new(Semaphore::new(64)),
            tx,
            budget: Arc::new(Semaphore::new(COMMAND_BYTES)),
            stop: stop.clone(),
            observation,
            peers: Arc::new(members.clone()),
            pid: std::process::id(),
        };
        let mut outputs = HashMap::new();
        let mut inputs = Vec::new();
        for (id, candidates) in routes {
            let (tx, rx) = mpsc::channel(1);
            outputs.insert(id, tx);
            inputs.push((candidates, rx));
        }
        let (ready, opened) = oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("parrot-directory".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut owner = DurableDirectory::open(
                        directory,
                        &group,
                        id,
                        members,
                        Arc::new(RealClock {
                            election_ms: timing.election_ms,
                            heartbeat_ms: timing.heartbeat_ms,
                        }),
                    )
                    .map_err(operation)?;
                    publish(&owner, &status);
                    let _ = ready.send(());
                    let mut tick = Instant::now();
                    let mut reads = Vec::new();
                    while !stop.is_cancelled() {
                        if tick.elapsed() >= TICK {
                            owner.tick().map_err(operation)?;
                            tick = Instant::now();
                        }
                        match rx.recv_timeout(TICK.saturating_sub(tick.elapsed())) {
                            Ok(command) if !stop.is_cancelled() => {
                                apply(&mut owner, &group, command, &mut reads)
                            }
                            Ok(_) => break,
                            Err(sync::RecvTimeoutError::Timeout) => {}
                            Err(sync::RecvTimeoutError::Disconnected) => break,
                        }
                        if owner.is_closed() {
                            return Err(operation("durable owner fenced after failure"));
                        }
                        finish_reads(&owner, &mut reads);
                        publish(&owner, &status);
                        for (peer, rpc) in owner.outbox().map_err(operation)? {
                            let payload = bincode::serde::encode_to_vec(
                                WireRpc {
                                    group: group.clone(),
                                    rpc,
                                },
                                bincode::config::standard(),
                            )
                            .map_err(operation)?;
                            if payload.len() > MAX_RPC {
                                return Err(operation("outbound Raft RPC exceeds wire budget"));
                            }
                            if let Some(output) = outputs.get(&peer) {
                                // Dropped messages are retried by Raft's next election
                                // or heartbeat. Never block disk progress on a peer.
                                let _ = output.try_send(Bytes::from(payload));
                            }
                        }
                    }
                    Ok(())
                }))
                .unwrap_or_else(|_| Err(operation("Directory worker panicked")));
                stop.cancel();
                status.send_modify(|view| {
                    view.closed = true;
                    view.role = Role::Follower;
                    view.leader = None;
                    view.terminal_error = result.err();
                });
            })
            .map_err(operation)?;
        // Construct the cancellation guard before awaiting readiness. Cancelling
        // startup, a bind failure or any later error must release the file lock.
        let mut host = Self {
            handle,
            worker: Some(worker),
            remote: None,
            tasks: TaskTracker::new(),
            pending: Some(inputs),
            attempt_timeout: Duration::from_millis(timing.rpc_timeout_ms),
        };
        if opened.await.is_err() {
            host.wait_worker().await;
            return Err(host
                .handle
                .observation()
                .terminal_error
                .unwrap_or(NetworkDirectoryError::Closed));
        }
        let remote = RemoteActorSystem::new(
            config,
            Arc::new(Lookup {
                handle: host.handle.clone(),
                fallback,
            }),
        )
        .map_err(operation)?;
        host.remote = Some(remote);
        Ok(host)
    }
    /// Start network admission and peer pumps after local application assembly.
    /// Repeated calls are harmless; a closed owner cannot be restarted.
    pub async fn start_transport(&mut self) -> Result<()> {
        self.handle.check()?;
        let remote = self.remote().clone();
        remote.start().await.map_err(operation)?;
        let Some(inputs) = self.pending.take() else {
            return Ok(());
        };
        for (peers, rx) in inputs {
            self.tasks.spawn(pump(
                Arc::downgrade(&remote),
                self.handle.stop.clone(),
                peers,
                rx,
                self.attempt_timeout,
            ));
        }
        let stop = self.handle.stop.clone();
        let mut shutdown = remote.shutdown_signal();
        self.tasks.spawn(async move {
            tokio::select! {
                _ = stop.cancelled() => {},
                _ = shutdown.wait_for(|closed| *closed) => { stop.cancel(); },
            }
        });
        Ok(())
    }
    pub fn handle(&self) -> DirectoryHandle {
        self.handle.clone()
    }
    pub fn remote(&self) -> &Arc<RemoteActorSystem> {
        self.remote.as_ref().expect("started Directory")
    }
    pub async fn close(&mut self) -> Result<()> {
        if self.handle.pid != std::process::id() {
            return Err(NetworkDirectoryError::Closed);
        }
        self.handle.stop.cancel();
        self.tasks.close();
        if let Some(remote) = &self.remote {
            remote.shutdown().await.map_err(operation)?;
        }
        self.tasks.wait().await;
        self.wait_worker().await;
        self.handle.observation().terminal_error.map_or(Ok(()), Err)
    }
    async fn wait_worker(&mut self) {
        let mut status = self.handle.observation.clone();
        while !status.borrow_and_update().closed {
            if status.changed().await.is_err() {
                break;
            }
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for NetworkDirectory {
    fn drop(&mut self) {
        if self.handle.pid == std::process::id() {
            self.handle.stop.cancel();
            self.tasks.close();
            if let Some(remote) = &self.remote {
                remote.begin_close();
            }
            // This can wait for an already-started fsync; it never needs Tokio
            // scheduling. Use close().await for the network completion barrier.
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}
fn publish(owner: &DurableDirectory, status: &watch::Sender<DirectoryObservation>) {
    status.send_replace(DirectoryObservation {
        role: owner.role(),
        term: owner.current_term(),
        applied_index: owner.applied_index(),
        leader: owner.leader_hint().map(str::to_owned),
        closed: false,
        terminal_error: None,
    });
}
fn apply(
    owner: &mut DurableDirectory,
    group: &str,
    command: Command,
    reads: &mut Vec<PendingRead>,
) {
    match command.request {
        Request::Read(query, deadline, slot, reply) => {
            if reply.is_closed() {
                return;
            }
            if Instant::now() >= deadline {
                let _ = reply.send(Err(NetworkDirectoryError::Timeout));
                return;
            }
            match owner.begin_linear_read(&query).map_err(durable_error) {
                Ok(ticket) => reads.push(PendingRead {
                    ticket,
                    deadline,
                    reply,
                    _slot: slot,
                    _budget: command._budget,
                }),
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
        }
        Request::Client(client, bytes, reply) => {
            if !reply.is_closed() {
                let result = super::client::decode(&bytes).and_then(|request| {
                    if request.group != group {
                        return Err(NetworkDirectoryError::Forbidden);
                    }
                    match request.operation {
                        super::client::Operation::Submit { id, command } => owner
                            .submit_request(&client, &id, &command)
                            .map_err(durable_error),
                        super::client::Operation::Status { id, digest } => owner
                            .request_status(&client, &id, digest)
                            .map_err(durable_error),
                    }
                });
                let _ = reply.send(result);
            }
        }
        Request::Propose(bytes, reply) => {
            if !reply.is_closed() {
                let result = DirCmd::decode(&bytes)
                    .map_err(invalid)
                    .and_then(|cmd| owner.propose(&cmd).map_err(durable_error));
                let _ = reply.send(result);
            }
        }
        Request::Receive(peer, bytes, reply) => {
            if !reply.is_closed() {
                let result = decode(&bytes, group, &peer)
                    .and_then(|rpc| owner.step(peer, rpc).map_err(durable_error));
                let _ = reply.send(result);
            }
        }
        Request::Query(query, reply) => {
            let _ = reply.send(owner.query_local(&query).map_err(operation));
        }
        Request::Status(receipt, reply) => {
            let _ = reply.send(owner.proposal_status(&receipt).map_err(operation));
        }
    }
}
fn finish_reads(owner: &DurableDirectory, reads: &mut Vec<PendingRead>) {
    let mut i = 0;
    while i < reads.len() {
        let pending = &mut reads[i];
        if pending.reply.is_closed() {
            reads.swap_remove(i);
            continue;
        }
        let result = if Instant::now() >= pending.deadline {
            Err(NetworkDirectoryError::Timeout)
        } else {
            owner
                .poll_linear_read(&mut pending.ticket)
                .map_err(durable_error)
        };
        match result {
            Ok(None) => i += 1,
            result => {
                let pending = reads.swap_remove(i);
                let _ = pending
                    .reply
                    .send(result.map(|read| read.expect("completed read")));
            }
        }
    }
}
async fn pump(
    remote: std::sync::Weak<RemoteActorSystem>,
    stop: CancellationToken,
    peers: Vec<NodeAddr>,
    mut rx: mpsc::Receiver<Bytes>,
    attempt_timeout: Duration,
) {
    let node_id = peers[0].node_id.clone();
    let mut candidate = 0usize;
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            payload = rx.recv() => {
                let Some(payload) = payload else { break; };
                let Some(system) = remote.upgrade() else { break; };
                let attempt = async {
                    if system.sender_of(&node_id).is_none_or(|s| s.node_id() != node_id) {
                        let route = &peers[candidate];
                        candidate = (candidate + 1) % peers.len();
                        system.connect(route).await.map_err(operation)?;
                    }
                    let sender = system.sender_of(&node_id).ok_or(NetworkDirectoryError::Closed)?;
                    if sender.node_id() != node_id || !matches!(sender.authentication(), PeerAuthentication::MutualTls | PeerAuthentication::InProcess) {
                        return Err(NetworkDirectoryError::Forbidden);
                    }
                    let cid = system.callbacks.next_cid();
                    let (tx, reply) = oneshot::channel();
                    system.callbacks.insert_on(cid, node_id.clone(), tx, &sender).map_err(operation)?;
                    let _pending = system.callbacks.guard(cid);
                    sender.send(Frame::ask(cid, &format!("parrot://{}{RAFT_PATH}", node_id), RAFT_KEY, payload, None)).await.map_err(operation)?;
                    let response = tokio::select! {
                        biased;
                        reply = reply => reply.map_err(operation)?,
                        _ = sender.closed() => return Err(NetworkDirectoryError::Closed),
                    };
                    match response {
                        ReplyPayload::Ok(bytes, key) if key == RAFT_ACK_KEY && bytes.is_empty() => Ok(()),
                        _ => Err(operation("Raft transport acceptance rejected")),
                    }
                };
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    result = tokio::time::timeout(attempt_timeout, attempt) => {
                        if !matches!(result, Ok(Ok(()))) {
                            tracing::debug!(peer = %node_id, ?result, "Directory Raft delivery failed; protocol will retry");
                        }
                    },
                }
            }
        }
    }
}
