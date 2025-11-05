use super::*;
use parrot_api::trace::{self, TraceContext};
use std::{sync::atomic::AtomicUsize, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity([u8; 16], [u8; 8]);
fn current() -> Option<Identity> {
    TraceContext::current().map(|context| Identity(context.trace_id(), context.span_id()))
}
fn identity(context: &Option<TraceContext>) -> Option<Identity> {
    context
        .as_ref()
        .map(|context| Identity(context.trace_id(), context.span_id()))
}
#[derive(Default)]
struct Events(Mutex<Vec<(&'static str, Option<Identity>)>>);
impl Events {
    fn note(&self, label: &'static str) {
        self.0.lock().unwrap().push((label, current()));
    }
    fn snapshot(&self) -> Vec<(&'static str, Option<Identity>)> {
        self.0.lock().unwrap().clone()
    }
    fn require(&self, label: &'static str, expected: &Option<TraceContext>, count: usize) {
        let events = self.snapshot();
        let actual: Vec<_> = events
            .iter()
            .filter(|row| row.0 == label)
            .map(|row| row.1.clone())
            .collect();
        assert_eq!(
            actual,
            vec![identity(expected); count],
            "constructor ownership must preserve the designated causal identity: {label}; {events:?}"
        );
    }
}
struct OnDrop {
    label: &'static str,
    events: Arc<Events>,
}
impl Drop for OnDrop {
    fn drop(&mut self) {
        self.events.note(self.label);
    }
}
struct Output {
    value: usize,
    owner: OnDrop,
}
fn factory(events: &Arc<Events>, origin: Option<TraceContext>) -> Arc<Factory<Output>> {
    let events = events.clone();
    let capture = OnDrop {
        label: "factory-drop",
        events: events.clone(),
    };
    let sequence = AtomicUsize::new(0);
    trace::in_scope(origin, || {
        Factory::new(
            move || {
                std::hint::black_box(&capture);
                events.note("constructor");
                Output {
                    value: sequence.fetch_add(1, Ordering::SeqCst) + 1,
                    owner: OnDrop {
                        label: "output-drop",
                        events: events.clone(),
                    },
                }
            },
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(1)),
        )
    })
}
fn publisher(events: &Arc<Events>) -> impl FnOnce(Output) -> Result<usize, ()> + Send + 'static {
    let events = events.clone();
    let capture = OnDrop {
        label: "publisher-drop",
        events: events.clone(),
    };
    move |output| {
        std::hint::black_box(&capture);
        std::hint::black_box(&output.owner);
        events.note("publisher");
        let value = output.value + 10;
        drop(output);
        Ok(value)
    }
}
async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("constructor identity independent liveness watchdog")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_construction_uses_each_attempt_and_never_factory_creation_parent() {
    within(async {
        for traced in [false, true] {
            let events = Arc::new(Events::default());
            let origin = Some(TraceContext::root());
            let factory = factory(&events, origin.clone());
            let polling = Some(TraceContext::root());
            for index in 1..=2 {
                let attempt = traced.then(TraceContext::root);
                let work = trace::in_scope(attempt.clone(), || {
                    factory.construct(Arc::default(), publisher(&events))
                });
                let result = trace::scope(polling.clone(), async {
                    let result = work.await;
                    assert_eq!(
                        current(),
                        identity(&polling),
                        "construct must restore polling identity"
                    );
                    result
                })
                .await
                .unwrap();
                assert_eq!(
                    result,
                    index + 10,
                    "factory must execute once for each attempt"
                );
                for label in ["constructor", "publisher", "output-drop", "publisher-drop"] {
                    let rows = events.snapshot();
                    let actual = rows.iter().rfind(|row| row.0 == label).unwrap();
                    assert_eq!(
                        actual.1,
                        identity(&attempt),
                        "construction attempt must keep its own identity: {label}"
                    );
                    assert_eq!(rows.iter().filter(|row| row.0 == label).count(), index);
                }
            }
            trace::scope(polling, factory.retire()).await.unwrap();
            events.require("factory-drop", &origin, 1);
            assert!(TraceContext::current().is_none());
            eprintln!("PARROT_CONSTRUCTOR_TRACE_CASE_OK case=repeat traced={traced}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_publication_and_discard_keep_the_preparation_attempt() {
    within(async {
        for traced in [false, true] {
            for discard in [false, true] {
                let events = Arc::new(Events::default());
                let origin = Some(TraceContext::root());
                let attempt = traced.then(TraceContext::root);
                let polling = Some(TraceContext::root());
                let publication_caller = Some(TraceContext::root());
                let factory = factory(&events, origin.clone());
                let prepare =
                    trace::in_scope(attempt.clone(), || factory.prepare::<()>(Arc::default()));
                let prepared = trace::scope(polling.clone(), prepare).await.unwrap();
                if discard {
                    let cleanup = trace::in_scope(publication_caller, || prepared.discard());
                    trace::scope(polling.clone(), cleanup).await.unwrap();
                    events.require("publisher", &attempt, 0);
                } else {
                    let work = trace::in_scope(publication_caller, || {
                        prepared.publish(publisher(&events))
                    });
                    let result = trace::scope(polling.clone(), work).await.unwrap();
                    assert_eq!(result, 11);
                    events.require("publisher", &attempt, 1);
                    events.require("publisher-drop", &attempt, 1);
                }
                trace::scope(polling, factory.retire()).await.unwrap();
                events.require("constructor", &attempt, 1);
                events.require("output-drop", &attempt, 1);
                events.require("factory-drop", &origin, 1);
                assert!(TraceContext::current().is_none());
                eprintln!(
                    "PARROT_CONSTRUCTOR_TRACE_CASE_OK case=staged traced={traced} discard={discard}"
                );
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_and_unpolled_publication_retire_owned_inputs_under_attempt() {
    within(async {
        for traced in [false, true] {
            for unpolled in [false, true] {
                let events = Arc::new(Events::default());
                let origin = Some(TraceContext::root());
                let attempt = traced.then(TraceContext::root);
                let polling = Some(TraceContext::root());
                let factory = factory(&events, origin.clone());
                let control = Arc::new(ConstructionControl::default());
                let prepare = trace::in_scope(attempt.clone(), || factory.prepare::<()>(control.clone()));
                let prepared = trace::scope(polling.clone(), prepare).await.unwrap();
                let publication = trace::in_scope(Some(TraceContext::root()), ||
                    prepared.publish(publisher(&events)));
                if unpolled {
                    trace::in_scope(polling.clone(), || drop(publication));
                } else {
                    control.cancel();
                    let result = trace::scope(polling.clone(), publication).await;
                    assert!(matches!(result, Err(ConstructionError::Cancelled)));
                }
                // Retirement joins the outstanding prepared output even when
                // its publication waiter was never polled or was cancelled.
                trace::scope(polling, factory.retire()).await.unwrap();
                events.require("constructor", &attempt, 1);
                events.require("output-drop", &attempt, 1);
                events.require("publisher", &attempt, 0);
                events.require("publisher-drop", &attempt, 1);
                events.require("factory-drop", &origin, 1);
                assert!(TraceContext::current().is_none());
                eprintln!("PARROT_CONSTRUCTOR_TRACE_CASE_OK case=cancelled traced={traced} unpolled={unpolled}");
            }
        }
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unpolled_constructor_retires_callback_without_calling_factory() {
    within(async {
        for traced in [false, true] {
            let events = Arc::new(Events::default());
            let attempt = traced.then(TraceContext::root);
            let factory = factory(&events, None);
            let work = trace::in_scope(attempt.clone(), || {
                factory.construct(Arc::default(), publisher(&events))
            });
            trace::in_scope(Some(TraceContext::root()), || drop(work));
            factory.retire().await.unwrap();
            events.require("constructor", &attempt, 0);
            events.require("publisher", &attempt, 0);
            events.require("output-drop", &attempt, 0);
            events.require("publisher-drop", &attempt, 1);
            events.require("factory-drop", &None, 1);
            assert!(TraceContext::current().is_none());
            eprintln!("PARROT_CONSTRUCTOR_TRACE_CASE_OK case=unpolled traced={traced}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn suspended_capacity_waiters_release_inputs_without_borrowing_polling_identity() {
    within(async {
        for traced in [false, true] {
            for staged in [false, true] {
                let events = Arc::new(Events::default());
                let origin = Some(TraceContext::root());
                let attempt = traced.then(TraceContext::root);
                let polling = Some(TraceContext::root());
                let factory = factory(&events, origin.clone());
                if staged {
                    let prepare =
                        trace::in_scope(attempt.clone(), || factory.prepare::<()>(Arc::default()));
                    let prepared = trace::scope(polling.clone(), prepare).await.unwrap();
                    let _permit = factory.workers.clone().acquire_owned().await.unwrap();
                    let work = trace::in_scope(Some(TraceContext::root()), || {
                        prepared.publish(publisher(&events))
                    });
                    let mut work = Box::pin(work);
                    trace::scope(polling.clone(), async {
                        std::future::poll_fn(|cx| {
                            assert!(std::future::Future::poll(work.as_mut(), cx).is_pending());
                            std::task::Poll::Ready(())
                        })
                        .await;
                        drop(work);
                        assert_eq!(current(), identity(&polling));
                    })
                    .await;
                    // The construction permit remains held during cleanup.
                    // Disposal must use the independent cleanup capacity.
                    trace::scope(polling.clone(), factory.retire())
                        .await
                        .unwrap();
                    events.require("constructor", &attempt, 1);
                    events.require("output-drop", &attempt, 1);
                } else {
                    let _permit = factory.workers.clone().acquire_owned().await.unwrap();
                    let work = trace::in_scope(attempt.clone(), || {
                        factory.construct(Arc::default(), publisher(&events))
                    });
                    let mut work = Box::pin(work);
                    trace::scope(polling.clone(), async {
                        std::future::poll_fn(|cx| {
                            assert!(std::future::Future::poll(work.as_mut(), cx).is_pending());
                            std::task::Poll::Ready(())
                        })
                        .await;
                        drop(work);
                        assert_eq!(current(), identity(&polling));
                    })
                    .await;
                    trace::scope(polling, factory.retire()).await.unwrap();
                    events.require("constructor", &attempt, 0);
                    events.require("output-drop", &attempt, 0);
                }
                events.require("publisher", &attempt, 0);
                events.require("publisher-drop", &attempt, 1);
                events.require("factory-drop", &origin, 1);
                assert!(TraceContext::current().is_none());
                eprintln!(
                    "PARROT_CONSTRUCTOR_TRACE_CASE_OK case=capacity traced={traced} staged={staged}"
                );
            }
        }
    })
    .await;
}

#[test]
fn unretired_factory_capture_keeps_creation_identity_on_last_owner_drop() {
    for traced in [false, true] {
        for ambient in [false, true] {
            let events = Arc::new(Events::default());
            let origin = traced.then(TraceContext::root);
            let disposal = ambient.then(TraceContext::root);
            let factory = factory(&events, origin.clone());
            let final_owner = factory.clone();
            trace::in_scope(Some(TraceContext::root()), || drop(factory));
            events.require("factory-drop", &origin, 0);
            trace::in_scope(disposal.clone(), || {
                drop(final_owner);
                assert_eq!(
                    current(),
                    identity(&disposal),
                    "capture disposal must restore its caller identity"
                );
            });
            events.require("constructor", &origin, 0);
            events.require("factory-drop", &origin, 1);
            assert!(TraceContext::current().is_none());
            eprintln!(
                "PARROT_CONSTRUCTOR_TRACE_CASE_OK case=final-owner traced={traced} ambient={ambient}"
            );
        }
    }
}

#[test]
fn staged_output_and_final_capture_keep_distinct_identities_during_runtime_shutdown() {
    for traced in [false, true] {
        for mode in ["already-stopped", "never-polled", "waiting-capacity"] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let events = Arc::new(Events::default());
            let origin = Some(TraceContext::root());
            let attempt = traced.then(TraceContext::root);
            let polling = Some(TraceContext::root());
            let factory = factory(&events, origin.clone());
            let cleanup = factory.cleanup.clone();
            let permit = cleanup.try_acquire_owned().unwrap();
            let prepare =
                trace::in_scope(attempt.clone(), || factory.prepare::<()>(Arc::default()));
            let prepared = runtime
                .block_on(trace::scope(polling.clone(), prepare))
                .unwrap();
            // The prepared invocation now owns the final factory capture.
            drop(factory);
            match mode {
                "already-stopped" => {
                    trace::in_scope(polling.clone(), || {
                        runtime.shutdown_timeout(Duration::from_secs(1))
                    });
                    trace::in_scope(polling.clone(), || drop(prepared));
                }
                "never-polled" => {
                    trace::in_scope(polling.clone(), || drop(prepared));
                    trace::in_scope(polling.clone(), || {
                        runtime.shutdown_timeout(Duration::from_secs(1))
                    });
                }
                "waiting-capacity" => {
                    trace::in_scope(polling.clone(), || drop(prepared));
                    runtime.block_on(tokio::task::yield_now());
                    assert_eq!(
                        events
                            .snapshot()
                            .iter()
                            .filter(|row| row.0 == "output-drop")
                            .count(),
                        0
                    );
                    trace::in_scope(polling.clone(), || {
                        runtime.shutdown_timeout(Duration::from_secs(1))
                    });
                }
                _ => unreachable!(),
            }
            drop(permit);
            events.require("constructor", &attempt, 1);
            events.require("output-drop", &attempt, 1);
            events.require("factory-drop", &origin, 1);
            assert!(TraceContext::current().is_none());
            eprintln!("PARROT_CONSTRUCTOR_TRACE_CASE_OK case=shutdown traced={traced} mode={mode}");
        }
    }
}
