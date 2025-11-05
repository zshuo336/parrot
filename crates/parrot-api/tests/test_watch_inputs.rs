//! Direct watch adapters own their watcher input even before first poll.
use parrot_api::{
    address::ActorRef,
    death_watch::{DeathWatch, TerminationRecipient},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

struct Reference {
    drops: Arc<AtomicUsize>,
    hostile_drop: bool,
    capability: Option<DeathWatch>,
    panic_capability: bool,
    fresh_recipient: Option<(Arc<AtomicUsize>, &'static str)>,
    panic_payload: Option<Arc<AtomicUsize>>,
    calls: Arc<AtomicUsize>,
}
impl std::fmt::Debug for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reference")
            .field("hostile_drop", &self.hostile_drop)
            .finish()
    }
}
impl Reference {
    fn new(drops: &Arc<AtomicUsize>, hostile_drop: bool) -> Self {
        Self {
            drops: drops.clone(),
            hostile_drop,
            capability: None,
            panic_capability: false,
            fresh_recipient: None,
            panic_payload: None,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}
impl Drop for Reference {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.hostile_drop, "watcher disposal failed");
    }
}
impl ActorRef for Reference {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { unreachable!() })
    }
    fn send_with_timeout<'a>(
        &'a self,
        _: BoxedMessage,
        _: Option<std::time::Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { unreachable!() })
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { unreachable!() })
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/watch-fixture".into()
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self::new(&self.drops, false))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn death_watch(&self) -> Option<DeathWatch> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(drops) = &self.panic_payload {
            std::panic::panic_any(HostilePayload(drops.clone()));
        }
        assert!(!self.panic_capability, "capability callback failed");
        if let Some((drops, label)) = &self.fresh_recipient {
            let identity = DeathWatch::new(4);
            identity
                .set_recipient(Arc::new(HostileRecipient(drops.clone(), label)))
                .unwrap();
            return Some(identity);
        }
        self.capability.clone()
    }
}

struct HostilePayload(Arc<AtomicUsize>);
impl Drop for HostilePayload {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("panic payload disposal failed");
    }
}
struct HostileRecipient(Arc<AtomicUsize>, &'static str);
impl Drop for HostileRecipient {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("{} recipient disposal failed", self.1);
    }
}
impl TerminationRecipient for HostileRecipient {
    fn deliver(&self, _: parrot_api::context::ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn unpolled(unwatch: bool) {
    let target = Reference::new(&Arc::new(AtomicUsize::new(0)), false);
    let drops = Arc::new(AtomicUsize::new(0));
    let watcher = Box::new(Reference::new(&drops, true));
    let future = if unwatch {
        target.unwatch(watcher)
    } else {
        target.watch(watcher)
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future)));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(
        result.is_ok(),
        "unpolled cancellation escaped the watcher disposal boundary"
    );
}
fn rejected(unwatch: bool) {
    let target = Reference::new(&Arc::new(AtomicUsize::new(0)), false);
    let drops = Arc::new(AtomicUsize::new(0));
    let watcher = Box::new(Reference::new(&drops, true));
    let future = if unwatch {
        target.unwatch(watcher)
    } else {
        target.watch(watcher)
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        futures::executor::block_on(future)
    }));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let error = result
        .expect("watcher disposal replaced an ordinary capability error with unwinding")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("target has no death-watch capability"),
        "{error}"
    );
}
#[test]
fn unpolled_watch_retires_watcher_without_unwinding() {
    unpolled(false);
}
#[test]
fn unpolled_unwatch_retires_watcher_without_unwinding() {
    unpolled(true);
}
#[test]
fn rejected_watch_preserves_capability_error_after_watcher_disposal() {
    rejected(false);
}
#[test]
fn rejected_unwatch_preserves_capability_error_after_watcher_disposal() {
    rejected(true);
}

struct Recipient;
impl TerminationRecipient for Recipient {
    fn deliver(&self, _: parrot_api::context::ActorTerminated) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
fn identity() -> DeathWatch {
    let identity = DeathWatch::new(4);
    identity.set_recipient(Arc::new(Recipient)).unwrap();
    identity
}
fn operation(target: &Reference, watcher: Reference, remove: bool) -> ActorResult<()> {
    let future = if remove {
        target.unwatch(Box::new(watcher))
    } else {
        target.watch(Box::new(watcher))
    };
    futures::executor::block_on(future)
}
fn callback_failure(remove: bool, target_fails: bool, test: &str) {
    isolated(test, || callback_failure_body(remove, target_fails));
}
fn isolated(test: &str, body: impl FnOnce()) {
    if std::env::var("PARROT_WATCH_INPUT_CHILD").as_deref() == Ok(test) {
        body();
        return;
    }
    struct Log(std::path::PathBuf);
    impl Drop for Log {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let log =
        Log(std::env::temp_dir().join(format!("parrot-watch-inputs-{}.log", uuid::Uuid::new_v4())));
    let output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&log.0)
        .unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    let mut child = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("PARROT_WATCH_INPUT_CHILD", test)
            .env("RUST_BACKTRACE", "0")
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            let output = std::fs::read_to_string(&log.0).unwrap();
            assert!(
                status.success(),
                "isolated watch contract failed: {status}\n{output}"
            );
            assert!(
                output.contains("1 passed; 0 failed"),
                "contract did not execute: {output}"
            );
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "isolated watch contract timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
fn callback_failure_body(remove: bool, target_fails: bool) {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut target = Reference::new(&Arc::new(AtomicUsize::new(0)), false);
    target.capability = Some(identity());
    target.panic_capability = target_fails;
    let mut watcher = Reference::new(&drops, true);
    watcher.panic_capability = !target_fails;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        operation(&target, watcher, remove)
    }));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let error = result
        .expect("capability callback/cleanup escaped the adapter")
        .unwrap_err();
    assert!(
        error.to_string().contains("capability callback failed"),
        "{error}"
    );
}
#[test]
fn watch_target_callback_and_watcher_drop_are_independent() {
    callback_failure(
        false,
        true,
        "watch_target_callback_and_watcher_drop_are_independent",
    );
}
#[test]
fn unwatch_target_callback_and_watcher_drop_are_independent() {
    callback_failure(
        true,
        true,
        "unwatch_target_callback_and_watcher_drop_are_independent",
    );
}
#[test]
fn watch_watcher_callback_and_watcher_drop_are_independent() {
    callback_failure(
        false,
        false,
        "watch_watcher_callback_and_watcher_drop_are_independent",
    );
}
#[test]
fn unwatch_watcher_callback_and_watcher_drop_are_independent() {
    callback_failure(
        true,
        false,
        "unwatch_watcher_callback_and_watcher_drop_are_independent",
    );
}
#[test]
fn successful_watch_and_unwatch_report_cleanup_failure_without_fabricating_rollback() {
    for remove in [false, true] {
        let target_identity = identity();
        let watcher_identity = identity();
        if remove {
            target_identity.watch(&watcher_identity).unwrap();
        }
        let mut target = Reference::new(&Arc::new(AtomicUsize::new(0)), false);
        target.capability = Some(target_identity.clone());
        let drops = Arc::new(AtomicUsize::new(0));
        let mut watcher = Reference::new(&drops, true);
        watcher.capability = Some(watcher_identity.clone());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            operation(&target, watcher, remove)
        }));
        let error = result.expect("cleanup panic escaped").unwrap_err();
        assert!(
            error.to_string().contains("watcher disposal failed"),
            "{error}"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(target_identity.subscriptions().0, usize::from(!remove));
        assert_eq!(watcher_identity.subscriptions().1, usize::from(!remove));
        target_identity.unwatch(&watcher_identity);
    }
}
#[test]
fn watcher_capability_error_precedes_watcher_cleanup_failure() {
    for remove in [false, true] {
        let mut target = Reference::new(&Arc::new(AtomicUsize::new(0)), false);
        target.capability = Some(identity());
        let drops = Arc::new(AtomicUsize::new(0));
        let watcher = Reference::new(&drops, true);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            operation(&target, watcher, remove)
        }));
        let error = result.expect("cleanup panic escaped").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("watcher has no death-watch capability"),
            "{error}"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
#[test]
fn watch_callbacks_remain_lazy_and_repeat_registration_coalesces() {
    let target_identity = identity();
    let watcher_identity = identity();
    let mut target = Reference::new(&Arc::new(AtomicUsize::new(0)), false);
    target.capability = Some(target_identity.clone());
    let drops = Arc::new(AtomicUsize::new(0));
    for remove in [false, true] {
        let mut watcher = Reference::new(&drops, false);
        watcher.capability = Some(watcher_identity.clone());
        let calls = watcher.calls.clone();
        let future = if remove {
            target.unwatch(Box::new(watcher))
        } else {
            target.watch(Box::new(watcher))
        };
        drop(future);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    assert_eq!(target.calls.load(Ordering::SeqCst), 0);
    for remove in [false, false, true] {
        let mut watcher = Reference::new(&drops, false);
        watcher.capability = Some(watcher_identity.clone());
        operation(&target, watcher, remove).unwrap();
        assert_eq!(target_identity.subscriptions().0, usize::from(!remove));
    }
    assert_eq!(drops.load(Ordering::SeqCst), 5);
}

fn last_identities(remove: bool) {
    let target_drops = Arc::new(AtomicUsize::new(0));
    let watcher_drops = Arc::new(AtomicUsize::new(0));
    let reference_drops = Arc::new(AtomicUsize::new(0));
    let mut target = Reference::new(&Arc::new(AtomicUsize::new(0)), false);
    target.fresh_recipient = Some((target_drops.clone(), "target"));
    let mut watcher = Reference::new(&reference_drops, true);
    watcher.fresh_recipient = Some((watcher_drops.clone(), "watcher"));
    let error = operation(&target, watcher, remove).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("watcher recipient disposal failed"),
        "first cleanup failure must win: {error}"
    );
    for drops in [target_drops, watcher_drops, reference_drops] {
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "each independent owner must retire exactly once"
        );
    }
}
#[test]
fn watch_retires_last_capability_handles_independently() {
    isolated(
        "watch_retires_last_capability_handles_independently",
        || last_identities(false),
    );
}
#[test]
fn unwatch_retires_last_capability_handles_independently() {
    isolated(
        "unwatch_retires_last_capability_handles_independently",
        || last_identities(true),
    );
}
#[test]
fn missing_watcher_capability_precedes_last_target_handle_cleanup_failure() {
    isolated(
        "missing_watcher_capability_precedes_last_target_handle_cleanup_failure",
        || {
            for remove in [false, true] {
                let target_drops = Arc::new(AtomicUsize::new(0));
                let reference_drops = Arc::new(AtomicUsize::new(0));
                let mut target = Reference::new(&Arc::new(AtomicUsize::new(0)), false);
                target.fresh_recipient = Some((target_drops.clone(), "target"));
                let watcher = Reference::new(&reference_drops, true);
                let error = operation(&target, watcher, remove).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("watcher has no death-watch capability"),
                    "{error}"
                );
                assert_eq!(target_drops.load(Ordering::SeqCst), 1);
                assert_eq!(reference_drops.load(Ordering::SeqCst), 1);
            }
        },
    );
}
#[test]
fn callback_panic_payload_and_watcher_retire_independently() {
    isolated(
        "callback_panic_payload_and_watcher_retire_independently",
        || {
            for remove in [false, true] {
                for target_fails in [false, true] {
                    let payload_drops = Arc::new(AtomicUsize::new(0));
                    let reference_drops = Arc::new(AtomicUsize::new(0));
                    let mut target = Reference::new(&Arc::new(AtomicUsize::new(0)), false);
                    target.capability = Some(identity());
                    let mut watcher = Reference::new(&reference_drops, true);
                    if target_fails {
                        target.panic_payload = Some(payload_drops.clone());
                    } else {
                        watcher.panic_payload = Some(payload_drops.clone());
                    }
                    let error = operation(&target, watcher, remove).unwrap_err();
                    assert!(
                        error.to_string().contains(
                            "non-string panic payload; panic payload destructor panicked"
                        ),
                        "{error}"
                    );
                    assert_eq!(payload_drops.load(Ordering::SeqCst), 1);
                    assert_eq!(reference_drops.load(Ordering::SeqCst), 1);
                }
            }
        },
    );
}

#[tokio::test]
async fn default_watch_delivers_real_termination_and_unwatch_suppresses_it() {
    struct Recording(Arc<std::sync::Mutex<Vec<parrot_api::context::ActorTerminated>>>);
    impl TerminationRecipient for Recording {
        fn deliver(
            &self,
            event: parrot_api::context::ActorTerminated,
        ) -> BoxedFuture<'_, ActorResult<()>> {
            Box::pin(async move {
                self.0.lock().unwrap().push(event);
                Ok(())
            })
        }
    }
    for remove in [false, true] {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let target_identity = identity();
        let watcher_identity = DeathWatch::new(4);
        watcher_identity
            .set_recipient(Arc::new(Recording(events.clone())))
            .unwrap();
        let mut target = Reference::new(&Arc::new(AtomicUsize::new(0)), false);
        target.capability = Some(target_identity.clone());
        let drops = Arc::new(AtomicUsize::new(0));
        let watcher = || {
            let mut watcher = Reference::new(&drops, false);
            watcher.capability = Some(watcher_identity.clone());
            Box::new(watcher) as BoxedActorRef
        };
        target.watch(watcher()).await.unwrap();
        target.watch(watcher()).await.unwrap();
        if remove {
            target.unwatch(watcher()).await.unwrap();
        }
        let event = parrot_api::context::ActorTerminated {
            generation: target_identity.id(),
            path: "/observed".into(),
            reason: parrot_api::DeathReason::Normal,
        };
        target_identity
            .terminated_checked(event.clone(), std::time::Duration::from_secs(1))
            .await
            .unwrap();
        let recorded = events.lock().unwrap();
        assert_eq!(recorded.len(), usize::from(!remove));
        if !remove {
            assert_eq!(recorded[0].generation, event.generation);
            assert_eq!(recorded[0].path, event.path);
            assert!(matches!(
                recorded[0].reason,
                parrot_api::DeathReason::Normal
            ));
        }
        assert_eq!(target_identity.subscriptions(), (0, 0));
        assert_eq!(watcher_identity.subscriptions(), (0, 0));
        assert_eq!(drops.load(Ordering::SeqCst), 2 + usize::from(remove));
    }
}
