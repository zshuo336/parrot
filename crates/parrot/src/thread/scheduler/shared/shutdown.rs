//! Bounded, stable shutdown diagnostics. No arbitrary panic payload is formatted,
//! and payload destruction cannot interrupt another owned resource's retirement.
use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use tokio::task::JoinError;

pub(super) type Receipt = tokio::sync::watch::Receiver<Option<Result<(), String>>>;

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
