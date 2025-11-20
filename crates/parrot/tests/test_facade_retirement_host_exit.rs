//! Facade retirement survives its host while actual mixed engines drain.
use futures::FutureExt;
use parrot::{
    actix::{ActixActorSystem, ActixContext},
    system::ParrotActorSystem,
    thread::{ThreadActorSystem, ThreadActorSystemConfig, ThreadContext},
};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    engine::{AsyncEngineRuntime, InstanceConfig, RuntimeActor, SpawnOptions},
    system::{
        ActorSystem, ActorSystemConfig, SystemError, SystemResources, SystemState, SystemStatus,
        SystemTimeouts,
    },
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct State {
    entered: AtomicUsize,
    calls: AtomicUsize,
    futures: AtomicUsize,
    actors: AtomicUsize,
    open: AtomicBool,
    stopped: AtomicBool,
    changed: Notify,
}
impl State {
    async fn wait(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.open.load(Ordering::Acquire) {
                return;
            }
            changed.await;
        }
    }
    fn release(&self) {
        self.open.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }
}
struct FutureDrop(Arc<State>);
impl Drop for FutureDrop {
    fn drop(&mut self) {
        self.0.futures.fetch_add(1, Ordering::SeqCst);
    }
}
struct ThreadProbe(Arc<State>);
struct ActixProbe(Arc<State>);
impl ActixEngineExt for ActixProbe {}
macro_rules! actor {
    ($name:ident, $context:ty) => {
        impl Drop for $name {
            fn drop(&mut self) {
                self.0.actors.fetch_add(1, Ordering::SeqCst);
            }
        }
        impl Actor for $name {
            type Config = EmptyConfig;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn use_async_handler(&self) -> bool {
                true
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move { Ok(message) })
            }
            fn before_stop<'a>(
                &'a mut self,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async move {
                    self.0.entered.fetch_add(1, Ordering::SeqCst);
                    self.0.wait().await;
                    Ok(())
                })
            }
        }
    };
}
actor!(ThreadProbe, ThreadContext<Self>);
actor!(ActixProbe, ActixContext<parrot::actix::ActixActor<Self>>);

struct Extension {
    state: Arc<State>,
    fail: bool,
}
impl AsyncEngineRuntime for Extension {
    fn spawn<'a>(
        &'a self,
        _: RuntimeActor,
        _: InstanceConfig,
        _: SpawnOptions,
    ) -> BoxedFuture<'a, Result<BoxedActorRef, SystemError>> {
        panic!("extension must not replace actual native actors")
    }
    fn get_actor<'a>(&'a self, _: &'a str) -> BoxedFuture<'a, Option<BoxedActorRef>> {
        Box::pin(async { None })
    }
    fn status(&self) -> SystemStatus {
        SystemStatus {
            state: if self.state.stopped.load(Ordering::Acquire) {
                SystemState::Stopped
            } else {
                SystemState::Running
            },
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
        self.state.calls.fetch_add(1, Ordering::SeqCst);
        let guard = FutureDrop(self.state.clone());
        Box::pin(async move {
            let _guard = guard;
            self.state.entered.fetch_add(1, Ordering::SeqCst);
            self.state.wait().await;
            self.state.stopped.store(true, Ordering::Release);
            if self.fail {
                Err(SystemError::InitializationError(
                    "extension retirement witness".into(),
                ))
            } else {
                Ok(())
            }
        })
    }
}
async fn until(mut condition: impl FnMut() -> bool) -> bool {
    tokio::time::timeout(WAIT, async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok()
}

fn contract(started: bool, outcome: u8) {
    let observer = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let host = actix::System::new();
    let states: Vec<_> = (0..3).map(|_| Arc::new(State::default())).collect();
    let (facade, thread, actix, references) = host.block_on(async {
        let facade = Arc::new(
            ParrotActorSystem::new(ActorSystemConfig {
                timeouts: SystemTimeouts {
                    system_shutdown: if outcome == 2 {
                        Duration::from_millis(150)
                    } else {
                        WAIT
                    },
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap(),
        );
        let thread = ThreadActorSystem::try_shared_with_handle(
            ThreadActorSystemConfig {
                shared_pool_size: 1,
                shared_burst_workers_max: 0,
                shutdown_timeout: WAIT,
                ..Default::default()
            },
            observer.handle().clone(),
        )
        .unwrap();
        let actix = ActixActorSystem::with_arbiter_count(1).await.unwrap();
        let a = thread
            .spawn_at(
                ThreadProbe(states[0].clone()),
                "/thread",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let b = actix
            .spawn_root_typed(ActixProbe(states[1].clone()), EmptyConfig)
            .await
            .unwrap();
        // Actual replies, rather than returned references, prove both engines serve.
        assert_eq!(
            *a.send(Box::new(17u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            17
        );
        assert_eq!(
            *b.send(Box::new(23u64))
                .await
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            23
        );
        facade
            .register_runtime("thread".into(), thread.clone(), true)
            .unwrap();
        facade
            .register_runtime("thread-alias".into(), thread.clone(), false)
            .unwrap();
        facade
            .register_actix_system("actix".into(), actix.clone(), false)
            .await
            .unwrap();
        let extension = Arc::new(Extension {
            state: states[2].clone(),
            fail: outcome == 1,
        });
        facade
            .register_runtime("extension".into(), extension.clone(), false)
            .unwrap();
        facade
            .register_runtime("extension-alias".into(), extension, false)
            .unwrap();
        if started {
            assert!(facade.shutdown_runtime().now_or_never().is_none());
            assert!(
                until(|| states
                    .iter()
                    .all(|state| state.entered.load(Ordering::SeqCst) == 1))
                .await,
                "all real shutdown paths must enter before destroying their host"
            );
        }
        (facade, thread, actix, [Box::new(a) as BoxedActorRef, b])
    });
    drop(host);
    observer.block_on(async {
        let pending = started || facade.shutdown_runtime().now_or_never().is_none();
        let entered = until(|| {
            states
                .iter()
                .all(|state| state.entered.load(Ordering::SeqCst) == 1)
        })
        .await;
        let held = states
            .iter()
            .all(|state| state.actors.load(Ordering::SeqCst) == 0)
            && states[2].futures.load(Ordering::SeqCst) == 0;
        // Always release real actors before reporting an old-production failure.
        states[0].release();
        states[1].release();
        let early = if outcome == 2 {
            Some(
                tokio::time::timeout(WAIT, facade.shutdown_runtime())
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string(),
            )
        } else {
            None
        };
        let timeout_owned = outcome != 2
            || (ActorSystem::status(facade.as_ref()).state == SystemState::ShuttingDown
                && states[2].futures.load(Ordering::SeqCst) == 0);
        states[2].release();
        let first = tokio::time::timeout(WAIT, facade.shutdown_runtime())
            .await
            .unwrap()
            .map_err(|error| error.to_string());
        let finished =
            until(|| ActorSystem::status(facade.as_ref()).state == SystemState::Stopped).await;
        thread.shutdown_internal().await.unwrap();
        actix.clone().shutdown().await.unwrap();
        assert!(
            pending && entered && held,
            "facade retirement lost its pending engine owners after host exit"
        );
        assert!(finished, "facade did not observe actual engine retirement");
        assert!(timeout_owned, "timeout discarded actual extension cleanup");
        match outcome {
            0 => {
                first.as_ref().unwrap();
            }
            1 => assert!(first.as_ref().unwrap_err().contains(
                "engine \"extension\": System initialization failed: extension retirement witness"
            )),
            _ => {
                assert!(
                    early
                        .as_ref()
                        .unwrap()
                        .contains("facade shutdown timed out; cleanup remains owned")
                );
                assert_eq!(first.as_ref().unwrap_err(), early.as_ref().unwrap());
            }
        }
        for _ in 0..2 {
            assert_eq!(
                facade
                    .shutdown_runtime()
                    .await
                    .map_err(|error| error.to_string()),
                first
            );
        }
        assert_eq!(
            states[2].calls.load(Ordering::SeqCst),
            1,
            "aliases share physical shutdown"
        );
        assert_eq!(states[2].futures.load(Ordering::SeqCst), 1);
        assert_eq!(
            ActorSystem::status(thread.as_ref()).state,
            SystemState::Stopped
        );
        assert_eq!(actix.status().state, SystemState::Stopped);
        assert_eq!(actix.status().resources.thread_count, 0);
        for state in &states[..2] {
            assert_eq!(state.actors.load(Ordering::SeqCst), 1);
        }
        for reference in references {
            assert!(reference.send(Box::new(29u64)).await.is_err());
        }
        assert!(matches!(
            facade.unregister_system("thread"),
            Err(SystemError::ShuttingDown)
        ));
    });
}
macro_rules! case {
    ($name:ident, $started:expr, $outcome:expr) => {
        #[test]
        fn $name() {
            contract($started, $outcome);
        }
    };
}
case!(
    shutdown_started_after_host_exit_drains_mixed_engines,
    false,
    0
);
case!(
    shutdown_started_before_host_exit_drains_mixed_engines,
    true,
    0
);
case!(shutdown_after_host_exit_preserves_engine_error, false, 1);
case!(shutdown_during_host_exit_preserves_engine_error, true, 1);
case!(shutdown_after_host_exit_retains_timed_out_cleanup, false, 2);
case!(shutdown_during_host_exit_retains_timed_out_cleanup, true, 2);

type DisposalRecord = (
    Option<parrot_api::trace::TraceContext>,
    std::thread::ThreadId,
);
struct CapturedInput(std::sync::mpsc::Sender<DisposalRecord>);
impl parrot_api::actor::ActorConfig for CapturedInput {}
impl Drop for CapturedInput {
    fn drop(&mut self) {
        self.0
            .send((
                parrot_api::trace::TraceContext::current(),
                std::thread::current().id(),
            ))
            .unwrap();
    }
}
struct InputActor(CapturedInput);
impl Actor for InputActor {
    type Config = CapturedInput;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        let _ = &self.0;
        Box::pin(async { Ok(message) })
    }
}

fn assert_disposals(
    receiver: &std::sync::mpsc::Receiver<DisposalRecord>,
    context: &parrot_api::trace::TraceContext,
    count: usize,
) {
    for _ in 0..count {
        let (actual, thread) = receiver.recv_timeout(WAIT).expect("owned input retired");
        assert_eq!(actual.as_ref(), Some(context), "capture trace was replaced");
        assert_ne!(thread, std::thread::current().id(), "caller ran cleanup");
    }
    assert!(
        receiver.try_recv().is_err(),
        "input disposed more than once"
    );
}

#[test]
fn stored_configuration_keeps_capture_trace_after_host_exit() {
    use parrot_api::{
        supervisor::{
            BasicDecisionFn, OneForAllStrategy, SupervisionDecision, SupervisorStrategyType,
        },
        trace::TraceContext,
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    let capture = CapturedInput(sender);
    let mut config = ActorSystemConfig::default();
    config.guardian_config.supervision_strategy =
        SupervisorStrategyType::OneForAll(OneForAllStrategy {
            decider: BasicDecisionFn::new(move |_| {
                let _ = &capture;
                SupervisionDecision::Stop
            }),
            max_restarts: 1,
            within: Duration::from_secs(60),
        });
    let original = TraceContext::root();
    let construction = original.in_scope(|| ParrotActorSystem::new(config));
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let facade = host
        .block_on(TraceContext::root().scope(construction))
        .unwrap();
    drop(host);
    TraceContext::root().in_scope(|| drop(facade));
    assert_disposals(&receiver, &original, 1);
}

#[test]
fn unpolled_creation_inputs_retire_off_caller_after_host_exit() {
    use parrot_api::trace::TraceContext;
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let facade = host
        .block_on(ParrotActorSystem::new(Default::default()))
        .unwrap();
    drop(host);
    let (sender, receiver) = std::sync::mpsc::channel();
    let original = TraceContext::root();
    let creation = original.in_scope(|| {
        facade.spawn_root_thread_in_system(
            "missing",
            InputActor(CapturedInput(sender.clone())),
            CapturedInput(sender),
        )
    });
    TraceContext::root().in_scope(|| drop(creation));
    assert_disposals(&receiver, &original, 2);
    futures::executor::block_on(facade.shutdown_runtime()).unwrap();
}

#[test]
fn empty_facade_can_shutdown_without_a_tokio_host() {
    let facade = futures::executor::block_on(ParrotActorSystem::new(Default::default())).unwrap();
    futures::executor::block_on(facade.shutdown_runtime()).unwrap();
    futures::executor::block_on(facade.shutdown_runtime()).unwrap();
    assert_eq!(
        AsyncEngineRuntime::status(&facade).state,
        SystemState::Stopped
    );
}
