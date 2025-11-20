use parrot_config::ParrotConfig;

fn config() -> ParrotConfig {
    let mut config = ParrotConfig::builder();
    config.thread.shared_pool_size = Some(1);
    config.thread.shared_burst_workers_max = Some(0);
    config
}

fn rejected(config: ParrotConfig, key: &str) {
    let error = config
        .build()
        .expect_err("invalid thread input built")
        .to_string();
    assert!(error.contains(key), "{error}");
}

#[test]
fn capacity_upper_boundaries_are_inclusive() {
    let mut config = config();
    config.thread.shared_pool_size = Some(4096);
    config.thread.shared_queue_capacity = Some(1_048_576);
    config.thread.default_mailbox_capacity = Some(1_048_576);
    config.thread.max_dedicated_threads = Some(4096);
    config.thread.max_context_tasks = Some(65_536);
    let resolved = config.build().unwrap();
    assert_eq!(resolved.thread.shared_pool_size, 4096);
    assert_eq!(resolved.thread.max_context_tasks, 65_536);
}

#[test]
fn excessive_core_worker_capacity_is_rejected() {
    for value in [4097, usize::MAX] {
        let mut config = config();
        config.thread.shared_pool_size = Some(value);
        rejected(config, "thread.shared_pool_size");
    }
}

#[test]
fn core_and_burst_workers_share_one_nonwrapping_budget() {
    let mut exact = config();
    exact.thread.shared_burst_workers_max = Some(4095);
    assert_eq!(exact.build().unwrap().thread.shared_burst_workers_max, 4095);
    for value in [4096, usize::MAX] {
        let mut config = config();
        config.thread.shared_burst_workers_max = Some(value);
        rejected(config, "thread.shared_burst_workers_max");
    }
}

#[test]
fn excessive_ready_and_mailbox_capacities_are_rejected() {
    for value in [1_048_577, usize::MAX] {
        let mut queue = config();
        queue.thread.shared_queue_capacity = Some(value);
        rejected(queue, "thread.shared_queue_capacity");
        let mut mailbox = config();
        mailbox.thread.default_mailbox_capacity = Some(value);
        rejected(mailbox, "thread.default_mailbox_capacity");
    }
}

#[test]
fn excessive_dedicated_and_context_task_capacities_are_rejected() {
    for value in [4097, usize::MAX] {
        let mut config = config();
        config.thread.max_dedicated_threads = Some(value);
        rejected(config, "thread.max_dedicated_threads");
    }
    for value in [65_537, usize::MAX] {
        let mut config = config();
        config.thread.max_context_tasks = Some(value);
        rejected(config, "thread.max_context_tasks");
    }
}

#[test]
fn intentional_disabling_immediate_timeouts_and_effective_priority_remain_valid() {
    let mut config = config();
    config.thread.max_dedicated_threads = Some(0);
    config.thread.max_context_tasks = Some(0);
    config.thread.shared_burst_backlog_threshold_ms = Some(0);
    config.thread.shared_burst_idle_timeout_ms = Some(0);
    config.thread.default_ask_timeout_ms = Some(0);
    config.thread.shutdown_timeout_ms = Some(0);
    let resolved = config.clone().build().unwrap();
    assert_eq!(resolved.thread.default_ask_timeout_ms, 0);
    assert_eq!(resolved.thread.max_dedicated_threads, 0);
    config.thread.shared_pool_size = Some(usize::MAX);
    let higher = ParrotConfig::builder()
        .thread_shared_pool_size(2)
        .with_fallback(config);
    assert_eq!(higher.build().unwrap().thread.shared_pool_size, 2);
}
