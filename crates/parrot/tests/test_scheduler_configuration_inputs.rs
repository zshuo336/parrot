use parrot::thread::{
    SystemError, ThreadActorConfig,
    scheduler::{
        MAX_MESSAGES_PER_BATCH, ThreadSchedulerFactory,
        dedicated_thread::{DedicatedThreadConfig, DedicatedThreadScheduler},
        queue::SchedulingQueue,
        sharded::ShardedScheduler,
        shared::{SharedThreadPool, SharedThreadPoolConfig, Worker, WorkerConfig},
    },
};
use parrot_config::ThreadResolved;
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

fn invalid<T>(result: Result<T, SystemError>, key: &str) {
    assert!(
        matches!(result, Err(SystemError::ConfigError(ref message)) if message.contains(key)),
        "expected configuration error for {key}"
    );
}

#[test]
fn scheduler_count_limits_are_checked_without_allocating_workers_or_queues() {
    let mut shared = SharedThreadPoolConfig {
        pool_size: 1,
        burst_workers_max: 0,
        ..Default::default()
    };
    for value in [0, ThreadResolved::MAX_WORKERS + 1, usize::MAX] {
        shared.pool_size = value;
        invalid(shared.validate(), "scheduler.shared.pool_size");
    }
    shared.pool_size = ThreadResolved::MAX_WORKERS;
    shared.validate().unwrap();
    shared.burst_workers_max = 1;
    invalid(shared.validate(), "scheduler.shared.burst_workers_max");
    shared.pool_size = 1;
    shared.burst_workers_max = usize::MAX;
    invalid(shared.validate(), "scheduler.shared.burst_workers_max");
    shared.burst_workers_max = ThreadResolved::MAX_WORKERS - 1;
    shared.max_queue_capacity = ThreadResolved::MAX_QUEUE_CAPACITY;
    shared.max_messages_per_batch = MAX_MESSAGES_PER_BATCH;
    shared.validate().unwrap();
    for value in [0, ThreadResolved::MAX_QUEUE_CAPACITY + 1, usize::MAX] {
        shared.max_queue_capacity = value;
        invalid(shared.validate(), "scheduler.shared.max_queue_capacity");
    }
    shared.max_queue_capacity = 1;
    for value in [0, MAX_MESSAGES_PER_BATCH + 1, usize::MAX] {
        shared.max_messages_per_batch = value;
        invalid(shared.validate(), "scheduler.shared.max_messages_per_batch");
        invalid(
            WorkerConfig {
                batch_size: value,
                ..Default::default()
            }
            .validate(),
            "scheduler.worker.batch_size",
        );
        invalid(
            DedicatedThreadConfig {
                max_messages_per_batch: value,
                ..Default::default()
            }
            .validate(),
            "scheduler.dedicated.max_messages_per_batch",
        );
        invalid(
            ShardedScheduler::validate(1, value),
            "scheduler.sharded.batch_size",
        );
    }
    for value in [0, ThreadResolved::MAX_WORKERS + 1, usize::MAX] {
        invalid(
            ShardedScheduler::validate(value, 1),
            "scheduler.sharded.shards",
        );
    }
    ShardedScheduler::validate(ThreadResolved::MAX_WORKERS, MAX_MESSAGES_PER_BATCH).unwrap();
    WorkerConfig {
        batch_size: MAX_MESSAGES_PER_BATCH,
        ..Default::default()
    }
    .validate()
    .unwrap();
    for value in [ThreadResolved::MAX_WORKERS + 1, usize::MAX] {
        invalid(
            DedicatedThreadConfig {
                max_threads: value,
                ..Default::default()
            }
            .validate(),
            "scheduler.dedicated.max_threads",
        );
    }
    DedicatedThreadConfig {
        max_threads: 0,
        default_thread_stack_size: 0,
        ..Default::default()
    }
    .validate()
    .unwrap();
    DedicatedThreadConfig {
        max_threads: ThreadResolved::MAX_WORKERS,
        default_thread_stack_size: ThreadActorConfig::MAX_THREAD_STACK_SIZE,
        max_messages_per_batch: MAX_MESSAGES_PER_BATCH,
        ..Default::default()
    }
    .validate()
    .unwrap();
    for value in [ThreadActorConfig::MAX_THREAD_STACK_SIZE + 1, usize::MAX] {
        invalid(
            DedicatedThreadConfig {
                default_thread_stack_size: value,
                ..Default::default()
            }
            .validate(),
            "scheduler.dedicated.default_thread_stack_size",
        );
    }
}

#[test]
fn durations_follow_their_actual_clock_consumers() {
    for duration in [Duration::ZERO, Duration::from_millis(1)] {
        SharedThreadPoolConfig {
            idle_sleep_duration: duration,
            burst_backlog_threshold: duration,
            burst_idle_timeout: duration,
            ..Default::default()
        }
        .validate()
        .unwrap();
        DedicatedThreadConfig {
            idle_sleep_duration: duration,
            ..Default::default()
        }
        .validate()
        .unwrap();
        WorkerConfig {
            idle_sleep_duration: duration,
            ..Default::default()
        }
        .validate()
        .unwrap();
    }
    invalid(
        SharedThreadPoolConfig {
            idle_sleep_duration: Duration::MAX,
            ..Default::default()
        }
        .validate(),
        "scheduler.shared.idle_sleep_duration",
    );
    invalid(
        SharedThreadPoolConfig {
            burst_workers_max: 1,
            burst_backlog_threshold: Duration::MAX,
            ..Default::default()
        }
        .validate(),
        "scheduler.shared.burst_backlog_threshold",
    );
    invalid(
        DedicatedThreadConfig {
            idle_sleep_duration: Duration::MAX,
            ..Default::default()
        }
        .validate(),
        "scheduler.dedicated.idle_sleep_duration",
    );
    invalid(
        WorkerConfig {
            idle_sleep_duration: Duration::MAX,
            ..Default::default()
        }
        .validate(),
        "scheduler.worker.idle_sleep_duration",
    );
    SharedThreadPoolConfig {
        burst_idle_timeout: Duration::MAX,
        ..Default::default()
    }
    .validate()
    .unwrap();
    SharedThreadPoolConfig {
        burst_workers_max: 0,
        burst_backlog_threshold: Duration::MAX,
        ..Default::default()
    }
    .validate()
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn fallible_constructors_reject_before_creating_runtime_tasks() {
    let handle = tokio::runtime::Handle::current();
    let before = handle.metrics().num_alive_tasks();
    for config in [
        SharedThreadPoolConfig {
            pool_size: 0,
            ..Default::default()
        },
        SharedThreadPoolConfig {
            max_queue_capacity: 0,
            ..Default::default()
        },
        SharedThreadPoolConfig {
            max_messages_per_batch: 0,
            ..Default::default()
        },
        SharedThreadPoolConfig {
            idle_sleep_duration: Duration::MAX,
            ..Default::default()
        },
    ] {
        assert!(matches!(
            SharedThreadPool::try_new(Some(config), handle.clone()),
            Err(SystemError::ConfigError(_))
        ));
        assert_eq!(handle.metrics().num_alive_tasks(), before);
    }
    invalid(
        DedicatedThreadScheduler::try_new(Some(DedicatedThreadConfig {
            max_messages_per_batch: 0,
            ..Default::default()
        })),
        "scheduler.dedicated.max_messages_per_batch",
    );
    invalid(
        Worker::try_new(
            0,
            handle.clone(),
            Arc::new(SchedulingQueue::new(1)),
            Arc::new(AtomicBool::new(false)),
            WorkerConfig {
                batch_size: 0,
                ..Default::default()
            },
        ),
        "scheduler.worker.batch_size",
    );
    for (n, batch, key) in [
        (0, 1, "scheduler.sharded.shards"),
        (1, 0, "scheduler.sharded.batch_size"),
    ] {
        invalid(ShardedScheduler::try_start(n, batch), key);
    }
    assert_eq!(handle.metrics().num_alive_tasks(), before);
}

#[tokio::test(flavor = "current_thread")]
async fn group_rejects_bad_dedicated_input_before_starting_the_valid_shared_pool() {
    let handle = tokio::runtime::Handle::current();
    let before = handle.metrics().num_alive_tasks();
    let result = ThreadSchedulerFactory::new(handle.clone()).try_create_scheduler_group(
        Some(SharedThreadPoolConfig {
            pool_size: 1,
            burst_workers_max: 0,
            ..Default::default()
        }),
        Some(DedicatedThreadConfig {
            idle_sleep_duration: Duration::MAX,
            ..Default::default()
        }),
    );
    invalid(result, "scheduler.dedicated.idle_sleep_duration");
    assert_eq!(handle.metrics().num_alive_tasks(), before);
}
