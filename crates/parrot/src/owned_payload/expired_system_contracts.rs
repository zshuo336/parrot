use super::*;
use crate::thread::{
    BackpressureStrategy, ThreadActorSystem, ThreadActorSystemConfig, context::ThreadContext,
    tests_support::DummyActor,
};
use futures::FutureExt;
use parrot_api::{
    Message,
    address::{ActorPath, ActorRef},
    context::{ActorContext, ActorContextScheduler},
    message::CloneableMessage,
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
        panic!("expired-system input disposal witness");
    }
}
struct SyncProbe(Mutex<Probe>);
impl Message for SyncProbe {
    type Result = ();
}
impl Clone for SyncProbe {
    fn clone(&self) -> Self {
        let _ = &self.0;
        panic!("an unpolled input must not be cloned");
    }
}

fn check(case: u8) {
    let (entered, receive_entered) = mpsc::channel();
    let (release, receive_release) = mpsc::channel();
    let (ready, receive_ready) = mpsc::channel();
    let (returned, receive_returned) = mpsc::channel();
    let (resume, receive_resume) = mpsc::channel();
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe = Probe {
        entered,
        release: receive_release,
        drops: drops.clone(),
        _send_only: Cell::new(()),
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
        let reference = host.block_on(async {
            if case == 5 {
                system
                    .spawn_supervised(|| DummyActor, "/expired", Default::default())
                    .await
                    .unwrap()
            } else {
                system
                    .spawn_at(DummyActor, "/expired", None, Default::default())
                    .await
                    .unwrap()
            }
        });
        // Exercise the public bound-context constructor with a real address.
        // These requests remain unpolled; no registration or delivery is admitted.
        let mut ctx = ThreadContext::<DummyActor>::new(
            Arc::downgrade(&system),
            host.handle().clone(),
            ActorPath::new(Arc::new(reference.clone()), "/expired".into()),
            None,
            Default::default(),
        );
        ctx.set_self_ref(Box::new(reference.clone()));
        host.block_on(system.shutdown_internal()).unwrap();
        assert!(!host.block_on(reference.is_alive()));
        let weak = Arc::downgrade(&system);
        let slots = system.cleanup_slots.clone();
        let pending = pending_cleanup_slots(&slots);
        let permit = slots.clone().try_acquire_owned().unwrap();
        drop(system);
        drop(host);
        assert!(weak.upgrade().is_none(), "views retained the actor system");
        assert_eq!(reference.mailbox_cell().lock().unwrap().strong_count(), 0);
        ready.send(permit).unwrap();

        let reference = reference.clone();
        let erased = reference.clone_boxed();
        let request = context.in_scope(|| -> BoxedFuture<'_, ()> {
            match case {
                0 => Box::pin(reference.ask(Box::new(probe)).map(|_| ())),
                1 | 5 => Box::pin(reference.send_msg(Box::new(probe)).map(|_| ())),
                2 => Box::pin(
                    reference
                        .send_with_priority(Box::new(probe), BackpressureStrategy::Block, true)
                        .map(|_| ()),
                ),
                3 => Box::pin(erased.send(Box::new(probe)).map(|_| ())),
                4 => Box::pin(erased.deliver(Box::new(probe)).map(|_| ())),
                6 => Box::pin(
                    ctx.send(reference.clone_boxed(), Box::new(probe))
                        .map(|_| ()),
                ),
                7 => Box::pin(
                    ctx.ask(reference.clone_boxed(), Box::new(probe))
                        .map(|_| ()),
                ),
                8 => Box::pin(
                    ctx.schedule_once(reference.clone_boxed(), Box::new(probe), WAIT)
                        .map(|_| ()),
                ),
                9 => Box::pin(
                    ctx.schedule_periodic(
                        reference.clone_boxed(),
                        CloneableMessage::from_message(SyncProbe(Mutex::new(probe))),
                        WAIT,
                        WAIT,
                    )
                    .map(|_| ()),
                ),
                10 => Box::pin(
                    ctx.schedule(SyncProbe(Mutex::new(probe)), WAIT, None)
                        .map(|_| ()),
                ),
                _ => unreachable!(),
            }
        });
        let caller = std::thread::current().id();
        trace::in_scope(Some(TraceContext::root()), || drop(request));
        assert!(TraceContext::current().is_none());
        assert!(!DisposalScope::active());
        returned.send(caller).unwrap();
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
            .expect("real disposal must restore the original system cleanup budgets");
        });
    });
    let permit = receive_ready.recv_timeout(WAIT * 3).unwrap();
    let returned_before_release = receive_returned.recv_timeout(WAIT);
    let premature_disposal = receive_entered.try_recv().ok();
    drop(permit);
    let disposal = premature_disposal
        .clone()
        .or_else(|| receive_entered.recv_timeout(WAIT).ok());
    // Release every blocking witness and join before reporting an old-code failure.
    let _ = release.send(());
    let _ = resume.send(());
    let joined = owner.join();
    assert!(joined.is_ok(), "input owner must isolate destructor panic");
    let caller = returned_before_release
        .expect("unpolled cancellation blocked after the actor system expired");
    assert!(
        premature_disposal.is_none(),
        "expired-system views bypassed their original shared cleanup budget"
    );
    let (worker, actual_context) = disposal.expect("input must actually be disposed");
    assert_ne!(worker, caller);
    assert_eq!(actual_context, Some(expected));
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
}

macro_rules! contract {
    ($name:ident, $case:expr) => {
        #[test]
        fn $name() {
            crate::test_support::isolated(
                concat!(
                    "owned_payload::expired_system_contracts::",
                    stringify!($name)
                ),
                || check($case),
            );
        }
    };
}
contract!(root_ask_retains_cleanup_after_system_exit, 0);
contract!(root_tell_retains_cleanup_after_system_exit, 1);
contract!(priority_tell_retains_cleanup_after_system_exit, 2);
contract!(erased_ask_retains_cleanup_after_system_exit, 3);
contract!(erased_tell_retains_cleanup_after_system_exit, 4);
contract!(factory_reference_retains_cleanup_after_system_exit, 5);
contract!(context_send_retains_cleanup_after_system_exit, 6);
contract!(context_ask_retains_cleanup_after_system_exit, 7);
contract!(context_once_retains_cleanup_after_system_exit, 8);
contract!(context_periodic_retains_cleanup_after_system_exit, 9);
contract!(typed_schedule_retains_cleanup_after_system_exit, 10);
