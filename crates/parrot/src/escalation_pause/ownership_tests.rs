use super::*;
use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Weak,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
enum Disposal {
    Clean,
    Panic,
    Recursive,
}
#[derive(Default)]
struct Seen {
    drops: AtomicUsize,
    diagnostics: AtomicUsize,
    reentered: AtomicUsize,
}
#[derive(Default)]
struct WakeCount(AtomicUsize);
impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct Recursive;
impl Drop for Recursive {
    fn drop(&mut self) {
        panic!("pause panic payload destructor failed");
    }
}
struct UserError {
    id: u64,
    seen: Arc<Seen>,
    disposal: Disposal,
    reenter: Option<Weak<EscalationPause>>,
}
impl fmt::Debug for UserError {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.seen.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("pause invoked user Debug");
    }
}
impl fmt::Display for UserError {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.seen.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("pause invoked user Display");
    }
}
impl std::error::Error for UserError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.seen.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("pause invoked user source");
    }
}
impl Drop for UserError {
    fn drop(&mut self) {
        self.seen.drops.fetch_add(1, Ordering::SeqCst);
        if let Some(pause) = self.reenter.as_ref().and_then(Weak::upgrade) {
            assert!(pause.pending().is_some());
            self.seen.reentered.fetch_add(1, Ordering::SeqCst);
        }
        match self.disposal {
            Disposal::Clean => {}
            Disposal::Panic => panic!("pause error destructor failed"),
            Disposal::Recursive => std::panic::panic_any(Recursive),
        }
    }
}
fn error(
    seen: &Arc<Seen>,
    disposal: Disposal,
    reenter: Option<&Arc<EscalationPause>>,
) -> Arc<ActorError> {
    Arc::new(ActorError::Other(anyhow::Error::new(UserError {
        id: 97,
        seen: seen.clone(),
        disposal,
        reenter: reenter.map(Arc::downgrade),
    })))
}
fn no_unwind(action: impl FnOnce(), diagnosis: &str) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(action)) {
        // Preserve the original failure without recursively destroying its payload.
        std::mem::forget(payload);
        panic!("{diagnosis}");
    }
}
fn disposed(seen: &Seen) {
    assert_eq!(seen.drops.load(Ordering::SeqCst), 1);
    assert_eq!(seen.diagnostics.load(Ordering::SeqCst), 0);
}
fn retained(disposal: Disposal) {
    let pause = EscalationPause::default();
    let seen = Arc::new(Seen::default());
    assert!(pause.begin(error(&seen, disposal, None)));
    assert_eq!(seen.drops.load(Ordering::SeqCst), 0);
    no_unwind(|| drop(pause), "retained pause error escaped destruction");
    disposed(&seen);
}
#[test]
fn retained_error_panic_is_isolated_without_executor() {
    retained(Disposal::Panic);
}
#[test]
fn retained_recursive_panic_is_isolated_without_executor() {
    retained(Disposal::Recursive);
}
fn duplicate(disposal: Disposal) {
    let pause = EscalationPause::default();
    let first = Arc::new(ActorError::Stopped);
    assert!(pause.begin(first.clone()));
    let seen = Arc::new(Seen::default());
    no_unwind(
        || assert!(!pause.begin(error(&seen, disposal, None))),
        "rejected pause error escaped begin",
    );
    assert!(Arc::ptr_eq(&pause.pending().unwrap(), &first));
    pause.resume(&first);
    assert!(pause.pending().is_none());
    disposed(&seen);
}
#[test]
fn duplicate_error_panic_is_isolated_without_executor() {
    duplicate(Disposal::Panic);
}
#[test]
fn duplicate_recursive_panic_is_isolated_without_executor() {
    duplicate(Disposal::Recursive);
}
#[test]
fn diagnostics_do_not_execute_user_error_callbacks() {
    let pause = EscalationPause::default();
    let seen = Arc::new(Seen::default());
    assert!(pause.begin(error(&seen, Disposal::Clean, None)));
    no_unwind(
        || assert!(format!("{pause:?}").contains("EscalationPause")),
        "pause Debug invoked user error diagnostics",
    );
    drop(pause);
    disposed(&seen);
}
#[test]
fn first_cause_keeps_exact_identity_and_external_ownership() {
    let pause = EscalationPause::default();
    let seen = Arc::new(Seen::default());
    let original = error(&seen, Disposal::Clean, None);
    assert!(pause.begin(original.clone()));
    assert!(!pause.begin(Arc::new(ActorError::Stopped)));
    let pending = pause.pending().unwrap();
    assert!(Arc::ptr_eq(&pending, &original));
    let ActorError::Other(business) = pending.as_ref() else {
        panic!("business error category was replaced");
    };
    assert_eq!(business.downcast_ref::<UserError>().unwrap().id, 97);
    drop(pending);
    drop(pause);
    assert_eq!(seen.drops.load(Ordering::SeqCst), 0);
    drop(original);
    disposed(&seen);
}
#[test]
fn only_matching_resume_polls_original_future() {
    let pause = EscalationPause::default();
    let first = Arc::new(ActorError::Stopped);
    let wrong = Arc::new(ActorError::Stopped);
    assert!(pause.begin(first.clone()));
    let polls = AtomicUsize::new(0);
    let mut work = Box::pin(pause.guard(std::future::poll_fn(|_| {
        polls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(97)
    })));
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut context = Context::from_waker(&waker);
    assert!(work.as_mut().poll(&mut context).is_pending());
    pause.resume(&wrong);
    assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
    assert!(work.as_mut().poll(&mut context).is_pending());
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    pause.resume(&first);
    assert!(
        wakes.0.load(Ordering::SeqCst) > 0,
        "matching Resume lost its wakeup"
    );
    assert_eq!(work.as_mut().poll(&mut context), Poll::Ready(97));
    assert_eq!(polls.load(Ordering::SeqCst), 1);
}
#[test]
fn escalation_preserves_an_already_polled_future_until_matching_resume() {
    struct StageDrop<'a>(&'a AtomicUsize);
    impl Drop for StageDrop<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let pause = EscalationPause::default();
    let polls = AtomicUsize::new(0);
    let drops = AtomicUsize::new(0);
    let witness = StageDrop(&drops);
    let stage = async {
        let _witness = witness;
        std::future::poll_fn(|_| {
            if polls.fetch_add(1, Ordering::SeqCst) == 0 {
                Poll::Pending
            } else {
                Poll::Ready(97)
            }
        })
        .await
    };
    let mut work = Box::pin(pause.guard(stage));
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut context = Context::from_waker(&waker);
    assert!(work.as_mut().poll(&mut context).is_pending());
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    let cause = Arc::new(ActorError::Stopped);
    assert!(pause.begin(cause.clone()));
    assert!(wakes.0.load(Ordering::SeqCst) > 0, "begin lost its wakeup");
    assert!(work.as_mut().poll(&mut context).is_pending());
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let before_resume = wakes.0.load(Ordering::SeqCst);
    pause.resume(&cause);
    assert!(wakes.0.load(Ordering::SeqCst) > before_resume);
    assert_eq!(work.as_mut().poll(&mut context), Poll::Ready(97));
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
#[test]
fn stale_resume_cannot_clear_next_cause() {
    let pause = EscalationPause::default();
    let first = Arc::new(ActorError::Stopped);
    let second = Arc::new(ActorError::Stopped);
    assert!(pause.begin(first.clone()));
    pause.resume(&first);
    assert!(pause.begin(second.clone()));
    pause.resume(&first);
    assert!(Arc::ptr_eq(&pause.pending().unwrap(), &second));
    pause.resume(&second);
    assert!(pause.pending().is_none());
}
#[test]
fn duplicate_disposal_can_reenter_the_actual_pause() {
    in_child("PARROT_PAUSE_REENTRY_CHILD", || {
        let pause = Arc::new(EscalationPause::default());
        assert!(pause.begin(Arc::new(ActorError::Stopped)));
        let seen = Arc::new(Seen::default());
        assert!(!pause.begin(error(&seen, Disposal::Clean, Some(&pause))));
        assert_eq!(seen.reentered.load(Ordering::SeqCst), 1);
        disposed(&seen);
    });
}
#[test]
fn notification_waker_can_reenter_begin_and_resume() {
    struct Reenter {
        pause: Weak<EscalationPause>,
        calls: AtomicUsize,
    }
    impl Wake for Reenter {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            let pause = self.pause.upgrade().unwrap();
            drop(pause.pending());
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }
    in_child("PARROT_PAUSE_WAKE_REENTRY_CHILD", || {
        let pause = Arc::new(EscalationPause::default());
        let wake = Arc::new(Reenter {
            pause: Arc::downgrade(&pause),
            calls: AtomicUsize::new(0),
        });
        let waker = Waker::from(wake.clone());
        let mut context = Context::from_waker(&waker);
        let mut work = Box::pin(pause.guard(std::future::pending::<()>()));
        assert!(work.as_mut().poll(&mut context).is_pending());
        let cause = Arc::new(ActorError::Stopped);
        assert!(pause.begin(cause.clone()));
        assert_eq!(wake.calls.load(Ordering::SeqCst), 1);
        assert!(work.as_mut().poll(&mut context).is_pending());
        pause.resume(&cause);
        assert_eq!(wake.calls.load(Ordering::SeqCst), 2);
        assert!(pause.pending().is_none());
    });
}
fn in_child(flag: &str, action: impl FnOnce()) {
    if std::env::var_os(flag).is_some() {
        action();
        return;
    }
    let thread = std::thread::current();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", thread.name().unwrap(), "--nocapture"])
        .env(flag, "1")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "pause reentry child failed: {status}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("pause reentry deadlocked its cause lock");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn runtime_shutdown(duplicate: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let seen = Arc::new(Seen::default());
    no_unwind(
        || {
            let _entered = runtime.enter();
            let pause = EscalationPause::default();
            if duplicate {
                assert!(pause.begin(Arc::new(ActorError::Stopped)));
            }
            assert_eq!(
                pause.begin(error(&seen, Disposal::Recursive, None)),
                !duplicate
            );
            drop(pause);
            assert_eq!(seen.drops.load(Ordering::SeqCst), 0);
        },
        "pause error escaped unpolled runtime ownership",
    );
    no_unwind(|| drop(runtime), "pause error escaped runtime shutdown");
    disposed(&seen);
}
#[test]
fn shutdown_retires_unpolled_retained_error() {
    runtime_shutdown(false);
}
#[test]
fn shutdown_retires_unpolled_duplicate_error() {
    runtime_shutdown(true);
}
