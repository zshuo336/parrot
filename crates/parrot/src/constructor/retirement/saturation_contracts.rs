use crate::{owned_payload, test_support::isolated};
use parrot_api::{
    actor::{Actor, ActorState},
    context::ActorContext,
    engine::{AsyncEngineRuntime, AsyncFactoryRuntime, FactoryOptions, RuntimeActor, SpawnOptions},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;

const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    entered: tokio::sync::Notify,
    drops: AtomicUsize,
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.open.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
struct Capture(Arc<Gate>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.entered.notify_one();
        let (open, _) = self
            .0
            .changed
            .wait_timeout_while(self.0.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(*open, "test must release the factory capture");
        self.0.drops.fetch_add(1, Ordering::SeqCst);
        panic!("saturated factory capture disposal failure");
    }
}
struct Echo;
impl Actor for Echo {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}

fn saturated_stop(actix: bool, global: bool) {
    actix::System::new().block_on(async {
        let thread =
            crate::thread::ThreadActorSystem::shared(crate::thread::ThreadActorSystemConfig {
                shared_pool_size: 1,
                shared_burst_workers_max: 0,
                shutdown_timeout: Duration::from_secs(20),
                ..Default::default()
            });
        let other =
            crate::thread::ThreadActorSystem::shared(crate::thread::ThreadActorSystemConfig {
                shared_pool_size: 1,
                shared_burst_workers_max: 0,
                ..Default::default()
            });
        let actix_system = if actix {
            Some(
                crate::actix::ActixActorSystem::with_guardian(
                    crate::actix::ActixSystemSettings {
                        arbiter_count: 1,
                        shutdown_timeout: Duration::from_secs(20),
                        ..Default::default()
                    },
                    Default::default(),
                )
                .await
                .unwrap(),
            )
        } else {
            None
        };
        let system: &dyn AsyncFactoryRuntime = match &actix_system {
            Some(system) => system,
            None => thread.as_ref(),
        };
        let gate = Arc::new(Gate::default());
        let release = Release(gate.clone());
        let capture = Capture(gate.clone());
        let actor = system
            .spawn_factory(
                Box::new(move || {
                    std::hint::black_box(&capture);
                    Box::new(Echo) as RuntimeActor
                }),
                FactoryOptions {
                    spawn: SpawnOptions::at("/saturated"),
                    max_restarts: 0,
                    within: WAIT,
                },
            )
            .await
            .unwrap();
        let independent = AsyncEngineRuntime::spawn(
            other.as_ref(),
            Box::new(Echo),
            Box::new(()),
            SpawnOptions::at("/independent"),
        )
        .await
        .unwrap();
        let filler = if global {
            Arc::new(Semaphore::new(8))
        } else {
            thread.cleanup_slots.clone()
        };
        let count = if global {
            owned_payload::MAX_PENDING_CLEANUPS_GLOBAL
        } else {
            owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT
        };
        let mut permits: Vec<_> = (0..count)
            .map(|_| owned_payload::try_pending_cleanup_permit(&filler).unwrap())
            .collect();
        assert!(owned_payload::try_pending_cleanup_permit(&filler).is_none());
        let mut first = actor.stop();
        assert!(futures::poll!(&mut first).is_pending());
        drop(first);
        // These timers execute on the observer host, independently of the
        // single shared retirement driver whose progress is under test.
        assert!(
            tokio::time::timeout(Duration::from_millis(150), gate.entered.notified())
                .await
                .is_err(),
            "saturated public stop ran factory Drop before cleanup admission"
        );
        tokio::time::timeout(WAIT, independent.stop())
            .await
            .expect("saturated factory blocked another system's stop")
            .unwrap();
        tokio::time::timeout(WAIT, other.retirement_runtime().spawn(async { 17 }))
            .await
            .expect("saturated factory blocked shared driver heartbeat")
            .unwrap();
        let mut first = actor.stop();
        let mut second = actor.stop();
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        drop(permits.pop());
        tokio::time::timeout(WAIT, gate.entered.notified())
            .await
            .expect("one returned quota did not admit factory disposal");
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        tokio::time::timeout(WAIT, other.retirement_runtime().spawn(async { 23 }))
            .await
            .expect("admitted capture Drop blocked shared driver heartbeat")
            .unwrap();
        drop(release);
        let (first, second) = tokio::time::timeout(WAIT, async { tokio::join!(first, second) })
            .await
            .expect("actual factory disposal did not finish stop receipts");
        let error = first.unwrap_err().to_string();
        assert!(
            error.contains("saturated factory capture disposal failure"),
            "{error}"
        );
        assert_eq!(error, second.unwrap_err().to_string());
        assert_eq!(gate.drops.load(Ordering::SeqCst), 1);
        drop(permits);
        AsyncEngineRuntime::shutdown(other.as_ref()).await.unwrap();
        let _ = AsyncEngineRuntime::shutdown(thread.as_ref()).await;
        if let Some(system) = actix_system {
            let _ = system.shutdown().await;
        }
    });
}

#[test]
fn local_saturation_preserves_other_system_stop() {
    isolated(
        "constructor::retirement::saturation_contracts::local_saturation_preserves_other_system_stop",
        || saturated_stop(false, false),
    );
}

#[test]
fn global_saturation_preserves_other_system_stop() {
    isolated(
        "constructor::retirement::saturation_contracts::global_saturation_preserves_other_system_stop",
        || saturated_stop(false, true),
    );
}

#[test]
fn actix_saturation_preserves_other_system_stop() {
    isolated(
        "constructor::retirement::saturation_contracts::actix_saturation_preserves_other_system_stop",
        || saturated_stop(true, true),
    );
}

#[test]
fn abandoned_lifecycle_admission_keeps_seal_and_actual_receipt() {
    isolated(
        "constructor::retirement::saturation_contracts::abandoned_lifecycle_admission_keeps_seal_and_actual_receipt",
        || {
            for polled in [false, true] {
                let host = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let slots = Arc::new(Semaphore::new(1));
                let permits: Vec<_> = (0..owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT)
                    .map(|_| owned_payload::try_pending_cleanup_permit(&slots).unwrap())
                    .collect();
                let gate = Arc::new(Gate::default());
                let release = Release(gate.clone());
                let capture = Capture(gate.clone());
                let factory = super::Factory::new(
                    move || {
                        std::hint::black_box(&capture);
                    },
                    Arc::new(Semaphore::new(1)),
                    slots.clone(),
                );
                let mut retirement = {
                    let _entered = host.enter();
                    factory.retire_in_lifecycle()
                };
                host.block_on(async {
                    assert!(matches!(
                        factory.prepare::<()>(Arc::default()).await,
                        Err(super::ConstructionError::Cancelled)
                    ));
                    if polled {
                        assert!(futures::poll!(&mut retirement).is_pending());
                    }
                });
                assert_eq!(gate.drops.load(Ordering::SeqCst), 0);
                assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
                // Abandonment cannot lose the capture or leave a permanently
                // incomplete shared receipt, even before the first poll.
                drop(release);
                drop(retirement);
                let error = host.block_on(factory.retire()).unwrap_err();
                assert!(error.contains("saturated factory capture disposal failure"));
                assert_eq!(error, host.block_on(factory.retire()).unwrap_err());
                assert_eq!(gate.drops.load(Ordering::SeqCst), 1);
                drop(permits);
                assert!(owned_payload::try_pending_cleanup_permit(&slots).is_some());
            }
        },
    );
}
