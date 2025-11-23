use super::*;
use crate::actix::{ActixActor, ActixActorSystem, ActixSystemSettings};
use futures::FutureExt;
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorConfig, ActorState},
    context::ActorFactoryContext,
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisorStrategyType},
    trace::{self, TraceContext},
    types::BoxedMessage,
};
use std::{
    cell::Cell,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};
use tokio::sync::Semaphore;

const WAIT: Duration = Duration::from_secs(5);
type Context = ActixContext<ActixActor<Parent>>;

struct Probe {
    entered: mpsc::Sender<(std::thread::ThreadId, Option<TraceContext>)>,
    release: mpsc::Receiver<()>,
    drops: Arc<AtomicUsize>,
    _send_only: Cell<()>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.entered
            .send((std::thread::current().id(), TraceContext::current()))
            .unwrap();
        self.release.recv_timeout(WAIT * 3).unwrap();
        self.drops.fetch_add(1, Ordering::SeqCst);
        panic!("expired Actix context input disposal witness");
    }
}
struct SyncProbe(Mutex<Probe>);
impl Message for SyncProbe {
    type Result = ();
}
impl Clone for SyncProbe {
    fn clone(&self) -> Self {
        let _ = &self.0;
        panic!("an unpolled context input must never be cloned");
    }
}
struct Config(Option<Mutex<Probe>>);
impl ActorConfig for Config {}
impl Drop for Config {
    fn drop(&mut self) {
        drop(self.0.take());
    }
}
struct Parent(Option<Mutex<Probe>>);
impl Drop for Parent {
    fn drop(&mut self) {
        drop(self.0.take());
    }
}
impl ActixEngineExt for Parent {}

struct Dynamic(Option<Mutex<Probe>>);
impl Drop for Dynamic {
    fn drop(&mut self) {
        drop(self.0.take());
    }
}
impl Actor for Dynamic {
    type Config = parrot_api::engine::InstanceConfig;
    type Context = dyn ActorContext;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn init<'a>(&'a mut self, _: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("an unpolled dynamic instance must never initialize");
    }
}
struct Target(Mutex<Probe>);
impl std::fmt::Debug for Target {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = &self.0;
        formatter.write_str("UnpolledWatchTarget")
    }
}
impl parrot_api::address::ActorRef for Target {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("unpolled watch input was sent a message");
    }
    fn send_with_timeout<'a>(
        &'a self,
        _: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("unpolled watch input was asked a message");
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("unpolled watch input was delivered a message");
    }
    fn watch<'a>(&'a self, _: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("an unpolled watch delegate must never execute");
    }
    fn unwatch<'a>(&'a self, _: BoxedActorRef) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("an unpolled unwatch delegate must never execute");
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        panic!("an unpolled watch target must not be stopped");
    }
    fn path(&self) -> String {
        "/unpolled-watch-target".into()
    }
}
impl Actor for Parent {
    type Config = Config;
    type Context = Context;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Ok(reply) = message.downcast::<tokio::sync::oneshot::Sender<Context>>() {
                assert!(reply.send(ctx.clone()).is_ok());
            }
            Ok(Box::new(()) as BoxedMessage)
        })
    }
}

fn policy(probe: Option<Probe>) -> SupervisorStrategyType {
    let capture = Mutex::new(probe);
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| {
            let _ = &*capture.lock().unwrap();
            panic!("an unpolled context policy must never be called");
        }),
    })
}

// Probe the real per-system pending pool through its production admission API.
// Each isolated process has an otherwise idle global pool.
fn pending_available(slots: &Arc<Semaphore>) -> usize {
    let mut held = Vec::new();
    while let Some(permit) = crate::owned_payload::try_pending_cleanup_permit(slots) {
        held.push(permit);
    }
    held.len()
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
        _send_only: Cell::new(()),
    };
    let host = actix::System::new();
    let (mut ctx, slots, weak) = host.block_on(async {
        let system = ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: WAIT,
            ..Default::default()
        })
        .await
        .unwrap();
        let reference = system
            .spawn_root_typed(Parent(None), Config(None))
            .await
            .unwrap();
        let (reply, receive) = tokio::sync::oneshot::channel::<Context>();
        reference.deliver(Box::new(reply)).await.unwrap();
        let ctx = tokio::time::timeout(WAIT, receive).await.unwrap().unwrap();
        let slots = system.cleanup_slots.clone();
        let weak = system.downgrade();
        system.shutdown().await.unwrap();
        assert!(
            weak.upgrade().is_err(),
            "context retained a live system lease"
        );
        (ctx, slots, weak)
    });
    drop(host);
    assert!(weak.upgrade().is_err(), "context revived its original host");
    let baseline = pending_available(&slots);
    assert_eq!(slots.available_permits(), 1);
    assert_eq!(baseline, 1024);
    let active = slots.clone().try_acquire_owned().unwrap();
    let expected = TraceContext::root();
    let context = expected.clone();
    let owned_slots = slots.clone();
    let owner = std::thread::spawn(move || {
        let request = context.in_scope(|| -> BoxedFuture<'_, ()> {
            match case {
                0 => Box::pin(ctx.send(ctx.get_self_ref(), Box::new(probe)).map(|_| ())),
                1 => Box::pin(ctx.ask(ctx.get_self_ref(), Box::new(probe)).map(|_| ())),
                2 => Box::pin(
                    ctx.schedule_once(ctx.get_self_ref(), Box::new(probe), WAIT)
                        .map(|_| ()),
                ),
                3 => Box::pin(
                    ctx.schedule_periodic(
                        ctx.get_self_ref(),
                        CloneableMessage::from_message(SyncProbe(Mutex::new(probe))),
                        WAIT,
                        WAIT,
                    )
                    .map(|_| ()),
                ),
                4 => Box::pin(
                    ctx.schedule(SyncProbe(Mutex::new(probe)), WAIT, None)
                        .map(|_| ()),
                ),
                5..=7 => {
                    let (actor, config, strategy) = match case {
                        5 => (Parent(Some(Mutex::new(probe))), Config(None), None),
                        6 => (Parent(None), Config(Some(Mutex::new(probe))), None),
                        _ => (Parent(None), Config(None), Some(policy(Some(probe)))),
                    };
                    let actor =
                        <Context as parrot_api::context::ActorSpawnContext<Parent>>::wrap_spawn(
                            actor,
                        );
                    match strategy {
                        Some(strategy) => Box::pin(
                            ctx.spawner()
                                .spawn_with_strategy(actor, Box::new(config), strategy)
                                .map(|_| ()),
                        ),
                        None => Box::pin(ctx.spawner().spawn(actor, Box::new(config)).map(|_| ())),
                    }
                }
                8 | 9 => {
                    let (capture, strategy) = if case == 8 {
                        (Some(probe), None)
                    } else {
                        (None, Some(policy(Some(probe))))
                    };
                    let capture = Mutex::new(capture);
                    let factory =
                        <Context as ActorFactoryContext<Parent>>::wrap_factory(move || {
                            let _ = &*capture.lock().unwrap();
                            panic!("an unpolled native factory must never execute");
                        });
                    Box::pin(
                        ctx.spawner()
                            .spawn_factory_erased(factory, strategy)
                            .map(|_| ()),
                    )
                }
                10 => {
                    let capture = Mutex::new(probe);
                    let factory: parrot_api::engine::RuntimeFactory = Box::new(move || {
                        let _ = &*capture.lock().unwrap();
                        panic!("an unpolled dynamic factory must never execute");
                    });
                    Box::pin(
                        ctx.async_spawner()
                            .unwrap()
                            .spawn_factory(factory, None)
                            .map(|_| ()),
                    )
                }
                11 | 12 => {
                    let (capture, strategy) = if case == 11 {
                        (Some(probe), None)
                    } else {
                        (None, Some(policy(Some(probe))))
                    };
                    let capture = Mutex::new(capture);
                    let factory: parrot_api::engine::LocalRuntimeFactory = Box::new(move || {
                        let _ = &*capture.lock().unwrap();
                        panic!("an unpolled Local factory must never execute");
                    });
                    Box::pin(
                        ctx.local_spawner()
                            .unwrap()
                            .spawn_local_factory(factory, strategy)
                            .map(|_| ()),
                    )
                }
                13..=15 => {
                    let (actor, config, strategy) = match case {
                        13 => (
                            Dynamic(Some(Mutex::new(probe))),
                            Box::new(()) as parrot_api::engine::InstanceConfig,
                            None,
                        ),
                        14 => (
                            Dynamic(None),
                            Box::new(probe) as parrot_api::engine::InstanceConfig,
                            None,
                        ),
                        _ => (
                            Dynamic(None),
                            Box::new(()) as parrot_api::engine::InstanceConfig,
                            Some(policy(Some(probe))),
                        ),
                    };
                    Box::pin(
                        ctx.async_spawner()
                            .unwrap()
                            .spawn(Box::new(actor), config, strategy)
                            .map(|_| ()),
                    )
                }
                16 => {
                    let factory: parrot_api::engine::RuntimeFactory =
                        Box::new(|| panic!("an unpolled dynamic factory must never execute"));
                    Box::pin(
                        ctx.async_spawner()
                            .unwrap()
                            .spawn_factory(factory, Some(policy(Some(probe))))
                            .map(|_| ()),
                    )
                }
                17 => Box::pin(ctx.watch(Box::new(Target(Mutex::new(probe)))).map(|_| ())),
                18 => Box::pin(ctx.unwatch(Box::new(Target(Mutex::new(probe)))).map(|_| ())),
                _ => unreachable!(),
            }
        });
        let caller = std::thread::current().id();
        trace::in_scope(Some(TraceContext::root()), || drop(request));
        assert!(TraceContext::current().is_none());
        assert!(!crate::owned_payload::DisposalScope::active());
        returned
            .send((caller, pending_available(&owned_slots)))
            .unwrap();
        receive_resume.recv_timeout(WAIT * 3).unwrap();
    });
    let cancelled = receive_returned.recv_timeout(WAIT);
    let premature = receive_entered.try_recv().ok();
    drop(active);
    let disposed = premature
        .clone()
        .or_else(|| receive_entered.recv_timeout(WAIT).ok());
    // Unblock both witnesses before asserting, including against broken wiring.
    let _ = release.send(());
    let _ = resume.send(());
    let joined = owner.join();
    assert!(
        joined.is_ok(),
        "context input disposal escaped its cleanup owner"
    );
    let (caller, remaining) = cancelled.expect("unpolled cancellation blocked after host exit");
    let owned = if case == 4 {
        1
    } else if (5..=7).contains(&case) || (13..=15).contains(&case) {
        3
    } else {
        2
    };
    assert_eq!(
        remaining,
        baseline - owned,
        "context inputs bypassed their original pending budget"
    );
    assert!(
        premature.is_none(),
        "input bypassed its original active worker budget"
    );
    let (worker, actual) = disposed.expect("captured input was never actually retired");
    assert_ne!(worker, caller);
    assert_eq!(actual, Some(expected));
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    observer.block_on(async {
        tokio::time::timeout(WAIT, async {
            loop {
                if slots.available_permits() == 1 && pending_available(&slots) == baseline {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual disposal did not restore original budgets");
    });
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

macro_rules! contract {
    ($name:ident, $case:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!("actix::context::expired_contracts::", stringify!($name)),
                || check($case),
            );
        }
    };
}
contract!(send_after_host_exit, 0);
contract!(ask_after_host_exit, 1);
contract!(once_after_host_exit, 2);
contract!(periodic_after_host_exit, 3);
contract!(typed_schedule_after_host_exit, 4);
contract!(instance_actor_after_host_exit, 5);
contract!(instance_config_after_host_exit, 6);
contract!(instance_policy_after_host_exit, 7);
contract!(factory_capture_after_host_exit, 8);
contract!(factory_policy_after_host_exit, 9);
contract!(dynamic_factory_capture_after_host_exit, 10);
contract!(local_factory_capture_after_host_exit, 11);
contract!(local_factory_policy_after_host_exit, 12);
contract!(dynamic_instance_actor_after_host_exit, 13);
contract!(dynamic_instance_config_after_host_exit, 14);
contract!(dynamic_instance_policy_after_host_exit, 15);
contract!(dynamic_factory_policy_after_host_exit, 16);
contract!(watch_target_after_host_exit, 17);
contract!(unwatch_target_after_host_exit, 18);
