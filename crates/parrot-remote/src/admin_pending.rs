//! Bounded, connection-bound pending requests shared by admin protocol versions.
use crate::{
    error::{ErrCode, RemoteError},
    transport::FrameSender,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::oneshot;

#[cfg(test)]
mod notification_contracts;

pub static WRONG_ADMIN_REPLY_SOURCE: AtomicU64 = AtomicU64::new(0);
pub static LATE_ADMIN_REPLY: AtomicU64 = AtomicU64::new(0);

/// Typed administrative reply used by the pending registry.
pub trait AdminResponse: Send + 'static {
    fn request_id(&self) -> u64;
    fn failure(request_id: u64, code: ErrCode, detail: &str) -> Self;
}

pub struct Pending<R> {
    state: Mutex<State<R>>,
    capacity: usize,
}
struct State<R> {
    closed: bool,
    slots: HashMap<u64, Slot<R>>,
}
struct Slot<R> {
    tx: oneshot::Sender<R>,
    connection: Option<FrameSender>,
    identity: Arc<()>,
}

impl<R> Slot<R> {
    fn notify(self, reply: impl FnOnce() -> Option<R>) -> bool {
        let Self {
            tx,
            connection,
            identity,
        } = self;
        let cleanup = crate::ingress::unwind::Cleanup::default();
        let mut tx = cleanup.owned(tx);
        let sent = match cleanup.call(reply) {
            Ok(Some(reply)) => match cleanup.call(|| tx.take().send(reply)) {
                Ok(Ok(())) => true,
                Ok(Err(reply)) => {
                    cleanup.dispose(reply);
                    false
                }
                // The slot has committed even if its receiver Waker unwinds.
                Err(()) => true,
            },
            Ok(None) | Err(()) => false,
        };
        tx.retire();
        cleanup.dispose(connection);
        cleanup.dispose(identity);
        sent
    }
}

impl<R> Drop for Pending<R> {
    fn drop(&mut self) {
        let slots = std::mem::take(
            &mut self
                .state
                .get_mut()
                .unwrap_or_else(|error| error.into_inner())
                .slots,
        );
        for slot in slots.into_values() {
            slot.notify(|| None);
        }
    }
}

pub(crate) struct PendingRequest<'a, R> {
    registry: &'a Pending<R>,
    request_id: u64,
    identity: Arc<()>,
}
impl<R> Drop for PendingRequest<'_, R> {
    fn drop(&mut self) {
        let mut state = self.registry.state.lock().unwrap();
        if state
            .slots
            .get(&self.request_id)
            .is_some_and(|slot| Arc::ptr_eq(&slot.identity, &self.identity))
        {
            let removed = state.slots.remove(&self.request_id);
            drop(state);
            if let Some(slot) = removed {
                slot.notify(|| None);
            }
        }
    }
}
impl<R> Default for Pending<R> {
    fn default() -> Self {
        Self::new(65536)
    }
}
impl<R> Pending<R> {
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(State {
                closed: false,
                slots: HashMap::new(),
            }),
            capacity,
        }
    }
    pub fn len(&self) -> usize {
        self.state.lock().unwrap().slots.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn remove(&self, id: u64) -> bool {
        let removed = self.state.lock().unwrap().slots.remove(&id);
        if let Some(slot) = removed {
            slot.notify(|| None);
            true
        } else {
            false
        }
    }

    /// Fence admission under an outer lifecycle lock; notify after releasing it.
    pub(crate) fn close_admission(&self) {
        self.state.lock().unwrap().closed = true;
    }

    /// Trusted process-local registration. Unbound slots cannot receive network replies.
    pub fn insert(&self, id: u64, tx: oneshot::Sender<R>) -> Result<(), RemoteError> {
        self.insert_slot(id, tx, None).map(|_| ())
    }
    pub fn insert_on(
        &self,
        id: u64,
        tx: oneshot::Sender<R>,
        connection: &FrameSender,
    ) -> Result<(), RemoteError> {
        self.insert_slot(id, tx, Some(connection.clone()))
            .map(|_| ())
    }
    fn insert_slot(
        &self,
        id: u64,
        tx: oneshot::Sender<R>,
        connection: Option<FrameSender>,
    ) -> Result<Arc<()>, RemoteError> {
        let cleanup = crate::ingress::unwind::Cleanup::default();
        let mut tx = cleanup.owned(tx);
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return Err(RemoteError::Transport("admin registry is shut down".into()));
        }
        if state.slots.contains_key(&id) {
            return Err(RemoteError::Transport("duplicate admin request id".into()));
        }
        if state.slots.len() >= self.capacity {
            return Err(RemoteError::CallbacksFull(self.capacity));
        }
        if connection.as_ref().is_some_and(FrameSender::is_closed) {
            return Err(RemoteError::Transport(
                "admin connection already closed".into(),
            ));
        }
        let identity = Arc::new(());
        state.slots.insert(
            id,
            Slot {
                tx: tx.take(),
                connection,
                identity: identity.clone(),
            },
        );
        Ok(identity)
    }
    pub(crate) fn register_on(
        &self,
        id: u64,
        connection: &FrameSender,
    ) -> Result<(oneshot::Receiver<R>, PendingRequest<'_, R>), RemoteError> {
        let (tx, rx) = oneshot::channel();
        let identity = self.insert_slot(id, tx, Some(connection.clone()))?;
        Ok((
            rx,
            PendingRequest {
                registry: self,
                request_id: id,
                identity,
            },
        ))
    }
}
impl<R: AdminResponse> Pending<R> {
    /// Trusted local completion. Network ingress must use complete_from.
    pub fn complete(&self, reply: R) -> bool {
        let id = reply.request_id();
        let removed = self.state.lock().unwrap().slots.remove(&id);
        removed.is_some_and(|slot| slot.notify(|| Some(reply)))
    }
    pub fn complete_from(&self, reply: R, source: &FrameSender) -> bool {
        self.consume_from(reply.request_id(), source, true, |_| reply)
    }
    pub fn fail_from(&self, id: u64, code: ErrCode, detail: &str, source: &FrameSender) -> bool {
        self.consume_from(id, source, false, |id| R::failure(id, code, detail))
    }
    fn consume_from(
        &self,
        id: u64,
        source: &FrameSender,
        report_missing: bool,
        reply: impl FnOnce(u64) -> R,
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(slot) = state.slots.get(&id) else {
            if report_missing {
                LATE_ADMIN_REPLY.fetch_add(1, Ordering::Relaxed);
            }
            return false;
        };
        if !slot
            .connection
            .as_ref()
            .is_some_and(|owner| owner.same_connection(source))
        {
            WRONG_ADMIN_REPLY_SOURCE.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let slot = state.slots.remove(&id).expect("checked under lock");
        drop(state);
        slot.notify(|| Some(reply(id)));
        true
    }
    pub fn fail_connection(&self, connection: &FrameSender, detail: &str) {
        let selected: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .slots
            .extract_if(|_, slot| {
                slot.connection
                    .as_ref()
                    .is_some_and(|owner| owner.same_connection(connection))
            })
            .collect();
        for (id, slot) in selected {
            slot.notify(|| Some(R::failure(id, ErrCode::ConnectionLost, detail)));
        }
    }

    pub fn fail_all(&self, detail: &str) {
        let slots = std::mem::take(&mut self.state.lock().unwrap().slots);
        Self::fail_slots(slots, detail);
    }
    pub fn close(&self, detail: &str) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        let slots = std::mem::take(&mut state.slots);
        drop(state);
        Self::fail_slots(slots, detail);
    }
    fn fail_slots(slots: HashMap<u64, Slot<R>>, detail: &str) {
        for (id, slot) in slots {
            slot.notify(|| Some(R::failure(id, ErrCode::ConnectionLost, detail)));
        }
    }
}

impl AdminResponse for crate::admin::AdminReply {
    fn request_id(&self) -> u64 {
        self.req_id()
    }
    fn failure(req_id: u64, code: ErrCode, detail: &str) -> Self {
        Self::Failed {
            req_id,
            code: code as u16,
            detail: detail.into(),
        }
    }
}
impl AdminResponse for crate::admin_v2::AdminReplyV2 {
    fn request_id(&self) -> u64 {
        self.req_id()
    }
    fn failure(req_id: u64, code: ErrCode, detail: &str) -> Self {
        Self::Failed {
            req_id,
            code: code as u16,
            detail: detail.into(),
        }
    }
}
