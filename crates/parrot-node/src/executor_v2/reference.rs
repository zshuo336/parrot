//! Shared ownership without invoking user clone_boxed, with one final-drop boundary.
use parrot_api::address::ActorRef;
use std::sync::Arc;

pub(super) struct ReferenceOwner {
    path: String,
    value: Option<Arc<dyn ActorRef>>,
}
impl ReferenceOwner {
    pub(super) fn new(path: String, value: Arc<dyn ActorRef>) -> Arc<Self> {
        Arc::new(Self {
            path,
            value: Some(value),
        })
    }
    // Report disposal when this operation holds the final snapshot. Concurrent
    // status snapshots may keep the reference alive; their final Drop logs any
    // later failure without delaying an already confirmed actor retirement.
    pub(super) fn release(owner: Arc<Self>) -> bool {
        match Arc::try_unwrap(owner) {
            Ok(mut owner) => drop_catching(owner.value.take()),
            Err(_) => true,
        }
    }
}
impl std::ops::Deref for ReferenceOwner {
    type Target = dyn ActorRef;
    fn deref(&self) -> &Self::Target {
        self.value.as_deref().expect("owned Node reference")
    }
}
impl Drop for ReferenceOwner {
    fn drop(&mut self) {
        if !drop_catching(self.value.take()) {
            tracing::warn!(path = %self.path, "Node reference destructor panicked");
        }
    }
}

pub(super) fn drop_catching<T>(value: T) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value))) {
        Ok(()) => true,
        Err(payload) => {
            // Bound even a throwing panic_any payload. Never forget the actor
            // reference itself; only a second throwing payload is leaked.
            if let Err(nested) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                std::mem::forget(nested);
            }
            false
        }
    }
}
