//! Opt-in, monotonic phase timing. Never infer commit or delivery from a duration.
use std::time::Instant;

pub(crate) const TARGET: &str = "parrot::durable::timing";

/// An unfinished phase may have been cancelled or left by an early return.
/// Explicit terminal results must be recorded before the owner is retired.
pub(crate) struct Phase {
    span: tracing::Span,
    name: &'static str,
    started: Option<Instant>,
    outcome: &'static str,
}
impl Phase {
    pub(crate) fn new(span: tracing::Span, name: &'static str) -> Self {
        let started = (tracing::enabled!(target: TARGET, tracing::Level::TRACE)
            && !span.is_disabled())
        .then(Instant::now);
        if started.is_some() {
            span.in_scope(|| {
                tracing::trace!(target: TARGET, phase = name, boundary = "start");
            });
        }
        Self {
            span,
            name,
            started,
            outcome: "unfinished",
        }
    }
    pub(crate) fn finish(mut self, success: bool) {
        self.outcome = if success { "ok" } else { "error" };
    }
}
impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            let elapsed_us = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
            self.span.in_scope(|| {
                tracing::trace!(target: TARGET, phase = self.name, boundary = "finish",
                    outcome = self.outcome, elapsed_us);
            });
        }
    }
}
