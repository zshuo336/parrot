use super::{callback, drop_catching, failed_v2, v2_err, R};
use futures::FutureExt;
use parrot_api::trace::{self, SpanGuard, SpanKind, SpanStatus, TraceContext};
use std::{future::Future, panic::AssertUnwindSafe, pin::Pin};

// Retain the future separately from its poll boundary so its destructor cannot
// replace a caught panic or escape during task cancellation.
pub(super) struct Operation<F> {
    work: Option<Pin<Box<F>>>,
    guard: Option<SpanGuard>,
    pub(super) context: Option<TraceContext>,
    pub(super) permit: Option<tokio::sync::OwnedSemaphorePermit>,
    callbacks: Option<std::sync::Arc<callback::Cleanup>>,
}
impl<F: Future<Output = R>> Operation<F> {
    pub(super) fn new(work: F, target: &'static str) -> Self {
        Self::with_kind(work, target, SpanKind::ManagedTask, None)
    }

    pub(super) fn query(
        work: F,
        target: &'static str,
        callbacks: std::sync::Arc<callback::Cleanup>,
    ) -> Self {
        Self::with_kind(work, target, SpanKind::Application, Some(callbacks))
    }

    fn with_kind(
        work: F,
        target: &'static str,
        kind: SpanKind,
        callbacks: Option<std::sync::Arc<callback::Cleanup>>,
    ) -> Self {
        let context = TraceContext::current().map(|context| context.child());
        let guard = trace::in_scope(context.clone(), || SpanGuard::current(kind, target));
        Self {
            work: Some(Box::pin(work)),
            guard: Some(guard),
            context,
            permit: None,
            callbacks,
        }
    }

    pub(super) async fn run(&mut self, req_id: u64) -> R {
        let reply = match AssertUnwindSafe(self.work.as_mut().expect("owned operation"))
            .catch_unwind()
            .await
        {
            Ok(reply) => reply,
            Err(payload) => {
                drop_catching(payload);
                self.retire(SpanStatus::Panicked);
                return panicked(req_id);
            }
        };
        self.finish_reply(reply)
    }

    pub(super) fn finish_reply(&mut self, reply: R) -> R {
        let status = match &reply {
            R::Failed { code, .. } if *code == v2_err::DRAIN_TIMEOUT => SpanStatus::Cancelled,
            R::Failed { .. } | R::Drained { aborted: 1.., .. } => SpanStatus::Error,
            _ => SpanStatus::Ok,
        };
        if self.retire(status) {
            reply
        } else {
            panicked(reply.req_id())
        }
    }
}
impl<F> Operation<F> {
    fn retire(&mut self, status: SpanStatus) -> bool {
        trace::in_scope(self.context.clone(), || {
            let clean = drop_catching(self.work.take());
            if let Some(guard) = self.guard.take() {
                let callbacks_failed = self
                    .callbacks
                    .as_ref()
                    .is_some_and(|cleanup| cleanup.failed());
                guard.finish(
                    if !clean || callbacks_failed && status == SpanStatus::Cancelled {
                        SpanStatus::Panicked
                    } else if callbacks_failed && status == SpanStatus::Ok {
                        SpanStatus::Error
                    } else {
                        status
                    },
                );
            }
            clean
        })
    }
}
impl<F> Drop for Operation<F> {
    fn drop(&mut self) {
        self.retire(if std::thread::panicking() {
            SpanStatus::Panicked
        } else {
            SpanStatus::Cancelled
        });
    }
}
fn panicked(req_id: u64) -> R {
    failed_v2(
        req_id,
        v2_err::SPAWN_FAILED,
        "node management operation panicked",
    )
}
