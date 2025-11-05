//! User reference destructors must not unwind through other owned references.
#![cfg(feature = "host")]
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::{
    assemble::ParrotSpawner,
    host::{ThreadPropsFactory, ThreadPropsSpawner},
};
use std::future::Future;

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
#[derive(Debug)]
struct State {
    mode: String,
    stopped: AtomicUsize,
    roots_dropped: AtomicUsize,
    clones_dropped: AtomicUsize,
    reject_stop: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    cloned: tokio::sync::Notify,
}
fn state() -> &'static std::sync::OnceLock<Arc<State>> {
    static STATE: std::sync::OnceLock<Arc<State>> = std::sync::OnceLock::new();
    &STATE
}
struct Echo(Arc<State>);
impl Actor for Echo {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move { Ok(message) })
    }
    fn before_stop<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.stopped.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
#[derive(Debug)]
struct Ref {
    actor: BoxedActorRef,
    state: Arc<State>,
    cloned: bool,
    index: usize,
}
impl Drop for Ref {
    fn drop(&mut self) {
        if self.cloned {
            self.state.clones_dropped.fetch_add(1, Ordering::SeqCst);
        } else {
            self.state.roots_dropped.fetch_add(1, Ordering::SeqCst);
        }
        let panic = match self.state.mode.as_str() {
            "clone" | "buffered" | "rejected" => self.cloned,
            _ => !self.cloned,
        };
        assert!(!panic, "injected user reference destructor panic");
    }
}
impl ActorRef for Ref {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send(message)
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.actor.send_with_timeout(message, timeout)
    }
    fn deliver<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        self.actor.deliver(message)
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.state.reject_stop.load(Ordering::SeqCst)
                && (self.state.mode != "mixed" || self.index == 0)
            {
                return Err(parrot_api::errors::ActorError::InternalError(
                    "injected stop failure".into(),
                ));
            }
            self.actor.stop().await
        })
    }
    fn path(&self) -> String {
        self.actor.path()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        self.actor.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        assert!(
            !(self.state.mode == "clone" && self.index == 1),
            "injected clone panic"
        );
        if self.index == 1 {
            self.state.cloned.notify_one();
        }
        Box::new(Self {
            actor: self.actor.clone_boxed(),
            state: self.state.clone(),
            cloned: true,
            index: self.index,
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn spawn(
    engine: Arc<ThreadActorSystem>,
    path: String,
    _: Option<toml::Table>,
) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    Box::pin(async move {
        let state = state().get().unwrap().clone();
        let index = path.rsplit('-').next().unwrap().parse::<usize>().unwrap();
        if index == 2 {
            return Err(parrot_api::errors::ActorError::InternalError(
                "third factory failed".into(),
            ));
        }
        let actor = engine
            .spawn_at(Echo(state.clone()), &path, None, Default::default())
            .await
            .unwrap();
        actor.send(Box::new(17_u64)).await.unwrap();
        if state.mode == "rejected" && index == 1 {
            state.entered.notify_one();
            state.release.notified().await;
        }
        Ok(Box::new(Ref {
            actor: Box::new(actor),
            state,
            cloned: false,
            index,
        }) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! { ThreadPropsFactory { name: "test.host-disposal", spawn } }
async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn exercise(mode: &str) {
    let probe = Arc::new(State {
        mode: mode.into(),
        stopped: 0.into(),
        roots_dropped: 0.into(),
        clones_dropped: 0.into(),
        reject_stop: matches!(mode, "retained" | "retry" | "mixed").into(),
        entered: Default::default(),
        release: Default::default(),
        cloned: Default::default(),
    });
    state().set(probe.clone()).unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    let spawner = ThreadPropsSpawner::new(engine.clone());
    let count = if matches!(mode, "rollback" | "retained" | "retry" | "mixed") {
        3
    } else {
        2
    };
    let paths: Vec<_> = (0..count).map(|i| format!("/user/disposal-{i}")).collect();
    if matches!(mode, "buffered" | "rejected") {
        let mut future = Box::pin(spawner.spawn_props("test.host-disposal", &paths, None));
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let notify = if mode == "buffered" {
            &probe.cloned
        } else {
            &probe.entered
        };
        // Both the host owner and this waiter use this current-thread runtime.
        // The final synchronous clone returns and sends before the waiter resumes.
        tokio::time::timeout(Duration::from_secs(3), notify.notified())
            .await
            .unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future))).is_ok(),
            "cancelled handoff unwound"
        );
        probe.release.notify_one();
        wait_for(|| {
            engine.actor_count() == 0
                && probe.roots_dropped.load(Ordering::SeqCst) == 2
                && probe.clones_dropped.load(Ordering::SeqCst) == 2
        })
        .await;
    } else {
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            spawner.spawn_props("test.host-disposal", &paths, None),
        )
        .await
        .unwrap();
        if mode == "claimed" {
            let actors = result.unwrap();
            wait_for(|| probe.roots_dropped.load(Ordering::SeqCst) == 2).await;
            assert_eq!(engine.actor_count(), 2);
            assert!(
                actors[0].as_any().is::<Ref>(),
                "handoff must retain the public reference type"
            );
            for actor in &actors {
                assert_eq!(
                    *actor
                        .send(Box::new(42_u64))
                        .await
                        .unwrap()
                        .downcast::<u64>()
                        .unwrap(),
                    42
                );
                actor.stop().await.unwrap();
            }
            drop(actors);
        } else {
            let error = result.unwrap_err();
            if matches!(mode, "retained" | "retry" | "mixed") {
                let remaining = if mode == "mixed" { 1 } else { 2 };
                assert_eq!(error.retained_actors().len(), remaining);
                assert_eq!(spawner.pending_cleanup().len(), remaining);
                assert_eq!(engine.actor_count(), remaining);
                if mode == "mixed" {
                    assert!(error
                        .to_string()
                        .contains("release /user/disposal-1 reference panicked"));
                    assert!(error.to_string().contains("injected stop failure"));
                }
                if mode == "retry" || mode == "mixed" {
                    probe.reject_stop.store(false, Ordering::SeqCst);
                    spawner.retry_pending_cleanup().await.unwrap();
                    assert!(spawner.pending_cleanup().is_empty());
                } else {
                    // Dropping cleanup handles is not an asynchronous retirement.
                    engine.shutdown_internal().await.unwrap();
                }
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        drop(error);
                        drop(spawner);
                    }))
                    .is_ok(),
                    "retained owner disposal unwound"
                );
            } else {
                assert!(
                    error.to_string().contains(if mode == "clone" {
                        "clone panicked"
                    } else {
                        "third factory failed"
                    }),
                    "primary failure lost: {error}"
                );
                assert_eq!(engine.actor_count(), 0, "rollback skipped an actual actor");
            }
        }
    }
    assert_eq!(engine.actor_count(), 0);
    assert_eq!(probe.stopped.load(Ordering::SeqCst), 2);
    assert_eq!(probe.roots_dropped.load(Ordering::SeqCst), 2);
    let clones = match mode {
        "clone" => 1,
        "claimed" | "buffered" | "rejected" => 2,
        _ => 0,
    };
    assert_eq!(probe.clones_dropped.load(Ordering::SeqCst), clones);
    engine.shutdown_internal().await.unwrap();
}
// Baselines with two panicking destructors can abort, so isolate every case.
fn child(name: &str, mode: &str) {
    if std::env::var("PARROT_HOST_DISPOSAL_CHILD").as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(exercise(mode));
    } else {
        let output = tempfile::NamedTempFile::new().unwrap();
        let mut process = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_HOST_DISPOSAL_CHILD", name)
            .stdout(output.as_file().try_clone().unwrap())
            .stderr(output.as_file().try_clone().unwrap())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = process.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                process.kill().unwrap();
                process.wait().unwrap();
                panic!(
                    "disposal child timed out: {}",
                    std::fs::read_to_string(output.path()).unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            status.success(),
            "disposal child failed: {status}\n{}",
            std::fs::read_to_string(output.path()).unwrap()
        );
    }
}
#[test]
fn rollback_disposal_does_not_skip_originals() {
    child("rollback_disposal_does_not_skip_originals", "rollback");
}
#[test]
fn partial_clone_disposal_does_not_abort_rollback() {
    child("partial_clone_disposal_does_not_abort_rollback", "clone");
}
#[test]
fn buffered_handoff_disposal_keeps_retirement_owned() {
    child(
        "buffered_handoff_disposal_keeps_retirement_owned",
        "buffered",
    );
}
#[test]
fn rejected_handoff_disposal_keeps_retirement_owned() {
    child(
        "rejected_handoff_disposal_keeps_retirement_owned",
        "rejected",
    );
}
#[test]
fn retained_cleanup_owner_disposal_is_isolated() {
    child("retained_cleanup_owner_disposal_is_isolated", "retained");
}
#[test]
fn retired_cleanup_handles_dispose_without_unwind() {
    child("retired_cleanup_handles_dispose_without_unwind", "retry");
}
#[test]
fn claimed_handoff_disposes_originals_without_stopping_clones() {
    child(
        "claimed_handoff_disposes_originals_without_stopping_clones",
        "claimed",
    );
}

#[test]
fn partial_retirement_disposal_preserves_failed_original_for_retry() {
    child(
        "partial_retirement_disposal_preserves_failed_original_for_retry",
        "mixed",
    );
}
