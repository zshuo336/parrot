use api_neutral::{BoxedMessage, BoxedResult, ErasedActor};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
pub(crate) type Errors = Arc<Mutex<Vec<String>>>;
pub(crate) fn panic_text(payload: BoxedMessage) -> String {
    let result = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).into()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "opaque panic".into()
    };
    safe_drop(payload);
    result
}
pub(crate) fn safe_drop<T>(value: T) {
    if let Err(p) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
        if let Err(p) = catch_unwind(AssertUnwindSafe(|| drop(p))) {
            std::mem::forget(p);
        }
    }
}
pub(crate) fn deadline(timeout: Duration) -> Result<Instant, String> {
    if timeout.is_zero() {
        return Err("timeout must be positive".into());
    }
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| "timeout is too large".into())
}
pub(crate) fn finish_shutdown(result: Result<(), String>, errors: &Errors) -> Result<(), String> {
    result?;
    let errors = errors.lock().unwrap();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}
pub(crate) enum Command {
    Start,
    Message(BoxedMessage),
}
pub(crate) struct ActorOwner {
    actor: Option<Box<dyn ErasedActor>>,
    attempted: bool,
    ready: bool,
    errors: Errors,
    pub retired: Option<Box<dyn FnOnce() + Send>>,
}
impl ActorOwner {
    pub fn new(actor: Box<dyn ErasedActor>, errors: Errors) -> Self {
        Self {
            actor: Some(actor),
            attempted: false,
            ready: false,
            errors,
            retired: None,
        }
    }
    /// Panics deliberately propagate to the runtime's worker boundary.
    pub fn apply(&mut self, command: Command) -> BoxedResult {
        match command {
            Command::Start => {
                if self.attempted {
                    return Err("actor initialization already attempted".into());
                }
                self.attempted = true;
                self.actor.as_mut().unwrap().pre_start()?;
                self.ready = true;
                Ok(Box::new(()))
            }
            Command::Message(message) => {
                if !self.ready {
                    safe_drop(message);
                    return Err("actor initialization did not succeed".into());
                }
                self.actor.as_mut().unwrap().receive(message)
            }
        }
    }
}
impl Drop for ActorOwner {
    fn drop(&mut self) {
        if let Some(mut actor) = self.actor.take() {
            if self.attempted {
                if let Err(payload) = catch_unwind(AssertUnwindSafe(|| actor.post_stop())) {
                    let message = format!("post_stop panicked: {}", panic_text(payload));
                    self.errors.lock().unwrap().push(message);
                }
            }
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(actor))) {
                let message = format!("actor Drop panicked: {}", panic_text(payload));
                self.errors.lock().unwrap().push(message);
            }
        }
        if let Some(retired) = self.retired.take() {
            retired();
        }
    }
}
