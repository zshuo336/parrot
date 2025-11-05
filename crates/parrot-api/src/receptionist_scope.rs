//! Publication fencing and retirement receipts for one actor generation.
use crate::{errors::ActorError, owned_input, receptionist::ReceptionistKey, types::ActorResult};
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::Notify;

type Binding = (usize, ReceptionistKey, String);
type Cleanup = Box<dyn FnMut() -> ActorResult<()> + Send>;

#[derive(Default)]
struct State {
    closed: bool,
    retiring: bool,
    result: Option<Result<(), (bool, String)>>,
    bindings: HashMap<Binding, Cleanup>,
}

/// A managed actor generation's publication gate. Cloning this handle does not
/// extend the generation's lifetime. Engines must close and retire the scope.
/// Cleanup callbacks run outside the scope lock and must not retain an actor.
#[derive(Default)]
pub struct RegistrationScope {
    state: Mutex<State>,
    closed: Notify,
    finished: Notify,
}

impl std::fmt::Debug for RegistrationScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistrationScope").finish_non_exhaustive()
    }
}

impl RegistrationScope {
    /// Maximum distinct active registry/key/path bindings per generation.
    pub const MAX_BINDINGS: usize = 1024;

    /// Serialize a synchronous table mutation with generation closure. The
    /// callback must defer notifications until after this function returns.
    pub fn with_live<T>(&self, commit: impl FnMut() -> T) -> ActorResult<T> {
        let mut commit = owned_input::Input::new(commit);
        let result = {
            let state = self.state.lock().unwrap();
            if state.closed {
                Err(ActorError::Stopped)
            } else {
                owned_input::call(|| commit.value_mut()())
            }
        };
        commit.finish(result)
    }

    /// Remember exact-owner cleanup before publishing. Registry identity must
    /// remain unique while the callback exists (a captured Weak allocation is
    /// sufficient). Repeated registration of the same binding consumes one slot.
    pub fn publish<T>(
        &self,
        binding: Binding,
        cleanup: impl FnMut() -> ActorResult<()> + Send + 'static,
        commit: impl FnMut() -> T,
    ) -> ActorResult<T> {
        let mut cleanup = owned_input::Input::new(Box::new(cleanup) as Cleanup);
        let mut commit = owned_input::Input::new(commit);
        let result = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                Err(ActorError::Stopped)
            } else if !state.bindings.contains_key(&binding)
                && state.bindings.len() >= Self::MAX_BINDINGS
            {
                Err(ActorError::InternalError(
                    "actor service registration limit reached".into(),
                ))
            } else {
                state
                    .bindings
                    .entry(binding)
                    .or_insert_with(|| cleanup.take());
                owned_input::call(|| commit.value_mut()())
            }
        };
        commit.finish(cleanup.finish(result))
    }

    /// Atomically remove a publication and its cleanup binding. The mutation
    /// must defer notifications, just as `publish` does.
    pub fn withdraw<T>(&self, binding: &Binding, commit: impl FnMut() -> T) -> ActorResult<T> {
        let mut commit = owned_input::Input::new(commit);
        let (result, callback) = {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                (Err(ActorError::Stopped), None)
            } else {
                let result = owned_input::call(|| commit.value_mut()());
                let callback = if result.is_ok() {
                    state.bindings.remove(binding)
                } else {
                    None
                };
                (result, callback)
            }
        };
        commit.finish(owned_input::Input::new(callback).finish(result))
    }

    /// Only mark closed and notify the engine-owned worker. No table operations
    /// or subscriber callbacks may execute in an engine's cancellation locks.
    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.closed.notify_waiters();
    }

    pub async fn closed(&self) {
        let notified = self.closed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.state.lock().unwrap().closed {
            notified.await;
        }
    }

    /// Retire once, outside engine/table locks. Concurrent callers wait on the
    /// same receipt through `join`; each callback has its own panic boundary.
    pub fn retire(&self) {
        let callbacks = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            if state.retiring {
                return;
            }
            state.retiring = true;
            std::mem::take(&mut state.bindings)
        };
        self.closed.notify_waiters();
        let mut result = Ok(());
        for callback in callbacks.into_values() {
            let mut callback = owned_input::Input::new(callback);
            let next = owned_input::call(|| callback.value_mut()()).and_then(|result| result);
            let next = callback.finish(next).map_err(|error| {
                let mut error = owned_input::Input::new(error);
                let panic = matches!(error.value(), ActorError::Panic(_));
                let message = owned_input::call(|| match error.value() {
                    ActorError::Panic(message) => message.clone(),
                    error => error.to_string(),
                });
                let disposal = error.retire();
                match message.and_then(|message| disposal.map(|()| message)) {
                    Ok(message) => (panic, message),
                    Err(_) => (
                        true,
                        "service cleanup error formatting or disposal panicked".into(),
                    ),
                }
            });
            if result.is_ok() {
                result = next;
            }
        }
        self.state.lock().unwrap().result = Some(result);
        self.finished.notify_waiters();
    }

    pub async fn join(&self) -> ActorResult<()> {
        loop {
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.state.lock().unwrap().result.clone() {
                return result.map_err(|(panic, message)| {
                    if panic {
                        ActorError::Panic(message)
                    } else {
                        ActorError::InternalError(message)
                    }
                });
            }
            notified.await;
        }
    }
}
