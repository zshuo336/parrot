//! User reference destructors must not unwind through other owned references.
use parrot::thread::{context::ThreadContext, system::ThreadActorSystem};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::ActorRef,
    errors::ActorError,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use parrot_app::host::ThreadPropsFactory;
use parrot_node::NodeComponentExecutor;
use parrot_remote::admin_v2::{
    v2_err, AdminArtifactRef, AdminInstancePolicy, AdminReplyV2 as R, ComponentDeploy,
    ComponentExecutor,
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
    armed: std::sync::atomic::AtomicBool,
    callback_fail: std::sync::atomic::AtomicBool,
    callback_drops: AtomicUsize,
    stopped: AtomicUsize,
    roots_dropped: AtomicUsize,
    clones_dropped: AtomicUsize,
    reject_stop: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
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
struct ThrowingPayload;
impl Drop for ThrowingPayload {
    fn drop(&mut self) {
        std::panic::panic_any(Self);
    }
}
impl Drop for Ref {
    fn drop(&mut self) {
        if self.cloned {
            self.state.clones_dropped.fetch_add(1, Ordering::SeqCst);
        } else {
            self.state.roots_dropped.fetch_add(1, Ordering::SeqCst);
        }
        if self.cloned
            && self.state.armed.load(Ordering::SeqCst)
            && !matches!(self.state.mode.as_str(), "health" | "status_panic")
            && !self.state.mode.starts_with("callback:")
        {
            if self.state.mode == "payload" {
                std::panic::panic_any(ThrowingPayload);
            }
            panic!("injected Node reference destructor panic");
        }
    }
}
struct CallbackFuture<'a, T> {
    inner: BoxedFuture<'a, T>,
    state: Arc<State>,
    poll_panics: bool,
    drop_panics: bool,
    pending: bool,
}
impl<T> Future for CallbackFuture<'_, T> {
    type Output = T;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<T> {
        if self.pending {
            self.state.entered.notify_one();
            return std::task::Poll::Pending;
        }
        assert!(!self.poll_panics, "injected callback poll panic");
        self.inner.as_mut().poll(cx)
    }
}
impl<T> Drop for CallbackFuture<'_, T> {
    fn drop(&mut self) {
        self.state.callback_drops.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.drop_panics,
            "injected callback future destructor panic"
        );
    }
}
#[derive(Debug)]
struct CallbackError {
    display_panics: bool,
    drop_panics: bool,
}
impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        assert!(
            !self.display_panics,
            "injected callback error formatting panic"
        );
        f.write_str("callback business failure")
    }
}
impl std::error::Error for CallbackError {}
impl Drop for CallbackError {
    fn drop(&mut self) {
        assert!(
            !self.drop_panics,
            "injected callback error destructor panic"
        );
    }
}
struct CallbackReply;
impl Drop for CallbackReply {
    fn drop(&mut self) {
        panic!("injected health reply destructor panic");
    }
}
struct PanicSubscriber;
impl tracing::Subscriber for PanicSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
            && metadata.target().starts_with("parrot_node::executor_v2")
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        panic!("injected management subscriber panic");
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
impl ActorRef for Ref {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        if self.state.mode == "health"
            && self.index == 1
            && self.state.callback_fail.load(Ordering::SeqCst)
        {
            panic!("injected health-check construction panic");
        }
        let mode = self.state.mode.as_str();
        if self.state.callback_fail.load(Ordering::SeqCst)
            && self.index == 1
            && mode.starts_with("callback:health_")
        {
            let inner = Box::pin(async move {
                self.actor.send(message).await?;
                if mode == "callback:health_error_both" {
                    Err(ActorError::Other(
                        CallbackError {
                            display_panics: true,
                            drop_panics: true,
                        }
                        .into(),
                    ))
                } else {
                    Ok(Box::new(CallbackReply) as BoxedMessage)
                }
            });
            return Box::pin(CallbackFuture {
                inner,
                state: self.state.clone(),
                poll_panics: mode == "callback:health_poll_drop",
                drop_panics: mode == "callback:health_poll_drop",
                pending: false,
            });
        }
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
        let mode = self.state.mode.as_str();
        if self.state.callback_fail.load(Ordering::SeqCst)
            && self.index == 1
            && (mode.starts_with("callback:stop_") || mode.starts_with("callback:rollback_"))
        {
            let inner = Box::pin(async move {
                if mode.starts_with("callback:stop_error_")
                    || mode.starts_with("callback:rollback_error_")
                {
                    Err(ActorError::Other(
                        CallbackError {
                            display_panics: mode.ends_with("display") || mode.ends_with("both"),
                            drop_panics: mode.ends_with("drop") || mode.ends_with("both"),
                        }
                        .into(),
                    ))
                } else {
                    self.actor.stop().await
                }
            });
            return Box::pin(CallbackFuture {
                inner,
                state: self.state.clone(),
                poll_panics: matches!(
                    mode,
                    "callback:stop_poll_drop" | "callback:rollback_poll_drop"
                ),
                drop_panics: matches!(
                    mode,
                    "callback:stop_poll_drop"
                        | "callback:stop_ready_drop"
                        | "callback:rollback_poll_drop"
                ),
                pending: false,
            });
        }
        Box::pin(async move {
            if self.state.reject_stop.load(Ordering::SeqCst) && self.index == 0 {
                return Err(parrot_api::errors::ActorError::InternalError(
                    "injected stop rejection".into(),
                ));
            }
            self.actor.stop().await
        })
    }
    fn path(&self) -> String {
        self.actor.path()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        let mode = self.state.mode.as_str();
        if self.state.callback_fail.load(Ordering::SeqCst)
            && self.index == 0
            && (mode.starts_with("callback:status_") || mode == "callback:metrics_poll_drop")
        {
            return Box::pin(CallbackFuture {
                inner: self.actor.is_alive(),
                state: self.state.clone(),
                poll_panics: mode.ends_with("poll_drop") || mode.ends_with("poll_logger"),
                drop_panics: !mode.ends_with("logger"),
                pending: mode == "callback:status_cancel_drop",
            });
        }
        Box::pin(async move {
            if matches!(self.state.mode.as_str(), "status" | "cancel_status") {
                self.state.entered.notify_one();
                self.state.release.notified().await;
            }
            if self.state.mode == "status_panic"
                && self.index == 0
                && self.state.callback_fail.load(Ordering::SeqCst)
            {
                panic!("injected status poll panic");
            }
            self.actor.is_alive().await
        })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
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
        let actor = engine
            .spawn_at(Echo(state.clone()), &path, None, Default::default())
            .await
            .unwrap();
        actor.send(Box::new(7_u64)).await.unwrap();
        Ok(Box::new(Ref {
            actor: Box::new(actor),
            state,
            cloned: false,
            index,
        }) as BoxedActorRef)
    })
}
parrot_api::message::inventory::submit! { ThreadPropsFactory { name: "test.node-disposal", spawn } }
fn legacy_spawn(path: &str) -> BoxedFuture<'static, ActorResult<BoxedActorRef>> {
    let path = path.to_string();
    let state = state().get().unwrap().clone();
    let third = path.ends_with("-2");
    let wrap = third && state.mode.starts_with("callback:factory_");
    let observed = state.clone();
    let inner = Box::pin(async move {
        if third && state.mode.starts_with("callback:factory_error_") {
            return Err(ActorError::Other(
                CallbackError {
                    display_panics: state.mode.ends_with("display") || state.mode.ends_with("both"),
                    drop_panics: state.mode.ends_with("drop") || state.mode.ends_with("both"),
                }
                .into(),
            ));
        }
        if third && state.mode != "callback:factory_ready_drop" {
            return Err(parrot_api::errors::ActorError::InternalError(
                "legacy third factory failed".into(),
            ));
        }
        let actor = spawn(parrot_node::NODE.get().unwrap().ts.clone(), path, None).await?;
        // The legacy path receives this view directly, without the app spawner.
        let view = actor.clone_boxed();
        drop(actor);
        Ok(view)
    });
    if wrap {
        Box::pin(CallbackFuture {
            inner,
            poll_panics: observed.mode == "callback:factory_poll_drop",
            drop_panics: matches!(
                observed.mode.as_str(),
                "callback:factory_poll_drop" | "callback:factory_ready_drop"
            ),
            pending: false,
            state: observed,
        })
    } else {
        inner
    }
}
parrot_api::message::inventory::submit! { parrot_remote::admin::PropsFactory { name: "test.node-legacy-disposal", spawn: legacy_spawn } }
async fn exercise(mode: &str) {
    let probe = Arc::new(State {
        mode: mode.into(),
        armed: false.into(),
        callback_fail: true.into(),
        callback_drops: 0.into(),
        stopped: 0.into(),
        roots_dropped: 0.into(),
        clones_dropped: 0.into(),
        reject_stop: (mode == "mixed").into(),
        entered: Default::default(),
        release: Default::default(),
    });
    state().set(probe.clone()).unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    if mode == "legacy"
        || mode.starts_with("callback:rollback_")
        || mode.starts_with("callback:factory_")
    {
        let facade = Arc::new(
            parrot::system::ParrotActorSystem::new(Default::default())
                .await
                .unwrap(),
        );
        parrot_node::NODE
            .set(parrot_node::NodeState {
                facade: facade.clone(),
                ts: engine.clone(),
            })
            .ok()
            .unwrap();
        let executor = NodeComponentExecutor::new(engine.clone());
        probe.armed.store(true, Ordering::SeqCst);
        let command = ComponentDeploy {
            name: "legacy".into(),
            version: "1.0.0".into(),
            artifact: AdminArtifactRef::Props {
                factory: "test.node-legacy-disposal".into(),
            },
            instances: AdminInstancePolicy::Pool { count: 3 },
            config: None,
        };
        let reply = executor.deploy(1, &command).await;
        if mode.starts_with("callback:factory_") {
            if mode == "callback:factory_error_drop" {
                assert!(
                    matches!(reply, R::Failed { ref detail, .. } if detail.contains("callback business failure")),
                    "cleanup replaced the formatted factory cause: {reply:?}"
                );
            }
            assert!(
                matches!(reply, R::Failed { ref detail, .. } if detail.contains("Props factory test.node-legacy-disposal panicked")),
                "{reply:?}"
            );
            assert_eq!(
                engine.actor_count(),
                0,
                "factory failure orphaned an admitted instance"
            );
            let count = if mode == "callback:factory_ready_drop" {
                3
            } else {
                2
            };
            assert_eq!(probe.stopped.load(Ordering::SeqCst), count);
            assert_eq!(probe.clones_dropped.load(Ordering::SeqCst), count);
            assert_eq!(probe.callback_drops.load(Ordering::SeqCst), 1);
            assert!(
                matches!(executor.status(2, "/user/legacy").await, R::Failed { code, .. } if code == v2_err::COMPONENT_NOT_FOUND)
            );
            engine.shutdown_internal().await.unwrap();
            return;
        }
        if mode.starts_with("callback:rollback_") {
            let stop_failure = if mode == "callback:rollback_poll_drop" {
                "Panic: injected callback poll panic"
            } else {
                "actor stop panicked"
            };
            assert!(
                matches!(reply, R::Failed { ref detail, .. } if detail.contains("legacy third factory failed") && detail.contains(stop_failure)),
                "{reply:?}"
            );
            assert_eq!(engine.actor_count(), 1);
            assert_eq!(probe.stopped.load(Ordering::SeqCst), 1);
            let status = executor.status(2, "/user/legacy").await;
            assert!(
                matches!(status, R::Status { ref states, .. } if states.len() == 1 && states[0].path.ends_with("-1") && states[0].state == "cleanup_failed"),
                "{status:?}"
            );
            probe.callback_fail.store(false, Ordering::SeqCst);
            assert!(matches!(
                executor.stop(3, "/user/legacy").await,
                R::Stopped { .. }
            ));
            assert_eq!(engine.actor_count(), 0);
            assert_eq!(probe.stopped.load(Ordering::SeqCst), 2);
            assert_eq!(probe.clones_dropped.load(Ordering::SeqCst), 2);
            assert_eq!(probe.callback_drops.load(Ordering::SeqCst), 1);
            engine.shutdown_internal().await.unwrap();
            return;
        }
        assert!(
            matches!(reply,R::Failed{ref detail,..} if detail.contains("legacy third factory failed") && detail.contains("reference panicked")),
            "{reply:?}"
        );
        assert_eq!(engine.actor_count(), 0);
        assert_eq!(probe.stopped.load(Ordering::SeqCst), 2);
        assert_eq!(probe.clones_dropped.load(Ordering::SeqCst), 2);
        assert!(
            matches!(executor.status(2,"/user/legacy").await,R::Failed{code,..} if code==v2_err::COMPONENT_NOT_FOUND)
        );
        engine.shutdown_internal().await.unwrap();
        // NODE retains the empty facade until this isolated child exits.
        // All actors belong to the separately awaited engine above.
        return;
    }
    let executor = NodeComponentExecutor::new(engine.clone());
    let command = ComponentDeploy {
        name: "disposal".into(),
        version: "1.0.0".into(),
        artifact: AdminArtifactRef::Props {
            factory: "test.node-disposal".into(),
        },
        instances: AdminInstancePolicy::Pool { count: 2 },
        config: None,
    };
    assert!(matches!(
        executor.deploy(1, &command).await,
        R::Deployed { .. }
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        while probe.roots_dropped.load(Ordering::SeqCst) != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    probe.armed.store(true, Ordering::SeqCst);
    let owner_mode = matches!(mode, "owner" | "owner_logger");
    if owner_mode {
        engine.shutdown_internal().await.unwrap();
        let _subscriber =
            (mode == "owner_logger").then(|| tracing::subscriber::set_default(PanicSubscriber));
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(executor))).is_ok(),
            "executor Drop unwound"
        );
    } else {
        match mode {
            mode if mode.starts_with("callback:status_")
                || mode == "callback:metrics_poll_drop" =>
            {
                let _subscriber = mode
                    .ends_with("logger")
                    .then(|| tracing::subscriber::set_default(PanicSubscriber));
                if mode == "callback:status_cancel_drop" {
                    let mut status = Box::pin(executor.status(2, "/user/disposal"));
                    assert!(futures::poll!(&mut status).is_pending());
                    probe.entered.notified().await;
                    probe.callback_fail.store(false, Ordering::SeqCst);
                    assert!(matches!(
                        executor.stop(3, "/user/disposal").await,
                        R::Stopped { .. }
                    ));
                    assert!(
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(status)))
                            .is_ok(),
                        "cancelled callback future unwound"
                    );
                } else {
                    let states = if mode == "callback:metrics_poll_drop" {
                        match executor.metrics(2).await {
                            R::Metrics { snapshot, .. } => snapshot.component_states,
                            other => panic!("{other:?}"),
                        }
                    } else {
                        match executor.status(2, "/user/disposal").await {
                            R::Status { states, .. } => states,
                            other => panic!("{other:?}"),
                        }
                    };
                    assert_eq!(states.len(), 2);
                    assert_eq!(states[0].state, "unavailable");
                    assert_eq!(states[1].state, "running");
                    probe.callback_fail.store(false, Ordering::SeqCst);
                    assert!(matches!(
                        executor.stop(3, "/user/disposal").await,
                        R::Stopped { .. }
                    ));
                }
                assert_eq!(probe.callback_drops.load(Ordering::SeqCst), 1);
            }
            mode if mode.starts_with("callback:stop_") || mode.starts_with("callback:health_") => {
                if mode.starts_with("callback:stop_") {
                    let reply = executor.stop(2, "/user/disposal").await;
                    if mode == "callback:stop_error_drop" {
                        assert!(
                            matches!(reply, R::Failed { ref detail, .. } if detail.contains("callback business failure")),
                            "cleanup replaced the formatted stop cause: {reply:?}"
                        );
                    }
                    let stop_failure = match mode {
                        "callback:stop_poll_drop" => "Panic: injected callback poll panic",
                        "callback:stop_ready_drop" => {
                            "Panic: injected callback future destructor panic"
                        }
                        _ => "actor stop panicked",
                    };
                    assert!(
                        matches!(reply, R::Failed { ref detail, .. } if detail.contains(stop_failure)),
                        "{reply:?}"
                    );
                    assert_eq!(
                        probe.stopped.load(Ordering::SeqCst),
                        if mode == "callback:stop_ready_drop" {
                            2
                        } else {
                            1
                        },
                        "a failed callback skipped a healthy member"
                    );
                    let status = executor.status(3, "/user/disposal").await;
                    assert!(
                        matches!(status, R::Status { ref states, .. } if states.len() == 1 && states[0].path.ends_with("-1") && states[0].state == "cleanup_failed"),
                        "{status:?}"
                    );
                    probe.callback_fail.store(false, Ordering::SeqCst);
                    assert!(matches!(
                        executor.stop(4, "/user/disposal").await,
                        R::Stopped { .. }
                    ));
                } else {
                    let reply = executor.drain(2, "/user/disposal", 3000).await;
                    assert!(
                        matches!(
                            reply,
                            R::Drained {
                                drained: 1,
                                aborted: 1,
                                ..
                            }
                        ),
                        "{reply:?}"
                    );
                    assert_eq!(probe.stopped.load(Ordering::SeqCst), 1);
                    probe.callback_fail.store(false, Ordering::SeqCst);
                    assert!(matches!(
                        executor.drain(4, "/user/disposal", 3000).await,
                        R::Drained {
                            drained: 1,
                            aborted: 0,
                            ..
                        }
                    ));
                }
                assert_eq!(probe.callback_drops.load(Ordering::SeqCst), 1);
            }
            "status" | "cancel_status" => {
                let mut status = Box::pin(executor.status(2, "/user/disposal"));
                std::future::poll_fn(|cx| {
                    assert!(status.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                probe.entered.notified().await;
                assert!(matches!(
                    executor.stop(3, "/user/disposal").await,
                    R::Stopped { .. }
                ));
                if mode == "cancel_status" {
                    assert!(
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(status)))
                            .is_ok(),
                        "cancelled status unwound"
                    );
                } else {
                    // Status owns the final reference for both retired instances.
                    probe.release.notify_one();
                    tokio::select! {
                        reply = &mut status => panic!("status should first reach the second member: {reply:?}"),
                        _ = probe.entered.notified() => {},
                    }
                    probe.release.notify_one();
                    assert!(
                        matches!(status.await, R::Status { req_id: 2, ref states } if states.len()==2 && states.iter().all(|s|s.state=="unavailable"))
                    );
                }
            }
            "health" => {
                let reply = executor.drain(2, "/user/disposal", 3000).await;
                assert!(
                    matches!(
                        reply,
                        R::Drained {
                            drained: 1,
                            aborted: 1,
                            ..
                        }
                    ),
                    "health panic skipped a sibling: {reply:?}"
                );
                assert_eq!(engine.actor_count(), 1);
                probe.callback_fail.store(false, Ordering::SeqCst);
                assert!(matches!(
                    executor.drain(3, "/user/disposal", 3000).await,
                    R::Drained {
                        drained: 1,
                        aborted: 0,
                        ..
                    }
                ));
            }
            "status_panic" => {
                let reply = executor.status(2, "/user/disposal").await;
                assert!(
                    matches!(reply, R::Status { ref states, .. } if states.len()==2 && states[0].state=="unavailable" && states[1].state=="running"),
                    "{reply:?}"
                );
                probe.callback_fail.store(false, Ordering::SeqCst);
                assert!(matches!(
                    executor.stop(3, "/user/disposal").await,
                    R::Stopped { .. }
                ));
            }
            _ => {
                let reply = if mode == "drain" {
                    executor.drain(2, "/user/disposal", 3000).await
                } else {
                    executor.stop(2, "/user/disposal").await
                };
                assert!(
                    matches!(reply, R::Failed { ref detail, .. } if detail.contains("reference") && detail.contains("panicked")),
                    "disposal must be reported after continuing cleanup: {reply:?}"
                );
                if mode == "mixed" {
                    assert_eq!(engine.actor_count(), 1);
                    assert!(
                        matches!(executor.status(3, "/user/disposal").await,R::Status{ref states,..} if states.len()==1 && states[0].state=="cleanup_failed")
                    );
                    probe.reject_stop.store(false, Ordering::SeqCst);
                    assert!(matches!(
                        executor.stop(4, "/user/disposal").await,
                        R::Failed { .. }
                    ));
                }
            }
        }
        assert_eq!(engine.actor_count(), 0);
        assert!(
            matches!(executor.status(5, "/user/disposal").await,R::Failed{code,..} if code==v2_err::COMPONENT_NOT_FOUND)
        );
        probe.armed.store(false, Ordering::SeqCst);
        assert!(matches!(
            executor.deploy(6, &command).await,
            R::Deployed { .. }
        ));
        for i in 0..2 {
            let actor = engine
                .get_actor_ref(&format!("/user/disposal-{i}"))
                .unwrap();
            assert_eq!(
                *actor
                    .send(Box::new(42_u64))
                    .await
                    .unwrap()
                    .downcast::<u64>()
                    .unwrap(),
                42
            );
        }
        assert!(matches!(
            executor.stop(7, "/user/disposal").await,
            R::Stopped { .. }
        ));
        engine.shutdown_internal().await.unwrap();
    }
    assert_eq!(engine.actor_count(), 0);
    assert_eq!(
        probe.stopped.load(Ordering::SeqCst),
        if owner_mode { 2 } else { 4 }
    );
    assert_eq!(
        probe.clones_dropped.load(Ordering::SeqCst),
        if owner_mode { 2 } else { 4 }
    );
}
// Baselines with two panicking destructors can abort, so isolate every case.
fn child(name: &str, mode: &str) {
    if std::env::var("PARROT_NODE_DISPOSAL_CHILD").as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(exercise(mode));
    } else {
        let output = tempfile::NamedTempFile::new().unwrap();
        let mut process = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("PARROT_NODE_DISPOSAL_CHILD", name)
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
fn node_stop_disposal_continues_and_clears_records() {
    child("node_stop_disposal_continues_and_clears_records", "stop");
}
#[test]
fn node_drain_disposal_continues_and_clears_records() {
    child("node_drain_disposal_continues_and_clears_records", "drain");
}
#[test]
fn node_final_owner_drop_isolates_reference_panics() {
    child("node_final_owner_drop_isolates_reference_panics", "owner");
}
#[test]
fn node_status_snapshot_final_disposal_does_not_unwind() {
    child(
        "node_status_snapshot_final_disposal_does_not_unwind",
        "status",
    );
}
#[test]
fn node_disposal_failure_preserves_failed_stop_for_retry() {
    child(
        "node_disposal_failure_preserves_failed_stop_for_retry",
        "mixed",
    );
}
#[test]
fn node_health_panic_does_not_skip_other_members() {
    child("node_health_panic_does_not_skip_other_members", "health");
}
#[test]
fn node_status_panic_preserves_other_member_reports() {
    child(
        "node_status_panic_preserves_other_member_reports",
        "status_panic",
    );
}

#[test]
fn node_cancelled_status_snapshot_disposal_is_isolated() {
    child(
        "node_cancelled_status_snapshot_disposal_is_isolated",
        "cancel_status",
    );
}
#[test]
fn node_legacy_rollback_disposal_cannot_skip_other_originals() {
    child(
        "node_legacy_rollback_disposal_cannot_skip_other_originals",
        "legacy",
    );
}
#[test]
fn node_reference_panic_payload_disposal_is_bounded() {
    child(
        "node_reference_panic_payload_disposal_is_bounded",
        "payload",
    );
}

macro_rules! callback_case {
    ($name:ident, $mode:literal) => {
        #[test]
        fn $name() {
            child(stringify!($name), concat!("callback:", $mode));
        }
    };
}
callback_case!(node_callback_status_ready_drop, "status_ready_drop");
callback_case!(node_callback_status_poll_drop, "status_poll_drop");
callback_case!(node_callback_status_cancel_drop, "status_cancel_drop");
callback_case!(node_callback_status_poll_logger, "status_poll_logger");
callback_case!(node_callback_metrics_poll_drop, "metrics_poll_drop");
callback_case!(node_callback_stop_ready_drop, "stop_ready_drop");
callback_case!(node_callback_stop_poll_drop, "stop_poll_drop");
callback_case!(node_callback_stop_error_display, "stop_error_display");
callback_case!(node_callback_stop_error_drop, "stop_error_drop");
callback_case!(node_callback_stop_error_both, "stop_error_both");
callback_case!(node_callback_health_poll_drop, "health_poll_drop");
callback_case!(node_callback_health_error_both, "health_error_both");
callback_case!(node_callback_health_reply_drop, "health_reply_drop");
callback_case!(node_callback_rollback_poll_drop, "rollback_poll_drop");
callback_case!(node_callback_rollback_error_both, "rollback_error_both");
callback_case!(node_callback_factory_poll_drop, "factory_poll_drop");
callback_case!(node_callback_factory_ready_drop, "factory_ready_drop");
callback_case!(node_callback_factory_error_display, "factory_error_display");
callback_case!(node_callback_factory_error_drop, "factory_error_drop");
callback_case!(node_callback_factory_error_both, "factory_error_both");
#[test]
fn node_callback_owner_logger() {
    child("node_callback_owner_logger", "owner_logger");
}
