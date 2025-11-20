use super::*;
use parrot_api::{
    supervisor::{BasicDecisionFn, OneForAllStrategy, OneForOneStrategy},
    trace::{self, TraceContext},
};
use std::time::Duration;

type Identity = ([u8; 16], [u8; 8]);
type Events = Arc<Mutex<Vec<(&'static str, Option<Identity>)>>>;

fn identity(context: Option<TraceContext>) -> Option<Identity> {
    context.map(|context| (context.trace_id(), context.span_id()))
}
fn note(events: &Events, phase: &'static str) {
    events
        .lock()
        .unwrap()
        .push((phase, identity(TraceContext::current())));
}
struct Payload(Events);
impl Drop for Payload {
    fn drop(&mut self) {
        note(&self.0, "panic-payload");
        panic!("policy payload drop");
    }
}
struct Capture {
    events: Events,
    phase: &'static str,
    panic: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        note(&self.events, self.phase);
        if self.panic {
            std::panic::panic_any(Payload(self.events.clone()));
        }
    }
}
fn strategy(
    events: &Events,
    phase: &'static str,
    all: bool,
    panic: bool,
) -> SupervisorStrategyType {
    let capture = Capture {
        events: events.clone(),
        phase,
        panic,
    };
    let decider = BasicDecisionFn::new(move |_| {
        note(&capture.events, "callback");
        SupervisionDecision::Stop
    });
    if all {
        SupervisorStrategyType::OneForAll(OneForAllStrategy {
            max_restarts: 1,
            within: Duration::from_secs(5),
            decider,
        })
    } else {
        SupervisorStrategyType::OneForOne(OneForOneStrategy {
            max_restarts: 1,
            within: Duration::from_secs(5),
            decider,
        })
    }
}
fn store(events: &Events, owner: &Option<TraceContext>, all: bool, slots: usize) -> PolicyStore {
    trace::in_scope(owner.clone(), || {
        PolicyStore::new(
            Some(strategy(events, "capture", all, false)),
            Arc::new(Semaphore::new(slots)),
        )
    })
}
fn expect(events: &Events, expected: Vec<(&'static str, Option<Identity>)>) {
    assert_eq!(*events.lock().unwrap(), expected);
}

#[tokio::test]
async fn explicit_retirement_keeps_acceptance_identity_across_repeated_waits() {
    for traced in [false, true] {
        for all in [false, true] {
            let owner = traced.then(TraceContext::root);
            let ambient = TraceContext::root();
            let events = Events::default();
            let store = store(&events, &owner, all, 1);
            trace::scope(Some(ambient), async {
                store.dispose().await.unwrap();
                store.dispose().await.unwrap();
            })
            .await;
            expect(&events, vec![("capture", identity(owner))]);
            assert!(TraceContext::current().is_none());
        }
    }
}

#[test]
fn final_store_drop_keeps_acceptance_identity_without_runtime() {
    for traced in [false, true] {
        let owner = traced.then(TraceContext::root);
        let ambient = TraceContext::root();
        let events = Events::default();
        let store = store(&events, &owner, false, 1);
        trace::in_scope(Some(ambient.clone()), || {
            drop(store);
            assert_eq!(
                identity(TraceContext::current()),
                identity(Some(ambient.clone()))
            );
        });
        expect(&events, vec![("capture", identity(owner))]);
        assert!(TraceContext::current().is_none());
    }
}

#[test]
fn replacement_keeps_old_and_new_policy_owners_separate() {
    for traced in [false, true] {
        let old = traced.then(TraceContext::root);
        let new = Some(TraceContext::root());
        let ambient = Some(TraceContext::root());
        let events = Events::default();
        let store = store(&events, &old, false, 1);
        trace::in_scope(new.clone(), || {
            store.set(Some(strategy(&events, "replacement", true, false)))
        });
        trace::in_scope(ambient, || drop(store));
        expect(
            &events,
            vec![("capture", identity(old)), ("replacement", identity(new))],
        );
    }
}

#[tokio::test]
async fn rejected_setter_keeps_its_call_identity_after_store_closure() {
    let events = Events::default();
    let owner = Some(TraceContext::root());
    let rejected = Some(TraceContext::root());
    let store = store(&events, &owner, false, 1);
    store.dispose().await.unwrap();
    trace::in_scope(rejected.clone(), || {
        store.set(Some(strategy(&events, "rejected", false, false)))
    });
    store.dispose().await.unwrap();
    expect(
        &events,
        vec![
            ("capture", identity(owner)),
            ("rejected", identity(rejected)),
        ],
    );
}

#[tokio::test]
async fn final_decision_snapshot_keeps_policy_owner_and_callback_cause_separate() {
    for traced in [false, true] {
        for all in [false, true] {
            let owner = traced.then(TraceContext::root);
            let cause = Some(TraceContext::root());
            let events = Events::default();
            let store = store(&events, &owner, all, 1);
            let tracker = DecisionTracker::default();
            let snapshot = tracker
                .snapshot(store.slots.clone(), || store.get())
                .unwrap()
                .unwrap();
            store.set(None);
            assert!(events.lock().unwrap().is_empty());
            let result =
                trace::in_scope(cause.clone(), || decide(snapshot, &ActorError::Stopped)).unwrap();
            assert!(matches!(result, SupervisionDecision::Stop));
            tracker.close().await.unwrap();
            store.dispose().await.unwrap();
            expect(
                &events,
                vec![("callback", identity(cause)), ("capture", identity(owner))],
            );
            assert!(TraceContext::current().is_none());
        }
    }
}

#[test]
fn host_cancelled_snapshot_keeps_owner_before_poll_and_while_waiting_for_permit() {
    for polled in [false, true] {
        for traced in [false, true] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let owner = traced.then(TraceContext::root);
            let events = Events::default();
            let store = store(&events, &owner, false, 0);
            let tracker = DecisionTracker::default();
            let snapshot = tracker
                .snapshot(store.slots.clone(), || store.get())
                .unwrap()
                .unwrap();
            store.set(None);
            runtime.block_on(trace::scope(Some(TraceContext::root()), async {
                drop(snapshot);
                if polled {
                    tokio::task::yield_now().await;
                }
                assert!(events.lock().unwrap().is_empty());
            }));
            runtime.shutdown_timeout(Duration::from_secs(1));
            futures::executor::block_on(tracker.close()).unwrap();
            expect(&events, vec![("capture", identity(owner))]);
        }
    }
}

#[test]
fn host_cancelled_store_disposal_keeps_owner_before_poll_and_while_waiting_for_permit() {
    for polled in [false, true] {
        for traced in [false, true] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let owner = traced.then(TraceContext::root);
            let events = Events::default();
            let store = store(&events, &owner, false, 0);
            runtime.block_on(trace::scope(Some(TraceContext::root()), async {
                let mut disposal = Box::pin(store.dispose());
                assert!(futures::poll!(&mut disposal).is_pending());
                drop(disposal);
                if polled {
                    tokio::task::yield_now().await;
                }
                assert!(events.lock().unwrap().is_empty());
            }));
            runtime.shutdown_timeout(Duration::from_secs(1));
            futures::executor::block_on(store.dispose()).unwrap();
            expect(&events, vec![("capture", identity(owner))]);
        }
    }
}

#[tokio::test]
async fn recursive_disposal_payload_keeps_owner_and_repeatable_failure_receipt() {
    for traced in [false, true] {
        let owner = traced.then(TraceContext::root);
        let events = Events::default();
        let store = trace::in_scope(owner.clone(), || {
            PolicyStore::new(
                Some(strategy(&events, "capture", false, true)),
                Arc::new(Semaphore::new(1)),
            )
        });
        for _ in 0..2 {
            let error = trace::scope(Some(TraceContext::root()), store.dispose())
                .await
                .unwrap_err();
            assert!(
                matches!(error, ActorError::InternalError(ref text)
                if text.contains("policy disposal failed") && text.contains("panic payload destructor panicked")),
                "{error}"
            );
        }
        expect(
            &events,
            vec![
                ("capture", identity(owner.clone())),
                ("panic-payload", identity(owner)),
            ],
        );
    }
}
