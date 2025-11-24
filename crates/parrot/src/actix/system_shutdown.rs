//! The task owns a terminal receipt even before its first poll or on host exit.
use super::super::lifecycle::Outcome;
use super::{ArbiterPool, Registry};
use parrot_api::{errors::ActorError, types::ActorResult};
use std::sync::{Arc, RwLock};
use tokio::{sync::watch, time::Instant};

pub(super) struct ShutdownOwner {
    work: Option<ShutdownWork>,
    cancellation: Arc<super::super::cancellation_executor::Reservation>,
}

struct ShutdownWork {
    pool: ArbiterPool,
    state: Arc<RwLock<Registry>>,
    finished: watch::Sender<Option<Outcome>>,
    deadline: Instant,
    root_cohorts: Arc<crate::root_cohort::RootCohorts>,
    io: Option<Arc<crate::io_runtime::IoRuntime>>,
}

fn publish(finished: &watch::Sender<Option<Outcome>>, outcome: Outcome) {
    crate::owned_payload::publish_watch_if_modified(finished, |value| {
        if value.is_some() {
            return false;
        }
        *value = Some(outcome);
        true
    });
}

pub(super) fn timeout(finished: &watch::Sender<Option<Outcome>>) {
    publish(
        finished,
        Outcome::Timeout("Actix system shutdown exceeded configured deadline".into()),
    );
}

impl ShutdownOwner {
    pub(super) fn new(
        pool: ArbiterPool,
        state: Arc<RwLock<Registry>>,
        deadline: Instant,
        finished: watch::Sender<Option<Outcome>>,
        cancellation: Arc<super::super::cancellation_executor::Reservation>,
        root_cohorts: Arc<crate::root_cohort::RootCohorts>,
        io: Option<Arc<crate::io_runtime::IoRuntime>>,
    ) -> Self {
        Self {
            work: Some(ShutdownWork {
                pool,
                state,
                finished,
                deadline,
                root_cohorts,
                io,
            }),
            cancellation,
        }
    }

    pub(super) fn timer(&self) -> impl Future<Output = ()> + Send + 'static {
        let work = self.work.as_ref().unwrap();
        let finished = work.finished.clone();
        let deadline = work.deadline;
        async move {
            let mut completion = finished.subscribe();
            tokio::select! {
                _ = completion.wait_for(|value| value.is_some()) => {},
                _ = tokio::time::sleep_until(deadline) => timeout(&finished),
            }
        }
    }

    pub(super) fn finish(mut self, result: ActorResult<()>) {
        let work = self.work.take().unwrap();
        let disposal = work.clear();
        let outcome = Outcome::from_result(result.and(disposal));
        if Instant::now() >= work.deadline {
            timeout(&work.finished);
        } else {
            publish(&work.finished, outcome);
        }
    }
}

impl ShutdownWork {
    fn clear(&self) -> ActorResult<()> {
        let actors = {
            let mut state = self.state.write().unwrap();
            state.stopped = true;
            std::mem::take(&mut state.actors)
        };
        crate::owned_payload::PublicationInput::new(actors).retire()
    }

    fn cancel(self) {
        for worker in &self.pool.inner.workers {
            worker.stop();
        }
        let joined = self.pool.inner.retirement.join_blocking();
        let cohorts = futures::executor::block_on(self.root_cohorts.join());
        let io = match self.io.as_ref() {
            Some(io) => futures::executor::block_on(io.shutdown()),
            None => Ok(()),
        };
        let disposal = self.clear();
        let mut message =
            "Actix shutdown task cancelled; owned workers retired without graceful completion"
                .to_owned();
        for result in [joined, cohorts, io, disposal] {
            if let Err(error) = result {
                message.push_str("; ");
                message.push_str(&crate::error_receipt::actor_summary(&error));
            }
        }
        if Instant::now() >= self.deadline {
            timeout(&self.finished);
        } else {
            publish(
                &self.finished,
                Outcome::from_result(Err(ActorError::InternalError(message))),
            );
        }
    }
}

impl Drop for ShutdownOwner {
    fn drop(&mut self) {
        let Some(work) = self.work.take() else { return };
        if work
            .pool
            .inner
            .threads
            .contains(&std::thread::current().id())
        {
            self.cancellation.clone().submit(move || work.cancel());
        } else {
            work.cancel();
        }
    }
}
