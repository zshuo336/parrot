//! Bounded local or negotiated direct remote shadow mailbox admission.
//!
//! Policies see the original application path, before forwarding aliases. A
//! shadow is an independent ordinary TELL, never a second reply, durable write
//! or completion receipt. The host's LocalLookup must return raw local refs.
use crate::{
    codec_registry::CodecRegistry,
    ingress::{
        unwind::{self, Cleanup},
        LocalLookup,
    },
};
use parrot_api::trace::TraceContext;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// Local source prefix matching; the longest policy wins. A shadow may be a
/// canonical local path or a negotiated direct parrot://node/path destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireMirrorPolicy {
    pub source_prefix: String,
    pub shadow_path: String,
}
#[derive(Clone, Copy, Debug)]
pub struct WireMirrorOptions {
    /// Includes the active attempt as well as queued messages.
    pub capacity: usize,
    /// Encoded payload plus source path and type key; decoded memory is not bounded.
    pub max_message_bytes: usize,
    pub max_pending_bytes: usize,
    /// Starts at admission, including queue wait; cooperative cancellation only.
    pub delivery_timeout: Duration,
}
impl Default for WireMirrorOptions {
    fn default() -> Self {
        Self {
            capacity: 256,
            max_message_bytes: 1024 * 1024,
            max_pending_bytes: 8 * 1024 * 1024,
            delivery_timeout: Duration::from_millis(100),
        }
    }
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WireMirrorError {
    #[error("mirror requires a canonical source and distinct local or qualified shadow path of at most 4096 bytes")]
    InvalidPolicy,
    #[error("remote shadow requires a bound remote system and another node identity")]
    RemoteUnavailable,
    #[error("invalid mirror capacity, byte budget or timeout")]
    InvalidOptions,
    #[error("mirror registration requires a Tokio runtime")]
    NoRuntime,
    #[error("mirror registry is closed")]
    Closed,
    #[error("at most 64 registered policies and 64 unfinished workers, including retiring policies, are allowed")]
    RegistryFull,
    #[error("mirror group contains a duplicate or already registered source prefix")]
    Conflict,
}
pub(crate) fn canonical(path: &str) -> bool {
    path.len() <= 4096
        && path.starts_with('/')
        && !path.chars().any(|c| c.is_control() || c == '?' || c == '#')
        && (path == "/"
            || path[1..]
                .split('/')
                .all(|s| !s.is_empty() && s != "." && s != ".."))
}
/// Canonical qualified shadow destination. Never accepts network endpoints,
/// aliases or arbitrary URL schemes; the authenticated node table owns routing.
pub(crate) fn remote_shadow(path: &str) -> Option<(&str, &str)> {
    let qualified = path.strip_prefix("parrot://")?;
    let (node, tail) = qualified.split_once('/')?;
    let local = &path[path.len() - tail.len() - 1..];
    if path.len() > 4096
        || parrot_config::endpoint::validate_node_id(node).is_err()
        || !canonical(local)
        || local == "/"
    {
        return None;
    }
    Some((node, local))
}
fn matches(prefix: &str, path: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|s| s.starts_with('/'))
}
impl WireMirrorPolicy {
    pub fn new(source_prefix: impl Into<String>, shadow_path: impl Into<String>) -> Self {
        Self {
            source_prefix: source_prefix.into(),
            shadow_path: shadow_path.into(),
        }
    }
    pub fn validate(&self) -> Result<(), WireMirrorError> {
        if !canonical(&self.source_prefix)
            || (!canonical(&self.shadow_path) && remote_shadow(&self.shadow_path).is_none())
            || self.shadow_path == "/"
            || self.source_prefix == self.shadow_path
        {
            return Err(WireMirrorError::InvalidPolicy);
        }
        Ok(())
    }
}
impl WireMirrorOptions {
    pub fn validate(self) -> Result<(), WireMirrorError> {
        if self.capacity == 0
            || self.capacity > 65_536
            || self.max_message_bytes == 0
            || self.max_message_bytes > self.max_pending_bytes
            || self.max_pending_bytes > 64 * 1024 * 1024
            || self.delivery_timeout.is_zero()
            || self.delivery_timeout > Duration::from_secs(60)
        {
            return Err(WireMirrorError::InvalidOptions);
        }
        Ok(())
    }
}
/// Concurrent snapshots are approximate. After wait_closed, enqueued equals
/// delivered + failed + timed_out + cancelled. Delivered means mailbox admission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WireMirrorStats {
    pub enqueued: u64,
    pub full: u64,
    /// Local copies rejected by a missing, failing or panicking encoder.
    pub copy_failed: u64,
    pub oversized: u64,
    pub delivered: u64,
    pub failed: u64,
    pub timed_out: u64,
    pub cancelled: u64,
    /// Attempts with at least one isolated callback/future/destructor panic.
    pub panicked_attempts: u64,
}
#[derive(Default)]
struct Counters {
    enqueued: AtomicU64,
    full: AtomicU64,
    copy_failed: AtomicU64,
    oversized: AtomicU64,
    delivered: AtomicU64,
    failed: AtomicU64,
    timed_out: AtomicU64,
    cancelled: AtomicU64,
    panicked_attempts: AtomicU64,
}
impl Counters {
    fn snapshot(&self) -> WireMirrorStats {
        let get = |v: &AtomicU64| v.load(Ordering::Relaxed);
        WireMirrorStats {
            enqueued: get(&self.enqueued),
            full: get(&self.full),
            copy_failed: get(&self.copy_failed),
            oversized: get(&self.oversized),
            delivered: get(&self.delivered),
            failed: get(&self.failed),
            timed_out: get(&self.timed_out),
            cancelled: get(&self.cancelled),
            panicked_attempts: get(&self.panicked_attempts),
        }
    }
}
struct Job {
    type_key: String,
    payload: Vec<u8>,
    context: Option<TraceContext>,
    deadline: tokio::time::Instant,
    _slot: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}
struct Entry {
    policy: WireMirrorPolicy,
    admission: Mutex<Admission>,
    options: WireMirrorOptions,
    sender: mpsc::Sender<Job>,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    cancel: CancellationToken,
    closed: watch::Receiver<bool>,
    done: watch::Sender<bool>,
    counters: Arc<Counters>,
}
// A worker may be destroyed while a synchronous encoder runs on another thread.
// The shared receipt and worker slot retire only after both owners have finished.
struct Admission {
    encoders: usize,
    worker_done: bool,
    permit: Option<OwnedSemaphorePermit>,
}
impl Entry {
    fn finish(&self) {
        let permit = {
            let mut admission = self.admission.lock().unwrap();
            if admission.worker_done && admission.encoders == 0 {
                admission.permit.take()
            } else {
                None
            }
        };
        if let Some(permit) = permit {
            // A waiter owns its waker, which may synchronously reenter close.
            // Notify with no admission lock; retain the worker slot through wake.
            self.done.send_replace(true);
            drop(permit);
        }
    }
}
struct Encoding<'a>(&'a Entry);
impl Drop for Encoding<'_> {
    fn drop(&mut self) {
        let mut gate = self.0.admission.lock().unwrap();
        gate.encoders -= 1;
        drop(gate);
        self.0.finish();
    }
}
/// Dropping a handle does not remove its policy. Call close to stop this exact
/// generation; a replaced handle cannot cancel its successor.
#[derive(Clone)]
pub struct WireMirrorHandle(Arc<Entry>);
impl WireMirrorHandle {
    pub fn close(&self) {
        let _gate = self.0.admission.lock().unwrap();
        self.0.cancel.cancel();
    }
    /// Offer a local message through its registered codec without transferring
    /// primary ownership. Reserve a worst-case message budget before invoking
    /// user code; encoder allocations and execution time cannot be sandboxed.
    /// Unknown or failing codecs affect only the shadow. Reentrant callbacks are
    /// allowed; no registry or admission lock is held while encoding.
    pub fn offer_local(&self, source: &str, message: &parrot_api::types::BoxedMessage) {
        let entry = &self.0;
        if !canonical(source)
            || !matches(&entry.policy.source_prefix, source)
            || source == entry.policy.shadow_path
        {
            return;
        }
        let (_encoding, slot, mut bytes) = {
            let mut gate = entry.admission.lock().unwrap();
            if entry.cancel.is_cancelled() {
                return;
            }
            let permits = entry
                .slots
                .clone()
                .try_acquire_owned()
                .ok()
                .and_then(|slot| {
                    entry
                        .bytes
                        .clone()
                        .try_acquire_many_owned(entry.options.max_message_bytes as u32)
                        .ok()
                        .map(|bytes| (slot, bytes))
                });
            let Some((slot, bytes)) = permits else {
                entry.counters.full.fetch_add(1, Ordering::Relaxed);
                return;
            };
            gate.encoders += 1;
            (Encoding(entry), slot, bytes)
        };
        let deadline = tokio::time::Instant::now() + entry.options.delivery_timeout;
        let cleanup = Cleanup::default();
        let _panic_receipt = PanicReceipt {
            cleanup: &cleanup,
            counters: &entry.counters,
        };
        let encoded = cleanup.call(|| CodecRegistry::global().encode_outgoing(message));
        let (type_key, payload) = match encoded {
            Ok(Ok(encoded)) => encoded,
            _ => {
                entry.counters.copy_failed.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let size = source
            .len()
            .checked_add(type_key.len())
            .and_then(|n| n.checked_add(payload.len()));
        let Some(size) = size.filter(|n| *n <= entry.options.max_message_bytes) else {
            entry.counters.oversized.fetch_add(1, Ordering::Relaxed);
            return;
        };
        drop(bytes.split(entry.options.max_message_bytes - size));
        let _gate = entry.admission.lock().unwrap();
        if entry.cancel.is_cancelled() {
            return;
        }
        let job = Job {
            type_key,
            payload,
            context: TraceContext::current(),
            deadline,
            _slot: slot,
            _bytes: bytes,
        };
        if entry.sender.try_send(job).is_ok() {
            entry.counters.enqueued.fetch_add(1, Ordering::Relaxed);
        } else {
            entry.counters.full.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn stats(&self) -> WireMirrorStats {
        self.0.counters.snapshot()
    }
    pub fn is_closed(&self) -> bool {
        *self.0.closed.borrow()
    }
    /// Waits for all accepted jobs and user futures to retire, not just cancellation.
    pub async fn wait_closed(&self) {
        let mut rx = self.0.closed.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
}
/// Owns an atomically installed set of policies. Drop removes only these exact
/// generations and fences new copies. Await close for actual worker/encoder cleanup.
pub struct WireMirrorGroup {
    state: Weak<Mutex<RegistryState>>,
    enabled: Weak<AtomicBool>,
    handles: Vec<WireMirrorHandle>,
}
impl WireMirrorGroup {
    pub(crate) fn is_finished(&self) -> bool {
        self.handles.iter().all(|handle| *handle.0.closed.borrow())
    }
    pub(crate) fn is_current(&self) -> bool {
        self.state.upgrade().is_some_and(|state| {
            let state = state.lock().unwrap();
            state.open
                && self.handles.iter().all(|handle| {
                    !handle.is_closed()
                        && state
                            .entries
                            .get(&handle.0.policy.source_prefix)
                            .is_some_and(|entry| Arc::ptr_eq(entry, &handle.0))
                })
        })
    }
    pub fn handle_for(&self, source: &str) -> Option<WireMirrorHandle> {
        self.handles
            .iter()
            .filter(|h| matches(&h.0.policy.source_prefix, source))
            .max_by_key(|h| h.0.policy.source_prefix.len())
            .cloned()
    }
    pub fn cancel(&self) {
        if let Some(state) = self.state.upgrade() {
            let mut state = state.lock().unwrap();
            for handle in &self.handles {
                let key = &handle.0.policy.source_prefix;
                if state
                    .entries
                    .get(key)
                    .is_some_and(|entry| Arc::ptr_eq(entry, &handle.0))
                {
                    state.entries.remove(key);
                }
                handle.close();
            }
            if let Some(enabled) = self.enabled.upgrade() {
                enabled.store(!state.entries.is_empty(), Ordering::Release);
            }
        } else {
            for handle in &self.handles {
                handle.close();
            }
        }
    }
    pub async fn close(&self) {
        self.cancel();
        for handle in &self.handles {
            handle.wait_closed().await;
        }
    }
}
impl Drop for WireMirrorGroup {
    fn drop(&mut self) {
        self.cancel();
    }
}
struct RegistryState {
    open: bool,
    entries: BTreeMap<String, Arc<Entry>>,
}
pub(crate) struct WireMirrorRegistry {
    state: Arc<Mutex<RegistryState>>,
    enabled: Arc<AtomicBool>,
    workers: Arc<Semaphore>,
    tasks: TaskTracker,
    local: Weak<dyn LocalLookup>,
    remote: Mutex<Weak<crate::RemoteActorSystem>>,
}
impl WireMirrorRegistry {
    pub(crate) fn new(local: &Arc<dyn LocalLookup>) -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState {
                open: true,
                entries: BTreeMap::new(),
            })),
            enabled: Arc::new(AtomicBool::new(false)),
            workers: Arc::new(Semaphore::new(64)),
            tasks: TaskTracker::new(),
            local: Arc::downgrade(local),
            remote: Mutex::new(Weak::new()),
        }
    }
    pub(crate) fn attach_remote(&self, system: Weak<crate::RemoteActorSystem>) {
        *self.remote.lock().unwrap() = system;
    }
    fn check_target(&self, policy: &WireMirrorPolicy) -> Result<(), WireMirrorError> {
        if let Some((node, _)) = remote_shadow(&policy.shadow_path) {
            let system = self
                .remote
                .lock()
                .unwrap()
                .upgrade()
                .ok_or(WireMirrorError::RemoteUnavailable)?;
            if node == system.config.node_id {
                return Err(WireMirrorError::RemoteUnavailable);
            }
        }
        Ok(())
    }
    pub(crate) fn register(
        &self,
        policy: WireMirrorPolicy,
        options: WireMirrorOptions,
    ) -> Result<WireMirrorHandle, WireMirrorError> {
        policy.validate()?;
        self.check_target(&policy)?;
        options.validate()?;
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| WireMirrorError::NoRuntime)?;
        let mut state = self.state.lock().unwrap();
        if !state.open {
            return Err(WireMirrorError::Closed);
        }
        // Closed handles do not remove their policies. Bound retained registry
        // metadata as well as live/retiring workers.
        if state.entries.len() >= 64 && !state.entries.contains_key(&policy.source_prefix) {
            return Err(WireMirrorError::RegistryFull);
        }
        let permit = self
            .workers
            .clone()
            .try_acquire_owned()
            .map_err(|_| WireMirrorError::RegistryFull)?;
        let entry = self.spawn(policy, options, permit, &runtime);
        if let Some(old) = state
            .entries
            .insert(entry.policy.source_prefix.clone(), entry.clone())
        {
            let _gate = old.admission.lock().unwrap();
            old.cancel.cancel();
        }
        self.enabled.store(true, Ordering::Release);
        Ok(WireMirrorHandle(entry))
    }
    fn spawn(
        &self,
        policy: WireMirrorPolicy,
        options: WireMirrorOptions,
        permit: OwnedSemaphorePermit,
        runtime: &tokio::runtime::Handle,
    ) -> Arc<Entry> {
        let (sender, receiver) = mpsc::channel(options.capacity);
        let (done, closed) = watch::channel(false);
        let entry = Arc::new(Entry {
            policy,
            admission: Mutex::new(Admission {
                encoders: 0,
                worker_done: false,
                permit: Some(permit),
            }),
            options,
            sender,
            slots: Arc::new(Semaphore::new(options.capacity)),
            bytes: Arc::new(Semaphore::new(options.max_pending_bytes)),
            cancel: CancellationToken::new(),
            closed,
            done,
            counters: Arc::new(Counters::default()),
        });
        // Construct the owner outside the async body: an unpolled task still
        // drains its receiver and publishes its retirement receipt on drop.
        let worker = Worker {
            entry: entry.clone(),
            local: self.local.clone(),
            remote: self.remote.lock().unwrap().clone(),
            receiver,
            active: None,
        };
        self.tasks.spawn_on(worker.run(), runtime);
        entry
    }
    /// All-or-nothing ownership, without replacing an existing source policy.
    pub(crate) fn register_group(
        &self,
        policies: Vec<(WireMirrorPolicy, WireMirrorOptions)>,
    ) -> Result<WireMirrorGroup, WireMirrorError> {
        self.replace_group(None, policies)
    }
    /// Replace only a still-current owned group. Validation and reservation
    /// precede commit; a conflict or exhausted budget preserves the old group.
    pub(crate) fn replace_group(
        &self,
        previous: Option<&WireMirrorGroup>,
        policies: Vec<(WireMirrorPolicy, WireMirrorOptions)>,
    ) -> Result<WireMirrorGroup, WireMirrorError> {
        if policies.len() > 64 {
            return Err(WireMirrorError::RegistryFull);
        }
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| WireMirrorError::NoRuntime)?;
        let mut sources = std::collections::BTreeSet::new();
        for (policy, options) in &policies {
            policy.validate()?;
            self.check_target(policy)?;
            options.validate()?;
            if !sources.insert(policy.source_prefix.as_str()) {
                return Err(WireMirrorError::Conflict);
            }
        }
        let mut state = self.state.lock().unwrap();
        if !state.open {
            return Err(WireMirrorError::Closed);
        }
        if let Some(previous) = previous {
            if !Weak::ptr_eq(&previous.state, &Arc::downgrade(&self.state))
                || previous.handles.iter().any(|handle| {
                    handle.is_closed()
                        || !state
                            .entries
                            .get(&handle.0.policy.source_prefix)
                            .is_some_and(|entry| Arc::ptr_eq(entry, &handle.0))
                })
            {
                return Err(WireMirrorError::Conflict);
            }
        }
        let owned = |source: &str| {
            previous.is_some_and(|group| {
                group
                    .handles
                    .iter()
                    .any(|handle| handle.0.policy.source_prefix == source)
            })
        };
        if state.entries.len() - previous.map_or(0, |group| group.handles.len()) + policies.len()
            > 64
        {
            return Err(WireMirrorError::RegistryFull);
        }
        if sources
            .iter()
            .any(|source| state.entries.contains_key(*source) && !owned(source))
        {
            return Err(WireMirrorError::Conflict);
        }
        let mut permits = self
            .workers
            .clone()
            .try_acquire_many_owned(policies.len() as u32)
            .map_err(|_| WireMirrorError::RegistryFull)?;
        let mut handles = Vec::with_capacity(policies.len());
        if let Some(previous) = previous {
            for handle in &previous.handles {
                state.entries.remove(&handle.0.policy.source_prefix);
                handle.close();
            }
        }
        for (policy, options) in policies {
            let entry = self.spawn(
                policy,
                options,
                permits.split(1).expect("reserved worker"),
                &runtime,
            );
            state
                .entries
                .insert(entry.policy.source_prefix.clone(), entry.clone());
            handles.push(WireMirrorHandle(entry));
        }
        self.enabled
            .store(!state.entries.is_empty(), Ordering::Release);
        Ok(WireMirrorGroup {
            state: Arc::downgrade(&self.state),
            enabled: Arc::downgrade(&self.enabled),
            handles,
        })
    }
    pub(crate) fn remove(&self, prefix: &str) -> Option<WireMirrorHandle> {
        let mut state = self.state.lock().unwrap();
        let entry = state.entries.remove(prefix)?;
        {
            let _gate = entry.admission.lock().unwrap();
            entry.cancel.cancel();
        }
        self.enabled
            .store(!state.entries.is_empty(), Ordering::Release);
        Some(WireMirrorHandle(entry))
    }
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }
    pub(crate) fn offer(&self, source: &str, type_key: &str, payload: &[u8]) {
        if !self.enabled.load(Ordering::Acquire) || !canonical(source) {
            return;
        }
        // Fence selection, bounded copy and queue admission with replacement/close.
        let state = self.state.lock().unwrap();
        if !state.open {
            return;
        }
        let Some(entry) = state
            .entries
            .values()
            .filter(|e| matches(&e.policy.source_prefix, source))
            .max_by_key(|e| e.policy.source_prefix.len())
        else {
            return;
        };
        let _gate = entry.admission.lock().unwrap();
        if source == entry.policy.shadow_path || entry.cancel.is_cancelled() {
            return;
        }
        let size = payload
            .len()
            .checked_add(source.len())
            .and_then(|n| n.checked_add(type_key.len()));
        let Some(size) = size.filter(|n| *n <= entry.options.max_message_bytes) else {
            entry.counters.oversized.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let permits = entry
            .slots
            .clone()
            .try_acquire_owned()
            .ok()
            .and_then(|slot| {
                entry
                    .bytes
                    .clone()
                    .try_acquire_many_owned(size as u32)
                    .ok()
                    .map(|bytes| (slot, bytes))
            });
        let Some((slot, bytes)) = permits else {
            entry.counters.full.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let deadline = tokio::time::Instant::now() + entry.options.delivery_timeout;
        // Own only the bounded slice, not a clone retaining a whole batch or
        // a user-defined Bytes owner with a destructor on the primary path.
        let job = Job {
            type_key: type_key.to_owned(),
            payload: payload.to_vec(),
            context: TraceContext::current(),
            deadline,
            _slot: slot,
            _bytes: bytes,
        };
        if entry.sender.try_send(job).is_ok() {
            entry.counters.enqueued.fetch_add(1, Ordering::Relaxed);
        } else {
            entry.counters.full.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.open = false;
        self.enabled.store(false, Ordering::Release);
        for entry in state.entries.values() {
            let _gate = entry.admission.lock().unwrap();
            entry.cancel.cancel();
        }
        state.entries.clear();
        self.tasks.close();
    }
    pub(crate) async fn wait_closed(&self) {
        self.tasks.wait().await;
        // Includes local encoders outliving their worker/runtime.
        let _retired = self.workers.clone().acquire_many_owned(64).await;
    }
}
impl Drop for WireMirrorRegistry {
    fn drop(&mut self) {
        self.close();
    }
}
struct Worker {
    entry: Arc<Entry>,
    local: Weak<dyn LocalLookup>,
    remote: Weak<crate::RemoteActorSystem>,
    receiver: mpsc::Receiver<Job>,
    active: Option<Job>,
}
impl Worker {
    async fn run(mut self) {
        loop {
            self.active = tokio::select! {
                biased;
                _ = self.entry.cancel.cancelled() => break,
                job = self.receiver.recv() => job,
            };
            let Some(job) = self.active.as_ref() else {
                break;
            };
            let cleanup = Cleanup::default();
            let _panic_receipt = PanicReceipt {
                cleanup: &cleanup,
                counters: &self.entry.counters,
            };
            let outcome = parrot_api::trace::scope(job.context.clone(), async {
                // A ready target can otherwise win before Tokio's timer driver
                // observes a deadline expired by synchronous encoding/blocking.
                if self.entry.cancel.is_cancelled() { return Outcome::Cancelled; }
                if tokio::time::Instant::now() >= job.deadline { return Outcome::TimedOut; }
                tokio::select! {
                    biased;
                    _ = self.entry.cancel.cancelled() => Outcome::Cancelled,
                    _ = tokio::time::sleep_until(job.deadline) => Outcome::TimedOut,
                    delivered = deliver(&self.local, &self.remote, &self.entry.policy.shadow_path, job, &cleanup) => if delivered { Outcome::Delivered } else { Outcome::Failed },
                }
            }).await;
            let counter = match outcome {
                Outcome::Cancelled => &self.entry.counters.cancelled,
                Outcome::TimedOut => &self.entry.counters.timed_out,
                Outcome::Delivered => &self.entry.counters.delivered,
                Outcome::Failed => &self.entry.counters.failed,
            };
            counter.fetch_add(1, Ordering::Relaxed);
            self.active.take();
        }
    }
}
enum Outcome {
    Delivered,
    Failed,
    TimedOut,
    Cancelled,
}
struct PanicReceipt<'a> {
    cleanup: &'a Cleanup,
    counters: &'a Counters,
}
impl Drop for PanicReceipt<'_> {
    fn drop(&mut self) {
        if self.cleanup.failed() {
            self.counters
                .panicked_attempts
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let mut gate = self.entry.admission.lock().unwrap();
        self.entry.cancel.cancel();
        self.receiver.close();
        if self.active.take().is_some() {
            self.entry
                .counters
                .cancelled
                .fetch_add(1, Ordering::Relaxed);
        }
        while self.receiver.try_recv().is_ok() {
            self.entry
                .counters
                .cancelled
                .fetch_add(1, Ordering::Relaxed);
        }
        gate.worker_done = true;
        drop(gate);
        self.entry.finish();
    }
}
async fn deliver(
    local: &Weak<dyn LocalLookup>,
    remote: &Weak<crate::RemoteActorSystem>,
    path: &str,
    job: &Job,
    cleanup: &Cleanup,
) -> bool {
    if remote_shadow(path).is_some() {
        let Some(system) = remote.upgrade() else {
            return false;
        };
        let Ok(target) = system.shadow_ref(path) else {
            return false;
        };
        // The reference retains only a weak system. A policy cannot keep its
        // owning node alive, and encoded payloads require no source-side codec.
        drop(system);
        let result = target
            .deliver_shadow_before(&job.type_key, &job.payload, job.deadline)
            .await;
        return match result {
            Ok(()) => true,
            Err(error) => {
                cleanup.dispose(error);
                false
            }
        };
    }
    let Some(local) = local.upgrade() else {
        return false;
    };
    let local = cleanup.owned(local);
    // Local shadows and remote shadow ingress both reject forwarding aliases.
    if !matches!(
        cleanup.call(|| local.value().forwarding_target(path)),
        Ok(None)
    ) {
        return false;
    }
    let target = match unwind::run(cleanup, || local.value().lookup(path)).await {
        Ok(Some(target)) => cleanup.owned(target),
        _ => return false,
    };
    if cleanup.failed() {
        return false;
    }
    let decoded = match cleanup
        .call(|| CodecRegistry::global().decode_incoming(&job.type_key, &job.payload))
    {
        Ok(Ok(message)) => message,
        _ => return false,
    };
    let mut decoded = cleanup.owned(decoded);
    let outcome = unwind::run(cleanup, || target.value().deliver(decoded.take())).await;
    match outcome {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            cleanup.dispose(error);
            false
        }
        Err(()) => false,
    }
}
