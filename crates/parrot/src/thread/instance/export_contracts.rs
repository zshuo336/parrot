use super::*;
use crate::thread::{BackpressureStrategy, SchedulingMode, ThreadActorSystemConfig};
use parrot_api::{
    actor::{ActorConfig, ActorState},
    trace::TraceContext,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    sync::{
        Condvar,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (_guard, result) = self
            .changed
            .wait_timeout_while(
                self.released.lock().unwrap(),
                Duration::from_secs(15),
                |released| !*released,
            )
            .unwrap();
        assert!(!result.timed_out(), "configuration gate was never released");
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
#[derive(Default)]
struct Counts {
    calls: AtomicUsize,
    initialized: AtomicUsize,
    actors: AtomicUsize,
    configs: AtomicUsize,
    contexts: Mutex<Vec<Option<TraceContext>>>,
}
struct Config {
    counts: Arc<Counts>,
    gate: Option<Arc<Gate>>,
    mode: SchedulingMode,
}
impl ActorConfig for Config {
    fn engine_config(&self) -> Option<Box<dyn std::any::Any + Send>> {
        self.counts.calls.fetch_add(1, Ordering::SeqCst);
        self.counts
            .contexts
            .lock()
            .unwrap()
            .push(TraceContext::current());
        if let Some(gate) = &self.gate {
            gate.block();
        }
        Some(Box::new(ThreadActorConfig {
            mailbox_capacity: Some(1),
            ask_timeout: Some(Duration::from_millis(123)),
            backpressure_strategy: Some(BackpressureStrategy::Error),
            scheduling_mode: Some(self.mode.clone()),
            ..Default::default()
        }))
    }
}
impl Drop for Config {
    fn drop(&mut self) {
        self.counts
            .contexts
            .lock()
            .unwrap()
            .push(TraceContext::current());
        self.counts.configs.fetch_add(1, Ordering::SeqCst);
    }
}
struct Echo {
    counts: Arc<Counts>,
    initialization: Option<Arc<tokio::sync::Notify>>,
}
impl Drop for Echo {
    fn drop(&mut self) {
        self.counts
            .contexts
            .lock()
            .unwrap()
            .push(TraceContext::current());
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Echo {
    type Config = Config;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.counts.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            if let Some(gate) = &self.initialization {
                gate.notified().await;
            }
            Ok(())
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}
fn shared_mode() -> SchedulingMode {
    SchedulingMode::SharedPool {
        max_messages_per_run: 4,
    }
}
fn inputs(counts: &Arc<Counts>, gate: Option<&Arc<Gate>>) -> (Echo, Config) {
    (
        Echo {
            counts: counts.clone(),
            initialization: None,
        },
        Config {
            counts: counts.clone(),
            gate: gate.cloned(),
            mode: shared_mode(),
        },
    )
}
fn system(creation: Duration) -> Arc<ThreadActorSystem> {
    let mut system = ThreadActorSystem::try_new(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        max_dedicated_threads: 1,
        shutdown_timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .unwrap();
    system.operation_timeouts.actor_creation = creation;
    let system = Arc::new(system);
    system.set_self_weak(Arc::downgrade(&system));
    system
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("export contract deadline")
}
async fn pending(system: &ThreadActorSystem) -> Arc<PendingInstance> {
    bounded(async {
        loop {
            if let Some(pending) =
                system
                    .registry
                    .read()
                    .unwrap()
                    .values()
                    .find_map(|entry| match entry {
                        ActorRegistryEntry::Instance(pending) => Some(pending.clone()),
                        _ => None,
                    })
            {
                break pending;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}
fn spawn(
    system: &Arc<ThreadActorSystem>,
    counts: &Arc<Counts>,
    gate: &Arc<Gate>,
) -> tokio::task::JoinHandle<Result<ThreadActorRef<Echo>, SpawnError>> {
    let system = system.clone();
    let (actor, config) = inputs(counts, Some(gate));
    tokio::spawn(async move { system.spawn_root_typed_thread(actor, config).await })
}
async fn retired(pending: &PendingInstance, counts: &Counts) {
    bounded(super::super::system::await_stop(
        pending.retired.subscribe(),
    ))
    .await
    .unwrap();
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_joins_running_configuration_before_retirement_receipt() {
    let system = system(Duration::ZERO);
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let waiter = spawn(&system, &counts, &gate);
    bounded(gate.entered.notified()).await;
    assert_eq!(
        system.actor_count(),
        1,
        "running export has no lifecycle owner"
    );
    let pending = pending(&system).await;
    let mut shutdown = Box::pin(system.shutdown_internal());
    assert!(futures::poll!(&mut shutdown).is_pending());
    assert!(pending.retired.borrow().is_none());
    assert_eq!(counts.configs.load(Ordering::SeqCst), 0);
    assert_eq!(system.actor_count(), 1);
    drop(release);
    bounded(shutdown).await.unwrap();
    assert!(matches!(
        bounded(waiter).await.unwrap(),
        Err(SpawnError::Cancelled)
    ));
    retired(&pending, &counts).await;
    assert_eq!(system.actor_count(), 0);
}

#[tokio::test]
async fn configuration_uses_creation_deadline_and_retains_late_inputs() {
    let system = system(Duration::from_millis(100));
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let waiter = spawn(&system, &counts, &gate);
    bounded(gate.entered.notified()).await;
    let pending = pending(&system).await;
    assert!(matches!(
        bounded(waiter).await.unwrap(),
        Err(SpawnError::Timeout(_))
    ));
    assert!(pending.retired.borrow().is_none());
    assert_eq!(counts.configs.load(Ordering::SeqCst), 0);
    assert_eq!(system.actor_count(), 1);
    drop(release);
    retired(&pending, &counts).await;
    assert_eq!(system.actor_count(), 0);
    let (actor, config) = inputs(&counts, None);
    let healthy = bounded(system.spawn_root_typed_thread(actor, config))
        .await
        .unwrap();
    let reply = bounded(healthy.send(Box::new(47_u64))).await.unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 47);
    bounded(healthy.stop()).await.unwrap();
    bounded(system.shutdown_internal()).await.unwrap();
}

#[tokio::test]
async fn cancelled_configuration_waiting_for_capacity_skips_hook_and_retires() {
    let system = system(Duration::ZERO);
    let permit = system.factory_slots.clone().acquire_owned().await.unwrap();
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let waiter = spawn(&system, &counts, &gate);
    let pending = pending(&system).await;
    bounded(pending.reference.stop()).await.unwrap();
    assert!(matches!(
        bounded(waiter).await.unwrap(),
        Err(SpawnError::Cancelled)
    ));
    retired(&pending, &counts).await;
    assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
    drop(permit);
    drop(release);
    bounded(system.shutdown_internal()).await.unwrap();
}

#[tokio::test]
async fn exported_defaults_reach_pending_views_and_real_mailbox_on_all_schedulers() {
    for mode in [
        shared_mode(),
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "export-defaults".into(),
        },
    ] {
        let system = system(Duration::ZERO);
        let counts = Arc::new(Counts::default());
        let gate = Arc::new(Gate::default());
        let release = Release(gate.clone());
        let initialization = Arc::new(tokio::sync::Notify::new());
        let (mut actor, mut config) = inputs(&counts, Some(&gate));
        actor.initialization = Some(initialization.clone());
        config.mode = mode;
        let owner = system.clone();
        let waiter =
            tokio::spawn(async move { owner.spawn_root_typed_thread(actor, config).await });
        bounded(gate.entered.notified()).await;
        let early = pending(&system).await.reference.clone();
        drop(release);
        let reference = bounded(waiter).await.unwrap().unwrap();
        assert_eq!(reference.default_timeout(), Duration::from_millis(123));
        assert_eq!(reference.default_strategy(), &BackpressureStrategy::Error);
        bounded(async {
            while counts.initialized.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        bounded(early.deliver(Box::new(1_u64))).await.unwrap();
        let error = bounded(early.deliver(Box::new(2_u64))).await.unwrap_err();
        assert!(error.to_string().contains("Full"), "{error}");
        initialization.notify_one();
        let mailbox = system.get_mailbox(reference.actor_path()).unwrap();
        bounded(async {
            while !mailbox.is_empty().await {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let reply = bounded(reference.send(Box::new(73_u64))).await.unwrap();
        assert_eq!(*reply.downcast::<u64>().unwrap(), 73);
        bounded(reference.stop()).await.unwrap();
        assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
        assert_eq!(counts.configs.load(Ordering::SeqCst), 1);
        bounded(system.shutdown_internal()).await.unwrap();
    }
}

#[test]
fn executor_queued_configuration_skips_hook_after_shutdown() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let system = {
        let _entered = runtime.enter();
        let mut system = ThreadActorSystem::new(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            shutdown_timeout: Duration::from_secs(5),
            ..Default::default()
        });
        // Deterministically queue a blocking job after it has obtained its permit.
        Arc::get_mut(&mut system.root_cohorts)
            .unwrap()
            .set_test_retirement_runtime(runtime.handle().clone());
        let system = Arc::new(system);
        system.set_self_weak(Arc::downgrade(&system));
        system
    };
    let executor_gate = Arc::new(Gate::default());
    let release = Release(executor_gate.clone());
    let blocked = executor_gate.clone();
    let blocker = runtime.spawn_blocking(move || blocked.block());
    runtime.block_on(async {
        bounded(executor_gate.entered.notified()).await;
        let counts = Arc::new(Counts::default());
        let gate = Arc::new(Gate::default());
        let _hook_release = Release(gate.clone());
        let waiter = spawn(&system, &counts, &gate);
        let pending = pending(&system).await;
        assert_eq!(system.factory_slots.available_permits(), 0);
        let owner = system.clone();
        let shutdown = tokio::spawn(async move { owner.shutdown_internal().await });
        bounded(pending.control.cancelled()).await;
        assert!(pending.retired.borrow().is_none());
        drop(release);
        bounded(blocker).await.unwrap();
        bounded(shutdown).await.unwrap().unwrap();
        assert!(matches!(
            bounded(waiter).await.unwrap(),
            Err(SpawnError::Cancelled)
        ));
        retired(&pending, &counts).await;
        assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn original_host_exit_retains_export_and_creation_trace_until_disposal() {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let system = {
        let _entered = host.enter();
        system(Duration::ZERO)
    };
    let counts = Arc::new(Counts::default());
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let (actor, config) = inputs(&counts, Some(&gate));
    let context = TraceContext::root();
    let mut creation = Box::pin(context.in_scope(|| system.spawn_root_typed_thread(actor, config)));
    let pending = host.block_on(async {
        assert!(futures::poll!(&mut creation).is_pending());
        bounded(gate.entered.notified()).await;
        pending(&system).await
    });
    TraceContext::root().in_scope(|| drop(creation));
    drop(host);
    assert!(pending.retired.borrow().is_none());
    assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
    assert_eq!(counts.configs.load(Ordering::SeqCst), 0);
    drop(release);
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    observer.block_on(async {
        retired(&pending, &counts).await;
        // Dropping a caller-owned runtime cancels its shared scheduler workers.
        // Input retirement must survive it, while shutdown preserves that failure.
        for _ in 0..2 {
            let result = bounded(system.shutdown_internal()).await;
            assert!(
                matches!(result, Err(SystemError::ShutdownError(ref message))
                if message == "1 shared cleanup failure(s): shared task unexpectedly cancelled"),
                "{result:?}"
            );
        }
    });
    let contexts = counts.contexts.lock().unwrap();
    assert_eq!(contexts.len(), 3, "export, config disposal, actor disposal");
    for observed in contexts.iter() {
        let observed = observed.as_ref().expect("lost creation trace");
        assert_eq!(observed.trace_id(), context.trace_id());
        assert_eq!(observed.span_id(), context.span_id());
    }
}
