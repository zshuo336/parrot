//! Bound unwinding while retiring one user callback or captured panic payload.
pub(super) fn drop_catching<T>(value: T) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value))) {
        Ok(()) => true,
        Err(payload) => {
            if let Err(nested) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
            {
                // Repeatedly throwing user payloads must not recurse forever.
                // This cannot contain a double panic inside one user value.
                std::mem::forget(nested);
            }
            false
        }
    }
}

pub(super) struct Transition<T> {
    value: Option<T>,
    context: Option<parrot_api::trace::TraceContext>,
}
impl<T> Transition<T> {
    pub(super) fn new(value: T) -> Self {
        Self {
            value: Some(value),
            context: parrot_api::trace::TraceContext::current(),
        }
    }
    pub(super) fn value(&self) -> &T {
        self.value.as_ref().expect("inbox owns its transition")
    }
    pub(super) fn retire(&mut self) -> bool {
        parrot_api::trace::in_scope(self.context.clone(), || {
            self.value.take().is_none_or(drop_catching)
        })
    }
}
impl<T> Drop for Transition<T> {
    fn drop(&mut self) {
        self.retire();
    }
}
