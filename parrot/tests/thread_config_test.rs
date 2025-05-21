// Integration tests for config types in parrot::thread::config

use parrot::thread::config::*; // Revert to standard integration test import
use std::time::Duration;

#[test]
fn test_system_config_defaults() {
    let config = ThreadActorSystemConfig::default();

    assert_eq!(config.shared_pool_size, num_cpus::get());
    assert!(matches!(config.default_scheduling_mode, SchedulingMode::SharedPool { max_messages_per_run: 10 }));
    assert_eq!(config.default_mailbox_capacity, 1024);
    assert_eq!(config.default_ask_timeout, Duration::from_secs(5));
    assert!(matches!(config.default_supervisor_strategy, SupervisorStrategy::Restart { max_retries: 3, within: _ }));
    assert_eq!(config.default_backpressure_strategy, BackpressureStrategy::Block);
    // Add checks for other defaults if they become non-optional
    // assert!(config.dedicated_thread_affinity_strategy.is_none());
    // assert_eq!(config.thread_name_prefix, "parrot-thread-worker-");
}

#[test]
fn test_actor_config_defaults() {
    let config = ThreadActorConfig::default();

    assert!(config.scheduling_mode.is_none());
    assert!(config.mailbox_capacity.is_none());
    assert!(config.supervisor_strategy.is_none());
    assert!(config.ask_timeout.is_none());
    assert!(config.backpressure_strategy.is_none());
    // Add checks for other defaults
    // assert!(config.core_affinity.is_none());
    // assert!(config.dispatcher.is_none());
}

// Optional: Test Debug formatting if needed (can be verbose)
#[test]
fn test_config_debug_format() {
    let sys_config = ThreadActorSystemConfig::default();
    let actor_config = ThreadActorConfig::default();
    println!("System Config Default Debug: {:?}", sys_config);
    println!("Actor Config Default Debug: {:?}", actor_config);
    // Basic check to ensure Debug trait doesn't panic
    assert!(format!("{:?}", sys_config).contains("shared_pool_size"));
    assert!(format!("{:?}", actor_config).contains("scheduling_mode"));
} 