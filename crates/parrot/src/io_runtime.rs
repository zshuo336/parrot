//! Native engines share one managed I/O boundary, including generation disposal.
use crate::{context_tasks::ContextTasks, error_receipt::ActorReceipt,
    owned_payload::{OwnedPayload, PublicationInput}};
use parrot_api::{errors::ActorError, runtime::{ActorRuntime, RuntimeConfig, TokioActorRuntime},
    types::{ActorResult, BoxedFuture, BoxedMessage}};
use std::{future::Future, pin::Pin, sync::{Arc, Mutex}, task::{Context, Poll}};

type Work = BoxedFuture<'static, ActorResult<BoxedMessage>>;
type Failure = Arc<Mutex<Option<ActorReceipt>>>;

pub(crate) struct IoRuntime {
    runtime: TokioActorRuntime,
    tasks: Arc<ContextTasks>,
    retirement: tokio::runtime::Handle,
    cleanup: Arc<tokio::sync::Semaphore>,
}

impl IoRuntime {
    pub(crate) async fn new(count: usize, retirement: &tokio::runtime::Handle,
        cleanup: &Arc<tokio::sync::Semaphore>) -> ActorResult<Arc<Self>> {
        let runtime = TokioActorRuntime::start(RuntimeConfig {
            worker_threads: Some(count),
            ..Default::default()
        }).await?;
        Ok(Arc::new(Self { runtime, tasks: ContextTasks::new(1024),
            retirement: retirement.clone(), cleanup: cleanup.clone() }))
    }

    pub(crate) fn close(&self) {
        self.tasks.close();
    }

    pub(crate) fn is_current(&self) -> bool {
        self.runtime.shutdown_would_wait_for_current_task()
    }

    pub(crate) async fn shutdown(&self) -> ActorResult<()> {
        self.close();
        let tasks = self.tasks.join().await;
        let joined = self.runtime.clone().shutdown().await;
        tasks.and(joined)
    }

    pub(crate) fn run(self: &Arc<Self>, future: Work,
        generation: Option<Arc<ContextTasks>>) -> Work {
        let input = OwnedPayload::new(future, &self.retirement, &self.cleanup);
        let io = self.clone();
        Box::pin(parrot_api::trace::capture(async move {
            let failure: Failure = Arc::default();
            let (send, receive) = tokio::sync::oneshot::channel();
            let work = Input { future: Some(PublicationInput::new(input.take())),
                failure: failure.clone(), owner: io.tasks.clone(), generation: generation.clone() };
            let task = io.tasks.user_task(async move {
                // The result stays protected if cancellation drops its receiver.
                let _ = send.send(PublicationInput::new(work.await));
                Ok(())
            })?;
            let task: BoxedFuture<'static, anyhow::Result<()>> = match generation {
                Some(generation) => Box::pin(generation.user_task(async move {
                    task.await.map_err(|_| ActorError::InternalError("I/O task owner failed".into()))
                })?),
                None => Box::pin(task),
            };
            let _ = io.runtime.spawn(task).await?;
            let failed = failure.lock().unwrap().clone();
            if let Some(error) = failed {
                return Err(error.into_error());
            }
            receive.await.map_err(|_| ActorError::Stopped)?.take()
        }))
    }
}

pub(crate) fn reject(future: Work) -> Work {
    let input = OwnedPayload::unbound(future);
    Box::pin(async move {
        input.retire().await?;
        Err(ActorError::Stopped)
    })
}

struct Input {
    future: Option<PublicationInput<Work>>,
    failure: Failure,
    owner: Arc<ContextTasks>,
    generation: Option<Arc<ContextTasks>>,
}

impl Input {
    fn primary(&self, error: ActorError) -> ActorReceipt {
        let error = ActorReceipt::from_error(error);
        let mut failure = self.failure.lock().unwrap();
        if failure.is_none() {
            *failure = Some(error.clone());
        }
        failure.as_ref().unwrap().clone()
    }

    fn cleanup_failure(&self, error: ActorError) {
        let message = crate::error_receipt::actor_summary(&error);
        self.owner.record_message_disposal_failure(&message);
        if let Some(generation) = &self.generation {
            generation.record_message_disposal_failure(&message);
        }
        self.primary(error);
    }

    fn dispose(&mut self) -> ActorResult<()> {
        match self.future.take().map(PublicationInput::retire).transpose() {
            Ok(_) => Ok(()),
            Err(error) => {
                let receipt = self.primary(error);
                self.cleanup_failure(receipt.clone().into_error());
                Err(receipt.into_error())
            }
        }
    }
}

impl Future for Input {
    type Output = ActorResult<BoxedMessage>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future.as_mut().expect("completed I/O input").value_mut().as_mut().poll(cx)
        }));
        let result = match outcome {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(Ok(value))) => Ok(value),
            Ok(Poll::Ready(Err(error))) => Err(self.primary(error).into_error()),
            Err(payload) => {
                let error = ActorError::Panic(crate::owned_payload::panic_message(payload));
                let receipt = self.primary(error);
                self.cleanup_failure(receipt.clone().into_error());
                Err(receipt.into_error())
            }
        };
        let cleanup = self.dispose();
        Poll::Ready(match result {
            Err(error) => Err(error),
            Ok(value) => match cleanup {
                Ok(()) => Ok(value),
                Err(error) => {
                    if let Err(secondary) = PublicationInput::new(value).retire() {
                        self.cleanup_failure(secondary);
                    }
                    Err(error)
                }
            }
        })
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}
