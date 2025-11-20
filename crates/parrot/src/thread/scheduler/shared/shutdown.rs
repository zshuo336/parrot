//! Bounded, stable shutdown diagnostics. No arbitrary panic payload is formatted,
//! and payload destruction cannot interrupt another owned resource's retirement.
use std::any::Any;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::task::JoinError;

pub(super) type Receipt = tokio::sync::watch::Receiver<Option<Result<(), String>>>;

// Keep the original close operation alive past the worker grace deadline. Both
// its polling and its final disposal may invoke a custom mailbox's user code.
pub(super) struct MailboxClose {
    operation: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    mailbox: Option<Arc<dyn crate::thread::mailbox::Mailbox>>,
    failures: Arc<Failures>,
    phase: &'static str,
}
impl MailboxClose {
    pub(super) fn new(
        mailbox: Arc<dyn crate::thread::mailbox::Mailbox>,
        failures: Arc<Failures>,
        phase: &'static str,
    ) -> Self {
        let closing = mailbox.clone();
        Self {
            operation: Some(Box::pin(async move { closing.close().await })),
            mailbox: Some(mailbox),
            failures,
            phase,
        }
    }
    fn dispose(&mut self) {
        let operation = self.operation.take();
        self.failures
            .isolate("mailbox close future destructor", || drop(operation));
        let mailbox = self.mailbox.take();
        self.failures
            .isolate("mailbox destructor", || drop(mailbox));
    }
}
impl Future for MailboxClose {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let result = catch_unwind(AssertUnwindSafe(|| {
            this.operation.as_mut().unwrap().as_mut().poll(cx)
        }));
        match result {
            Ok(Poll::Pending) => Poll::Pending,
            result => {
                if let Err(payload) = result {
                    this.failures.record_panic(this.phase, payload);
                }
                this.dispose();
                Poll::Ready(())
            }
        }
    }
}
impl Drop for MailboxClose {
    fn drop(&mut self) {
        self.dispose();
    }
}

#[derive(Default)]
pub(super) struct Failures(Mutex<Summary>);
#[derive(Default)]
struct Summary {
    count: usize,
    messages: Vec<String>,
}

fn bounded(message: &str) -> String {
    let end = message
        .char_indices()
        .nth(1024)
        .map_or(message.len(), |(i, _)| i);
    let mut bounded = message[..end].to_owned();
    if end < message.len() {
        bounded.push('…');
    }
    bounded
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    let mut message = if let Some(value) = payload.downcast_ref::<String>() {
        bounded(value)
    } else if let Some(value) = payload.downcast_ref::<&str>() {
        bounded(value)
    } else {
        "non-string panic payload".into()
    };
    if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        message.push_str("; panic payload destructor panicked");
        // A recursively hostile secondary payload must not unwind cleanup again.
        // This exceptional payload alone is intentionally leaked.
        std::mem::forget(secondary);
    }
    message
}

impl Failures {
    fn record(&self, message: String) {
        let mut summary = self.0.lock().unwrap();
        summary.count = summary.count.saturating_add(1);
        if summary.messages.len() < 8 {
            summary.messages.push(bounded(&message));
        }
    }

    pub(super) fn join(&self, result: Result<(), JoinError>, requested_abort: bool) {
        if let Err(error) = result {
            if error.is_panic() {
                self.record(format!(
                    "shared task panicked: {}",
                    panic_message(error.into_panic())
                ));
            } else if !requested_abort {
                self.record("shared task unexpectedly cancelled".into());
            }
        }
    }

    pub(super) fn isolate(&self, phase: &str, operation: impl FnOnce()) {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(operation)) {
            self.record(format!("{phase} panicked: {}", panic_message(payload)));
        }
    }

    pub(super) fn record_panic(&self, phase: &str, payload: Box<dyn Any + Send>) {
        self.record(format!("{phase} panicked: {}", panic_message(payload)));
    }

    pub(super) fn result(&self) -> Result<(), String> {
        let summary = self.0.lock().unwrap();
        if summary.count == 0 {
            Ok(())
        } else {
            Err(format!(
                "{} shared cleanup failure(s): {}",
                summary.count,
                summary.messages.join("; ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CloseProbe {
        ready: bool,
        panic_poll: bool,
        drops: Arc<AtomicUsize>,
    }
    impl Future for CloseProbe {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            assert!(!self.panic_poll, "custom close poll failure");
            if self.ready {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }
    impl Drop for CloseProbe {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            panic!("custom close future destructor failure");
        }
    }
    #[test]
    fn close_future_poll_completion_and_cancellation_disposal_are_isolated() {
        for (ready, panic_poll) in [(false, false), (true, false), (false, true)] {
            let failures = Arc::new(Failures::default());
            let drops = Arc::new(AtomicUsize::new(0));
            let mut close = Box::pin(MailboxClose {
                operation: Some(Box::pin(CloseProbe {
                    ready,
                    panic_poll,
                    drops: drops.clone(),
                })),
                mailbox: None,
                failures: failures.clone(),
                phase: "custom close",
            });
            let waker = futures::task::noop_waker();
            let polled = close.as_mut().poll(&mut Context::from_waker(&waker));
            assert_eq!(polled.is_ready(), ready || panic_poll);
            drop(close);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            let message = failures.result().unwrap_err();
            assert!(
                message.contains("custom close future destructor failure"),
                "{message}"
            );
            assert_eq!(message.contains("custom close poll failure"), panic_poll);
        }
    }
}
