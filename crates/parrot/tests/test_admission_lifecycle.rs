//! Logical policy ownership includes callbacks and disposal, not Arc counts.
use futures::FutureExt;
use parrot::thread::{
    SchedulingMode, SupervisorStrategy, ThreadActorConfig, ThreadActorSystem,
    ThreadActorSystemConfig, ThreadContext,
    error::{MailboxError, SystemError},
    mailbox::admission::MessageAdmission,
    processor::ActorProcessor,
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    panic::AssertUnwindSafe,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Counts {
    actors: AtomicUsize,
    initialized: AtomicUsize,
    handled: AtomicUsize,
    policies: AtomicUsize,
    prepared: AtomicUsize,
    panic_payloads: AtomicUsize,
}
#[derive(Default, Debug)]
struct Gate {
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    wake: Condvar,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let (guard, result) = self
            .wake
            .wait_timeout_while(
                self.released.lock().unwrap(),
                Duration::from_secs(5),
                |released| !*released,
            )
            .unwrap();
        drop(guard);
        assert!(!result.timed_out(), "test did not release policy gate");
    }
    fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
        self.wake.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Echo {
    counts: Arc<Counts>,
    fail_init: bool,
}
impl Drop for Echo {
    fn drop(&mut self) {
        self.counts.actors.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.counts.initialized.fetch_add(1, Ordering::SeqCst);
        let fail = self.fail_init;
        Box::pin(async move {
            if fail {
                Err(ActorError::InitializationError(
                    "injected initialization failure".into(),
                ))
            } else {
                Ok(())
            }
        })
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.counts.handled.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            assert_ne!(
                message.downcast_ref::<u64>(),
                Some(&99),
                "actor restart trigger"
            );
            Ok(message)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
struct BadPayload(Arc<Counts>);
impl Drop for BadPayload {
    fn drop(&mut self) {
        self.0.panic_payloads.fetch_add(1, Ordering::SeqCst);
        panic!("admission panic payload destructor");
    }
}
struct Policy {
    counts: Arc<Counts>,
    drop_panic: usize,
    callback_recursive: bool,
    callback_gate: Option<Arc<Gate>>,
    callback_gate2: Option<Arc<Gate>>,
    drop_gate: Option<Arc<Gate>>,
}
impl std::fmt::Debug for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Policy")
    }
}
impl Drop for Policy {
    fn drop(&mut self) {
        if let Some(gate) = &self.drop_gate {
            gate.block();
        }
        self.counts.policies.fetch_add(1, Ordering::SeqCst);
        match self.drop_panic {
            1 => panic!("admission policy destructor"),
            2 => std::panic::panic_any(BadPayload(self.counts.clone())),
            _ => (),
        }
    }
}
impl MessageAdmission for Policy {
    fn prepare(&self, message: BoxedMessage) -> Result<BoxedMessage, MailboxError> {
        self.counts.prepared.fetch_add(1, Ordering::SeqCst);
        match message.downcast_ref::<u64>() {
            Some(5) => {
                if let Some(gate) = &self.callback_gate {
                    gate.block();
                }
            }
            Some(6) => {
                if let Some(gate) = &self.callback_gate2 {
                    gate.block();
                }
            }
            Some(7) => {
                if self.callback_recursive {
                    std::panic::panic_any(BadPayload(self.counts.clone()));
                }
                panic!("admission callback panic");
            }
            Some(13) => return Err(MailboxError::Full { capacity: 17 }),
            _ => (),
        }
        Ok(message)
    }
}
fn policy(counts: &Arc<Counts>) -> Policy {
    Policy {
        counts: counts.clone(),
        drop_panic: 0,
        callback_recursive: false,
        callback_gate: None,
        callback_gate2: None,
        drop_gate: None,
    }
}
fn system(timeout: Duration) -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 2,
        shared_burst_workers_max: 0,
        shutdown_timeout: timeout,
        ..Default::default()
    })
}
fn modes() -> [SchedulingMode; 3] {
    [
        SchedulingMode::SharedPool {
            max_messages_per_run: 8,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "admission-contract".into(),
        },
    ]
}
fn config(policy: Arc<dyn MessageAdmission>, mode: SchedulingMode) -> ThreadActorConfig {
    ThreadActorConfig {
        message_admission: Some(policy),
        scheduling_mode: Some(mode),
        ..Default::default()
    }
}
async fn wait_for(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("lifecycle progress");
}
async fn entered(gate: &Gate) {
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
}

async fn drop_failure(mode: SchedulingMode, recursive: bool) {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let mut p = policy(&counts);
    p.drop_panic = if recursive { 2 } else { 1 };
    let actor = sys
        .spawn_at(
            Echo {
                counts: counts.clone(),
                fail_init: false,
            },
            "/drop",
            None,
            config(Arc::new(p), mode),
        )
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    let outcome = actor.stop().await;
    assert!(
        matches!(&outcome, Err(ActorError::Panic(message)) if message.contains("admission policy disposal failed")),
        "{outcome:?}"
    );
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.panic_payloads.load(Ordering::SeqCst),
        usize::from(recursive)
    );
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn shared_policy_disposal_failure_is_in_stop_receipt() {
    drop_failure(modes()[0].clone(), false).await;
}
#[tokio::test]
async fn shared_recursive_policy_disposal_failure_is_in_stop_receipt() {
    drop_failure(modes()[0].clone(), true).await;
}
#[tokio::test]
async fn dedicated_policy_disposal_failures_are_in_stop_receipt() {
    for recursive in [false, true] {
        drop_failure(SchedulingMode::DedicatedThread, recursive).await;
    }
}
#[tokio::test]
async fn sharded_policy_disposal_failures_are_in_stop_receipt() {
    for recursive in [false, true] {
        drop_failure(modes()[2].clone(), recursive).await;
    }
}
#[tokio::test]
async fn retained_framework_handles_do_not_retain_raw_policy_or_reopen_callbacks() {
    for mode in modes() {
        let counts = Arc::new(Counts::default());
        let sys = system(Duration::from_secs(3));
        let actor = sys
            .spawn_at(
                Echo {
                    counts: counts.clone(),
                    fail_init: false,
                },
                "/held",
                None,
                config(Arc::new(policy(&counts)), mode),
            )
            .await
            .unwrap();
        actor.send(Box::new(0u64)).await.unwrap();
        let mailbox = sys.get_mailbox("/held").unwrap();
        let processor = mailbox.get_processor().unwrap();
        let retained = processor
            .as_any_ref()
            .downcast_ref::<ActorProcessor<Echo>>()
            .unwrap()
            .config()
            .clone();
        let prepared = counts.prepared.load(Ordering::SeqCst);
        actor.stop().await.unwrap();
        assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
        assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
        assert!(matches!(
            retained.message_admission.unwrap().prepare(Box::new(0u64)),
            Err(MailboxError::Closed)
        ));
        assert_eq!(counts.prepared.load(Ordering::SeqCst), prepared);
        assert!(mailbox.is_closed().await);
        drop(processor);
        drop(mailbox);
        sys.shutdown_internal().await.unwrap();
        assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn caller_retained_raw_policy_is_not_owned_retirement_work() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let raw = Arc::new(policy(&counts));
    let actor = sys
        .spawn_at(
            Echo {
                counts: counts.clone(),
                fail_init: false,
            },
            "/raw",
            None,
            config(raw.clone(), modes()[0].clone()),
        )
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), actor.stop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
    drop(raw);
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    sys.shutdown_internal().await.unwrap();
}
async fn callback_panics(recursive: bool) {
    for mode in modes() {
        let counts = Arc::new(Counts::default());
        let sys = system(Duration::from_secs(3));
        let mut p = policy(&counts);
        p.callback_recursive = recursive;
        let actor = sys
            .spawn_at(
                Echo {
                    counts: counts.clone(),
                    fail_init: false,
                },
                "/callback",
                None,
                config(Arc::new(p), mode),
            )
            .await
            .unwrap();
        actor.send(Box::new(0u64)).await.unwrap();
        for ask in [false, true] {
            let outcome = AssertUnwindSafe(async {
                if ask {
                    actor.send(Box::new(7u64)).await.map(|_| ())
                } else {
                    actor.deliver(Box::new(7u64)).await
                }
            })
            .catch_unwind()
            .await;
            assert!(
                matches!(outcome, Ok(Err(ActorError::InternalError(ref message))) if message.contains("message admission panicked")),
                "callback escaped or was admitted"
            );
        }
        let mailbox = sys.get_mailbox("/callback").unwrap();
        assert!(
            matches!(
                mailbox
                    .push(Box::new(13u64), parrot::thread::BackpressureStrategy::Block)
                    .await,
                Err(MailboxError::Full { capacity: 17 })
            ),
            "business rejection kind must remain intact"
        );
        assert_eq!(
            *actor
                .send(Box::new(8u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            8
        );
        assert_eq!(
            counts.handled.load(Ordering::SeqCst),
            2,
            "rejected work must not reach the actor"
        );
        assert_eq!(
            counts.panic_payloads.load(Ordering::SeqCst),
            if recursive { 2 } else { 0 }
        );
        actor.stop().await.unwrap();
        sys.shutdown_internal().await.unwrap();
    }
}
#[tokio::test]
async fn ordinary_callback_panic_preserves_ask_tell_and_business_rejection() {
    callback_panics(false).await;
}
#[tokio::test]
async fn recursive_callback_payload_preserves_ask_tell_and_business_rejection() {
    callback_panics(true).await;
}
#[tokio::test]
async fn stop_joins_active_callback_without_blocking_unrelated_actor() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut p = policy(&counts);
    p.callback_gate = Some(gate.clone());
    let actor = sys
        .spawn_at(
            Echo {
                counts: counts.clone(),
                fail_init: false,
            },
            "/busy",
            None,
            config(Arc::new(p), modes()[0].clone()),
        )
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    let neighbor = sys
        .spawn_root_typed_thread(
            Echo {
                counts: Arc::new(Counts::default()),
                fail_init: false,
            },
            EmptyConfig,
        )
        .await
        .unwrap();
    let producer = actor.clone();
    let producer = tokio::task::spawn_blocking(move || {
        futures::executor::block_on(producer.deliver(Box::new(5u64)))
    });
    entered(&gate).await;
    let mut stopped = Box::pin(actor.stop());
    assert!(futures::poll!(&mut stopped).is_pending());
    assert_eq!(
        *tokio::time::timeout(Duration::from_secs(1), neighbor.send(Box::new(42u64)))
            .await
            .unwrap()
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        42
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut stopped)
            .await
            .is_err(),
        "active prepare cannot be detached from stop"
    );
    assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
    gate.release();
    assert!(
        producer.await.unwrap().is_err(),
        "sealed mailbox must reject the prepared output"
    );
    stopped.await.unwrap();
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn cancelled_stop_waiter_keeps_blocked_disposal_and_shared_error_receipt() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut p = policy(&counts);
    p.drop_gate = Some(gate.clone());
    p.drop_panic = 1;
    let actor = sys
        .spawn_at(
            Echo {
                counts: counts.clone(),
                fail_init: false,
            },
            "/blocked",
            None,
            config(Arc::new(p), modes()[0].clone()),
        )
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    let mut first = Box::pin(actor.stop());
    assert!(futures::poll!(&mut first).is_pending());
    entered(&gate).await;
    drop(first);
    let mut second = Box::pin(actor.stop());
    assert!(futures::poll!(&mut second).is_pending());
    let mut third = Box::pin(actor.stop());
    assert!(futures::poll!(&mut third).is_pending());
    assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
    gate.release();
    let (second, third) = tokio::join!(second, third);
    assert!(matches!(&second, Err(ActorError::Panic(_))));
    assert_eq!(
        second.unwrap_err().to_string(),
        third.unwrap_err().to_string()
    );
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn deadline_does_not_release_path_or_policy_before_actual_disposal() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_millis(500));
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut p = policy(&counts);
    p.drop_gate = Some(gate.clone());
    let actor = sys
        .spawn_at(
            Echo {
                counts: counts.clone(),
                fail_init: false,
            },
            "/deadline",
            None,
            config(Arc::new(p), modes()[0].clone()),
        )
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    let mut stopped = Box::pin(actor.stop());
    assert!(futures::poll!(&mut stopped).is_pending());
    entered(&gate).await;
    assert!(matches!(stopped.await, Err(ActorError::TimeoutDetail(_))));
    assert_eq!(
        sys.actor_count(),
        1,
        "timed-out owner still reserves its path"
    );
    assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
    gate.release();
    wait_for(|| sys.actor_count() == 0).await;
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn successful_restart_reuses_policy_until_final_logical_stop() {
    for mode in modes() {
        let counts = Arc::new(Counts::default());
        let sys = system(Duration::from_secs(3));
        let created = counts.clone();
        let mut cfg = config(Arc::new(policy(&counts)), mode);
        cfg.supervisor_strategy = Some(SupervisorStrategy::Restart {
            max_retries: 1,
            within: Duration::from_secs(10),
        });
        let actor = sys
            .spawn_supervised_with_config(
                move || Echo {
                    counts: created.clone(),
                    fail_init: false,
                },
                "/restart",
                cfg,
            )
            .await
            .unwrap();
        actor.send(Box::new(0u64)).await.unwrap();
        assert!(actor.send(Box::new(99u64)).await.is_err());
        wait_for(|| counts.initialized.load(Ordering::SeqCst) == 2).await;
        assert_eq!(
            *actor
                .send(Box::new(8u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            8
        );
        assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
        actor.stop().await.unwrap();
        assert_eq!(counts.actors.load(Ordering::SeqCst), 2);
        assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
        sys.shutdown_internal().await.unwrap();
    }
}
#[tokio::test]
async fn exhausted_restart_retires_policy_once() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let created = counts.clone();
    let mut cfg = config(Arc::new(policy(&counts)), modes()[0].clone());
    cfg.supervisor_strategy = Some(SupervisorStrategy::Restart {
        max_retries: 1,
        within: Duration::from_secs(10),
    });
    let actor = sys
        .spawn_supervised_with_config(
            move || Echo {
                counts: created.clone(),
                fail_init: false,
            },
            "/exhaust",
            cfg,
        )
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    assert!(actor.send(Box::new(99u64)).await.is_err());
    wait_for(|| counts.initialized.load(Ordering::SeqCst) == 2).await;
    actor.send(Box::new(0u64)).await.unwrap();
    assert!(actor.send(Box::new(99u64)).await.is_err());
    wait_for(|| sys.actor_count() == 0).await;
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 2);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn initialization_failure_still_reports_policy_disposal_failure() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut p = policy(&counts);
    p.drop_gate = Some(gate.clone());
    p.drop_panic = 1;
    let actor = sys
        .spawn_at(
            Echo {
                counts: counts.clone(),
                fail_init: true,
            },
            "/init",
            None,
            config(Arc::new(p), modes()[0].clone()),
        )
        .await
        .unwrap();
    entered(&gate).await;
    let mut stopped = Box::pin(actor.stop());
    assert!(futures::poll!(&mut stopped).is_pending());
    gate.release();
    assert!(matches!(stopped.await, Err(ActorError::Panic(_))));
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    sys.shutdown_internal().await.unwrap();
}
#[tokio::test]
async fn cancelled_pending_constructor_keeps_policy_until_real_retirement() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let captured = gate.clone();
    let created = counts.clone();
    let owner = sys.clone();
    let mut p = policy(&counts);
    p.drop_panic = 1;
    let spawn = tokio::spawn(async move {
        owner
            .spawn_supervised_with_config(
                move || {
                    captured.block();
                    Echo {
                        counts: created.clone(),
                        fail_init: false,
                    }
                },
                "/pending",
                config(Arc::new(p), modes()[0].clone()),
            )
            .await
    });
    entered(&gate).await;
    spawn.abort();
    let _ = spawn.await;
    let mut stopped = Box::pin(sys.stop_actor("/pending"));
    assert!(futures::poll!(&mut stopped).is_pending());
    assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
    gate.release();
    assert!(matches!(stopped.await, Err(SystemError::Panic(_))));
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn final_callback_not_first_callback_releases_policy() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let one = Arc::new(Gate::default());
    let two = Arc::new(Gate::default());
    let _one = Release(one.clone());
    let _two = Release(two.clone());
    let mut p = policy(&counts);
    p.callback_gate = Some(one.clone());
    p.callback_gate2 = Some(two.clone());
    let actor = sys
        .spawn_at(
            Echo {
                counts: counts.clone(),
                fail_init: false,
            },
            "/two",
            None,
            config(Arc::new(p), modes()[0].clone()),
        )
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    let a = actor.clone();
    let b = actor.clone();
    let first =
        tokio::task::spawn_blocking(move || futures::executor::block_on(a.deliver(Box::new(5u64))));
    let last =
        tokio::task::spawn_blocking(move || futures::executor::block_on(b.deliver(Box::new(6u64))));
    entered(&one).await;
    entered(&two).await;
    let mut stopped = Box::pin(actor.stop());
    assert!(futures::poll!(&mut stopped).is_pending());
    wait_for(|| counts.actors.load(Ordering::SeqCst) == 1).await;
    one.release();
    assert!(first.await.unwrap().is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut stopped)
            .await
            .is_err()
    );
    assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
    two.release();
    assert!(last.await.unwrap().is_err());
    stopped.await.unwrap();
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn old_generation_callback_remains_owned_through_restart() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let mut p = policy(&counts);
    p.callback_gate = Some(gate.clone());
    let mut cfg = config(Arc::new(p), modes()[0].clone());
    cfg.supervisor_strategy = Some(SupervisorStrategy::Restart {
        max_retries: 1,
        within: Duration::from_secs(10),
    });
    let made = counts.clone();
    let actor = sys
        .spawn_supervised_with_config(
            move || Echo {
                counts: made.clone(),
                fail_init: false,
            },
            "/old-callback",
            cfg,
        )
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    let original = actor.clone();
    let producer = tokio::task::spawn_blocking(move || {
        futures::executor::block_on(original.deliver(Box::new(5u64)))
    });
    entered(&gate).await;
    assert!(actor.send(Box::new(99u64)).await.is_err());
    wait_for(|| counts.initialized.load(Ordering::SeqCst) == 2).await;
    assert_eq!(
        *actor
            .send(Box::new(8u64))
            .await
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        8
    );
    let mut stopped = Box::pin(actor.stop());
    assert!(futures::poll!(&mut stopped).is_pending());
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut stopped)
            .await
            .is_err()
    );
    assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
    gate.release();
    assert!(producer.await.unwrap().is_err());
    stopped.await.unwrap();
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 2);
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn constructor_panic_keeps_policy_disposal_in_pending_stop_receipt() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let constructor = Arc::new(Gate::default());
    let disposal = Arc::new(Gate::default());
    let _constructor = Release(constructor.clone());
    let _disposal = Release(disposal.clone());
    let mut p = policy(&counts);
    p.drop_gate = Some(disposal.clone());
    p.drop_panic = 1;
    let captured = constructor.clone();
    let owner = sys.clone();
    let spawn = tokio::spawn(async move {
        owner
            .spawn_supervised_with_config(
                move || -> Echo {
                    captured.block();
                    panic!("constructor failure");
                },
                "/constructor-panic",
                config(Arc::new(p), modes()[0].clone()),
            )
            .await
    });
    entered(&constructor).await;
    constructor.release();
    entered(&disposal).await;
    let mut stopped = Box::pin(sys.stop_actor("/constructor-panic"));
    assert!(futures::poll!(&mut stopped).is_pending());
    disposal.release();
    assert!(matches!(stopped.await, Err(SystemError::Panic(_))));
    assert!(
        spawn
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("constructor failure")
    );
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 0);
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn rejected_factory_joins_managed_policy_and_raw_spare_config() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let called = Arc::new(AtomicUsize::new(0));
    let constructor_calls = called.clone();
    let created = counts.clone();
    let mut p = policy(&counts);
    p.drop_panic = 1;
    let mut cfg = config(Arc::new(p), modes()[0].clone());
    cfg.message_batch_size = Some(0);
    let outcome = sys
        .spawn_supervised_with_config(
            move || {
                constructor_calls.fetch_add(1, Ordering::SeqCst);
                Echo {
                    counts: created.clone(),
                    fail_init: false,
                }
            },
            "/refused",
            cfg,
        )
        .await;
    assert!(matches!(
        outcome,
        Err(parrot::thread::error::SpawnError::DisposalFailed(_))
    ));
    assert_eq!(called.load(Ordering::SeqCst), 0);
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn cancelled_stop_during_replacement_keeps_policy_until_constructor_retires() {
    let counts = Arc::new(Counts::default());
    let sys = system(Duration::from_secs(3));
    let gate = Arc::new(Gate::default());
    let _release = Release(gate.clone());
    let made = Arc::new(AtomicUsize::new(0));
    let calls = made.clone();
    let captured = gate.clone();
    let created = counts.clone();
    let mut p = policy(&counts);
    p.drop_panic = 1;
    let mut cfg = config(Arc::new(p), modes()[0].clone());
    cfg.supervisor_strategy = Some(SupervisorStrategy::Restart {
        max_retries: 1,
        within: Duration::from_secs(10),
    });
    let actor = sys
        .spawn_supervised_with_config(
            move || {
                if calls.fetch_add(1, Ordering::SeqCst) != 0 {
                    captured.block();
                }
                Echo {
                    counts: created.clone(),
                    fail_init: false,
                }
            },
            "/replacement-cancel",
            cfg,
        )
        .await
        .unwrap();
    actor.send(Box::new(0u64)).await.unwrap();
    assert!(actor.send(Box::new(99u64)).await.is_err());
    entered(&gate).await;
    let mut abandoned = Box::pin(actor.stop());
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut abandoned)
            .await
            .is_err()
    );
    drop(abandoned);
    assert_eq!(sys.actor_count(), 1);
    assert_eq!(counts.policies.load(Ordering::SeqCst), 0);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 1);
    let mut stopped = Box::pin(actor.stop());
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut stopped)
            .await
            .is_err()
    );
    gate.release();
    let result = stopped.await;
    assert!(
        matches!(result, Err(ActorError::Panic(ref message)) if message.contains("admission policy disposal failed"))
    );
    assert_eq!(counts.policies.load(Ordering::SeqCst), 1);
    assert_eq!(counts.actors.load(Ordering::SeqCst), 2);
    assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(made.load(Ordering::SeqCst), 2);
    assert_eq!(sys.actor_count(), 0);
    sys.shutdown_internal().await.unwrap();
}
