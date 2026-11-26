// SPDX-License-Identifier: AGPL-3.0-only
use parrot::actix::{ActixActor, ActixActorSystem, ActixContext};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig, EngineContextHandle},
    errors::ActorError,
    supervisor::{DefaultStrategy, SupervisorStrategyType},
    trace::TraceContext,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, ThreadId},
};

type Events = Arc<Mutex<Vec<(&'static str, ThreadId, Option<TraceContext>)>>>;
struct Probe {
    asynchronous: bool,
    fail_once: AtomicBool,
    events: Events,
}
impl Probe {
    fn record(&self, name: &'static str) {
        self.events
            .lock()
            .unwrap()
            .push((name, thread::current().id(), TraceContext::current()));
    }
}
impl Actor for Probe {
    type Context = ActixContext<ActixActor<Self>>;
    type Config = EmptyConfig;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        self.record("select");
        if self.fail_once.swap(false, Ordering::SeqCst) {
            panic!("dispatch mode unavailable");
        }
        self.asynchronous
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.record("async");
        Box::pin(async move { Ok(msg) })
    }
}
impl ActixEngineExt for Probe {
    fn receive_message_with_engine<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
        _: EngineContextHandle<'_>,
    ) -> Option<ActorResult<BoxedMessage>> {
        self.record("sync");
        Some(Ok(msg))
    }
}
async fn check(asynchronous: bool, fail_once: bool) {
    let system = ActixActorSystem::with_arbiter_count(1).await.unwrap();
    let events = Events::default();
    let actor = system
        .spawn_root_typed_with_strategy(
            Probe {
                asynchronous,
                fail_once: AtomicBool::new(fail_once),
                events: events.clone(),
            },
            EmptyConfig,
            SupervisorStrategyType::Default(DefaultStrategy::ResumeOnFailure),
        )
        .await
        .unwrap();
    let first = TraceContext::root();
    let second = TraceContext::root();
    let failure = if fail_once {
        Some(
            first
                .scope(async { actor.send(Box::new(19u32)).await })
                .await,
        )
    } else {
        None
    };
    let reply = second
        .scope(async { actor.send(Box::new(42u32)).await })
        .await;
    let stopped = actor.stop().await;
    let shutdown = system.shutdown().await;
    if let Some(failure) = failure {
        assert!(
            matches!(&failure, Err(ActorError::Panic(message)) if message.contains("dispatch mode unavailable")),
            "{failure:?}"
        );
    }
    assert_eq!(*reply.unwrap().downcast::<u32>().unwrap(), 42);
    stopped.unwrap();
    shutdown.unwrap();
    let events = events.lock().unwrap();
    let (selection, handler) = if fail_once {
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].0, "select");
        let failed = events[0].2.as_ref().unwrap();
        assert_eq!(
            (failed.trace_id(), failed.parent_span_id()),
            (first.trace_id(), Some(first.span_id()))
        );
        assert_eq!(events[0].1, events[1].1);
        (&events[1], &events[2])
    } else {
        assert_eq!(events.len(), 2);
        (&events[0], &events[1])
    };
    assert_eq!(selection.0, "select");
    assert_eq!(handler.0, if asynchronous { "async" } else { "sync" });
    assert_eq!(selection.1, handler.1);
    assert_ne!(selection.1, thread::current().id());
    let context = handler.2.as_ref().unwrap();
    assert_eq!(
        (context.trace_id(), context.parent_span_id()),
        (second.trace_id(), Some(second.span_id()))
    );
    assert_eq!(
        selection.2.as_ref(),
        Some(context),
        "dispatch selector lost message context"
    );
}

#[actix::test]
async fn sync_selector_observes_message_context_on_arbiter() {
    check(false, false).await;
}
#[actix::test]
async fn async_selector_observes_message_context_on_arbiter() {
    check(true, false).await;
}
#[actix::test]
async fn selector_panic_is_reported_and_resume_preserves_actor() {
    check(false, true).await;
}
