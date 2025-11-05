use super::*;
use parrot_api::{
    actor::{ActorState, EmptyConfig},
    context::ActorContext,
    engine::{AsyncFactoryRuntime, FactoryOptions, RuntimeActor, SpawnOptions},
    trace::{self, TraceContext},
};
use std::{any::Any, future::Future, time::Duration};

#[derive(Debug, Default)]
struct Events {
    rows: Mutex<Vec<(&'static str, Option<TraceContext>)>>,
    changed: tokio::sync::Notify,
}
impl Events {
    fn note(&self, label: &'static str) {
        self.rows
            .lock()
            .unwrap()
            .push((label, TraceContext::current()));
        self.changed.notify_waiters();
    }
    fn count(&self, label: &str) -> usize {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.0 == label)
            .count()
    }
    fn require(&self, label: &str, expected: &Option<TraceContext>, count: usize) {
        let rows = self.rows.lock().unwrap().clone();
        let actual: Vec<_> = rows
            .iter()
            .filter(|row| row.0 == label)
            .map(|row| row.1.clone())
            .collect();
        assert_eq!(
            actual,
            vec![expected.clone(); count],
            "factory admission must preserve its call identity: {label}; {rows:?}"
        );
    }
    async fn wait(&self, label: &str, count: usize) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.count(label) >= count {
                return;
            }
            changed.await;
        }
    }
}
#[derive(Debug)]
struct OnDrop {
    label: &'static str,
    events: Arc<Events>,
}
impl Drop for OnDrop {
    fn drop(&mut self) {
        self.events.note(self.label);
    }
}
impl super::super::mailbox::admission::MessageAdmission for OnDrop {
    fn prepare(
        &self,
        message: BoxedMessage,
    ) -> Result<BoxedMessage, super::super::error::MailboxError> {
        Ok(message)
    }
}
struct Native(Arc<Events>);
struct Erased(Arc<Events>);
macro_rules! echo {
    ($actor:ident, $config:ty, $context:ty) => {
        impl Actor for $actor {
            type Config = $config;
            type Context = $context;
            fn state(&self) -> ActorState {
                ActorState::Running
            }
            fn receive_message<'a>(
                &'a mut self,
                message: BoxedMessage,
                _: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    self.0.note("handler");
                    Ok(message)
                })
            }
        }
    };
}
echo!(Native, EmptyConfig, ThreadContext<Self>);
echo!(Erased, Box<dyn Any + Send>, dyn ActorContext);

#[derive(Debug)]
enum Failure {
    Native(SpawnError),
    Runtime(parrot_api::system::SystemError),
}
fn spawn<'a>(
    system: &'a Arc<ThreadActorSystem>,
    events: &Arc<Events>,
    path: &'a str,
    erased: bool,
    invalid: bool,
) -> BoxedFuture<'a, Result<BoxedActorRef, Failure>> {
    let capture = OnDrop {
        label: "factory-drop",
        events: events.clone(),
    };
    let observed = events.clone();
    if erased {
        let factory = Box::new(move || {
            std::hint::black_box(&capture);
            observed.note("constructor");
            Box::new(Erased(observed.clone())) as RuntimeActor
        });
        let work = system.spawn_factory(
            factory,
            FactoryOptions {
                spawn: SpawnOptions::at(path),
                max_restarts: 0,
                within: if invalid {
                    Duration::ZERO
                } else {
                    Duration::from_secs(60)
                },
            },
        );
        Box::pin(async move { work.await.map_err(Failure::Runtime) })
    } else {
        let factory = move || {
            std::hint::black_box(&capture);
            observed.note("constructor");
            Native(observed.clone())
        };
        let work = system.spawn_supervised_with_config(
            factory,
            path,
            ThreadActorConfig {
                supervisor_strategy: Some(super::super::config::SupervisorStrategy::Stop),
                message_batch_size: invalid.then_some(0),
                message_admission: Some(Arc::new(OnDrop {
                    label: "policy-drop",
                    events: events.clone(),
                })),
                ..Default::default()
            },
        );
        Box::pin(async move {
            work.await
                .map(|reference| reference.clone_boxed())
                .map_err(Failure::Native)
        })
    }
}
fn system() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(super::super::config::ThreadActorSystemConfig {
        shared_pool_size: 1,
        shared_burst_workers_max: 0,
        shutdown_timeout: Duration::from_secs(5),
        ..Default::default()
    })
}
async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("factory admission independent liveness watchdog")
}
async fn poll_under<T>(context: Option<TraceContext>, future: impl Future<Output = T>) -> T {
    trace::scope(context.clone(), async move {
        let result = future.await;
        assert_eq!(
            TraceContext::current(),
            context,
            "admission must restore the polling identity"
        );
        result
    })
    .await
}
fn require_retirement(events: &Events, origin: &Option<TraceContext>, erased: bool) {
    events.require("factory-drop", origin, 1);
    events.require("policy-drop", origin, usize::from(!erased));
}
fn require_handler(events: &Events, parent: &Option<TraceContext>, count: usize) {
    let rows = events.rows.lock().unwrap().clone();
    let handlers: Vec<_> = rows.iter().filter(|row| row.0 == "handler").collect();
    assert_eq!(handlers.len(), count);
    let actual = &handlers.last().unwrap().1;
    match (actual, parent) {
        (None, None) => {}
        (Some(actual), Some(parent)) => {
            assert_eq!(
                actual.trace_id(),
                parent.trace_id(),
                "handler must use the message's trace"
            );
            assert_eq!(
                actual.parent_span_id(),
                Some(parent.span_id()),
                "handler must descend from the message"
            );
            assert_ne!(actual.span_id(), parent.span_id());
        }
        _ => panic!("handler inherited factory admission identity: {rows:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_and_erased_roots_keep_call_identity_without_leaking_it_to_messages() {
    within(async {
        for erased in [false, true] {
            for traced in [false, true] {
                for poll_traced in [false, true] {
                    let system = system();
                    let events = Arc::new(Events::default());
                    let origin = traced.then(TraceContext::root);
                    let polling = poll_traced.then(TraceContext::root);
                    let work = trace::in_scope(origin.clone(), || spawn(&system, &events, "/initial", erased, false));
                    let reference = poll_under(polling.clone(), work).await.unwrap();
                    for index in 0..2 {
                        let parent = (index == 0).then(TraceContext::root);
                        let work = trace::in_scope(parent.clone(), || reference.send(Box::new(73_u64)));
                        let reply = poll_under(polling.clone(), work).await.unwrap();
                        assert_eq!(*reply.downcast::<u64>().unwrap(), 73);
                        require_handler(&events, &parent, index + 1);
                    }
                    // Retained reference/config handles cannot postpone final capture retirement.
                    poll_under(polling, reference.stop()).await.unwrap();
                    system.shutdown_internal().await.unwrap();
                    events.require("constructor", &origin, 1);
                    require_retirement(&events, &origin, erased);
                    assert!(system.get_actor_ref("/initial").is_none());
                    assert!(TraceContext::current().is_none());
                    eprintln!("PARROT_FACTORY_ADMISSION_CASE_OK case=success erased={erased} traced={traced} poll_traced={poll_traced}");
                }
            }
        }
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_roots_retire_call_owned_captures_without_executing_constructor() {
    within(async {
        for erased in [false, true] {
            for traced in [false, true] {
                for rejection in ["duplicate", "invalid", "shutdown"] {
                    let system = system();
                    if rejection == "duplicate" {
                        system.spawn_at(Native(Arc::default()), "/rejected", None, Default::default()).await.unwrap();
                    }
                    if rejection == "shutdown" { system.shutdown_internal().await.unwrap(); }
                    let events = Arc::new(Events::default());
                    let origin = traced.then(TraceContext::root);
                    let polling = Some(TraceContext::root());
                    let work = trace::in_scope(origin.clone(), || spawn(&system, &events, "/rejected", erased, rejection == "invalid"));
                    let error = poll_under(polling, work).await.unwrap_err();
                    match (erased, rejection, error) {
                        (false, "duplicate", Failure::Native(SpawnError::ActorPathAlreadyExists(path))) => assert_eq!(path, "/rejected"),
                        (false, "invalid", Failure::Native(SpawnError::InvalidConfig(message))) => assert_eq!(message, "message_batch_size must be in 1..=256"),
                        (false, "shutdown", Failure::Native(SpawnError::SystemShutdown)) => {},
                        (true, "invalid", Failure::Runtime(parrot_api::system::SystemError::ActorCreationError(message))) => assert_eq!(message, "factory restart window must be positive"),
                        (true, "duplicate", Failure::Runtime(parrot_api::system::SystemError::ActorCreationError(message))) => assert_eq!(message, SpawnError::ActorPathAlreadyExists("/rejected".into()).to_string()),
                        (true, "shutdown", Failure::Runtime(parrot_api::system::SystemError::ActorCreationError(message))) => assert_eq!(message, SpawnError::SystemShutdown.to_string()),
                        (_, _, error) => panic!("wrong factory admission rejection: {rejection}: {error:?}"),
                    }
                    system.shutdown_internal().await.unwrap();
                    events.require("constructor", &origin, 0);
                    require_retirement(&events, &origin, erased);
                    assert!(TraceContext::current().is_none());
                    eprintln!("PARROT_FACTORY_ADMISSION_CASE_OK case=rejected erased={erased} traced={traced} reason={rejection}");
                }
            }
        }
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unpolled_roots_retire_in_call_scope_without_reserving_paths() {
    within(async {
        for erased in [false, true] {
            for traced in [false, true] {
                let system = system();
                let events = Arc::new(Events::default());
                let origin = traced.then(TraceContext::root);
                let work = trace::in_scope(origin.clone(), || {
                    spawn(&system, &events, "/unpolled", erased, false)
                });
                trace::in_scope(Some(TraceContext::root()), || drop(work));
                events.wait("factory-drop", 1).await;
                if !erased {
                    events.wait("policy-drop", 1).await;
                }
                assert_eq!(system.actor_count(), 0);
                events.require("constructor", &origin, 0);
                require_retirement(&events, &origin, erased);
                system.shutdown_internal().await.unwrap();
                assert!(TraceContext::current().is_none());
                eprintln!(
                    "PARROT_FACTORY_ADMISSION_CASE_OK case=unpolled erased={erased} traced={traced}"
                );
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_capacity_waiters_retire_without_waiting_for_constructor_capacity() {
    within(async {
        for erased in [false, true] {
            for traced in [false, true] {
                let system = system();
                let held = system.factory_slots.clone().acquire_owned().await.unwrap();
                let events = Arc::new(Events::default());
                let origin = traced.then(TraceContext::root);
                let polling = Some(TraceContext::root());
                let work = trace::in_scope(origin.clone(), || spawn(&system, &events, "/queued", erased, false));
                let mut work = Box::pin(work);
                poll_under(polling.clone(), std::future::poll_fn(|cx| {
                    assert!(work.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })).await;
                assert!(system.get_actor_ref("/queued").is_some(), "pending admission must reserve the path");
                trace::in_scope(polling, || drop(work));
                events.wait("factory-drop", 1).await;
                if !erased { events.wait("policy-drop", 1).await; }
                events.require("constructor", &origin, 0);
                require_retirement(&events, &origin, erased);
                // Cleanup above must finish while all constructor capacity is held.
                drop(held);
                system.shutdown_internal().await.unwrap();
                assert!(system.get_actor_ref("/queued").is_none());
                assert!(TraceContext::current().is_none());
                eprintln!("PARROT_FACTORY_ADMISSION_CASE_OK case=cancelled-capacity erased={erased} traced={traced}");
            }
        }
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn roots_queued_together_keep_distinct_identities_after_capacity_is_released() {
    within(async {
        for erased in [false, true] {
            let system = system();
            let held = system.factory_slots.clone().acquire_owned().await.unwrap();
            let first = Arc::new(Events::default());
            let second = Arc::new(Events::default());
            let first_parent = Some(TraceContext::root());
            let second_parent = Some(TraceContext::root());
            let one = trace::in_scope(first_parent.clone(), || {
                spawn(&system, &first, "/one", erased, false)
            });
            let two = trace::in_scope(second_parent.clone(), || {
                spawn(&system, &second, "/two", erased, false)
            });
            let mut one = Box::pin(one);
            let mut two = Box::pin(two);
            let polling = Some(TraceContext::root());
            poll_under(
                polling.clone(),
                std::future::poll_fn(|cx| {
                    assert!(one.as_mut().poll(cx).is_pending());
                    assert!(two.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                }),
            )
            .await;
            assert!(system.get_actor_ref("/one").is_some());
            assert!(system.get_actor_ref("/two").is_some());
            assert_eq!(first.count("constructor"), 0);
            assert_eq!(second.count("constructor"), 0);
            drop(held);
            let (one, two) = poll_under(polling.clone(), async { tokio::join!(one, two) }).await;
            let one = one.unwrap();
            let two = two.unwrap();
            poll_under(polling, async { tokio::try_join!(one.stop(), two.stop()) })
                .await
                .unwrap();
            system.shutdown_internal().await.unwrap();
            first.require("constructor", &first_parent, 1);
            second.require("constructor", &second_parent, 1);
            require_retirement(&first, &first_parent, erased);
            require_retirement(&second, &second_parent, erased);
            assert!(TraceContext::current().is_none());
            eprintln!("PARROT_FACTORY_ADMISSION_CASE_OK case=queued-pair erased={erased}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_constructors_report_the_original_failure_and_retire_their_captures() {
    within(async {
        for erased in [false, true] {
            for traced in [false, true] {
                let system = system();
                let events = Arc::new(Events::default());
                let origin = traced.then(TraceContext::root);
                let capture = OnDrop {
                    label: "factory-drop",
                    events: events.clone(),
                };
                let observed = events.clone();
                let work: BoxedFuture<'_, Result<BoxedActorRef, Failure>> =
                    trace::in_scope(origin.clone(), || {
                        if erased {
                            let work = system.spawn_factory(
                                Box::new(move || -> RuntimeActor {
                                    std::hint::black_box(&capture);
                                    observed.note("constructor");
                                    panic!("factory admission failure");
                                }),
                                FactoryOptions {
                                    spawn: SpawnOptions::at("/panic"),
                                    max_restarts: 0,
                                    ..Default::default()
                                },
                            );
                            Box::pin(async move { work.await.map_err(Failure::Runtime) })
                                as BoxedFuture<'_, _>
                        } else {
                            let work = system.spawn_supervised(
                                move || -> Native {
                                    std::hint::black_box(&capture);
                                    observed.note("constructor");
                                    panic!("factory admission failure");
                                },
                                "/panic",
                                super::super::config::SupervisorStrategy::Stop,
                            );
                            Box::pin(async move {
                                work.await
                                    .map(|reference| reference.clone_boxed())
                                    .map_err(Failure::Native)
                            }) as BoxedFuture<'_, _>
                        }
                    });
                let result = poll_under(Some(TraceContext::root()), work).await;
                system.shutdown_internal().await.unwrap();
                let expected = "actor factory panicked: factory admission failure";
                match (erased, result) {
                    (false, Err(Failure::Native(SpawnError::InitializationFailed(message)))) => {
                        assert_eq!(message, expected)
                    }
                    (
                        true,
                        Err(Failure::Runtime(parrot_api::system::SystemError::ActorCreationError(
                            message,
                        ))),
                    ) => assert_eq!(
                        message,
                        SpawnError::InitializationFailed(expected.into()).to_string()
                    ),
                    (_, result) => panic!("wrong constructor panic result: {result:?}"),
                }
                events.require("constructor", &origin, 1);
                events.require("factory-drop", &origin, 1);
                assert!(system.get_actor_ref("/panic").is_none());
                assert!(TraceContext::current().is_none());
                eprintln!(
                    "PARROT_FACTORY_ADMISSION_CASE_OK case=panic erased={erased} traced={traced}"
                );
            }
        }
    })
    .await;
}
