use super::{
    identity, AdmissionId, Control, Data, Identity, Operation, Receipt, ADMISSION_RECEIPT_KEY,
    ADMIT_KEY, CONTROL_KEY, DATA_KEY, RECEIPT_KEY,
};
use crate::{
    error::ErrCode,
    frame::Frame,
    registry::{CallbackRegistry, ReplyPayload},
    transport::FrameSender,
};
use parrot_api::{errors::ActorError, types::ActorResult};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::time::Instant;

mod aliases;
mod retirement;
use retirement::{LaneEntry, SenderLane};

enum Attempt {
    Delivered,
    Legacy(Frame),
    Alias(Frame),
    Full(Frame),
}

#[derive(Debug)]
enum LaneError {
    Stopped,
    Full,
    SenderScope,
}

impl From<LaneError> for ActorError {
    fn from(error: LaneError) -> Self {
        match error {
            LaneError::Stopped => Self::Stopped,
            LaneError::Full => ErrCode::Overloaded
                .to_actor_error("outgoing ordered destination capacity exhausted".into()),
            LaneError::SenderScope => ErrCode::Overloaded.to_actor_error(
                "conflicting ordered sender scope; retire the previous session before switching"
                    .into(),
            ),
        }
    }
}

enum Admission {
    Accepted,
    Inactive,
    Alias,
}

struct Grant {
    receipt: Receipt,
    actor: bool,
    sender: FrameSender,
    path: String,
    renew_at: Instant,
}

impl Grant {
    fn retired_by(&self, receipt: &Receipt) -> bool {
        !receipt.active
            && receipt.ticket == self.receipt.ticket
            && receipt.receiver == self.receipt.receiver
    }
}

struct Lane {
    stream: Identity,
    sequence: u64,
    grant: Option<Grant>,
    sent_ordered: bool,
    closed: bool,
}
impl Default for Lane {
    fn default() -> Self {
        Self {
            stream: identity(),
            sequence: 0,
            grant: None,
            sent_ordered: false,
            closed: false,
        }
    }
}

#[derive(Clone)]
enum SenderContext {
    Node,
    Actor(parrot_api::sender::WeakActorSender),
    Shared {
        life: parrot_api::sender::WeakActorSender,
        existing_only: bool,
    },
}

#[derive(Clone)]
pub(crate) struct Outbound {
    source: String,
    incarnation: Identity,
    context: SenderContext,
    tables: Arc<SessionTables>,
}

pub(crate) struct SessionTables {
    capacity: usize,
    lanes: Mutex<HashMap<String, LaneEntry>>,
    sender_added: tokio::sync::Notify,
    closed: AtomicBool,
    cancel: tokio_util::sync::CancellationToken,
    aliases: aliases::AliasRoutes,
}

impl std::ops::Deref for Outbound {
    type Target = SessionTables;
    fn deref(&self) -> &Self::Target {
        &self.tables
    }
}

/// A negotiation owns its destination slot while any sender or closer still uses it.
/// Once a ticket exists, explicit retirement owns the slot instead.
struct LaneUse<'a> {
    outbound: &'a Outbound,
    destination: String,
    lane: Option<Arc<tokio::sync::Mutex<Lane>>>,
    retained: bool,
}

impl<'a> LaneUse<'a> {
    fn new(outbound: &'a Outbound, destination: &str, lane: Arc<tokio::sync::Mutex<Lane>>) -> Self {
        Self {
            outbound,
            destination: destination.to_owned(),
            lane: Some(lane),
            retained: false,
        }
    }
}

impl Drop for LaneUse<'_> {
    fn drop(&mut self) {
        if self.retained {
            return;
        }
        let mut lanes = self.outbound.lanes.lock().unwrap();
        let lane = self.lane.take().expect("live lane handle");
        let Some(entry) = lanes
            .get_mut(&self.destination)
            .map(|entry| &entry.lane)
            .filter(|entry| Arc::ptr_eq(entry, &lane))
        else {
            return;
        };
        // Release this reference under the map lock so simultaneous cancellations
        // cannot both see another owner and abandon the slot.
        drop(lane);
        // Alias bindings carry weak identities, never an owner of this lane.
        if Arc::strong_count(entry) == 1
            && entry
                .try_lock()
                .is_ok_and(|state| state.grant.is_none() && !state.sent_ordered)
        {
            lanes.remove(&self.destination);
        }
    }
}

impl Outbound {
    pub fn new(source: String, capacity: usize) -> Self {
        Self {
            source,
            incarnation: identity(),
            context: SenderContext::Node,
            tables: Arc::new(SessionTables {
                capacity,
                lanes: Mutex::new(HashMap::new()),
                sender_added: Default::default(),
                closed: AtomicBool::new(false),
                cancel: tokio_util::sync::CancellationToken::new(),
                aliases: Default::default(),
            }),
        }
    }

    pub(crate) fn for_actor(&self, actor: &parrot_api::sender::ActorSender) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            source: format!(
                "{}@{}",
                self.source,
                hex::encode(Sha256::digest(actor.path().as_bytes()))
            ),
            incarnation: actor.incarnation(),
            context: SenderContext::Actor(actor.downgrade()),
            tables: self.tables.clone(),
        }
    }

    fn table_key<'a>(&self, path: &'a str) -> std::borrow::Cow<'a, str> {
        if self.source.contains('@') {
            format!("{}@{path}", hex::encode(self.incarnation)).into()
        } else {
            path.into()
        }
    }

    pub(crate) fn has_reference(&self, path: &str) -> bool {
        self.lanes
            .lock()
            .unwrap()
            .contains_key(self.lane_key(path).as_ref())
            || self.aliases.contains(&self.table_key(path))
    }

    pub(crate) fn for_delivery(
        &self,
        actor: &parrot_api::sender::ActorSender,
        destination: &str,
        path: &str,
        sender: &FrameSender,
        required: bool,
    ) -> Self {
        let owner = self.for_actor(actor);
        if required {
            return owner;
        }
        let existing_only = sender.supports_sender_sessions();
        if existing_only {
            let lanes = self.lanes.lock().unwrap();
            if !Self::shared_at(&lanes, destination, path) {
                return owner;
            }
        }
        if owner.has_reference(path) {
            return owner;
        }
        Self {
            context: SenderContext::Shared {
                life: actor.downgrade(),
                existing_only,
            },
            ..self.clone()
        }
    }

    fn shared_at(lanes: &HashMap<String, LaneEntry>, destination: &str, path: &str) -> bool {
        [destination, path]
            .iter()
            .any(|key| lanes.get(*key).is_some_and(|e| e.shared_by_actors))
    }

    // A replacement with the same logical source must wait for the prior
    // generation's retirement even when unused capacity is available.
    fn lane_key<'a>(&self, path: &'a str) -> std::borrow::Cow<'a, str> {
        if matches!(self.context, SenderContext::Actor(_)) {
            format!("{}@{path}", self.source).into()
        } else {
            path.into()
        }
    }

    fn sender_retired(&self) -> bool {
        match &self.context {
            SenderContext::Node => false,
            SenderContext::Actor(life) | SenderContext::Shared { life, .. } => life.is_retired(),
        }
    }

    pub fn close_admission(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(crate) async fn resolve_endpoint(
        &self,
        destination: &str,
        path: &str,
        sender: &FrameSender,
        callbacks: &CallbackRegistry,
        hops: u8,
        deadline: Instant,
    ) -> ActorResult<super::OrderedEndpoint> {
        self.lookup(
            Route {
                destination,
                path,
                hops,
            },
            sender,
            callbacks,
            false,
            deadline,
        )
        .await
        .map(|(endpoint, _)| endpoint)
    }

    async fn lookup(
        &self,
        route: Route<'_>,
        sender: &FrameSender,
        callbacks: &CallbackRegistry,
        signed: bool,
        deadline: Instant,
    ) -> ActorResult<(super::OrderedEndpoint, Option<String>)> {
        if self.closed.load(Ordering::Acquire) {
            return Err(ActorError::Stopped);
        }
        if !sender.supports_ordered_endpoint() {
            return Err(
                ErrCode::NotRemotable.to_actor_error("ordered endpoint lookup unavailable".into())
            );
        }
        let mut control = Control {
            operation: if signed && sender.supports_ordered_route() {
                Operation::ResolveRoute
            } else {
                Operation::ResolveEndpoint
            },
            source: self.source.clone(),
            incarnation: self.incarnation,
            stream: identity(),
            ticket: [0; 16],
        };
        loop {
            let reply = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(ActorError::Stopped),
                reply = exchange(sender, callbacks, route.destination, route.path, route.hops, &control, deadline) => reply,
            };
            let (bytes, key) = match reply {
                Err((ErrCode::NotRemotable, detail))
                    if control.operation == Operation::ResolveRoute
                        && detail == super::route::UNSUPPORTED =>
                {
                    control.operation = Operation::ResolveEndpoint;
                    control.stream = identity();
                    continue;
                }
                other => other.map_err(|(code, detail)| code.to_actor_error(detail))?,
            };
            if control.operation == Operation::ResolveRoute && key == super::ROUTE_RECEIPT_KEY {
                if let Some((endpoint, path)) =
                    super::route::decode_receipt(&bytes, control.stream, route.path)
                {
                    return Ok((endpoint, Some(path)));
                }
            } else if control.operation == Operation::ResolveEndpoint && key == super::ENDPOINT_KEY
            {
                if let Some(endpoint) = super::OrderedEndpoint::decode(&bytes, control.stream) {
                    return Ok((endpoint, None));
                }
            }
            return Err(ErrCode::ProtocolViolation
                .to_actor_error("invalid ordered endpoint receipt".into()));
        }
    }
    pub fn close(&self) {
        self.close_admission();
        self.cancel.cancel();
        self.aliases.clear();
        let retired = std::mem::take(&mut *self.lanes.lock().unwrap());
        drop(retired);
    }

    fn lane(
        &self,
        destination: &str,
        path: &str,
        actor: bool,
        supports_ordered: bool,
    ) -> Result<Option<LaneUse<'_>>, LaneError> {
        let mut lanes = self.lanes.lock().unwrap();
        if self.closed.load(Ordering::Acquire) || self.sender_retired() {
            return Err(LaneError::Stopped);
        }
        // Keep an established scope across reconnects, including capability loss.
        let actor_key = self.lane_key(path);
        let node_key = self.lane_key(destination);
        let key = if lanes.contains_key(actor_key.as_ref())
            || (actor && !lanes.contains_key(node_key.as_ref()))
        {
            actor_key.as_ref()
        } else {
            node_key.as_ref()
        };
        // Reserve the delivery scope under the same lock as the lane itself.
        // A pending Open or Close must fence a concurrent capability transition.
        let owner = match &self.context {
            SenderContext::Actor(_) if Self::shared_at(&lanes, destination, path) => {
                return Err(LaneError::SenderScope);
            }
            SenderContext::Actor(life) => Some(life),
            SenderContext::Shared { existing_only, .. } => {
                let continuing = lanes.get(key).is_some_and(|entry| entry.shared_by_actors);
                if !continuing
                    && (*existing_only
                        || lanes
                            .values()
                            .filter_map(|entry| entry.owner.as_ref())
                            .any(|owner| {
                                owner.destination == destination
                                    && (key == destination || owner.path == path)
                            }))
                {
                    return Err(LaneError::SenderScope);
                }
                None
            }
            SenderContext::Node => None,
        };
        let shared_by_actors = matches!(self.context, SenderContext::Shared { .. });
        if let Some(entry) = lanes.get_mut(key) {
            if !entry.owned_by(self.incarnation) {
                return Err(LaneError::Full);
            }
            entry.shared_by_actors |= shared_by_actors && supports_ordered;
            return Ok(Some(LaneUse::new(self, key, entry.lane.clone())));
        }
        if !supports_ordered {
            return Ok(None);
        }
        if lanes.len() >= self.capacity {
            return Err(LaneError::Full);
        }
        let lane = Arc::new(tokio::sync::Mutex::new(Lane::default()));
        let owner = owner.map(|life| SenderLane {
            source: self.source.clone(),
            incarnation: self.incarnation,
            lifetime: life.clone(),
            destination: destination.into(),
            path: path.into(),
        });
        if owner.is_some() {
            self.sender_added.notify_one();
        }
        lanes.insert(
            key.into(),
            LaneEntry {
                lane: lane.clone(),
                owner,
                shared_by_actors,
            },
        );
        Ok(Some(LaneUse::new(self, key, lane)))
    }

    /// Some(frame) is an explicit pre-session compatibility fallback. Once any
    /// ordered data has been published, route changes cannot downgrade the lane.
    #[cfg(test)]
    async fn send(
        &self,
        destination: &str,
        sender: &FrameSender,
        callbacks: &CallbackRegistry,
        frame: Frame,
        required: bool,
    ) -> ActorResult<Option<Frame>> {
        crate::transport::notify_safely(async {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Err(ActorError::Stopped),
                result = self.send_open(destination, sender, callbacks, frame, required, None) => match result? {
                    Attempt::Delivered => Ok(None),
                    Attempt::Legacy(frame) => Ok(Some(frame)),
                    Attempt::Alias(_) => Err(ErrCode::NotRemotable.to_actor_error(super::OPEN_ALIAS.into())),
                    Attempt::Full(_) => Err(LaneError::Full.into()),
                },
            }
        })
        .await
    }

    async fn send_open(
        &self,
        destination: &str,
        sender: &FrameSender,
        callbacks: &CallbackRegistry,
        frame: Frame,
        required: bool,
        binding: Option<(&str, &super::OrderedEndpoint)>,
    ) -> ActorResult<Attempt> {
        let routed = destination != sender.node_id();
        if self.source.contains('@') && !sender.supports_sender_sessions() {
            return Err(ErrCode::NotRemotable
                .to_actor_error("route does not support ordered sender sessions".into()));
        }
        let supports_ordered =
            sender.supports_ordered_tell() && (!routed || sender.supports_ordered_admission());
        let actor_path = binding.map_or(frame.path.as_str(), |(_, endpoint)| &endpoint.path);
        let mut actor_capable =
            sender.supports_actor_sessions() && crate::actor_path::split(actor_path).is_some();
        'negotiate: loop {
            let mut usage =
                match self.lane(destination, actor_path, actor_capable, supports_ordered) {
                    Ok(Some(usage)) => usage,
                    Ok(None) => return legacy_fallback(frame, required),
                    Err(LaneError::Full)
                        if required && binding.is_none() && sender.supports_ordered_endpoint() =>
                    {
                        return Ok(Attempt::Full(frame));
                    }
                    Err(error) => return Err(error.into()),
                };
            let actor = usage.destination != self.lane_key(destination);
            if actor && !actor_capable {
                return Err(ErrCode::NotRemotable.to_actor_error(super::ACTOR_UNSUPPORTED.into()));
            }
            let mut state = usage.lane.as_ref().expect("live lane handle").lock().await;
            usage.retained = state.grant.is_some() || state.sent_ordered;
            if state.closed || self.closed.load(Ordering::Acquire) || self.sender_retired() {
                return Err(ActorError::Stopped);
            }
            if !supports_ordered {
                return legacy_fallback(frame, required || state.sent_ordered);
            }
            if binding.is_some() && !sender.supports_ordered_admission() {
                return Err(ErrCode::NotRemotable
                    .to_actor_error("resolved endpoint requires receiver admission".into()));
            }
            let bind_ticket = |ticket| {
                if let Some((alias, endpoint)) = binding {
                    self.aliases.bind(
                        &self.table_key(alias),
                        endpoint,
                        usage.lane.as_ref().expect("live lane handle"),
                        ticket,
                        self.capacity,
                    )?;
                }
                Ok::<_, ActorError>(())
            };
            let expected_receiver = binding.map(|(_, endpoint)| endpoint.receiver);
            let mut retried = false;
            loop {
                if state.sequence == u64::MAX {
                    if let Some(grant) = &state.grant {
                        let control = Control {
                            operation: if grant.actor {
                                Operation::CloseActor
                            } else {
                                Operation::Close
                            },
                            source: self.source.clone(),
                            incarnation: self.incarnation,
                            stream: state.stream,
                            ticket: grant.receipt.ticket,
                        };
                        self.retire(
                            grant,
                            sender,
                            callbacks,
                            Route {
                                destination,
                                path: &grant.path,
                                hops: frame.header.hop_limit,
                            },
                            &control,
                            Instant::now() + Duration::from_secs(5),
                        )
                        .await?;
                    }
                    state.stream = identity();
                    state.grant = None;
                    state.sequence = 0;
                }
                let refresh = state.grant.as_ref().is_none_or(|grant| {
                    !grant.receipt.active
                        || expected_receiver
                            .is_some_and(|receiver| receiver != grant.receipt.receiver)
                        || grant.path != frame.path
                        || grant.renew_at <= Instant::now()
                        || grant.sender.dispatch_key() != sender.dispatch_key()
                });
                if refresh {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut control = Control {
                        operation: if actor {
                            Operation::OpenActor
                        } else if routed {
                            Operation::OpenAdmission
                        } else {
                            Operation::Open
                        },
                        source: self.source.clone(),
                        incarnation: self.incarnation,
                        stream: state.stream,
                        ticket: [0; 16],
                    };
                    let opened = request(
                        sender,
                        callbacks,
                        destination,
                        &frame.path,
                        frame.header.hop_limit,
                        &control,
                        deadline,
                    )
                    .await;
                    let mut receipt = match opened {
                        Ok(receipt) => receipt,
                        Err((ErrCode::NotRemotable, detail))
                            if actor
                                && !self.source.contains('@')
                                && !usage.retained
                                && detail == super::ACTOR_UNSUPPORTED =>
                        {
                            actor_capable = false;
                            continue 'negotiate;
                        }
                        Err((ErrCode::NotRemotable, detail))
                            if required
                                && sender.supports_ordered_endpoint()
                                && detail == super::OPEN_ALIAS =>
                        {
                            return Ok(Attempt::Alias(frame))
                        }
                        Err((ErrCode::NotRemotable, _)) if !required && !state.sent_ordered => {
                            return Ok(Attempt::Legacy(frame))
                        }
                        Err((code, detail)) => return Err(code.to_actor_error(detail)),
                    };
                    if expected_receiver.is_some_and(|receiver| receiver != receipt.receiver) {
                        return Err(ErrCode::Stopped
                            .to_actor_error("resolved ordered receiver changed".into()));
                    }
                    if let Some(previous) = state
                        .grant
                        .as_ref()
                        .filter(|grant| grant.receipt.receiver != receipt.receiver)
                    {
                        // Only the original receiver can certify its workers retired.
                        // Keep its grant on cancellation or failure so a new route
                        // cannot silently erase an unresolved predecessor.
                        let closing = Control {
                            operation: if previous.actor {
                                Operation::CloseActor
                            } else {
                                Operation::Close
                            },
                            source: self.source.clone(),
                            incarnation: self.incarnation,
                            stream: state.stream,
                            ticket: previous.receipt.ticket,
                        };
                        self.retire(
                            previous,
                            sender,
                            callbacks,
                            Route {
                                destination,
                                path: &previous.path,
                                hops: frame.header.hop_limit,
                            },
                            &closing,
                            deadline,
                        )
                        .await?;
                    }
                    bind_ticket(receipt.ticket)?;
                    usage.retained = true;
                    if !receipt.active {
                        if state.grant.as_ref().is_none_or(|grant| {
                            grant.receipt.ticket != receipt.ticket
                                || grant.receipt.receiver != receipt.receiver
                        }) {
                            state.sequence = 0;
                        }
                        // Cancellation after COMMIT publication must still leave enough
                        // identity for explicit close to join the receiver's handoff.
                        state.grant = Some(Grant {
                            receipt,
                            actor,
                            sender: sender.clone(),
                            path: frame.path.clone(),
                            renew_at: Instant::now(),
                        });
                        control.operation = if actor {
                            Operation::CommitActor
                        } else {
                            Operation::Commit
                        };
                        control.ticket = receipt.ticket;
                        let committed = request(
                            sender,
                            callbacks,
                            destination,
                            &frame.path,
                            frame.header.hop_limit,
                            &control,
                            deadline,
                        )
                        .await
                        .map_err(|(code, detail)| code.to_actor_error(detail))?;
                        if !committed.active
                            || committed.ticket != receipt.ticket
                            || committed.generation != receipt.generation
                            || committed.receiver != receipt.receiver
                            || committed.epoch != receipt.epoch
                        {
                            return Err(ErrCode::ProtocolViolation.to_actor_error(
                                "ordered commit receipt differs from challenge".into(),
                            ));
                        }
                        receipt = committed;
                    }
                    if state.grant.as_ref().is_none_or(|grant| {
                        grant.receipt.ticket != receipt.ticket
                            || grant.receipt.receiver != receipt.receiver
                    }) {
                        state.sequence = 0;
                    }
                    state.grant = Some(Grant {
                        receipt,
                        actor,
                        sender: sender.clone(),
                        path: frame.path.clone(),
                        renew_at: Instant::now()
                            + Duration::from_millis((receipt.lease_ms / 2).max(1) as u64),
                    });
                } else {
                    bind_ticket(state.grant.as_ref().expect("active grant").receipt.ticket)?;
                }
                match self
                    .publish(
                        &mut state,
                        &frame,
                        sender,
                        callbacks,
                        destination,
                        routed || (required && sender.supports_ordered_admission()),
                    )
                    .await?
                {
                    Admission::Accepted => return Ok(Attempt::Delivered),
                    Admission::Alias if required && sender.supports_ordered_endpoint() => {
                        return Ok(Attempt::Alias(frame))
                    }
                    Admission::Alias => {
                        return Err(ErrCode::NotRemotable.to_actor_error(super::OPEN_ALIAS.into()))
                    }
                    Admission::Inactive => {}
                }
                // A matching negative receipt proves this attempt never entered the worker.
                // Preserve its grant so refresh must still fence any previous receiver.
                state.sequence -= 1;
                state.grant.as_mut().expect("confirmed grant").renew_at = Instant::now();
                if retried {
                    return Err(ErrCode::Stopped
                        .to_actor_error("ordered receiver rejected the refreshed ticket".into()));
                }
                retried = true;
            }
        }
    }

    async fn publish(
        &self,
        state: &mut Lane,
        original: &Frame,
        sender: &FrameSender,
        callbacks: &CallbackRegistry,
        destination: &str,
        confirm_admission: bool,
    ) -> ActorResult<Admission> {
        let mut frame = original.clone();
        let grant = state.grant.as_ref().expect("confirmed grant");
        let receipt = grant.receipt;
        let sequence = state.sequence.checked_add(1).ok_or_else(|| {
            ErrCode::Overloaded.to_actor_error("ordered sequence exhausted".into())
        })?;
        let id = AdmissionId {
            receiver: receipt.receiver,
            ticket: receipt.ticket,
            sequence,
        };
        let envelope = Data {
            source: self.source.clone(),
            incarnation: self.incarnation,
            stream: state.stream,
            ticket: receipt.ticket,
            sequence,
            key: frame.type_key,
            payload: frame.payload,
        };
        frame.payload = if confirm_admission {
            frame.type_key = ADMIT_KEY.into();
            frame.header.flags |= crate::frame::flags::TELL_ACK;
            frame.header.correlation_id = callbacks.next_cid();
            envelope.encode_scoped(Some(id.receiver), grant.actor)
        } else {
            frame.type_key = DATA_KEY.into();
            envelope.encode_scoped(None, grant.actor)
        }
        .map_err(|detail| ErrCode::ProtocolViolation.to_actor_error(detail))?;
        frame.header.seq = 0;
        let frame = sender
            .trace_request(frame)
            .map_err(|error| error.to_actor_error())?;
        let permit = sender
            .reserve(&frame)
            .await
            .map_err(|error| error.to_actor_error())?;
        let pending = if confirm_admission {
            Some(
                callbacks
                    .register_on(frame.header.correlation_id, destination.to_owned(), sender)
                    .map_err(|error| error.to_actor_error())?,
            )
        } else {
            None
        };
        // No await separates assigning a sequence from publication.
        if self.sender_retired() {
            return Err(ActorError::Stopped);
        }
        state.sequence = sequence;
        state.sent_ordered = true;
        permit.send(frame);
        let Some((reply, _guard)) = pending else {
            return Ok(Admission::Accepted);
        };
        tokio::select! {
            biased;
            result = reply => match result {
                Ok(ReplyPayload::Ok(bytes, key)) if key == ADMISSION_RECEIPT_KEY => id.outcome(&bytes)
                    .map(|admitted| if admitted { Admission::Accepted } else { Admission::Inactive })
                    .ok_or_else(|| ErrCode::ProtocolViolation.to_actor_error("ordered admission identity differs".into())),
                Ok(ReplyPayload::Err(ErrCode::Overloaded, detail)) if detail == id.capacity_error() => {
                    // We still hold the lane lock. Only this exact attempted sequence
                    // was refused before enqueue; the active ticket remains valid.
                    state.sequence -= 1;
                    Err(ErrCode::Overloaded.to_actor_error("ordered receiver at capacity".into()))
                }
                Ok(ReplyPayload::Err(ErrCode::NotRemotable, detail)) if detail == id.alias_error() => {
                    state.sequence -= 1;
                    state.grant.as_mut().expect("confirmed grant").renew_at = Instant::now();
                    Ok(Admission::Alias)
                }
                Ok(ReplyPayload::Err(code @ (ErrCode::ActorNotFound | ErrCode::Timeout | ErrCode::ProtocolViolation), detail))
                    if detail == id.endpoint_error() => {
                    state.sequence -= 1;
                    Err(code.to_actor_error("ordered receiver could not acquire the destination actor".into()))
                }
                Ok(ReplyPayload::Err(code, detail)) => Err(code.to_actor_error(detail)),
                Ok(_) => Err(ErrCode::ProtocolViolation.to_actor_error("invalid ordered admission response".into())),
                Err(_) => Err(ErrCode::ConnectionLost.to_actor_error("ordered admission callback closed".into())),
            },
            _ = sender.closed() => Err(ErrCode::ConnectionLost.to_actor_error("ordered admission connection closed".into())),
            _ = tokio::time::sleep(Duration::from_secs(5)) => Err(ActorError::Timeout),
        }
    }

    #[cfg(test)]
    pub async fn close_lane<F: std::future::Future<Output = Option<FrameSender>>>(
        &self,
        destination: &str,
        resolve_sender: impl FnOnce(String) -> F,
        callbacks: &CallbackRegistry,
        deadline: Instant,
    ) -> ActorResult<()> {
        self.close_matching(
            destination,
            resolve_sender,
            callbacks,
            deadline,
            aliases::CloseScope::Node,
        )
        .await
    }

    async fn close_matching<F: std::future::Future<Output = Option<FrameSender>>>(
        &self,
        destination: &str,
        resolve_sender: impl FnOnce(String) -> F,
        callbacks: &CallbackRegistry,
        deadline: Instant,
        scope: aliases::CloseScope<'_>,
    ) -> ActorResult<()> {
        let found = {
            let lanes = self.lanes.lock().unwrap();
            if self.closed.load(Ordering::Acquire) {
                return Err(ActorError::Stopped);
            }
            scope
                .path()
                .and_then(|path| lanes.get_key_value(self.lane_key(path).as_ref()))
                .or_else(|| lanes.get_key_value(self.lane_key(destination).as_ref()))
                .filter(|(_, entry)| {
                    entry.owned_by(self.incarnation) && scope.matches_lane(&entry.lane)
                })
                .map(|(key, entry)| (key.clone(), entry.lane.clone()))
        };
        let Some((key, lane)) = found else {
            return Ok(());
        };
        let mut usage = LaneUse::new(self, &key, lane);
        let lane = usage.lane.as_ref().expect("live lane handle");
        crate::transport::notify_safely(async {
            tokio::time::timeout_at(deadline, async {
                let mut state = lane.lock().await;
                usage.retained = state.grant.is_some() || state.sent_ordered;
                if state.closed || !scope.matches_grant(state.grant.as_ref()) {
                    return Ok(());
                }
                if let Some(grant) = &state.grant {
                    let first = crate::actor_path::split(super::route::head(&grant.path))
                        .map_or(destination, |(node, _)| node);
                    let current = resolve_sender(first.to_owned()).await;
                    let sender = current.as_ref().unwrap_or(&grant.sender);
                    let control = Control {
                        operation: if grant.actor {
                            Operation::CloseActor
                        } else {
                            Operation::Close
                        },
                        source: self.source.clone(),
                        incarnation: self.incarnation,
                        stream: state.stream,
                        ticket: grant.receipt.ticket,
                    };
                    self.retire(
                        grant,
                        sender,
                        callbacks,
                        Route {
                            destination,
                            path: &grant.path,
                            hops: crate::frame::DEFAULT_HOP_LIMIT,
                        },
                        &control,
                        deadline,
                    )
                    .await?;
                }
                state.closed = true;
                let mut lanes = self.lanes.lock().unwrap();
                if lanes
                    .get(&key)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.lane, lane))
                {
                    lanes.remove(&key);
                }
                Ok(())
            })
            .await
            .unwrap_or(Err(ActorError::Timeout))
        })
        .await
    }
}

fn legacy_fallback(frame: Frame, requires_ordered: bool) -> ActorResult<Attempt> {
    if requires_ordered {
        return Err(ErrCode::NotRemotable
            .to_actor_error("route does not support ordered TELL sessions".into()));
    }
    Ok(Attempt::Legacy(frame))
}

struct Route<'a> {
    destination: &'a str,
    path: &'a str,
    hops: u8,
}

impl Outbound {
    async fn retire(
        &self,
        grant: &Grant,
        current: &FrameSender,
        callbacks: &CallbackRegistry,
        route: Route<'_>,
        control: &Control,
        deadline: Instant,
    ) -> ActorResult<()> {
        let current_path = if current.node_id() == route.destination && route.path.contains('#') {
            super::route::terminal(route.path)
                .map_err(|code| code.to_actor_error("invalid saved ordered route".into()))?
        } else {
            route.path
        };
        // Recovery and Close must carry the original sender and any signed route.
        // A replacement link can advertise node recovery without either feature.
        let carries_identity = |sender: &FrameSender, path: &str| {
            (!control.source.contains('@') || sender.supports_sender_sessions())
                && (!path.contains('#') || sender.supports_ordered_route())
        };
        if let Some(epoch) = grant.receipt.epoch.filter(|_| {
            current.supports_ordered_recovery() && carries_identity(current, current_path)
        }) {
            let mut recovery = control.clone();
            recovery.operation = Operation::Recover {
                receiver: grant.receipt.receiver,
                epoch,
            };
            match request(
                current,
                callbacks,
                route.destination,
                current_path,
                route.hops,
                &recovery,
                deadline,
            )
            .await
            {
                Ok(receipt) if grant.retired_by(&receipt) => return Ok(()),
                Ok(_) => {
                    return Err(ErrCode::ProtocolViolation
                        .to_actor_error("ordered recovery identity differs".into()))
                }
                Err((ErrCode::NotRemotable | ErrCode::Forbidden, _)) => {}
                Err((code, detail)) => return Err(code.to_actor_error(detail)),
            }
        }
        let (sender, path) = if grant.sender.is_closed() {
            (current, current_path)
        } else {
            (&grant.sender, route.path)
        };
        if !carries_identity(sender, path)
            || !sender.supports_ordered_tell()
            || (grant.actor && !sender.supports_actor_sessions())
        {
            return Err(ErrCode::NotRemotable
                .to_actor_error("route cannot retire the original ordered session".into()));
        }
        let result = request(
            sender,
            callbacks,
            route.destination,
            path,
            route.hops,
            control,
            deadline,
        )
        .await;
        let receipt = match result {
            Err((ErrCode::Forbidden, detail))
                if path.contains('#') && current.supports_ordered_route() =>
            {
                // A restarted issuer has a new signing key. Re-discover through
                // current policy, then require proof about the original grant.
                let alias = super::route::head(path);
                let (first, _) = crate::actor_path::split(alias).expect("validated saved alias");
                let (endpoint, signed) = self
                    .lookup(
                        Route {
                            destination: first,
                            path: alias,
                            hops: route.hops,
                        },
                        current,
                        callbacks,
                        true,
                        deadline,
                    )
                    .await?;
                let terminal = super::route::terminal(path)
                    .map_err(|code| code.to_actor_error("invalid saved ordered route".into()))?;
                let signed = signed
                    .filter(|fresh| fresh != path && endpoint.path == terminal)
                    .ok_or_else(|| ErrCode::Forbidden.to_actor_error(detail.clone()))?;
                let mut retry = control.clone();
                if endpoint.receiver != grant.receipt.receiver {
                    let epoch = grant
                        .receipt
                        .epoch
                        .filter(|_| current.supports_ordered_recovery())
                        .ok_or_else(|| ErrCode::Forbidden.to_actor_error(detail))?;
                    retry.operation = Operation::Recover {
                        receiver: grant.receipt.receiver,
                        epoch,
                    };
                }
                request(
                    current,
                    callbacks,
                    route.destination,
                    &signed,
                    route.hops,
                    &retry,
                    deadline,
                )
                .await
            }
            other => other,
        }
        .map_err(|(code, detail)| code.to_actor_error(detail))?;
        if !grant.retired_by(&receipt) {
            return Err(ErrCode::ProtocolViolation
                .to_actor_error("ordered retirement identity differs".into()));
        }
        Ok(())
    }
}

async fn request(
    sender: &FrameSender,
    callbacks: &CallbackRegistry,
    destination: &str,
    path: &str,
    hops: u8,
    control: &Control,
    deadline: Instant,
) -> Result<Receipt, (ErrCode, String)> {
    let (bytes, key) = exchange(
        sender,
        callbacks,
        destination,
        path,
        hops,
        control,
        deadline,
    )
    .await?;
    if key != RECEIPT_KEY {
        return Err((
            ErrCode::ProtocolViolation,
            "invalid ordered control response".into(),
        ));
    }
    let receipt = Receipt::decode(&bytes).map_err(|detail| (ErrCode::ProtocolViolation, detail))?;
    if let Operation::Recover { receiver, epoch } = control.operation {
        if receipt.active
            || receipt.ticket != control.ticket
            || receipt.receiver != receiver
            || receipt.epoch != Some(epoch)
        {
            return Err((
                ErrCode::ProtocolViolation,
                "ordered recovery receipt differs from request".into(),
            ));
        }
    }
    if receipt.epoch.is_some() && !sender.supports_ordered_recovery() {
        return Err((
            ErrCode::ProtocolViolation,
            "unnegotiated ordered recovery receipt".into(),
        ));
    }
    Ok(receipt)
}

async fn exchange(
    sender: &FrameSender,
    callbacks: &CallbackRegistry,
    destination: &str,
    path: &str,
    hops: u8,
    control: &Control,
    deadline: Instant,
) -> Result<(bytes::Bytes, String), (ErrCode, String)> {
    let operation = async {
        let payload = control
            .encode()
            .map_err(|detail| (ErrCode::ProtocolViolation, detail))?;
        let cid = callbacks.next_cid();
        let (receiver, _pending) = callbacks
            .register_on(cid, destination.to_owned(), sender)
            .map_err(|error| (ErrCode::ConnectionLost, error.to_string()))?;
        let mut frame = Frame::ask(cid, path, CONTROL_KEY, payload, None);
        frame.header.hop_limit = hops;
        sender
            .send(frame)
            .await
            .map_err(|error| (ErrCode::ConnectionLost, error.to_string()))?;
        tokio::select! {
            biased;
            _ = sender.closed() => Err((ErrCode::ConnectionLost, "ordered control connection closed".into())),
            reply = receiver => match reply {
                Ok(ReplyPayload::Ok(bytes, key)) => Ok((bytes, key)),
                Ok(ReplyPayload::Err(code, detail)) => Err((code, detail)),
                _ => Err((ErrCode::ProtocolViolation, "invalid ordered control response".into())),
            },
        }
    };
    tokio::time::timeout_at(deadline, operation)
        .await
        .unwrap_or_else(|_| Err((ErrCode::Timeout, "ordered control deadline exceeded".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake::caps;

    #[test]
    fn sender_scope_is_reserved_at_allocation_and_rechecked_after_selection() {
        let outbound = Outbound::new("source".into(), 4);
        let owner = parrot_api::sender::ActorSenderOwner::new("/user/writer");
        let actor = owner.sender();
        let path = "parrot://target/service";
        let other = "parrot://target/other";
        let base = caps::ORDERED_TELL
            | caps::ORDERED_ADMISSION
            | caps::TELL_COMPLETION
            | caps::ORDERED_ACTOR;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let old = FrameSender::capability_test_sender(tx.clone(), "target", base);
        let new = FrameSender::capability_test_sender(tx, "target", base | caps::ORDERED_SENDER);
        let fallback = outbound.for_delivery(&actor, "target", path, &old, false);
        let private = outbound.for_actor(&actor);
        let slot = fallback.lane("target", path, true, true).unwrap().unwrap();
        let selected = outbound.for_delivery(&actor, "target", path, &new, false);
        assert!(matches!(
            private.lane("target", path, true, true),
            Err(LaneError::SenderScope)
        ));
        // A path-scoped shared ticket leaves other destination paths independent.
        let sibling = private.lane("target", other, true, true).unwrap().unwrap();
        drop(slot);
        assert!(matches!(
            selected.lane("target", path, true, true),
            Err(LaneError::SenderScope)
        ));
        let replacement = private.lane("target", path, true, true).unwrap().unwrap();
        assert!(matches!(
            fallback.lane("target", path, true, true),
            Err(LaneError::SenderScope)
        ));
        drop((sibling, replacement));
        assert!(outbound.lanes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn recovery_confirmation_requires_the_requested_epoch() {
        let callbacks = CallbackRegistry::default();
        let (tx, mut frames) = tokio::sync::mpsc::channel(1);
        let sender = FrameSender::capability_test_sender(
            tx,
            "target",
            caps::ORDERED_TELL | caps::ORDERED_RECOVERY,
        );
        let control = Control {
            source: "source".into(),
            incarnation: [1; 16],
            stream: [2; 16],
            ticket: [3; 16],
            operation: Operation::Recover {
                receiver: [4; 16],
                epoch: 7,
            },
        };
        for epoch in [None, Some(6), Some(7)] {
            let mut reply = Box::pin(request(
                &sender,
                &callbacks,
                "target",
                "parrot://target/service",
                4,
                &control,
                Instant::now() + Duration::from_secs(3),
            ));
            assert!(futures::poll!(&mut reply).is_pending());
            let frame = frames.try_recv().unwrap();
            let receipt = Receipt {
                receiver: [4; 16],
                ticket: [3; 16],
                epoch,
                active: false,
                generation: 1,
                lease_ms: super::super::LEASE_MS,
            };
            let response = ReplyPayload::Ok(receipt.encode(), RECEIPT_KEY.into());
            assert!(callbacks.complete_from(frame.header.correlation_id, response, &sender));
            if epoch == Some(7) {
                assert_eq!(reply.await.unwrap(), receipt);
            } else {
                assert_eq!(reply.await.unwrap_err().0, ErrCode::ProtocolViolation);
            }
        }
    }

    #[tokio::test]
    async fn signed_lookup_retries_only_explicit_capability_refusal() {
        let outbound = Outbound::new("source".into(), 1);
        let callbacks = CallbackRegistry::default();
        let path = "parrot://alias/service";
        let endpoint = super::super::OrderedEndpoint {
            path: "parrot://end/target".into(),
            receiver: [2; 16],
        };
        let base = caps::ORDERED_TELL
            | caps::TELL_COMPLETION
            | caps::ORDERED_ADMISSION
            | caps::ORDERED_ENDPOINT;
        for (cap, fallback) in [
            (base, false),
            (base | caps::ORDERED_ROUTE, true),
            (base | caps::ORDERED_ROUTE, false),
        ] {
            let (tx, mut frames) = tokio::sync::mpsc::channel(2);
            let sender = FrameSender::capability_test_sender(tx, "hub", cap);
            let mut lookup = Box::pin(outbound.lookup(
                Route {
                    destination: "alias",
                    path,
                    hops: 4,
                },
                &sender,
                &callbacks,
                true,
                Instant::now() + Duration::from_secs(3),
            ));
            assert!(futures::poll!(&mut lookup).is_pending());
            let mut frame = frames.try_recv().unwrap();
            let decode =
                |frame: &Frame| Control::decode(frame.borrowed_ask_payload().unwrap()).unwrap();
            let initial = decode(&frame);
            if cap != base {
                assert_eq!(initial.operation, Operation::ResolveRoute);
                let error = if fallback {
                    ReplyPayload::Err(
                        ErrCode::NotRemotable,
                        super::super::route::UNSUPPORTED.into(),
                    )
                } else {
                    ReplyPayload::Err(ErrCode::Forbidden, "denied".into())
                };
                assert!(callbacks.complete_from(frame.header.correlation_id, error, &sender));
                if !fallback {
                    assert!(lookup.await.unwrap_err().to_string().contains("forbidden"));
                    assert!(frames.try_recv().is_err());
                    continue;
                }
                assert!(futures::poll!(&mut lookup).is_pending());
                frame = frames.try_recv().unwrap();
                assert_ne!(initial.stream, decode(&frame).stream);
            }
            let control = decode(&frame);
            assert_eq!(control.operation, Operation::ResolveEndpoint);
            let reply = ReplyPayload::Ok(
                endpoint.encode(control.stream),
                super::super::ENDPOINT_KEY.into(),
            );
            assert!(callbacks.complete_from(frame.header.correlation_id, reply, &sender));
            assert_eq!(lookup.await.unwrap(), (endpoint.clone(), None));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ambiguous_admission_never_republishes_or_reuses_the_sequence() {
        let outbound = Outbound::new("source".into(), 1);
        let callbacks = CallbackRegistry::default();
        let (tx, mut frames) = tokio::sync::mpsc::channel(4);
        let sender = FrameSender::capability_test_sender(
            tx,
            "hub",
            caps::ORDERED_TELL | caps::TELL_COMPLETION | caps::ORDERED_ADMISSION,
        );
        let usage = outbound
            .lane("target", "target", false, true)
            .unwrap()
            .unwrap();
        usage.lane.as_ref().unwrap().lock().await.grant = Some(Grant {
            actor: false,
            receipt: Receipt {
                receiver: [2; 16],
                ticket: [3; 16],
                active: true,
                generation: 1,
                epoch: None,
                lease_ms: super::super::LEASE_MS,
            },
            sender: sender.clone(),
            path: "parrot://target/service".into(),
            renew_at: Instant::now() + Duration::from_secs(30),
        });
        let mut last_cid = None;
        for sequence in 1..=9 {
            let frame = Frame::tell("parrot://target/service", "work", bytes::Bytes::new());
            let mut send = Box::pin(outbound.send("target", &sender, &callbacks, frame, true));
            assert!(futures::poll!(&mut send).is_pending());
            let published = frames.try_recv().unwrap();
            let (Some(id), data) =
                Data::decode_frame(&published.type_key, published.payload).unwrap()
            else {
                panic!("expected correlated ordered data");
            };
            assert_eq!(data.sequence, sequence);
            let cid = published.header.correlation_id;
            let response = |bytes| ReplyPayload::Ok(bytes, ADMISSION_RECEIPT_KEY.into());
            if let Some(late) = last_cid {
                assert!(!callbacks.complete_from(late, response(id.receipt(false)), &sender));
                assert!(futures::poll!(&mut send).is_pending());
            }
            match sequence {
                1 => drop(send),
                2 => assert!(matches!(send.await, Err(ActorError::Timeout))),
                3 => {
                    let other = AdmissionId {
                        ticket: [9; 16],
                        ..id
                    };
                    assert!(callbacks.complete_from(cid, response(other.receipt(false)), &sender));
                    assert!(send
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("identity differs"));
                }
                4 => {
                    assert!(callbacks.complete_from(cid, response(id.receipt(true)), &sender));
                    assert!(send.await.unwrap().is_none());
                }
                5..=8 => {
                    let detail = match sequence {
                        5 => "inbound dispatch queue full".into(),
                        6 => AdmissionId {
                            receiver: [9; 16],
                            ..id
                        }
                        .capacity_error(),
                        7 => AdmissionId {
                            ticket: [9; 16],
                            ..id
                        }
                        .capacity_error(),
                        _ => AdmissionId {
                            sequence: sequence - 1,
                            ..id
                        }
                        .capacity_error(),
                    };
                    assert!(callbacks.complete_from(
                        cid,
                        ReplyPayload::Err(ErrCode::Overloaded, detail),
                        &sender
                    ));
                    assert!(send
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("remote overloaded"));
                }
                _ => {
                    sender.close();
                    assert!(send.await.is_err());
                }
            }
            assert!(frames.try_recv().is_err());
            assert!(callbacks.is_empty());
            last_cid = Some(cid);
        }
    }

    #[tokio::test]
    async fn older_first_hop_rejects_strict_routed_data_before_grant_allocation() {
        let outbound = Outbound::new("source".into(), 1);
        let callbacks = CallbackRegistry::default();
        let (tx, mut frames) = tokio::sync::mpsc::channel(2);
        let sender = FrameSender::capability_test_sender(tx, "hub", caps::ORDERED_TELL);
        let frame = Frame::tell("parrot://target/service", "work", bytes::Bytes::new());
        assert!(outbound
            .send("target", &sender, &callbacks, frame.clone(), true)
            .await
            .is_err());
        assert_eq!(
            outbound
                .send("target", &sender, &callbacks, frame.clone(), false)
                .await
                .unwrap()
                .unwrap()
                .type_key,
            frame.type_key
        );
        assert!(frames.try_recv().is_err());
        assert!(outbound.lanes.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn pending_sends_and_close_hold_the_slot_until_the_last_user_cancels() {
        let outbound = Outbound::new("source".into(), 1);
        let callbacks = CallbackRegistry::default();
        let (tx, mut frames) = tokio::sync::mpsc::channel(4);
        let sender = FrameSender::capability_test_sender(
            tx,
            "hub",
            caps::BIN | caps::ORDERED_TELL | caps::TELL_COMPLETION | caps::ORDERED_ADMISSION,
        );
        let send = |destination| {
            let frame = Frame::tell("/target", "work", bytes::Bytes::new());
            Box::pin(outbound.send(destination, &sender, &callbacks, frame, true))
        };
        let mut first = send("first");
        assert!(futures::poll!(&mut first).is_pending());
        assert_eq!(frames.try_recv().unwrap().type_key, CONTROL_KEY);
        let mut retry = send("first");
        assert!(futures::poll!(&mut retry).is_pending());
        let mut closing = Box::pin(outbound.close_lane(
            "first",
            |_| std::future::ready(Some(sender.clone())),
            &callbacks,
            Instant::now() + Duration::from_secs(5),
        ));
        assert!(futures::poll!(&mut closing).is_pending());

        drop(first);
        assert!(futures::poll!(&mut retry).is_pending());
        assert_eq!(frames.try_recv().unwrap().type_key, CONTROL_KEY);
        assert!(send("second")
            .await
            .unwrap_err()
            .to_string()
            .contains("capacity"));
        drop(retry);
        assert!(send("second")
            .await
            .unwrap_err()
            .to_string()
            .contains("capacity"));
        drop(closing);
        assert!(callbacks.is_empty());

        // An admitted Open sends a control frame; let its unacknowledged ticket time out.
        let mut next = send("second");
        assert!(futures::poll!(&mut next).is_pending());
        assert_eq!(frames.try_recv().unwrap().type_key, CONTROL_KEY);
        assert!(matches!(next.await, Err(ActorError::TimeoutDetail(detail))
            if detail == "ordered control deadline exceeded"));
        let mut after_timeout = send("third");
        assert!(futures::poll!(&mut after_timeout).is_pending());
        assert_eq!(frames.try_recv().unwrap().type_key, CONTROL_KEY);
        drop(after_timeout);
        assert!(callbacks.is_empty());
    }
}
