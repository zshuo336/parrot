use parrot::thread::{
    config::{SchedulingMode, SupervisorStrategy, ThreadActorSystemConfig},
    error::SystemError,
    system::ThreadActorSystem,
};
use std::time::Duration;

fn config() -> ThreadActorSystemConfig {
    ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    }
}

#[test]
fn ambient_constructors_return_missing_runtime_errors() {
    assert!(matches!(
        ThreadActorSystem::try_new(config()),
        Err(SystemError::ThreadSetupError(_))
    ));
    assert!(matches!(
        ThreadActorSystem::try_shared(config()),
        Err(SystemError::ThreadSetupError(_))
    ));
}

#[test]
fn explicit_handle_works_without_an_ambient_runtime() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let system =
        ThreadActorSystem::try_with_runtime_handle(config(), runtime.handle().clone()).unwrap();
    runtime.block_on(system.shutdown_internal()).unwrap();
    let system =
        ThreadActorSystem::try_shared_with_handle(config(), runtime.handle().clone()).unwrap();
    runtime.block_on(system.shutdown_internal()).unwrap();
}

#[tokio::test]
async fn rejection_spawns_no_runtime_tasks_through_any_fallible_constructor() {
    let runtime = tokio::runtime::Handle::current();
    let before = runtime.metrics().num_alive_tasks();
    let mut invalid = config();
    invalid.shared_pool_size = usize::MAX;
    assert!(ThreadActorSystem::try_new(invalid.clone()).is_err());
    assert!(ThreadActorSystem::try_shared(invalid.clone()).is_err());
    assert!(ThreadActorSystem::try_with_runtime_handle(invalid.clone(), runtime.clone()).is_err());
    assert!(ThreadActorSystem::try_shared_with_handle(invalid, runtime.clone()).is_err());
    assert_eq!(runtime.metrics().num_alive_tasks(), before);
}

#[tokio::test]
async fn unrepresentable_deadlines_are_rejected_before_system_ownership() {
    for (key, change) in [
        ("thread.default_ask_timeout", 0),
        ("thread.shutdown_timeout", 1),
        ("thread.default_supervisor_strategy.within", 2),
    ] {
        let mut invalid = config();
        match change {
            0 => invalid.default_ask_timeout = Duration::MAX,
            1 => invalid.shutdown_timeout = Duration::MAX,
            _ => {
                invalid.default_supervisor_strategy = SupervisorStrategy::Restart {
                    max_retries: 1,
                    within: Duration::MAX,
                }
            }
        }
        let error = invalid
            .validate()
            .expect_err("unrepresentable deadline validated")
            .to_string();
        assert!(error.contains(key), "{error}");
        let error = ThreadActorSystem::try_shared(invalid)
            .unwrap_err()
            .to_string();
        assert!(error.contains(key), "{error}");
    }
}

#[tokio::test]
async fn zero_deadlines_and_disabled_features_remain_valid() {
    let config = ThreadActorSystemConfig {
        max_context_tasks: 0,
        max_dedicated_threads: 0,
        default_ask_timeout: Duration::ZERO,
        shutdown_timeout: Duration::ZERO,
        default_supervisor_strategy: SupervisorStrategy::Restart {
            max_retries: 0,
            within: Duration::ZERO,
        },
        shared_burst_backlog_threshold_ms: 0,
        shared_burst_idle_timeout_ms: 0,
        ..config()
    };
    config.validate().unwrap();
    let system = ThreadActorSystem::try_shared(config).unwrap();
    // Drop must also accept an immediate shutdown deadline.
    drop(system);
    tokio::task::yield_now().await;
}

#[tokio::test]
async fn a_zero_default_scheduler_budget_is_rejected() {
    let mut invalid = config();
    invalid.default_scheduling_mode = SchedulingMode::SharedPool {
        max_messages_per_run: 0,
    };
    let error = ThreadActorSystem::try_shared(invalid)
        .unwrap_err()
        .to_string();
    assert!(error.contains("max_messages_per_run"), "{error}");
}

#[test]
fn legacy_constructors_reject_invalid_configuration_before_runtime_lookup() {
    let mut invalid = config();
    invalid.shared_pool_size = 0;
    let error = ThreadActorSystem::try_new(invalid.clone()).unwrap_err();
    assert!(matches!(error, SystemError::ConfigError(_)));
    let panic = std::panic::catch_unwind(|| ThreadActorSystem::new(invalid)).unwrap_err();
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(message.contains("thread.shared_pool_size"), "{message}");
}
