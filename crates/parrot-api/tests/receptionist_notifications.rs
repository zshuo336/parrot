use parrot_api::{
    errors::ActorError,
    receptionist::{ReceptionistKey, RegistrationScope},
    types::ActorResult,
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        Arc, Weak,
    },
    task::{Context, Poll, Wake, Waker},
};

#[derive(Clone, Copy)]
enum Phase {
    Closed,
    Joined,
}

#[derive(Clone, Copy)]
enum Receipt {
    Ok,
    Error,
    Panic,
}

impl Receipt {
    fn matches(self, result: &ActorResult<()>) -> bool {
        match (self, result) {
            (Self::Ok, Ok(())) => true,
            (Self::Error, Err(ActorError::InternalError(message))) => {
                message == "Internal error: cleanup error"
            }
            (Self::Panic, Err(ActorError::Panic(message))) => message == "cleanup panic",
            _ => false,
        }
    }
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
        panic!("notification panic payload destructor");
    }
}

struct Probe {
    scope: Weak<RegistrationScope>,
    cleaned: Arc<AtomicUsize>,
    phase: Phase,
    receipt: Receipt,
    throwing: Throw,
    wakes: AtomicUsize,
    committed: AtomicBool,
    payloads_dropped: Arc<AtomicUsize>,
}

impl Wake for Probe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, SeqCst);
        let scope = self.scope.upgrade().unwrap();
        let fenced = matches!(scope.with_live(|| ()), Err(ActorError::Stopped));
        let committed = match self.phase {
            Phase::Closed => fenced,
            Phase::Joined => {
                let mut receipt = Box::pin(scope.join());
                let waker = futures::task::noop_waker();
                matches!(receipt.as_mut().poll(&mut Context::from_waker(&waker)), Poll::Ready(result) if self.receipt.matches(&result))
                    && fenced
                    && self.cleaned.load(SeqCst) == 4
            }
        };
        self.committed.store(committed, SeqCst);
        match self.throwing {
            Throw::Never => {}
            Throw::Wake => panic!("notification observer panic"),
            Throw::Payload => std::panic::panic_any(Payload(self.payloads_dropped.clone())),
        }
    }
}

type Waiter<'a> = Pin<Box<dyn Future<Output = ActorResult<()>> + 'a>>;

fn waiter(scope: &RegistrationScope, phase: Phase) -> Waiter<'_> {
    match phase {
        Phase::Closed => Box::pin(async {
            scope.closed().await;
            Ok(())
        }),
        Phase::Joined => Box::pin(scope.join()),
    }
}

// The old implementation can return a hostile panic payload. Keep that failure
// observable without running its destructor outside the boundary being tested.
fn contained(call: impl FnOnce()) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(()) => true,
        Err(payload) => {
            std::mem::forget(payload);
            false
        }
    }
}

fn bind(scope: &RegistrationScope, receipt: Receipt) -> Arc<AtomicUsize> {
    let cleaned = Arc::new(AtomicUsize::new(0));
    for id in 0..4 {
        let cleaned = cleaned.clone();
        scope
            .publish(
                (
                    id,
                    ReceptionistKey::new("edge/notifications").unwrap(),
                    format!("/actor/{id}"),
                ),
                move || {
                    cleaned.fetch_add(1, SeqCst);
                    match (id, receipt) {
                        (0, Receipt::Error) => {
                            Err(ActorError::InternalError("cleanup error".into()))
                        }
                        (0, Receipt::Panic) => panic!("cleanup panic"),
                        _ => Ok(()),
                    }
                },
                || (),
            )
            .unwrap();
    }
    cleaned
}

fn cohort(phase: Phase, retire: bool, throwing: Throw, receipt: Receipt) {
    let scope = Arc::new(RegistrationScope::default());
    let cleaned = bind(&scope, receipt);
    let mut waiting = Vec::new();
    let mut probes = Vec::new();
    let mut wakers = Vec::new();
    for _ in 0..40 {
        let probe = Arc::new(Probe {
            scope: Arc::downgrade(&scope),
            cleaned: cleaned.clone(),
            phase,
            receipt,
            throwing,
            wakes: AtomicUsize::new(0),
            committed: AtomicBool::new(false),
            payloads_dropped: Arc::new(AtomicUsize::new(0)),
        });
        let waker = Waker::from(probe.clone());
        let mut future = waiter(&scope, phase);
        assert!(future
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        waiting.push(future);
        probes.push(probe);
        wakers.push(waker);
    }
    assert!(
        contained(|| {
            if retire {
                scope.retire();
            } else {
                scope.close();
            }
        }),
        "notification callback escaped scope closure/retirement"
    );
    assert_eq!(cleaned.load(SeqCst), if retire { 4 } else { 0 });
    for ((future, probe), waker) in waiting.iter_mut().zip(&probes).zip(&wakers) {
        assert_eq!(
            probe.wakes.load(SeqCst),
            1,
            "a registered observer lost its notification"
        );
        assert!(
            probe.committed.load(SeqCst),
            "notification preceded the committed state"
        );
        assert_eq!(
            probe.payloads_dropped.load(SeqCst),
            usize::from(matches!(throwing, Throw::Payload))
        );
        let Poll::Ready(result) = future.as_mut().poll(&mut Context::from_waker(waker)) else {
            panic!("the notified original waiter stayed pending");
        };
        assert!(match phase {
            Phase::Closed => result.is_ok(),
            Phase::Joined => receipt.matches(&result),
        });
    }
    scope.retire();
    scope.close();
    assert_eq!(
        cleaned.load(SeqCst),
        4,
        "retirement must remain exactly once"
    );
    assert!(receipt.matches(&futures::executor::block_on(scope.join())));
    for probe in probes {
        assert_eq!(
            probe.wakes.load(SeqCst),
            1,
            "completed observer was notified again"
        );
    }
}

#[derive(Default)]
struct DropObserved {
    wakes: AtomicUsize,
    drops: AtomicUsize,
    unlocked: AtomicBool,
}

struct DropProbe {
    scope: Weak<RegistrationScope>,
    observed: Arc<DropObserved>,
}

impl Wake for DropProbe {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.observed.wakes.fetch_add(1, SeqCst);
    }
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        let scope = self.scope.upgrade().unwrap();
        let result = scope.with_live(|| ());
        self.observed.unlocked.store(
            result.is_ok() || matches!(result, Err(ActorError::Stopped)),
            SeqCst,
        );
        self.observed.drops.fetch_add(1, SeqCst);
        panic!("observer destructor panic");
    }
}

fn disposal(phase: Phase, complete: bool, replace: bool) {
    let scope = Arc::new(RegistrationScope::default());
    let cleaned = bind(&scope, Receipt::Ok);
    let observed = Arc::new(DropObserved::default());
    let waker = Waker::from(Arc::new(DropProbe {
        scope: Arc::downgrade(&scope),
        observed: observed.clone(),
    }));
    let mut future = waiter(&scope, phase);
    assert!(future
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    drop(waker);
    let replacement = Arc::new(Probe {
        scope: Arc::downgrade(&scope),
        cleaned: cleaned.clone(),
        phase,
        receipt: Receipt::Ok,
        throwing: Throw::Never,
        wakes: AtomicUsize::new(0),
        committed: AtomicBool::new(false),
        payloads_dropped: Arc::new(AtomicUsize::new(0)),
    });
    let replacement_waker = Waker::from(replacement.clone());
    if replace {
        assert!(
            contained(|| assert!(future
                .as_mut()
                .poll(&mut Context::from_waker(&replacement_waker))
                .is_pending())),
            "replacing a waker let its destructor escape polling"
        );
        assert_eq!(observed.drops.load(SeqCst), 1);
        assert_eq!(observed.wakes.load(SeqCst), 0);
    }
    if complete {
        assert!(
            contained(|| match phase {
                Phase::Closed => scope.close(),
                Phase::Joined => scope.retire(),
            }),
            "waker disposal escaped notification"
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
            "waker disposal escaped completion"
        );
        if replace {
            assert!(replacement.committed.load(SeqCst));
        }
    }
    assert!(
        contained(|| drop(future)),
        "waker disposal escaped cancellation"
    );
    assert_eq!(observed.drops.load(SeqCst), 1);
    assert!(observed.unlocked.load(SeqCst));
    scope.retire();
    assert_eq!(cleaned.load(SeqCst), 4);
    futures::executor::block_on(scope.join()).unwrap();
}

fn isolated(name: &str, body: impl FnOnce()) {
    const CHILD: &str = "PARROT_REGISTRATION_NOTIFY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return body();
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, "1")
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "registration notification child failed: {status}"
            );
            return;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("registration notification child timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

macro_rules! cases {
    ($($name:ident => $body:expr;)*) => { $(
        #[test] fn $name() { isolated(stringify!($name), || $body); }
    )* };
}

cases! {
    close_closed_normal => cohort(Phase::Closed, false, Throw::Never, Receipt::Ok);
    close_closed_wake_panic => cohort(Phase::Closed, false, Throw::Wake, Receipt::Ok);
    close_closed_payload_panic => cohort(Phase::Closed, false, Throw::Payload, Receipt::Ok);
    retire_closed_normal => cohort(Phase::Closed, true, Throw::Never, Receipt::Ok);
    retire_closed_wake_panic => cohort(Phase::Closed, true, Throw::Wake, Receipt::Ok);
    retire_closed_payload_panic => cohort(Phase::Closed, true, Throw::Payload, Receipt::Ok);
    join_ok_normal => cohort(Phase::Joined, true, Throw::Never, Receipt::Ok);
    join_ok_wake_panic => cohort(Phase::Joined, true, Throw::Wake, Receipt::Ok);
    join_ok_payload_panic => cohort(Phase::Joined, true, Throw::Payload, Receipt::Ok);
    join_error_normal => cohort(Phase::Joined, true, Throw::Never, Receipt::Error);
    join_error_wake_panic => cohort(Phase::Joined, true, Throw::Wake, Receipt::Error);
    join_error_payload_panic => cohort(Phase::Joined, true, Throw::Payload, Receipt::Error);
    join_panic_normal => cohort(Phase::Joined, true, Throw::Never, Receipt::Panic);
    join_panic_wake_panic => cohort(Phase::Joined, true, Throw::Wake, Receipt::Panic);
    join_panic_payload_panic => cohort(Phase::Joined, true, Throw::Payload, Receipt::Panic);
    cancelled_closed_disposes_waker_safely => disposal(Phase::Closed, false, false);
    cancelled_join_disposes_waker_safely => disposal(Phase::Joined, false, false);
    completed_closed_disposes_waker_safely => disposal(Phase::Closed, true, false);
    completed_join_disposes_waker_safely => disposal(Phase::Joined, true, false);
    replacement_closed_disposes_waker_safely => disposal(Phase::Closed, true, true);
    replacement_join_disposes_waker_safely => disposal(Phase::Joined, true, true);
}
