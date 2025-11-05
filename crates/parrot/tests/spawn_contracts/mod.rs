//! Registration timing is accepted only after identity, replies and retirement.
use anyhow::{Context, Result, anyhow, ensure};
use futures::FutureExt;
use parrot::thread::{ThreadActorConfig, ThreadActorSystem, ThreadContext};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(super) enum Behavior {
    Normal,
    WrongIdentity,
    WrongValue,
    Reject,
    FailDrop,
}
#[derive(Default)]
struct Stats {
    initialized: AtomicU64,
    handled: AtomicU64,
    dropped: AtomicU64,
}
struct Probe {
    id: usize,
    stats: Arc<Stats>,
    behavior: Behavior,
}
struct Reply {
    id: usize,
    value: u64,
}
impl Actor for Probe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.stats.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = *message
                .downcast::<u64>()
                .map_err(|_| ActorError::MessageHandlingError("spawn probe input type".into()))?;
            self.stats.handled.fetch_add(1, Ordering::SeqCst);
            if matches!(self.behavior, Behavior::Reject) {
                return Err(ActorError::MessageHandlingError(
                    "spawn probe rejected work".into(),
                ));
            }
            Ok(Box::new(Reply {
                id: self.id + usize::from(matches!(self.behavior, Behavior::WrongIdentity)),
                value: value + u64::from(matches!(self.behavior, Behavior::WrongValue)),
            }) as BoxedMessage)
        })
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.stats.dropped.fetch_add(1, Ordering::SeqCst);
        if matches!(self.behavior, Behavior::FailDrop) {
            panic!("spawn probe destructor failure");
        }
    }
}
#[derive(Debug)]
pub(super) struct Measurement {
    pub count: usize,
    pub elapsed: Duration,
}
impl Measurement {
    pub fn actors_per_second(&self) -> Result<f64> {
        ensure!(
            self.count > 0 && !self.elapsed.is_zero(),
            "registration measurement must be nonempty and nonzero"
        );
        let rate = self.count as f64 / self.elapsed.as_secs_f64();
        ensure!(rate.is_finite(), "registration rate must be finite");
        Ok(rate)
    }
}
async fn verify_reply(actor: &dyn ActorRef, id: usize, value: u64) -> Result<()> {
    let reply = actor
        .send_with_timeout(Box::new(value), Some(Duration::from_secs(10)))
        .await?;
    let reply = reply
        .downcast::<Reply>()
        .map_err(|_| anyhow!("spawn reply type"))?;
    ensure!(
        reply.id == id,
        "wrong actor identity: expected {id}, got {}",
        reply.id
    );
    ensure!(
        reply.value == value,
        "wrong reply value: expected {value}, got {}",
        reply.value
    );
    Ok(())
}
/// Also shuts down on validation errors. Timing covers registration and its
/// per-actor observation allocation; probes and all retirement are outside it.
pub(super) async fn exercise(
    system: &Arc<ThreadActorSystem>,
    count: usize,
    behavior: Behavior,
) -> Result<Measurement> {
    let mut observations = Vec::new();
    let work = std::panic::AssertUnwindSafe(async {
        ensure!(count > 0, "registration workload must not be empty");
        let start = Instant::now();
        let mut refs = Vec::with_capacity(count);
        for id in 0..count {
            let stats = Arc::new(Stats::default());
            observations.push(stats.clone());
            let actor = system
                .spawn_at(
                    Probe {
                        id,
                        stats,
                        behavior: if id == 0 { behavior } else { Behavior::Normal },
                    },
                    &format!("/spawn-contract/{id}"),
                    None,
                    ThreadActorConfig::default(),
                )
                .await?;
            refs.push(actor);
        }
        let measurement = Measurement {
            count,
            elapsed: start.elapsed(),
        };
        ensure!(
            system.actor_paths().len() == count,
            "registration count mismatch"
        );
        for (id, actor) in refs.iter().enumerate() {
            let path = format!("/spawn-contract/{id}");
            ensure!(
                actor.path() == path && actor.is_alive().await,
                "returned actor is not live at its requested path"
            );
            let resolved = system
                .get_actor_ref(&path)
                .context("registered actor cannot be resolved")?;
            ensure!(
                resolved.path() == path && resolved.is_alive().await,
                "resolved actor is not live at its requested path"
            );
            verify_reply(actor, id, id as u64 * 2).await?;
            verify_reply(resolved.as_ref(), id, id as u64 * 2 + 1).await?;
            ensure!(
                observations[id].initialized.load(Ordering::SeqCst) == 1,
                "actor initialization count"
            );
            ensure!(
                observations[id].handled.load(Ordering::SeqCst) == 2,
                "returned and resolved views did not reach the same actor exactly twice"
            );
        }
        // Collect every explicit stop before evaluating the errors. A later
        // system shutdown cannot reconstruct a retired generation's lost error.
        let mut stop_errors = Vec::new();
        for id in 0..count {
            if let Err(error) = system.stop_actor(&format!("/spawn-contract/{id}")).await {
                stop_errors.push(format!("actor {id}: {error:?}"));
            }
        }
        ensure!(
            stop_errors.is_empty(),
            "individual actor retirement failed: {stop_errors:?}"
        );
        for actor in &refs {
            ensure!(!actor.is_alive().await, "actor stayed live after stop");
        }
        Ok(measurement)
    })
    .catch_unwind()
    .await;
    let shutdown = system.shutdown_internal().await;
    let retired = observations
        .iter()
        .all(|stats| stats.dropped.load(Ordering::SeqCst) == 1);
    let registry_empty = system.actor_paths().is_empty();
    let work = match work {
        Ok(result) => result,
        Err(panic) => {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            Err(anyhow!("registration verification panicked: {message}"))
        }
    };
    if let Err(error) = &shutdown {
        eprintln!("registration system retirement failed: {error:?}");
    }
    ensure!(
        retired && registry_empty,
        "registration verification left live actors or incomplete disposal"
    );
    let measurement = work?;
    shutdown.context("registration system retirement")?;
    Ok(measurement)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn small_herd_checks_all_views_and_retirement() {
        let system = ThreadActorSystem::shared(Default::default());
        let measurement = exercise(&system, 32, Behavior::Normal).await.unwrap();
        assert_eq!(measurement.count, 32);
        assert!(measurement.actors_per_second().unwrap().is_finite());
    }
    async fn rejected(behavior: Behavior, reason: &str) {
        let system = ThreadActorSystem::shared(Default::default());
        let error = exercise(&system, 4, behavior).await.unwrap_err();
        assert!(format!("{error:#}").contains(reason), "{error:#}");
        assert!(system.actor_paths().is_empty());
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_identity_is_rejected() {
        rejected(Behavior::WrongIdentity, "wrong actor identity").await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_payload_is_rejected() {
        rejected(Behavior::WrongValue, "wrong reply value").await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn business_error_is_rejected() {
        rejected(Behavior::Reject, "spawn probe rejected work").await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_individual_retirement_is_rejected() {
        rejected(Behavior::FailDrop, "spawn probe destructor failure").await;
    }
    #[test]
    fn zero_duration_is_not_infinite_throughput_success() {
        assert!(
            Measurement {
                count: 20000,
                elapsed: Duration::ZERO
            }
            .actors_per_second()
            .is_err()
        );
    }
    #[test]
    fn empty_work_is_not_a_measurement() {
        assert!(
            Measurement {
                count: 0,
                elapsed: Duration::from_millis(1)
            }
            .actors_per_second()
            .is_err()
        );
    }
}
