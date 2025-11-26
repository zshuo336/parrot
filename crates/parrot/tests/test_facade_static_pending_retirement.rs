// SPDX-License-Identifier: AGPL-3.0-only
//! Cancellation must retain a static result's reservation until actual retirement.
use parrot::{
    system::ParrotActorSystem,
    thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    actor::{Actor, ActorState},
    address::RetirementStatus,
    context::ActorContext,
    engine::{AsyncEngineRuntime, InstanceConfig, RuntimeActor, SpawnOptions},
    message::Message,
    system::{SystemError, SystemResources, SystemState, SystemStatus},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    typed_context::TypedContext,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    entered: AtomicUsize,
    dropped: AtomicUsize,
    reference: Mutex<Option<BoxedActorRef>>,
}
impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    fn block(&self) {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let (open, timeout) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(10), |open| {
                !*open
            })
            .unwrap();
        assert!(
            *open && !timeout.timed_out(),
            "Drop barrier was not released"
        );
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct CleanupBlocker(Arc<Gate>);
impl Drop for CleanupBlocker {
    fn drop(&mut self) {
        self.0.block();
    }
}
impl Actor for CleanupBlocker {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(message) })
    }
}
struct Query;
impl Message for Query {
    type Result = ();
}
struct StaticMember(Arc<Gate>);
impl Drop for StaticMember {
    fn drop(&mut self) {
        self.0.block();
    }
}
impl ParrotTypedDispatch for StaticMember {
    type Msg = Query;
    type Reply = ();
    fn dispatch(&mut self, _: Query) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn init_with_context<'a>(
        &'a mut self,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            *self.0.reference.lock().unwrap() = context.actor_ref();
            Ok(())
        })
    }
}
impl ParrotMsgVariant<StaticMember> for Query {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(_: ()) -> ActorResult<()> {
        Ok(())
    }
}
struct PendingEngine(Arc<AtomicUsize>);
impl AsyncEngineRuntime for PendingEngine {
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            let result = std::future::pending().await;
            drop((actor, config));
            result
        })
    }
    fn get_actor<'a>(&'a self, _: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async { None })
    }
    fn status(&self) -> SystemStatus {
        SystemStatus {
            state: SystemState::Running,
            active_actors: 0,
            uptime: Duration::ZERO,
            resources: SystemResources {
                cpu_usage: 0.0,
                memory_usage: 0,
                thread_count: 0,
            },
        }
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        Box::pin(async { Ok(()) })
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual lifecycle phase did not complete");
}
async fn check(backend: usize, factory: bool) {
    let native: Arc<dyn AsyncEngineRuntime> = if backend == 3 {
        Arc::new(
            parrot::actix::ActixActorSystem::with_settings(parrot::actix::ActixSystemSettings {
                arbiter_count: 1,
                shutdown_timeout: Duration::from_millis(30),
                ..Default::default()
            })
            .await
            .unwrap(),
        )
    } else {
        ThreadActorSystem::shared(ThreadActorSystemConfig {
            shutdown_timeout: Duration::from_millis(30),
            default_scheduling_mode: match backend {
                0 => SchedulingMode::SharedPool {
                    max_messages_per_run: 32,
                },
                1 => SchedulingMode::DedicatedThread,
                2 => SchedulingMode::Sharded {
                    affinity_key: "static-retirement".into(),
                },
                _ => unreachable!(),
            },
            ..Default::default()
        })
    };
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    facade
        .register_runtime("native".into(), native, true)
        .unwrap();
    let entries = Arc::new(AtomicUsize::new(0));
    facade
        .register_runtime(
            "pending".into(),
            Arc::new(PendingEngine(entries.clone())),
            false,
        )
        .unwrap();
    let cleanup = Arc::new(Gate::default());
    let _release_cleanup = Release(cleanup.clone());
    drop(facade.spawn_on(
        "missing",
        Box::new(CleanupBlocker(cleanup.clone())),
        Box::new(()),
        Default::default(),
    ));
    until(|| cleanup.entered.load(Ordering::SeqCst) == 1).await;
    let actor = Arc::new(Gate::default());
    let _release_actor = Release(actor.clone());
    let captured = actor.clone();
    let mut creating = if factory {
        facade.spawn_typed_factory::<StaticMember, Query, _>(
            move || StaticMember(captured.clone()),
            "/static/pending",
        )
    } else {
        facade.spawn_typed::<StaticMember, Query>(StaticMember(captured), "/static/pending")
    };
    tokio::select! {
        result = &mut creating => panic!("creation escaped its pending owner cleanup: {}", result.is_err()),
        _ = until(|| actor.reference.lock().unwrap().is_some()) => {}
    }
    let reference = actor
        .reference
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .clone_boxed();
    assert!(reference.send(Box::new(Query)).await.unwrap().is::<()>());
    assert!(futures::poll!(creating.as_mut()).is_pending());
    drop(creating);
    until(|| actor.entered.load(Ordering::SeqCst) == 1).await;
    let stopped = reference.stop().await;
    assert!(matches!(
        stopped,
        Err(parrot_api::errors::ActorError::TimeoutDetail(_))
            | Err(parrot_api::errors::ActorError::Timeout)
    ));
    assert!(matches!(
        reference.retirement_status(),
        RetirementStatus::Pending
    ));
    cleanup.release();
    until(|| cleanup.dropped.load(Ordering::SeqCst) == 1).await;
    // This facade has one disposal slot and 1024 shared pending-owner reservations.
    // One must remain held by the cancelled static result, not its stop deadline.
    let mut pending = Vec::new();
    for expected in 1..=1023 {
        let mut work = facade.spawn_on(
            "pending",
            Box::new(CleanupBlocker(Arc::new(Gate {
                open: Mutex::new(true),
                ..Default::default()
            }))),
            Box::new(()),
            Default::default(),
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                assert!(futures::poll!(work.as_mut()).is_pending());
                if entries.load(Ordering::SeqCst) == expected {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pending-owner admission did not become available");
        pending.push(work);
    }
    assert_eq!(entries.load(Ordering::SeqCst), 1023);
    let mut blocked = facade.spawn_on(
        "pending",
        Box::new(CleanupBlocker(Arc::new(Gate {
            open: Mutex::new(true),
            ..Default::default()
        }))),
        Box::new(()),
        Default::default(),
    );
    let observation = tokio::time::sleep(Duration::from_millis(100));
    tokio::pin!(observation);
    loop {
        assert!(futures::poll!(blocked.as_mut()).is_pending());
        assert_eq!(
            entries.load(Ordering::SeqCst),
            1023,
            "static result released its retirement reservation before actual actor Drop"
        );
        tokio::select! { _ = &mut observation => break, _ = tokio::task::yield_now() => {} }
    }
    assert_eq!(actor.dropped.load(Ordering::SeqCst), 0);
    actor.release();
    until(|| actor.dropped.load(Ordering::SeqCst) == 1).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            assert!(futures::poll!(blocked.as_mut()).is_pending());
            if entries.load(Ordering::SeqCst) == 1024 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        reference.retirement_status(),
        RetirementStatus::Retired(_)
    ));
    drop(blocked);
    drop(pending);
    let _ = facade.shutdown_runtime().await;
}

macro_rules! cases {
    ($module:ident, $backend:expr, $test:path) => {
        mod $module {
            use super::*;
            #[$test]
            async fn cancelled_static_instance_retains_actual_retirement_reservation() {
                isolated($backend, false).await;
            }
            #[$test]
            async fn cancelled_static_factory_retains_actual_retirement_reservation() {
                isolated($backend, true).await;
            }
        }
    };
}
async fn isolated(backend: usize, factory: bool) {
    let identity = format!("{backend}-{factory}");
    if std::env::var("PARROT_STATIC_RETIREMENT_CHILD").as_deref() == Ok(&identity) {
        check(backend, factory).await;
        return;
    }
    let module = ["shared", "dedicated", "sharded", "actix_backend"][backend];
    let kind = if factory { "factory" } else { "instance" };
    let name = format!("{module}::cancelled_static_{kind}_retains_actual_retirement_reservation");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env("PARROT_STATIC_RETIREMENT_CHILD", identity)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "static retirement contract failed: {status}"
            );
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("static retirement child exceeded its watchdog");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
cases!(shared, 0, tokio::test);
cases!(dedicated, 1, tokio::test);
cases!(sharded, 2, tokio::test);
cases!(actix_backend, 3, actix::test);
