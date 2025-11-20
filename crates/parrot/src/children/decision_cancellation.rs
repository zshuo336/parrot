use super::*;
use futures::FutureExt;
use parrot_api::supervisor::{BasicDecisionFn, OneForAllStrategy, OneForOneStrategy};
use std::{fmt, panic::AssertUnwindSafe, sync::atomic::AtomicUsize};

#[derive(Default)]
struct Counts {
    drops: AtomicUsize,
    payloads: AtomicUsize,
    calls: AtomicUsize,
    diagnostics: AtomicUsize,
}
struct Recursive(Arc<Counts>);
impl Drop for Recursive {
    fn drop(&mut self) {
        self.0.payloads.fetch_add(1, Ordering::SeqCst);
        panic!("decision cancellation payload destructor");
    }
}
struct Capture {
    counts: Arc<Counts>,
    mode: u8,
}
impl fmt::Debug for Capture {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.counts.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("unexpected cancellation Debug");
    }
}
impl fmt::Display for Capture {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.counts.diagnostics.fetch_add(1, Ordering::SeqCst);
        panic!("unexpected cancellation Display");
    }
}
impl std::error::Error for Capture {}
impl Drop for Capture {
    fn drop(&mut self) {
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            0 => {}
            1 => panic!("decision cancellation capture destructor"),
            2 => std::panic::panic_any(Recursive(self.counts.clone())),
            _ => unreachable!(),
        }
    }
}
fn capture(counts: &Arc<Counts>, mode: u8) -> Capture {
    Capture {
        counts: counts.clone(),
        mode,
    }
}
fn caught(action: impl FnOnce()) -> Option<String> {
    std::panic::catch_unwind(AssertUnwindSafe(action))
        .err()
        .map(crate::owned_payload::panic_message)
}
fn strategy(counts: &Arc<Counts>, mode: u8, all: bool) -> SupervisorStrategyType {
    let capture = capture(counts, mode);
    let decider = BasicDecisionFn::new(move |_| {
        capture.counts.calls.fetch_add(1, Ordering::SeqCst);
        let _ = &capture;
        SupervisionDecision::Resume
    });
    if all {
        SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 1,
            within: Duration::from_secs(1),
            decider,
        })
    } else {
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 1,
            within: Duration::from_secs(1),
            decider,
        })
    }
}
async fn settled(counts: &Counts, mode: u8) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while counts.drops.load(Ordering::SeqCst) != 1
            || counts.payloads.load(Ordering::SeqCst) != usize::from(mode == 2)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("capture was abandoned or retired more than once");
    assert_eq!(counts.diagnostics.load(Ordering::SeqCst), 0);
}
fn disposal(result: ActorResult<()>, mode: u8) {
    if mode == 0 {
        result.unwrap();
    } else {
        let error = match result {
            Err(ActorError::InternalError(error)) => error,
            _ => panic!("missing actual snapshot disposal failure"),
        };
        assert!(error.contains("policy disposal failed"), "{error}");
        assert_eq!(
            error.contains("panic payload destructor panicked"),
            mode == 2,
            "{error}"
        );
    }
}
async fn cancelled_snapshot(mode: u8, all: bool) {
    snapshot_cancellation(mode, all, false).await;
}
async fn snapshot_cancellation(mode: u8, all: bool, inherited: bool) {
    let counts = Arc::new(Counts::default());
    let slots = Arc::new(Semaphore::new(0));
    let parent = inherited.then(|| ChildScope::new(Duration::from_secs(3), slots.clone()));
    let selected = strategy(&counts, mode, all);
    let selected = if let Some(parent) = &parent {
        parent.policy.set(Some(selected));
        None
    } else {
        Some(selected)
    };
    let lifetime = ChildLifetime::with_policy(
        parent.as_ref().map_or_else(Weak::new, Arc::downgrade),
        selected,
        slots.clone(),
    );
    let mut waiter = Box::pin(lifetime.failure_policy(
        Arc::new(ActorError::Stopped),
        slots.clone(),
        tokio::time::Instant::now() + Duration::from_secs(3),
    ));
    assert!(futures::poll!(&mut waiter).is_pending());
    if let Some(parent) = &parent {
        parent.policy.set(None);
    } else {
        lifetime.set_strategy(None);
    }
    assert_eq!(
        counts.drops.load(Ordering::SeqCst),
        0,
        "selected snapshot must stay owned"
    );
    let escaped = caught(|| drop(waiter));
    let premature = counts.drops.load(Ordering::SeqCst);
    slots.add_permits(1);
    let result = lifetime.dispose_policy().await;
    let repeated = lifetime.dispose_policy().await;
    settled(&counts, mode).await;
    assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
    assert!(
        escaped.is_none(),
        "snapshot cancellation escaped: {escaped:?}"
    );
    assert_eq!(
        premature, 0,
        "cancelled snapshot cleanup bypassed its permit"
    );
    disposal(result, mode);
    disposal(repeated, mode);
}
async fn closed_permit(mode: u8, all: bool) {
    let counts = Arc::new(Counts::default());
    let slots = Arc::new(Semaphore::new(0));
    let lifetime = ChildLifetime::with_policy(
        Weak::new(),
        Some(strategy(&counts, mode, all)),
        slots.clone(),
    );
    let mut waiter = Box::pin(lifetime.failure_policy(
        Arc::new(ActorError::Stopped),
        slots.clone(),
        tokio::time::Instant::now() + Duration::from_secs(3),
    ));
    assert!(futures::poll!(&mut waiter).is_pending());
    lifetime.set_strategy(None);
    slots.close();
    let outcome = AssertUnwindSafe(waiter).catch_unwind().await;
    let cleanup = lifetime.dispose_policy().await;
    let result: ActorResult<()> = match outcome {
        Ok(Err(error)) => Err(error),
        Ok(Ok(_)) => panic!("closed permit invoked a decision"),
        Err(payload) => panic!(
            "permit rejection escaped: {}",
            crate::owned_payload::panic_message(payload)
        ),
    };
    settled(&counts, mode).await;
    assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
    if mode == 0 {
        assert!(matches!(result, Err(ActorError::Stopped)));
    } else {
        disposal(result.map(|_| ()), mode);
    }
    disposal(cleanup, mode);
}
async fn unpolled_message(success: bool, mode: u8) {
    let counts = Arc::new(Counts::default());
    let (control, _completion) = crate::actix::ActorControl::new(Duration::from_secs(3));
    let result = if success {
        Ok(Box::new(capture(&counts, mode)) as parrot_api::types::BoxedMessage)
    } else {
        Err(ActorError::Other(anyhow::Error::new(capture(
            &counts, mode,
        ))))
    };
    let runtime = tokio::runtime::Handle::current();
    let escaped = caught(|| drop(control.supervise_message(result, false, &runtime)));
    settled(&counts, mode).await;
    assert!(
        escaped.is_none(),
        "unpolled message outcome escaped: {escaped:?}"
    );
    assert!(control.lifetime.decision_deadline().is_none());
    assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
    let failure = control.cleanup_failure.lock().unwrap().clone();
    if success && mode != 0 {
        assert!(matches!(
            failure.as_deref().map(|e| e.as_ref()),
            Some(ActorError::Panic(_))
        ));
    } else {
        assert!(failure.is_none());
    }
}
fn cancelled_message_after_decision(mode: u8) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = runtime.handle().clone();
    let counts = Arc::new(Counts::default());
    let (control, _completion) = crate::actix::ActorControl::new(Duration::from_secs(3));
    let mut waiter = Box::pin(control.supervise_message(
        Err(ActorError::Other(anyhow::Error::new(capture(
            &counts, mode,
        )))),
        false,
        &handle,
    ));
    runtime.block_on(async {
        assert!(futures::poll!(&mut waiter).is_pending());
        control.lifetime.settle_live_decision().await.unwrap();
    });
    drop(runtime);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 0);
    let escaped = caught(|| drop(waiter));
    assert!(
        escaped.is_none(),
        "completed decision cancellation escaped: {escaped:?}"
    );
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        counts.payloads.load(Ordering::SeqCst),
        usize::from(mode == 2)
    );
    assert_eq!(counts.diagnostics.load(Ordering::SeqCst), 0);
}
async fn selected_snapshot(all: bool) {
    let old = Arc::new(Counts::default());
    let new = Arc::new(Counts::default());
    let slots = Arc::new(Semaphore::new(0));
    let lifetime =
        ChildLifetime::with_policy(Weak::new(), Some(strategy(&old, 0, all)), slots.clone());
    let mut waiter = Box::pin(lifetime.failure_policy(
        Arc::new(ActorError::Stopped),
        slots.clone(),
        tokio::time::Instant::now() + Duration::from_secs(3),
    ));
    assert!(futures::poll!(&mut waiter).is_pending());
    lifetime.set_strategy(Some(strategy(&new, 0, all)));
    slots.add_permits(1);
    assert!(matches!(
        waiter.await.unwrap().unwrap().decision,
        SupervisionDecision::Resume
    ));
    assert_eq!(old.calls.load(Ordering::SeqCst), 1);
    assert_eq!(new.calls.load(Ordering::SeqCst), 0);
    lifetime.dispose_policy().await.unwrap();
    settled(&old, 0).await;
    settled(&new, 0).await;
}
fn runtime_cancelled_snapshot(mode: u8) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let counts = Arc::new(Counts::default());
    let slots = Arc::new(Semaphore::new(0));
    let lifetime = ChildLifetime::with_policy(
        Weak::new(),
        Some(strategy(&counts, mode, false)),
        slots.clone(),
    );
    let escaped = runtime.block_on(async {
        let mut waiter = Box::pin(lifetime.failure_policy(
            Arc::new(ActorError::Stopped),
            slots,
            tokio::time::Instant::now() + Duration::from_secs(3),
        ));
        assert!(futures::poll!(&mut waiter).is_pending());
        lifetime.set_strategy(None);
        let escaped = caught(|| drop(waiter));
        tokio::task::yield_now().await;
        escaped
    });
    drop(runtime);
    let restored = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    restored.block_on(async {
        let result = lifetime.dispose_policy().await;
        let repeated = lifetime.dispose_policy().await;
        settled(&counts, mode).await;
        assert!(
            escaped.is_none(),
            "runtime snapshot cancellation escaped: {escaped:?}"
        );
        disposal(result, mode);
        disposal(repeated, mode);
    });
    assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
}
async fn cancelled_running_snapshot(mode: u8, callback_panic: bool) {
    let counts = Arc::new(Counts::default());
    let token = capture(&counts, mode);
    let (started, entered) = tokio::sync::oneshot::channel();
    let started = Mutex::new(Some(started));
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            let (ready, changed) = &*self.0;
            *ready.lock().unwrap() = true;
            changed.notify_all();
        }
    }
    let release = Release(gate.clone());
    let blocked = gate.clone();
    let strategy = SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 1,
        within: Duration::from_secs(1),
        decider: BasicDecisionFn::new(move |_| {
            token.counts.calls.fetch_add(1, Ordering::SeqCst);
            let _ = &token;
            started.lock().unwrap().take().unwrap().send(()).unwrap();
            let (ready, changed) = &*blocked;
            let mut ready = ready.lock().unwrap();
            while !*ready {
                ready = changed.wait(ready).unwrap();
            }
            if callback_panic {
                panic!("cancelled decision callback panic");
            }
            SupervisionDecision::Resume
        }),
    });
    let slots = Arc::new(Semaphore::new(1));
    let lifetime = ChildLifetime::with_policy(Weak::new(), Some(strategy), slots.clone());
    let mut waiter = Box::pin(lifetime.failure_policy(
        Arc::new(ActorError::Stopped),
        slots,
        tokio::time::Instant::now() + Duration::from_secs(3),
    ));
    assert!(futures::poll!(&mut waiter).is_pending());
    tokio::time::timeout(Duration::from_secs(3), entered)
        .await
        .unwrap()
        .unwrap();
    lifetime.set_strategy(None);
    drop(waiter);
    let mut retirement = Box::pin(lifetime.dispose_policy());
    let first = futures::poll!(&mut retirement);
    let pending = first.is_pending();
    drop(release);
    let result = match first {
        std::task::Poll::Ready(result) => result,
        std::task::Poll::Pending => tokio::time::timeout(Duration::from_secs(3), retirement)
            .await
            .unwrap(),
    };
    settled(&counts, mode).await;
    assert!(pending, "retirement completed before its running snapshot");
    assert_eq!(counts.calls.load(Ordering::SeqCst), 1);
    let repeated = lifetime.dispose_policy().await;
    if callback_panic {
        for result in [result, repeated] {
            let error = result.expect_err("cancelled callback failure must survive retirement");
            let message = error.to_string();
            assert!(
                message.contains("cancelled decision callback panic"),
                "{message}"
            );
            assert_eq!(
                message.contains("policy disposal failed"),
                mode != 0,
                "{message}"
            );
            assert_eq!(
                message.contains("panic payload destructor panicked"),
                mode == 2,
                "{message}"
            );
        }
    } else {
        disposal(result, mode);
        disposal(repeated, mode);
    }
}
#[tokio::test]
async fn disposed_child_does_not_readmit_its_live_parent_policy() {
    let counts = Arc::new(Counts::default());
    let slots = Arc::new(Semaphore::new(1));
    let parent = ChildScope::new(Duration::from_secs(3), slots.clone());
    parent.policy.set(Some(strategy(&counts, 0, false)));
    let lifetime = ChildLifetime::with_policy(Arc::downgrade(&parent), None, slots.clone());
    lifetime.dispose_policy().await.unwrap();
    let result = lifetime
        .failure_policy(
            Arc::new(ActorError::Stopped),
            slots,
            tokio::time::Instant::now() + Duration::from_secs(3),
        )
        .await;
    parent.policy.dispose().await.unwrap();
    assert!(
        matches!(result, Err(ActorError::Stopped)),
        "retired child accepted a new decision"
    );
    assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
    settled(&counts, 0).await;
}
#[tokio::test]
async fn completed_success_transfers_the_original_reply_to_its_caller() {
    let counts = Arc::new(Counts::default());
    let reply = Box::new(capture(&counts, 1));
    let address = &*reply as *const Capture;
    let (control, _completion) = crate::actix::ActorControl::new(Duration::from_secs(3));
    let (result, failure) = control
        .supervise_message(Ok(reply), false, &tokio::runtime::Handle::current())
        .await;
    let reply = result.unwrap().downcast::<Capture>().unwrap();
    assert_eq!(&*reply as *const Capture, address);
    assert!(failure.is_none());
    assert_eq!(counts.drops.load(Ordering::SeqCst), 0);
    assert!(
        caught(|| drop(reply)).is_some(),
        "published reply belongs to its caller"
    );
    assert!(control.cleanup_failure.lock().unwrap().is_none());
}
#[tokio::test]
async fn completed_timeout_keeps_its_message_error_category() {
    let (control, _completion) = crate::actix::ActorControl::new(Duration::from_secs(3));
    let (result, failure) = control
        .supervise_message(
            Err(ActorError::Timeout),
            false,
            &tokio::runtime::Handle::current(),
        )
        .await;
    assert!(matches!(result, Err(ActorError::Timeout)));
    assert!(matches!(failure.as_deref(), Some(ActorError::Timeout)));
}
macro_rules! case {
    ($name:ident, $body:expr) => {
        #[tokio::test]
        async fn $name() {
            $body.await;
        }
    };
}
case!(cancel_one_clean, cancelled_snapshot(0, false));
case!(cancel_one_panic, cancelled_snapshot(1, false));
case!(cancel_one_recursive, cancelled_snapshot(2, false));
case!(cancel_all_clean, cancelled_snapshot(0, true));
case!(cancel_all_panic, cancelled_snapshot(1, true));
case!(cancel_all_recursive, cancelled_snapshot(2, true));
case!(closed_one_clean, closed_permit(0, false));
case!(closed_one_panic, closed_permit(1, false));
case!(closed_one_recursive, closed_permit(2, false));
case!(closed_all_clean, closed_permit(0, true));
case!(closed_all_panic, closed_permit(1, true));
case!(closed_all_recursive, closed_permit(2, true));
case!(unpolled_success_panic, unpolled_message(true, 1));
case!(unpolled_success_recursive, unpolled_message(true, 2));
case!(unpolled_error_panic, unpolled_message(false, 1));
case!(unpolled_error_recursive, unpolled_message(false, 2));
case!(selection_one_preserved, selected_snapshot(false));
case!(selection_all_preserved, selected_snapshot(true));
case!(inherited_one_panic, snapshot_cancellation(1, false, true));
case!(
    inherited_all_recursive,
    snapshot_cancellation(2, true, true)
);
case!(running_snapshot_clean, cancelled_running_snapshot(0, false));
case!(running_snapshot_panic, cancelled_running_snapshot(1, false));
case!(
    running_snapshot_recursive,
    cancelled_running_snapshot(2, false)
);
case!(
    cancelled_callback_panic,
    cancelled_running_snapshot(0, true)
);
case!(
    cancelled_callback_and_capture_panic,
    cancelled_running_snapshot(1, true)
);
case!(
    cancelled_callback_and_recursive_capture,
    cancelled_running_snapshot(2, true)
);
#[test]
fn cancelled_message_panic() {
    cancelled_message_after_decision(1);
}
#[test]
fn cancelled_message_recursive() {
    cancelled_message_after_decision(2);
}
#[test]
fn runtime_snapshot_clean() {
    runtime_cancelled_snapshot(0);
}
#[test]
fn runtime_snapshot_panic() {
    runtime_cancelled_snapshot(1);
}
#[test]
fn runtime_snapshot_recursive() {
    runtime_cancelled_snapshot(2);
}
