//! Derived local protocols must work through the real owner-thread runtime.
use parrot::actix::ActixActorSystem;
use parrot_api::{
    Message, ParrotTypedActor,
    address::ActorRefExt,
    context::ActorTerminated,
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalTypedActorRef, LocalTypedLifecycle, LocalTypedReceive},
    typed_context::TypedContext,
    types::ActorResult,
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
    thread::ThreadId,
    time::Duration,
};

type Events = Arc<Mutex<Vec<(&'static str, ThreadId)>>>;
fn record(events: &Events, phase: &'static str) {
    events
        .lock()
        .unwrap()
        .push((phase, std::thread::current().id()));
}
pub struct Add(u64);
impl Message for Add {
    type Result = u64;
}
pub struct Read;
impl Message for Read {
    type Result = (u64, String);
}
// An alias checks actual type identity, not spelling of the attribute token.
type Ended = ActorTerminated;
#[derive(Clone, Copy)]
enum Mode {
    Ok,
    InitError,
    StopError,
    HandlerError,
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Add, Read, Ended), local, lifecycle)]
struct Service {
    value: Rc<RefCell<u64>>,
    events: Events,
    mode: Mode,
    terminated: tokio::sync::mpsc::UnboundedSender<ActorTerminated>,
}
impl Service {
    fn new(
        events: Events,
        mode: Mode,
        terminated: tokio::sync::mpsc::UnboundedSender<ActorTerminated>,
    ) -> Self {
        record(&events, "construct");
        Self {
            value: Rc::new(RefCell::new(0)),
            events,
            mode,
            terminated,
        }
    }
}
impl LocalTypedLifecycle for Service {
    fn init_with_context<'a>(
        &'a mut self,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        let value = self.value.clone();
        Box::pin(async move {
            record(&self.events, "init-before");
            tokio::task::yield_now().await;
            record(&self.events, "init-after");
            assert_eq!(context.path(), "/derived-local");
            *value.borrow_mut() = 10;
            if matches!(self.mode, Mode::InitError) {
                return Err(ActorError::InitializationError(
                    "local derived init failed".into(),
                ));
            }
            Ok(())
        })
    }
    fn before_stop_with_context<'a>(
        &'a mut self,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        let value = self.value.clone();
        Box::pin(async move {
            record(&self.events, "stop-before");
            tokio::task::yield_now().await;
            record(&self.events, "stop-after");
            assert_eq!(context.path(), "/derived-local");
            assert!(*value.borrow() >= 10);
            if matches!(self.mode, Mode::StopError) {
                return Err(ActorError::InternalError(
                    "local derived stop failed".into(),
                ));
            }
            Ok(())
        })
    }
}
impl LocalTypedReceive<Add> for Service {
    fn receive_typed(&mut self, Add(amount): Add) -> LocalBoxedFuture<'_, ActorResult<u64>> {
        let value = self.value.clone();
        Box::pin(async move {
            record(&self.events, "handler-before");
            tokio::task::yield_now().await;
            record(&self.events, "handler-after");
            if matches!(self.mode, Mode::HandlerError) {
                return Err(ActorError::MessageHandlingError(
                    "local derived handler failed".into(),
                ));
            }
            *value.borrow_mut() += amount;
            Ok(*value.borrow())
        })
    }
}
impl LocalTypedReceive<Read> for Service {
    fn receive_typed_with_context<'a>(
        &'a mut self,
        _: Read,
        context: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<(u64, String)>> {
        Box::pin(async move { Ok((*self.value.borrow(), context.path().to_owned())) })
    }
}
impl LocalTypedReceive<Ended> for Service {
    fn receive_typed(&mut self, event: Ended) -> LocalBoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move {
            self.terminated.send(event).unwrap();
            Ok(())
        })
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        assert_eq!(Rc::strong_count(&self.value), 1);
        record(&self.events, "drop");
    }
}
async fn setup(
    mode: Mode,
) -> (
    Arc<ActixActorSystem>,
    LocalTypedActorRef<Service, Add>,
    Events,
    tokio::sync::mpsc::UnboundedReceiver<ActorTerminated>,
) {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(2).await.unwrap());
    let events = Events::default();
    let made = events.clone();
    let (send, receive) = tokio::sync::mpsc::unbounded_channel();
    let actor = system
        .spawn_local_typed(
            move || Service::new(made.clone(), mode, send.clone()),
            "/derived-local",
        )
        .await
        .unwrap();
    (system, actor, events, receive)
}
fn assert_events(events: &Events, caller: ThreadId, expected: &[&str]) {
    let events = events.lock().unwrap();
    let owner = events[0].1;
    assert_ne!(owner, caller);
    assert!(events.iter().all(|e| e.1 == owner), "{events:?}");
    assert_eq!(events.iter().map(|e| e.0).collect::<Vec<_>>(), expected);
}
#[actix::test]
async fn derived_local_multi_protocol_cross_thread_and_dynamic_views_share_owner() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LocalTypedActorRef<Service, Add>>();
    let caller = std::thread::current().id();
    let (system, actor, events, _) = setup(Mode::Ok).await;
    let sent = actor.clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || futures::executor::block_on(sent.ask(Add(7))))
            .await
            .unwrap()
            .unwrap(),
        17
    );
    let read = actor.ref_for::<Read>();
    assert_eq!(read.ask(Read).await.unwrap(), (17, "/derived-local".into()));
    let dynamic = read.clone().into_dyn();
    assert_eq!(dynamic.ask(Read).await.unwrap().0, 17);
    let found = system.get_actor(&"/derived-local".into()).await.unwrap();
    assert_eq!(found.ask(Add(5)).await.unwrap(), 22);
    assert_eq!(
        found.death_watch().unwrap().id(),
        dynamic.death_watch().unwrap().id()
    );
    actor.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
    assert_events(
        &events,
        caller,
        &[
            "construct",
            "init-before",
            "init-after",
            "handler-before",
            "handler-after",
            "handler-before",
            "handler-after",
            "stop-before",
            "stop-after",
            "drop",
        ],
    );
}
async fn failure(mode: Mode, diagnostic: &str, expected: &[&str]) {
    let caller = std::thread::current().id();
    let (system, actor, events, _) = setup(mode).await;
    let reply = actor.ask(Add(1)).await;
    if matches!(mode, Mode::StopError) {
        reply.unwrap();
    } else if matches!(mode, Mode::HandlerError) {
        assert!(
            matches!(reply, Err(ActorError::MessageHandlingError(ref text)) if text == diagnostic)
        );
    } else {
        assert!(reply.is_err());
    }
    if matches!(mode, Mode::HandlerError) {
        // Observe natural fault retirement before consulting its retained stop
        // result; an immediate explicit stop can instead cancel that decision.
        tokio::time::timeout(Duration::from_secs(3), async {
            while system.status().active_actors != 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }
    let stopped = actor.stop().await;
    assert!(stopped.unwrap_err().to_string().contains(diagnostic));
    system.as_ref().clone().shutdown().await.unwrap();
    assert_events(&events, caller, expected);
}
#[actix::test]
async fn derived_local_init_error_prevents_handlers_and_normal_stop_hook() {
    failure(
        Mode::InitError,
        "local derived init failed",
        &["construct", "init-before", "init-after", "drop"],
    )
    .await;
}
#[actix::test]
async fn derived_local_stop_error_survives_actual_actor_destruction() {
    failure(
        Mode::StopError,
        "local derived stop failed",
        &[
            "construct",
            "init-before",
            "init-after",
            "handler-before",
            "handler-after",
            "stop-before",
            "stop-after",
            "drop",
        ],
    )
    .await;
}
#[actix::test]
async fn derived_local_handler_error_skips_normal_stop_and_retires_on_owner() {
    failure(
        Mode::HandlerError,
        "local derived handler failed",
        &[
            "construct",
            "init-before",
            "init-after",
            "handler-before",
            "handler-after",
            "drop",
        ],
    )
    .await;
}
#[actix::test]
async fn derived_local_termination_alias_is_delivered_through_non_event_entry_view() {
    let (system, watcher, _, mut events) = setup(Mode::Ok).await;
    watcher.ask(Add(0)).await.unwrap();
    let target = system
        .spawn_local_typed::<Plain, Ping, _>(|| Plain(Rc::new(())), "/watched")
        .await
        .unwrap();
    target.ask(Ping).await.unwrap();
    let target = target.into_dyn();
    let id = target.death_watch().unwrap().id();
    target.watch(watcher.clone().into_dyn()).await.unwrap();
    target.stop().await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.generation, id);
    assert_eq!(event.path, "/watched");
    watcher.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}
pub struct Ping;
impl Message for Ping {
    type Result = ();
}
#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Ping), local)]
struct Plain(Rc<()>);
impl LocalTypedReceive<Ping> for Plain {
    fn receive_typed(&mut self, _: Ping) -> LocalBoxedFuture<'_, ActorResult<()>> {
        let state = self.0.clone();
        Box::pin(async move {
            tokio::task::yield_now().await;
            assert_eq!(Rc::strong_count(&state), 2);
            Ok(())
        })
    }
}
// Opt-in is mandatory even if a matching trait is implemented.
impl LocalTypedLifecycle for Plain {
    fn init(&mut self) -> LocalBoxedFuture<'_, ActorResult<()>> {
        panic!("lifecycle was not requested")
    }
    fn before_stop(&mut self) -> LocalBoxedFuture<'_, ActorResult<()>> {
        panic!("lifecycle was not requested")
    }
}
#[actix::test]
async fn derived_local_does_not_implicitly_enable_lifecycle() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let actor = system
        .spawn_local_typed::<Plain, Ping, _>(|| Plain(Rc::new(())), "/plain")
        .await
        .unwrap();
    actor.ask(Ping).await.unwrap();
    actor.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}
