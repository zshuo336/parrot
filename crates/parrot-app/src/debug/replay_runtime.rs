//! Replay decoded requests into an explicitly supplied local actor lookup.
use super::{ReplayError, ReplayLimits, ReplayLog};
use futures::FutureExt;
use parrot_api::types::{BoxedActorRef, BoxedMessage};
use parrot_remote::{codec_registry::CodecRegistry, frame::frame_type, LocalLookup};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Debug, Clone, Copy, Default)]
pub enum ReplayDirection {
    Tx,
    #[default]
    Rx,
}
impl ReplayDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Tx => "tx",
            Self::Rx => "rx",
        }
    }
}
/// How ordinary captured TELLs are executed. Captured TELL_ACK always waits for completion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReplayTellMode {
    /// Preserve ordinary TELL admission semantics; the handler may not have run.
    #[default]
    Admission,
    /// Wait on the actor's completion route and discard its business reply.
    /// The resulting receipt describes this replay, not an original wire ACK.
    Completion,
}
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    pub limits: ReplayLimits,
    /// Replay one side of the trace, so tx/rx copies are never both applied.
    pub direction: ReplayDirection,
    /// Ordinary TELL policy. Library default is admission; the CLI uses completion.
    pub tell_mode: ReplayTellMode,
    /// Explicitly map `parrot://<node>/<path>` for this node to a local actor path.
    /// Other nodes and all URIs without this opt-in are rejected.
    pub target_node: Option<String>,
    /// Each lookup/admission/reply deadline; positive and at most one day.
    pub call_timeout: Duration,
}
impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            limits: ReplayLimits::default(),
            direction: ReplayDirection::Rx,
            tell_mode: ReplayTellMode::Admission,
            target_node: None,
            call_timeout: Duration::from_secs(5),
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct ReplayReply {
    pub seq: u64,
    pub cid: u64,
    /// Zero-based member index for a captured BATCH request.
    pub batch_index: Option<usize>,
    pub type_key: String,
    pub payload: Vec<u8>,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReplayReport {
    /// Successful ASK/completion-TELL calls and admitted ordinary TELLs.
    /// Ordinary TELL admission does not prove handler completion.
    pub delivered: usize,
    pub replies: Vec<ReplayReply>,
}
struct Prepared<'a> {
    record: &'a super::JsonlRecord,
    cid: u64,
    batch_index: Option<usize>,
    completion: bool,
    actor: Owned<BoxedActorRef>,
    message: Owned<BoxedMessage>,
}
fn failed(seq: u64, completed: usize, reason: impl std::fmt::Display) -> ReplayError {
    // ActorError::Other can contain foreign Display and Drop implementations.
    // Borrow it while formatting, then retire it at a separate boundary so a
    // formatting panic cannot unwind through a second throwing destructor.
    let mut text = match std::panic::catch_unwind(AssertUnwindSafe(|| reason.to_string())) {
        Ok(text) => text,
        Err(payload) => {
            crate::disposal::drop_catching(payload);
            "replay error formatting panicked".to_string()
        }
    };
    if !crate::disposal::drop_catching(reason) {
        text.push_str("; error disposal panicked");
    }
    ReplayError::Delivery {
        seq,
        completed,
        reason: text,
    }
}
// Give each replay-owned user value an independent destructor boundary. In
// particular, never catch the destruction of the entire prepared Vec: two
// throwing elements would abort while unwinding the same container.
struct Owned<T> {
    value: Option<T>,
    cleanup_failed: Arc<AtomicBool>,
}
impl<T> Owned<T> {
    fn new(value: T, cleanup_failed: &Arc<AtomicBool>) -> Self {
        Self {
            value: Some(value),
            cleanup_failed: cleanup_failed.clone(),
        }
    }
    fn value(&self) -> &T {
        self.value.as_ref().expect("replay owns this value")
    }
    fn take(&mut self) -> T {
        self.value.take().expect("replay owns this value")
    }
}
impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            if !crate::disposal::drop_catching(value) {
                self.cleanup_failed.store(true, Ordering::Release);
            }
        }
    }
}
// The callbacks return an already pinned BoxedFuture (which is Unpin as a
// handle). Its allocation stays pinned while polling and during destruction.
impl<F: Future + Unpin> Future for Owned<F> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(self.value.as_mut().expect("replay owns this future")).poll(cx)
    }
}
fn failed_panic(
    seq: u64,
    completed: usize,
    reason: &str,
    payload: Box<dyn std::any::Any + Send>,
) -> ReplayError {
    // Merely discarding a catch_unwind payload can itself panic.
    crate::disposal::drop_catching(payload);
    failed(seq, completed, reason)
}
fn check_cleanup(cleanup: &AtomicBool, seq: u64, completed: usize) -> Result<(), ReplayError> {
    if cleanup.load(Ordering::Acquire) {
        Err(failed(
            seq,
            completed,
            "replay owned value disposal panicked",
        ))
    } else {
        Ok(())
    }
}
#[derive(Clone, Copy)]
enum CallKind {
    Lookup,
    Ask,
    Tell,
    Completion,
}
impl CallKind {
    fn timeout(self) -> &'static str {
        match self {
            Self::Lookup => "actor lookup timed out",
            Self::Ask => "ASK timed out; execution may already have occurred",
            Self::Tell => "TELL admission timed out",
            Self::Completion => "completion TELL timed out; execution may have occurred",
        }
    }
    fn panicked(self) -> &'static str {
        match self {
            Self::Lookup => "actor lookup panicked",
            Self::Ask => "ASK panicked; execution may already have occurred",
            Self::Tell => "TELL admission panicked",
            Self::Completion => "completion TELL panicked; execution may have occurred",
        }
    }
}
async fn invoke<F: Future + Unpin>(
    callback: impl FnOnce() -> F,
    cleanup: &Arc<AtomicBool>,
    timeout: Duration,
    seq: u64,
    completed: usize,
    kind: CallKind,
) -> Result<F::Output, ReplayError> {
    // Separate construction, polling and disposal. Wrapping `callback().await`
    // in one async block would lose an already successful result if disposal
    // of the returned user future panics after Poll::Ready.
    let deadline = tokio::time::Instant::now() + timeout;
    // Construction itself is user code and belongs to the same budget. Retain
    // its result before deciding whether an expired deadline wins.
    let mut constructed = Owned::new(
        std::panic::catch_unwind(AssertUnwindSafe(callback)),
        cleanup,
    );
    if tokio::time::Instant::now() >= deadline {
        return Err(failed(seq, completed, kind.timeout()));
    }
    let future = constructed
        .take()
        .map_err(|payload| failed_panic(seq, completed, kind.panicked(), payload))?;
    let mut result = Owned::new(
        tokio::time::timeout_at(
            deadline,
            AssertUnwindSafe(Owned::new(future, cleanup)).catch_unwind(),
        )
        .await,
        cleanup,
    );
    // Tokio Timeout polls work before its timer. A late Ready (or panic) must
    // not erase the caller's positive deadline. A discarded lookup/reference,
    // reply or panic payload still gets its own bounded disposal boundary.
    if tokio::time::Instant::now() >= deadline {
        return Err(failed(seq, completed, kind.timeout()));
    }
    result
        .take()
        .map_err(|_| failed(seq, completed, kind.timeout()))?
        .map_err(|payload| failed_panic(seq, completed, kind.panicked(), payload))
}
impl ReplayLog {
    /// Preflight the whole log, resolve local actors and decode all selected
    /// requests before delivering any. Only ASK/TELL in the selected direction
    /// are delivered; control frames and recorded responses are observations.
    /// URI targets require an explicit target_node mapping; never infer a destination.
    ///
    /// Executes sequentially on a Tokio runtime. Delivery failures stop replay
    /// without rolling back earlier effects. Codec/actor allocations and actor
    /// determinism are application responsibilities, not provided by JSONL.
    /// Callback construction and polling share one positive deadline; a late
    /// ready result cannot turn expiry into success. The budget cannot preempt
    /// synchronous user code, and timeout never implies business rollback.
    /// Each replay-owned reference, message, reply and callback future has an
    /// independent unwind boundary on release, including cancellation. Successful
    /// calls remain counted if later release fails; no following call is made.
    /// This cannot contain panic=abort or multiple panics inside one user value.
    /// Repeatedly throwing panic payload destructors are bounded by retaining
    /// the final throwing payload rather than recursively invoking user code.
    pub async fn replay_local(
        &self,
        lookup: &dyn LocalLookup,
        options: ReplayOptions,
    ) -> Result<ReplayReport, ReplayError> {
        self.validate(options.limits)?;
        if options.call_timeout.is_zero() || options.call_timeout > Duration::from_secs(86_400) {
            return Err(ReplayError::InvalidTimeout);
        }
        let cleanup_failed = Arc::new(AtomicBool::new(false));
        let registry = CodecRegistry::global();
        let mut prepared = Vec::new();
        let mut expanded_bytes = 0usize;
        let mut source = None;
        for record in &self.records {
            if let Some(wire) = &record.wire {
                if source.is_some_and(|node| node != &wire.local_node) {
                    return Err(failed(
                        record.seq,
                        0,
                        "multiple capture nodes require explicit separate replay",
                    ));
                }
                source = Some(&wire.local_node);
            }
        }
        for record in self.records.iter().filter(|r| {
            r.dir == options.direction.as_str()
                && matches!(r.frame_type, frame_type::ASK | frame_type::TELL)
        }) {
            use parrot_remote::frame::{flags, Frame, PROTOCOL_VERSION};
            let mut frame = Frame::tell(
                &record.path,
                &record.type_key,
                record.payload_bytes_with_limits(options.limits)?.into(),
            );
            frame.header.frame_type = record.frame_type;
            frame.header.correlation_id = record.cid;
            if let Some(wire) = &record.wire {
                frame.header.version = wire.version;
                frame.header.flags = wire.flags;
                frame.header.hop_count = wire.hop_count;
                frame.header.hop_limit = wire.hop_limit;
                frame.header.seq = wire.wire_seq;
                if wire.version != PROTOCOL_VERSION {
                    return Err(failed(record.seq, 0, "unsupported wire version"));
                }
                frame
                    .validate_payload_flags()
                    .map_err(|e| failed(record.seq, 0, e))?;
            }
            let members: Box<dyn Iterator<Item = (Option<usize>, Frame)> + Send + '_> =
                if frame.header.flags & flags::BATCH != 0 {
                    Box::new(
                        frame
                            .batch_frames()
                            .map_err(|e| failed(record.seq, 0, e))?
                            .enumerate()
                            .map(|(index, frame)| (Some(index), frame)),
                    )
                } else {
                    Box::new(std::iter::once((None, frame.clone())))
                };
            for (batch_index, member) in members {
                if prepared.len() == options.limits.max_records {
                    return Err(failed(
                        record.seq,
                        0,
                        "expanded request count budget exceeded",
                    ));
                }
                expanded_bytes = expanded_bytes
                    .checked_add(member.payload.len())
                    .and_then(|n| n.checked_add(member.path.len()))
                    .and_then(|n| n.checked_add(member.type_key.len()))
                    .filter(|n| *n <= options.limits.max_file_bytes)
                    .ok_or_else(|| {
                        failed(record.seq, 0, "expanded request byte budget exceeded")
                    })?;
                let path = if let Some(rest) = member.path.strip_prefix("parrot://") {
                    let (node, _) = rest
                        .split_once('/')
                        .ok_or_else(|| failed(record.seq, 0, "URI requires a local actor path"))?;
                    if node.is_empty() || options.target_node.as_deref() != Some(node) {
                        return Err(failed(
                            record.seq,
                            0,
                            "URI requires an explicit matching target_node",
                        ));
                    }
                    &rest[node.len()..]
                } else {
                    &member.path
                };
                if !path.starts_with('/')
                    || path.starts_with("//")
                    || path.chars().any(char::is_control)
                {
                    return Err(failed(
                        record.seq,
                        0,
                        "replay requires a local absolute actor path",
                    ));
                }
                let bytes = if record.wire.is_some() && record.frame_type == frame_type::ASK {
                    member
                        .split_reply_to()
                        .map_err(|error| failed(record.seq, 0, error))?
                        .1
                } else {
                    member
                        .split_trace_parent()
                        .map_err(|error| failed(record.seq, 0, error))?
                        .1
                };
                let message = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    registry.decode_incoming(&member.type_key, &bytes)
                }))
                .map_err(|payload| failed_panic(record.seq, 0, "codec panicked", payload))?
                .map_err(|error| failed(record.seq, 0, error.name()))?;
                let message = Owned::new(message, &cleanup_failed);
                let actor = invoke(
                    || lookup.lookup(path),
                    &cleanup_failed,
                    options.call_timeout,
                    record.seq,
                    0,
                    CallKind::Lookup,
                )
                .await?
                .ok_or_else(|| failed(record.seq, 0, "actor not found"))?;
                let actor = Owned::new(actor, &cleanup_failed);
                check_cleanup(&cleanup_failed, record.seq, 0)?;
                prepared.push(Prepared {
                    record,
                    cid: member.header.correlation_id,
                    batch_index,
                    completion: member.header.flags & flags::TELL_ACK != 0,
                    actor,
                    message,
                });
            }
        }
        let mut report = ReplayReport::default();
        let mut reply_bytes = 0usize;
        for Prepared {
            record,
            cid,
            batch_index,
            completion,
            actor,
            mut message,
        } in prepared
        {
            let completed = report.delivered;
            if completion
                || (record.frame_type == frame_type::TELL
                    && options.tell_mode == ReplayTellMode::Completion)
            {
                let reply = invoke(
                    || {
                        actor
                            .value()
                            .send_with_timeout(message.take(), Some(options.call_timeout))
                    },
                    &cleanup_failed,
                    options.call_timeout,
                    record.seq,
                    completed,
                    CallKind::Completion,
                )
                .await?
                .map_err(|error| failed(record.seq, completed, error))?;
                let reply = Owned::new(reply, &cleanup_failed);
                report.delivered += 1;
                drop(reply);
                check_cleanup(&cleanup_failed, record.seq, report.delivered)?;
                report.replies.push(ReplayReply {
                    seq: record.seq,
                    cid,
                    batch_index,
                    type_key: parrot_remote::frame::TELL_COMPLETION_KEY.into(),
                    payload: Vec::new(),
                });
            } else if record.frame_type == frame_type::TELL {
                invoke(
                    || actor.value().deliver(message.take()),
                    &cleanup_failed,
                    options.call_timeout,
                    record.seq,
                    completed,
                    CallKind::Tell,
                )
                .await?
                .map_err(|error| failed(record.seq, completed, error))?;
                report.delivered += 1;
            } else {
                let reply = invoke(
                    || {
                        actor
                            .value()
                            .send_with_timeout(message.take(), Some(options.call_timeout))
                    },
                    &cleanup_failed,
                    options.call_timeout,
                    record.seq,
                    completed,
                    CallKind::Ask,
                )
                .await?
                .map_err(|error| failed(record.seq, completed, error))?;
                let reply = Owned::new(reply, &cleanup_failed);
                report.delivered += 1;
                check_cleanup(&cleanup_failed, record.seq, report.delivered)?;
                let (type_key, payload) = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    registry.encode_outgoing(reply.value())
                }))
                .map_err(|payload| {
                    failed_panic(
                        record.seq,
                        report.delivered,
                        "reply codec panicked",
                        payload,
                    )
                })?
                .map_err(|error| failed(record.seq, report.delivered, error.name()))?;
                drop(reply);
                check_cleanup(&cleanup_failed, record.seq, report.delivered)?;
                reply_bytes = reply_bytes
                    .checked_add(payload.len())
                    .filter(|n| *n <= options.limits.max_file_bytes)
                    .ok_or_else(|| {
                        failed(record.seq, report.delivered, "reply byte budget exceeded")
                    })?;
                if payload.len() > options.limits.max_payload_bytes {
                    return Err(failed(
                        record.seq,
                        report.delivered,
                        "reply payload budget exceeded",
                    ));
                }
                report.replies.push(ReplayReply {
                    seq: record.seq,
                    cid,
                    batch_index,
                    type_key,
                    payload,
                });
            }
            // A completed call remains completed even if releasing its original
            // reference fails. Do not deliver the next request after that failure.
            drop(actor);
            drop(message);
            check_cleanup(&cleanup_failed, record.seq, report.delivered)?;
        }
        Ok(report)
    }
}
