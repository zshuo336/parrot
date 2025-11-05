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
            state.slots.remove(&self.request_id);
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
        self.state.lock().unwrap().slots.remove(&id).is_some()
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
                tx,
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
        self.state
            .lock()
            .unwrap()
            .slots
            .remove(&reply.request_id())
            .is_some_and(|slot| slot.tx.send(reply).is_ok())
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
        let _ = slot.tx.send(reply(id));
        true
    }
    pub fn fail_connection(&self, connection: &FrameSender, detail: &str) {
        let mut state = self.state.lock().unwrap();
        let ids: Vec<_> = state
            .slots
            .iter()
            .filter(|(_, slot)| {
                slot.connection
                    .as_ref()
                    .is_some_and(|owner| owner.same_connection(connection))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let slot = state.slots.remove(&id).expect("checked under lock");
            let _ = slot
                .tx
                .send(R::failure(id, ErrCode::ConnectionLost, detail));
        }
    }

    pub fn fail_all(&self, detail: &str) {
        Self::fail_slots(&mut self.state.lock().unwrap(), detail);
    }
    pub fn close(&self, detail: &str) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        Self::fail_slots(&mut state, detail);
    }
    fn fail_slots(state: &mut State<R>, detail: &str) {
        for (id, slot) in state.slots.drain() {
            let _ = slot
                .tx
                .send(R::failure(id, ErrCode::ConnectionLost, detail));
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
