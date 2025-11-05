//! Atomic state + dedup snapshots, owned by one bounded storage worker.
use super::timing::{Phase, TARGET};
use super::{crc32, open_regular, sync_dir, DurableEnvelope, OpenKind, WalOwner};
use bytes::Bytes;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

mod unwind;
use unwind::{drop_catching, Transition};

#[derive(Debug, Clone, thiserror::Error)]
pub enum InboxError {
    #[error("invalid inbox input: {0}")]
    Invalid(String),
    #[error("inbox resource budget exhausted")]
    Capacity,
    #[error("inbox expects sequence {expected}, got {received}")]
    Gap { expected: u64, received: u64 },
    #[error("inbox business transition failed: {0}")]
    Business(String),
    #[error("inbox storage failed: {0}")]
    Storage(String),
    #[error("inbox closed")]
    Closed,
    #[error("inbox worker panicked")]
    Panicked,
}
/// Success certifies durable completion for this exact envelope. Implementations
/// must atomically persist business state and dedup, or use durable idempotency.
#[async_trait::async_trait]
pub trait DurableReceiver: Send + Sync + 'static {
    async fn receive(&self, message: DurableEnvelope) -> Result<(), InboxError>;
}
#[derive(Debug, Clone, Copy)]
pub struct InboxLimits {
    pub max_state_bytes: usize,
    pub max_snapshot_bytes: usize,
    pub max_streams: usize,
    pub max_stream_bytes: usize,
    pub command_capacity: usize,
    pub command_bytes: u32,
}
impl Default for InboxLimits {
    fn default() -> Self {
        Self {
            max_state_bytes: 1024 * 1024,
            max_snapshot_bytes: 4 * 1024 * 1024,
            max_streams: 4096,
            max_stream_bytes: 1024 * 1024,
            command_capacity: 256,
            command_bytes: 16 * 1024 * 1024,
        }
    }
}
impl InboxLimits {
    fn validate(self) -> Result<(), InboxError> {
        if [
            self.max_state_bytes,
            self.max_snapshot_bytes,
            self.max_streams,
            self.max_stream_bytes,
            self.command_capacity,
            self.command_bytes as usize,
        ]
        .into_iter()
        .any(|n| n == 0 || n > Semaphore::MAX_PERMITS)
            || self.max_snapshot_bytes > 16 * 1024 * 1024
            || self.max_state_bytes > self.max_snapshot_bytes
        {
            return Err(InboxError::Invalid("invalid limits".into()));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default)]
pub struct InboxSnapshot {
    pub state: Bytes,
    pub revision: u64,
    pub streams: usize,
    pub closed: bool,
    pub terminal_error: Option<InboxError>,
}
struct Command {
    message: DurableEnvelope,
    span: tracing::Span,
    context: Option<parrot_api::trace::TraceContext>,
    queued: Phase,
    _budget: OwnedSemaphorePermit,
    reply: oneshot::Sender<Result<(), InboxError>>,
}
#[derive(Clone)]
pub struct InboxHandle {
    tx: mpsc::Sender<Command>,
    budget: Arc<Semaphore>,
    status: watch::Receiver<InboxSnapshot>,
    stop: CancellationToken,
    pid: u32,
}
impl InboxHandle {
    /// A cheap immutable view. Callers retaining old views own their memory.
    pub fn snapshot(&self) -> InboxSnapshot {
        self.status.borrow().clone()
    }
    /// Trusted local submission. Network callers must pass through ingress, which
    /// authenticates source/relay identities. Cancellation after processing starts
    /// does not cancel the atomic transaction; retry the same identity/sequence.
    pub fn apply(
        &self,
        message: DurableEnvelope,
    ) -> impl std::future::Future<Output = Result<(), InboxError>> + '_ {
        parrot_api::trace::capture(async move {
            if self.pid != std::process::id() || self.stop.is_cancelled() {
                return Err(InboxError::Closed);
            }
            message.validate().map_err(InboxError::Invalid)?;
            let size = u32::try_from(message.encoded_len().map_err(InboxError::Invalid)?)
                .map_err(|_| InboxError::Capacity)?;
            let permit = self.tx.try_reserve().map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => InboxError::Capacity,
                mpsc::error::TrySendError::Closed(_) => InboxError::Closed,
            })?;
            let budget = self
                .budget
                .clone()
                .try_acquire_many_owned(size)
                .map_err(|_| InboxError::Capacity)?;
            let message = DurableEnvelope {
                payload: Bytes::copy_from_slice(&message.payload),
                ..message
            };
            let (tx, rx) = oneshot::channel();
            let span = tracing::trace_span!(target: TARGET, "inbox.request",
            source = %message.source, session = %message.session,
            receiver = %message.receiver, path = %message.path, sequence = message.sequence);
            let queued = Phase::new(span.clone(), "inbox.queue");
            permit.send(Command {
                message,
                context: parrot_api::trace::TraceContext::current(),
                span,
                queued,
                _budget: budget,
                reply: tx,
            });
            rx.await.map_err(|_| {
                self.status
                    .borrow()
                    .terminal_error
                    .clone()
                    .unwrap_or(InboxError::Closed)
            })?
        })
    }
}
#[async_trait::async_trait]
impl DurableReceiver for InboxHandle {
    async fn receive(&self, message: DurableEnvelope) -> Result<(), InboxError> {
        self.apply(message).await
    }
}

/// Transitions return the complete next state. They must have no external side
/// effects: only returned state and stream watermarks participate in the commit.
/// One owned thread performs all file I/O and calls transitions serially.
pub struct TransactionalInbox {
    handle: InboxHandle,
    worker: Option<std::thread::JoinHandle<()>>,
    join_failed: bool,
}
impl TransactionalInbox {
    pub fn open<'a, F>(
        directory: impl Into<PathBuf> + 'a,
        node: &'a str,
        path: &'a str,
        initial: Bytes,
        limits: InboxLimits,
        transition: F,
    ) -> impl std::future::Future<Output = Result<Self, InboxError>> + 'a
    where
        F: Fn(&[u8], &DurableEnvelope) -> Result<Bytes, String> + Send + 'static,
    {
        // Take protected ownership before returning a future: cancellation before
        // its first poll must not run an unguarded user callback destructor.
        let mut transition = Transition::new(transition);
        parrot_api::trace::capture(async move {
            limits.validate()?;
            parrot_config::endpoint::validate_node_id(node).map_err(InboxError::Invalid)?;
            if node.len() > 1024
                || !path.starts_with('/')
                || path.len() > 4096
                || path.contains('\0')
                || initial.len() > limits.max_state_bytes
            {
                return Err(InboxError::Invalid(
                    "invalid receiver identity or initial state".into(),
                ));
            }
            let (node, path, directory) = (node.to_owned(), path.to_owned(), directory.into());
            let initial = Bytes::copy_from_slice(&initial);
            let runtime = tokio::runtime::Handle::try_current()
                .map_err(|e| InboxError::Invalid(e.to_string()))?;
            let (tx, mut rx) = mpsc::channel::<Command>(limits.command_capacity);
            let (status_tx, status) = watch::channel(InboxSnapshot::default());
            let stop = CancellationToken::new();
            let handle = InboxHandle {
                tx,
                budget: Arc::new(Semaphore::new(limits.command_bytes as usize)),
                status,
                stop: stop.clone(),
                pid: std::process::id(),
            };
            let (ready_tx, ready_rx) = oneshot::channel();
            let worker = std::thread::Builder::new()
                .name("parrot-inbox".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let mut store = Store::open(directory, node, path, initial, limits)?;
                        status_tx.send_replace(store.view());
                        let _ = ready_tx.send(());
                        loop {
                            let cmd = runtime.block_on(async {
                                tokio::select! {
                                    biased;
                                    _ = stop.cancelled() => None,
                                    cmd = rx.recv() => cmd,
                                }
                            });
                            let Some(cmd) = cmd else {
                                break;
                            };
                            if cmd.reply.is_closed() {
                                continue;
                            }
                            cmd.queued.finish(true);
                            let timing = Phase::new(cmd.span.clone(), "inbox.transaction");
                            let result = parrot_api::trace::in_scope(cmd.context.clone(), || {
                                cmd.span
                                    .in_scope(|| store.apply(&cmd.message, transition.value()))
                            });
                            timing.finish(result.is_ok());
                            let fatal = store.failed;
                            if result.is_ok() {
                                status_tx.send_replace(store.view());
                            }
                            let _ = cmd.reply.send(result.clone());
                            if fatal {
                                return result;
                            }
                        }
                        Ok(())
                    }));
                    let mut result = match result {
                        Ok(result) => result,
                        Err(payload) => {
                            drop_catching(payload);
                            Err(InboxError::Panicked)
                        }
                    };
                    // A successful shutdown includes retiring the user's closure.
                    // Publish closure only after this independent unwind boundary.
                    if !transition.retire() && result.is_ok() {
                        result = Err(InboxError::Panicked);
                    }
                    stop.cancel();
                    status_tx.send_modify(|s| {
                        s.closed = true;
                        s.terminal_error = result.err();
                    });
                })
                .map_err(|e| InboxError::Storage(e.to_string()))?;
            let mut inbox = Self {
                handle,
                worker: Some(worker),
                join_failed: false,
            };
            if ready_rx.await.is_err() {
                inbox.wait_closed().await;
                inbox.join();
                return Err(inbox.handle.snapshot().terminal_error.unwrap_or(
                    if inbox.join_failed {
                        InboxError::Panicked
                    } else {
                        InboxError::Closed
                    },
                ));
            }
            Ok(inbox)
        })
    }
    pub fn handle(&self) -> InboxHandle {
        self.handle.clone()
    }
    pub async fn close(&mut self) -> Result<(), InboxError> {
        if self.handle.pid != std::process::id() {
            return Err(InboxError::Closed);
        }
        self.handle.stop.cancel();
        self.wait_closed().await;
        self.join();
        let state = self.handle.snapshot();
        if let Some(error) = state.terminal_error {
            Err(error)
        } else if self.join_failed || !state.closed {
            Err(InboxError::Panicked)
        } else {
            Ok(())
        }
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
            if self.handle.pid == std::process::id() {
                if let Err(payload) = worker.join() {
                    self.join_failed = true;
                    drop_catching(payload);
                }
            }
        }
    }
}
impl Drop for TransactionalInbox {
    fn drop(&mut self) {
        if self.handle.pid == std::process::id() {
            self.handle.stop.cancel();
            self.join();
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Saved {
    version: u8,
    node: String,
    path: String,
    revision: u64,
    state: Bytes,
    // Ordered serialization gives a canonical snapshot independent of hash seeds.
    streams: BTreeMap<(String, String), u64>,
}
struct Store {
    directory: PathBuf,
    saved: Saved,
    limits: InboxLimits,
    failed: bool,
    _owner: WalOwner,
}
impl Store {
    fn open(
        directory: PathBuf,
        node: String,
        path: String,
        initial: Bytes,
        limits: InboxLimits,
    ) -> Result<Self, InboxError> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory).map_err(storage)?;
        let file = open_regular(&directory.join(".parrot-inbox.lock"), OpenKind::Lock)
            .map_err(InboxError::Storage)?;
        file.try_lock()
            .map_err(|e| InboxError::Storage(format!("inbox owner lock: {e}")))?;
        let owner = WalOwner {
            file,
            pid: std::process::id(),
        };
        let main = directory.join("inbox.state");
        let next = directory.join("inbox.next");
        let saved = match std::fs::symlink_metadata(&main) {
            Ok(_) => {
                let file = open_regular(&main, OpenKind::Read).map_err(InboxError::Storage)?;
                let mut bytes = Vec::new();
                file.take(limits.max_snapshot_bytes as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(storage)?;
                if bytes.len() < 12
                    || bytes.len() > limits.max_snapshot_bytes
                    || &bytes[..8] != b"PINBOX1\0"
                    || crc32(&bytes[12..]) != u32::from_le_bytes(bytes[8..12].try_into().unwrap())
                {
                    return Err(InboxError::Storage("corrupt inbox snapshot".into()));
                }
                let saved: Saved = parrot_api::message::serde_remote_deserialize_with_limits(
                    &bytes[12..],
                    parrot_api::message::RemoteDecodeLimits {
                        max_payload_bytes: limits.max_snapshot_bytes,
                        // Saved and its six fields cost seven serde visits;
                        // each stream adds a tuple, two strings and a u64.
                        // Match the writer's configurable stream budget instead
                        // of imposing the general Wire decoder's hidden ceiling.
                        // The byte cap bounds this even for huge stream limits.
                        max_values: limits.max_streams.min(limits.max_snapshot_bytes) * 4 + 7,
                        ..Default::default()
                    },
                )
                .map_err(InboxError::Storage)?;
                if saved.version != 1 || saved.node != node || saved.path != path {
                    return Err(InboxError::Invalid(
                        "saved receiver binding mismatch".into(),
                    ));
                }
                saved
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(&next).is_ok() {
                    return Err(InboxError::Storage(
                        "incomplete initial snapshot; refusing reset".into(),
                    ));
                }
                Saved {
                    version: 1,
                    node,
                    path,
                    revision: 0,
                    state: initial,
                    streams: BTreeMap::new(),
                }
            }
            Err(e) => return Err(storage(e)),
        };
        let mut store = Self {
            directory,
            saved,
            limits,
            failed: false,
            _owner: owner,
        };
        store.validate(&store.saved)?;
        if std::fs::symlink_metadata(&main).is_ok() {
            // Only a validated committed snapshot permits dropping an abandoned
            // pre-rename temporary. Never follow a substituted special file.
            match std::fs::symlink_metadata(&next) {
                Ok(_) => {
                    drop(open_regular(&next, OpenKind::Read).map_err(InboxError::Storage)?);
                    std::fs::remove_file(&next).map_err(storage)?;
                    sync_dir(&store.directory).map_err(InboxError::Storage)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(storage(e)),
            }
            // A previous writer may have renamed successfully but failed the
            // directory sync. Re-establish durability before acknowledging a
            // recovered duplicate, even if no new transaction is necessary.
            open_regular(&main, OpenKind::Read)
                .map_err(InboxError::Storage)?
                .sync_all()
                .map_err(storage)?;
            sync_dir(&store.directory).map_err(InboxError::Storage)?;
        } else {
            store.write(&store.saved.clone())?;
        }
        Ok(store)
    }
    fn validate(&self, saved: &Saved) -> Result<(), InboxError> {
        let mut size = 0usize;
        for ((source, session), seq) in &saved.streams {
            if source.len() > 1024
                || parrot_config::endpoint::validate_node_id(source).is_err()
                || session.is_empty()
                || session.len() > 1024
                || session.contains('\0')
                || *seq == 0
            {
                return Err(InboxError::Storage("invalid saved stream identity".into()));
            }
            size = size
                .checked_add(source.len() + session.len())
                .ok_or(InboxError::Capacity)?;
        }
        if saved.state.len() > self.limits.max_state_bytes
            || saved.streams.len() > self.limits.max_streams
            || size > self.limits.max_stream_bytes
        {
            return Err(InboxError::Capacity);
        }
        Ok(())
    }
    fn view(&self) -> InboxSnapshot {
        InboxSnapshot {
            state: self.saved.state.clone(),
            revision: self.saved.revision,
            streams: self.saved.streams.len(),
            ..Default::default()
        }
    }
    fn apply<F>(&mut self, message: &DurableEnvelope, transition: &F) -> Result<(), InboxError>
    where
        F: Fn(&[u8], &DurableEnvelope) -> Result<Bytes, String>,
    {
        if message.receiver != self.saved.node || message.path != self.saved.path {
            return Err(InboxError::Invalid("receiver binding mismatch".into()));
        }
        let key = (message.source.clone(), message.session.clone());
        let watermark = self.saved.streams.get(&key).copied().unwrap_or(0);
        if message.sequence <= watermark {
            return Ok(());
        }
        let next = watermark
            .checked_add(1)
            .ok_or_else(|| InboxError::Invalid("stream exhausted".into()))?;
        if message.sequence != next {
            return Err(InboxError::Gap {
                expected: next,
                received: message.sequence,
            });
        }
        let mut saved = self.saved.clone();
        saved.streams.insert(key, message.sequence);
        self.validate(&saved)?;
        saved.revision = saved
            .revision
            .checked_add(1)
            .ok_or_else(|| InboxError::Invalid("revision exhausted".into()))?;
        // Only new, admissible transactions get a child record. Duplicates,
        // gaps and pre-transition budget rejection do not fabricate execution.
        let context = parrot_api::trace::TraceContext::current().map(|parent| parent.child());
        parrot_api::trace::in_scope(context, || {
            use parrot_api::trace::{SpanGuard, SpanKind, SpanStatus};
            let guard = SpanGuard::current(SpanKind::InboxTransaction, &message.path);
            let mut panicked = false;
            let result = (|| {
                saved.state = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    transition(&self.saved.state, message)
                })) {
                    Ok(result) => {
                        result.map_err(|e| InboxError::Business(e.chars().take(1024).collect()))?
                    }
                    Err(payload) => {
                        panicked = true;
                        drop_catching(payload);
                        return Err(InboxError::Business("transition panicked".into()));
                    }
                };
                self.validate(&saved)?;
                // Tight-copy user output to avoid retaining a huge sliced allocation.
                saved.state = Bytes::copy_from_slice(&saved.state);
                self.write(&saved)?;
                self.saved = saved;
                Ok(())
            })();
            guard.finish(if panicked {
                SpanStatus::Panicked
            } else if result.is_ok() {
                SpanStatus::Ok
            } else {
                SpanStatus::Error
            });
            result
        })
    }

    fn write(&mut self, saved: &Saved) -> Result<(), InboxError> {
        let mut counter = super::SizeCounter {
            size: 12,
            cap: self.limits.max_snapshot_bytes,
        };
        bincode::serde::encode_into_std_write(saved, &mut counter, bincode::config::standard())
            .map_err(|_| InboxError::Capacity)?;
        let body = bincode::serde::encode_to_vec(saved, bincode::config::standard())
            .map_err(|e| InboxError::Storage(e.to_string()))?;
        let result = (|| {
            let next = self.directory.join("inbox.next");
            let mut file = open_regular(&next, OpenKind::New).map_err(InboxError::Storage)?;
            file.write_all(b"PINBOX1\0")
                .and_then(|_| file.write_all(&crc32(&body).to_le_bytes()))
                .and_then(|_| file.write_all(&body))
                .map_err(storage)?;
            let timing = Phase::new(tracing::Span::current(), "inbox.file_sync");
            let result = file.sync_all().map_err(storage);
            timing.finish(result.is_ok());
            result?;
            std::fs::rename(next, self.directory.join("inbox.state")).map_err(storage)?;
            let timing = Phase::new(tracing::Span::current(), "inbox.directory_sync");
            let result = sync_dir(&self.directory).map_err(InboxError::Storage);
            timing.finish(result.is_ok());
            result?;
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}
fn storage(error: std::io::Error) -> InboxError {
    InboxError::Storage(error.to_string())
}
