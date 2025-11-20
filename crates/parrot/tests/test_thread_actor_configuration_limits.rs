//! Exact limits are pure validation: never allocate the extreme ring or stack.
use parrot::thread::{
    SchedulingMode, SupervisorStrategy, ThreadActorConfig, ThreadActorSystemConfig,
};
use parrot_config::ConfigError;
use std::time::Duration;

fn rejected(config: ThreadActorConfig, expected: &str) {
    match config.validate() {
        Err(ConfigError::Invalid { key, reason }) => {
            assert_eq!(key, expected);
            assert!(!reason.is_empty());
        }
        result => panic!("expected {expected} configuration rejection: {result:?}"),
    }
}
#[test]
fn mailbox_boundaries_are_identical_for_all_scheduling_modes() {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 1,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "limit".into(),
        },
    ] {
        for value in [1, 1_048_576] {
            ThreadActorConfig {
                scheduling_mode: Some(mode.clone()),
                mailbox_capacity: Some(value),
                ..Default::default()
            }
            .validate()
            .unwrap();
        }
        for value in [0, 1_048_577, usize::MAX] {
            rejected(
                ThreadActorConfig {
                    scheduling_mode: Some(mode.clone()),
                    mailbox_capacity: Some(value),
                    ..Default::default()
                },
                "actor.mailbox_capacity",
            );
        }
    }
}
#[test]
fn stack_limit_includes_zero_and_64_mib_without_allocating_threads() {
    assert_eq!(ThreadActorConfig::MAX_THREAD_STACK_SIZE, 67_108_864);
    for value in [0, 1, 3 * 1024 * 1024, 67_108_864] {
        ThreadActorConfig {
            thread_stack_size: Some(value),
            ..Default::default()
        }
        .validate()
        .unwrap();
    }
    for value in [67_108_865, usize::MAX] {
        rejected(
            ThreadActorConfig {
                thread_stack_size: Some(value),
                ..Default::default()
            },
            "actor.thread_stack_size",
        );
    }
}
#[test]
fn batch_size_limit_is_inclusive_and_does_not_change_scheduler_budget() {
    for value in [1, 256] {
        ThreadActorConfig {
            message_batch_size: Some(value),
            scheduling_mode: Some(SchedulingMode::SharedPool {
                max_messages_per_run: 1,
            }),
            ..Default::default()
        }
        .validate()
        .unwrap();
    }
    for value in [0, 257, usize::MAX] {
        rejected(
            ThreadActorConfig {
                message_batch_size: Some(value),
                ..Default::default()
            },
            "actor.message_batch_size",
        );
    }
    rejected(
        ThreadActorConfig {
            scheduling_mode: Some(SchedulingMode::SharedPool {
                max_messages_per_run: 0,
            }),
            ..Default::default()
        },
        "actor.scheduling_mode.max_messages_per_run",
    );
}
#[test]
fn durations_preserve_zero_but_reject_unrepresentable_deadlines() {
    ThreadActorConfig {
        ask_timeout: Some(Duration::ZERO),
        idle_sleep_duration: Some(Duration::ZERO),
        supervisor_strategy: Some(SupervisorStrategy::Restart {
            max_retries: 0,
            within: Duration::ZERO,
        }),
        ..Default::default()
    }
    .validate()
    .unwrap();
    rejected(
        ThreadActorConfig {
            ask_timeout: Some(Duration::MAX),
            ..Default::default()
        },
        "actor.ask_timeout",
    );
    rejected(
        ThreadActorConfig {
            idle_sleep_duration: Some(Duration::MAX),
            ..Default::default()
        },
        "actor.idle_sleep_duration",
    );
    rejected(
        ThreadActorConfig {
            supervisor_strategy: Some(SupervisorStrategy::Restart {
                max_retries: 1,
                within: Duration::MAX,
            }),
            ..Default::default()
        },
        "actor.supervisor_strategy.within",
    );
}
#[test]
fn merging_preserves_explicit_zero_values_and_inherits_only_omitted_fields() {
    let defaults = ThreadActorSystemConfig::default();
    let override_config = ThreadActorConfig {
        ask_timeout: Some(Duration::ZERO),
        idle_sleep_duration: Some(Duration::ZERO),
        thread_stack_size: Some(0),
        message_batch_size: Some(256),
        mailbox_capacity: Some(1),
        scheduling_mode: Some(SchedulingMode::SharedPool {
            max_messages_per_run: 1,
        }),
        ..Default::default()
    };
    let merged = defaults.merge_with_actor_config(&override_config);
    merged.validate().unwrap();
    assert_eq!(merged.ask_timeout, Some(Duration::ZERO));
    assert_eq!(merged.idle_sleep_duration, Some(Duration::ZERO));
    assert_eq!(merged.thread_stack_size, Some(0));
    assert_eq!(merged.mailbox_capacity, Some(1));
    assert_eq!(merged.message_batch_size, Some(256));
    assert!(matches!(
        merged.scheduling_mode,
        Some(SchedulingMode::SharedPool {
            max_messages_per_run: 1
        })
    ));
    let inherited = defaults.merge_with_actor_config(&ThreadActorConfig::default());
    inherited.validate().unwrap();
    assert_eq!(
        inherited.mailbox_capacity,
        Some(defaults.default_mailbox_capacity)
    );
    assert_eq!(inherited.ask_timeout, Some(defaults.default_ask_timeout));
}
