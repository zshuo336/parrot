//! Drive one static lifecycle on an existing native scheduler. The empty
//! routing mailbox carries only its processor; typed messages never cross it.
use super::{SystemError, TypedExit};
use crate::{
    owned_payload::{OwnedPayload, PublicationInput},
    thread::{
        config::{SchedulingMode, ThreadActorConfig},
        mailbox::{Mailbox, mpsc::MpscMailbox},
        processor::{ProcessorInterface, ProcessorStats, ProcessorStatsTrait},
        scheduler::SchedulerGroup,
        system::{StopCompletion, StopOutcome, await_stop},
    },
};
use parrot_api::{address::ActorPath, types::BoxedFuture};
use std::{
    any::Any,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tokio::sync::{Notify, Semaphore, oneshot, watch};

type Outcome = anyhow::Result<TypedExit>;
type WorkFuture = Pin<Box<dyn Future<Output = Outcome> + Send>>;

struct Work {
    future: Option<WorkFuture>,
    result: Option<oneshot::Sender<OwnedPayload<Outcome>>>,
    retired: Option<watch::Sender<Option<StopOutcome>>>,
    runtime: tokio::runtime::Handle,
    cleanup: Arc<Semaphore>,
}
impl Work {
    fn dispose(&mut self) -> parrot_api::types::ActorResult<()> {
        let result = PublicationInput::new(self.future.take()).retire();
        if let Some(retired) = self.retired.take() {
            let outcome = StopOutcome::from_result(result.as_ref().map(|_| ()).map_err(|error| {
                SystemError::ShutdownError(crate::error_receipt::actor_summary(error))
            }));
            crate::owned_payload::publish_watch(&retired, outcome);
        }
        result
    }
}
impl Future for Work {
    type Output = Result<(), SystemError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.future
                .as_mut()
                .expect("live static driver")
                .as_mut()
                .poll(cx)
        }));
        let outcome = match result {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => result,
            Err(payload) => Err(anyhow::anyhow!(crate::owned_payload::panic_message(
                payload
            ))),
        };
        let mut outcome = PublicationInput::new(outcome);
        if let Err(error) = self.dispose() {
            outcome = PublicationInput::new(Err(anyhow::anyhow!(
                crate::error_receipt::actor_summary(&error)
            )));
        }
        let outcome = OwnedPayload::new(outcome.take(), &self.runtime, &self.cleanup);
        // A cancelled receiver retains disposal ownership through OwnedPayload.
        let _ = self
            .result
            .take()
            .expect("one static outcome")
            .send(outcome);
        Poll::Ready(Ok(()))
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}

pub(super) struct Driver {
    work: Mutex<Option<OwnedPayload<Work>>>,
    retirement: StopCompletion,
    cancelled: AtomicBool,
    changed: Notify,
    scheduled: AtomicBool,
}
pub(super) struct CancelOnDrop(pub Arc<Driver>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel_processing();
    }
}
impl Driver {
    pub(super) fn new(
        future: impl Future<Output = Outcome> + Send + 'static,
        runtime: tokio::runtime::Handle,
        cleanup: Arc<Semaphore>,
    ) -> (Arc<Self>, oneshot::Receiver<OwnedPayload<Outcome>>) {
        let (result, receiver) = oneshot::channel();
        let (retired, retirement) = watch::channel(None);
        let work = Work {
            future: Some(Box::pin(future)),
            result: Some(result),
            retired: Some(retired),
            runtime: runtime.clone(),
            cleanup: cleanup.clone(),
        };
        (
            Arc::new(Self {
                work: Mutex::new(Some(OwnedPayload::new(work, &runtime, &cleanup))),
                retirement,
                cancelled: AtomicBool::new(false),
                changed: Notify::new(),
                scheduled: AtomicBool::new(false),
            }),
            receiver,
        )
    }

    pub(super) fn schedule(
        self: &Arc<Self>,
        group: &SchedulerGroup,
        path: &str,
        mode: &SchedulingMode,
    ) -> Result<(), SystemError> {
        let mailbox = Arc::new(MpscMailbox::new(1, ActorPath::placeholder(path)));
        mailbox.set_processor(self.clone());
        match mode {
            SchedulingMode::DedicatedThread => group.dedicated_scheduler.schedule_with_processor(
                path,
                mailbox,
                self.clone(),
                ThreadActorConfig::default(),
            )?,
            SchedulingMode::Sharded { affinity_key } => group
                .try_sharded()?
                .schedule_affinity(path, mailbox, affinity_key)
                .map_err(SystemError::ThreadSetupError)?,
            SchedulingMode::SharedPool { .. } => {
                unreachable!("shared static polls keep their executor")
            }
        }
        self.scheduled.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) async fn finish(
        self: &Arc<Self>,
        group: &SchedulerGroup,
        path: &str,
        mode: &SchedulingMode,
        slots: Arc<Semaphore>,
        deadline: tokio::time::Instant,
    ) -> Result<(), SystemError> {
        self.cancel_processing();
        if self.scheduled.load(Ordering::Acquire) {
            let result = group
                .retire_actor(path, mode, self.clone(), slots, deadline)
                .await;
            if matches!(mode, SchedulingMode::Sharded { .. }) {
                let shard = group.sharded_scheduler.lock().unwrap().clone();
                if let Some(shard) = shard {
                    // The system registry still retains this generation until
                    // finish returns, so its path cannot yet name a replacement.
                    let _ = shard.deschedule(path);
                }
            }
            result
        } else {
            // Failed admission or an unpolled host task has no native owner.
            self.retirement().await
        }
    }
}

impl ProcessorStatsTrait for Driver {
    fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
        None
    }
}
impl ProcessorInterface for Driver {
    fn is_initialized(&self) -> bool {
        self.work.lock().unwrap().is_none()
    }
    fn initialize_and_start_erased(
        self: Arc<Self>,
    ) -> BoxedFuture<'static, Result<(), SystemError>> {
        // The scheduler's initialization stage is a single owned async driver.
        // It multiplexes Pending normally; actual actor init/handlers/hooks and
        // replacements remain inside the existing typed lifecycle state machine.
        Box::pin(async move {
            let work = self.work.lock().unwrap().take();
            match work {
                Some(work) => work.take().await,
                None => Ok(()),
            }
        })
    }
    fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
    fn retire_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async move {
            let work = self.work.lock().unwrap().take();
            if let Some(work) = work {
                // Accepted but unpolled storage is disposed on this owner too.
                let _ = PublicationInput::new(work.take()).retire();
            }
            await_stop(self.retirement.clone()).await
        })
    }
    fn retirement(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async move {
            let work = self.work.lock().unwrap().take();
            if let Some(work) = work {
                // This fallback is reached only after native submission failed
                // or the native owner exited before taking its queued input.
                let _ = work.retire().await;
            }
            await_stop(self.retirement.clone()).await
        })
    }
    fn process_batch_erased(
        self: Arc<Self>,
        _: Arc<dyn Mailbox>,
        _: usize,
        _: bool,
    ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
        Box::pin(async { Ok((0, 0)) })
    }
    fn cancel_processing(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
    fn processing_cancelled(&self) -> BoxedFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.cancelled.load(Ordering::Acquire) {
                    return;
                }
                changed.await;
            }
        })
    }
    fn as_any(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
