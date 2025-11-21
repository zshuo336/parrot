use super::*;
use crate::thread::{
    ThreadActorSystem, ThreadActorSystemConfig, context::ThreadContext, tests_support::DummyActor,
};
use parrot_api::{
    address::ActorPath,
    context::{ActorFactoryContext, ActorSpawner},
    supervisor::{BasicDecisionFn, OneForOneStrategy, SupervisionDecision, SupervisorStrategyType},
    trace::{self, TraceContext},
};
use std::{cell::Cell, sync::mpsc, time::Duration};

const WAIT: Duration = Duration::from_secs(3);
struct Probe {
    entered: mpsc::Sender<(std::thread::ThreadId, Option<TraceContext>)>,
    release: mpsc::Receiver<()>,
    drops: Arc<std::sync::atomic::AtomicUsize>,
    _send_only: Cell<()>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.entered
            .send((std::thread::current().id(), TraceContext::current()))
            .unwrap();
        self.release.recv_timeout(WAIT * 3).unwrap();
        self.drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        panic!("expired factory input disposal witness");
    }
}
fn check(policy: bool) {
    let (entered, receive_entered) = mpsc::channel();
    let (release, receive_release) = mpsc::channel();
    let (returned, receive_returned) = mpsc::channel();
    let (resume, receive_resume) = mpsc::channel();
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe = Mutex::new(Probe {
        entered,
        release: receive_release,
        drops: drops.clone(),
        _send_only: Cell::new(()),
    });
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
        let reference = host
            .block_on(system.spawn_at(DummyActor, "/expired-factory", None, Default::default()))
            .unwrap();
        let ctx = ThreadContext::<DummyActor>::new(
            Arc::downgrade(&system),
            host.handle().clone(),
            ActorPath::new(Arc::new(reference.clone()), "/expired-factory".into()),
            None,
            Default::default(),
        );
        host.block_on(system.shutdown_internal()).unwrap();
        let weak = Arc::downgrade(&system);
        let slots = system.cleanup_slots.clone();
        let pending = pending_cleanup_slots(&slots);
        let active = slots.clone().try_acquire_owned().unwrap();
        drop(system);
        drop(host);
        assert!(weak.upgrade().is_none());
        assert_eq!(reference.mailbox_cell().lock().unwrap().strong_count(), 0);
        let request = context.in_scope(|| {
            let (factory, strategy) = if policy {
                (
                    <ThreadContext<DummyActor> as ActorFactoryContext<DummyActor>>::wrap_factory(
                        || panic!("an unpolled factory must not execute"),
                    ),
                    Some(SupervisorStrategyType::OneForOne(OneForOneStrategy {
                        max_restarts: 0,
                        within: WAIT,
                        decider: BasicDecisionFn::new(move |_| {
                            let _ = &*probe.lock().unwrap();
                            SupervisionDecision::Stop
                        }),
                    })),
                )
            } else {
                (
                    <ThreadContext<DummyActor> as ActorFactoryContext<DummyActor>>::wrap_factory(
                        move || {
                            let _ = &*probe.lock().unwrap();
                            panic!("an unpolled factory must not execute");
                        },
                    ),
                    None,
                )
            };
            ActorSpawner::spawn_factory_erased(&ctx, factory, strategy)
        });
        let caller = std::thread::current().id();
        trace::in_scope(Some(TraceContext::root()), || drop(request));
        assert!(TraceContext::current().is_none());
        assert!(!DisposalScope::active());
        returned
            .send((caller, pending.available_permits(), active))
            .unwrap();
        receive_resume.recv_timeout(WAIT * 3).unwrap();
        let observer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        observer.block_on(async {
            tokio::time::timeout(WAIT, async {
                while slots.available_permits() != 1
                    || pending.available_permits() != MAX_PENDING_CLEANUPS_PER_SLOT
                    || global_pending_cleanup_slots().available_permits()
                        != MAX_PENDING_CLEANUPS_GLOBAL
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("factory input disposal did not restore original budgets");
        });
    });
    let (caller, pending, active) = receive_returned.recv_timeout(WAIT * 3).unwrap();
    let premature = receive_entered.try_recv().ok();
    drop(active);
    let disposed = premature
        .clone()
        .or_else(|| receive_entered.recv_timeout(WAIT).ok());
    let _ = release.send(());
    let _ = resume.send(());
    let joined = owner.join();
    assert!(joined.is_ok());
    assert_eq!(
        pending,
        MAX_PENDING_CLEANUPS_PER_SLOT - 2,
        "expired factory inputs bypassed the original pending cleanup budget"
    );
    assert!(
        premature.is_none(),
        "input bypassed the original worker quota"
    );
    let (worker, actual) = disposed.expect("real capture disposal never ran");
    assert_ne!(worker, caller);
    assert_eq!(actual, Some(expected));
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
}
#[test]
fn factory_capture_retains_original_cleanup_after_system_exit() {
    crate::test_support::isolated(
        "owned_payload::expired_factory_contracts::factory_capture_retains_original_cleanup_after_system_exit",
        || check(false),
    );
}
#[test]
fn factory_policy_retains_original_cleanup_after_system_exit() {
    crate::test_support::isolated(
        "owned_payload::expired_factory_contracts::factory_policy_retains_original_cleanup_after_system_exit",
        || check(true),
    );
}
