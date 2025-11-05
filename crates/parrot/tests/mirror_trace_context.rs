use parrot::{
    mirror::{MirrorCopier, MirrorPolicy},
    system::ParrotActorSystem,
    thread::{context::ThreadContext, system::ThreadActorSystem},
};
use parrot_api::{
    actor::{Actor, ActorState, EmptyConfig},
    address::{ActorPath, ActorRef},
    system::ActorSystem,
    trace::{self, TraceContext, TraceRecorder},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Default)]
struct Seen {
    handlers: Mutex<Vec<(&'static str, Option<TraceContext>, u64)>>,
    copiers: Mutex<Vec<(&'static str, Option<TraceContext>)>>,
    retired: Mutex<Vec<(bool, Option<TraceContext>)>>,
}
struct Observe {
    name: &'static str,
    seen: Arc<Seen>,
}
impl Actor for Observe {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        message: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            let value = match message.downcast::<u64>() {
                Ok(value) => *value,
                Err(message) => message.downcast::<RetirementInput>().unwrap().value,
            };
            self.seen
                .handlers
                .lock()
                .unwrap()
                .push((self.name, TraceContext::current(), value));
            Ok(Box::new(value + 1) as BoxedMessage)
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}
fn dispatch(actor: &dyn ActorRef, mode: u8) -> BoxedFuture<'_, ActorResult<Option<BoxedMessage>>> {
    dispatch_owned(actor, mode, Box::new(41u64))
}
fn dispatch_owned(
    actor: &dyn ActorRef,
    mode: u8,
    value: BoxedMessage,
) -> BoxedFuture<'_, ActorResult<Option<BoxedMessage>>> {
    match mode {
        0 => {
            let f = actor.send(value);
            Box::pin(async move { f.await.map(Some) })
        }
        1 => {
            let f = actor.send_with_timeout(value, Some(Duration::from_secs(3)));
            Box::pin(async move { f.await.map(Some) })
        }
        2 => {
            let f = actor.send_with_options(value, Default::default());
            Box::pin(async move { f.await.map(Some) })
        }
        3 => {
            let f = actor.deliver(value);
            Box::pin(async move { f.await.map(|()| None) })
        }
        4 => {
            let f = actor.deliver_with_options(value, Default::default());
            Box::pin(async move { f.await.map(|()| None) })
        }
        _ => unreachable!(),
    }
}
async fn exercise(mode: u8, traced: bool, mirrored: bool) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    let seen = Arc::new(Seen::default());
    for (name, path) in [("primary", "/source/primary"), ("shadow", "/shadow")] {
        engine
            .spawn_at(
                Observe {
                    name,
                    seen: seen.clone(),
                },
                path,
                None,
                Default::default(),
            )
            .await
            .unwrap();
    }
    facade
        .register_thread_system("engine".into(), engine, true)
        .await
        .unwrap();
    let measure = seen.clone();
    let copy = seen.clone();
    facade
        .register_mirror_copier(MirrorCopier::new(
            move |_: &u64| {
                measure
                    .copiers
                    .lock()
                    .unwrap()
                    .push(("measure", TraceContext::current()));
                8
            },
            move |v: &u64| {
                copy.copiers
                    .lock()
                    .unwrap()
                    .push(("copy", TraceContext::current()));
                Ok(*v)
            },
        ))
        .unwrap();
    let mirror = mirrored.then(|| {
        facade
            .register_mirror_policy(MirrorPolicy::new("/source/", "/shadow"))
            .unwrap()
    });
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/primary"))
        .await
        .unwrap();
    let original = TraceRecorder::new(32).unwrap();
    let unrelated = TraceRecorder::new(32).unwrap();
    let parent = traced.then(|| original.root());
    let foreign = unrelated.root();
    let call = trace::in_scope(parent.clone(), || dispatch(actor.as_ref(), mode));
    assert!(seen.handlers.lock().unwrap().is_empty());
    assert!(seen.copiers.lock().unwrap().is_empty());
    let reply = foreign.scope(call).await.unwrap();
    assert!(TraceContext::current().is_none());
    if mode < 3 {
        assert_eq!(*reply.unwrap().downcast::<u64>().unwrap(), 42);
    } else {
        assert!(reply.is_none());
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while seen.handlers.lock().unwrap().len() < if mirrored { 2 } else { 1 } {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    if let Some(handle) = mirror {
        tokio::time::timeout(Duration::from_secs(3), async {
            while handle.stats().admitted != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handle.shutdown().await;
        assert_eq!(handle.stats().admitted, 1);
    }
    facade.internal_shutdown().await.unwrap();
    let handlers = seen.handlers.lock().unwrap();
    let copiers = seen.copiers.lock().unwrap();
    assert_eq!(handlers.len(), if mirrored { 2 } else { 1 });
    for (name, actual, value) in handlers.iter() {
        assert_eq!(*value, 41);
        match (&parent, actual) {
            (Some(parent), Some(actual)) => {
                assert_eq!(
                    actual.trace_id(),
                    parent.trace_id(),
                    "{name} adopted the polling context or lost the original trace"
                );
                assert_eq!(
                    actual.parent_span_id(),
                    Some(parent.span_id()),
                    "{name} lost the caller parent"
                );
                assert_ne!(actual.span_id(), parent.span_id());
            }
            (None, None) => {}
            _ => panic!("{name} must retain the construction context, including None: {actual:?}"),
        }
    }
    assert_eq!(copiers.len(), if mirrored { 2 } else { 0 });
    for (stage, actual) in copiers.iter() {
        assert_eq!(
            actual, &parent,
            "{stage} adopted an unrelated polling scope"
        );
    }
    let expected = if traced {
        if mirrored { 2 } else { 1 }
    } else {
        0
    };
    assert_eq!(original.snapshot().records.len(), expected);
    assert_eq!(
        unrelated.snapshot().records.len(),
        0,
        "mirror wrapper polluted an unrelated recorder"
    );
}
macro_rules! contract {
    ($name:ident,$mode:expr,$traced:expr,$mirror:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            exercise($mode, $traced, $mirror).await;
        }
    };
}
contract!(mirror_ask_keeps_creation_parent, 0, true, true);
contract!(mirror_timeout_ask_keeps_creation_parent, 1, true, true);
contract!(mirror_options_ask_keeps_creation_parent, 2, true, true);
contract!(mirror_tell_keeps_creation_parent, 3, true, true);
contract!(mirror_options_tell_keeps_creation_parent, 4, true, true);
contract!(mirror_ask_keeps_explicit_none, 0, false, true);
contract!(mirror_timeout_ask_keeps_explicit_none, 1, false, true);
contract!(mirror_options_ask_keeps_explicit_none, 2, false, true);
contract!(mirror_tell_keeps_explicit_none, 3, false, true);
contract!(mirror_options_tell_keeps_explicit_none, 4, false, true);
contract!(direct_ask_keeps_creation_parent, 0, true, false);
contract!(direct_timeout_ask_keeps_creation_parent, 1, true, false);
contract!(direct_options_ask_keeps_creation_parent, 2, true, false);
contract!(direct_tell_keeps_creation_parent, 3, true, false);
contract!(direct_options_tell_keeps_creation_parent, 4, true, false);
contract!(direct_ask_keeps_explicit_none, 0, false, false);
contract!(direct_timeout_ask_keeps_explicit_none, 1, false, false);
contract!(direct_options_ask_keeps_explicit_none, 2, false, false);
contract!(direct_tell_keeps_explicit_none, 3, false, false);
contract!(direct_options_tell_keeps_explicit_none, 4, false, false);

struct RetirementInput {
    value: u64,
    shadow: bool,
    seen: Arc<Seen>,
}
impl Drop for RetirementInput {
    fn drop(&mut self) {
        self.seen
            .retired
            .lock()
            .unwrap()
            .push((self.shadow, TraceContext::current()));
    }
}
async fn retirement(mode: u8, traced: bool, polled: bool) {
    let facade = ParrotActorSystem::new(Default::default()).await.unwrap();
    let engine = ThreadActorSystem::shared(Default::default());
    let seen = Arc::new(Seen::default());
    engine
        .spawn_at(
            Observe {
                name: "primary",
                seen: seen.clone(),
            },
            "/source/primary",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    facade
        .register_thread_system("engine".into(), engine, true)
        .await
        .unwrap();
    let measure = seen.clone();
    let copy = seen.clone();
    facade
        .register_mirror_copier(MirrorCopier::new(
            move |_: &RetirementInput| {
                measure
                    .copiers
                    .lock()
                    .unwrap()
                    .push(("measure", TraceContext::current()));
                8
            },
            move |value: &RetirementInput| {
                copy.copiers
                    .lock()
                    .unwrap()
                    .push(("copy", TraceContext::current()));
                Ok(RetirementInput {
                    value: value.value,
                    shadow: true,
                    seen: value.seen.clone(),
                })
            },
        ))
        .unwrap();
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/missing"))
        .unwrap();
    let actor = facade
        .get_actor(&ActorPath::placeholder("/source/primary"))
        .await
        .unwrap();
    let parent = traced.then(TraceContext::root);
    let foreign = TraceContext::root();
    let message = Box::new(RetirementInput {
        value: 41,
        shadow: false,
        seen: seen.clone(),
    });
    let call = trace::in_scope(parent.clone(), || {
        dispatch_owned(actor.as_ref(), mode, message)
    });
    if polled {
        let reply = foreign.scope(call).await.unwrap();
        if mode < 3 {
            assert_eq!(*reply.unwrap().downcast::<u64>().unwrap(), 42);
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while seen.handlers.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    } else {
        foreign.in_scope(|| drop(call));
    }
    foreign.scope(handle.shutdown()).await;
    facade.internal_shutdown().await.unwrap();
    assert!(TraceContext::current().is_none());
    let retired = seen.retired.lock().unwrap();
    assert_eq!(retired.len(), if polled { 2 } else { 1 });
    assert_eq!(
        retired.iter().filter(|(shadow, _)| *shadow).count(),
        usize::from(polled)
    );
    for (shadow, actual) in retired.iter() {
        if !shadow && polled {
            match (&parent, actual) {
                (Some(parent), Some(actual)) => {
                    assert_eq!(actual.trace_id(), parent.trace_id());
                    assert_eq!(actual.parent_span_id(), Some(parent.span_id()));
                }
                (None, None) => {}
                _ => panic!("primary input retirement lost its invocation context: {actual:?}"),
            }
        } else {
            assert_eq!(
                actual, &parent,
                "unadmitted input inherited the disposer context"
            );
        }
    }
    assert_eq!(seen.handlers.lock().unwrap().len(), usize::from(polled));
    let copiers = seen.copiers.lock().unwrap();
    assert_eq!(copiers.len(), if polled { 2 } else { 0 });
    for (_, context) in copiers.iter() {
        assert_eq!(context, &parent);
    }
}
macro_rules! retirement_contract {
    ($name:ident,$mode:expr,$traced:expr,$polled:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() {
            retirement($mode, $traced, $polled).await;
        }
    };
}
retirement_contract!(unpolled_ask_retires_with_parent, 0, true, false);
retirement_contract!(unpolled_timeout_ask_retires_with_parent, 1, true, false);
retirement_contract!(unpolled_options_ask_retires_with_parent, 2, true, false);
retirement_contract!(unpolled_tell_retires_with_parent, 3, true, false);
retirement_contract!(unpolled_options_tell_retires_with_parent, 4, true, false);
retirement_contract!(unpolled_ask_retires_with_none, 0, false, false);
retirement_contract!(unpolled_timeout_ask_retires_with_none, 1, false, false);
retirement_contract!(unpolled_options_ask_retires_with_none, 2, false, false);
retirement_contract!(unpolled_tell_retires_with_none, 3, false, false);
retirement_contract!(unpolled_options_tell_retires_with_none, 4, false, false);
retirement_contract!(unadmitted_ask_retires_with_parent, 0, true, true);
retirement_contract!(unadmitted_timeout_ask_retires_with_parent, 1, true, true);
retirement_contract!(unadmitted_options_ask_retires_with_parent, 2, true, true);
retirement_contract!(unadmitted_tell_retires_with_parent, 3, true, true);
retirement_contract!(unadmitted_options_tell_retires_with_parent, 4, true, true);
retirement_contract!(unadmitted_ask_retires_with_none, 0, false, true);
retirement_contract!(unadmitted_timeout_ask_retires_with_none, 1, false, true);
retirement_contract!(unadmitted_options_ask_retires_with_none, 2, false, true);
retirement_contract!(unadmitted_tell_retires_with_none, 3, false, true);
retirement_contract!(unadmitted_options_tell_retires_with_none, 4, false, true);

#[derive(Debug)]
struct Immediate;
#[async_trait::async_trait]
impl ActorRef for Immediate {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async { Ok(Box::new(42u64) as BoxedMessage) })
    }
    fn send_with_timeout<'a>(
        &'a self,
        message: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        self.send(message)
    }
    fn deliver<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/source/primary".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn clone_boxed(&self) -> parrot_api::types::BoxedActorRef {
        Box::new(Self)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn runtime_retirement(traced: bool) {
    // No executor poll is allowed before runtime destruction. An immediate
    // primary isolates the queue's destruction context from actor scheduling.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    let facade = futures::executor::block_on(ParrotActorSystem::new(Default::default())).unwrap();
    let seen = Arc::new(Seen::default());
    facade
        .register_prefix_handler("/source/", Arc::new(Immediate))
        .unwrap();
    facade
        .register_mirror_copier(MirrorCopier::new(
            |_: &RetirementInput| 8,
            |v: &RetirementInput| {
                Ok(RetirementInput {
                    value: v.value,
                    shadow: true,
                    seen: v.seen.clone(),
                })
            },
        ))
        .unwrap();
    let handle = facade
        .register_mirror_policy(MirrorPolicy::new("/source/", "/missing"))
        .unwrap();
    let source =
        futures::executor::block_on(facade.get_actor(&ActorPath::placeholder("/source/primary")))
            .unwrap();
    let parent = traced.then(TraceContext::root);
    let foreign = TraceContext::root();
    let call = trace::in_scope(parent.clone(), || {
        source.send(Box::new(RetirementInput {
            value: 41,
            shadow: false,
            seen: seen.clone(),
        }))
    });
    let reply = futures::executor::block_on(foreign.scope(call)).unwrap();
    assert_eq!(*reply.downcast::<u64>().unwrap(), 42);
    assert_eq!(handle.stats().queued, 1);
    drop(entered);
    foreign.in_scope(|| drop(runtime));
    assert!(TraceContext::current().is_none());
    assert!(handle.is_finished());
    assert_eq!(handle.stats().cancelled, 1);
    let retired = seen.retired.lock().unwrap();
    assert_eq!(retired.len(), 2);
    assert_eq!(retired.iter().filter(|(shadow, _)| *shadow).count(), 1);
    for (_, actual) in retired.iter() {
        assert_eq!(
            actual, &parent,
            "runtime destruction must not supply an unrelated parent"
        );
    }
}
#[test]
fn unpolled_worker_retirement_keeps_original_parent() {
    runtime_retirement(true);
}
#[test]
fn unpolled_worker_retirement_clears_unrelated_context() {
    runtime_retirement(false);
}
