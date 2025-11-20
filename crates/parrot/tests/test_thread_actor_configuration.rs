//! Actor overrides must be rejected before publication or user construction.
use futures::FutureExt;
use parrot::thread::{
    ActorProcessor, BackpressureStrategy, MailboxError, ProcessorInterface, SchedulingMode,
    SpawnError, SupervisorStrategy, ThreadActor, ThreadActorConfig, ThreadActorSystem,
    ThreadActorSystemConfig, ThreadContext,
    mailbox::{Mailbox, admission::MessageAdmission, mpsc::MpscMailbox},
};
use parrot_api::{
    actor::{Actor, ActorConfig, ActorState},
    address::{ActorPath, ActorRef},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[path = "thread_configuration/export.rs"]
mod export_contracts;

#[derive(Debug, Default)]
struct Counts {
    disposed: AtomicUsize,
    initialized: AtomicUsize,
    constructed: AtomicUsize,
}
#[derive(Debug)]
struct Input {
    counts: Arc<Counts>,
    panic: bool,
    gate: Option<Arc<Gate>>,
}
impl Input {
    fn touch(&self) {
        std::hint::black_box(self);
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.entered.notify_one();
            let (guard, result) = gate
                .changed
                .wait_timeout_while(
                    gate.released
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()),
                    Duration::from_secs(30),
                    |released| !*released,
                )
                .unwrap_or_else(|error| error.into_inner());
            drop(guard);
            assert!(!result.timed_out(), "input disposal gate not released");
        }
        self.counts.disposed.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "actor override input disposal panic");
    }
}
impl ActorConfig for Input {}
impl MessageAdmission for Input {
    fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, MailboxError> {
        Ok(message)
    }
}
struct Probe(Input);
impl Actor for Probe {
    type Config = Input;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.counts.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}

#[derive(Clone, Debug)]
struct EngineConfiguredInput {
    thread: ThreadActorConfig,
    calls: Arc<AtomicUsize>,
}
impl ActorConfig for EngineConfiguredInput {
    fn engine_config(&self) -> Option<Box<dyn std::any::Any + Send>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Some(Box::new(self.thread.clone()))
    }
}

struct EngineConfiguredProbe;
impl Actor for EngineConfiguredProbe {
    type Config = EngineConfiguredInput;
    type Context = ThreadContext<Self>;

    fn state(&self) -> ActorState {
        ActorState::Running
    }

    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
}
fn input(counts: &Arc<Counts>, panic: bool) -> Input {
    Input {
        counts: counts.clone(),
        panic,
        gate: None,
    }
}
fn system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        max_dedicated_threads: 1,
        shutdown_timeout: Duration::from_secs(5),
        ..Default::default()
    })
}
async fn bounded<F: Future>(work: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), work)
        .await
        .expect("override contract deadline")
}
fn invalid(case: &str) -> ThreadActorConfig {
    let mut config = ThreadActorConfig::default();
    match case {
        "mailbox_capacity" => config.mailbox_capacity = Some(0),
        // Use MPSC here: the old SPSC constructor eagerly allocates its ring.
        "mailbox_upper" => config.mailbox_capacity = Some(1_048_577),
        "message_batch_size" => config.message_batch_size = Some(257),
        "scheduling_mode.max_messages_per_run" => {
            config.scheduling_mode = Some(SchedulingMode::SharedPool {
                max_messages_per_run: 0,
            })
        }
        "ask_timeout" => config.ask_timeout = Some(Duration::MAX),
        "supervisor_strategy.within" => {
            config.supervisor_strategy = Some(SupervisorStrategy::Restart {
                max_retries: 1,
                within: Duration::MAX,
            })
        }
        "idle_sleep_duration" => config.idle_sleep_duration = Some(Duration::MAX),
        "thread_stack_size" => config.thread_stack_size = Some(usize::MAX),
        _ => panic!("unknown test case"),
    }
    config
}
fn require_invalid(
    result: Result<parrot::thread::address::ThreadActorRef<Probe>, SpawnError>,
    case: &str,
) {
    let key = if case == "mailbox_upper" {
        "mailbox_capacity"
    } else {
        case
    };
    assert!(
        matches!(result, Err(SpawnError::InvalidConfig(ref message)) if message.contains(&format!("actor.{key}"))),
        "wrong error for {case}: {result:?}"
    );
}
async fn rejected_instance(case: &str) {
    let system = system();
    let counts = Arc::new(Counts::default());
    let result = bounded(system.spawn_at(
        Probe(input(&counts, false)),
        "/override",
        None,
        invalid(case),
    ))
    .await;
    let disposed_at_return = counts.disposed.load(Ordering::SeqCst);
    let initialized_at_return = counts.initialized.load(Ordering::SeqCst);
    let registered = system.get_actor_ref("/override").is_some();
    if let Ok(reference) = &result {
        bounded(reference.stop()).await.unwrap();
    }
    // A rejected operation must leave the same explicit path reusable.
    let replacement = bounded(system.spawn_at(
        Probe(input(&Arc::default(), false)),
        "/override",
        None,
        Default::default(),
    ))
    .await
    .unwrap();
    let answer = bounded(replacement.send(Box::new(73_u64))).await.unwrap();
    assert_eq!(*answer.downcast::<u64>().unwrap(), 73);
    bounded(replacement.stop()).await.unwrap();
    bounded(system.shutdown_internal()).await.unwrap();
    require_invalid(result, case);
    assert_eq!(disposed_at_return, 1, "rejection returned before disposal");
    assert_eq!(initialized_at_return, 0, "rejected actor initialized");
    assert!(!registered, "invalid actor reserved or published a path");
}
macro_rules! rejection {
    ($name:ident, $case:literal) => {
        #[tokio::test]
        async fn $name() {
            rejected_instance($case).await;
        }
    };
}
rejection!(zero_mailbox_is_not_rounded_up, "mailbox_capacity");
rejection!(mailbox_override_has_a_finite_budget, "mailbox_upper");
rejection!(business_batch_override_is_checked, "message_batch_size");
rejection!(
    shared_run_budget_must_be_positive,
    "scheduling_mode.max_messages_per_run"
);
rejection!(ask_deadline_must_be_representable, "ask_timeout");
rejection!(
    restart_window_must_be_representable,
    "supervisor_strategy.within"
);
rejection!(idle_deadline_must_be_representable, "idle_sleep_duration");
rejection!(stack_override_has_a_finite_budget, "thread_stack_size");

#[tokio::test]
async fn invalid_factory_inputs_retire_without_calling_constructor() {
    for case in [
        "mailbox_capacity",
        "mailbox_upper",
        "message_batch_size",
        "scheduling_mode.max_messages_per_run",
        "ask_timeout",
        "supervisor_strategy.within",
        "idle_sleep_duration",
        "thread_stack_size",
    ] {
        let system = system();
        let counts = Arc::new(Counts::default());
        let capture = input(&counts, false);
        let observed = counts.clone();
        let mut config = invalid(case);
        config.message_admission = Some(Arc::new(input(&counts, false)));
        let result = bounded(system.spawn_supervised_with_config(
            move || {
                capture.touch();
                observed.constructed.fetch_add(1, Ordering::SeqCst);
                Probe(input(&observed, false))
            },
            "/factory",
            config,
        ))
        .await;
        let disposed = counts.disposed.load(Ordering::SeqCst);
        let calls = counts.constructed.load(Ordering::SeqCst);
        let registered = system.get_actor_ref("/factory").is_some();
        if let Ok(reference) = &result {
            bounded(reference.stop()).await.unwrap();
        }
        let replacement = bounded(system.spawn_supervised_with_config(
            || Probe(input(&Arc::default(), false)),
            "/factory",
            Default::default(),
        ))
        .await
        .unwrap();
        let answer = bounded(replacement.send(Box::new(37_u64))).await.unwrap();
        assert_eq!(*answer.downcast::<u64>().unwrap(), 37);
        bounded(replacement.stop()).await.unwrap();
        bounded(system.shutdown_internal()).await.unwrap();
        require_invalid(result, case);
        assert_eq!(calls, 0, "rejected factory ran for {case}");
        assert_eq!(
            disposed, 2,
            "factory capture/policy not disposed for {case}"
        );
        assert!(!registered);
    }
}

#[derive(Debug, Default)]
struct Gate {
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    changed: Condvar,
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self
            .0
            .released
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
        self.0.changed.notify_all();
    }
}
#[tokio::test(flavor = "current_thread")]
async fn rejection_disposal_is_outside_registry_and_survives_caller_cancellation() {
    for cancel in [false, true] {
        let system = system();
        let counts = Arc::new(Counts::default());
        let gate = Arc::new(Gate::default());
        let release = Release(gate.clone());
        let mut config = input(&counts, false);
        config.gate = Some(gate.clone());
        let owner = system.clone();
        let observed = counts.clone();
        let caller = tokio::spawn(async move {
            owner
                .spawn_typed_with_config(
                    Probe(input(&observed, false)),
                    config,
                    ThreadActorConfig {
                        mailbox_capacity: Some(0),
                        message_admission: Some(Arc::new(input(&observed, false))),
                        ..Default::default()
                    },
                )
                .await
        });
        bounded(gate.entered.notified()).await;
        let published = system.actor_count();
        let returned_early = caller.is_finished();
        if cancel {
            caller.abort();
        }
        drop(release);
        if cancel {
            assert!(bounded(caller).await.unwrap_err().is_cancelled());
        } else {
            let result = bounded(caller).await.unwrap();
            if let Ok(reference) = &result {
                bounded(reference.stop()).await.unwrap();
            }
            require_invalid(result, "mailbox_capacity");
        }
        bounded(async {
            while counts.disposed.load(Ordering::SeqCst) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        bounded(system.shutdown_internal()).await.unwrap();
        assert!(
            !returned_early,
            "creation returned while input disposal was blocked"
        );
        assert_eq!(
            published, 0,
            "invalid configuration reserved a path before disposal"
        );
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
        assert_eq!(counts.disposed.load(Ordering::SeqCst), 3);
    }
}

#[tokio::test]
async fn configured_instance_rejection_joins_all_inputs_and_preserves_disposal_errors() {
    for panic in [false, true] {
        let system = system();
        let counts = Arc::new(Counts::default());
        let result = bounded(
            AssertUnwindSafe(system.spawn_typed_with_config(
                Probe(input(&counts, panic)),
                input(&counts, panic),
                ThreadActorConfig {
                    mailbox_capacity: Some(0),
                    message_admission: Some(Arc::new(input(&counts, panic))),
                    ..Default::default()
                },
            ))
            .catch_unwind(),
        )
        .await
        .expect("input disposal unwound caller");
        let disposed = counts.disposed.load(Ordering::SeqCst);
        let initialized = counts.initialized.load(Ordering::SeqCst);
        if let Ok(reference) = &result {
            let _ = bounded(reference.stop()).await;
        }
        let _ = bounded(system.shutdown_internal()).await;
        assert_eq!(
            disposed, 3,
            "creation returned before independent inputs retired"
        );
        assert_eq!(initialized, 0);
        if panic {
            assert!(
                matches!(result, Err(SpawnError::DisposalFailed(ref message)) if message.contains("actor override input disposal panic")),
                "{result:?}"
            );
        } else {
            require_invalid(result, "mailbox_capacity");
        }
        assert_eq!(system.actor_count(), 0);
    }
}

#[tokio::test]
async fn actor_config_can_export_thread_engine_configuration() {
    let system = system();
    let calls = Arc::new(AtomicUsize::new(0));
    let config = EngineConfiguredInput {
        thread: ThreadActorConfig {
            mailbox_capacity: Some(7),
            ..Default::default()
        },
        calls: calls.clone(),
    };
    let creation = system.spawn_root_typed_thread(EngineConfiguredProbe, config);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "conversion must be lazy");
    let reference = bounded(creation)
        .await
        .expect("configured actor should start");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let path = reference.path();
    assert_eq!(system.get_mailbox(&path).unwrap().capacity(), 7);
    let reply = bounded(reference.send(Box::new(73_u64))).await.unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 73);
    bounded(reference.stop()).await.unwrap();
    bounded(system.shutdown_internal()).await.unwrap();
}

#[tokio::test]
async fn rejected_factory_capture_panic_is_a_disposal_failure() {
    let system = system();
    let counts = Arc::new(Counts::default());
    let capture = input(&counts, true);
    let observed = counts.clone();
    let result = bounded(
        AssertUnwindSafe(system.spawn_supervised_with_config(
            move || {
                capture.touch();
                observed.constructed.fetch_add(1, Ordering::SeqCst);
                Probe(input(&observed, false))
            },
            "/rejected-panic",
            ThreadActorConfig {
                message_batch_size: Some(0),
                message_admission: Some(Arc::new(input(&counts, true))),
                ..Default::default()
            },
        ))
        .catch_unwind(),
    )
    .await
    .expect("factory capture disposal unwound caller");
    bounded(system.shutdown_internal()).await.unwrap();
    assert!(
        matches!(result, Err(SpawnError::DisposalFailed(ref message)) if message.contains("actor override input disposal panic")),
        "{result:?}"
    );
    assert_eq!(counts.constructed.load(Ordering::SeqCst), 0);
    assert_eq!(counts.disposed.load(Ordering::SeqCst), 2);
    assert_eq!(system.actor_count(), 0);
}

#[tokio::test]
async fn valid_small_overrides_reply_and_retire_on_every_scheduler() {
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 1,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "override".into(),
        },
    ] {
        let system = system();
        let counts = Arc::new(Counts::default());
        let reference = bounded(system.spawn_at(
            Probe(input(&counts, false)),
            "/valid",
            None,
            ThreadActorConfig {
                scheduling_mode: Some(mode),
                mailbox_capacity: Some(1),
                message_batch_size: Some(1),
                supervisor_strategy: Some(SupervisorStrategy::Stop),
                ask_timeout: Some(Duration::from_secs(5)),
                thread_stack_size: Some(3 * 1024 * 1024),
                idle_sleep_duration: Some(Duration::ZERO),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
        assert_eq!(system.get_mailbox("/valid").unwrap().capacity(), 1);
        let answer = bounded(reference.send(Box::new(91_u64))).await.unwrap();
        assert_eq!(*answer.downcast::<u64>().unwrap(), 91);
        bounded(reference.stop()).await.unwrap();
        assert_eq!(counts.disposed.load(Ordering::SeqCst), 1);
        assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
        bounded(system.shutdown_internal()).await.unwrap();
    }
}

struct Ordered {
    seen: Arc<Mutex<Vec<u64>>>,
    coalesce: bool,
    batches: Arc<Mutex<Vec<usize>>>,
    init_gate: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl Actor for Ordered {
    type Config = parrot_api::actor::EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        let gate = self.init_gate.take();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.await.unwrap();
            }
            Ok(())
        })
    }
    fn can_batch_message(&self, _: &BoxedMessage) -> bool {
        self.coalesce
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.seen
            .lock()
            .unwrap()
            .push(*message.downcast_ref::<u64>().unwrap());
        Box::pin(async { Ok(message) })
    }
    fn receive_batch<'a>(
        &'a mut self,
        messages: Vec<BoxedMessage>,
        context: &'a mut Self::Context,
    ) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
        self.batches.lock().unwrap().push(messages.len());
        Box::pin(async move {
            let mut results = Vec::new();
            for message in messages {
                results.push(self.receive_message(message, context).await);
            }
            results
        })
    }
}
async fn run_budget(coalesce: bool) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let processor = Arc::new(ActorProcessor::new(
        ThreadActor::new_for_test(Ordered {
            seen: seen.clone(),
            coalesce,
            batches: Arc::default(),
            init_gate: None,
        }),
        ThreadContext::new_for_test("/budget"),
        "/budget".into(),
        ThreadActorConfig {
            scheduling_mode: Some(SchedulingMode::SharedPool {
                max_messages_per_run: 2,
            }),
            message_batch_size: Some(if coalesce { 8 } else { 1 }),
            ..Default::default()
        },
    ));
    let mailbox: Arc<dyn Mailbox> =
        Arc::new(MpscMailbox::new(16, ActorPath::placeholder("/budget")));
    for number in 0..7_u64 {
        mailbox
            .push(Box::new(number), BackpressureStrategy::Error)
            .await
            .unwrap();
    }
    processor
        .clone()
        .initialize_and_start_erased()
        .await
        .unwrap();
    let first = processor
        .process_batch_of_messages(&mailbox, 6, false)
        .await
        .unwrap();
    let after_first = seen.lock().unwrap().clone();
    let remaining = mailbox.len().await;
    let second = processor
        .process_batch_of_messages(&mailbox, 1, false)
        .await
        .unwrap();
    let after_second = seen.lock().unwrap().clone();
    while !mailbox.is_empty().await {
        processor
            .process_batch_of_messages(&mailbox, 6, false)
            .await
            .unwrap();
    }
    processor.clone().stop_erased().await.unwrap();
    assert_eq!(first, (2, 0), "actor run budget was ignored");
    assert_eq!(after_first, vec![0, 1]);
    assert_eq!(remaining, 5);
    assert_eq!(second, (1, 0), "actor budget exceeded scheduler budget");
    assert_eq!(after_second, vec![0, 1, 2]);
    assert_eq!(*seen.lock().unwrap(), (0..7).collect::<Vec<u64>>());
}
#[tokio::test]
async fn shared_run_budget_limits_serial_dispatch_and_preserves_fifo() {
    bounded(run_budget(false)).await;
}
#[tokio::test]
async fn shared_run_budget_limits_business_batches_and_preserves_fifo() {
    bounded(run_budget(true)).await;
}

#[tokio::test]
async fn system_default_and_actor_override_run_budgets_reach_real_dispatch() {
    for (mode, expected) in [
        (None, 2),
        (
            Some(SchedulingMode::SharedPool {
                max_messages_per_run: 3,
            }),
            3,
        ),
    ] {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            shared_burst_workers_max: 0,
            default_scheduling_mode: SchedulingMode::SharedPool {
                max_messages_per_run: 2,
            },
            ..Default::default()
        });
        let seen = Arc::new(Mutex::new(Vec::new()));
        let batches = Arc::new(Mutex::new(Vec::new()));
        let (release, wait) = tokio::sync::oneshot::channel();
        let reference = bounded(system.spawn_at(
            Ordered {
                seen: seen.clone(),
                coalesce: true,
                batches: batches.clone(),
                init_gate: Some(wait),
            },
            "/inherited-budget",
            None,
            ThreadActorConfig {
                scheduling_mode: mode,
                message_batch_size: Some(8),
                mailbox_capacity: Some(16),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
        for number in 0..8_u64 {
            bounded(reference.deliver(Box::new(number))).await.unwrap();
        }
        release.send(()).unwrap();
        let answer = bounded(reference.send(Box::new(8_u64))).await.unwrap();
        assert_eq!(*answer.downcast::<u64>().unwrap(), 8);
        bounded(reference.stop()).await.unwrap();
        bounded(system.shutdown_internal()).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), (0..9).collect::<Vec<u64>>());
        let batches = batches.lock().unwrap();
        assert_eq!(
            batches[0], expected,
            "effective per-actor budget did not reach processor"
        );
        assert!(batches.iter().all(|size| *size <= expected), "{batches:?}");
        assert_eq!(batches.iter().sum::<usize>(), 9);
    }
}

#[tokio::test]
async fn explicit_yield_overrides_worker_default_and_limits_business_batching() {
    for actor_yield in [false, true] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let batches = Arc::new(Mutex::new(Vec::new()));
        let processor = Arc::new(ActorProcessor::new(
            ThreadActor::new_for_test(Ordered {
                seen: seen.clone(),
                coalesce: true,
                batches: batches.clone(),
                init_gate: None,
            }),
            ThreadContext::new_for_test("/yield"),
            "/yield".into(),
            ThreadActorConfig {
                message_batch_size: Some(8),
                yield_after_each_message: Some(actor_yield),
                ..Default::default()
            },
        ));
        let mailbox: Arc<dyn Mailbox> =
            Arc::new(MpscMailbox::new(8, ActorPath::placeholder("/yield")));
        for number in 0..4_u64 {
            mailbox
                .push(Box::new(number), BackpressureStrategy::Error)
                .await
                .unwrap();
        }
        processor
            .clone()
            .initialize_and_start_erased()
            .await
            .unwrap();
        let result = bounded(processor.process_batch_of_messages(&mailbox, 4, !actor_yield))
            .await
            .unwrap();
        processor.clone().stop_erased().await.unwrap();
        assert_eq!(result, (4, 0));
        assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(
            *batches.lock().unwrap(),
            if actor_yield {
                vec![1, 1, 1, 1]
            } else {
                vec![4]
            }
        );
    }
}
