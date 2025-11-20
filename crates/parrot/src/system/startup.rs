//! Facade-owned engines acquire their guardian before becoming discoverable.
use super::{ActorSystemImpl, ParrotActorSystem};
use crate::{
    actix::{ActixActorSystem, ActixSystemSettings},
    owned_payload::{OwnedPayload, PublicationInput},
    thread::{ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    runtime::{LoadBalancingStrategy, RuntimeConfig},
    system::{ActorSystemConfig, SystemError, SystemTimeouts},
    types::BoxedFuture,
};

#[cfg(test)]
#[path = "startup/input_contracts.rs"]
mod input_contracts;

/// Apply RuntimeConfig fields that have an exact native Thread meaning.
/// Unsupported scheduler policies are rejected instead of silently ignored.
pub(crate) fn apply_thread_runtime_config(
    settings: &mut ThreadActorSystemConfig,
    timeouts: &mut SystemTimeouts,
    runtime: &RuntimeConfig,
) -> Result<(), SystemError> {
    validate_runtime_config(timeouts, runtime)?;
    if !matches!(
        runtime.scheduler_config.load_balancing,
        LoadBalancingStrategy::RoundRobin
    ) {
        return Err(SystemError::InitializationError(
            "runtime load_balancing currently supports only RoundRobin for Thread".into(),
        ));
    }
    if let Some(worker_threads) = runtime.worker_threads {
        settings.shared_pool_size = worker_threads;
    }
    if runtime.scheduler_config.task_queue_capacity > 0 {
        settings.shared_queue_capacity = runtime.scheduler_config.task_queue_capacity;
    }
    apply_task_timeout(timeouts, runtime);
    Ok(())
}

pub(crate) fn apply_actix_runtime_config(
    settings: &mut ActixSystemSettings,
    timeouts: &mut SystemTimeouts,
    runtime: &RuntimeConfig,
) -> Result<(), SystemError> {
    validate_runtime_config(timeouts, runtime)?;
    if runtime.scheduler_config.task_queue_capacity > 0 {
        return Err(SystemError::InitializationError(
            "runtime task_queue_capacity has no Actix ready-queue equivalent; configure native mailbox_capacity separately".into(),
        ));
    }
    if let Some(workers) = runtime.worker_threads {
        settings.arbiter_count = workers;
    }
    settings.load_balancing = runtime.scheduler_config.load_balancing.clone();
    apply_task_timeout(timeouts, runtime);
    Ok(())
}

fn apply_task_timeout(timeouts: &mut SystemTimeouts, runtime: &RuntimeConfig) {
    if !runtime.scheduler_config.task_timeout.is_zero() {
        timeouts.message_handling = runtime.scheduler_config.task_timeout;
    }
}

fn validate_runtime_config(
    timeouts: &SystemTimeouts,
    runtime: &RuntimeConfig,
) -> Result<(), SystemError> {
    let invalid = |message: &str| SystemError::InitializationError(message.into());
    if runtime
        .worker_threads
        .is_some_and(|workers| workers == 0 || workers > parrot_config::ThreadResolved::MAX_WORKERS)
    {
        return Err(invalid("runtime worker_threads must be in 1..=4096"));
    }
    if runtime.io_threads.is_some() {
        return Err(invalid(
            "runtime io_threads cannot reconfigure the caller-owned I/O driver",
        ));
    }
    if runtime.scheduler_config.task_queue_capacity
        > parrot_config::ThreadResolved::MAX_QUEUE_CAPACITY
    {
        return Err(invalid(
            "runtime task_queue_capacity must be at most 1048576",
        ));
    }
    parrot_config::validate_thread_duration(
        "runtime.scheduler.task_timeout",
        runtime.scheduler_config.task_timeout,
    )
    .map_err(|error| SystemError::InitializationError(error.to_string()))?;
    if !runtime.scheduler_config.task_timeout.is_zero()
        && !timeouts.message_handling.is_zero()
        && timeouts.message_handling != runtime.scheduler_config.task_timeout
    {
        return Err(invalid(
            "runtime scheduler task_timeout conflicts with system message_handling timeout",
        ));
    }
    validate_timeouts(timeouts)
}

pub(crate) fn validate_timeouts(timeouts: &SystemTimeouts) -> Result<(), SystemError> {
    for (key, duration) in [
        ("actor_creation", timeouts.actor_creation),
        ("message_handling", timeouts.message_handling),
        ("system_shutdown", timeouts.system_shutdown),
    ] {
        parrot_config::validate_thread_duration(&format!("system.timeouts.{key}"), duration)
            .map_err(|error| SystemError::InitializationError(error.to_string()))?;
    }
    Ok(())
}

impl ParrotActorSystem {
    /// Start a usable facade with one managed Thread engine named `default`.
    /// The facade owns shutdown of this engine. Root supervision comes from
    /// `guardian_config`; native scheduler settings use their normal defaults.
    pub fn start(config: ActorSystemConfig) -> BoxedFuture<'static, Result<Self, SystemError>> {
        Self::with_thread(config, ThreadActorSystemConfig::default())
    }

    /// Create a default Thread engine using explicit native settings and the
    /// facade's guardian. A nonempty facade name overrides the native name.
    /// Positive system timeouts configure this engine's operation deadlines.
    /// RuntimeConfig maps worker count, ready capacity and handler timeout.
    /// Separate I/O threads and non-RoundRobin policies are rejected.
    pub fn with_thread(
        config: ActorSystemConfig,
        mut settings: ThreadActorSystemConfig,
    ) -> BoxedFuture<'static, Result<Self, SystemError>> {
        let config = OwnedPayload::unbound(config);
        Box::pin(parrot_api::trace::capture(async move {
            let mut timeouts = config.value().timeouts.clone();
            if let Err(error) = apply_thread_runtime_config(
                &mut settings,
                &mut timeouts,
                &config.value().runtime_config,
            ) {
                config.retire().await.map_err(SystemError::ActorError)?;
                return Err(error);
            }
            let mut value = config.take();
            let guardian = std::mem::take(&mut value.guardian_config);
            value.timeouts = timeouts;
            let config = OwnedPayload::unbound(value);
            if !config.value().name.is_empty() {
                settings.name = config.value().name.clone();
            }
            let guardian = config.protect(guardian);
            let facade = Self::empty(config).await?;
            let engine = ThreadActorSystem::with_guardian_and_timeouts(
                settings,
                guardian.take(),
                facade.config.value().timeouts.clone(),
            )
            .await?;
            facade.register_system(
                "default".into(),
                PublicationInput::new(ActorSystemImpl::Thread(engine)),
                None,
                true,
            )?;
            Ok(facade)
        }))
    }

    /// Create a default Actix engine using native settings and this facade's
    /// guardian. Requires a running Actix System, as the native constructor does.
    /// RuntimeConfig worker count overrides arbiter_count; task_timeout sets the
    /// handler budget; load_balancing selects actor placement on owned arbiters.
    /// Explicit I/O count and ready capacity fail.
    pub fn with_actix(
        config: ActorSystemConfig,
        mut settings: ActixSystemSettings,
    ) -> BoxedFuture<'static, Result<Self, SystemError>> {
        let config = OwnedPayload::unbound(config);
        Box::pin(parrot_api::trace::capture(async move {
            let mut timeouts = config.value().timeouts.clone();
            if let Err(error) = apply_actix_runtime_config(
                &mut settings,
                &mut timeouts,
                &config.value().runtime_config,
            ) {
                config.retire().await.map_err(SystemError::ActorError)?;
                return Err(error);
            }
            let mut value = config.take();
            let guardian = std::mem::take(&mut value.guardian_config);
            value.timeouts = timeouts;
            let config = OwnedPayload::unbound(value);
            let guardian = config.protect(guardian);
            let facade = Self::empty(config).await?;
            let engine = ActixActorSystem::with_guardian_and_timeouts(
                settings,
                guardian.take(),
                facade.config.value().timeouts.clone(),
            )
            .await?;
            facade.register_system(
                "default".into(),
                PublicationInput::new(ActorSystemImpl::Actix(engine)),
                None,
                true,
            )?;
            Ok(facade)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_api::runtime::SchedulerConfig;
    use std::time::Duration;

    #[test]
    fn actix_runtime_config_maps_every_placement_policy() {
        for strategy in [
            LoadBalancingStrategy::RoundRobin,
            LoadBalancingStrategy::Random,
            LoadBalancingStrategy::LeastLoaded,
        ] {
            let mut settings = ActixSystemSettings::default();
            let config = RuntimeConfig {
                scheduler_config: SchedulerConfig {
                    load_balancing: strategy.clone(),
                    ..Default::default()
                },
                ..Default::default()
            };
            apply_actix_runtime_config(&mut settings, &mut SystemTimeouts::default(), &config)
                .unwrap();
            assert_eq!(
                format!("{:?}", settings.load_balancing),
                format!("{strategy:?}")
            );
        }
    }

    #[test]
    fn thread_runtime_config_maps_exact_fields_and_timeout() {
        let mut settings = ThreadActorSystemConfig::default();
        let mut timeouts = SystemTimeouts::default();
        apply_thread_runtime_config(
            &mut settings,
            &mut timeouts,
            &RuntimeConfig {
                worker_threads: Some(2),
                scheduler_config: SchedulerConfig {
                    task_queue_capacity: 77,
                    task_timeout: Duration::from_millis(31),
                    ..Default::default()
                },
                ..RuntimeConfig::default()
            },
        )
        .unwrap();
        assert_eq!(settings.shared_pool_size, 2);
        assert_eq!(settings.shared_queue_capacity, 77);
        assert_eq!(timeouts.message_handling, Duration::from_millis(31));
    }

    #[test]
    fn thread_runtime_config_rejects_ambiguous_or_unsupported_settings() {
        let mut settings = ThreadActorSystemConfig::default();
        let mut timeouts = SystemTimeouts {
            message_handling: Duration::from_secs(2),
            ..Default::default()
        };
        let conflict = apply_thread_runtime_config(
            &mut settings,
            &mut timeouts,
            &RuntimeConfig {
                scheduler_config: SchedulerConfig {
                    task_timeout: Duration::from_secs(1),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(matches!(conflict, Err(SystemError::InitializationError(_))));

        let unsupported = apply_thread_runtime_config(
            &mut settings,
            &mut SystemTimeouts::default(),
            &RuntimeConfig {
                scheduler_config: SchedulerConfig {
                    load_balancing: LoadBalancingStrategy::LeastLoaded,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(matches!(
            unsupported,
            Err(SystemError::InitializationError(_))
        ));
    }
}
