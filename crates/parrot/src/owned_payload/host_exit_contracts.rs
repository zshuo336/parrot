use super::*;
use crate::{
    thread::{ThreadActorSystem, ThreadActorSystemConfig, tests_support::DummyActor},
    typed_context::{TypedContext, TypedContextExt},
};
use futures::FutureExt;
use parrot_api::{
    Message,
    actor::{Actor, ActorState, EmptyConfig},
    context::ActorContext,
    engine::{AsyncCohortRuntime, AsyncRootCohort, SpawnOptions},
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    system::ActorSystem,
    trace::{self, TraceContext},
    typed::{ParrotMsgVariant, ParrotTypedDispatch},
    types::BoxedMessage,
};
use std::{
    cell::Cell,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(3);

struct Probe {
    entered: mpsc::Sender<(std::thread::ThreadId, Option<TraceContext>)>,
    release: mpsc::Receiver<()>,
    drops: Arc<AtomicUsize>,
    // Accepted messages and typed actors must remain Send-only.
    _not_sync: Cell<()>,
}
impl Clone for Probe {
    fn clone(&self) -> Self {
        panic!("an unpolled request must never clone its input");
    }
}
impl Message for Probe {
    type Result = ();
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.entered
            .send((std::thread::current().id(), TraceContext::current()))
            .unwrap();
        self.release
            .recv_timeout(WAIT * 3)
            .expect("release actual input disposal before joining its owner");
        self.drops.fetch_add(1, Ordering::SeqCst);
        panic!("host-exit input disposal witness");
    }
}

struct Context;
impl Message for Context {
    type Result = TypedContext;
}
struct Parent;
impl ParrotMsgVariant<Parent> for Context {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(context: TypedContext) -> ActorResult<TypedContext> {
        Ok(context)
    }
}
impl ParrotTypedDispatch for Parent {
    type Msg = Context;
    type Reply = TypedContext;
    fn dispatch(&mut self, _: Context) -> BoxedFuture<'_, ActorResult<TypedContext>> {
        unreachable!("parent needs its managed context");
    }
    fn dispatch_with_context<'a>(
        &'a mut self,
        _: Context,
        context: &'a TypedContext,
    ) -> BoxedFuture<'a, ActorResult<TypedContext>> {
        Box::pin(async move { Ok(context.clone()) })
    }
}
struct Child(Probe);
struct Ping;
impl Message for Ping {
    type Result = ();
}
impl ParrotMsgVariant<Child> for Ping {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: ()) -> ActorResult<()> {
        Ok(reply)
    }
}
impl ParrotTypedDispatch for Child {
    type Msg = Ping;
    type Reply = ();
    fn dispatch(&mut self, _: Ping) -> BoxedFuture<'_, ActorResult<()>> {
        let _ = &self.0;
        unreachable!("an unpolled child must never receive messages");
    }
}

struct Neutral(Option<Mutex<Probe>>);
impl Actor for Neutral {
    type Config = BoxedMessage;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        let _ = &self.0;
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        unreachable!("an unpolled cohort request must never initialize an actor");
    }
    fn receive_message<'a>(
        &'a mut self,
        _: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        unreachable!("an unpolled cohort request must never dispatch");
    }
}

fn strategy(probe: Probe) -> SupervisorStrategyType {
    let capture = Mutex::new(probe);
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| {
            let _ = &*capture.lock().unwrap();
            SupervisionDecision::Stop
        }),
    })
}

fn check(case: u8) {
    let (entered, receive_entered) = mpsc::channel();
    let (release, receive_release) = mpsc::channel();
    let (returned, receive_returned) = mpsc::channel();
    let (resume, receive_resume) = mpsc::channel();
    let drops = Arc::new(AtomicUsize::new(0));
    let probe = Probe {
        entered,
        release: receive_release,
        drops: drops.clone(),
        _not_sync: Cell::new(()),
    };
    let expected = TraceContext::root();
    let context = expected.clone();
    let owner = std::thread::spawn(move || {
        let host = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let system = host.block_on(async {
            ThreadActorSystem::shared(ThreadActorSystemConfig {
                shared_pool_size: 1,
                shared_burst_workers_max: 0,
                ..Default::default()
            })
        });
        let reference = (case == 2 || case == 3).then(|| {
            host.block_on(system.spawn_typed_with_config(
                DummyActor,
                EmptyConfig,
                Default::default(),
            ))
            .unwrap()
        });
        let cohort: Option<Arc<dyn AsyncRootCohort>> = (5..=7).contains(&case).then(|| {
            host.block_on(AsyncCohortRuntime::root_cohort(
                &*system,
                Default::default(),
            ))
            .unwrap()
        });
        let parent = (case >= 8).then(|| {
            host.block_on(async {
                let parent = system
                    .spawn_typed::<Parent, Context>(Parent, "/parent")
                    .await
                    .unwrap();
                parent.ask(Context).await.unwrap()
            })
        });
        let slots = system.cleanup_slots.clone();
        let request = context.in_scope(|| -> BoxedFuture<'_, ()> {
            match case {
                0 => Box::pin(system.broadcast_message(Box::new(probe)).map(|_| ())),
                1 => Box::pin(system.broadcast(probe).map(|_| ())),
                2 => Box::pin(reference.as_ref().unwrap().ask(Box::new(probe)).map(|_| ())),
                3 => Box::pin(
                    reference
                        .as_ref()
                        .unwrap()
                        .send_with_strategy(
                            Box::new(probe),
                            crate::thread::BackpressureStrategy::Block,
                        )
                        .map(|_| ()),
                ),
                4 => {
                    Box::pin(AsyncCohortRuntime::root_cohort(&*system, strategy(probe)).map(|_| ()))
                }
                5 => Box::pin(
                    cohort
                        .as_ref()
                        .unwrap()
                        .spawn(
                            Box::new(Neutral(Some(Mutex::new(probe)))),
                            Box::new(()),
                            SpawnOptions::default(),
                        )
                        .map(|_| ()),
                ),
                6 => Box::pin(
                    cohort
                        .as_ref()
                        .unwrap()
                        .spawn(
                            Box::new(Neutral(None)),
                            Box::new(probe),
                            SpawnOptions::default(),
                        )
                        .map(|_| ()),
                ),
                7 => {
                    let capture = Mutex::new(probe);
                    Box::pin(
                        cohort
                            .as_ref()
                            .unwrap()
                            .spawn_factory(
                                Box::new(move || {
                                    let _ = &*capture.lock().unwrap();
                                    panic!("an unpolled cohort must never invoke its constructor");
                                }),
                                SpawnOptions::default(),
                            )
                            .map(|_| ()),
                    )
                }
                8 => Box::pin(
                    parent
                        .as_ref()
                        .unwrap()
                        .spawn_typed_child::<Child, Ping>(Child(probe))
                        .map(|_| ()),
                ),
                9 => {
                    let capture = Mutex::new(probe);
                    Box::pin(
                        parent
                            .as_ref()
                            .unwrap()
                            .spawn_typed_child_factory::<Child, Ping, _>(move || {
                                let _ = &*capture.lock().unwrap();
                                panic!("an unpolled child must never invoke its constructor");
                            })
                            .map(|_| ()),
                    )
                }
                _ => unreachable!(),
            }
        });
        // The input is still caller-owned when its original host exits.
        drop(host);
        let thread = std::thread::current().id();
        trace::in_scope(Some(TraceContext::root()), || drop(request));
        assert!(TraceContext::current().is_none());
        assert!(!DisposalScope::active());
        returned.send(thread).unwrap();
        receive_resume.recv_timeout(WAIT * 3).unwrap();
        let observer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        observer.block_on(async {
            if let Err(error) = system.shutdown_internal().await {
                // Host exit cancels the scheduler and the live typed parent.
                // If its lifecycle observer has not removed the parent yet,
                // shutdown must also preserve that parent's cancellation.
                let scheduler = "1 shared cleanup failure(s): shared task unexpectedly cancelled";
                let parent = format!(
                    "2 shutdown failures: Failed during shutdown: {scheduler}; Actor creation failed: typed actor /parent execution failed without a disposal receipt: Err(JoinError::Cancelled(Id("
                );
                assert!(
                    matches!(error, crate::thread::error::SystemError::ShutdownError(ref text)
                    if text == scheduler || (case >= 8 && text.strip_prefix(&parent)
                        .and_then(|id| id.strip_suffix(")))"))
                        .is_some_and(|id| id.parse::<u64>().is_ok_and(|id| id > 0)))),
                    "{error}"
                );
            }
            tokio::time::timeout(WAIT, async {
                while pending_cleanup_slots(&slots).available_permits()
                    != MAX_PENDING_CLEANUPS_PER_SLOT
                    || global_pending_cleanup_slots().available_permits()
                        != MAX_PENDING_CLEANUPS_GLOBAL
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("real disposal must restore all pending-owner budgets");
        });
    });
    let actual = receive_entered.recv_timeout(WAIT);
    let returned_before_release = receive_returned.recv_timeout(WAIT);
    // Always release and join before asserting the historical failure.
    let _ = release.send(());
    let _ = resume.send(());
    let joined = owner.join();
    assert!(joined.is_ok(), "input owner must isolate destructor panic");
    let (disposal_thread, disposal_context) = actual.expect("input must actually enter disposal");
    let caller_thread = returned_before_release
        .expect("unpolled cancellation blocked on original-host input disposal");
    assert_ne!(
        disposal_thread, caller_thread,
        "user disposal ran on the cancelling caller"
    );
    assert_eq!(disposal_context, Some(expected));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

macro_rules! contract {
    ($name:ident, $case:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!("owned_payload::host_exit_contracts::", stringify!($name)),
                || check($case),
            );
        }
    };
}
contract!(native_broadcast_cancellation_survives_original_host_exit, 0);
contract!(trait_broadcast_cancellation_survives_original_host_exit, 1);
contract!(address_ask_cancellation_survives_original_host_exit, 2);
contract!(address_tell_cancellation_survives_original_host_exit, 3);
contract!(
    neutral_cohort_policy_cancellation_survives_original_host_exit,
    4
);
contract!(
    neutral_cohort_actor_cancellation_survives_original_host_exit,
    5
);
contract!(
    neutral_cohort_config_cancellation_survives_original_host_exit,
    6
);
contract!(
    neutral_cohort_factory_cancellation_survives_original_host_exit,
    7
);
contract!(
    typed_child_actor_cancellation_survives_original_host_exit,
    8
);
contract!(
    typed_child_factory_cancellation_survives_original_host_exit,
    9
);
