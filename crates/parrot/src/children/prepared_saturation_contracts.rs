use super::*;
use crate::{owned_payload, test_support::isolated};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    engine::AsyncEngineRuntime,
    errors::ActorError,
    supervisor::{BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Condvar,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    entered: tokio::sync::Notify,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(
            *open,
            "test must release the prepared output or constructor"
        );
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.open.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
struct Plan {
    builds: AtomicUsize,
    drops: AtomicUsize,
    constructor: Option<Arc<Gate>>,
    disposal: Option<Arc<Gate>>,
}
impl Plan {
    fn make(self: &Arc<Self>) -> Output {
        let generation = self.builds.fetch_add(1, Ordering::SeqCst);
        if generation == 1
            && let Some(gate) = &self.constructor
        {
            gate.block();
        }
        Output {
            plan: self.clone(),
            generation,
        }
    }
}
struct Output {
    plan: Arc<Plan>,
    generation: usize,
}
impl Drop for Output {
    fn drop(&mut self) {
        if self.generation == 1
            && let Some(gate) = &self.plan.disposal
        {
            gate.block();
        }
        self.plan.drops.fetch_add(1, Ordering::SeqCst);
        if self.generation == 1 && self.plan.disposal.is_some() {
            panic!("saturated prepared output failure");
        }
    }
}
struct ThreadOutput(Output);
struct ActixOutput(Output);
macro_rules! actor {
    ($name:ident, $context:ty) => {
        impl Actor for $name {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                std::hint::black_box(&self.0);
                Box::pin(async move {
                    if message.is::<bool>() {
                        Err(ActorError::InternalError(
                            "deliberate cohort failure".into(),
                        ))
                    } else {
                        Ok(message)
                    }
                })
            }
        }
    };
}
actor!(ThreadOutput, crate::thread::ThreadContext<ThreadOutput>);
actor!(
    ActixOutput,
    crate::actix::ActixContext<crate::actix::ActixActor<ActixOutput>>
);
impl parrot_api::ActixEngineExt for ActixOutput {}

fn saturated_prepared(actix: bool) {
    actix::System::new().block_on(async {
        let other =
            crate::thread::ThreadActorSystem::shared(crate::thread::ThreadActorSystemConfig {
                shared_pool_size: 1,
                shared_burst_workers_max: 0,
                ..Default::default()
            });
        let thread =
            crate::thread::ThreadActorSystem::shared(crate::thread::ThreadActorSystemConfig {
                shared_pool_size: 2,
                shared_burst_workers_max: 0,
                shutdown_timeout: Duration::from_secs(20),
                ..Default::default()
            });
        let actix_system = if actix {
            Some(
                crate::actix::ActixActorSystem::with_settings(crate::actix::ActixSystemSettings {
                    arbiter_count: 2,
                    shutdown_timeout: Duration::from_secs(20),
                    ..Default::default()
                })
                .await
                .unwrap(),
            )
        } else {
            None
        };
        let policy = SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 2,
            within: WAIT,
            decider: BasicDecisionFn::new(|_| SupervisionDecision::Restart),
        });
        let cohort = if let Some(system) = &actix_system {
            system.root_cohort(policy).await.unwrap()
        } else {
            thread.root_cohort(policy).await.unwrap()
        };
        let output_gate = Arc::new(Gate::default());
        let constructor_gate = Arc::new(Gate::default());
        let output_release = Release(output_gate.clone());
        let constructor_release = Release(constructor_gate.clone());
        let plans = [
            Arc::new(Plan {
                builds: AtomicUsize::new(0),
                drops: AtomicUsize::new(0),
                constructor: None,
                disposal: Some(output_gate.clone()),
            }),
            Arc::new(Plan {
                builds: AtomicUsize::new(0),
                drops: AtomicUsize::new(0),
                constructor: Some(constructor_gate.clone()),
                disposal: None,
            }),
        ];
        let mut actors: Vec<BoxedActorRef> = Vec::new();
        for (index, plan) in plans.iter().enumerate() {
            let plan = plan.clone();
            let path = format!("/prepared-{index}");
            let reference: BoxedActorRef = if let Some(system) = &actix_system {
                system
                    .spawn_root_factory_in_cohort(move || ActixOutput(plan.make()), &path, &cohort)
                    .await
                    .unwrap()
            } else {
                Box::new(
                    thread
                        .spawn_supervised_in_cohort(
                            move || ThreadOutput(plan.make()),
                            &path,
                            Default::default(),
                            &cohort,
                        )
                        .await
                        .unwrap(),
                )
            };
            actors.push(reference);
        }
        let independent = other
            .spawn_root_typed_thread(
                ThreadOutput(
                    Arc::new(Plan {
                        builds: AtomicUsize::new(1),
                        drops: AtomicUsize::new(0),
                        constructor: None,
                        disposal: None,
                    })
                    .make(),
                ),
                EmptyConfig,
            )
            .await
            .unwrap();
        assert!(actors[0].send(Box::new(true)).await.is_err());
        tokio::time::timeout(WAIT, constructor_gate.entered.notified())
            .await
            .unwrap();
        // Inspect the actual epoch vote, so cancellation occurs after one
        // output is staged, while its sibling constructor is still blocked.
        tokio::time::timeout(WAIT, async {
            loop {
                let prepared = cohort
                    .admission
                    .scope
                    .active_recovery
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .is_some_and(|recovery| {
                        recovery
                            .state
                            .lock()
                            .unwrap()
                            .next
                            .values()
                            .any(|step| *step == 2)
                    });
                if prepared {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("no member reached the prepared barrier");
        let slots = if actix {
            Arc::new(tokio::sync::Semaphore::new(8))
        } else {
            thread.cleanup_slots.clone()
        };
        let mut pending: Vec<_> =
            std::iter::from_fn(|| owned_payload::try_pending_cleanup_permit(&slots)).collect();
        assert!(pending.len() >= owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT);
        if actix {
            assert!(pending.len() > owned_payload::MAX_PENDING_CLEANUPS_PER_SLOT);
        }
        assert!(owned_payload::try_pending_cleanup_permit(&slots).is_none());
        let mut stopped = cohort.stop();
        assert!(futures::poll!(&mut stopped).is_pending());
        drop(stopped);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), output_gate.entered.notified())
                .await
                .is_err(),
            "saturated cohort cancellation ran prepared Drop before admission"
        );
        tokio::time::timeout(WAIT, independent.stop())
            .await
            .expect("prepared cancellation blocked another system's stop")
            .unwrap();
        tokio::time::timeout(WAIT, other.retirement_runtime().spawn(async { 7 }))
            .await
            .expect("prepared cancellation blocked shared driver heartbeat")
            .unwrap();
        drop(pending.pop());
        tokio::time::timeout(WAIT, output_gate.entered.notified())
            .await
            .expect("returned capacity did not admit prepared disposal");
        tokio::time::timeout(WAIT, other.retirement_runtime().spawn(async { 11 }))
            .await
            .expect("prepared destructor blocked shared driver heartbeat")
            .unwrap();
        let mut first = cohort.stop();
        let mut second = cohort.stop();
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        drop(output_release);
        drop(constructor_release);
        drop(pending);
        let (a, b) = tokio::time::timeout(WAIT, async { tokio::join!(first, second) })
            .await
            .expect("actual output disposal did not finish cohort receipts");
        let error = a.unwrap_err().to_string();
        assert!(
            error.contains("saturated prepared output failure"),
            "{error}"
        );
        assert_eq!(error, b.unwrap_err().to_string());
        for plan in &plans {
            assert_eq!(plan.builds.load(Ordering::SeqCst), 2);
            assert_eq!(plan.drops.load(Ordering::SeqCst), 2);
        }
        AsyncEngineRuntime::shutdown(other.as_ref()).await.unwrap();
        let _ = AsyncEngineRuntime::shutdown(thread.as_ref()).await;
        if let Some(system) = actix_system {
            let _ = system.shutdown().await;
        }
    });
}

#[test]
fn thread_prepared_saturation_preserves_shared_driver() {
    isolated(
        "children::recovery::prepared_saturation_contracts::thread_prepared_saturation_preserves_shared_driver",
        || saturated_prepared(false),
    );
}
#[test]
fn actix_prepared_saturation_preserves_shared_driver() {
    isolated(
        "children::recovery::prepared_saturation_contracts::actix_prepared_saturation_preserves_shared_driver",
        || saturated_prepared(true),
    );
}
