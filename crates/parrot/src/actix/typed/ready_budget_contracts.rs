//! Poll the real ordinary/factory runners at controlled mailbox and handler boundaries.
use super::*;
use parrot_api::types::BoxedFuture;
use std::{
    future::{Future, poll_fn},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[derive(Default)]
struct Observed {
    initialized: AtomicUsize,
    completed: Mutex<Vec<usize>>,
    messages_retired: AtomicUsize,
    stopped: AtomicUsize,
    actors_retired: AtomicUsize,
}
struct Work {
    id: usize,
    observed: Arc<Observed>,
}
impl Drop for Work {
    fn drop(&mut self) {
        self.observed
            .messages_retired
            .fetch_add(1, Ordering::SeqCst);
    }
}
struct Probe {
    observed: Arc<Observed>,
    pause_at: Option<usize>,
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.observed.actors_retired.fetch_add(1, Ordering::SeqCst);
    }
}
impl ParrotTypedDispatch for Probe {
    type Msg = Work;
    type Reply = usize;
    fn init(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.observed.initialized.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
    fn dispatch(&mut self, msg: Work) -> BoxedFuture<'_, ActorResult<usize>> {
        let suspend = self.pause_at == Some(msg.id);
        Box::pin(async move {
            let mut suspended = false;
            if suspend {
                poll_fn(|cx| {
                    if suspended {
                        Poll::Ready(())
                    } else {
                        suspended = true;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
            }
            self.observed.completed.lock().unwrap().push(msg.id);
            Ok(msg.id)
        })
    }
    fn before_stop(&mut self) -> BoxedFuture<'_, ActorResult<()>> {
        self.observed.stopped.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone, Copy)]
enum Scenario {
    FullMailbox,
    EmptyMailbox,
    SuspendedHandler,
    StopDuringBurst,
}
async fn exercise(factory: bool, scenario: Scenario) {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let runtime = tokio::runtime::Handle::current();
    let (control, _completion) = ActorControl::new(Duration::from_secs(5));
    let tree = Arc::new(crate::typed_context::TypedTree::new_backend(
        crate::typed_context::Backend::Actix(Box::new(system.downgrade())),
        crate::typed_context::Identity {
            path: "/budget-contract".into(),
            own: Arc::new(OnceLock::new()),
            parent: None,
        },
        crate::typed_context::Hooks {
            stop: Arc::new(|_| {}),
            escalate: Arc::new(|_| {}),
        },
        Duration::from_secs(5),
        runtime.clone(),
        system.policy_slots.clone(),
        system.cleanup_slots.clone(),
    ));
    assert!(control.typed_tree.set(tree).is_ok());
    let observed = Arc::new(Observed::default());
    let pause_at = matches!(scenario, Scenario::SuspendedHandler).then_some(9);
    let make_actor = {
        let observed = observed.clone();
        move || Probe {
            observed: observed.clone(),
            pause_at,
        }
    };
    let (actor, source) = if factory {
        (
            None,
            Some(crate::constructor::owned::OwnedFactory::new(
                make_actor,
                system.constructor_slots.clone(),
                system.cleanup_slots.clone(),
                runtime.clone(),
            )),
        )
    } else {
        (Some(make_actor()), None)
    };
    let (sender, receiver) = parrot_api::typed_channel::typed_channel::<Probe>(64);
    let (finished, retirement) = tokio::sync::oneshot::channel();
    let inputs = TypedInputs {
        actor,
        creation_context: None,
        receiver: Some(receiver),
        factory: source.as_ref().map(|source| source.factory.clone()),
        policy_slots: system.policy_slots.clone(),
        cleanup_error: None,
        dispatch_cancel_failed: false,
        control: control.clone(),
        finished: Some(finished),
    };
    let mut runner = Box::pin(run_actor(inputs, control.clone(), runtime));
    // Real construction may suspend. Once init has completed, the empty
    // mailbox guarantees the complete runner has reached Pending.
    tokio::time::timeout(
        Duration::from_secs(5),
        poll_fn(|cx| {
            assert!(
                runner.as_mut().poll(cx).is_pending(),
                "runner exited before messages"
            );
            if observed.initialized.load(Ordering::SeqCst) == 1 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }),
    )
    .await
    .unwrap();
    let (reply, responses) = flume::unbounded();
    let enqueue = |range: std::ops::Range<usize>| {
        for id in range {
            assert!(
                sender
                    .try_send(TypedEnvelope {
                        msg: Work {
                            id,
                            observed: observed.clone()
                        },
                        reply: Some(reply.clone()),
                        trace_context: None
                    })
                    .is_ok()
            );
        }
    };
    let mut poll_runner = || {
        assert!(
            runner
                .as_mut()
                .poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
                .is_pending()
        );
        observed.completed.lock().unwrap().len()
    };
    // Collect mismatches until after real cleanup, so a regression cannot
    // skip factory retirement or leave the system workers running.
    let (counts, expected, sent, handled) = match scenario {
        Scenario::FullMailbox => {
            enqueue(0..25);
            (
                vec![poll_runner(), poll_runner(), poll_runner()],
                vec![10, 20, 25],
                25,
                25,
            )
        }
        Scenario::EmptyMailbox => {
            enqueue(0..9);
            let first = poll_runner();
            enqueue(9..19);
            (vec![first, poll_runner()], vec![9, 19], 19, 19)
        }
        Scenario::SuspendedHandler => {
            enqueue(0..19);
            (vec![poll_runner(), poll_runner()], vec![9, 19], 19, 19)
        }
        Scenario::StopDuringBurst => {
            enqueue(0..25);
            (vec![poll_runner()], vec![10], 25, 10)
        }
    };
    control.request_stop(tokio::time::Instant::now() + Duration::from_secs(5));
    let exit = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .unwrap();
    let retired = retirement.await.unwrap().result();
    if let Some(source) = source {
        source.retire().await.unwrap();
    }
    let shutdown = system.as_ref().clone().shutdown().await;
    exit.outcome.unwrap();
    exit.cleanup.unwrap();
    retired.unwrap();
    shutdown.unwrap();
    assert_eq!(counts, expected, "real runner burst boundaries changed");
    assert_eq!(
        *observed.completed.lock().unwrap(),
        (0..handled).collect::<Vec<_>>()
    );
    assert_eq!(
        responses
            .try_iter()
            .collect::<Vec<_>>()
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>(),
        (0..handled).collect::<Vec<_>>()
    );
    assert_eq!(observed.messages_retired.load(Ordering::SeqCst), sent);
    assert_eq!(observed.initialized.load(Ordering::SeqCst), 1);
    assert_eq!(observed.stopped.load(Ordering::SeqCst), 1);
    assert_eq!(observed.actors_retired.load(Ordering::SeqCst), 1);
    assert!(sender.is_disconnected());
}

#[actix::test]
async fn ordinary_full_mailbox_keeps_fairness() {
    exercise(false, Scenario::FullMailbox).await;
}
#[actix::test]
async fn factory_full_mailbox_keeps_fairness() {
    exercise(true, Scenario::FullMailbox).await;
}
#[actix::test]
async fn ordinary_empty_mailbox_resets_budget() {
    exercise(false, Scenario::EmptyMailbox).await;
}
#[actix::test]
async fn factory_empty_mailbox_resets_budget() {
    exercise(true, Scenario::EmptyMailbox).await;
}
#[actix::test]
async fn ordinary_pending_handler_resets_budget() {
    exercise(false, Scenario::SuspendedHandler).await;
}
#[actix::test]
async fn factory_pending_handler_resets_budget() {
    exercise(true, Scenario::SuspendedHandler).await;
}
#[actix::test]
async fn ordinary_stop_interrupts_full_mailbox() {
    exercise(false, Scenario::StopDuringBurst).await;
}
#[actix::test]
async fn factory_stop_interrupts_full_mailbox() {
    exercise(true, Scenario::StopDuringBurst).await;
}
