//! Public I/O results retain their first error across independent cleanup failures.
use futures::FutureExt;
use parrot::system::ParrotActorSystem;
use parrot_api::{
    engine::{AsyncEngineRuntime, AsyncIoRuntime, InstanceConfig, RuntimeActor, SpawnOptions},
    errors::ActorError,
    system::{SystemError, SystemResources, SystemState, SystemStatus},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc, Weak,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Clone, Copy)]
enum Outcome {
    Error,
    CustomError,
    Success,
    PollPanic,
    Pending,
}
struct Original;
struct Recursive;
impl Drop for Recursive {
    fn drop(&mut self) {
        std::panic::panic_any(Recursive);
    }
}
struct State {
    future_drops: AtomicUsize,
    input_drops: AtomicUsize,
    reply_drops: AtomicUsize,
    engine_drops: AtomicUsize,
    facade: Weak<ParrotActorSystem>,
    remove_self: bool,
    engine_panics: bool,
    error: std::sync::Mutex<Option<ActorError>>,
    error_displays: AtomicUsize,
    error_drops: AtomicUsize,
}
#[derive(Debug)]
struct CustomError(Arc<State>);
impl std::fmt::Display for CustomError {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.error_displays.fetch_add(1, Ordering::SeqCst);
        panic!("I/O business error must not be formatted during cleanup")
    }
}
impl std::error::Error for CustomError {}
impl Drop for CustomError {
    fn drop(&mut self) {
        self.0.error_drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("I/O ownership state")
    }
}
struct Input(Arc<State>);
impl Drop for Input {
    fn drop(&mut self) {
        self.0.input_drops.fetch_add(1, Ordering::SeqCst);
    }
}
struct Reply(Arc<State>, bool);
impl Drop for Reply {
    fn drop(&mut self) {
        self.0.reply_drops.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            std::panic::panic_any(Recursive);
        }
    }
}
struct Work {
    state: Arc<State>,
    _input: BoxedFuture<'static, ActorResult<BoxedMessage>>,
    outcome: Outcome,
    future_panics: bool,
    reply_panics: bool,
}
impl std::future::Future for Work {
    type Output = ActorResult<BoxedMessage>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        if self.state.remove_self {
            let facade = self.state.facade.upgrade().unwrap();
            drop(facade.unregister_system("io").unwrap());
        }
        match self.outcome {
            Outcome::Error => Poll::Ready(Err(ActorError::MessageHandlingError(
                "original I/O error".into(),
            ))),
            Outcome::CustomError => {
                Poll::Ready(Err(self.state.error.lock().unwrap().take().unwrap()))
            }
            Outcome::Success => {
                Poll::Ready(Ok(Box::new(Reply(self.state.clone(), self.reply_panics))))
            }
            Outcome::PollPanic => std::panic::panic_any(Original),
            Outcome::Pending => Poll::Pending,
        }
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        self.state.future_drops.fetch_add(1, Ordering::SeqCst);
        if self.future_panics {
            std::panic::panic_any(Recursive);
        }
    }
}
struct Extension {
    state: Arc<State>,
    outcome: Outcome,
    future_panics: bool,
    reply_panics: bool,
}
impl Drop for Extension {
    fn drop(&mut self) {
        self.state.engine_drops.fetch_add(1, Ordering::SeqCst);
        if self.state.engine_panics {
            std::panic::panic_any(Recursive);
        }
    }
}
impl AsyncEngineRuntime for Extension {
    fn io_runtime(&self) -> Option<&dyn AsyncIoRuntime> {
        Some(self)
    }
    fn spawn<'a>(
        &'a self,
        _: RuntimeActor,
        _: InstanceConfig,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        Box::pin(async { Err(SystemError::ShuttingDown) })
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
impl AsyncIoRuntime for Extension {
    fn run_io(
        &self,
        input: BoxedFuture<'static, ActorResult<BoxedMessage>>,
    ) -> BoxedFuture<'static, ActorResult<BoxedMessage>> {
        Box::pin(Work {
            state: self.state.clone(),
            _input: input,
            outcome: self.outcome,
            future_panics: self.future_panics,
            reply_panics: self.reply_panics,
        })
    }
}
async fn setup(
    outcome: Outcome,
    future_panics: bool,
    reply_panics: bool,
    remove_self: bool,
) -> (Arc<ParrotActorSystem>, Arc<State>) {
    let facade = Arc::new(ParrotActorSystem::new(Default::default()).await.unwrap());
    let state = Arc::new(State {
        future_drops: AtomicUsize::new(0),
        input_drops: AtomicUsize::new(0),
        reply_drops: AtomicUsize::new(0),
        engine_drops: AtomicUsize::new(0),
        facade: Arc::downgrade(&facade),
        remove_self,
        engine_panics: remove_self,
        error: std::sync::Mutex::new(None),
        error_displays: AtomicUsize::new(0),
        error_drops: AtomicUsize::new(0),
    });
    facade
        .register_runtime(
            "io".into(),
            Arc::new(Extension {
                state: state.clone(),
                outcome,
                future_panics,
                reply_panics,
            }),
            true,
        )
        .unwrap();
    (facade, state)
}
fn operation(
    facade: &ParrotActorSystem,
    state: Arc<State>,
    named: bool,
) -> BoxedFuture<'static, ActorResult<BoxedMessage>> {
    let input = Input(state);
    let future = Box::pin(async move {
        let _input = input;
        std::future::pending::<ActorResult<BoxedMessage>>().await
    });
    if named {
        facade.run_io_on("io", future)
    } else {
        AsyncIoRuntime::run_io(facade, future)
    }
}
async fn wait(value: &AtomicUsize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while value.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn check_inputs(state: &State) {
    assert_eq!(state.future_drops.load(Ordering::SeqCst), 1);
    assert_eq!(state.input_drops.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn original_io_business_error_survives_recursive_future_drop_panic() {
    for named in [false, true] {
        let (facade, state) = setup(Outcome::Error, true, false, false).await;
        let result = operation(&facade, state.clone(), named).await;
        assert!(
            matches!(result, Err(ActorError::MessageHandlingError(text)) if text == "original I/O error")
        );
        check_inputs(&state);
        facade.shutdown_runtime().await.unwrap();
    }
}
#[tokio::test]
async fn successful_io_reply_is_retired_before_disposal_error_returns() {
    for named in [false, true] {
        for reply_panics in [false, true] {
            let (facade, state) = setup(Outcome::Success, true, reply_panics, false).await;
            assert!(operation(&facade, state.clone(), named).await.is_err());
            assert_eq!(state.reply_drops.load(Ordering::SeqCst), 1);
            check_inputs(&state);
            facade.shutdown_runtime().await.unwrap();
        }
    }
}
#[tokio::test]
async fn io_poll_panic_identity_survives_recursive_future_drop_panic() {
    for named in [false, true] {
        let (facade, state) = setup(Outcome::PollPanic, true, false, false).await;
        let payload = AssertUnwindSafe(operation(&facade, state.clone(), named))
            .catch_unwind()
            .await
            .unwrap_err();
        assert!(payload.is::<Original>());
        check_inputs(&state);
        facade.shutdown_runtime().await.unwrap();
    }
}
#[tokio::test]
async fn cancelled_io_future_disposes_its_work_and_original_input() {
    for named in [false, true] {
        let (facade, state) = setup(Outcome::Pending, true, false, false).await;
        let mut future = operation(&facade, state.clone(), named);
        assert!(futures::poll!(future.as_mut()).is_pending());
        assert!(std::panic::catch_unwind(AssertUnwindSafe(|| drop(future))).is_ok());
        wait(&state.input_drops).await;
        check_inputs(&state);
        facade.shutdown_runtime().await.unwrap();
    }
}
#[tokio::test]
async fn healthy_io_result_keeps_the_original_error_and_delivers_reply_ownership() {
    for named in [false, true] {
        for outcome in [Outcome::Error, Outcome::Success] {
            let (facade, state) = setup(outcome, false, false, false).await;
            let result = operation(&facade, state.clone(), named).await;
            match outcome {
                Outcome::Error => assert!(
                    matches!(result, Err(ActorError::MessageHandlingError(text)) if text == "original I/O error")
                ),
                Outcome::Success => {
                    assert_eq!(state.reply_drops.load(Ordering::SeqCst), 0);
                    drop(result.unwrap().downcast::<Reply>().unwrap());
                    assert_eq!(state.reply_drops.load(Ordering::SeqCst), 1);
                }
                _ => unreachable!(),
            }
            check_inputs(&state);
            facade.shutdown_runtime().await.unwrap();
        }
    }
}
#[tokio::test]
async fn io_original_error_survives_future_and_last_engine_disposal_failure() {
    for named in [false, true] {
        let (facade, state) = setup(Outcome::Error, true, false, true).await;
        let result = operation(&facade, state.clone(), named).await;
        assert!(
            matches!(result, Err(ActorError::MessageHandlingError(text)) if text == "original I/O error")
        );
        wait(&state.engine_drops).await;
        check_inputs(&state);
        assert!(facade.list_registered_systems().unwrap().is_empty());
        facade.shutdown_runtime().await.unwrap();
    }
}

#[tokio::test]
async fn custom_io_error_identity_survives_future_and_last_engine_cleanup_without_formatting() {
    for named in [false, true] {
        let (facade, state) = setup(Outcome::CustomError, true, false, true).await;
        let error = anyhow::Error::new(CustomError(state.clone()));
        let identity = error.downcast_ref::<CustomError>().unwrap() as *const CustomError as usize;
        *state.error.lock().unwrap() = Some(ActorError::Other(error));
        let Err(ActorError::Other(error)) = operation(&facade, state.clone(), named).await else {
            panic!("original custom I/O error was replaced");
        };
        assert_eq!(
            error.downcast_ref::<CustomError>().unwrap() as *const CustomError as usize,
            identity
        );
        assert_eq!(state.error_drops.load(Ordering::SeqCst), 0);
        assert_eq!(state.error_displays.load(Ordering::SeqCst), 0);
        wait(&state.engine_drops).await;
        check_inputs(&state);
        drop(error);
        assert_eq!(state.error_drops.load(Ordering::SeqCst), 1);
        facade.shutdown_runtime().await.unwrap();
    }
}
