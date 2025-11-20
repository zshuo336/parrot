//! A rejected constructor still owns the real retirement of captured policies.
use super::*;
use parrot_api::{
    supervisor::{BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType},
    system::GuardianConfig,
};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Gate {
    released: Mutex<bool>,
    wake: Condvar,
    entered: tokio::sync::Notify,
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}
struct Capture {
    gate: Arc<Gate>,
    hostile: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.gate.entered.notify_one();
        let mut released = self.gate.released.lock().unwrap();
        while !*released {
            released = self.gate.wake.wait(released).unwrap();
        }
        drop(released);
        assert!(!self.hostile, "invalid deadline policy disposal witness");
    }
}

async fn rejection_joins_cleanup(actix: bool) {
    for key in ["actor_creation", "message_handling", "system_shutdown"] {
        for hostile in [false, true] {
            let gate = Arc::new(Gate::default());
            let release = Release(gate.clone());
            let capture = Capture {
                gate: gate.clone(),
                hostile,
            };
            let guardian = GuardianConfig {
                supervision_strategy: SupervisorStrategyType::OneForAll(OneForAllStrategy {
                    max_restarts: 1,
                    within: Duration::from_secs(1),
                    decider: BasicDecisionFn::new(move |_| {
                        let _ = &capture;
                        SupervisionDecision::Stop
                    }),
                }),
                ..Default::default()
            };
            let mut timeouts = SystemTimeouts::default();
            match key {
                "actor_creation" => timeouts.actor_creation = Duration::MAX,
                "message_handling" => timeouts.message_handling = Duration::MAX,
                _ => timeouts.system_shutdown = Duration::MAX,
            }
            let rejection = async move {
                if actix {
                    ActixActorSystem::with_guardian_and_timeouts(
                        ActixSystemSettings {
                            arbiter_count: 1,
                            ..Default::default()
                        },
                        guardian,
                        timeouts,
                    )
                    .await
                    .err()
                    .expect("invalid deadline accepted")
                } else {
                    ThreadActorSystem::with_guardian_and_timeouts(
                        ThreadActorSystemConfig {
                            shared_pool_size: 1,
                            shared_burst_workers_max: 0,
                            ..Default::default()
                        },
                        guardian,
                        timeouts,
                    )
                    .await
                    .expect_err("invalid deadline accepted")
                }
            };
            tokio::pin!(rejection);
            assert!(
                futures::poll!(rejection.as_mut()).is_pending(),
                "{key}: rejection preceded policy disposal"
            );
            tokio::time::timeout(Duration::from_secs(8), gate.entered.notified())
                .await
                .unwrap();
            assert!(
                futures::poll!(rejection.as_mut()).is_pending(),
                "{key}: blocked disposal was not joined"
            );
            drop(release);
            let error = tokio::time::timeout(Duration::from_secs(8), rejection)
                .await
                .unwrap();
            if hostile {
                assert!(matches!(error, SystemError::ActorError(_)), "{error}");
                assert!(error.to_string().contains("disposal failed"), "{error}");
            } else {
                assert!(
                    matches!(error, SystemError::InitializationError(_)),
                    "{error}"
                );
                assert!(error.to_string().contains(key), "{error}");
            }
        }
    }
}

#[tokio::test]
async fn thread_invalid_deadlines_join_policy_disposal() {
    rejection_joins_cleanup(false).await;
}

#[actix::test]
async fn actix_invalid_deadlines_join_policy_disposal() {
    rejection_joins_cleanup(true).await;
}

#[actix::test]
async fn effective_system_shutdown_override_precedes_native_deadline_validation() {
    let system = ActixActorSystem::with_guardian_and_timeouts(
        ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: Duration::MAX,
            ..Default::default()
        },
        GuardianConfig::default(),
        SystemTimeouts {
            system_shutdown: Duration::from_secs(8),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    system.shutdown().await.unwrap();
}
