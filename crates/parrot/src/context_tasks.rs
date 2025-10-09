//! Bounded tasks owned by one actor generation, independent of its executor.
use parrot_api::{
    errors::ActorError,
    message::CloneableMessage,
    types::{ActorResult, BoxedActorRef, BoxedMessage},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::Notify;

pub(crate) const DEFAULT_MAX_CONTEXT_TASKS: usize = parrot_config::Defaults::max_context_tasks();
#[derive(Debug, Default)]
struct Status {
    closed: bool,
    active: usize,
    panic: Option<String>,
}
#[derive(Debug, Default)]
struct State {
    status: Mutex<Status>,
    cancelled: Notify,
    finished: Notify,
}
impl State {
    fn close(&self) {
        self.status.lock().unwrap().closed = true;
        self.cancelled.notify_waiters();
    }
    async fn cancelled(&self) {
        let notified = self.cancelled.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let closed = self.status.lock().unwrap().closed;
        if !closed {
            notified.await;
        }
    }
    fn record_panic(&self, payload: Box<dyn std::any::Any + Send>) {
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "context task panicked with non-string payload".into());
        self.status.lock().unwrap().panic.get_or_insert(message);
    }
}

/// Contexts and lifecycle controllers own this value. Tasks retain only State,
/// so a long-lived timer cannot keep its own cancellation owner alive.
#[derive(Debug)]
pub(crate) struct ContextTasks {
    state: Arc<State>,
    limit: usize,
}
impl Drop for ContextTasks {
    fn drop(&mut self) {
        self.close();
    }
}
impl ContextTasks {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::default(),
            limit,
        })
    }
    pub(crate) fn close(&self) {
        self.state.close();
    }
    pub(crate) async fn join(&self) -> ActorResult<()> {
        loop {
            let finished = self.state.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            {
                let state = self.state.status.lock().unwrap();
                if state.active == 0 {
                    return match &state.panic {
                        Some(message) => Err(ActorError::Panic(message.clone())),
                        None => Ok(()),
                    };
                }
            }
            finished.await;
        }
    }
    fn register(
        &self,
        future: impl Future<Output = ActorResult<()>> + Send + 'static,
    ) -> ActorResult<OwnedTask> {
        // No user payload or future is destroyed while holding the admission lock.
        let admission = {
            let mut state = self.state.status.lock().unwrap();
            if state.closed {
                Err(ActorError::Stopped)
            } else if state.active >= self.limit {
                Err(ActorError::InternalError(format!(
                    "actor context task limit {} reached",
                    self.limit
                )))
            } else {
                state.active += 1;
                Ok(Ticket(self.state.clone()))
            }
        };
        let ticket = admission?;
        let state = self.state.clone();
        let execution = async move {
            tokio::select! { biased; _ = state.cancelled() => Ok(()), result = future => result }
        };
        Ok(OwnedTask {
            future: Some(Box::pin(execution)),
            ticket: Some(ticket),
        })
    }
    pub(crate) fn once(
        &self,
        target: BoxedActorRef,
        message: BoxedMessage,
        delay: Duration,
    ) -> ActorResult<OwnedTask> {
        let deadline = checked_deadline(delay)?;
        self.register(async move {
            tokio::time::sleep_until(deadline).await;
            target.deliver(message).await
        })
    }
    pub(crate) fn periodic(
        &self,
        target: BoxedActorRef,
        message: CloneableMessage,
        initial_delay: Duration,
        interval: Duration,
    ) -> ActorResult<OwnedTask> {
        if interval.is_zero() {
            return Err(ActorError::InternalError(
                "periodic interval must be positive".into(),
            ));
        }
        let initial = checked_deadline(initial_delay)?;
        checked_deadline(interval)?;
        self.register(async move {
            tokio::time::sleep_until(initial).await;
            loop {
                target.deliver(message.clone().into_boxed()).await?;
                // Fixed delay after successful admission; never catch up by
                // bursting missed ticks and never wait for a business reply.
                tokio::time::sleep_until(checked_deadline(interval)?).await;
            }
        })
    }
}
fn checked_deadline(delay: Duration) -> ActorResult<tokio::time::Instant> {
    tokio::time::Instant::now()
        .checked_add(delay)
        .ok_or_else(|| ActorError::InternalError("scheduled delay exceeds clock range".into()))
}
struct Ticket(Arc<State>);
impl Drop for Ticket {
    fn drop(&mut self) {
        self.0.status.lock().unwrap().active -= 1;
        self.0.finished.notify_waiters();
    }
}

/// Completion follows disposal even when the executor drops a suspended task.
/// Catch both polling and destructor panics before releasing its ticket.
pub(crate) struct OwnedTask {
    future: Option<Pin<Box<dyn Future<Output = ActorResult<()>> + Send>>>,
    ticket: Option<Ticket>,
}
impl OwnedTask {
    fn dispose(&mut self) {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(self.future.take())))
            && let Some(ticket) = &self.ticket
        {
            ticket.0.record_panic(payload);
        }
        drop(self.ticket.take());
    }
}
impl Drop for OwnedTask {
    fn drop(&mut self) {
        self.dispose();
    }
}
impl Future for OwnedTask {
    type Output = anyhow::Result<()>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future
                .as_mut()
                .expect("context task polled after completion")
                .as_mut()
                .poll(cx)
        }));
        match result {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(Err(error))) => {
                tracing::warn!(%error, "scheduled delivery failed; timer terminated")
            }
            Ok(Poll::Ready(Ok(()))) => {}
            Err(payload) => self.ticket.as_ref().unwrap().0.record_panic(payload),
        }
        self.dispose();
        Poll::Ready(Ok(()))
    }
}
