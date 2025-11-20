use super::*;
use crate::thread::{SchedulingMode, ThreadActorSystemConfig};
use parrot_api::{
    actor::{ActorConfig, ActorState},
    trace::TraceContext,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Condvar,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Gate {
    started: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.started.notify_one();
        let (_guard, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(10), |open| {
                !*open
            })
            .unwrap();
        assert!(!timeout.timed_out(), "cleanup gate was never released");
    }
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.started.notified())
            .await
            .unwrap();
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct Input {
    identity: Arc<()>,
    drops: Arc<AtomicUsize>,
    gate: Option<Arc<Gate>>,
    failure: bool,
    failure_message: &'static str,
    contexts: Arc<Mutex<Vec<Option<TraceContext>>>>,
}
impl Drop for Input {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.block();
        }
        self.contexts.lock().unwrap().push(TraceContext::current());
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.failure, "{}", self.failure_message);
    }
}
struct Config(Input);
impl ActorConfig for Config {}
struct Instance {
    input: Input,
    initialized: Arc<AtomicUsize>,
}
impl Actor for Instance {
    type Config = Config;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(msg) })
    }
}
#[derive(Default)]
struct Counts {
    actors: Arc<AtomicUsize>,
    configs: Arc<AtomicUsize>,
    initialized: Arc<AtomicUsize>,
    contexts: Arc<Mutex<Vec<Option<TraceContext>>>>,
}
impl Counts {
    fn inputs(
        &self,
        actor_gate: Option<Arc<Gate>>,
        config_gate: Option<Arc<Gate>>,
        failure: bool,
    ) -> (Instance, Config) {
        (
            Instance {
                input: Input {
                    identity: Arc::new(()),
                    drops: self.actors.clone(),
                    gate: actor_gate,
                    failure,
                    failure_message: "admitted actor destructor failed",
                    contexts: self.contexts.clone(),
                },
                initialized: self.initialized.clone(),
            },
            Config(Input {
                identity: Arc::new(()),
                drops: self.configs.clone(),
                gate: config_gate,
                failure: false,
                failure_message: "admitted configuration destructor failed",
                contexts: self.contexts.clone(),
            }),
        )
    }
}

fn system(capacity: usize) -> Arc<ThreadActorSystem> {
    system_with_creation_timeout(capacity, Duration::ZERO)
}

fn system_with_creation_timeout(capacity: usize, timeout: Duration) -> Arc<ThreadActorSystem> {
    let mut system = ThreadActorSystem::try_new(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        default_scheduling_mode: SchedulingMode::SharedPool {
            max_messages_per_run: 4,
        },
        ..Default::default()
    })
    .unwrap();
    system.instance_admission = Arc::new(tokio::sync::Semaphore::new(capacity));
    system.operation_timeouts.actor_creation = timeout;
    let system = Arc::new(system);
    system.set_self_weak(Arc::downgrade(&system));
    system
}

#[tokio::test]
async fn production_limit_counts_unpolled_inputs_before_registry_or_task_submission() {
    let system = ThreadActorSystem::try_shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        ..Default::default()
    })
    .unwrap();
    let counts = Counts::default();
    let mut admitted = Vec::new();
    for _ in 0..ThreadActorSystem::MAX_PENDING_INSTANCE_CREATIONS {
        let (actor, config) = counts.inputs(None, None, false);
        admitted.push(
            system
                .try_spawn_typed_with_config(actor, config, Default::default())
                .unwrap(),
        );
    }
    let (actor, config) = counts.inputs(None, None, false);
    let rejected = system
        .try_spawn_typed_with_config(actor, config, Default::default())
        .err()
        .expect("production capacity was bypassed");
    assert_eq!(rejected.reason, InstanceAdmissionFailure::Full);
    assert!(system.registry.read().unwrap().is_empty());
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
    drop(admitted);
    recovered(&system, ThreadActorSystem::MAX_PENDING_INSTANCE_CREATIONS).await;
    assert_eq!(
        counts.actors.load(Ordering::SeqCst),
        ThreadActorSystem::MAX_PENDING_INSTANCE_CREATIONS
    );
    assert_eq!(
        counts.configs.load(Ordering::SeqCst),
        ThreadActorSystem::MAX_PENDING_INSTANCE_CREATIONS
    );
    drop(rejected);
    system.shutdown_internal().await.unwrap();
}

async fn recovered(system: &ThreadActorSystem, capacity: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while system.instance_admission.available_permits() != capacity {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("creation ownership did not recover");
}

#[test]
fn unpolled_admission_is_bounded_and_rejection_returns_original_inputs_without_tasks() {
    crate::test_support::isolated(
        concat!(
            module_path!(),
            "::unpolled_admission_is_bounded_and_rejection_returns_original_inputs_without_tasks"
        )
        .strip_prefix("parrot::")
        .unwrap(),
        || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(rejection_contract())
        },
    );
}

async fn rejection_contract() {
    let system = system(2);
    let counts = Counts::default();
    let mut admitted = Vec::new();
    for _ in 0..2 {
        let (actor, config) = counts.inputs(None, None, false);
        admitted.push(
            system
                .try_spawn_typed_with_config(actor, config, Default::default())
                .unwrap(),
        );
    }
    assert_eq!(system.instance_admission.available_permits(), 0);
    let tasks = system.retirement_runtime().metrics().num_alive_tasks();
    let mut rejected = Vec::new();
    for _ in 0..32 {
        let (actor, config) = counts.inputs(None, None, false);
        let identity = actor.input.identity.clone();
        let config_identity = config.0.identity.clone();
        let engine = ThreadActorConfig {
            ask_timeout: Some(Duration::from_millis(37)),
            ..Default::default()
        };
        let rejection = system
            .try_spawn_typed_with_config(actor, config, engine)
            .err()
            .expect("full creation pool accepted an input");
        assert_eq!(rejection.reason, InstanceAdmissionFailure::Full);
        assert!(Arc::ptr_eq(&rejection.actor.input.identity, &identity));
        assert!(Arc::ptr_eq(&rejection.config.0.identity, &config_identity));
        assert_eq!(
            rejection.engine.ask_timeout,
            Some(Duration::from_millis(37))
        );
        rejected.push(rejection);
    }
    assert_eq!(
        system.retirement_runtime().metrics().num_alive_tasks(),
        tasks
    );
    assert!(system.registry.read().unwrap().is_empty());
    assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 0);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    drop(rejected);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 32);
    drop(admitted);
    recovered(&system, 2).await;
    assert_eq!(counts.actors.load(Ordering::SeqCst), 34);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 34);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn unpolled_cancellation_holds_capacity_until_actual_disposal_and_preserves_identity() {
    let system = system(1);
    let counts = Counts::default();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let context = TraceContext::root();
    let (actor, config) = counts.inputs(Some(gate.clone()), None, true);
    let future = context.in_scope(|| {
        system
            .try_spawn_typed_with_config(actor, config, Default::default())
            .unwrap()
    });
    TraceContext::root().in_scope(|| drop(future));
    gate.entered().await;
    assert_eq!(system.instance_admission.available_permits(), 0);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
    let (actor, config) = counts.inputs(None, None, false);
    let rejected = system
        .try_spawn_typed_with_config(actor, config, Default::default())
        .err()
        .expect("cleanup released admission before disposal");
    assert_eq!(rejected.reason, InstanceAdmissionFailure::Full);
    gate.release();
    recovered(&system, 1).await;
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    assert_eq!(
        *counts.contexts.lock().unwrap(),
        vec![Some(context.clone()), Some(context)]
    );
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    drop(rejected);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancelling_polled_creation_cannot_release_pending_cleanup_or_block_another_system() {
    let system = system(1);
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (actor, config) = counts.inputs(None, Some(gate.clone()), false);
    let owner = system.clone();
    let job = tokio::spawn(async move {
        owner
            .try_spawn_typed_with_config(actor, config, Default::default())
            .unwrap()
            .await
    });
    gate.entered().await;
    job.abort();
    assert!(job.await.unwrap_err().is_cancelled());
    assert_eq!(system.instance_admission.available_permits(), 0);
    let healthy = self::system(1);
    let healthy_counts = Counts::default();
    let (actor, config) = healthy_counts.inputs(None, None, false);
    let reference = tokio::time::timeout(
        Duration::from_secs(3),
        healthy
            .try_spawn_typed_with_config(actor, config, Default::default())
            .unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    let reply = reference.ask(Box::new(73u64)).await.unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 73);
    recovered(&healthy, 1).await;
    healthy.shutdown_internal().await.unwrap();
    assert_eq!(system.instance_admission.available_permits(), 0);
    gate.release();
    recovered(&system, 1).await;
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn publication_releases_creation_capacity_while_live_actor_keeps_working() {
    let system = system(1);
    let counts = Counts::default();
    let (actor, config) = counts.inputs(None, None, false);
    let reference = system
        .try_spawn_typed_with_config(actor, config, Default::default())
        .unwrap()
        .await
        .unwrap();
    recovered(&system, 1).await;
    assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    assert_eq!(
        *reference
            .ask(Box::new(91u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        91
    );
    system.shutdown_internal().await.unwrap();
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stopped_system_rejects_before_owning_or_disposing_inputs() {
    let system = system(1);
    system.shutdown_internal().await.unwrap();
    let counts = Counts::default();
    let (actor, config) = counts.inputs(None, None, false);
    let rejected = system
        .try_spawn_typed_with_config(actor, config, Default::default())
        .err()
        .expect("stopped system accepted an input");
    assert_eq!(rejected.reason, InstanceAdmissionFailure::ShuttingDown);
    assert_eq!(system.instance_admission.available_permits(), 1);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 0);
    drop(rejected);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn admitted_unpolled_creation_rechecks_shutdown_before_publication() {
    let system = system(1);
    let counts = Counts::default();
    let (actor, config) = counts.inputs(None, None, false);
    let creation = system
        .try_spawn_typed_with_config(actor, config, Default::default())
        .unwrap();
    system.shutdown_internal().await.unwrap();
    assert_eq!(system.instance_admission.available_permits(), 0);
    assert!(matches!(creation.await, Err(SpawnError::SystemShutdown)));
    recovered(&system, 1).await;
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert!(system.registry.read().unwrap().is_empty());
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn invalid_engine_configuration_retires_all_inputs_before_releasing_admission() {
    let system = system(1);
    let counts = Counts::default();
    let (actor, config) = counts.inputs(None, None, false);
    let result = system
        .try_spawn_typed_with_config(
            actor,
            config,
            ThreadActorConfig {
                mailbox_capacity: Some(0),
                ..Default::default()
            },
        )
        .unwrap()
        .await;
    assert!(matches!(result, Err(SpawnError::InvalidConfig(ref message))
        if message.contains("actor.mailbox_capacity")));
    recovered(&system, 1).await;
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert!(system.registry.read().unwrap().is_empty());
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn configuration_failure_preserves_disposal_error_and_restores_admission() {
    let system = system(1);
    let counts = Counts::default();
    let (actor, mut config) = counts.inputs(None, None, true);
    config.0.failure = true;
    let result = system
        .try_spawn_typed_with_config(actor, config, Default::default())
        .unwrap()
        .await;
    assert!(
        matches!(result, Err(SpawnError::DisposalFailed(ref message))
            if message.contains("admitted configuration destructor failed")
                && !message.contains("admitted actor destructor failed"))
    );
    recovered(&system, 1).await;
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn creation_deadline_reports_timeout_without_releasing_blocked_input_ownership() {
    let system = system_with_creation_timeout(1, Duration::from_millis(50));
    let counts = Counts::default();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let (actor, config) = counts.inputs(None, Some(gate.clone()), false);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        system
            .try_spawn_typed_with_config(actor, config, Default::default())
            .unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(SpawnError::Timeout(_))));
    gate.entered().await;
    assert_eq!(system.instance_admission.available_permits(), 0);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 0);
    gate.release();
    recovered(&system, 1).await;
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    system.shutdown_internal().await.unwrap();
}

#[test]
fn original_host_exit_keeps_admitted_cleanup_until_real_disposal_finishes() {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let counts = Counts::default();
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let system = host.block_on(async {
        let system = system(1);
        let owner = system.clone();
        let (actor, config) = counts.inputs(None, Some(gate.clone()), false);
        tokio::spawn(async move {
            owner
                .try_spawn_typed_with_config(actor, config, Default::default())
                .unwrap()
                .await
        });
        gate.entered().await;
        system
    });
    drop(host);
    assert_eq!(system.instance_admission.available_permits(), 0);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 0);
    gate.release();
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    observer.block_on(async {
        recovered(&system, 1).await;
        assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
        assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            let result = system.shutdown_internal().await;
            assert!(
                matches!(result, Err(SystemError::ShutdownError(ref message))
                if message == "1 shared cleanup failure(s): shared task unexpectedly cancelled")
            );
        }
    });
}
