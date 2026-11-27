//! Receiver-owned challenges, fenced handoff and bounded ordered TELL workers.
use super::{
    unwind, Cleanup, Ingress, ReorderPending, DEAD_TELL_DROPPED, REORDER_BUFFERED,
    REORDER_GAP_FLUSHED,
};
use crate::{
    error::ErrCode,
    frame::Frame,
    ordered::{self, Control, Data, Identity, Operation, Receipt},
    transport::FrameSender,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{atomic::Ordering, Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ready {
    Preparing,
    Active,
    Failed(ErrCode),
}

struct Pending {
    sequence: u64,
    frame: ReorderPending,
}

struct Lease {
    receiver: Identity,
    epoch: Option<u64>,
    _owner: Option<Arc<ordered::recovery::ReceiverOwner>>,
    source: String,
    incarnation: Identity,
    stream: Identity,
    ticket: Identity,
    generation: u64,
    tx: mpsc::Sender<Pending>,
    cancel: CancellationToken,
    deadline: Mutex<Instant>,
    ready: watch::Sender<Ready>,
    done: watch::Sender<bool>,
}
impl Lease {
    fn matches(&self, control: &Control) -> bool {
        self.incarnation == control.incarnation
            && self.stream == control.stream
            && self.ticket == control.ticket
    }
    fn receipt(&self, active: bool) -> Receipt {
        Receipt {
            active,
            receiver: self.receiver,
            epoch: self.epoch,
            ticket: self.ticket,
            generation: self.generation,
            lease_ms: ordered::LEASE_MS,
        }
    }
    async fn active(&self) -> Result<(), ErrCode> {
        let mut receiver = self.ready.subscribe();
        loop {
            let ready = *receiver.borrow_and_update();
            match ready {
                Ready::Active => return Ok(()),
                Ready::Failed(code) => return Err(code),
                Ready::Preparing => {}
            }
            if receiver.changed().await.is_err() {
                return Err(ErrCode::Stopped);
            }
        }
    }
    async fn finished(&self) {
        let mut receiver = self.done.subscribe();
        let _ = receiver.wait_for(|done| *done).await;
    }
    fn is_finished(&self) -> bool {
        *self.done.borrow()
    }
}

struct Challenge {
    control: Control,
    generation: u64,
    expires: Instant,
}

struct Closing {
    lease: Arc<Lease>,
    previous: Option<Arc<Lease>>,
}
impl Closing {
    fn is_finished(&self) -> bool {
        self.lease.is_finished()
            && self
                .previous
                .as_ref()
                .is_none_or(|lease| lease.is_finished())
    }
}

#[derive(Default)]
struct Source {
    generation: u64,
    challenge: Option<Challenge>,
    active: Option<Arc<Lease>>,
    retiring: Option<Arc<Lease>>,
    closing: Option<Closing>,
}

pub(super) struct Sessions {
    receiver: Identity,
    owner: Option<Arc<ordered::recovery::ReceiverOwner>>,
    sources: HashMap<String, Source>,
    capacity: usize,
}
impl Sessions {
    pub fn new(capacity: usize) -> Self {
        Self {
            receiver: ordered::identity(),
            owner: None,
            sources: HashMap::new(),
            capacity,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
    }
    fn prune_challenges(&mut self) {
        let now = Instant::now();
        for source in self.sources.values_mut() {
            if source.closing.as_ref().is_some_and(Closing::is_finished) {
                source.closing = None;
            }
            if source
                .challenge
                .as_ref()
                .is_some_and(|challenge| challenge.expires <= now)
            {
                source.challenge = None;
            }
        }
        self.sources.retain(|_, source| {
            source.challenge.is_some()
                || source.active.is_some()
                || source.retiring.is_some()
                || source.closing.is_some()
        });
    }
}
impl Drop for Sessions {
    fn drop(&mut self) {
        for source in self.sources.values() {
            for lease in source.active.iter().chain(source.retiring.iter()).chain(
                source.closing.iter().flat_map(|closing| {
                    std::iter::once(&closing.lease).chain(closing.previous.iter())
                }),
            ) {
                let _ = Cleanup::default().call(|| lease.cancel.cancel());
            }
        }
    }
}

struct Completion(Arc<Lease>);
impl Drop for Completion {
    fn drop(&mut self) {
        if *self.0.ready.borrow() == Ready::Preparing {
            let _ = Cleanup::default()
                .call(|| self.0.ready.send_replace(Ready::Failed(ErrCode::Stopped)));
        }
        let _ = Cleanup::default().call(|| self.0.done.send_replace(true));
    }
}

struct Registration {
    ingress: Weak<Ingress>,
    lease: Arc<Lease>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let Some(ingress) = self.ingress.upgrade() else {
            return;
        };
        let mut sessions = ingress.ordered.lock().unwrap();
        let Some(source) = sessions.sources.get_mut(&self.lease.source) else {
            return;
        };
        if source
            .active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.lease))
        {
            source.active = None;
        }
        if source
            .retiring
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.lease))
        {
            source.retiring = None;
        }
        if source.active.is_none()
            && source.retiring.is_none()
            && source.challenge.is_none()
            && source.closing.is_none()
        {
            sessions.sources.remove(&self.lease.source);
        }
    }
}

impl Ingress {
    fn ordered_local_path(&self, path: &str) -> Result<(), ErrCode> {
        let forwarded = Cleanup::default()
            .call(|| {
                self.local
                    .forwarding_target(super::local_path(path))
                    .is_some_and(|target| self.is_remote_target(&target))
            })
            .map_err(|_| ErrCode::ProtocolViolation)?;
        if forwarded {
            Err(ErrCode::NotRemotable)
        } else {
            Ok(())
        }
    }

    pub(crate) fn set_ordered_receiver_owner(&self, owner: Arc<ordered::recovery::ReceiverOwner>) {
        let mut sessions = self.ordered.lock().unwrap();
        assert!(sessions.sources.is_empty() && sessions.owner.is_none());
        sessions.receiver = owner.receiver();
        sessions.owner = Some(owner);
    }

    /// Grant only authenticated relays permission to preserve an ordered origin.
    pub fn set_ordered_relays(
        &self,
        nodes: std::collections::BTreeSet<String>,
    ) -> Result<(), String> {
        if nodes.len() > 4096 {
            return Err("too many ordered relay grants".into());
        }
        for node in &nodes {
            parrot_config::endpoint::validate_node_id(node)?;
        }
        *self.ordered_relays.write().unwrap() = nodes;
        Ok(())
    }

    fn ordered_origin(
        &self,
        source: &str,
        frame: &Frame,
        back: &FrameSender,
    ) -> Result<(), ErrCode> {
        if !self.control_policy.allows_cluster(back) {
            return Err(ErrCode::Forbidden);
        }
        if frame.header.hop_count == 0 {
            if back.node_id() == source {
                Ok(())
            } else {
                Err(ErrCode::Forbidden)
            }
        } else if matches!(
            self.control_policy,
            crate::security::ControlPolicy::InsecureDevelopment
        ) || self.ordered_relays.read().unwrap().contains(back.node_id())
        {
            Ok(())
        } else {
            // An older route policy has not opted into preserving third-party
            // origins. A source may choose legacy only before sending a session.
            Err(ErrCode::NotRemotable)
        }
    }

    pub(super) fn close_ordered(&self) {
        let (retired, _owner) = {
            let mut sessions = self.ordered.lock().unwrap();
            (std::mem::take(&mut sessions.sources), sessions.owner.take())
        };
        for source in retired.into_values() {
            for lease in source.active.into_iter().chain(source.retiring).chain(
                source
                    .closing
                    .into_iter()
                    .flat_map(|closing| std::iter::once(closing.lease).chain(closing.previous)),
            ) {
                let _ = Cleanup::default().call(|| lease.cancel.cancel());
            }
        }
    }

    pub(super) async fn on_ordered_control(self: &Arc<Self>, frame: Frame, back: &FrameSender) {
        let result = async {
            let (_, payload) = frame
                .split_reply_to()
                .map_err(|_| ErrCode::ProtocolViolation)?;
            let control = Control::decode(&payload).map_err(|_| ErrCode::ProtocolViolation)?;
            self.ordered_origin(&control.source, &frame, back)?;
            if control.operation == Operation::OpenAdmission && !back.supports_ordered_admission() {
                return Err(ErrCode::NotRemotable);
            }
            if self
                .ordered
                .lock()
                .unwrap()
                .owner
                .as_ref()
                .is_some_and(|owner| !owner.valid_process())
            {
                return Err(ErrCode::Stopped);
            }
            if matches!(control.operation, Operation::Recover { .. })
                && !back.supports_ordered_recovery()
            {
                return Err(ErrCode::NotRemotable);
            }
            if self.is_remote_target(&frame.path) {
                self.try_relay_request(&frame, back, frame.header.correlation_id)
                    .await;
                return Ok(None);
            }
            let receiver = self
                .relay
                .read()
                .unwrap()
                .as_ref()
                .map(|router| router.self_node().to_owned());
            if crate::actor_path::split(&frame.path).map(|pair| pair.0) != receiver.as_deref() {
                return Err(ErrCode::ProtocolViolation);
            }
            if matches!(
                control.operation,
                Operation::Open | Operation::OpenAdmission
            ) {
                self.ordered_local_path(&frame.path)?;
            }
            match control.operation {
                Operation::Open | Operation::OpenAdmission => {
                    self.open_ordered(control).await.map(Some)
                }
                Operation::Commit => self.commit_ordered(control).await.map(Some),
                Operation::Close => self.close_ordered_lease(control).await.map(Some),
                Operation::Recover { receiver, epoch } => {
                    let sessions = self.ordered.lock().unwrap();
                    if self.reorder_closed.load(Ordering::Acquire) {
                        return Err(ErrCode::Stopped);
                    }
                    let owner = sessions.owner.as_ref().ok_or(ErrCode::NotRemotable)?;
                    if !owner.proves_retired(receiver, epoch) {
                        return Err(ErrCode::Forbidden);
                    }
                    Ok(Some(Receipt {
                        active: false,
                        receiver,
                        ticket: control.ticket,
                        generation: 1,
                        lease_ms: ordered::LEASE_MS,
                        epoch: Some(epoch),
                    }))
                }
            }
        }
        .await;
        let response = match result {
            Ok(Some(mut receipt)) => {
                if !back.supports_ordered_recovery() {
                    receipt.epoch = None;
                }
                Frame::reply(
                    frame.header.correlation_id,
                    "",
                    ordered::RECEIPT_KEY,
                    receipt.encode(),
                )
            }
            Ok(None) => return,
            Err(code) => Frame::reply_err(
                frame.header.correlation_id,
                &frame.path,
                code,
                "ordered session control rejected",
            ),
        };
        let _ = back.send(response).await;
    }

    async fn open_ordered(&self, mut control: Control) -> Result<Receipt, ErrCode> {
        let existing = {
            let mut sessions = self.ordered.lock().unwrap();
            if self.reorder_closed.load(Ordering::Acquire) {
                return Err(ErrCode::Stopped);
            }
            sessions.prune_challenges();
            let receiver = sessions.receiver;
            let epoch = sessions.owner.as_ref().map(|owner| owner.epoch());
            if !sessions.sources.contains_key(&control.source)
                && sessions.sources.len() >= sessions.capacity
            {
                return Err(ErrCode::Overloaded);
            }
            let source = sessions.sources.entry(control.source.clone()).or_default();
            if source.closing.is_some() {
                return Err(ErrCode::Overloaded);
            }
            if source.active.as_ref().is_some_and(|lease| {
                *lease.ready.borrow() == Ready::Preparing
                    && (lease.incarnation != control.incarnation || lease.stream != control.stream)
            }) {
                return Err(ErrCode::Overloaded);
            }
            if let Some(active) = source.active.as_ref().filter(|lease| {
                lease.incarnation == control.incarnation
                    && lease.stream == control.stream
                    && *lease.deadline.lock().unwrap() > Instant::now()
            }) {
                *active.deadline.lock().unwrap() =
                    Instant::now() + Duration::from_millis(ordered::LEASE_MS as u64);
                Some(active.clone())
            } else {
                if let Some(challenge) = source.challenge.as_ref().filter(|challenge| {
                    challenge.control.incarnation == control.incarnation
                        && challenge.control.stream == control.stream
                }) {
                    return Ok(Receipt {
                        active: false,
                        receiver,
                        epoch,
                        ticket: challenge.control.ticket,
                        generation: challenge.generation,
                        lease_ms: ordered::LEASE_MS,
                    });
                }
                source.generation = source
                    .generation
                    .checked_add(1)
                    .ok_or(ErrCode::Overloaded)?;
                control.ticket = ordered::identity();
                let receipt = Receipt {
                    active: false,
                    receiver,
                    epoch,
                    ticket: control.ticket,
                    generation: source.generation,
                    lease_ms: ordered::LEASE_MS,
                };
                source.challenge = Some(Challenge {
                    control,
                    generation: source.generation,
                    expires: Instant::now() + Duration::from_millis(ordered::CHALLENGE_MS as u64),
                });
                return Ok(receipt);
            }
        };
        let lease = existing.expect("existing active lease");
        lease.active().await?;
        Ok(lease.receipt(true))
    }

    async fn commit_ordered(self: &Arc<Self>, control: Control) -> Result<Receipt, ErrCode> {
        let (lease, cancelled) = {
            let mut sessions = self.ordered.lock().unwrap();
            if self.reorder_closed.load(Ordering::Acquire) {
                return Err(ErrCode::Stopped);
            }
            let receiver = sessions.receiver;
            let owner = sessions.owner.clone();
            let source = sessions
                .sources
                .get_mut(&control.source)
                .ok_or(ErrCode::Stopped)?;
            if source.closing.is_some() {
                return Err(ErrCode::Overloaded);
            }
            let lease = source
                .active
                .as_ref()
                .filter(|lease| lease.matches(&control))
                .cloned();
            if let Some(lease) = lease {
                (lease, None)
            } else {
                let challenge = source
                    .challenge
                    .as_ref()
                    .filter(|challenge| {
                        challenge.expires > Instant::now()
                            && challenge.control.incarnation == control.incarnation
                            && challenge.control.stream == control.stream
                            && challenge.control.ticket == control.ticket
                    })
                    .ok_or(ErrCode::Stopped)?;
                let generation = challenge.generation;
                let old = source.active.take().or_else(|| source.retiring.take());
                source.retiring = old.clone();
                let cancelled = old.clone();
                let (tx, rx) = mpsc::channel(256);
                let lease = Arc::new(Lease {
                    receiver,
                    epoch: owner.as_ref().map(|owner| owner.epoch()),
                    _owner: owner,
                    source: control.source.clone(),
                    incarnation: control.incarnation,
                    stream: control.stream,
                    ticket: control.ticket,
                    generation,
                    tx,
                    cancel: self.reorder_cancel.child_token(),
                    deadline: Mutex::new(
                        Instant::now() + Duration::from_millis(ordered::LEASE_MS as u64),
                    ),
                    ready: watch::channel(Ready::Preparing).0,
                    done: watch::channel(false).0,
                });
                source.challenge = None;
                source.active = Some(lease.clone());
                // Ingress owns every handoff before releasing the admission lock.
                let weak = Arc::downgrade(self);
                let worker = lease.clone();
                self.reorder_tasks
                    .spawn(crate::transport::notify_safely(async move {
                        let _completion = Completion(worker.clone());
                        let _registration = Registration {
                            ingress: weak.clone(),
                            lease: worker.clone(),
                        };
                        let work = async {
                            if let Some(old) = old {
                                old.cancel.cancel();
                                old.finished().await;
                            }
                            let Some(ingress) = weak.upgrade() else {
                                return;
                            };
                            let Some(_stream) = ingress.reorder_budget.stream() else {
                                worker
                                    .ready
                                    .send_replace(Ready::Failed(ErrCode::Overloaded));
                                return;
                            };
                            let gap = *ingress.reorder_gap_timeout.lock().unwrap();
                            let cap = *ingress.reorder_buffer_cap.lock().unwrap();
                            drop(ingress);
                            worker.ready.send_replace(Ready::Active);
                            ordered_worker(weak.clone(), worker.clone(), rx, gap, cap).await;
                        };
                        tokio::select! {
                            biased;
                            _ = worker.cancel.cancelled() => {},
                            _ = work => {},
                        }
                    }));
                (lease, cancelled)
            }
        };
        if let Some(old) = cancelled {
            old.cancel.cancel();
        }
        lease.active().await?;
        Ok(lease.receipt(true))
    }

    async fn close_ordered_lease(&self, control: Control) -> Result<Receipt, ErrCode> {
        let (lease, previous) = {
            let mut sessions = self.ordered.lock().unwrap();
            if self.reorder_closed.load(Ordering::Acquire) {
                return Err(ErrCode::Stopped);
            }
            let receiver = sessions.receiver;
            let epoch = sessions.owner.as_ref().map(|owner| owner.epoch());
            let absent = |generation| Receipt {
                active: false,
                receiver,
                epoch,
                ticket: control.ticket,
                generation,
                lease_ms: ordered::LEASE_MS,
            };
            let Some(source) = sessions.sources.get_mut(&control.source) else {
                return Ok(absent(1));
            };
            if let Some(closing) = source
                .closing
                .as_ref()
                .filter(|closing| closing.lease.matches(&control))
            {
                (closing.lease.clone(), closing.previous.clone())
            } else if let Some(lease) = source
                .retiring
                .as_ref()
                .filter(|lease| lease.matches(&control))
            {
                (lease.clone(), None)
            } else {
                let Some(lease) = source
                    .active
                    .as_ref()
                    .filter(|lease| lease.matches(&control))
                    .cloned()
                else {
                    // Only the live session owner can prove absence. Generic
                    // request-admission errors carry no retirement guarantee.
                    let receipt = absent(source.generation.max(1));
                    if source.challenge.as_ref().is_some_and(|challenge| {
                        challenge.control.incarnation == control.incarnation
                            && challenge.control.stream == control.stream
                            && challenge.control.ticket == control.ticket
                    }) {
                        source.challenge = None;
                    }
                    sessions.prune_challenges();
                    return Ok(receipt);
                };
                source.active = None;
                let previous = source.retiring.clone();
                if previous.is_none() {
                    source.retiring = Some(lease.clone());
                }
                // Retain the exact close identity and its predecessor even if
                // this request is cancelled or the replacement worker exits first.
                source.closing = Some(Closing {
                    lease: lease.clone(),
                    previous: previous.clone(),
                });
                (lease, previous)
            }
        };
        lease.cancel.cancel();
        if let Some(previous) = previous.as_ref() {
            previous.cancel.cancel();
        }
        lease.finished().await;
        if let Some(previous) = previous {
            previous.finished().await;
        }
        {
            let mut sessions = self.ordered.lock().unwrap();
            if let Some(source) = sessions.sources.get_mut(&control.source) {
                if source
                    .closing
                    .as_ref()
                    .is_some_and(|closing| closing.lease.matches(&control) && closing.is_finished())
                {
                    source.closing = None;
                }
            }
            sessions.prune_challenges();
        }
        Ok(lease.receipt(false))
    }

    pub(super) async fn on_ordered_tell(self: &Arc<Self>, frame: Frame, back: &FrameSender) {
        let payload = match frame.split_trace_parent() {
            Ok((_, payload)) => payload,
            Err(_) => {
                back.close();
                return;
            }
        };
        let (admission, data) = match Data::decode_frame(&frame.type_key, payload) {
            Ok(data) => data,
            Err(_) => {
                back.close();
                return;
            }
        };
        let cid = frame.header.correlation_id;
        if let Err(code) = self.ordered_origin(&data.source, &frame, back) {
            DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
            if admission.is_some() {
                back.send_reply(Frame::reply_err(
                    cid,
                    &frame.path,
                    code,
                    "ordered origin rejected",
                ));
            }
            return;
        }
        if self.is_remote_target(&frame.path) {
            let mut frame = frame;
            let Ok(context) = self.request_context(&mut frame) else {
                back.close();
                return;
            };
            parrot_api::trace::scope(context, async {
                if admission.is_some() {
                    self.try_relay_request(&frame, back, cid).await;
                } else if !self.try_relay_tell(&frame, back) {
                    DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
                }
            })
            .await;
            return;
        }
        if admission.is_some()
            && !self
                .relay
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|router| super::target_node(&frame.path) == Some(router.self_node()))
        {
            back.send_reply(Frame::reply_err(
                cid,
                &frame.path,
                ErrCode::ProtocolViolation,
                "invalid ordered destination",
            ));
            return;
        }
        let lease = {
            let sessions = self.ordered.lock().unwrap();
            if sessions
                .owner
                .as_ref()
                .is_some_and(|owner| !owner.valid_process())
            {
                DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
                if admission.is_some() {
                    back.send_reply(Frame::reply_err(
                        cid,
                        &frame.path,
                        ErrCode::Stopped,
                        "receiver unavailable",
                    ));
                }
                return;
            }
            sessions
                .sources
                .get(&data.source)
                .and_then(|source| source.active.as_ref())
                .filter(|lease| {
                    lease.incarnation == data.incarnation
                        && admission.is_none_or(|id| id.receiver == lease.receiver)
                        && lease.stream == data.stream
                        && lease.ticket == data.ticket
                        && *lease.ready.borrow() == Ready::Active
                        && *lease.deadline.lock().unwrap() > Instant::now()
                })
                .cloned()
        };
        let Some(lease) = lease else {
            DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
            if let Some(id) = admission {
                back.send_reply(Frame::reply(
                    cid,
                    "",
                    ordered::ADMISSION_RECEIPT_KEY,
                    id.receipt(false),
                ));
            }
            return;
        };
        if let Err(code) = self.ordered_local_path(&frame.path) {
            DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
            if let Some(id) = admission {
                let detail = if code == ErrCode::NotRemotable {
                    id.alias_error()
                } else {
                    "ordered endpoint lookup failed".into()
                };
                back.send_reply(Frame::reply_err(cid, "", code, &detail));
            }
            return;
        }
        let Some(frame) = self.charge_reorder(frame) else {
            let reply = admission
                .map(|id| Frame::reply_err(cid, "", ErrCode::Overloaded, &id.capacity_error()));
            self.reject_reorder(back, reply);
            return;
        };
        *lease.deadline.lock().unwrap() =
            Instant::now() + Duration::from_millis(ordered::LEASE_MS as u64);
        let pending = Pending {
            sequence: data.sequence,
            frame,
        };
        let admitted = tokio::select! {
            biased;
            _ = lease.cancel.cancelled() => false,
            sent = lease.tx.send(pending) => sent.is_ok(),
        };
        if !admitted {
            DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(id) = admission {
            back.send_reply(Frame::reply(
                cid,
                "",
                ordered::ADMISSION_RECEIPT_KEY,
                id.receipt(admitted),
            ));
        }
    }

    async fn deliver_ordered(&self, pending: Pending) {
        let ReorderPending { mut frame, permit } = pending.frame;
        let cleanup = Cleanup::default();
        let result = unwind::run(&cleanup, || {
            Box::pin(async {
                let context = self.request_context(&mut frame).map_err(|_| ())?;
                let (_, data) =
                    Data::decode_frame(&frame.type_key, frame.payload.clone()).map_err(|_| ())?;
                frame.header.flags &= !crate::frame::flags::TELL_ACK;
                frame.header.correlation_id = 0;
                frame.type_key = data.key;
                frame.payload = data.payload;
                // The lease terminates at its confirmed node. An application alias
                // must not silently strip origin/lease identity and forward elsewhere.
                parrot_api::trace::scope(
                    context,
                    self.tell_deliver_scoped(frame, &FrameSender::detached(), false),
                )
                .await;
                Ok::<_, ()>(())
            })
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            DEAD_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        drop(permit);
    }
}

async fn ordered_worker(
    ingress: Weak<Ingress>,
    lease: Arc<Lease>,
    mut rx: mpsc::Receiver<Pending>,
    gap: Duration,
    cap: usize,
) {
    let mut expected = 1u64;
    let mut buffer: BTreeMap<u64, Pending> = BTreeMap::new();
    let mut gap_deadline = Instant::now() + gap;
    loop {
        let expiry = *lease.deadline.lock().unwrap();
        let frame = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(expiry) => {
                if *lease.deadline.lock().unwrap() <= Instant::now() { return; }
                continue;
            },
            _ = tokio::time::sleep_until(gap_deadline), if !buffer.is_empty() => {
                let (sequence, pending) = buffer.pop_first().unwrap();
                REORDER_GAP_FLUSHED.fetch_add(1, Ordering::Relaxed);
                if let Some(ingress) = ingress.upgrade() { ingress.deliver_ordered(pending).await; }
                let Some(next) = sequence.checked_add(1) else { return; }; expected = next;
                while let Some(pending) = buffer.remove(&expected) {
                    if let Some(ingress) = ingress.upgrade() { ingress.deliver_ordered(pending).await; }
                    let Some(next) = expected.checked_add(1) else { return; }; expected = next;
                }
                gap_deadline = Instant::now() + gap;
                continue;
            },
            pending = rx.recv() => match pending { Some(pending) => pending, None => return },
        };
        let had_gap = !buffer.is_empty();
        let before = expected;
        if frame.sequence == expected {
            if let Some(ingress) = ingress.upgrade() {
                ingress.deliver_ordered(frame).await;
            }
            let Some(next) = expected.checked_add(1) else {
                return;
            };
            expected = next;
            while let Some(pending) = buffer.remove(&expected) {
                if let Some(ingress) = ingress.upgrade() {
                    ingress.deliver_ordered(pending).await;
                }
                let Some(next) = expected.checked_add(1) else {
                    return;
                };
                expected = next;
            }
        } else if frame.sequence > expected {
            if let std::collections::btree_map::Entry::Vacant(entry) = buffer.entry(frame.sequence)
            {
                entry.insert(frame);
                REORDER_BUFFERED.fetch_add(1, Ordering::Relaxed);
            }
            if buffer.len() > cap {
                let (sequence, pending) = buffer.pop_first().unwrap();
                REORDER_GAP_FLUSHED.fetch_add(1, Ordering::Relaxed);
                if let Some(ingress) = ingress.upgrade() {
                    ingress.deliver_ordered(pending).await;
                }
                let Some(next) = sequence.checked_add(1) else {
                    return;
                };
                expected = next;
                while let Some(pending) = buffer.remove(&expected) {
                    if let Some(ingress) = ingress.upgrade() {
                        ingress.deliver_ordered(pending).await;
                    }
                    let Some(next) = expected.checked_add(1) else {
                        return;
                    };
                    expected = next;
                }
            }
        } else {
            super::LATE_TELL_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
        if !buffer.is_empty() && (!had_gap || expected != before) {
            gap_deadline = Instant::now() + gap;
        }
    }
}
