use super::*;
use std::{
    sync::{
        Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
    task::{Wake, Waker},
};

#[derive(Clone, Copy)]
enum Phase {
    Cancelled,
    Finished,
    Schedule,
}

#[derive(Clone, Copy)]
enum Throw {
    Never,
    Wake,
    Payload,
}

struct Payload(Arc<AtomicUsize>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fetch_add(1, SeqCst);
        panic!("context notification payload destructor");
    }
}

struct Probe {
    owner: Weak<ContextTasks>,
    token: Weak<ScheduleCancellation>,
    phase: Phase,
    throwing: Throw,
    cleaned: Arc<AtomicUsize>,
    expected_cleaned: usize,
    wakes: AtomicUsize,
    committed: AtomicBool,
    unlocked: AtomicBool,
    payloads: Arc<AtomicUsize>,
}
impl Wake for Probe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, SeqCst);
        let owner = self.owner.upgrade().unwrap();
        let (unlocked, committed) = match owner.state.status.try_lock() {
            Ok(status) => (
                true,
                match self.phase {
                    Phase::Cancelled => status.closed,
                    Phase::Finished => {
                        status.active == 0 && self.cleaned.load(SeqCst) == self.expected_cleaned
                    }
                    Phase::Schedule => self.token.upgrade().unwrap().cancelled.load(SeqCst),
                },
            ),
            Err(_) => (false, false),
        };
        // A later valid wake must not erase an earlier contract violation.
        self.unlocked.fetch_and(unlocked, SeqCst);
        self.committed.fetch_and(committed, SeqCst);
        if unlocked {
            owner.cancel_schedule(Uuid::nil());
        }
        match self.throwing {
            Throw::Never => {}
            Throw::Wake => panic!("context notification observer panic"),
            Throw::Payload => std::panic::panic_any(Payload(self.payloads.clone())),
        }
    }
}

fn probe(
    owner: &Arc<ContextTasks>,
    token: &Arc<ScheduleCancellation>,
    phase: Phase,
    throwing: Throw,
    cleaned: &Arc<AtomicUsize>,
    expected_cleaned: usize,
) -> Arc<Probe> {
    Arc::new(Probe {
        owner: Arc::downgrade(owner),
        token: Arc::downgrade(token),
        phase,
        throwing,
        cleaned: cleaned.clone(),
        expected_cleaned,
        wakes: AtomicUsize::new(0),
        committed: AtomicBool::new(true),
        unlocked: AtomicBool::new(true),
        payloads: Arc::new(AtomicUsize::new(0)),
    })
}

fn check(probe: &Probe, throwing: Throw) {
    assert_eq!(
        probe.wakes.load(SeqCst),
        1,
        "original observer lost its notification"
    );
    assert!(
        probe.unlocked.load(SeqCst),
        "notification ran under the generation lock"
    );
    assert!(
        probe.committed.load(SeqCst),
        "notification preceded its committed state"
    );
    assert_eq!(
        probe.payloads.load(SeqCst),
        usize::from(matches!(throwing, Throw::Payload))
    );
}

// Preserve the original failure without dropping a hostile payload in the test driver.
fn contained(call: impl FnOnce()) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(()) => true,
        Err(payload) => {
            std::mem::forget(payload);
            false
        }
    }
}

struct TaskInput {
    cleaned: Arc<AtomicUsize>,
    panic_drop: bool,
}
impl Future for TaskInput {
    type Output = ActorResult<()>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}
impl Drop for TaskInput {
    fn drop(&mut self) {
        self.cleaned.fetch_add(1, SeqCst);
        assert!(!self.panic_drop, "actual task cleanup panic");
    }
}

type Waiter<'a> = Pin<Box<dyn Future<Output = ActorResult<()>> + 'a>>;
fn waiter<'a>(
    owner: &'a ContextTasks,
    token: &'a ScheduleCancellation,
    phase: Phase,
) -> Waiter<'a> {
    match phase {
        Phase::Cancelled => Box::pin(async {
            owner.state.cancelled().await;
            Ok(())
        }),
        Phase::Finished => Box::pin(owner.join()),
        Phase::Schedule => Box::pin(async {
            token.wait().await;
            Ok(())
        }),
    }
}

fn cohort(phase: Phase, throwing: Throw, close: bool, panic_drop: bool) {
    let owner = ContextTasks::new(1);
    let token = Arc::new(ScheduleCancellation::default());
    let id = Uuid::new_v4();
    owner.insert_schedule(id, token.clone()).unwrap();
    let cleaned = Arc::new(AtomicUsize::new(0));
    let mut task = Some(
        owner
            .user_task(TaskInput {
                cleaned: cleaned.clone(),
                panic_drop,
            })
            .unwrap(),
    );
    let mut waiters = Vec::new();
    let mut probes = Vec::new();
    let mut wakers = Vec::new();
    for _ in 0..40 {
        let observer = probe(&owner, &token, phase, throwing, &cleaned, 1);
        let waker = Waker::from(observer.clone());
        let mut future = waiter(&owner, &token, phase);
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        waiters.push(future);
        probes.push(observer);
        wakers.push(waker);
    }
    let completed = contained(|| match phase {
        Phase::Finished => drop(task.take()),
        Phase::Cancelled => owner.close(),
        Phase::Schedule if close => owner.close(),
        Phase::Schedule => owner.cancel_schedule(id),
    });
    let poisoned = owner.state.status.is_poisoned();
    // The old implementation may poison this mutex; keep teardown from masking the assertion.
    owner.state.status.clear_poison();
    assert!(completed, "observer escaped actual lifecycle notification");
    assert!(!poisoned, "observer poisoned the generation lock");
    for ((future, observer), waker) in waiters.iter_mut().zip(&probes).zip(&wakers) {
        check(observer, throwing);
        let Poll::Ready(result) = future.as_mut().poll(&mut Context::from_waker(waker)) else {
            panic!("notified original future stayed pending");
        };
        if panic_drop {
            assert!(
                matches!(result, Err(ActorError::Panic(message)) if message == "actual task cleanup panic")
            );
        } else {
            result.unwrap();
        }
    }
    drop(waiters);
    drop(task.take());
    owner.close();
    assert_eq!(cleaned.load(SeqCst), 1);
    assert_eq!(owner.state.status.lock().unwrap().active, 0);
    let result = futures::executor::block_on(owner.join());
    assert_eq!(result.is_err(), panic_drop);
    for observer in probes {
        assert_eq!(observer.wakes.load(SeqCst), 1);
    }
}

#[derive(Default)]
struct Disposal {
    wakes: AtomicUsize,
    drops: AtomicUsize,
    unlocked: AtomicBool,
}
struct DropProbe(Weak<ContextTasks>, Arc<Disposal>);
impl Wake for DropProbe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.1.wakes.fetch_add(1, SeqCst);
    }
}
impl Drop for DropProbe {
    fn drop(&mut self) {
        let owner = self.0.upgrade().unwrap();
        self.1
            .unlocked
            .store(owner.state.status.try_lock().is_ok(), SeqCst);
        self.1.drops.fetch_add(1, SeqCst);
        panic!("context observer destructor panic");
    }
}
fn disposal(phase: Phase, complete: bool, replace: bool) {
    let owner = ContextTasks::new(1);
    let token = Arc::new(ScheduleCancellation::default());
    let cleaned = Arc::new(AtomicUsize::new(0));
    let mut task = Some(
        owner
            .user_task(TaskInput {
                cleaned: cleaned.clone(),
                panic_drop: false,
            })
            .unwrap(),
    );
    let observed = Arc::new(Disposal::default());
    let waker = Waker::from(Arc::new(DropProbe(
        Arc::downgrade(&owner),
        observed.clone(),
    )));
    let mut future = waiter(&owner, &token, phase);
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);
    let replacement = probe(&owner, &token, phase, Throw::Never, &cleaned, 1);
    let replacement_waker = Waker::from(replacement.clone());
    if replace {
        assert!(
            contained(|| assert!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(&replacement_waker))
                    .is_pending()
            )),
            "old Waker destructor escaped replacement"
        );
        assert_eq!(observed.drops.load(SeqCst), 1);
        assert_eq!(observed.wakes.load(SeqCst), 0);
    }
    if complete {
        assert!(
            contained(|| match phase {
                Phase::Cancelled => owner.close(),
                Phase::Finished => drop(task.take()),
                Phase::Schedule => token.cancel(),
            }),
            "old Waker destructor escaped notification"
        );
        assert_eq!(
            if replace {
                replacement.wakes.load(SeqCst)
            } else {
                observed.wakes.load(SeqCst)
            },
            1
        );
        assert!(
            contained(|| assert!(matches!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(&replacement_waker)),
                Poll::Ready(Ok(()))
            ))),
            "old Waker destructor escaped completion"
        );
        if replace {
            check(&replacement, Throw::Never);
        }
    }
    assert!(
        contained(|| drop(future)),
        "old Waker destructor escaped cancellation"
    );
    assert_eq!(observed.drops.load(SeqCst), 1);
    assert!(observed.unlocked.load(SeqCst));
    drop(task.take());
    owner.close();
    futures::executor::block_on(owner.join()).unwrap();
    assert_eq!(cleaned.load(SeqCst), 1);
}

fn gateway(throwing: Throw) {
    use parrot_api::receptionist::{
        ReceptionistEvent, ReceptionistGateway, ReceptionistKey, RegistrationScope,
    };
    use parrot_config::receptionist::RegistrationLimits;
    use parrot_remote::{acl::AclRules, receptionist::ReceptionistFacade};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let facade = ReceptionistFacade::with_registration_limits(
        "context-notify",
        RegistrationLimits {
            max_registrations: 4,
            ..Default::default()
        },
    )
    .with_acl(AclRules::permissive());
    let key = ReceptionistKey::new("edge/context-notify").unwrap();
    let owner = ContextTasks::new(40);
    let token = Arc::new(ScheduleCancellation::default());
    let scope = owner.registration_scope(runtime.handle());
    let mut events = runtime.block_on(facade.subscribe(key.clone())).unwrap();
    for id in 0..4 {
        runtime
            .block_on(facade.register_scoped(key.clone(), format!("/task/{id}"), scope.clone()))
            .unwrap();
        assert_eq!(
            events.try_recv().unwrap(),
            ReceptionistEvent::Registered {
                key: key.clone(),
                remote_path: format!("/task/{id}")
            }
        );
    }
    assert!(
        runtime
            .block_on(facade.register(key.clone(), "/full".into()))
            .is_err()
    );
    let cleaned = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    let mut probes = Vec::new();
    let mut wakers = Vec::new();
    for _ in 0..40 {
        let observer = probe(&owner, &token, Phase::Cancelled, throwing, &cleaned, 40);
        let waker = Waker::from(observer.clone());
        let mut task = Box::pin(
            owner
                .user_task(TaskInput {
                    cleaned: cleaned.clone(),
                    panic_drop: false,
                })
                .unwrap(),
        );
        assert!(
            task.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        tasks.push(task);
        probes.push(observer);
        wakers.push(waker);
    }
    assert!(
        contained(|| owner.close()),
        "observer prevented actual generation closure"
    );
    assert!(
        matches!(scope.with_live(|| ()), Err(ActorError::Stopped)),
        "task notification interrupted service fencing"
    );
    for ((task, observer), waker) in tasks.iter_mut().zip(&probes).zip(&wakers) {
        check(observer, throwing);
        assert!(matches!(
            task.as_mut().poll(&mut Context::from_waker(waker)),
            Poll::Ready(Ok(()))
        ));
    }
    drop(tasks);
    runtime.block_on(owner.join()).unwrap();
    assert_eq!(cleaned.load(SeqCst), 40);
    let mut withdrawn = Vec::new();
    while let Ok(event) = events.try_recv() {
        let ReceptionistEvent::Unregistered {
            key: actual,
            remote_path,
        } = event
        else {
            panic!("unexpected service event");
        };
        assert_eq!(actual, key);
        withdrawn.push(remote_path);
    }
    withdrawn.sort();
    assert_eq!(
        withdrawn,
        (0..4).map(|id| format!("/task/{id}")).collect::<Vec<_>>()
    );
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(
        runtime
            .block_on(facade.subscribe(key.clone()))
            .unwrap()
            .len(),
        0
    );
    let replacement = Arc::new(RegistrationScope::default());
    for id in 0..4 {
        runtime
            .block_on(facade.register_scoped(
                key.clone(),
                format!("/task/{id}"),
                replacement.clone(),
            ))
            .unwrap();
    }
    replacement.retire();
    runtime.block_on(replacement.join()).unwrap();
    assert_eq!(runtime.block_on(facade.subscribe(key)).unwrap().len(), 0);
    assert_eq!(owner.state.status.lock().unwrap().active, 0);
    for observer in probes {
        assert_eq!(observer.wakes.load(SeqCst), 1);
    }
}

#[derive(Default, Debug)]
struct ScheduleCounts {
    deliveries: AtomicUsize,
    messages: AtomicUsize,
    targets: AtomicUsize,
}
#[derive(Clone)]
struct ScheduleMessage(Arc<MessageOwner>);
struct MessageOwner(Arc<ScheduleCounts>);
impl Drop for MessageOwner {
    fn drop(&mut self) {
        self.0.messages.fetch_add(1, SeqCst);
    }
}
#[derive(Clone, Debug)]
struct Target(Arc<TargetOwner>);
#[derive(Debug)]
struct TargetOwner(Arc<ScheduleCounts>);
impl Drop for TargetOwner {
    fn drop(&mut self) {
        self.0.targets.fetch_add(1, SeqCst);
    }
}
impl parrot_api::address::ActorRef for Target {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Err(ActorError::Stopped) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            let message = message
                .downcast::<ScheduleMessage>()
                .expect("wrong scheduled payload");
            assert!(Arc::ptr_eq(&message.0.0, &self.0.0));
            self.0.0.deliveries.fetch_add(1, SeqCst);
            Ok(())
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/context-notify/target".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn scheduled_task(
    owner: &ContextTasks,
    periodic: bool,
    counts: &Arc<ScheduleCounts>,
) -> (Uuid, OwnedTask) {
    let target = Box::new(Target(Arc::new(TargetOwner(counts.clone()))));
    let message = ScheduleMessage(Arc::new(MessageOwner(counts.clone())));
    if periodic {
        owner
            .periodic_with_id(
                target,
                CloneableMessage::from_cloneable(message),
                Duration::from_secs(10),
                Duration::from_secs(5),
            )
            .unwrap()
    } else {
        owner
            .once_with_id(target, Box::new(message), Duration::from_secs(10))
            .unwrap()
    }
}

fn scheduled(periodic: bool, close: bool, throwing: Throw) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::pause();
        let owner = ContextTasks::new(1);
        let counts = Arc::new(ScheduleCounts::default());
        let (id, task) = scheduled_task(&owner, periodic, &counts);
        let token = owner
            .state
            .status
            .lock()
            .unwrap()
            .scheduled
            .get(&id)
            .unwrap()
            .clone();
        let observer = probe(
            &owner,
            &token,
            Phase::Schedule,
            throwing,
            &Arc::new(AtomicUsize::new(0)),
            0,
        );
        let waker = Waker::from(observer.clone());
        let mut task = Box::pin(task);
        assert!(
            task.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        let notified = contained(|| {
            if close {
                owner.close();
            } else {
                owner.cancel_schedule(id);
            }
        });
        let poisoned = owner.state.status.is_poisoned();
        owner.state.status.clear_poison();
        assert!(
            notified,
            "observer interrupted actual scheduled cancellation"
        );
        assert!(
            !poisoned,
            "scheduled cancellation poisoned generation state"
        );
        let wakes = observer.wakes.load(SeqCst);
        assert!(
            wakes > 0,
            "cancelled schedule did not receive a notification"
        );
        assert!(
            observer.unlocked.load(SeqCst),
            "scheduled observer ran under generation lock"
        );
        assert!(observer.committed.load(SeqCst));
        assert_eq!(
            observer.payloads.load(SeqCst),
            wakes * usize::from(matches!(throwing, Throw::Payload))
        );
        assert!(matches!(
            task.as_mut().poll(&mut Context::from_waker(&waker)),
            Poll::Ready(Ok(()))
        ));
        drop(task);
        owner.join().await.unwrap();
        assert_eq!(counts.messages.load(SeqCst), 1);
        assert_eq!(counts.targets.load(SeqCst), 1);
        {
            let status = owner.state.status.lock().unwrap();
            assert_eq!(status.active, 0);
            assert!(status.scheduled.is_empty());
        }
        if !close {
            drop(
                owner
                    .user_task(async { Ok(()) })
                    .expect("schedule retained task capacity"),
            );
            owner.join().await.unwrap();
        }
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(
            counts.deliveries.load(SeqCst),
            0,
            "cancelled schedule delivered after its deadline"
        );
        assert_eq!(observer.wakes.load(SeqCst), wakes);
        owner.close();
    });
}

fn actual_delivery(periodic: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::pause();
        let owner = ContextTasks::new(1);
        let counts = Arc::new(ScheduleCounts::default());
        let (id, task) = scheduled_task(&owner, periodic, &counts);
        let mut task = Box::pin(task);
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        assert!(task.as_mut().poll(&mut context).is_pending());
        assert_eq!(counts.deliveries.load(SeqCst), 0);
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(task.as_mut().poll(&mut context).is_pending());
        assert_eq!(counts.deliveries.load(SeqCst), 0);
        tokio::time::advance(Duration::from_secs(1)).await;
        // Advancing the clock does not promise that the timer driver has run.
        let polled = tokio::time::timeout(
            Duration::from_secs(1),
            std::future::poll_fn(|cx| {
                let polled = task.as_mut().poll(cx);
                if polled.is_ready() || counts.deliveries.load(SeqCst) != 0 {
                    Poll::Ready(polled)
                } else {
                    Poll::Pending
                }
            }),
        )
        .await
        .expect("due schedule did not make progress");
        if periodic {
            assert!(polled.is_pending());
        } else {
            assert!(matches!(polled, Poll::Ready(Ok(()))));
        }
        assert_eq!(counts.deliveries.load(SeqCst), 1);
        if periodic {
            owner.cancel_schedule(id);
            assert!(matches!(
                task.as_mut().poll(&mut context),
                Poll::Ready(Ok(()))
            ));
        }
        drop(task);
        owner.join().await.unwrap();
        assert_eq!(counts.messages.load(SeqCst), 1);
        assert_eq!(counts.targets.load(SeqCst), 1);
        assert_eq!(owner.state.status.lock().unwrap().active, 0);
        assert!(owner.state.status.lock().unwrap().scheduled.is_empty());
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(counts.deliveries.load(SeqCst), 1);
        owner.close();
    });
}

fn isolated(name: &str, body: impl FnOnce()) {
    const CHILD: &str = "PARROT_CONTEXT_NOTIFY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        std::panic::set_hook(Box::new(|_| {}));
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            let message = crate::owned_payload::panic_message(payload);
            eprintln!("context notification assertion failed: {message}");
            panic!("context notification contract failed: {message}");
        }
        return;
    }
    let name = format!("context_tasks::notification_contracts::{name}");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env(CHILD, "1")
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "context notification child failed: {status}"
            );
            return;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("context notification child timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
macro_rules! cases {
    ($($name:ident => $body:expr;)*) => { $(#[test] fn $name() { isolated(stringify!($name), || $body); })* };
}
cases! {
    cancelled_normal => cohort(Phase::Cancelled, Throw::Never, true, false);
    cancelled_wake_panic => cohort(Phase::Cancelled, Throw::Wake, true, false);
    cancelled_payload_panic => cohort(Phase::Cancelled, Throw::Payload, true, false);
    finished_normal => cohort(Phase::Finished, Throw::Never, false, false);
    finished_wake_panic => cohort(Phase::Finished, Throw::Wake, false, false);
    finished_payload_panic => cohort(Phase::Finished, Throw::Payload, false, false);
    failed_cleanup_normal => cohort(Phase::Finished, Throw::Never, false, true);
    failed_cleanup_wake_panic => cohort(Phase::Finished, Throw::Wake, false, true);
    failed_cleanup_payload_panic => cohort(Phase::Finished, Throw::Payload, false, true);
    schedule_cancel_normal => cohort(Phase::Schedule, Throw::Never, false, false);
    schedule_cancel_wake_panic => cohort(Phase::Schedule, Throw::Wake, false, false);
    schedule_cancel_payload_panic => cohort(Phase::Schedule, Throw::Payload, false, false);
    schedule_close_normal => cohort(Phase::Schedule, Throw::Never, true, false);
    schedule_close_wake_panic => cohort(Phase::Schedule, Throw::Wake, true, false);
    schedule_close_payload_panic => cohort(Phase::Schedule, Throw::Payload, true, false);
    cancelled_waiter_drop => disposal(Phase::Cancelled, false, false);
    finished_waiter_drop => disposal(Phase::Finished, false, false);
    schedule_waiter_drop => disposal(Phase::Schedule, false, false);
    cancelled_waiter_completion => disposal(Phase::Cancelled, true, false);
    finished_waiter_completion => disposal(Phase::Finished, true, false);
    schedule_waiter_completion => disposal(Phase::Schedule, true, false);
    cancelled_waiter_replacement => disposal(Phase::Cancelled, true, true);
    finished_waiter_replacement => disposal(Phase::Finished, true, true);
    schedule_waiter_replacement => disposal(Phase::Schedule, true, true);
    actual_gateway_normal => gateway(Throw::Never);
    actual_gateway_wake_panic => gateway(Throw::Wake);
    actual_gateway_payload_panic => gateway(Throw::Payload);
    once_cancel_normal => scheduled(false, false, Throw::Never);
    once_cancel_wake_panic => scheduled(false, false, Throw::Wake);
    once_cancel_payload_panic => scheduled(false, false, Throw::Payload);
    periodic_cancel_normal => scheduled(true, false, Throw::Never);
    periodic_cancel_wake_panic => scheduled(true, false, Throw::Wake);
    periodic_cancel_payload_panic => scheduled(true, false, Throw::Payload);
    once_close_normal => scheduled(false, true, Throw::Never);
    once_close_wake_panic => scheduled(false, true, Throw::Wake);
    once_close_payload_panic => scheduled(false, true, Throw::Payload);
    periodic_close_normal => scheduled(true, true, Throw::Never);
    periodic_close_wake_panic => scheduled(true, true, Throw::Wake);
    periodic_close_payload_panic => scheduled(true, true, Throw::Payload);
    once_actual_delivery => actual_delivery(false);
    periodic_actual_delivery => actual_delivery(true);
}
