//! Real dynamic actor entry points, source backpressure and callback retirement.
use super::*;
use futures::Stream;
use parrot_api::stream::{StreamHandler, StreamRegistryExt};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

struct State {
    stage: usize,
    bad_drop: bool,
    custom: bool,
    entered: [AtomicUsize; 4],
    cancelled: AtomicUsize,
    sources: AtomicUsize,
    handlers: AtomicUsize,
    actors: AtomicUsize,
    builds: AtomicUsize,
    polls: AtomicUsize,
}
impl State {
    async fn callback(self: &Arc<Self>, stage: usize) {
        self.entered[stage].fetch_add(1, Ordering::SeqCst);
        if stage == self.stage || (self.stage == 4 && (stage == 1 || stage == 3)) {
            let _work = PendingWork(self.clone());
            std::future::pending::<()>().await;
        }
    }
}
struct PendingWork(Arc<State>);
impl Drop for PendingWork {
    fn drop(&mut self) {
        self.0.cancelled.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.0.bad_drop,
            "dynamic stream cancellation disposal witness"
        );
    }
}
struct Source(Arc<State>, bool);
impl Stream for Source {
    type Item = u32;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<u32>> {
        self.0.polls.fetch_add(1, Ordering::SeqCst);
        assert_ne!(self.0.stage, 3, "dynamic stream source failure witness");
        Poll::Ready(if std::mem::replace(&mut self.1, true) {
            None
        } else {
            Some(1)
        })
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.0.sources.fetch_add(1, Ordering::SeqCst);
    }
}
struct Custom(Arc<State>);
#[async_trait::async_trait]
impl StreamHandler<Source, dyn ActorContext> for Custom {
    async fn started(&mut self, _: &mut (dyn ActorContext + 'static)) {
        self.0.callback(0).await;
    }
    async fn handle(&mut self, _: u32, _: &mut (dyn ActorContext + 'static)) {
        self.0.callback(1).await;
    }
    async fn finished(&mut self, _: &mut (dyn ActorContext + 'static)) {
        self.0.callback(2).await;
    }
    async fn handle_error(&mut self, _: ActorError, _: &mut (dyn ActorContext + 'static)) {
        self.0.callback(3).await;
    }
}
impl Drop for Custom {
    fn drop(&mut self) {
        self.0.handlers.fetch_add(1, Ordering::SeqCst);
    }
}
enum Command {
    Register,
    Read,
}
struct Owner(Arc<State>);
impl Actor for Owner {
    type Config = Box<dyn Any + Send>;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        command: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            match *command.downcast::<Command>().unwrap() {
                Command::Register => {
                    let source = Source(self.0.clone(), false);
                    if self.0.custom {
                        ctx.stream_registry()
                            .add_stream_with_handler::<_, _, dyn ActorContext>(
                                source,
                                Custom(self.0.clone()),
                            )?;
                    } else {
                        ctx.stream_registry().add_stream(source)?;
                    }
                }
                Command::Read => {}
            }
            Ok(Box::new(self.0.cancelled.load(Ordering::SeqCst)) as BoxedMessage)
        })
    }
    fn stream_started<'a>(
        &'a mut self,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.callback(0).await;
            Ok(())
        })
    }
    fn handle_stream<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            self.0.callback(1).await;
            Ok(Box::new(()) as BoxedMessage)
        })
    }
    fn stream_finished<'a>(
        &'a mut self,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0.callback(2).await;
            Ok(())
        })
    }
    fn stream_error<'a>(
        &'a mut self,
        error: ActorError,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            if self.0.stage != 3 {
                timeout::<()>(Err(error));
                assert_eq!(
                    self.0.cancelled.load(Ordering::SeqCst),
                    1,
                    "error hook ran before actual cancellation"
                );
            }
            self.0.callback(3).await;
            Ok(())
        })
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.0.actors.fetch_add(1, Ordering::SeqCst);
    }
}
async fn exercise(mode: usize, custom: bool, stage: usize, bad_drop: bool, factory: bool) {
    let system = if mode < 2 {
        System::new(mode == 0).await
    } else {
        System::Thread(
            ThreadActorSystem::with_guardian_and_timeouts(
                ThreadActorSystemConfig {
                    default_scheduling_mode: if mode == 2 {
                        crate::thread::SchedulingMode::DedicatedThread
                    } else {
                        crate::thread::SchedulingMode::Sharded {
                            affinity_key: "stream-timeout".into(),
                        }
                    },
                    shared_pool_size: 2,
                    shared_burst_workers_max: 0,
                    ..Default::default()
                },
                guardian(),
                timeouts(),
            )
            .await
            .unwrap(),
        )
    };
    let state = Arc::new(State {
        stage,
        custom,
        bad_drop,
        entered: std::array::from_fn(|_| AtomicUsize::new(0)),
        cancelled: AtomicUsize::new(0),
        sources: AtomicUsize::new(0),
        handlers: AtomicUsize::new(0),
        actors: AtomicUsize::new(0),
        builds: AtomicUsize::new(0),
        polls: AtomicUsize::new(0),
    });
    let source = state.clone();
    let make = move || {
        source.builds.fetch_add(1, Ordering::SeqCst);
        Box::new(Owner(source.clone()))
            as Box<dyn Actor<Config = Box<dyn Any + Send>, Context = dyn ActorContext>>
    };
    let actor = if factory {
        system
            .runtime()
            .factory_runtime()
            .unwrap()
            .spawn_factory(
                Box::new(make),
                FactoryOptions {
                    spawn: SpawnOptions::at("/dynamic-stream-timeout"),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
    } else {
        system
            .runtime()
            .spawn(
                make(),
                Box::new(()),
                SpawnOptions::at("/dynamic-stream-timeout"),
            )
            .await
            .unwrap()
    };
    actor.send(Box::new(Command::Register)).await.unwrap();
    until(|| {
        state.sources.load(Ordering::SeqCst) == 1
            && state.handlers.load(Ordering::SeqCst) == usize::from(custom)
    })
    .await;
    let reply = tokio::time::timeout(WAIT, actor.send(Box::new(Command::Read)))
        .await
        .unwrap();
    if bad_drop {
        assert!(reply.is_err(), "failed cancellation reused the same actor");
    } else {
        assert_eq!(
            *reply.unwrap().downcast::<usize>().unwrap(),
            if stage == 4 { 2 } else { 1 },
            "timeout lost actor state"
        );
    }
    assert_eq!(
        state.cancelled.load(Ordering::SeqCst),
        if stage == 4 { 2 } else { 1 },
        "future was not retired exactly once"
    );
    assert_eq!(
        state.entered[if stage == 4 { 1 } else { stage }].load(Ordering::SeqCst),
        1
    );
    if stage != 3 {
        assert_eq!(
            state.entered[3].load(Ordering::SeqCst),
            usize::from(!custom && !bad_drop),
            "unexpected error callback after timeout"
        );
    }
    if stage == 0 && (custom || bad_drop) {
        assert_eq!(
            state.polls.load(Ordering::SeqCst),
            0,
            "source advanced after failed start"
        );
    }
    if stage == 1 && (custom || bad_drop) {
        assert_eq!(
            state.polls.load(Ordering::SeqCst),
            1,
            "source advanced without item acknowledgement"
        );
        assert_eq!(
            state.entered[2].load(Ordering::SeqCst),
            0,
            "failed item fabricated completion"
        );
    }
    system.shutdown(custom || stage == 3 || bad_drop).await;
    assert_eq!(state.actors.load(Ordering::SeqCst), 1);
    assert_eq!(
        state.builds.load(Ordering::SeqCst),
        1,
        "timed out callback was reconstructed"
    );
}
macro_rules! cases {
    ($module:ident, $mode:expr, $custom:expr, $factory:expr) => {
        mod $module {
            use super::*;
            #[actix::test]
            async fn started() {
                exercise($mode, $custom, 0, false, $factory).await;
            }
            #[actix::test]
            async fn item() {
                exercise($mode, $custom, 1, false, $factory).await;
            }
            #[actix::test]
            async fn finished() {
                exercise($mode, $custom, 2, false, $factory).await;
            }
            #[actix::test]
            async fn error() {
                exercise($mode, $custom, 3, false, $factory).await;
            }
            #[actix::test]
            async fn failed_start_cancellation() {
                exercise($mode, $custom, 0, true, $factory).await;
            }
            #[actix::test]
            async fn failed_item_cancellation() {
                exercise($mode, $custom, 1, true, $factory).await;
            }
            #[actix::test]
            async fn failed_finish_cancellation() {
                exercise($mode, $custom, 2, true, $factory).await;
            }
            #[actix::test]
            async fn failed_error_cancellation() {
                exercise($mode, $custom, 3, true, $factory).await;
            }
        }
    };
}
cases!(thread_default_instance, 1, false, false);
cases!(thread_custom_instance, 1, true, false);
cases!(actix_default_instance, 0, false, false);
cases!(actix_custom_instance, 0, true, false);
cases!(thread_default_factory, 1, false, true);
cases!(thread_custom_factory, 1, true, true);
cases!(actix_default_factory, 0, false, true);
cases!(actix_custom_factory, 0, true, true);
cases!(dedicated_default_instance, 2, false, false);
cases!(dedicated_custom_instance, 2, true, false);
cases!(dedicated_default_factory, 2, false, true);
cases!(dedicated_custom_factory, 2, true, true);
cases!(sharded_default_instance, 3, false, false);
cases!(sharded_custom_instance, 3, true, false);
cases!(sharded_default_factory, 3, false, true);
cases!(sharded_custom_factory, 3, true, true);

#[actix::test]
async fn thread_instance_error_hook_has_its_own_budget() {
    exercise(1, false, 4, false, false).await;
}
#[actix::test]
async fn thread_factory_error_hook_has_its_own_budget() {
    exercise(1, false, 4, false, true).await;
}
#[actix::test]
async fn actix_instance_error_hook_has_its_own_budget() {
    exercise(0, false, 4, false, false).await;
}
#[actix::test]
async fn actix_factory_error_hook_has_its_own_budget() {
    exercise(0, false, 4, false, true).await;
}

#[actix::test]
async fn dedicated_instance_error_hook_has_its_own_budget() {
    exercise(2, false, 4, false, false).await;
}
#[actix::test]
async fn dedicated_factory_error_hook_has_its_own_budget() {
    exercise(2, false, 4, false, true).await;
}
#[actix::test]
async fn sharded_instance_error_hook_has_its_own_budget() {
    exercise(3, false, 4, false, false).await;
}
#[actix::test]
async fn sharded_factory_error_hook_has_its_own_budget() {
    exercise(3, false, 4, false, true).await;
}
