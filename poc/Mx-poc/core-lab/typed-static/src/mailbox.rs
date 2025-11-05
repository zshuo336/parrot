use crate::{deadline, safe_drop, ProtocolMessage, ProtocolRef, SendError, TypedAskRef};
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) struct Work<M, R> {
    pub message: Option<M>,
    pub reply: Option<Sender<Result<R, String>>>,
    pub deadline: Option<Instant>,
}
impl<M, R> Work<M, R> {
    pub fn complete(&mut self, value: Result<R, String>) {
        if let Some(tx) = self.reply.take() {
            safe_drop(tx.send(value));
        } else {
            safe_drop(value);
        }
    }
}
impl<M, R> Drop for Work<M, R> {
    fn drop(&mut self) {
        if let Some(msg) = self.message.take() {
            safe_drop(msg);
        }
        if let Some(tx) = self.reply.take() {
            safe_drop(tx.send(Err("actor stopped before execution".into())));
        }
    }
}
struct Inbox<M, R> {
    pending: VecDeque<Work<M, R>>,
    outstanding: usize,
    closed: bool,
    retired: bool,
}
pub(crate) struct Mailbox<M, R> {
    inbox: Mutex<Inbox<M, R>>,
    wake: Condvar,
    capacity: usize,
}
pub(crate) trait Control: Send + Sync {
    fn close(&self);
}
impl<M: Send, R: Send> Control for Mailbox<M, R> {
    fn close(&self) {
        self.request_stop();
    }
}
impl<M, R> Mailbox<M, R> {
    pub fn new(capacity: usize) -> Self {
        Self {
            inbox: Mutex::new(Inbox {
                pending: VecDeque::new(),
                outstanding: 0,
                closed: false,
                retired: false,
            }),
            wake: Condvar::new(),
            capacity,
        }
    }
    fn submit(&self, work: Work<M, R>) -> Result<(), (String, Work<M, R>)> {
        let mut inbox = self.inbox.lock().unwrap();
        if inbox.closed {
            return Err(("actor stopped".into(), work));
        }
        if inbox.outstanding >= self.capacity {
            return Err(("mailbox full".into(), work));
        }
        inbox.outstanding += 1;
        inbox.pending.push_back(work);
        drop(inbox);
        self.wake.notify_one();
        Ok(())
    }
    pub fn take(&self) -> Option<Work<M, R>> {
        let mut inbox = self.inbox.lock().unwrap();
        loop {
            if inbox.closed {
                return None;
            }
            if let Some(work) = inbox.pending.pop_front() {
                return Some(work);
            }
            inbox = self.wake.wait(inbox).unwrap();
        }
    }
    pub fn complete_one(&self) {
        self.inbox.lock().unwrap().outstanding -= 1;
    }
    pub fn request_stop(&self) {
        self.inbox.lock().unwrap().closed = true;
        self.wake.notify_all();
    }
    pub fn drain(&self) -> VecDeque<Work<M, R>> {
        let mut inbox = self.inbox.lock().unwrap();
        inbox.closed = true;
        inbox.outstanding = 0;
        std::mem::take(&mut inbox.pending)
    }
    pub fn retired(&self) {
        self.inbox.lock().unwrap().retired = true;
        self.wake.notify_all();
    }
    fn wait_stopped(&self, timeout: Duration) -> Result<(), String> {
        let end = deadline(timeout)?;
        let mut inbox = self.inbox.lock().unwrap();
        while !inbox.retired {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("actor retirement timed out".into());
            }
            let (state, _) = self.wake.wait_timeout(inbox, left).unwrap();
            inbox = state;
        }
        Ok(())
    }
}
struct Producer<M, R> {
    mailbox: Arc<Mailbox<M, R>>,
    path: String,
    generation: u64,
}
impl<M, R> Drop for Producer<M, R> {
    fn drop(&mut self) {
        self.mailbox.request_stop();
    }
}
/// Cloning an address shares its actor identity and state. Last address Drop
/// requests retirement; engine shutdown retires it even while addresses survive.
///
/// Messages and replies are checked at compile time:
/// ```compile_fail
/// use typed_static::{TypedActorRef, TypedAskRef};
/// fn wrong_message(actor: TypedActorRef<u64, u64>) { actor.ask("wrong"); }
/// ```
/// ```compile_fail
/// use typed_static::{TypedActorRef, TypedAskRef};
/// fn wrong_reply(actor: TypedActorRef<u64, u64>) {
///     let reply: String = actor.ask(1).unwrap();
/// }
/// ```
/// ```compile_fail
/// use std::rc::Rc;
/// use typed_static::{SimpleEngine, TypedReceive};
/// struct Echo;
/// impl TypedReceive<Rc<u64>> for Echo {
///     type Reply = u64;
///     fn receive_typed(&mut self, msg: Rc<u64>) -> Result<u64, String> {Ok(*msg)}
/// }
/// let engine = SimpleEngine::new();
/// let actor = engine.actor(Echo, "not-send").unwrap();
/// ```
pub struct TypedActorRef<M, R> {
    producer: Arc<Producer<M, R>>,
}
impl<M, R> Clone for TypedActorRef<M, R> {
    fn clone(&self) -> Self {
        Self {
            producer: self.producer.clone(),
        }
    }
}
impl<M, R> TypedActorRef<M, R> {
    pub(crate) fn new(mailbox: Arc<Mailbox<M, R>>, path: String, generation: u64) -> Self {
        Self {
            producer: Arc::new(Producer {
                mailbox,
                path,
                generation,
            }),
        }
    }
    pub fn path(&self) -> &str {
        &self.producer.path
    }
    pub fn generation(&self) -> u64 {
        self.producer.generation
    }
    pub fn stop(&self) {
        self.producer.mailbox.request_stop();
    }
    /// Waits for user resources and path retirement. Engine shutdown additionally
    /// joins the OS worker. Cannot forcibly interrupt a running callback.
    pub fn wait_stopped(&self, timeout: Duration) -> Result<(), String> {
        self.producer.mailbox.wait_stopped(timeout)
    }
}
impl<M: Send + 'static, R: Send + 'static> TypedActorRef<M, R> {
    pub(crate) fn ask_until(&self, msg: M, end: Instant) -> Result<R, String> {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            safe_drop(msg);
            return Err("ask deadline expired".into());
        }
        let (tx, rx) = channel();
        if let Err((error, work)) = self.producer.mailbox.submit(Work {
            message: Some(msg),
            reply: Some(tx),
            deadline: Some(end),
        }) {
            drop(work);
            return Err(error);
        }
        rx.recv_timeout(end.saturating_duration_since(Instant::now()))
            .map_err(|e| e.to_string())?
    }
    pub fn for_message<N>(&self) -> ProtocolRef<N, M, R>
    where
        N: ProtocolMessage<M, R>,
    {
        ProtocolRef::new(self.clone())
    }
}
impl<M: Send + 'static, R: Send + 'static> TypedAskRef<M> for TypedActorRef<M, R> {
    type Reply = R;
    fn ask_with_timeout(&self, msg: M, timeout: Duration) -> Result<R, String> {
        match deadline(timeout) {
            Ok(end) => self.ask_until(msg, end),
            Err(error) => {
                safe_drop(msg);
                Err(error)
            }
        }
    }
    fn tell(&self, msg: M) -> Result<(), SendError<M>> {
        self.producer
            .mailbox
            .submit(Work {
                message: Some(msg),
                reply: None,
                deadline: None,
            })
            .map_err(|(reason, mut work)| SendError {
                reason,
                message: work.message.take().unwrap(),
            })
    }
}
