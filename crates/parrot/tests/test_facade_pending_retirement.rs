//! Unpublished outputs must outlive actual actor cleanup, including public stop expiry.
use parrot::{
    system::{ActorSystemImpl, ParrotActorSystem},
    thread::{SchedulingMode, ThreadActorSystem, ThreadActorSystemConfig},
};
use parrot_api::{
    actor::{Actor, ActorState},
    address::{ActorRef, RetirementStatus},
    context::ActorContext,
    engine::{
        AsyncEngineRuntime, AsyncFactoryRuntime, FactoryOptions, InstanceConfig, RuntimeActor,
        RuntimeFactory, SpawnOptions,
    },
    system::{SystemError, SystemStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    any::Any,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Default)]
struct State {
    gate: Mutex<bool>,
    changed: Condvar,
    entered: AtomicUsize,
    actor_drops: AtomicUsize,
    output_drops: AtomicUsize,
    stop_fault: AtomicUsize,
    observation_panics: AtomicUsize,
    reference: Mutex<Option<BoxedActorRef>>,
}
struct Release(Arc<State>);
impl Release {
    fn open(&self) {
        *self.0.gate.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        self.open();
    }
}
struct Member(Arc<State>);
impl Drop for Member {
    fn drop(&mut self) {
        self.0.entered.fetch_add(1, Ordering::SeqCst);
        let (guard, timeout) = self
            .0
            .changed
            .wait_timeout_while(
                self.0.gate.lock().unwrap(),
                Duration::from_secs(5),
                |open| !*open,
            )
            .unwrap();
        assert!(
            *guard && !timeout.timed_out(),
            "actual actor Drop gate was not released"
        );
        self.0.actor_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Actor for Member {
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
struct Output(BoxedActorRef, Arc<State>);
impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unpublished output")
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        self.1.output_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl ActorRef for Output {
    fn send<'a>(&'a self, message: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.send(message)
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        timeout: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.0.send_with_timeout(message, timeout)
    }
    fn deliver(&self, message: BoxedMessage) -> BoxedFuture<'_, ActorResult<()>> {
        self.0.deliver(message)
    }
    fn stop(&self) -> BoxedFuture<'_, ActorResult<()>> {
        Box::pin(StopOutput(self.0.stop(), self.1.clone()))
    }
    fn retirement_status(&self) -> RetirementStatus {
        if self
            .1
            .observation_panics
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            panic!("retirement observation witness");
        }
        self.0.retirement_status()
    }
    fn path(&self) -> String {
        self.0.path()
    }
    fn is_alive(&self) -> BoxedFuture<'_, bool> {
        self.0.is_alive()
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(Self(self.0.clone_boxed(), self.1.clone()))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
struct StopOutput<'a>(BoxedFuture<'a, ActorResult<()>>, Arc<State>);
impl std::future::Future for StopOutput<'_> {
    type Output = ActorResult<()>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = self.0.as_mut().poll(cx);
        if result.is_ready() && self.1.stop_fault.load(Ordering::SeqCst) == 1 {
            panic!("stop poll witness after public deadline");
        }
        result
    }
}
impl Drop for StopOutput<'_> {
    fn drop(&mut self) {
        if self.1.stop_fault.load(Ordering::SeqCst) == 2 {
            panic!("stop future disposal witness after public deadline");
        }
    }
}
struct Completed<'a>(BoxedFuture<'a, Result<BoxedActorRef, SystemError>>);
impl std::future::Future for Completed<'_> {
    type Output = Result<BoxedActorRef, SystemError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(cx)
    }
}
impl Drop for Completed<'_> {
    fn drop(&mut self) {
        panic!("completed creation future cleanup witness");
    }
}
struct Extension {
    native: Arc<dyn AsyncEngineRuntime>,
    state: Arc<State>,
}
impl Extension {
    async fn remember(&self, reference: BoxedActorRef) -> Result<BoxedActorRef, SystemError> {
        reference
            .send(Box::new(()))
            .await
            .map_err(SystemError::ActorError)?;
        *self.state.reference.lock().unwrap() = Some(reference.clone_boxed());
        Ok(Box::new(Output(reference, self.state.clone())))
    }
}
impl AsyncEngineRuntime for Extension {
    fn factory_runtime(&self) -> Option<&dyn AsyncFactoryRuntime> {
        Some(self)
    }
    fn spawn<'a>(
        &'a self,
        actor: RuntimeActor,
        config: InstanceConfig,
        options: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        Box::pin(Completed(Box::pin(async move {
            self.remember(
                AsyncEngineRuntime::spawn(self.native.as_ref(), actor, config, options).await?,
            )
            .await
        })))
    }
    fn get_actor<'a>(&'a self, path: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        AsyncEngineRuntime::get_actor(self.native.as_ref(), path)
    }
    fn status(&self) -> SystemStatus {
        AsyncEngineRuntime::status(self.native.as_ref())
    }
    fn shutdown(&self) -> BoxedFuture<'_, Result<(), SystemError>> {
        AsyncEngineRuntime::shutdown(self.native.as_ref())
    }
}
impl AsyncFactoryRuntime for Extension {
    fn spawn_factory<'a>(
        &'a self,
        factory: RuntimeFactory,
        options: FactoryOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        Box::pin(Completed(Box::pin(async move {
            self.remember(
                AsyncEngineRuntime::factory_runtime(self.native.as_ref())
                    .unwrap()
                    .spawn_factory(factory, options)
                    .await?,
            )
            .await
        })))
    }
}
async fn until(value: &AtomicUsize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while value.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
async fn check(route: usize, cancel: bool) {
    check_backend(route, cancel, 0).await;
}
async fn check_backend(route: usize, cancel: bool, backend: usize) {
    check_fault(route, cancel, backend, 0).await;
}
async fn check_fault(route: usize, cancel: bool, backend: usize, fault: usize) {
    let state = Arc::new(State::default());
    state.stop_fault.store(fault, Ordering::SeqCst);
    state
        .observation_panics
        .store(usize::from(fault == 3), Ordering::SeqCst);
    let release = Release(state.clone());
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
                    affinity_key: "late-retirement".into(),
                },
                _ => unreachable!(),
            },
            ..Default::default()
        })
    };
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let extension = Arc::new(Extension {
        native,
        state: state.clone(),
    });
    facade
        .register_runtime("external".into(), extension.clone(), true)
        .unwrap();
    let mut work: BoxedFuture<'_, Result<BoxedActorRef, SystemError>> = match route {
        0 => facade.spawn_on(
            "external",
            Box::new(Member(state.clone())),
            Box::new(()),
            SpawnOptions::at("/late/member"),
        ),
        1 => {
            let actor_state = state.clone();
            facade.spawn_factory_on(
                "external",
                Box::new(move || Box::new(Member(actor_state.clone()))),
                FactoryOptions {
                    spawn: SpawnOptions::at("/late/member"),
                    ..Default::default()
                },
            )
        }
        2 => Box::pin(async {
            ActorSystemImpl::Runtime(extension.clone())
                .spawn_root_boxed(Box::new(Member(state.clone())), Box::new(()))
                .await
        }),
        _ => unreachable!(),
    };
    tokio::select! {
        result = &mut work => panic!("creation returned before actor Drop entered: {}", result.is_err()),
        _ = until(&state.entered) => {}
    }
    let reference = state
        .reference
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .clone_boxed();
    if cancel {
        drop(work);
    } else {
        let result = reference.stop().await;
        assert!(matches!(
            result,
            Err(parrot_api::errors::ActorError::TimeoutDetail(_))
                | Err(parrot_api::errors::ActorError::Timeout)
        ));
        tokio::select! {
            result = &mut work => panic!(
                "creation cleanup returned after public stop but before actual retirement: {}",
                result.is_err()
            ),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        assert_eq!(state.actor_drops.load(Ordering::SeqCst), 0);
        assert_eq!(
            state.output_drops.load(Ordering::SeqCst),
            0,
            "creation released its unpublished output before actual actor retirement"
        );
        release.open();
        assert!(work.await.is_err());
        until(&state.actor_drops).await;
        until(&state.output_drops).await;
        let _ = facade.shutdown_runtime().await;
        return;
    }
    let result = reference.stop().await;
    assert!(matches!(
        result,
        Err(parrot_api::errors::ActorError::TimeoutDetail(_))
            | Err(parrot_api::errors::ActorError::Timeout)
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(state.actor_drops.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.output_drops.load(Ordering::SeqCst),
        0,
        "cancelled creation dropped its output while actual actor retirement was pending"
    );
    release.open();
    until(&state.actor_drops).await;
    until(&state.output_drops).await;
    let _ = facade.shutdown_runtime().await;
}
#[tokio::test]
async fn instance_cleanup_waits_past_public_deadline_for_actual_actor_drop() {
    check(0, false).await;
}
#[tokio::test]
async fn factory_cleanup_waits_past_public_deadline_for_actual_actor_drop() {
    check(1, false).await;
}
#[tokio::test]
async fn direct_runtime_cleanup_waits_past_public_deadline_for_actual_actor_drop() {
    check(2, false).await;
}
#[tokio::test]
async fn cancelled_creation_retains_its_output_until_actual_actor_drop() {
    check(0, true).await;
}

#[tokio::test]
async fn cancelled_factory_retains_its_output_until_actual_actor_drop() {
    check(1, true).await;
}
#[tokio::test]
async fn cancelled_direct_runtime_retains_its_output_until_actual_actor_drop() {
    check(2, true).await;
}

macro_rules! backend_cases {
    ($name:ident, $backend:expr, $test:path) => {
        mod $name {
            use super::*;
            #[$test]
            async fn instance_waits_for_actual_drop() {
                check_backend(0, false, $backend).await;
            }
            #[$test]
            async fn factory_waits_for_actual_drop() {
                check_backend(1, false, $backend).await;
            }
            #[$test]
            async fn direct_runtime_waits_for_actual_drop() {
                check_backend(2, false, $backend).await;
            }
            #[$test]
            async fn cancelled_instance_retains_actual_owner() {
                check_backend(0, true, $backend).await;
            }
            #[$test]
            async fn cancelled_factory_retains_actual_owner() {
                check_backend(1, true, $backend).await;
            }
            #[$test]
            async fn cancelled_direct_runtime_retains_actual_owner() {
                check_backend(2, true, $backend).await;
            }
        }
    };
}
backend_cases!(dedicated, 1, tokio::test);
backend_cases!(sharded, 2, tokio::test);
backend_cases!(actix, 3, ::actix::test);

#[tokio::test]
async fn stop_poll_failure_retains_actual_retirement_owner() {
    check_fault(0, false, 0, 1).await;
}
#[tokio::test]
async fn stop_future_drop_failure_retains_actual_retirement_owner() {
    check_fault(0, false, 0, 2).await;
}
#[tokio::test]
async fn observation_panic_does_not_release_unpublished_actor() {
    check_fault(0, false, 0, 3).await;
}
