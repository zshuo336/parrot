//! Native admission limits are checked before constructing Actix resources.
use super::ActixSystemSettings;
use parrot_api::system::SystemError;
use parrot_config::ThreadResolved;

pub(super) fn validate_workers(count: usize) -> Result<(), SystemError> {
    if count > ThreadResolved::MAX_WORKERS {
        return Err(SystemError::InitializationError(format!(
            "actix.arbiter_count must be in 0..={}",
            ThreadResolved::MAX_WORKERS
        )));
    }
    Ok(())
}

impl ActixSystemSettings {
    /// Check resource limits and monotonic deadlines without allocating workers.
    /// Zero arbiters keeps the native compatibility rule of selecting one worker.
    /// These per-system defaults are admission limits, not a node memory budget.
    pub fn validate(&self) -> Result<(), SystemError> {
        validate_workers(self.arbiter_count)?;
        for (key, value, minimum, maximum) in [
            (
                "mailbox_capacity",
                self.mailbox_capacity,
                1,
                ThreadResolved::MAX_QUEUE_CAPACITY,
            ),
            (
                "max_context_tasks",
                self.max_context_tasks,
                0,
                ThreadResolved::MAX_CONTEXT_TASKS,
            ),
        ] {
            if !(minimum..=maximum).contains(&value) {
                return Err(SystemError::InitializationError(format!(
                    "actix.{key} must be in {minimum}..={maximum}"
                )));
            }
        }
        for (key, duration) in [
            ("ask_timeout", self.ask_timeout),
            ("shutdown_timeout", self.shutdown_timeout),
        ] {
            parrot_config::validate_thread_duration(&format!("actix.{key}"), duration)
                .map_err(|error| SystemError::InitializationError(error.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actix::ArbiterPool;
    use std::time::Duration;

    #[test]
    fn exact_native_limits_are_valid_without_allocating_resources() {
        ActixSystemSettings::default().validate().unwrap();
        for arbiter_count in [0, 1, ThreadResolved::MAX_WORKERS] {
            for max_context_tasks in [0, ThreadResolved::MAX_CONTEXT_TASKS] {
                ActixSystemSettings {
                    arbiter_count,
                    max_context_tasks,
                    mailbox_capacity: ThreadResolved::MAX_QUEUE_CAPACITY,
                    ask_timeout: Duration::ZERO,
                    shutdown_timeout: Duration::ZERO,
                    ..Default::default()
                }
                .validate()
                .unwrap();
            }
        }
    }

    #[test]
    fn next_and_extreme_native_limits_are_rejected_without_allocation() {
        for field in ["arbiter_count", "mailbox_capacity", "max_context_tasks"] {
            let maximum = match field {
                "arbiter_count" => ThreadResolved::MAX_WORKERS,
                "mailbox_capacity" => ThreadResolved::MAX_QUEUE_CAPACITY,
                _ => ThreadResolved::MAX_CONTEXT_TASKS,
            };
            for value in [maximum + 1, usize::MAX] {
                let mut settings = ActixSystemSettings::default();
                // Recheck public mutable settings after an earlier successful validation.
                settings.validate().unwrap();
                match field {
                    "arbiter_count" => settings.arbiter_count = value,
                    "mailbox_capacity" => settings.mailbox_capacity = value,
                    _ => settings.max_context_tasks = value,
                }
                let error = settings.validate().unwrap_err();
                assert!(
                    error.to_string().contains(&format!("actix.{field}")),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn worker_pool_checks_limits_and_host_before_allocating_workers() {
        for count in [ThreadResolved::MAX_WORKERS + 1, usize::MAX] {
            let error = ArbiterPool::try_new(count).unwrap_err();
            assert!(matches!(error, SystemError::InitializationError(_)));
            assert!(error.to_string().contains("actix.arbiter_count"), "{error}");
        }
        let error = ArbiterPool::try_new(1).unwrap_err();
        assert!(
            error.to_string().contains("running Actix System"),
            "{error}"
        );
    }

    #[actix::test]
    async fn system_convenience_constructor_checks_worker_limit() {
        let error = crate::actix::ActixActorSystem::with_arbiter_count(usize::MAX)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("actix.arbiter_count"), "{error}");
        let pool = ArbiterPool::try_new(0).unwrap();
        assert_eq!(pool.size(), 1);
        drop(pool);
    }
}
