//! Native settings are checked before allocating workers or publishing a system.
use futures::FutureExt;
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    system::ParrotActorSystem,
};
use parrot_api::{
    supervisor::{BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType},
    system::{ActorSystemConfig, GuardianConfig, SystemError},
};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn settings() -> ActixSystemSettings {
    ActixSystemSettings {
        arbiter_count: 1,
        ..Default::default()
    }
}

fn invalid(case: usize) -> (ActixSystemSettings, &'static str) {
    let mut settings = settings();
    let field = match case {
        0 => {
            settings.arbiter_count = usize::MAX;
            "arbiter_count"
        }
        1 => {
            settings.mailbox_capacity = 0;
            "mailbox_capacity"
        }
        2 => {
            settings.mailbox_capacity = 1_048_577;
            "mailbox_capacity"
        }
        3 => {
            settings.max_context_tasks = 65_537;
            "max_context_tasks"
        }
        4 => {
            settings.ask_timeout = Duration::MAX;
            "ask_timeout"
        }
        5 => {
            settings.shutdown_timeout = Duration::MAX;
            "shutdown_timeout"
        }
        _ => unreachable!(),
    };
    (settings, field)
}

fn assert_invalid(error: SystemError, field: &str) {
    assert!(
        matches!(error, SystemError::InitializationError(_)),
        "{error}"
    );
    assert!(
        error.to_string().contains(&format!("actix.{field}")),
        "{error}"
    );
}

#[tokio::test]
async fn native_invalid_settings_are_rejected_before_requiring_an_actix_host() {
    assert!(actix::System::try_current().is_none());
    // In particular, never try to allocate usize::MAX native threads to check a limit.
    for case in 0..6 {
        let (settings, field) = invalid(case);
        let error = ActixActorSystem::with_settings(settings)
            .await
            .err()
            .expect("invalid settings accepted");
        assert_invalid(error, field);
    }
}

async fn reject_on_host(case: usize) {
    let (settings, field) = invalid(case);
    let error = match ActixActorSystem::with_settings(settings).await {
        Err(error) => error,
        Ok(system) => {
            // An old implementation may accept Duration::MAX; do not invoke its
            // overflowing public shutdown deadline while reporting that defect.
            drop(system);
            panic!("accepted invalid actix.{field}");
        }
    };
    assert_invalid(error, field);
}

#[actix::test]
async fn native_zero_mailbox_is_a_configuration_error() {
    reject_on_host(1).await;
}
#[actix::test]
async fn native_mailbox_limit_is_checked_before_publication() {
    reject_on_host(2).await;
}
#[actix::test]
async fn native_context_task_limit_is_checked_before_publication() {
    reject_on_host(3).await;
}
#[actix::test]
async fn native_ask_deadline_is_checked_before_publication() {
    reject_on_host(4).await;
}
#[actix::test]
async fn native_shutdown_deadline_is_checked_before_publication() {
    reject_on_host(5).await;
}

struct Capture {
    dropped: Arc<AtomicUsize>,
    hostile: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
        assert!(!self.hostile, "native settings guardian disposal witness");
    }
}
fn guardian(dropped: Arc<AtomicUsize>, hostile: bool) -> GuardianConfig {
    let capture = Capture { dropped, hostile };
    GuardianConfig {
        supervision_strategy: SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 1,
            within: Duration::from_secs(60),
            decider: BasicDecisionFn::new(move |_| {
                let _ = &capture;
                SupervisionDecision::Stop
            }),
        }),
        ..Default::default()
    }
}

#[actix::test]
async fn invalid_native_and_facade_settings_join_guardian_disposal() {
    for facade in [false, true] {
        for hostile in [false, true] {
            // Mailbox zero is already rejected by the old constructor, allowing
            // this test to distinguish actual disposal errors from admission.
            for case in [1, 2, 3, 4, 5] {
                let dropped = Arc::new(AtomicUsize::new(0));
                let guardian = guardian(dropped.clone(), hostile);
                let (settings, field) = invalid(case);
                let outcome = AssertUnwindSafe(async {
                    if facade {
                        match ParrotActorSystem::with_actix(
                            ActorSystemConfig {
                                guardian_config: guardian,
                                ..Default::default()
                            },
                            settings,
                        )
                        .await
                        {
                            Err(error) => error,
                            Ok(system) => {
                                drop(system);
                                panic!("facade accepted {field}")
                            }
                        }
                    } else {
                        match ActixActorSystem::with_guardian(settings, guardian).await {
                            Err(error) => error,
                            Ok(system) => {
                                drop(system);
                                panic!("native accepted {field}")
                            }
                        }
                    }
                })
                .catch_unwind()
                .await
                .expect("startup unwound");
                assert_eq!(dropped.load(Ordering::SeqCst), 1, "cleanup was not joined");
                if hostile {
                    assert!(matches!(outcome, SystemError::ActorError(_)), "{outcome}");
                    assert!(outcome.to_string().contains("disposal failed"), "{outcome}");
                } else {
                    assert_invalid(outcome, field);
                }
            }
        }
    }
}

#[actix::test]
async fn zero_native_worker_count_still_selects_one_real_worker() {
    let system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 0,
        max_context_tasks: 0,
        ..settings()
    })
    .await
    .unwrap();
    assert_eq!(system.arbiter_size(), 1);
    assert_eq!(system.arbiter_loads(), [0]);
    system.shutdown().await.unwrap();
}
