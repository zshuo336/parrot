use super::*;
use crate::actix::{ActixActorSystem, ActixSystemSettings};
use crate::thread::{ThreadActorSystemConfig, system::ActorRegistryEntry};
use parrot_api::{local_typed::LocalBoxedFuture, message::Message};
use std::{fmt, panic::AssertUnwindSafe, sync::atomic::AtomicUsize};

struct Request;
impl Message for Request {
    type Result = ();
}
struct Probe;
impl ParrotTypedDispatch for Probe {
    type Msg = Request;
    type Reply = ();
    fn dispatch(&mut self, _: Request) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl ParrotMsgVariant<Probe> for Request {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
struct LocalProbe;
impl LocalTypedDispatch for LocalProbe {
    type Msg = Request;
    type Reply = ();
    fn dispatch<'a>(
        &'a mut self,
        _: Request,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
impl ParrotMsgVariant<LocalProtocol<LocalProbe>> for Request {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}

enum Engine {
    Thread(Arc<ThreadActorSystem>),
    Actix(Arc<ActixActorSystem>),
}
#[derive(Clone)]
enum Control {
    Thread(Arc<crate::thread::typed::TypedControl>),
    Actix(Arc<crate::actix::ActorControl>),
}
impl Control {
    fn hook(&self) -> crate::children::Escalate {
        match self {
            Self::Thread(c) => c.escalation_hook(),
            Self::Actix(c) => c.typed_tree().escalation_hook(),
        }
    }
    fn stop(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        match self {
            Self::Thread(c) => c.request_stop(deadline),
            Self::Actix(c) => c.request_stop(deadline),
        }
    }
    fn check_locks(&self) {
        match self {
            Self::Thread(c) => c.check_escalation_locks(),
            Self::Actix(c) => c.check_escalation_locks(),
        }
    }
    async fn deadline_waiter(&self) {
        match self {
            Self::Thread(c) => c.escalation_deadline_waiter().await,
            Self::Actix(c) => c.escalation_deadline_waiter().await,
        }
    }
    fn alive(&self) -> Box<dyn Fn() -> bool> {
        match self {
            Self::Thread(c) => {
                let weak = Arc::downgrade(c);
                Box::new(move || weak.upgrade().is_some())
            }
            Self::Actix(c) => {
                let weak = Arc::downgrade(c);
                Box::new(move || weak.upgrade().is_some())
            }
        }
    }
    fn pending(&self) -> Option<Arc<ActorError>> {
        match self {
            Self::Thread(c) => c.pending_escalation_error(),
            Self::Actix(c) => c.escalated.lock().unwrap().as_ref().map(|e| Arc::clone(e)),
        }
    }
}
async fn fixture(engine: u8) -> (Engine, Control) {
    fixture_policy(
        engine,
        SupervisorStrategyType::Default(parrot_api::supervisor::DefaultStrategy::StopOnFailure),
    )
    .await
}
async fn fixture_policy(engine: u8, policy: SupervisorStrategyType) -> (Engine, Control) {
    if engine == 0 {
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig {
            shared_pool_size: 1,
            ..Default::default()
        });
        let actor = system
            .spawn_typed_with_strategy::<_, Request>(Probe, "/signal-contract", policy)
            .await
            .unwrap();
        actor.ask(Request).await.unwrap();
        let control = match system
            .registry
            .read()
            .unwrap()
            .get("/signal-contract")
            .unwrap()
        {
            ActorRegistryEntry::Typed(entry) => entry.control.clone(),
            _ => unreachable!(),
        };
        (Engine::Thread(system), Control::Thread(control))
    } else {
        let system = Arc::new(
            ActixActorSystem::with_settings(ActixSystemSettings {
                shutdown_timeout: Duration::from_secs(2),
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        if engine == 1 {
            system
                .spawn_typed_with_strategy::<_, Request>(Probe, "/signal-contract", policy)
                .await
                .unwrap()
                .ask(Request)
                .await
                .unwrap();
        } else {
            system
                .spawn_local_typed_with_strategy::<LocalProbe, Request, _>(
                    || LocalProbe,
                    "/signal-contract",
                    policy,
                )
                .await
                .unwrap()
                .ask(Request)
                .await
                .unwrap();
        }
        let control = system
            .registry
            .read()
            .unwrap()
            .actors
            .get("/signal-contract")
            .unwrap()
            .control
            .clone();
        (Engine::Actix(system), Control::Actix(control))
    }
}
async fn shutdown_result(engine: Engine) -> Result<(), String> {
    match engine {
        Engine::Thread(system) => system.shutdown_internal().await.map_err(|e| e.to_string()),
        Engine::Actix(system) => system
            .as_ref()
            .clone()
            .shutdown()
            .await
            .map_err(|e| e.to_string()),
    }
}
async fn shutdown(engine: Engine) {
    shutdown_result(engine).await.unwrap();
}
async fn until(mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !check() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("signal ownership did not settle");
}
struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("recursive signal payload");
    }
}
struct Hostile {
    drops: Arc<AtomicUsize>,
    payloads: Arc<AtomicUsize>,
    recursive: bool,
    reenter: Option<Box<dyn Fn() + Send + Sync>>,
}
impl fmt::Debug for Hostile {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("unexpected signal Debug")
    }
}
impl fmt::Display for Hostile {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("unexpected signal Display")
    }
}
impl std::error::Error for Hostile {}
impl Drop for Hostile {
    fn drop(&mut self) {
        if let Some(reenter) = &self.reenter {
            reenter();
        }
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.recursive {
            std::panic::panic_any(Payload(self.payloads.clone()));
        }
        panic!("signal error destructor");
    }
}
async fn rejected(engine: u8, expired: bool, recursive: bool) {
    let (system, control) = fixture(engine).await;
    let hook = control.hook();
    let alive = control.alive();
    control.stop();
    // Keep a closed control alive to distinguish closure rejection from Weak failure.
    let retained = if expired {
        drop(control);
        shutdown(system).await;
        None
    } else {
        shutdown(system).await;
        Some(control)
    };
    if expired {
        until(|| !alive()).await;
    } else {
        assert!(alive());
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(AtomicUsize::new(0));
    let error = Arc::new(ActorError::Other(anyhow::Error::new(Hostile {
        drops: drops.clone(),
        payloads: payloads.clone(),
        recursive,
        reenter: None,
    })));
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| hook(error)));
    if let Err(payload) = result {
        std::mem::forget(payload);
        panic!("actual installed hook let rejected error unwind");
    }
    until(|| {
        drops.load(Ordering::SeqCst) == 1
            && payloads.load(Ordering::SeqCst) == usize::from(recursive)
    })
    .await;
    drop(retained);
}
macro_rules! rejected_tests {
    ($closed:ident, $expired:ident, $recursive:ident, $engine:expr) => {
        #[actix::test]
        async fn $closed() {
            rejected($engine, false, false).await;
        }
        #[actix::test]
        async fn $expired() {
            rejected($engine, true, false).await;
        }
        #[actix::test]
        async fn $recursive() {
            rejected($engine, true, true).await;
        }
    };
}
rejected_tests!(
    thread_closed_rejection,
    thread_expired_rejection,
    thread_recursive_rejection,
    0
);
rejected_tests!(
    actix_closed_rejection,
    actix_expired_rejection,
    actix_recursive_rejection,
    1
);
rejected_tests!(
    local_closed_rejection,
    local_expired_rejection,
    local_recursive_rejection,
    2
);

struct Reenter {
    control: Control,
    wakes: AtomicUsize,
}
impl std::task::Wake for Reenter {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.control.check_locks();
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}
fn child_contract(name: &str) -> bool {
    const CHILD: &str = "PARROT_SIGNAL_CONTRACT_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return true;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "signal subprocess failed: {status}");
            return false;
        }
        if started.elapsed() > Duration::from_secs(10) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("signal callback deadlocked while reentering control locks");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
async fn hook_waker(engine: u8) {
    use std::{
        future::Future,
        task::{Context, Waker},
    };
    let (system, control) = fixture(engine).await;
    let wakes = Arc::new(Reenter {
        control: control.clone(),
        wakes: AtomicUsize::new(0),
    });
    let waker = Waker::from(wakes.clone());
    let mut waiter = Box::pin(control.deadline_waiter());
    assert!(
        waiter
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    (control.hook())(Arc::new(ActorError::Stopped));
    assert!(
        wakes.wakes.load(Ordering::SeqCst) > 0,
        "hook never woke the real deadline waiter"
    );
    drop(waiter);
    control.stop();
    if let Err(error) = shutdown_result(system).await {
        assert!(
            error.contains("Actor stopped") || error.contains("Stopped"),
            "unexpected fixture retirement: {error}"
        );
    }
}
macro_rules! waker_test {
    ($test:ident, $engine:expr) => {
        #[actix::test]
        async fn $test() {
            if child_contract(concat!(
                "typed_context::signal_contracts::",
                stringify!($test)
            )) {
                hook_waker($engine).await;
            }
        }
    };
}
waker_test!(thread_hook_waker_reenters_signal_and_deadline, 0);
waker_test!(actix_hook_waker_reenters_signal_and_deadline, 1);
waker_test!(local_hook_waker_reenters_signal_and_deadline, 2);

fn hostile(
    recursive: bool,
    reenter: Option<Box<dyn Fn() + Send + Sync>>,
) -> (Arc<ActorError>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let drops = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(AtomicUsize::new(0));
    let error = Arc::new(ActorError::Other(anyhow::Error::new(Hostile {
        drops: drops.clone(),
        payloads: payloads.clone(),
        recursive,
        reenter,
    })));
    (error, drops, payloads)
}
fn isolated(action: impl FnOnce()) -> bool {
    match std::panic::catch_unwind(AssertUnwindSafe(action)) {
        Ok(()) => true,
        Err(payload) => {
            std::mem::forget(payload);
            false
        }
    }
}
async fn disposed(drops: &AtomicUsize, payloads: &AtomicUsize, recursive: bool) {
    until(|| {
        drops.load(Ordering::SeqCst) == 1
            && payloads.load(Ordering::SeqCst) == usize::from(recursive)
    })
    .await;
}
async fn stored_signal(recursive: bool) {
    let (control, _finished) = crate::actix::ActorControl::new(Duration::from_secs(2));
    let weak = Arc::downgrade(&control);
    let (error, drops, payloads) = hostile(
        recursive,
        Some(Box::new(move || {
            weak.upgrade()
                .expect("control must remain alive during signal disposal")
                .check_escalation_locks();
        })),
    );
    let original = error.clone();
    *control.escalated.lock().unwrap() = Some(error.into());
    assert!(Arc::ptr_eq(
        control.escalated.lock().unwrap().as_ref().unwrap(),
        &original
    ));
    drop(original);
    let taken = control.escalated.lock().unwrap().take();
    let safe = isolated(|| drop(taken));
    assert!(safe, "taken signal lost protected final ownership");
    disposed(&drops, &payloads, recursive).await;
}
#[tokio::test]
async fn taken_signal_reentry_and_panic() {
    stored_signal(false).await;
}
#[tokio::test]
async fn taken_signal_reentry_and_recursive_panic() {
    stored_signal(true).await;
}

async fn final_signal(recursive: bool) {
    let (control, finished) = crate::actix::ActorControl::new(Duration::from_secs(2));
    let (error, drops, payloads) = hostile(recursive, None);
    *control.escalated.lock().unwrap() = Some(error.into());
    drop(finished);
    assert!(
        isolated(|| drop(control)),
        "final control let signal destructor unwind"
    );
    disposed(&drops, &payloads, recursive).await;
}
#[tokio::test]
async fn final_signal_panic() {
    final_signal(false).await;
}
#[tokio::test]
async fn final_signal_recursive_panic() {
    final_signal(true).await;
}

async fn no_parent(recursive: bool) {
    let lifetime = crate::children::ChildLifetime::new(std::sync::Weak::new());
    let (error, drops, payloads) = hostile(recursive, None);
    assert!(
        isolated(|| assert!(!lifetime.escalate(error))),
        "unowned escalation let error unwind"
    );
    disposed(&drops, &payloads, recursive).await;
}
#[tokio::test]
async fn no_parent_rejection_panic() {
    no_parent(false).await;
}
#[tokio::test]
async fn no_parent_rejection_recursive_panic() {
    no_parent(true).await;
}

async fn dispatch_failure(recursive: bool) {
    let lifetime = crate::children::ChildLifetime::new(std::sync::Weak::new());
    let (error, drops, payloads) = hostile(recursive, None);
    let error = Arc::try_unwrap(error).unwrap_or_else(|_| unreachable!());
    let mut first = None;
    let mut cancelled = false;
    let result = crate::typed_supervision::dispatch_stage(
        "/signal-dispatch",
        || std::future::ready(Err::<(), _>(error)),
        None,
        &lifetime,
        &tokio::runtime::Handle::current(),
        Duration::from_secs(2),
        crate::typed_supervision::DispatchCleanup::new(&mut first, &mut cancelled),
    )
    .await;
    assert!(result.is_err());
    assert!(
        first.is_none() && !cancelled,
        "cause disposal is not actor cleanup failure"
    );
    assert!(
        isolated(|| drop(result)),
        "dispatch returned an unprotected failure owner"
    );
    disposed(&drops, &payloads, recursive).await;
}
#[tokio::test]
async fn dispatch_failure_final_owner_panic() {
    dispatch_failure(false).await;
}
#[tokio::test]
async fn dispatch_failure_final_owner_recursive_panic() {
    dispatch_failure(true).await;
}

async fn reply_failure(recursive: bool) {
    let (error, drops, payloads) = hostile(recursive, None);
    let reply = crate::supervision_policy::reply_error(error);
    let ActorError::Other(ref cause) = reply else {
        panic!("reply lost the original error class");
    };
    assert!(
        cause
            .chain()
            .any(|cause| cause.downcast_ref::<Hostile>().is_some()),
        "reply lost the original concrete cause"
    );
    assert!(
        isolated(|| drop(reply)),
        "discarded reply lost protected ownership"
    );
    disposed(&drops, &payloads, recursive).await;
}
#[tokio::test]
async fn reply_failure_original_cause_and_panic() {
    reply_failure(false).await;
}
#[tokio::test]
async fn reply_failure_original_cause_and_recursive_panic() {
    reply_failure(true).await;
}
#[test]
fn reply_timeout_class_is_preserved() {
    assert!(matches!(
        crate::supervision_policy::reply_error(Arc::new(ActorError::Timeout)),
        ActorError::Timeout
    ));
}

#[derive(Default)]
struct Gate(std::sync::Mutex<bool>, std::sync::Condvar);
impl Gate {
    fn wait(&self) {
        let mut open = self.0.lock().unwrap();
        while !*open {
            open = self.1.wait(open).unwrap();
        }
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}
async fn duplicate_hook(engine: u8, rejected: Option<bool>) {
    use parrot_api::supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision};
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let policy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: Duration::from_secs(30),
        decider: BasicDecisionFn::new(move |_| {
            gate.wait();
            SupervisionDecision::Resume
        }),
    });
    let (system, control) = fixture_policy(engine, policy).await;
    let first = Arc::new(ActorError::Stopped);
    let hook = control.hook();
    hook(first.clone());
    if let Some(recursive) = rejected {
        let reenter = if recursive {
            let control = control.clone();
            Some(Box::new(move || control.check_locks()) as Box<dyn Fn() + Send + Sync>)
        } else {
            None
        };
        let (error, drops, payloads) = hostile(recursive, reenter);
        let safe = isolated(|| hook(error));
        assert!(safe, "duplicate signal let rejected error unwind");
        disposed(&drops, &payloads, recursive).await;
    }
    assert!(Arc::ptr_eq(
        &control.pending().expect("duplicate cleared pending cause"),
        &first
    ));
    use std::{
        future::Future,
        task::{Context, Waker},
    };
    let wakes = Arc::new(Reenter {
        control: control.clone(),
        wakes: AtomicUsize::new(0),
    });
    let waker = Waker::from(wakes.clone());
    let mut waiter = Box::pin(control.deadline_waiter());
    assert!(
        waiter
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(release);
    // The signal is cleared before its lock-free notification. Observe both
    // receipts; another executor may see the cleared state before wake returns.
    until(|| control.pending().is_none() && wakes.wakes.load(Ordering::SeqCst) > 0).await;
    drop(waiter);
    control.stop();
    shutdown(system).await;
}
macro_rules! duplicate_test {
    ($test:ident, $engine:expr, $recursive:expr) => {
        #[actix::test]
        async fn $test() {
            if child_contract(concat!(
                "typed_context::signal_contracts::",
                stringify!($test)
            )) {
                duplicate_hook($engine, $recursive).await;
            }
        }
    };
}
duplicate_test!(thread_duplicate_panic_preserves_first_cause, 0, Some(false));
duplicate_test!(actix_duplicate_panic_preserves_first_cause, 1, Some(false));
duplicate_test!(local_duplicate_panic_preserves_first_cause, 2, Some(false));
duplicate_test!(thread_duplicate_recursive_reentry, 0, Some(true));
duplicate_test!(actix_duplicate_recursive_reentry, 1, Some(true));
duplicate_test!(local_duplicate_recursive_reentry, 2, Some(true));
duplicate_test!(thread_resume_waker_reenters_signal_and_deadline, 0, None);
duplicate_test!(actix_resume_waker_reenters_signal_and_deadline, 1, None);
duplicate_test!(local_resume_waker_reenters_signal_and_deadline, 2, None);
