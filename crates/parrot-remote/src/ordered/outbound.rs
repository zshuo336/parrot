use super::{
    identity, Control, Data, Identity, Operation, Receipt, CONTROL_KEY, DATA_KEY, RECEIPT_KEY,
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

struct Grant {
    receipt: Receipt,
    sender: FrameSender,
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

pub(crate) struct Outbound {
    source: String,
    incarnation: Identity,
    capacity: usize,
    lanes: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Lane>>>>,
    closed: AtomicBool,
    cancel: tokio_util::sync::CancellationToken,
}

/// A negotiation owns its destination slot while any sender or closer still uses it.
/// Once a ticket exists, explicit retirement owns the slot instead.
struct LaneUse<'a> {
    outbound: &'a Outbound,
    destination: &'a str,
    lane: Option<Arc<tokio::sync::Mutex<Lane>>>,
    retained: bool,
}

impl<'a> LaneUse<'a> {
    fn new(
        outbound: &'a Outbound,
        destination: &'a str,
        lane: Arc<tokio::sync::Mutex<Lane>>,
    ) -> Self {
        Self {
            outbound,
            destination,
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
            .get_mut(self.destination)
            .filter(|entry| Arc::ptr_eq(entry, &lane))
        else {
            return;
        };
        // Release this reference under the map lock so simultaneous cancellations
        // cannot both see another owner and abandon the slot.
        drop(lane);
        if let Some(unique) = Arc::get_mut(entry) {
            let state = unique.get_mut();
            if state.grant.is_none() && !state.sent_ordered {
                lanes.remove(self.destination);
            }
        }
    }
}

impl Outbound {
    pub fn new(source: String, capacity: usize) -> Self {
        Self {
            source,
            incarnation: identity(),
            capacity,
            lanes: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub fn close_admission(&self) {
        self.closed.store(true, Ordering::Release);
    }
    pub fn close(&self) {
        self.close_admission();
        self.cancel.cancel();
        let retired = std::mem::take(&mut *self.lanes.lock().unwrap());
        drop(retired);
    }

    fn lane(
        &self,
        destination: &str,
        supports_ordered: bool,
    ) -> ActorResult<Option<Arc<tokio::sync::Mutex<Lane>>>> {
        let mut lanes = self.lanes.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {
            return Err(ActorError::Stopped);
        }
        if let Some(lane) = lanes.get(destination) {
            return Ok(Some(lane.clone()));
        }
        if !supports_ordered {
            return Ok(None);
        }
        if lanes.len() >= self.capacity {
            return Err(ErrCode::Overloaded
                .to_actor_error("outgoing ordered destination capacity exhausted".into()));
        }
        let lane = Arc::new(tokio::sync::Mutex::new(Lane::default()));
        lanes.insert(destination.into(), lane.clone());
        Ok(Some(lane))
    }

    /// Some(frame) is an explicit pre-session compatibility fallback. Once any
    /// ordered data has been published, route changes cannot downgrade the lane.
    pub async fn send(
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
                result = self.send_open(destination, sender, callbacks, frame, required) => result,
            }
        })
        .await
    }

    async fn send_open(
        &self,
        destination: &str,
        sender: &FrameSender,
        callbacks: &CallbackRegistry,
        mut frame: Frame,
        required: bool,
    ) -> ActorResult<Option<Frame>> {
        let supports_ordered = sender.supports_ordered_tell();
        let Some(lane) = self.lane(destination, supports_ordered)? else {
            return legacy_fallback(frame, required);
        };
        let mut usage = LaneUse::new(self, destination, lane);
        let mut state = usage.lane.as_ref().expect("live lane handle").lock().await;
        usage.retained = state.grant.is_some() || state.sent_ordered;
        if state.closed || self.closed.load(Ordering::Acquire) {
            return Err(ActorError::Stopped);
        }
        if !supports_ordered {
            return legacy_fallback(frame, required || state.sent_ordered);
        }
        if state.sequence == u64::MAX {
            if let Some(grant) = &state.grant {
                let control = Control {
                    operation: Operation::Close,
                    source: self.source.clone(),
                    incarnation: self.incarnation,
                    stream: state.stream,
                    ticket: grant.receipt.ticket,
                };
                retire(
                    grant,
                    sender,
                    callbacks,
                    Route {
                        destination,
                        path: &frame.path,
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
                || grant.renew_at <= Instant::now()
                || grant.sender.dispatch_key() != sender.dispatch_key()
        });
        if refresh {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut control = Control {
                operation: Operation::Open,
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
                Err((ErrCode::NotRemotable, _)) if !required && !state.sent_ordered => {
                    return Ok(Some(frame))
                }
                Err((code, detail)) => return Err(code.to_actor_error(detail)),
            };
            if let Some(previous) = state
                .grant
                .as_ref()
                .filter(|grant| grant.receipt.receiver != receipt.receiver)
            {
                // Only the original receiver can certify its workers retired.
                // Keep its grant on cancellation or failure so a new route
                // cannot silently erase an unresolved predecessor.
                let closing = Control {
                    operation: Operation::Close,
                    source: self.source.clone(),
                    incarnation: self.incarnation,
                    stream: state.stream,
                    ticket: previous.receipt.ticket,
                };
                retire(
                    previous,
                    sender,
                    callbacks,
                    Route {
                        destination,
                        path: &frame.path,
                        hops: frame.header.hop_limit,
                    },
                    &closing,
                    deadline,
                )
                .await?;
            }
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
                    sender: sender.clone(),
                    renew_at: Instant::now(),
                });
                control.operation = Operation::Commit;
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
                    return Err(ErrCode::ProtocolViolation
                        .to_actor_error("ordered commit receipt differs from challenge".into()));
                }
                receipt = committed;
            }
            if state.grant.as_ref().is_none_or(|grant| {
                grant.receipt.ticket != receipt.ticket || grant.receipt.receiver != receipt.receiver
            }) {
                state.sequence = 0;
            }
            state.grant = Some(Grant {
                receipt,
                sender: sender.clone(),
                renew_at: Instant::now()
                    + Duration::from_millis((receipt.lease_ms / 2).max(1) as u64),
            });
        }
        let sequence = state.sequence.checked_add(1).ok_or_else(|| {
            ErrCode::Overloaded.to_actor_error("ordered sequence exhausted".into())
        })?;
        let envelope = Data {
            source: self.source.clone(),
            incarnation: self.incarnation,
            stream: state.stream,
            ticket: state
                .grant
                .as_ref()
                .expect("confirmed grant")
                .receipt
                .ticket,
            sequence,
            key: frame.type_key,
            payload: frame.payload,
        };
        frame.type_key = DATA_KEY.into();
        frame.payload = envelope
            .encode()
            .map_err(|detail| ErrCode::ProtocolViolation.to_actor_error(detail))?;
        frame.header.seq = 0;
        let frame = sender
            .trace_request(frame)
            .map_err(|error| error.to_actor_error())?;
        let permit = sender
            .reserve(&frame)
            .await
            .map_err(|error| error.to_actor_error())?;
        // No await separates assigning a sequence from publication.
        state.sequence = sequence;
        state.sent_ordered = true;
        permit.send(frame);
        Ok(None)
    }

    pub async fn close_lane(
        &self,
        destination: &str,
        resolve_sender: impl std::future::Future<Output = Option<FrameSender>>,
        callbacks: &CallbackRegistry,
        path: &str,
        deadline: Instant,
    ) -> ActorResult<()> {
        let lane = {
            let lanes = self.lanes.lock().unwrap();
            if self.closed.load(Ordering::Acquire) {
                return Err(ActorError::Stopped);
            }
            lanes.get(destination).cloned()
        };
        let Some(lane) = lane else {
            return Ok(());
        };
        let mut usage = LaneUse::new(self, destination, lane);
        let lane = usage.lane.as_ref().expect("live lane handle");
        crate::transport::notify_safely(async {
            tokio::time::timeout_at(deadline, async {
                let mut state = lane.lock().await;
                usage.retained = state.grant.is_some() || state.sent_ordered;
                if state.closed {
                    return Ok(());
                }
                if let Some(grant) = &state.grant {
                    let current = resolve_sender.await;
                    let sender = current.as_ref().unwrap_or(&grant.sender);
                    let control = Control {
                        operation: Operation::Close,
                        source: self.source.clone(),
                        incarnation: self.incarnation,
                        stream: state.stream,
                        ticket: grant.receipt.ticket,
                    };
                    retire(
                        grant,
                        sender,
                        callbacks,
                        Route {
                            destination,
                            path,
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
                    .get(destination)
                    .is_some_and(|entry| Arc::ptr_eq(entry, lane))
                {
                    lanes.remove(destination);
                }
                Ok(())
            })
            .await
            .unwrap_or(Err(ActorError::Timeout))
        })
        .await
    }
}

fn legacy_fallback(frame: Frame, requires_ordered: bool) -> ActorResult<Option<Frame>> {
    if requires_ordered {
        return Err(ErrCode::NotRemotable
            .to_actor_error("route does not support ordered TELL sessions".into()));
    }
    Ok(Some(frame))
}

struct Route<'a> {
    destination: &'a str,
    path: &'a str,
    hops: u8,
}

async fn retire(
    grant: &Grant,
    current: &FrameSender,
    callbacks: &CallbackRegistry,
    route: Route<'_>,
    control: &Control,
    deadline: Instant,
) -> ActorResult<()> {
    if let Some(epoch) = grant
        .receipt
        .epoch
        .filter(|_| current.supports_ordered_recovery())
    {
        let mut recovery = control.clone();
        recovery.operation = Operation::Recover {
            receiver: grant.receipt.receiver,
            epoch,
        };
        match request(
            current,
            callbacks,
            route.destination,
            route.path,
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
    let sender = if grant.sender.is_closed() {
        current
    } else {
        &grant.sender
    };
    let receipt = request(
        sender,
        callbacks,
        route.destination,
        route.path,
        route.hops,
        control,
        deadline,
    )
    .await
    .map_err(|(code, detail)| code.to_actor_error(detail))?;
    if !grant.retired_by(&receipt) {
        return Err(
            ErrCode::ProtocolViolation.to_actor_error("ordered retirement identity differs".into())
        );
    }
    Ok(())
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
                Ok(ReplyPayload::Ok(bytes, key)) if key == RECEIPT_KEY => {
                    let receipt = Receipt::decode(&bytes).map_err(|detail| (ErrCode::ProtocolViolation, detail))?;
                    if receipt.epoch.is_some() && !sender.supports_ordered_recovery() {
                        return Err((ErrCode::ProtocolViolation, "unnegotiated ordered recovery receipt".into()));
                    }
                    Ok(receipt)
                },
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

    #[tokio::test(start_paused = true)]
    async fn pending_sends_and_close_hold_the_slot_until_the_last_user_cancels() {
        let outbound = Outbound::new("source".into(), 1);
        let callbacks = CallbackRegistry::default();
        let (tx, mut frames) = tokio::sync::mpsc::channel(4);
        let sender = FrameSender::capability_test_sender(tx, "hub", caps::BIN | caps::ORDERED_TELL);
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
            std::future::ready(Some(sender.clone())),
            &callbacks,
            "/target",
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
