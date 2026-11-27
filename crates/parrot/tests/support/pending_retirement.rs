// SPDX-License-Identifier: AGPL-3.0-only
use parrot::{
    actix::{ActixActorSystem, ActixSystemSettings},
    thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{engine::AsyncEngineRuntime, system::SystemTimeouts};
use std::{sync::Arc, time::Duration};

pub const STOP_TIMEOUT: Duration = Duration::from_millis(30);

pub async fn native_runtime(backend: usize, affinity: &str) -> Arc<dyn AsyncEngineRuntime> {
    let timeouts = SystemTimeouts {
        actor_creation: Duration::from_secs(3),
        system_shutdown: STOP_TIMEOUT,
        ..Default::default()
    };
    if backend == 3 {
        let settings = ActixSystemSettings {
            arbiter_count: 1,
            ..Default::default()
        };
        return Arc::new(
            ActixActorSystem::with_guardian_and_timeouts(settings, Default::default(), timeouts)
                .await
                .unwrap(),
        );
    }
    let settings = ThreadActorSystemConfig {
        default_scheduling_mode: match backend {
            0 => SchedulingMode::SharedPool {
                max_messages_per_run: 32,
            },
            1 => SchedulingMode::DedicatedThread,
            2 => SchedulingMode::Sharded {
                affinity_key: affinity.to_owned(),
            },
            _ => panic!("unknown test backend {backend}"),
        },
        ..Default::default()
    };
    ThreadActorSystem::with_guardian_and_timeouts(settings, Default::default(), timeouts)
        .await
        .unwrap()
}
