//! Scoped ownership of the recorder used by future incoming requests.
use parrot_api::trace::{TraceContext, TraceParent, TraceRecorder};
use std::sync::{Arc, Mutex, Weak};

/// Policy for a request without a negotiated incoming causal parent.
/// Existing parents are preserved under both policies. This does not enable
/// tracing globally or manufacture handler records for rejected requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IncomingTracePolicy {
    #[default]
    PropagateOnly,
    /// Assign an independent root identity to each untraced request. Actual
    /// handlers record their children; the identity itself is not a span record.
    StartRoots,
}

struct Binding {
    recorder: TraceRecorder,
    policy: IncomingTracePolicy,
    identity: Arc<()>,
}
type Slot = Mutex<Option<Binding>>;

#[derive(Default)]
pub(crate) struct TraceRecorderSlot(Arc<Slot>);

impl TraceRecorderSlot {
    pub(crate) fn set(&self, recorder: Option<TraceRecorder>) {
        *self.0.lock().unwrap() = recorder.map(|recorder| Binding {
            recorder,
            policy: IncomingTracePolicy::PropagateOnly,
            identity: Arc::new(()),
        });
    }

    pub(crate) fn attach(
        &self,
        recorder: TraceRecorder,
        policy: IncomingTracePolicy,
    ) -> TraceRecorderAttachment {
        let identity = Arc::new(());
        *self.0.lock().unwrap() = Some(Binding {
            recorder,
            policy,
            identity: identity.clone(),
        });
        TraceRecorderAttachment {
            slot: Arc::downgrade(&self.0),
            identity,
        }
    }

    pub(crate) fn context(&self, parent: Option<TraceParent>) -> Option<TraceContext> {
        match (self.0.lock().unwrap().as_ref(), parent) {
            (Some(binding), Some(parent)) => Some(binding.recorder.remote_parent(parent)),
            (None, Some(parent)) => Some(TraceContext::remote_parent(parent)),
            (Some(binding), None) if binding.policy == IncomingTracePolicy::StartRoots => {
                Some(binding.recorder.root())
            }
            _ => None,
        }
    }
}

/// Keep this value alive while future incoming causal requests should use its
/// recorder. Dropping it detaches only this binding: a subsequent attach or
/// `set_trace_recorder` replaces it permanently and cannot be cleared by an old
/// guard. It does not close the recorder, restore an older binding, cancel work,
/// or keep the remote system alive. Already imported requests retain their context.
#[must_use = "dropping the attachment immediately detaches its recorder"]
pub struct TraceRecorderAttachment {
    slot: Weak<Slot>,
    identity: Arc<()>,
}

impl Drop for TraceRecorderAttachment {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.upgrade() {
            let mut binding = slot.lock().unwrap();
            if binding
                .as_ref()
                .is_some_and(|binding| Arc::ptr_eq(&binding.identity, &self.identity))
            {
                *binding = None;
            }
        }
    }
}
