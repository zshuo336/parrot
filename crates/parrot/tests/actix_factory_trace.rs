use parrot::actix::{ActixActor, ActixActorSystem, ActixContext, ActixSystemSettings};
use parrot_api::{
    actor::{ActixEngineExt, Actor, ActorState, EmptyConfig},
    address::ActorRef,
    context::ActorContext,
    errors::ActorError,
    trace::{self, TraceContext, TraceRecorder},
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
#[derive(Default)]
struct Seen {
    made: AtomicUsize,
    dropped: AtomicUsize,
    handled: Mutex<Vec<(usize, Option<TraceContext>)>>,
    retired: Mutex<Vec<Option<TraceContext>>>,
}
struct Probe(Arc<Seen>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.retired.lock().unwrap().push(TraceContext::current());
    }
}
struct Observe {
    seen: Arc<Seen>,
    generation: usize,
}
impl Drop for Observe {
    fn drop(&mut self) {
        self.seen.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Native;
struct Generation;
struct Crash;
impl ActixEngineExt for Observe {}
impl Actor for Observe {
    type Config = EmptyConfig;
    type Context = ActixContext<ActixActor<Self>>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn use_async_handler(&self) -> bool {
        true
    }
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            assert!(!message.is::<Crash>(), "controlled reconstruction");
            if message.is::<Native>() {
                return Ok(Box::new(ctx.get_self_ref()) as BoxedMessage);
            }
            if message.is::<Generation>() {
                return Ok(Box::new(self.generation) as BoxedMessage);
            }
            let input = message.downcast::<Probe>().unwrap();
            input
                .0
                .handled
                .lock()
                .unwrap()
                .push((self.generation, TraceContext::current()));
            drop(input);
            Ok(Box::new(42_u64) as BoxedMessage)
        })
    }
}
async fn create(seen: &Arc<Seen>) -> (ActixActorSystem, BoxedActorRef) {
    let system = ActixActorSystem::with_settings(ActixSystemSettings {
        arbiter_count: 1,
        shutdown_timeout: Duration::from_secs(3),
        ..Default::default()
    })
    .await
    .unwrap();
    let observed = seen.clone();
    let actor = system
        .spawn_root_factory(move || Observe {
            seen: observed.clone(),
            generation: observed.made.fetch_add(1, Ordering::SeqCst),
        })
        .await
        .unwrap();
    assert_eq!(
        *actor
            .send(Box::new(Generation))
            .await
            .unwrap()
            .downcast::<usize>()
            .unwrap(),
        0
    );
    (system, actor)
}
fn dispatch(
    actor: &dyn ActorRef,
    mode: u8,
    seen: Arc<Seen>,
) -> BoxedFuture<'_, ActorResult<Option<BoxedMessage>>> {
    let input = Box::new(Probe(seen)) as BoxedMessage;
    match mode {
        0 => {
            let f = actor.send(input);
            Box::pin(async move { f.await.map(Some) })
        }
        1 => {
            let f = actor.send_with_timeout(input, Some(Duration::from_secs(3)));
            Box::pin(async move { f.await.map(Some) })
        }
        2 => {
            let f = actor.send_with_options(input, Default::default());
            Box::pin(async move { f.await.map(Some) })
        }
        3 => {
            let f = actor.deliver(input);
            Box::pin(async move { f.await.map(|()| None) })
        }
        4 => {
            let f = actor.deliver_with_options(input, Default::default());
            Box::pin(async move { f.await.map(|()| None) })
        }
        _ => unreachable!(),
    }
}
async fn delivery(mode: u8, traced: bool, kind: u8) {
    let seen = Arc::new(Seen::default());
    let (system, logical) = create(&seen).await;
    let actor = if kind == 0 {
        *logical
            .send(Box::new(Native))
            .await
            .unwrap()
            .downcast::<BoxedActorRef>()
            .unwrap()
    } else {
        logical.clone_boxed()
    };
    let original = TraceRecorder::new(16).unwrap();
    let unrelated = TraceRecorder::new(16).unwrap();
    let parent = traced.then(|| original.root());
    let call = trace::in_scope(parent.clone(), || {
        dispatch(actor.as_ref(), mode, seen.clone())
    });
    assert!(seen.handled.lock().unwrap().is_empty());
    if kind == 2 {
        assert!(logical.send(Box::new(Crash)).await.is_err());
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(reply) = logical.send(Box::new(Generation)).await
                    && *reply.downcast::<usize>().unwrap() == 1
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(seen.dropped.load(Ordering::SeqCst), 1);
    }
    let reply = unrelated.root().scope(call).await.unwrap();
    if mode < 3 {
        assert_eq!(*reply.unwrap().downcast::<u64>().unwrap(), 42);
    } else {
        assert!(reply.is_none());
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while seen.retired.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    logical.stop().await.unwrap();
    system.shutdown().await.unwrap();
    let handlers = seen.handled.lock().unwrap();
    assert_eq!(handlers.len(), 1);
    assert_eq!(
        handlers[0].0,
        usize::from(kind == 2),
        "unpolled logical call must use the current generation"
    );
    assert_eq!(seen.made.load(Ordering::SeqCst), 1 + usize::from(kind == 2));
    assert_eq!(
        seen.dropped.load(Ordering::SeqCst),
        seen.made.load(Ordering::SeqCst)
    );
    match (&parent, &handlers[0].1) {
        (Some(parent), Some(child)) => {
            assert_eq!(
                child.trace_id(),
                parent.trace_id(),
                "factory reference borrowed polling trace"
            );
            assert_eq!(child.parent_span_id(), Some(parent.span_id()));
            assert_ne!(child.span_id(), parent.span_id());
        }
        (None, None) => {}
        _ => panic!(
            "must preserve construction context including None: {:?}",
            handlers[0].1
        ),
    }
    assert_eq!(seen.retired.lock().unwrap().len(), 1);
    assert_eq!(original.snapshot().records.len(), usize::from(traced));
    assert!(unrelated.snapshot().records.is_empty());
    assert!(TraceContext::current().is_none());
}
async fn retirement(mode: u8, traced: bool, closed: bool) {
    let seen = Arc::new(Seen::default());
    let (system, actor) = create(&seen).await;
    let original = TraceRecorder::new(16).unwrap();
    let unrelated = TraceRecorder::new(16).unwrap();
    let parent = traced.then(|| original.root());
    let call = trace::in_scope(parent.clone(), || {
        dispatch(actor.as_ref(), mode, seen.clone())
    });
    if closed {
        actor.stop().await.unwrap();
        assert!(matches!(
            unrelated.root().scope(call).await,
            Err(ActorError::Stopped)
        ));
    } else {
        trace::in_scope(Some(unrelated.root()), || drop(call));
        actor.stop().await.unwrap();
    }
    system.shutdown().await.unwrap();
    assert_eq!(seen.made.load(Ordering::SeqCst), 1);
    assert_eq!(seen.dropped.load(Ordering::SeqCst), 1);
    assert!(seen.handled.lock().unwrap().is_empty());
    assert_eq!(
        *seen.retired.lock().unwrap(),
        vec![parent],
        "unadmitted input adopted its disposer's trace"
    );
    assert!(original.snapshot().records.is_empty());
    assert!(unrelated.snapshot().records.is_empty());
    assert!(TraceContext::current().is_none());
}
macro_rules! delivered {
    ($name:ident, $mode:expr, $traced:expr, $kind:expr) => {
        #[actix::test]
        async fn $name() {
            delivery($mode, $traced, $kind).await;
        }
    };
}
macro_rules! retired {
    ($name:ident, $mode:expr, $traced:expr, $closed:expr) => {
        #[actix::test]
        async fn $name() {
            retirement($mode, $traced, $closed).await;
        }
    };
}
delivered!(native_ask_parent, 0, true, 0);
delivered!(native_timeout_parent, 1, true, 0);
delivered!(native_options_ask_parent, 2, true, 0);
delivered!(native_tell_parent, 3, true, 0);
delivered!(native_options_tell_parent, 4, true, 0);
delivered!(native_ask_none, 0, false, 0);
delivered!(native_timeout_none, 1, false, 0);
delivered!(native_options_ask_none, 2, false, 0);
delivered!(native_tell_none, 3, false, 0);
delivered!(native_options_tell_none, 4, false, 0);
delivered!(logical_ask_parent, 0, true, 1);
delivered!(logical_timeout_parent, 1, true, 1);
delivered!(logical_options_ask_parent, 2, true, 1);
delivered!(logical_tell_parent, 3, true, 1);
delivered!(logical_options_tell_parent, 4, true, 1);
delivered!(logical_ask_none, 0, false, 1);
delivered!(logical_timeout_none, 1, false, 1);
delivered!(logical_options_ask_none, 2, false, 1);
delivered!(logical_tell_none, 3, false, 1);
delivered!(logical_options_tell_none, 4, false, 1);
delivered!(restarted_ask_parent, 0, true, 2);
delivered!(restarted_timeout_parent, 1, true, 2);
delivered!(restarted_options_ask_parent, 2, true, 2);
delivered!(restarted_tell_parent, 3, true, 2);
delivered!(restarted_options_tell_parent, 4, true, 2);
delivered!(restarted_ask_none, 0, false, 2);
delivered!(restarted_timeout_none, 1, false, 2);
delivered!(restarted_options_ask_none, 2, false, 2);
delivered!(restarted_tell_none, 3, false, 2);
delivered!(restarted_options_tell_none, 4, false, 2);
retired!(closed_ask_parent, 0, true, true);
retired!(closed_timeout_parent, 1, true, true);
retired!(closed_options_ask_parent, 2, true, true);
retired!(closed_tell_parent, 3, true, true);
retired!(closed_options_tell_parent, 4, true, true);
retired!(closed_ask_none, 0, false, true);
retired!(closed_timeout_none, 1, false, true);
retired!(closed_options_ask_none, 2, false, true);
retired!(closed_tell_none, 3, false, true);
retired!(closed_options_tell_none, 4, false, true);
retired!(unpolled_ask_parent, 0, true, false);
retired!(unpolled_timeout_parent, 1, true, false);
retired!(unpolled_options_ask_parent, 2, true, false);
retired!(unpolled_tell_parent, 3, true, false);
retired!(unpolled_options_tell_parent, 4, true, false);
retired!(unpolled_ask_none, 0, false, false);
retired!(unpolled_timeout_none, 1, false, false);
retired!(unpolled_options_ask_none, 2, false, false);
retired!(unpolled_tell_none, 3, false, false);
retired!(unpolled_options_tell_none, 4, false, false);
