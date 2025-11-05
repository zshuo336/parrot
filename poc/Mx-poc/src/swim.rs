//! In-process cluster POC with actual correlated direct/indirect probes.
//! The bus models bounded, lossy datagrams and directional partitions. This is
//! an executable protocol experiment, not the production network implementation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

mod automatic;
pub use automatic::{ClusterBindings, SwimConfig};

const BUS_CAPACITY: usize = 64;
const MAX_PENDING: usize = 128;
const MAX_INDIRECT: usize = 32;
const MAX_MEMBERS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterError {
    AlreadyRegistered,
    NotRunning,
    SequenceExhausted,
    IncarnationExhausted,
    RevisionExhausted,
    ProbeCapacity,
    InvalidTimeout,
    InvalidConfig,
    MembershipCapacity,
    InvalidMember,
    UnavailableMember,
    RuntimeFailed(String),
}
impl std::fmt::Display for ClusterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ClusterError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub node_id: String,
    pub addr: String,
    pub incarnation: u64,
    pub status: MemberStatus,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemberStatus {
    Alive,
    Suspect,
    Dead,
}

#[derive(Debug, Clone)]
pub enum SwimMsg {
    Ping {
        seq: u64,
    },
    PingReq {
        target: String,
        seq: u64,
        timeout: Duration,
    },
    Ack {
        target: String,
        seq: u64,
    },
    Membership(Vec<Member>),
}
#[derive(Debug)]
pub struct SwimEnvelope {
    pub from: String,
    pub message: SwimMsg,
}

#[derive(Default)]
struct BusState {
    queues: HashMap<String, mpsc::Sender<SwimEnvelope>>,
    blocked_nodes: HashSet<String>,
    blocked_links: HashSet<(String, String)>,
}
#[derive(Clone, Default)]
pub struct ClusterBus {
    state: Arc<Mutex<BusState>>,
}
impl ClusterBus {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&self, node: &str) -> Result<BusEndpoint, ClusterError> {
        let mut state = self.state.lock().unwrap();
        if state.queues.get(node).is_some_and(|tx| !tx.is_closed()) {
            return Err(ClusterError::AlreadyRegistered);
        }
        let (tx, rx) = mpsc::channel(BUS_CAPACITY);
        state.queues.insert(node.into(), tx.clone());
        Ok(BusEndpoint {
            sender: BusSender {
                bus: self.clone(),
                node: node.into(),
                identity: tx,
            },
            rx,
        })
    }
    /// Block both directions without removing the live node registration.
    pub fn partition(&self, node: &str) {
        self.state.lock().unwrap().blocked_nodes.insert(node.into());
    }
    pub fn heal(&self, node: &str) {
        self.state.lock().unwrap().blocked_nodes.remove(node);
    }
    pub fn block_link(&self, from: &str, to: &str) {
        self.state
            .lock()
            .unwrap()
            .blocked_links
            .insert((from.into(), to.into()));
    }
    pub fn heal_link(&self, from: &str, to: &str) {
        self.state
            .lock()
            .unwrap()
            .blocked_links
            .remove(&(from.into(), to.into()));
    }
    pub fn registered_count(&self) -> usize {
        self.state.lock().unwrap().queues.len()
    }
}

#[derive(Clone)]
struct BusSender {
    bus: ClusterBus,
    node: String,
    identity: mpsc::Sender<SwimEnvelope>,
}
impl BusSender {
    fn send(&self, to: &str, message: SwimMsg) -> bool {
        let state = self.bus.state.lock().unwrap();
        if !state
            .queues
            .get(&self.node)
            .is_some_and(|tx| tx.same_channel(&self.identity))
            || state.blocked_nodes.contains(&self.node)
            || state.blocked_nodes.contains(to)
            || state
                .blocked_links
                .contains(&(self.node.clone(), to.into()))
        {
            return false;
        }
        state.queues.get(to).is_some_and(|tx| {
            tx.try_send(SwimEnvelope {
                from: self.node.clone(),
                message,
            })
            .is_ok()
        })
    }
}

/// The receiver owns registration. Old senders cannot impersonate a new
/// registration with the same name, and no unbounded packet queue is used.
pub struct BusEndpoint {
    sender: BusSender,
    rx: mpsc::Receiver<SwimEnvelope>,
}
impl BusEndpoint {
    pub async fn recv(&mut self) -> Option<SwimEnvelope> {
        self.rx.recv().await
    }
    pub fn send(&self, to: &str, message: SwimMsg) -> bool {
        self.sender.send(to, message)
    }
}
impl Drop for BusEndpoint {
    fn drop(&mut self) {
        self.rx.close();
        let mut state = self.sender.bus.state.lock().unwrap();
        if state
            .queues
            .get(&self.sender.node)
            .is_some_and(|tx| tx.same_channel(&self.sender.identity))
        {
            state.queues.remove(&self.sender.node);
        }
    }
}

struct PendingProbe {
    target: String,
    helpers: HashSet<String>,
    tx: oneshot::Sender<()>,
}
#[derive(Default)]
struct ProbeTable {
    closed: bool,
    error: Option<ClusterError>,
    pending: HashMap<u64, PendingProbe>,
}
struct ProbeState {
    sender: BusSender,
    table: Mutex<ProbeTable>,
}
impl ProbeState {
    fn close(&self) {
        let mut table = self.table.lock().unwrap();
        table.closed = true;
        table.pending.clear();
    }
    fn complete(&self, from: &str, target: &str, seq: u64) {
        let mut table = self.table.lock().unwrap();
        let accepted = table
            .pending
            .get(&seq)
            .is_some_and(|p| p.target == target && (from == target || p.helpers.contains(from)));
        if accepted {
            if let Some(p) = table.pending.remove(&seq) {
                let _ = p.tx.send(());
            }
        }
    }
}
struct PendingGuard {
    state: Arc<ProbeState>,
    seq: u64,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.state.table.lock().unwrap().pending.remove(&self.seq);
    }
}

pub struct SwimNode {
    pub node_id: String,
    members: Mutex<BTreeMap<String, Member>>,
    sequence: AtomicU64,
    runtime: Mutex<Weak<ProbeState>>,
    changed: tokio::sync::Notify,
}
impl SwimNode {
    pub fn new(node_id: &str) -> Arc<Self> {
        Arc::new(Self {
            node_id: node_id.into(),
            members: Mutex::new(BTreeMap::from([(
                node_id.into(),
                Member {
                    node_id: node_id.into(),
                    addr: node_id.into(),
                    incarnation: 0,
                    status: MemberStatus::Alive,
                },
            )])),
            sequence: AtomicU64::new(0),
            runtime: Mutex::new(Weak::new()),
            changed: tokio::sync::Notify::new(),
        })
    }
    pub async fn members(&self) -> Vec<Member> {
        self.members.lock().unwrap().values().cloned().collect()
    }

    /// Higher incarnation wins; at the same incarnation Dead > Suspect > Alive.
    /// Self accusations are refuted above the observed incarnation, preserving
    /// our own address. Exhaustion is explicit and leaves the table unchanged.
    pub async fn merge_membership(&self, incoming: Vec<Member>) -> Result<(), ClusterError> {
        if incoming.len() > MAX_MEMBERS {
            return Err(ClusterError::MembershipCapacity);
        }
        if incoming
            .iter()
            .any(|m| m.node_id.is_empty() || m.node_id.len() > 1024 || m.addr.len() > 4096)
        {
            return Err(ClusterError::InvalidMember);
        }
        let mut members = self.members.lock().unwrap();
        let mut updated = members.clone();
        for inc in incoming {
            if inc.node_id == self.node_id {
                let me = updated.get_mut(&self.node_id).unwrap();
                if inc.incarnation >= me.incarnation {
                    me.incarnation = if inc.status == MemberStatus::Alive {
                        inc.incarnation
                    } else {
                        inc.incarnation
                            .checked_add(1)
                            .ok_or(ClusterError::IncarnationExhausted)?
                    };
                    me.status = MemberStatus::Alive;
                }
            } else {
                let entry = updated
                    .entry(inc.node_id.clone())
                    .or_insert_with(|| inc.clone());
                if (inc.incarnation, inc.status) > (entry.incarnation, entry.status) {
                    *entry = inc;
                }
            }
        }
        if updated.len() > MAX_MEMBERS {
            return Err(ClusterError::MembershipCapacity);
        }
        if *members != updated {
            *members = updated;
            self.changed.notify_one();
        }
        Ok(())
    }
    pub async fn refute(&self) -> Result<u64, ClusterError> {
        let mut members = self.members.lock().unwrap();
        let me = members.get_mut(&self.node_id).unwrap();
        me.incarnation = me
            .incarnation
            .checked_add(1)
            .ok_or(ClusterError::IncarnationExhausted)?;
        me.status = MemberStatus::Alive;
        self.changed.notify_one();
        Ok(me.incarnation)
    }
    /// The observation's incarnation is required: a stale timeout cannot
    /// suspect a newer incarnation, and Suspect cannot downgrade Dead.
    pub async fn mark_suspect(&self, node: &str, incarnation: u64) -> bool {
        let mut members = self.members.lock().unwrap();
        if node == self.node_id {
            return false;
        }
        if let Some(entry) = members.get_mut(node) {
            if entry.incarnation == incarnation && entry.status == MemberStatus::Alive {
                entry.status = MemberStatus::Suspect;
                self.changed.notify_one();
                return true;
            }
        }
        false
    }
    pub async fn mark_dead(&self, node: &str, incarnation: u64) -> bool {
        let mut members = self.members.lock().unwrap();
        if node == self.node_id {
            return false;
        }
        if let Some(entry) = members.get_mut(node) {
            if entry.incarnation == incarnation && entry.status == MemberStatus::Suspect {
                entry.status = MemberStatus::Dead;
                self.changed.notify_one();
                return true;
            }
        }
        false
    }
    pub fn next_seq(&self) -> Result<u64, ClusterError> {
        self.sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map(|previous| previous + 1)
            .map_err(|_| ClusterError::SequenceExhausted)
    }
    pub fn pending_count(&self) -> usize {
        self.runtime
            .lock()
            .unwrap()
            .upgrade()
            .map_or(0, |state| state.table.lock().unwrap().pending.len())
    }

    pub async fn gossip(&self, bus: &ClusterBus, target: &str) -> Result<bool, ClusterError> {
        let state = self
            .runtime
            .lock()
            .unwrap()
            .upgrade()
            .ok_or(ClusterError::NotRunning)?;
        if !Arc::ptr_eq(&bus.state, &state.sender.bus.state) || state.table.lock().unwrap().closed {
            return Err(ClusterError::NotRunning);
        }
        Ok(state
            .sender
            .send(target, SwimMsg::Membership(self.members().await)))
    }

    /// Start owned periodic probes, full-state gossip and suspicion expiry.
    pub fn start(self: &Arc<Self>, bus: &ClusterBus) -> Result<SwimRuntime, ClusterError> {
        self.start_with(bus, SwimConfig::default(), ClusterBindings::default())
    }

    pub fn start_with(
        self: &Arc<Self>,
        bus: &ClusterBus,
        config: SwimConfig,
        bindings: ClusterBindings,
    ) -> Result<SwimRuntime, ClusterError> {
        config.validate()?;
        self.start_inner(bus, Some((config, bindings)))
    }

    /// Low-level receive/probe endpoint for explicitly driven protocol tests.
    pub fn start_manual(self: &Arc<Self>, bus: &ClusterBus) -> Result<SwimRuntime, ClusterError> {
        self.start_inner(bus, None)
    }

    fn start_inner(
        self: &Arc<Self>,
        bus: &ClusterBus,
        automatic: Option<(SwimConfig, ClusterBindings)>,
    ) -> Result<SwimRuntime, ClusterError> {
        if self.node_id.is_empty() || self.node_id.len() > 1024 {
            return Err(ClusterError::InvalidMember);
        }
        let mut runtime = self.runtime.lock().unwrap();
        if runtime
            .upgrade()
            .is_some_and(|s| !s.table.lock().unwrap().closed)
        {
            return Err(ClusterError::AlreadyRegistered);
        }
        let mut endpoint = bus.register(&self.node_id)?;
        let state = Arc::new(ProbeState {
            sender: endpoint.sender.clone(),
            table: Mutex::new(ProbeTable::default()),
        });
        *runtime = Arc::downgrade(&state);
        let state_c = state.clone();
        let node = self.clone();
        let (shutdown, mut stopping) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut helpers = tokio::task::JoinSet::new();
            let automatic = async {
                match automatic {
                    Some((config, bindings)) => {
                        automatic::run(node.clone(), state_c.clone(), config, bindings).await
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(automatic);
            let result = loop {
                tokio::select! {
                    biased;
                    _ = &mut stopping => break Ok(()),
                    result = &mut automatic => break result,
                    completed = helpers.join_next(), if !helpers.is_empty() => { let _ = completed; },
                    envelope = endpoint.recv() => {
                        let Some(SwimEnvelope { from, message }) = envelope else { break Ok(()); };
                        match message {
                            SwimMsg::Ping { seq } => {
                                state_c.sender.send(&from, SwimMsg::Ack { target: node.node_id.clone(), seq });
                            }
                            SwimMsg::Ack { target, seq } => state_c.complete(&from, &target, seq),
                            SwimMsg::Membership(members) => { let _ = node.merge_membership(members).await; },
                            SwimMsg::PingReq { target, seq, timeout } => {
                                if helpers.len() >= MAX_INDIRECT || target == node.node_id || target == from { continue; }
                                let n = node.clone(); let state = state_c.clone();
                                helpers.spawn(async move {
                                    // This helper uses its OWN correlation ID and only forwards
                                    // success after the target replies on its own receive path.
                                    if n.probe_on(state.clone(), &target, &[], timeout).await
                                        .is_ok_and(|outcome| outcome.reachable) {
                                        state.sender.send(&from, SwimMsg::Ack { target, seq });
                                    }
                                });
                            }
                        }
                    }
                }
            };
            state_c.table.lock().unwrap().error = result.err();
            state_c.close();
            helpers.shutdown().await;
        });
        Ok(SwimRuntime {
            state,
            task: Some(task),
            shutdown: Some(shutdown),
        })
    }

    pub async fn probe(
        &self,
        bus: &ClusterBus,
        target: &str,
        via: &[String],
        timeout: Duration,
    ) -> Result<ProbeOutcome, ClusterError> {
        let state = self
            .runtime
            .lock()
            .unwrap()
            .upgrade()
            .ok_or(ClusterError::NotRunning)?;
        if !Arc::ptr_eq(&bus.state, &state.sender.bus.state) {
            return Err(ClusterError::NotRunning);
        }
        self.probe_on(state, target, via, timeout).await
    }
    async fn probe_on(
        &self,
        state: Arc<ProbeState>,
        target: &str,
        via: &[String],
        timeout: Duration,
    ) -> Result<ProbeOutcome, ClusterError> {
        if timeout.is_zero() || tokio::time::Instant::now().checked_add(timeout).is_none() {
            return Err(ClusterError::InvalidTimeout);
        }
        let seq = self.next_seq()?;
        let (tx, mut rx) = oneshot::channel();
        {
            let mut table = state.table.lock().unwrap();
            if table.closed {
                return Err(ClusterError::NotRunning);
            }
            if table.pending.len() >= MAX_PENDING {
                return Err(ClusterError::ProbeCapacity);
            }
            table.pending.insert(
                seq,
                PendingProbe {
                    target: target.into(),
                    helpers: HashSet::new(),
                    tx,
                },
            );
        }
        let _guard = PendingGuard {
            state: state.clone(),
            seq,
        };
        state.sender.send(target, SwimMsg::Ping { seq });
        match tokio::time::timeout(timeout, &mut rx).await {
            Ok(Ok(())) => {
                return Ok(ProbeOutcome {
                    target: target.into(),
                    reachable: true,
                    used_indirect: false,
                })
            }
            Ok(Err(_)) => return Err(ClusterError::NotRunning),
            Err(_) => {}
        }
        let mut helpers = BTreeSet::new();
        for id in via {
            if id != target && id != &self.node_id {
                helpers.insert(id.clone());
            }
            if helpers.len() == MAX_INDIRECT {
                break;
            }
        }
        if helpers.is_empty() {
            return Ok(ProbeOutcome {
                target: target.into(),
                reachable: false,
                used_indirect: false,
            });
        }
        {
            let mut table = state.table.lock().unwrap();
            if table.closed {
                return Err(ClusterError::NotRunning);
            }
            if let Some(pending) = table.pending.get_mut(&seq) {
                pending.helpers.extend(helpers.iter().cloned());
            }
        }
        for helper in &helpers {
            state.sender.send(
                helper,
                SwimMsg::PingReq {
                    target: target.into(),
                    seq,
                    timeout,
                },
            );
        }
        let reachable = match tokio::time::timeout(timeout, &mut rx).await {
            Ok(Ok(())) => true,
            Ok(Err(_)) => return Err(ClusterError::NotRunning),
            Err(_) => false,
        };
        Ok(ProbeOutcome {
            target: target.into(),
            reachable,
            used_indirect: true,
        })
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct ProbeOutcome {
    pub target: String,
    pub reachable: bool,
    pub used_indirect: bool,
}

/// Owns the ingress and all helper tasks. Drop cancels; shutdown also joins.
pub struct SwimRuntime {
    state: Arc<ProbeState>,
    task: Option<tokio::task::JoinHandle<()>>,
    shutdown: Option<oneshot::Sender<()>>,
}
impl SwimRuntime {
    /// Reports terminal scheduler/configuration failures instead of silently
    /// retaining an apparently live control plane.
    pub fn status(&self) -> Result<bool, ClusterError> {
        let state = self.state.table.lock().unwrap();
        if !state.closed && self.task.as_ref().is_some_and(|task| task.is_finished()) {
            return Err(ClusterError::RuntimeFailed(
                "control task ended without orderly retirement".into(),
            ));
        }
        match &state.error {
            Some(error) => Err(error.clone()),
            None => Ok(!state.closed),
        }
    }
    pub async fn shutdown(mut self) -> Result<(), ClusterError> {
        self.state.close();
        self.shutdown.take();
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|e| ClusterError::RuntimeFailed(e.to_string()))?;
        }
        self.status().map(|_| ())
    }
}
impl Drop for SwimRuntime {
    fn drop(&mut self) {
        self.state.close();
        // EOF requests the same scoped cleanup, including joining helper tasks.
        // Drop cannot await, but the receive loop has no user handlers to stall it.
        self.shutdown.take();
    }
}

// Service discovery publishes the current snapshot, not an unbounded event log.
// watch coalesces intermediate versions for slow consumers without losing the
// latest state. Mutation, revision assignment and subscription share one lock.
pub type ServiceEntries = Vec<(String, String)>;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSnapshot {
    pub revision: u64,
    pub entries: ServiceEntries,
}
struct ServiceKey {
    entries: BTreeSet<(String, String)>,
    snapshots: watch::Sender<ServiceSnapshot>,
}
#[derive(Default)]
struct DiscoveryState {
    revision: u64,
    keys: HashMap<String, ServiceKey>,
    unavailable: HashSet<String>,
}
pub struct Receptionist {
    state: Mutex<DiscoveryState>,
}
impl Receptionist {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(DiscoveryState::default()),
        })
    }
    pub async fn register(
        &self,
        key: &str,
        node: &str,
        address: &str,
    ) -> Result<bool, ClusterError> {
        self.update(key, (node.into(), address.into()), true)
    }
    pub async fn unregister(
        &self,
        key: &str,
        node: &str,
        address: &str,
    ) -> Result<bool, ClusterError> {
        self.update(key, (node.into(), address.into()), false)
    }
    fn update(
        &self,
        key: &str,
        entry: (String, String),
        insert: bool,
    ) -> Result<bool, ClusterError> {
        let mut state = self.state.lock().unwrap();
        if insert && state.unavailable.contains(&entry.0) {
            return Err(ClusterError::UnavailableMember);
        }
        let contains = state
            .keys
            .get(key)
            .is_some_and(|k| k.entries.contains(&entry));
        if contains == insert {
            return Ok(false);
        }
        let revision = state
            .revision
            .checked_add(1)
            .ok_or(ClusterError::RevisionExhausted)?;
        state.revision = revision;
        let service = state
            .keys
            .entry(key.into())
            .or_insert_with(|| new_service_key(revision));
        if insert {
            service.entries.insert(entry);
        } else {
            service.entries.remove(&entry);
        }
        publish(service, revision);
        prune_key(&mut state, key);
        Ok(true)
    }
    /// Apply one local membership view under the same lock as registration and
    /// publication. A concurrent late registration cannot resurrect a dead peer.
    fn apply_membership(&self, members: &[Member]) -> Result<(), ClusterError> {
        let dead: HashSet<_> = members
            .iter()
            .filter(|m| m.status == MemberStatus::Dead)
            .map(|m| m.node_id.clone())
            .collect();
        let mut state = self.state.lock().unwrap();
        let removed = state
            .keys
            .values()
            .any(|k| k.entries.iter().any(|(node, _)| dead.contains(node)));
        let revision = if removed {
            state
                .revision
                .checked_add(1)
                .ok_or(ClusterError::RevisionExhausted)?
        } else {
            state.revision
        };
        state.unavailable = dead;
        state.revision = revision;
        if removed {
            let dead = state.unavailable.clone();
            for key in state.keys.values_mut() {
                let before = key.entries.len();
                key.entries.retain(|(node, _)| !dead.contains(node));
                if before != key.entries.len() {
                    publish(key, revision);
                }
            }
            state
                .keys
                .retain(|_, k| !k.entries.is_empty() || k.snapshots.receiver_count() != 0);
        }
        Ok(())
    }
    pub async fn remove_node(&self, node: &str) -> Result<usize, ClusterError> {
        let mut state = self.state.lock().unwrap();
        let count = state
            .keys
            .values()
            .map(|k| k.entries.iter().filter(|(n, _)| n == node).count())
            .sum();
        if count == 0 {
            return Ok(0);
        }
        let revision = state
            .revision
            .checked_add(1)
            .ok_or(ClusterError::RevisionExhausted)?;
        state.revision = revision;
        for key in state.keys.values_mut() {
            let before = key.entries.len();
            key.entries.retain(|(n, _)| n != node);
            if before != key.entries.len() {
                publish(key, revision);
            }
        }
        state
            .keys
            .retain(|_, k| !k.entries.is_empty() || k.snapshots.receiver_count() != 0);
        Ok(count)
    }
    pub async fn subscribe(self: &Arc<Self>, key: &str) -> ServiceSubscription {
        let mut state = self.state.lock().unwrap();
        let revision = state.revision;
        let service = state
            .keys
            .entry(key.into())
            .or_insert_with(|| new_service_key(revision));
        ServiceSubscription {
            receiver: Some(service.snapshots.subscribe()),
            last_revision: None,
            key: key.into(),
            owner: Arc::downgrade(self),
        }
    }
    pub fn subscriber_count(&self, key: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .keys
            .get(key)
            .map_or(0, |s| s.snapshots.receiver_count())
    }
    pub fn retained_key_count(&self) -> usize {
        self.state.lock().unwrap().keys.len()
    }
}
fn new_service_key(revision: u64) -> ServiceKey {
    let (snapshots, _) = watch::channel(ServiceSnapshot {
        revision,
        entries: vec![],
    });
    ServiceKey {
        entries: BTreeSet::new(),
        snapshots,
    }
}
fn publish(service: &ServiceKey, revision: u64) {
    service.snapshots.send_replace(ServiceSnapshot {
        revision,
        entries: service.entries.iter().cloned().collect(),
    });
}
fn prune_key(state: &mut DiscoveryState, key: &str) {
    if state
        .keys
        .get(key)
        .is_some_and(|s| s.entries.is_empty() && s.snapshots.receiver_count() == 0)
    {
        state.keys.remove(key);
    }
}
pub struct ServiceSubscription {
    receiver: Option<watch::Receiver<ServiceSnapshot>>,
    last_revision: Option<u64>,
    key: String,
    owner: Weak<Receptionist>,
}
impl ServiceSubscription {
    pub async fn recv(&mut self) -> Option<ServiceSnapshot> {
        let receiver = self.receiver.as_mut()?;
        if self.last_revision.is_some() && receiver.changed().await.is_err() {
            return None;
        }
        let snapshot = receiver.borrow_and_update().clone();
        self.last_revision = Some(snapshot.revision);
        Some(snapshot)
    }
    pub fn try_recv(&mut self) -> Result<ServiceSnapshot, mpsc::error::TryRecvError> {
        let receiver = self
            .receiver
            .as_mut()
            .ok_or(mpsc::error::TryRecvError::Disconnected)?;
        if self.last_revision.is_some() {
            match receiver.has_changed() {
                Ok(true) => {}
                Ok(false) => return Err(mpsc::error::TryRecvError::Empty),
                Err(_) => {
                    if self.last_revision == Some(receiver.borrow().revision) {
                        return Err(mpsc::error::TryRecvError::Disconnected);
                    }
                }
            }
        }
        let snapshot = receiver.borrow_and_update().clone();
        self.last_revision = Some(snapshot.revision);
        Ok(snapshot)
    }
}
impl Drop for ServiceSubscription {
    fn drop(&mut self) {
        self.receiver.take();
        if let Some(owner) = self.owner.upgrade() {
            prune_key(&mut owner.state.lock().unwrap(), &self.key);
        }
    }
}

#[cfg(test)]
mod automatic_failure_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn sequence_exhaustion_retires_the_runtime_and_reports_failure() {
        let bus = ClusterBus::new();
        let node = SwimNode::new("A");
        node.merge_membership(vec![Member {
            node_id: "B".into(),
            addr: "B".into(),
            incarnation: 0,
            status: MemberStatus::Alive,
        }])
        .await
        .unwrap();
        node.sequence.store(u64::MAX, Ordering::Relaxed);
        let runtime = node.start(&bus).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(runtime.status(), Err(ClusterError::SequenceExhausted));
        assert_eq!(bus.registered_count(), 0);
        assert_eq!(node.pending_count(), 0);
        assert_eq!(
            runtime.shutdown().await,
            Err(ClusterError::SequenceExhausted)
        );
    }
}
