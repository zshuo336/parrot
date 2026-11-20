use super::*;
use crate::thread::{
    ProcessorInterface, SystemError, ThreadActorConfig, ThreadActorSystem, ThreadActorSystemConfig,
    ThreadContext,
    mailbox::{Mailbox, WeakMailboxRef, mpsc::MpscMailbox},
    processor::{ProcessorStats, ProcessorStatsTrait},
    tests_support::DummyActor,
};
use parrot_api::address::ActorPath;
use std::{any::Any, fmt, panic::AssertUnwindSafe, sync::atomic::AtomicUsize};

#[derive(Default)]
struct Counts {
    drops: AtomicUsize,
    payloads: AtomicUsize,
    diagnostics: AtomicUsize,
    decisions: AtomicUsize,
}
struct Payload(Arc<Counts>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("recursive escalation input payload");
    }
}
struct Business {
    counts: Arc<Counts>,
    recursive: bool,
}
impl fmt::Debug for Business {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.counts.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("unexpected input Debug");
    }
}
impl fmt::Display for Business {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.counts.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("unexpected input Display");
    }
}
impl std::error::Error for Business {}
impl Drop for Business {
    fn drop(&mut self) {
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
        if self.recursive {
            std::panic::panic_any(Payload(self.counts.clone()));
        }
        panic!("escalation input destructor");
    }
}
fn error(counts: &Arc<Counts>, recursive: bool) -> Arc<ActorError> {
    Arc::new(ActorError::Other(anyhow::Error::new(Business {
        counts: counts.clone(),
        recursive,
    })))
}
fn caught(action: impl FnOnce()) -> Option<String> {
    std::panic::catch_unwind(AssertUnwindSafe(action))
        .err()
        .map(crate::owned_payload::panic_message)
}
fn no_unwind(action: impl FnOnce()) {
    let escaped = caught(action);
    assert!(
        escaped.is_none(),
        "escalation input escaped its owner: {escaped:?}"
    );
}
async fn settled(counts: &Counts, recursive: bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while counts.drops.load(Ordering::SeqCst) != 1
            || counts.payloads.load(Ordering::SeqCst) != usize::from(recursive)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("error owner was abandoned or disposed more than once");
    assert_eq!(counts.diagnostics.load(Ordering::SeqCst), 0);
}

struct DefaultProcessor;
impl ProcessorStatsTrait for DefaultProcessor {
    fn get_statistics(&self) -> Option<Arc<ProcessorStats>> {
        None
    }
}
impl ProcessorInterface for DefaultProcessor {
    fn is_initialized(&self) -> bool {
        false
    }
    fn initialize_and_start_erased(
        self: Arc<Self>,
    ) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
    fn stop_erased(self: Arc<Self>) -> BoxedFuture<'static, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
    fn process_batch_erased(
        self: Arc<Self>,
        _: Arc<dyn Mailbox>,
        _: usize,
        _: bool,
    ) -> BoxedFuture<'static, Result<(usize, usize), SystemError>> {
        Box::pin(async { Ok((0, 0)) })
    }
    fn as_any(self: Arc<Self>) -> Arc<dyn Any> {
        self
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
async fn default_method(fail: bool, recursive: bool) {
    let counts = Arc::new(Counts::default());
    let processor: Arc<dyn ProcessorInterface> = Arc::new(DefaultProcessor);
    no_unwind(|| {
        if fail {
            processor.fail_from_escalation(error(&counts, recursive));
        } else {
            assert!(!processor.begin_escalation(error(&counts, recursive)));
        }
    });
    settled(&counts, recursive).await;
    assert!(!processor.has_failed());
    assert!(!processor.begin_escalation(Arc::new(ActorError::Stopped)));
}

async fn unpolled(kind: u8, recursive: bool) {
    let counts = Arc::new(Counts::default());
    let observed = counts.clone();
    let lifetime = ChildLifetime::with_policy(
        Weak::new(),
        Some(SupervisorStrategyType::OneForOne(
            parrot_api::supervisor::OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(1),
                decider: parrot_api::supervisor::BasicDecisionFn::new(move |_| {
                    observed.decisions.fetch_add(1, Ordering::SeqCst);
                    SupervisionDecision::Resume
                }),
            },
        )),
        Arc::new(Semaphore::new(1)),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let runtime = tokio::runtime::Handle::current();
    no_unwind(|| match kind {
        0 => drop(lifetime.resume_message(error(&counts, recursive), deadline, false, &runtime)),
        1 => drop(lifetime.resume_escalation(error(&counts, recursive), deadline, &runtime)),
        2 => drop(lifetime.failure_policy(
            error(&counts, recursive),
            Arc::new(Semaphore::new(1)),
            deadline,
        )),
        _ => unreachable!(),
    });
    settled(&counts, recursive).await;
    assert_eq!(
        counts.decisions.load(Ordering::SeqCst),
        0,
        "unpolled waiter ran business policy"
    );
    assert!(
        lifetime.decision_deadline().is_none(),
        "unpolled waiter published a decision"
    );
    lifetime.dispose_policy().await.unwrap();
}

fn mailbox(path: &str) -> Arc<dyn Mailbox + Send + Sync> {
    Arc::new(MpscMailbox::new(8, ActorPath::for_test(path)))
}
struct Request;
impl parrot_api::message::Message for Request {
    type Result = ();
}
struct StaticProbe;
impl parrot_api::typed::ParrotTypedDispatch for StaticProbe {
    type Msg = Request;
    type Reply = ();
    fn dispatch(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl parrot_api::typed::ParrotMsgVariant<StaticProbe> for Request {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
async fn dead_context(recursive: bool) {
    let mut context = ThreadContext::<DummyActor>::new_for_test("/dead-context");
    let mailbox = mailbox("/dead-context");
    context.set_mailbox(Arc::downgrade(&mailbox));
    let hook = context.owned_children.escalator.get().unwrap().clone();
    let counts = Arc::new(Counts::default());
    no_unwind(|| hook(error(&counts, recursive)));
    settled(&counts, recursive).await;
}

async fn rejected(kind: u8, recursive: bool) {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 1,
        ..Default::default()
    });
    let actor = system
        .spawn_at::<DummyActor>(DummyActor, "/input", None, ThreadActorConfig::default())
        .await
        .unwrap();
    let actual = system.get_mailbox("/input").unwrap() as Arc<dyn Mailbox + Send + Sync>;
    let mut expected: WeakMailboxRef = Arc::downgrade(&actual);
    let mut path = "/input";
    let unrelated = mailbox("/other");
    match kind {
        0 => {
            let expired = mailbox("/expired");
            expected = Arc::downgrade(&expired);
        }
        1 => path = "/missing",
        2 => expected = Arc::downgrade(&unrelated),
        3 => {
            system
                .registry
                .read()
                .unwrap()
                .get("/input")
                .unwrap()
                .dynamic()
                .unwrap()
                .lifetime
                .cancel_recovery();
        }
        4 => {
            let (_, done) = watch::channel(None);
            system
                .registry
                .write()
                .unwrap()
                .get_mut("/input")
                .unwrap()
                .dynamic_mut()
                .unwrap()
                .stop_completion = Some(done);
        }
        5 => {
            system
                .registry
                .write()
                .unwrap()
                .get_mut("/input")
                .unwrap()
                .dynamic_mut()
                .unwrap()
                .mailbox = unrelated.clone();
            expected = Arc::downgrade(&unrelated);
        }
        6 => {
            system
                .registry
                .write()
                .unwrap()
                .get_mut("/input")
                .unwrap()
                .dynamic_mut()
                .unwrap()
                .mailbox = unrelated.clone();
            unrelated.set_processor(Arc::new(DefaultProcessor));
            expected = Arc::downgrade(&unrelated);
        }
        7 => {
            system
                .spawn_typed::<_, Request>(StaticProbe, "/static")
                .await
                .unwrap();
            path = "/static";
        }
        _ => unreachable!(),
    }
    let counts = Arc::new(Counts::default());
    let escaped = caught(|| {
        system.runtime_spawn_escalation(path.into(), expected, error(&counts, recursive))
    });
    // Restore deliberately displaced registry inputs before normal shutdown.
    {
        let mut registry = system.registry.write().unwrap();
        let entry = registry.get_mut("/input").unwrap().dynamic_mut().unwrap();
        entry.mailbox = actual;
        if kind == 4 {
            entry.stop_completion = None;
        }
    }
    system.shutdown_internal().await.unwrap();
    drop(actor);
    assert!(
        escaped.is_none(),
        "dynamic escalation rejection escaped: {escaped:?}"
    );
    settled(&counts, recursive).await;
}

fn queued(supervision: bool, recursive: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut system = ThreadActorSystem::with_runtime_handle(
        ThreadActorSystemConfig::default(),
        runtime.handle().clone(),
    );
    // Freeze the actual owner to test cancellation before its first poll.
    // Public host-exit tests exercise production's independent owner wiring.
    Arc::get_mut(&mut system.root_cohorts)
        .unwrap()
        .set_test_retirement_runtime(runtime.handle().clone());
    let system = Arc::new(system);
    system.set_self_weak(Arc::downgrade(&system));
    let counts = Arc::new(Counts::default());
    let mailbox = mailbox("/queued");
    if supervision {
        system.runtime_spawn_supervision(
            "/queued".into(),
            Arc::downgrade(&mailbox),
            error(&counts, recursive),
        );
    } else {
        let lifetime = ChildLifetime::new(Weak::new());
        let waiter = lifetime.start_escalation(
            error(&counts, recursive),
            tokio::time::Instant::now() + Duration::from_secs(1),
            false,
            runtime.handle(),
        );
        drop(waiter);
    }
    no_unwind(|| drop(runtime));
    // Dropping a current-thread runtime aborts the async owner, but a
    // cleanup value may already be queued on Tokio's blocking executor. The
    // contract is eventual, exactly-once disposal; observing counters in the
    // same instant races that executor and makes this regression flaky under
    // the full library suite.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while (counts.drops.load(Ordering::SeqCst) != 1
        || counts.payloads.load(Ordering::SeqCst) != usize::from(recursive))
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        usize::from(recursive)
    );
    assert_eq!(counts.diagnostics.load(Ordering::SeqCst), 0);
}

fn isolated(name: &str, action: impl FnOnce()) {
    let full = format!("children::escalation_inputs::{name}");
    if std::env::var("PARROT_ESCALATION_INPUT_CHILD").as_deref() == Ok(&full) {
        action();
        println!("escalation input completed: {full}");
        return;
    }
    let log = tempfile::NamedTempFile::new().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([&full, "--exact", "--nocapture", "--test-threads=1"])
        .env("PARROT_ESCALATION_INPUT_CHILD", &full)
        .stdout(log.as_file().try_clone().unwrap())
        .stderr(log.as_file().try_clone().unwrap())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let timed_out = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            break true;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let status = child.wait().unwrap();
    let output = std::fs::read_to_string(log.path()).unwrap();
    assert!(
        !timed_out && status.success(),
        "queued input child failed: timeout={timed_out}, status={:?}\n{}",
        status,
        output
    );
    assert!(output.contains(&format!("escalation input completed: {full}")));
}

async fn retirement_unpolled(failure: bool, recursive: bool) {
    let system = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    let mailbox = mailbox("/retirement");
    let counts = Arc::new(Counts::default());
    no_unwind(|| {
        if failure {
            drop(system.on_child_failure(
                "/retirement",
                Arc::downgrade(&mailbox),
                error(&counts, recursive),
                parrot_api::supervisor::DeathReason::Normal,
            ));
        } else {
            drop(system.retire_generation(
                "/retirement",
                Arc::downgrade(&mailbox),
                error(&counts, recursive),
                parrot_api::supervisor::DeathReason::Normal,
                false,
            ));
        }
    });
    settled(&counts, recursive).await;
    assert_eq!(system.actor_count(), 0);
    system.shutdown_internal().await.unwrap();
}

#[tokio::test]
async fn shared_raw_input_remains_owned_by_its_caller() {
    let counts = Arc::new(Counts::default());
    let original = error(&counts, false);
    let processor = DefaultProcessor;
    assert!(!processor.begin_escalation(original.clone()));
    processor.fail_from_escalation(original.clone());
    let lifetime = ChildLifetime::new(Weak::new());
    let runtime = tokio::runtime::Handle::current();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    drop(lifetime.resume_message(original.clone(), deadline, false, &runtime));
    drop(lifetime.resume_escalation(original.clone(), deadline, &runtime));
    drop(lifetime.failure_policy(original.clone(), Arc::new(Semaphore::new(1)), deadline));
    tokio::time::timeout(Duration::from_secs(3), async {
        while Arc::strong_count(&original) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(counts.drops.load(Ordering::SeqCst), 0);
    assert_eq!(counts.diagnostics.load(Ordering::SeqCst), 0);
    // The framework does not take over references retained by external callers.
    assert!(caught(|| drop(original)).is_some());
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
}

async fn polled_identity(kind: u8) {
    let error = Arc::new(ActorError::InternalError("original cause".into()));
    let address = Arc::as_ptr(&error) as usize;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let lifetime = ChildLifetime::with_policy(
        Weak::new(),
        Some(SupervisorStrategyType::OneForOne(
            parrot_api::supervisor::OneForOneStrategy {
                max_restarts: 1,
                within: Duration::from_secs(1),
                decider: parrot_api::supervisor::BasicDecisionFn::new(move |cause| {
                    assert_eq!(cause as *const ActorError as usize, address);
                    observed.fetch_add(1, Ordering::SeqCst);
                    SupervisionDecision::Resume
                }),
            },
        )),
        Arc::new(Semaphore::new(1)),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let runtime = tokio::runtime::Handle::current();
    match kind {
        0 => assert!(
            lifetime
                .resume_message(error, deadline, false, &runtime)
                .await
        ),
        1 => assert!(lifetime.resume_escalation(error, deadline, &runtime).await),
        2 => assert!(matches!(
            lifetime
                .failure_policy(error, Arc::new(Semaphore::new(1)), deadline)
                .await
                .unwrap()
                .unwrap()
                .decision,
            SupervisionDecision::Resume
        )),
        _ => unreachable!(),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(lifetime.decision_deadline().is_none());
    lifetime.dispose_policy().await.unwrap();
}

macro_rules! async_case {
    ($name:ident, $body:expr) => {
        #[tokio::test]
        async fn $name() {
            $body.await;
        }
    };
}
async_case!(default_begin_panic, default_method(false, false));
async_case!(default_begin_recursive, default_method(false, true));
async_case!(default_fail_panic, default_method(true, false));
async_case!(default_fail_recursive, default_method(true, true));
async_case!(unpolled_message_panic, unpolled(0, false));
async_case!(unpolled_message_recursive, unpolled(0, true));
async_case!(unpolled_escalation_panic, unpolled(1, false));
async_case!(unpolled_escalation_recursive, unpolled(1, true));
async_case!(unpolled_policy_panic, unpolled(2, false));
async_case!(unpolled_policy_recursive, unpolled(2, true));
async_case!(dead_context_panic, dead_context(false));
async_case!(dead_context_recursive, dead_context(true));
async_case!(expired_mailbox_panic, rejected(0, false));
async_case!(expired_mailbox_recursive, rejected(0, true));
async_case!(missing_entry_panic, rejected(1, false));
async_case!(missing_entry_recursive, rejected(1, true));
async_case!(different_generation_panic, rejected(2, false));
async_case!(different_generation_recursive, rejected(2, true));
async_case!(stopping_entry_panic, rejected(3, false));
async_case!(stopping_entry_recursive, rejected(3, true));
async_case!(completing_entry_panic, rejected(4, false));
async_case!(completing_entry_recursive, rejected(4, true));
async_case!(missing_processor_panic, rejected(5, false));
async_case!(missing_processor_recursive, rejected(5, true));
async_case!(rejecting_processor_panic, rejected(6, false));
async_case!(rejecting_processor_recursive, rejected(6, true));
async_case!(static_entry_panic, rejected(7, false));
async_case!(static_entry_recursive, rejected(7, true));
async_case!(polled_message_preserves_original, polled_identity(0));
async_case!(polled_escalation_preserves_original, polled_identity(1));
async_case!(polled_policy_preserves_original, polled_identity(2));
async_case!(unpolled_failure_panic, retirement_unpolled(true, false));
async_case!(unpolled_failure_recursive, retirement_unpolled(true, true));
async_case!(unpolled_retirement_panic, retirement_unpolled(false, false));
async_case!(
    unpolled_retirement_recursive,
    retirement_unpolled(false, true)
);
#[test]
fn queued_supervision_panic() {
    isolated("queued_supervision_panic", || queued(true, false));
}
#[test]
fn queued_supervision_recursive() {
    isolated("queued_supervision_recursive", || queued(true, true));
}
#[test]
fn queued_escalation_panic() {
    isolated("queued_escalation_panic", || queued(false, false));
}
#[test]
fn queued_escalation_recursive() {
    isolated("queued_escalation_recursive", || queued(false, true));
}
