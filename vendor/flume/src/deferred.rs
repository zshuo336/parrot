//! Commit a nonblocking send before invoking receiver callbacks.
use super::*;

/// Receiver notifications retained by [`Sender::try_send_deferred`].
///
/// Dropping this value delivers the notifications. Keep it alive until any
/// enclosing admission lock has been released. The common case of one receiver
/// reuses its existing registration allocation; only multiple waiters allocate
/// an additional list.
#[must_use = "release application locks before delivering deferred receiver wakes"]
pub struct DeferredSendWakes<T> {
    first: Option<Arc<Hook<T, dyn Signal>>>,
    rest: Vec<Arc<Hook<T, dyn Signal>>>,
}

impl<T> DeferredSendWakes<T> {
    pub(crate) fn new() -> Self {
        Self {
            first: None,
            rest: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, hook: Arc<Hook<T, dyn Signal>>) {
        hook.signal().prepare();
        if self.first.is_none() {
            self.first = Some(hook);
        } else {
            self.rest.push(hook);
        }
    }
}

impl<T> Drop for DeferredSendWakes<T> {
    fn drop(&mut self) {
        let mut failure = None;
        for hook in self.first.take().into_iter().chain(self.rest.drain(..)) {
            let wake = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hook.signal().fire();
            }));
            // A cancelled future may leave this guard as the final owner of
            // a user waker. Isolate its destructor separately, after any wake
            // panic has been caught, so it cannot skip sibling notifications.
            let disposal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(hook)));
            for outcome in [wake, disposal] {
                if let Err(payload) = outcome {
                    if failure.is_none() {
                        failure = Some(payload);
                    } else {
                        // A user panic payload may itself panic on drop.
                        std::mem::forget(payload);
                    }
                }
            }
        }
        if let Some(payload) = failure {
            if std::thread::panicking() {
                std::mem::forget(payload);
            } else {
                std::panic::resume_unwind(payload);
            }
        }
    }
}

impl<T> Sender<T> {
    /// Try to commit a message, returning receiver wakes for later delivery.
    ///
    /// Queue insertion or synchronous receiver handoff finishes before return.
    /// No receiver waker runs until the returned notification guard is dropped.
    /// This lets callers release their own admission lock before a receiver
    /// callback reenters channel lifecycle operations. Full/disconnected errors
    /// retain the original message and have no associated receiver wakes.
    pub fn try_send_deferred(&self, msg: T) -> (Result<(), TrySendError<T>>, DeferredSendWakes<T>) {
        let mut wakes = DeferredSendWakes::new();
        let result = {
            let mut chan = wait_lock(&self.shared.chan);
            if self.shared.is_disconnected() {
                Err(TrySendError::Disconnected(msg))
            } else if !chan.waiting.is_empty() {
                chan.deliver(msg, &mut wakes);
                Ok(())
            } else if chan
                .sending
                .as_ref()
                .map(|(cap, _)| chan.queue.len() < *cap)
                .unwrap_or(true)
            {
                chan.queue.push_back(msg);
                Ok(())
            } else {
                Err(TrySendError::Full(msg))
            }
        };
        (result, wakes)
    }
}
