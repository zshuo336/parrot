//! Owner-thread actors: only protocol messages and references cross threads.
//!
//! `#[derive(ParrotTypedActor)]` with `#[ParrotTypedActor(msgs(Query), local)]`
//! generates `LocalTypedDispatch` and protocol mappings for `LocalProtocol<A>`.
//! It does not make the actor Send or introduce a new executor. Use the existing
//! owner-thread engine entry point; remote-thread callers send ordinary Send
//! messages through `LocalTypedActorRef`. Add `lifecycle` explicitly for hooks.
//!
//! Duplicate local options, including across attributes, are rejected:
//! ```compile_fail
//! use parrot_api::{Message, ParrotTypedActor};
//! pub struct Query;
//! impl Message for Query { type Result = (); }
//! #[derive(ParrotTypedActor)]
//! #[ParrotTypedActor(msgs(Query), local)]
//! #[ParrotTypedActor(local)]
//! struct Service;
//! impl parrot_api::local_typed::LocalTypedReceive<Query> for Service {}
//! ```
//!
//! Local lifecycle opt-in requires its matching trait:
//! ```compile_fail
//! use parrot_api::{Message, ParrotTypedActor};
//! pub struct Query;
//! impl Message for Query { type Result = (); }
//! #[derive(ParrotTypedActor)]
//! #[ParrotTypedActor(msgs(Query), local, lifecycle)]
//! struct Service;
//! impl parrot_api::local_typed::LocalTypedReceive<Query> for Service {}
//! ```
//!
//! A local actor's protocol marker does not permit moving its state to a Send
//! executor or using the ordinary Send dispatcher:
//! ```compile_fail
//! use parrot_api::{Message, ParrotTypedActor};
//! pub struct Query;
//! impl Message for Query { type Result = (); }
//! #[derive(ParrotTypedActor)]
//! #[ParrotTypedActor(msgs(Query), local)]
//! struct Service(std::rc::Rc<()>);
//! impl parrot_api::local_typed::LocalTypedReceive<Query> for Service {}
//! fn needs_send_dispatch<A: parrot_api::typed::ParrotTypedDispatch>() {}
//! needs_send_dispatch::<Service>();
//! ```
//!
//! Local state does not relax the message boundary:
//! ```compile_fail
//! use parrot_api::{Message, ParrotTypedActor};
//! pub struct Query(std::rc::Rc<()>);
//! impl Message for Query { type Result = (); }
//! #[derive(ParrotTypedActor)]
//! #[ParrotTypedActor(msgs(Query), local)]
//! struct Service;
//! impl parrot_api::local_typed::LocalTypedReceive<Query> for Service {}
//! ```
use crate::{
    errors::ActorError,
    typed::{ParrotTypedDispatch, TerminationProtocol},
    typed_context::TypedContext,
    types::{ActorResult, BoxedFuture},
};
use futures::{Stream, StreamExt};
use std::{
    any::Any, collections::VecDeque, future::Future, marker::PhantomData, pin::Pin, task::Poll,
};

/// A future polled and disposed on the actor's owning thread.
pub type LocalBoxedFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Maximum number of source streams admitted to one Local actor generation.
/// Registration happens during initialization, so this bounds construction
/// memory without introducing a cross-engine quota reservation.
pub const DEFAULT_MAX_LOCAL_STREAMS: usize = 1024;

/// A stream registry that is deliberately confined to one Local actor owner.
/// The stream and its items do not need `Send`; the registry is never placed in
/// the typed mailbox or a Tokio task that may migrate to another thread.
pub struct LocalStreamRegistry {
    entries: VecDeque<LocalStreamEntry>,
    admitted: std::rc::Rc<std::cell::Cell<usize>>,
    next_id: u64,
}

type LocalSource = Pin<Box<dyn Stream<Item = Box<dyn Any + 'static>> + 'static>>;

pub struct LocalStreamEntry {
    trace_context: Option<crate::trace::TraceContext>,
    recording: Option<crate::trace::SpanGuard>,
    outcome: crate::trace::SpanStatus,
    admitted: std::rc::Rc<std::cell::Cell<usize>>,
    id: u64,
    started: bool,
    stream: Option<LocalSource>,
}

impl Drop for LocalStreamEntry {
    fn drop(&mut self) {
        let result = crate::trace::in_scope(self.trace_context.clone(), || {
            crate::owned_input::call(|| drop(self.stream.take()))
        });
        if result.is_err() {
            self.outcome = crate::trace::SpanStatus::Panicked;
        }
        if let Some(recording) = self.recording.take() {
            recording.finish(self.outcome);
        }
        // Checked-out events retain admission until the source has retired.
        self.admitted.set(self.admitted.get() - 1);
        // A discarded event may already be unwinding from its item's destructor.
        // Preserve that original failure after independently retiring the source.
        if std::thread::panicking() {
            return;
        }
        if let Err(ActorError::Panic(message)) = result {
            std::panic::resume_unwind(Box::new(message));
        }
    }
}

pub enum LocalStreamEvent {
    Started {
        id: u64,
        entry: LocalStreamEntry,
    },
    Item {
        id: u64,
        item: Box<dyn Any + 'static>,
        entry: LocalStreamEntry,
    },
    Finished {
        id: u64,
        entry: LocalStreamEntry,
    },
    Error {
        id: u64,
        error: crate::errors::ActorError,
        entry: LocalStreamEntry,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalStreamEventKind {
    Started,
    Item,
    Finished,
    Error,
}

impl LocalStreamEvent {
    /// Registration identity of the source. A custom consumer must establish
    /// a fresh child scope for each actual callback; reading does not enter it.
    pub fn trace_context(&self) -> Option<crate::trace::TraceContext> {
        match self {
            Self::Started { entry, .. }
            | Self::Item { entry, .. }
            | Self::Finished { entry, .. }
            | Self::Error { entry, .. } => entry.trace_context.clone(),
        }
    }

    pub fn id(&self) -> u64 {
        match self {
            Self::Started { id, .. }
            | Self::Item { id, .. }
            | Self::Finished { id, .. }
            | Self::Error { id, .. } => *id,
        }
    }

    pub fn kind(&self) -> LocalStreamEventKind {
        match self {
            Self::Started { .. } => LocalStreamEventKind::Started,
            Self::Item { .. } => LocalStreamEventKind::Item,
            Self::Finished { .. } => LocalStreamEventKind::Finished,
            Self::Error { .. } => LocalStreamEventKind::Error,
        }
    }

    pub fn take_item(&mut self) -> Option<Box<dyn Any + 'static>> {
        match self {
            Self::Item { item, .. } => Some(std::mem::replace(item, Box::new(()))),
            _ => None,
        }
    }

    pub fn take_error(&mut self) -> Option<crate::errors::ActorError> {
        match self {
            Self::Error { error, .. } => Some(std::mem::replace(
                error,
                crate::errors::ActorError::InternalError("local stream error consumed".into()),
            )),
            _ => None,
        }
    }

    /// Attach an actual callback failure to the source lifetime. Callback spans
    /// remain independent siblings; success never erases an earlier source or
    /// callback failure. No user formatting or destruction runs here.
    pub fn record_callback_result(&mut self, result: &ActorResult<()>) {
        let entry = match self {
            Self::Started { entry, .. }
            | Self::Item { entry, .. }
            | Self::Finished { entry, .. }
            | Self::Error { entry, .. } => entry,
        };
        if let Err(error) = result {
            if matches!(error, ActorError::Panic(_)) {
                entry.outcome = crate::trace::SpanStatus::Panicked;
            } else if entry.outcome != crate::trace::SpanStatus::Panicked {
                entry.outcome = crate::trace::SpanStatus::Error;
            }
        }
    }

    /// Return an event to its original registry, or retire it. Requeueing into
    /// another generation panics, retiring the source without changing either
    /// registry's admission accounting. Only a completed Finished event marks
    /// successful exhaustion; dropping an uncompleted event is cancellation.
    pub fn complete(self, registry: &mut LocalStreamRegistry, requeue: bool) {
        let entry = match self {
            Self::Item {
                item, mut entry, ..
            } => {
                // Keep source cleanup outside the item's unwind boundary.
                if let Err(ActorError::Panic(message)) = crate::owned_input::call(|| drop(item)) {
                    entry.outcome = crate::trace::SpanStatus::Panicked;
                    std::panic::resume_unwind(Box::new(message));
                }
                entry
            }
            Self::Error { error, entry, .. } => {
                drop(error);
                entry
            }
            Self::Finished { mut entry, .. } => {
                if !requeue && entry.outcome == crate::trace::SpanStatus::Cancelled {
                    entry.outcome = crate::trace::SpanStatus::Ok;
                }
                entry
            }
            Self::Started { entry, .. } => entry,
        };
        if requeue {
            registry.requeue(entry);
        }
    }
}

impl LocalStreamRegistry {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            admitted: Default::default(),
            next_id: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Registering stores ownership only. It never polls the source.
    pub fn add_stream<S>(&mut self, stream: S) -> Result<(), &'static str>
    where
        S: Stream + 'static,
        S::Item: 'static,
    {
        if self.admitted.get() >= DEFAULT_MAX_LOCAL_STREAMS {
            return Err("local stream capacity exhausted");
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let erased = Box::pin(stream.map_local_item());
        self.admitted.set(self.admitted.get() + 1);
        self.entries.push_back(LocalStreamEntry {
            trace_context: crate::trace::TraceContext::current(),
            recording: Some(crate::trace::SpanGuard::child(
                crate::trace::SpanKind::ManagedTask,
                "context.stream.local",
            )),
            outcome: crate::trace::SpanStatus::Cancelled,
            admitted: self.admitted.clone(),
            id,
            started: false,
            stream: Some(erased),
        });
        Ok(())
    }

    pub(crate) fn requeue(&mut self, entry: LocalStreamEntry) {
        assert!(
            std::rc::Rc::ptr_eq(&self.admitted, &entry.admitted),
            "local stream event belongs to another registry"
        );
        self.entries.push_back(entry);
    }

    /// Fairly polls each ready source once per wake. A pending source is
    /// rotated behind the other sources, so one source cannot monopolize the
    /// owner thread and an all-pending set remains a real Pending future.
    pub fn next_event(&mut self) -> LocalBoxedFuture<'_, Option<LocalStreamEvent>> {
        Box::pin(futures::future::poll_fn(move |cx| {
            let count = self.entries.len();
            if count == 0 {
                return Poll::Ready(None);
            }
            for _ in 0..count {
                let mut entry = self.entries.pop_front().expect("counted local stream");
                if !entry.started {
                    entry.started = true;
                    return Poll::Ready(Some(LocalStreamEvent::Started {
                        id: entry.id,
                        entry,
                    }));
                }
                let polled = crate::trace::in_scope(entry.trace_context.clone(), || {
                    crate::owned_input::call(|| {
                        entry
                            .stream
                            .as_mut()
                            .expect("live local stream")
                            .as_mut()
                            .poll_next(cx)
                    })
                });
                let polled = match polled {
                    Ok(polled) => polled,
                    Err(payload) => {
                        entry.outcome = crate::trace::SpanStatus::Panicked;
                        return Poll::Ready(Some(LocalStreamEvent::Error {
                            id: entry.id,
                            error: payload,
                            entry,
                        }));
                    }
                };
                match polled {
                    Poll::Ready(Some(item)) => {
                        return Poll::Ready(Some(LocalStreamEvent::Item {
                            id: entry.id,
                            item,
                            entry,
                        }));
                    }
                    Poll::Ready(None) => {
                        return Poll::Ready(Some(LocalStreamEvent::Finished {
                            id: entry.id,
                            entry,
                        }));
                    }
                    Poll::Pending => self.entries.push_back(entry),
                }
            }
            Poll::Pending
        }))
    }
}

impl Default for LocalStreamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Owner-thread context available to Local actors during initialization.
pub struct LocalStreamContext<'a> {
    registry: &'a mut LocalStreamRegistry,
}

impl<'a> LocalStreamContext<'a> {
    pub fn new(registry: &'a mut LocalStreamRegistry) -> Self {
        Self { registry }
    }

    pub fn add_stream<S>(&mut self, stream: S) -> Result<(), &'static str>
    where
        S: Stream + 'static,
        S::Item: 'static,
    {
        self.registry.add_stream(stream)
    }
}

trait LocalItemMap: Stream {
    fn map_local_item(self) -> Pin<Box<dyn Stream<Item = Box<dyn Any + 'static>> + 'static>>;
}

impl<S> LocalItemMap for S
where
    S: Stream + 'static,
    S::Item: 'static,
{
    fn map_local_item(self) -> Pin<Box<dyn Stream<Item = Box<dyn Any + 'static>> + 'static>> {
        Box::pin(self.map(|item| Box::new(item) as Box<dyn Any + 'static>))
    }
}

/// A typed handler whose actor and future remain on their owner thread.
/// Use `#[ParrotTypedActor(msgs(Query), local)]` to generate its protocol.
/// Messages and replies still implement [`crate::message::Message`]'s Send
/// contract; local state is constructed inside the engine's Send factory.
pub trait LocalTypedReceive<M: crate::message::Message>: 'static {
    fn receive_typed<'a>(
        &'a mut self,
        _message: M,
    ) -> LocalBoxedFuture<'a, ActorResult<M::Result>> {
        Box::pin(async {
            Err(crate::errors::ActorError::MessageHandlingError(
                "this local protocol requires a managed typed context".into(),
            ))
        })
    }
    fn receive_typed_with_context<'a>(
        &'a mut self,
        message: M,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<M::Result>> {
        self.receive_typed(message)
    }
}

/// Explicit lifecycle opt-in for `#[ParrotTypedActor(msgs(Query), local, lifecycle)]`.
/// Hooks may hold non-Send values across await. Contextual defaults forward to
/// the context-free hooks. Without `lifecycle`, the derive does not call this
/// trait or similarly named inherent methods. Engine cleanup and error semantics
/// are the same as a hand-written [`LocalTypedDispatch`] implementation.
pub trait LocalTypedLifecycle: 'static {
    fn init(&mut self) -> LocalBoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn before_stop(&mut self) -> LocalBoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn init_with_context<'a>(
        &'a mut self,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        <Self as LocalTypedLifecycle>::init(self)
    }
    fn before_stop_with_context<'a>(
        &'a mut self,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        <Self as LocalTypedLifecycle>::before_stop(self)
    }
}

/// An actor constructed, initialized, polled and destroyed on one owner thread.
/// Its mailbox protocol remains Send, so ordinary remote-thread callers can use
/// its reference. Actor state and handler futures may contain Rc or RefCell.
/// A constructor sent to the engine must itself be Send; construct local state
/// inside that callback rather than moving an existing local actor across threads.
pub trait LocalTypedDispatch: 'static {
    type Msg: Send + 'static;
    type Reply: Send + 'static;
    fn termination_protocol() -> Option<TerminationProtocol<Self::Msg>> {
        None
    }
    fn init<'a>(&'a mut self, _context: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    /// Register owner-thread streams after ordinary initialization. The
    /// default preserves existing actors and registers nothing.
    fn init_local_streams<'a>(
        &'a mut self,
        _context: &'a TypedContext,
        _streams: &'a mut LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn local_stream_started<'a>(
        &'a mut self,
        _id: u64,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn local_stream_item<'a>(
        &'a mut self,
        _id: u64,
        _item: Box<dyn Any + 'static>,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn local_stream_finished<'a>(
        &'a mut self,
        _id: u64,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn local_stream_error<'a>(
        &'a mut self,
        _id: u64,
        _error: ActorError,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn before_stop<'a>(
        &'a mut self,
        _context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn dispatch<'a>(
        &'a mut self,
        message: Self::Msg,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<Self::Reply>>;
}

/// A local actor's mapping for one message and its reply.
///
/// Derive implements this on the actor, allowing protocols to include messages
/// from other crates (such as ActorTerminated) without violating Rust's orphan
/// rules. The API forwards the mapping to `ParrotMsgVariant<LocalProtocol<A>>`.
/// Hand-written message-side mappings remain supported when this trait is not
/// implemented for that actor/message pair; use one mapping style per pair.
pub trait LocalMsgVariant<M: crate::message::Message>: LocalTypedDispatch {
    fn inject(message: M) -> Self::Msg;
    fn extract(reply: Self::Reply) -> ActorResult<M::Result>;
}

impl<A, M> crate::typed::ParrotMsgVariant<LocalProtocol<A>> for M
where
    A: LocalMsgVariant<M>,
    M: crate::message::Message,
{
    fn inject(message: M) -> A::Msg {
        <A as LocalMsgVariant<M>>::inject(message)
    }
    fn extract(reply: A::Reply) -> ActorResult<M::Result> {
        <A as LocalMsgVariant<M>>::extract(reply)
    }
}

/// Existing Send dispatchers can also be constructed on their owner's thread.
impl<A: ParrotTypedDispatch> LocalTypedDispatch for A {
    type Msg = A::Msg;
    type Reply = A::Reply;
    fn termination_protocol() -> Option<TerminationProtocol<Self::Msg>> {
        A::termination_protocol()
    }
    fn init<'a>(&'a mut self, context: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.init_with_context(context)
    }
    fn before_stop<'a>(
        &'a mut self,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.before_stop_with_context(context)
    }
    fn dispatch<'a>(
        &'a mut self,
        message: Self::Msg,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<Self::Reply>> {
        self.dispatch_with_context(message, context)
    }
}

/// Protocol identity for a local actor. This marker never contains an actor.
/// Implement `ParrotMsgVariant<LocalProtocol<A>>` for each entry message; the
/// resulting TypedActorRef uses the same admission, deadline and dynamic bridge
/// as other typed references without requiring A to be Send.
pub struct LocalProtocol<A: LocalTypedDispatch>(PhantomData<fn() -> A>);
impl<A: LocalTypedDispatch> ParrotTypedDispatch for LocalProtocol<A> {
    type Msg = A::Msg;
    type Reply = A::Reply;
    fn termination_protocol() -> Option<TerminationProtocol<Self::Msg>> {
        A::termination_protocol()
    }
    fn dispatch<'a>(&'a mut self, _: Self::Msg) -> BoxedFuture<'a, ActorResult<Self::Reply>> {
        // There is no public constructor: this type is a protocol identity only.
        Box::pin(async { unreachable!("local protocol markers are never executed") })
    }
}

/// A Send + Sync protocol reference to state confined to its owning thread.
pub type LocalTypedActorRef<A, M> = crate::typed_channel::TypedActorRef<LocalProtocol<A>, M>;
