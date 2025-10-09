//! Cancellation and completion shared by managed Actix actor references.
use parrot_api::{
    errors::ActorError,
    typed_channel::{TypedActorLifecycle, TypedChannelState},
    types::{ActorResult, BoxedFuture},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{Notify, watch},
    time::Instant,
};

#[derive(Clone)]
pub(crate) enum Outcome {
    Stopped,
    Timeout(String),
    Panic(String),
    Failed(Arc<ActorError>),
}
impl Outcome {
    pub(crate) fn from_result(result: ActorResult<()>) -> Self {
        match result {
            Ok(()) => Self::Stopped,
            Err(ActorError::Timeout) => Self::Timeout("actor stop deadline elapsed".into()),
            Err(ActorError::TimeoutDetail(s)) => Self::Timeout(s),
            Err(ActorError::Panic(s)) => Self::Panic(s),
            Err(e) => Self::Failed(Arc::new(e)),
        }
    }
    fn result(self) -> ActorResult<()> {
        match self {
            Self::Stopped => Ok(()),
            Self::Timeout(s) => Err(ActorError::TimeoutDetail(s)),
            Self::Panic(s) => Err(ActorError::Panic(s)),
            Self::Failed(error) => Err(match error.as_ref() {
                ActorError::InitializationError(s) => ActorError::InitializationError(s.clone()),
                ActorError::MessageHandlingError(s) => ActorError::MessageHandlingError(s.clone()),
                ActorError::Stopped => ActorError::Stopped,
                ActorError::ActorNotFound(s) => ActorError::ActorNotFound(s.clone()),
                ActorError::InternalError(s) => ActorError::InternalError(s.clone()),
                ActorError::ProcessMessageError(s) => ActorError::ProcessMessageError(s.clone()),
                ActorError::ReplyChannelError(s) => ActorError::ReplyChannelError(s.clone()),
                error => ActorError::Other(anyhow::anyhow!("{error}")),
            }),
        }
    }
}
pub(crate) type Completion = watch::Receiver<Option<Outcome>>;
pub(crate) async fn await_completion(mut completion: Completion) -> ActorResult<()> {
    loop {
        if let Some(result) = completion.borrow_and_update().clone() {
            return result.result();
        }
        completion.changed().await.map_err(|_| {
            ActorError::InternalError(
                "Actix lifecycle owner ended without reporting completion".into(),
            )
        })?;
    }
}

pub(crate) struct ActorControl {
    pub channel: TypedChannelState,
    pub tasks: Arc<crate::context_tasks::ContextTasks>,
    deadline: Mutex<Option<Instant>>,
    changed: Notify,
    pub completion: Completion,
    pub timeout: Duration,
}
impl ActorControl {
    pub fn new(timeout: Duration) -> (Arc<Self>, watch::Sender<Option<Outcome>>) {
        Self::with_task_limit(timeout, crate::context_tasks::DEFAULT_MAX_CONTEXT_TASKS)
    }
    pub fn with_task_limit(
        timeout: Duration,
        limit: usize,
    ) -> (Arc<Self>, watch::Sender<Option<Outcome>>) {
        let (finished, completion) = watch::channel(None);
        (
            Arc::new(Self {
                channel: TypedChannelState::default(),
                tasks: crate::context_tasks::ContextTasks::new(limit),
                deadline: Mutex::new(None),
                changed: Notify::new(),
                completion,
                timeout,
            }),
            finished,
        )
    }
    pub fn request_stop(&self, deadline: Instant) {
        {
            let mut current = self.deadline.lock().unwrap();
            *current = Some(current.map_or(deadline, |old| old.min(deadline)));
        }
        self.tasks.close();
        self.channel.seal();
        self.changed.notify_waiters();
    }
    pub fn expired_now(&self) -> bool {
        self.deadline
            .lock()
            .unwrap()
            .is_some_and(|deadline| deadline <= Instant::now())
    }
    pub async fn expired(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let deadline = *self.deadline.lock().unwrap();
            if let Some(deadline) = deadline {
                tokio::select! { _ = tokio::time::sleep_until(deadline) => return, _ = changed => {} }
            } else {
                changed.await;
            }
        }
    }
}
impl TypedActorLifecycle for ActorControl {
    fn channel(&self) -> &TypedChannelState {
        &self.channel
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.request_stop(Instant::now() + self.timeout);
            await_completion(self.completion.clone()).await
        })
    }
}

pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "actor panicked with non-string payload".into())
}
