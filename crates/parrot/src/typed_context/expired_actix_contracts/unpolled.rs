use super::*;
use futures::FutureExt;
use parrot_api::{
    local_typed::{LocalBoxedFuture, LocalTypedReceive},
    trace::{self, TraceContext},
};
use std::{cell::Cell, rc::Rc, sync::mpsc};
use tokio::sync::Semaphore;

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
        panic!("unpolled static child input disposal witness");
    }
}
#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(Read), lifecycle)]
struct InputActor(Option<Probe>);
impl Drop for InputActor {
    fn drop(&mut self) {
        drop(self.0.take());
    }
}
impl TypedLifecycle for InputActor {
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        panic!("an unpolled static child must never initialize");
    }
}
impl TypedReceive<Read> for InputActor {}

#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(Read), local)]
struct Local {
    _state: Rc<()>,
}
impl LocalTypedReceive<Read> for Local {
    fn receive_typed(&mut self, _: Read) -> LocalBoxedFuture<'_, ActorResult<TypedContext>> {
        panic!("an unpolled Local child must never receive a message");
    }
}
fn policy(probe: Option<Probe>) -> SupervisorStrategyType {
    let capture = Mutex::new(probe);
    SupervisorStrategyType::OneForOne(OneForOneStrategy {
        max_restarts: 0,
        within: WAIT,
        decider: BasicDecisionFn::new(move |_| {
            let _ = &capture;
            panic!("an unpolled child policy must never be called");
        }),
    })
}
fn pending_available(slots: &Arc<Semaphore>) -> usize {
    let mut held = Vec::new();
    while let Some(permit) = crate::owned_payload::try_pending_cleanup_permit(slots) {
        held.push(permit);
    }
    held.len()
}
fn check(entry: u8, witness: u8) {
    let (context, slots) = expired();
    let baseline = pending_available(&slots);
    assert_eq!(baseline, 1024);
    let active = slots.clone().try_acquire_owned().unwrap();
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
    let expected = TraceContext::root();
    let invocation = expected.clone();
    let owned_slots = slots.clone();
    let owner = std::thread::spawn(move || {
        let request = invocation.in_scope(|| -> BoxedFuture<'_, ()> {
            let (input, strategy) = match witness {
                0 => (Some(probe), None),
                1 => (Some(probe), Some(policy(None))),
                2 => (None, Some(policy(Some(probe)))),
                _ => unreachable!(),
            };
            match entry {
                0 => {
                    let actor = InputActor(input);
                    match strategy {
                        Some(strategy) => Box::pin(
                            context
                                .spawn_typed_child_with_strategy::<_, Read>(actor, strategy)
                                .map(|_| ()),
                        ),
                        None => Box::pin(context.spawn_typed_child::<_, Read>(actor).map(|_| ())),
                    }
                }
                1 => {
                    let capture = Mutex::new(input);
                    let factory = move || -> InputActor {
                        let _ = &capture;
                        panic!("an unpolled static factory must never execute");
                    };
                    match strategy {
                        Some(strategy) => Box::pin(
                            context
                                .spawn_typed_child_factory_with_strategy::<_, Read, _>(
                                    factory, strategy,
                                )
                                .map(|_| ()),
                        ),
                        None => Box::pin(
                            context
                                .spawn_typed_child_factory::<_, Read, _>(factory)
                                .map(|_| ()),
                        ),
                    }
                }
                2 | 3 => {
                    // This constructor is Send but deliberately not Sync; its output is not Send.
                    let constructor = move || -> Local {
                        let _ = &input;
                        panic!("an unpolled Local constructor must never execute");
                    };
                    match (entry, strategy) {
                        (2, Some(strategy)) => Box::pin(
                            context
                                .spawn_local_typed_child_with_strategy::<_, Read, _>(
                                    constructor,
                                    strategy,
                                )
                                .map(|_| ()),
                        ),
                        (2, None) => Box::pin(
                            context
                                .spawn_local_typed_child::<_, Read, _>(constructor)
                                .map(|_| ()),
                        ),
                        (3, Some(strategy)) => Box::pin(
                            context
                                .spawn_local_typed_child_factory_with_strategy::<_, Read, _>(
                                    constructor,
                                    strategy,
                                )
                                .map(|_| ()),
                        ),
                        (3, None) => Box::pin(
                            context
                                .spawn_local_typed_child_factory::<_, Read, _>(constructor)
                                .map(|_| ()),
                        ),
                        _ => unreachable!(),
                    }
                }
                4 => {
                    let factory: parrot_api::engine::LocalRuntimeFactory = Box::new(move || {
                        let _ = &input;
                        panic!("an unpolled neutral Local factory must never execute");
                    });
                    Box::pin(
                        context
                            .local_spawner()
                            .unwrap()
                            .spawn_local_factory(factory, strategy)
                            .map(|_| ()),
                    )
                }
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
    // Release both witnesses before asserting so broken wiring cannot strand the caller.
    let _ = release.send(());
    let _ = resume.send(());
    let joined = owner.join();
    assert!(joined.is_ok(), "input disposal escaped its cleanup owner");
    let (caller, remaining) = cancelled.expect("unpolled cancellation blocked after host exit");
    assert_eq!(
        remaining,
        baseline - 2,
        "child inputs bypassed their original pending budget"
    );
    assert!(
        premature.is_none(),
        "child input bypassed its original active worker budget"
    );
    let (worker, actual) = disposed.expect("child input was never actually retired");
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
        .expect("actual disposal did not restore the original budgets");
    });
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
macro_rules! contract {
    ($name:ident, $entry:expr, $witness:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!(
                    "typed_context::expired_actix_contracts::unpolled::",
                    stringify!($name)
                ),
                || check($entry, $witness),
            );
        }
    };
}
contract!(instance_actor_after_host_exit, 0, 0);
contract!(instance_actor_with_policy_after_host_exit, 0, 1);
contract!(instance_policy_after_host_exit, 0, 2);
contract!(factory_capture_after_host_exit, 1, 0);
contract!(factory_capture_with_policy_after_host_exit, 1, 1);
contract!(factory_policy_after_host_exit, 1, 2);
contract!(local_once_capture_after_host_exit, 2, 0);
contract!(local_once_capture_with_policy_after_host_exit, 2, 1);
contract!(local_once_policy_after_host_exit, 2, 2);
contract!(local_repeatable_capture_after_host_exit, 3, 0);
contract!(local_repeatable_capture_with_policy_after_host_exit, 3, 1);
contract!(local_repeatable_policy_after_host_exit, 3, 2);
contract!(neutral_local_capture_after_host_exit, 4, 0);
contract!(neutral_local_capture_with_policy_after_host_exit, 4, 1);
contract!(neutral_local_policy_after_host_exit, 4, 2);
