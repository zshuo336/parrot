use futures::stream;
use parrot::actix::{ActixActorSystem, ActixSystemSettings};
use parrot_api::{
    address::ActorRefExt,
    errors::ActorError,
    local_typed::{LocalBoxedFuture, LocalProtocol, LocalTypedDispatch},
    message::Message,
    typed::ParrotMsgVariant,
    typed_context::TypedContext,
    types::ActorResult,
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::ThreadId,
    time::Duration,
};

type Events = Arc<Mutex<Vec<(&'static str, ThreadId)>>>;
fn record(events: &Events, name: &'static str) {
    events
        .lock()
        .unwrap()
        .push((name, std::thread::current().id()));
}
pub struct Add(u64);
impl Message for Add {
    type Result = u64;
}
struct LocalCounter {
    state: Rc<RefCell<u64>>,
    events: Events,
    init_gate: Option<Arc<tokio::sync::Notify>>,
    drop_gate: Option<Arc<Gate>>,
}
impl LocalCounter {
    fn new(events: Events) -> Self {
        record(&events, "construct");
        Self {
            state: Rc::new(RefCell::new(0)),
            events,
            init_gate: None,
            drop_gate: None,
        }
    }
}
impl Drop for LocalCounter {
    fn drop(&mut self) {
        if let Some(gate) = &self.drop_gate {
            gate.block();
        }
        assert_eq!(
            Rc::strong_count(&self.state),
            1,
            "handler state survived actor retirement"
        );
        record(&self.events, "actor_drop");
    }
}
struct PollOwner {
    events: Events,
    _state: Rc<RefCell<u64>>,
}
impl Drop for PollOwner {
    fn drop(&mut self) {
        record(&self.events, "future_drop");
    }
}
impl LocalTypedDispatch for LocalCounter {
    type Msg = Add;
    type Reply = u64;
    fn init<'a>(&'a mut self, context: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        record(&self.events, "init_construct");
        Box::pin(async move {
            record(&self.events, "init_poll");
            assert_eq!(context.actor_ref().unwrap().path(), context.path());
            if let Some(gate) = &self.init_gate {
                gate.notified().await;
            }
            Ok(())
        })
    }
    fn dispatch<'a>(
        &'a mut self,
        message: Add,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<u64>> {
        record(&self.events, "future_construct");
        let owner = PollOwner {
            events: self.events.clone(),
            _state: self.state.clone(),
        };
        Box::pin(async move {
            let _owner = owner;
            record(&self.events, "poll_before_await");
            tokio::task::yield_now().await;
            record(&self.events, "poll_after_await");
            *self.state.borrow_mut() += message.0;
            Ok(*self.state.borrow())
        })
    }
    fn before_stop<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            record(&self.events, "before_stop");
            Ok(())
        })
    }
}
impl ParrotMsgVariant<LocalProtocol<LocalCounter>> for Add {
    fn inject(message: Self) -> Add {
        message
    }
    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}
fn assert_owner(events: &Events, caller: ThreadId) {
    let events = events.lock().unwrap();
    assert!(!events.is_empty());
    let owner = events[0].1;
    assert_ne!(owner, caller);
    assert!(events.iter().all(|(_, id)| *id == owner), "{events:?}");
    assert_eq!(
        events
            .iter()
            .filter(|(name, _)| *name == "construct")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|(name, _)| *name == "actor_drop")
            .count(),
        1
    );
}

#[actix::test]
async fn rc_actor_and_local_future_stay_on_owner_with_real_lookup() {
    let caller = std::thread::current().id();
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let events = Events::default();
    let factory_events = events.clone();
    let actor = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            move || LocalCounter::new(factory_events.clone()),
            "/local/counter",
        )
        .await
        .unwrap();
    assert_eq!(system.status().active_actors, 1);
    let remote_thread = actor.clone();
    assert_eq!(
        tokio::spawn(async move { remote_thread.ask(Add(7)).await.unwrap() })
            .await
            .unwrap(),
        7
    );
    let found = system.get_actor(&"/local/counter".into()).await.unwrap();
    assert_eq!(found.ask(Add(5)).await.unwrap(), 12);
    assert_eq!(
        found.death_watch().unwrap().id(),
        actor.clone().into_dyn().death_watch().unwrap().id()
    );
    found.stop().await.unwrap();
    assert!(!actor.is_alive());
    assert!(system.get_actor(&"/local/counter".into()).await.is_none());
    system.as_ref().clone().shutdown().await.unwrap();
    assert_owner(&events, caller);
    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|(name, _)| *name == "future_drop")
            .count(),
        2
    );
    assert_eq!(events[events.len() - 2].0, "before_stop");
    assert_eq!(events.last().unwrap().0, "actor_drop");
}

struct LocalStreamProbe {
    events: Events,
    values: Arc<Mutex<Vec<u32>>>,
}

impl LocalTypedDispatch for LocalStreamProbe {
    type Msg = Add;
    type Reply = u64;

    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        streams: &'a mut parrot_api::local_typed::LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        streams
            .add_stream(stream::iter(vec![Rc::new(1_u32), Rc::new(2_u32)]))
            .unwrap();
        Box::pin(async { Ok(()) })
    }

    fn local_stream_started<'a>(
        &'a mut self,
        _: u64,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        record(&self.events, "stream_started");
        Box::pin(async { Ok(()) })
    }

    fn local_stream_item<'a>(
        &'a mut self,
        _: u64,
        item: Box<dyn std::any::Any + 'static>,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        let value = *item.downcast::<Rc<u32>>().expect("local Rc item");
        self.values.lock().unwrap().push(*value);
        record(&self.events, "stream_item");
        Box::pin(async { Ok(()) })
    }

    fn local_stream_finished<'a>(
        &'a mut self,
        _: u64,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        record(&self.events, "stream_finished");
        Box::pin(async { Ok(()) })
    }

    fn dispatch<'a>(
        &'a mut self,
        message: Add,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move { Ok(message.0) })
    }
}

impl ParrotMsgVariant<LocalProtocol<LocalStreamProbe>> for Add {
    fn inject(message: Self) -> Add {
        message
    }

    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}

#[actix::test]
async fn local_stream_stays_on_owner_and_applies_backpressure() {
    let caller = std::thread::current().id();
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let events = Events::default();
    let events_for_actor = events.clone();
    let values = Arc::new(Mutex::new(Vec::new()));
    let values_for_actor = values.clone();
    let actor = system
        .spawn_local_typed::<LocalStreamProbe, Add, _>(
            move || LocalStreamProbe {
                events: events_for_actor.clone(),
                values: values_for_actor.clone(),
            },
            "/local/stream",
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let values_ready = values.lock().unwrap().len() == 2;
            let finished = events
                .lock()
                .unwrap()
                .iter()
                .any(|(name, _)| *name == "stream_finished");
            if values_ready && finished {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*values.lock().unwrap(), vec![1, 2]);
    let snapshot = events.lock().unwrap().clone();
    assert_eq!(
        snapshot.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        vec![
            "stream_started",
            "stream_item",
            "stream_item",
            "stream_finished"
        ]
    );
    assert!(snapshot.iter().all(|(_, thread)| *thread != caller));
    actor.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}

struct FairStreamProbe {
    items: Arc<AtomicUsize>,
    errors: Arc<AtomicUsize>,
}

impl LocalTypedDispatch for FairStreamProbe {
    type Msg = Add;
    type Reply = u64;

    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        streams: &'a mut parrot_api::local_typed::LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        streams
            .add_stream(stream::iter(0_u32..1_000))
            .expect("stream registration");
        Box::pin(async { Ok(()) })
    }

    fn local_stream_item<'a>(
        &'a mut self,
        _: u64,
        item: Box<dyn std::any::Any + 'static>,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        let _ = item.downcast::<u32>().expect("u32 item");
        self.items.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn local_stream_error<'a>(
        &'a mut self,
        _: u64,
        _: ActorError,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.errors.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn dispatch<'a>(
        &'a mut self,
        _: Add,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<u64>> {
        let items = self.items.load(Ordering::SeqCst) as u64;
        Box::pin(async move { Ok(items) })
    }
}

impl ParrotMsgVariant<LocalProtocol<FairStreamProbe>> for Add {
    fn inject(message: Self) -> Add {
        message
    }

    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}

#[actix::test]
async fn continuously_ready_local_stream_does_not_starve_mailbox() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let items = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let actor = system
        .spawn_local_typed::<FairStreamProbe, Add, _>(
            {
                let items = items.clone();
                let errors = errors.clone();
                move || FairStreamProbe {
                    items: items.clone(),
                    errors: errors.clone(),
                }
            },
            "/local/fair",
        )
        .await
        .unwrap();
    let observed = actor.ask(Add(0)).await.unwrap();
    assert!(
        observed < 32,
        "mailbox was delayed behind too many stream items: {observed}"
    );
    assert_eq!(errors.load(Ordering::SeqCst), 0);
    actor.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}

struct ErrorStreamProbe {
    items: Arc<AtomicUsize>,
    errors: Arc<AtomicUsize>,
}

impl LocalTypedDispatch for ErrorStreamProbe {
    type Msg = Add;
    type Reply = u64;

    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        streams: &'a mut parrot_api::local_typed::LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        streams.add_stream(stream::iter(vec![1_u32, 2])).unwrap();
        Box::pin(async { Ok(()) })
    }

    fn local_stream_item<'a>(
        &'a mut self,
        _: u64,
        item: Box<dyn std::any::Any + 'static>,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        let _ = item.downcast::<u32>().expect("u32 item");
        let items = self.items.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move {
            if items == 1 {
                Err(ActorError::MessageHandlingError(
                    "stream callback failed".into(),
                ))
            } else {
                Ok(())
            }
        })
    }

    fn local_stream_error<'a>(
        &'a mut self,
        _: u64,
        _: ActorError,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        self.errors.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn dispatch<'a>(
        &'a mut self,
        _: Add,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async { Ok(0) })
    }
}

impl ParrotMsgVariant<LocalProtocol<ErrorStreamProbe>> for Add {
    fn inject(message: Self) -> Add {
        message
    }

    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}

#[actix::test]
async fn local_stream_callback_error_retires_stream_and_notifies_actor() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let items = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let actor = system
        .spawn_local_typed::<ErrorStreamProbe, Add, _>(
            {
                let items = items.clone();
                let errors = errors.clone();
                move || ErrorStreamProbe {
                    items: items.clone(),
                    errors: errors.clone(),
                }
            },
            "/local/stream-error",
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while errors.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(items.load(Ordering::SeqCst), 1);
    assert_eq!(errors.load(Ordering::SeqCst), 1);
    assert!(actor.ask(Add(0)).await.is_err());
    assert!(actor.stop().await.is_err());
    system.as_ref().clone().shutdown().await.unwrap();
}

struct PanickingPollStream;

impl futures::Stream for PanickingPollStream {
    type Item = u32;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        panic!("local source poll failed")
    }
}

struct SourceErrorProbe {
    errors: Arc<AtomicUsize>,
}

impl LocalTypedDispatch for SourceErrorProbe {
    type Msg = Add;
    type Reply = u64;

    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        streams: &'a mut parrot_api::local_typed::LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        streams.add_stream(PanickingPollStream).unwrap();
        Box::pin(async { Ok(()) })
    }

    fn local_stream_error<'a>(
        &'a mut self,
        _: u64,
        error: ActorError,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        assert!(error.to_string().contains("source poll failed"));
        self.errors.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn dispatch<'a>(
        &'a mut self,
        _: Add,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async { Ok(0) })
    }
}

impl ParrotMsgVariant<LocalProtocol<SourceErrorProbe>> for Add {
    fn inject(message: Self) -> Add {
        message
    }

    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}

#[actix::test]
async fn local_stream_source_panic_notifies_and_retires_actor() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let errors = Arc::new(AtomicUsize::new(0));
    let actor = system
        .spawn_local_typed::<SourceErrorProbe, Add, _>(
            {
                let errors = errors.clone();
                move || SourceErrorProbe {
                    errors: errors.clone(),
                }
            },
            "/local/source-error",
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while errors.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(errors.load(Ordering::SeqCst), 1);
    assert!(actor.ask(Add(0)).await.is_err());
    assert!(system.as_ref().clone().shutdown().await.is_err());
}

struct PendingDropStream {
    drops: Arc<AtomicUsize>,
    _local: Rc<()>,
}

impl futures::Stream for PendingDropStream {
    type Item = u32;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Pending
    }
}

impl Drop for PendingDropStream {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct PendingStreamProbe {
    drops: Arc<AtomicUsize>,
}

impl LocalTypedDispatch for PendingStreamProbe {
    type Msg = Add;
    type Reply = u64;

    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        streams: &'a mut parrot_api::local_typed::LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        streams
            .add_stream(PendingDropStream {
                drops: self.drops.clone(),
                _local: Rc::new(()),
            })
            .unwrap();
        Box::pin(async { Ok(()) })
    }

    fn dispatch<'a>(
        &'a mut self,
        _: Add,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async { Ok(0) })
    }
}

impl ParrotMsgVariant<LocalProtocol<PendingStreamProbe>> for Add {
    fn inject(message: Self) -> Add {
        message
    }

    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}

#[actix::test]
async fn stopping_local_actor_retires_pending_stream_source() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let drops = Arc::new(AtomicUsize::new(0));
    let actor = system
        .spawn_local_typed::<PendingStreamProbe, Add, _>(
            {
                let drops = drops.clone();
                move || PendingStreamProbe {
                    drops: drops.clone(),
                }
            },
            "/local/pending-stream",
        )
        .await
        .unwrap();
    assert_eq!(actor.ask(Add(0)).await.unwrap(), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    actor.stop().await.unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    system.as_ref().clone().shutdown().await.unwrap();
}

struct PanicDropStream;

impl futures::Stream for PanicDropStream {
    type Item = u32;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Pending
    }
}

impl Drop for PanicDropStream {
    fn drop(&mut self) {
        panic!("local stream drop failed")
    }
}

struct PanicDropStreamProbe;

impl LocalTypedDispatch for PanicDropStreamProbe {
    type Msg = Add;
    type Reply = u64;

    fn init_local_streams<'a>(
        &'a mut self,
        _: &'a TypedContext,
        streams: &'a mut parrot_api::local_typed::LocalStreamContext<'a>,
    ) -> LocalBoxedFuture<'a, ActorResult<()>> {
        streams.add_stream(PanicDropStream).unwrap();
        Box::pin(async { Ok(()) })
    }

    fn dispatch<'a>(
        &'a mut self,
        _: Add,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async { Ok(0) })
    }
}

impl ParrotMsgVariant<LocalProtocol<PanicDropStreamProbe>> for Add {
    fn inject(message: Self) -> Add {
        message
    }

    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}

#[actix::test]
async fn local_stream_drop_panic_is_reported_without_abort() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let actor = system
        .spawn_local_typed::<PanicDropStreamProbe, Add, _>(
            || PanicDropStreamProbe,
            "/local/panic-drop-stream",
        )
        .await
        .unwrap();
    assert_eq!(actor.ask(Add(0)).await.unwrap(), 0);
    let stop = actor.stop().await;
    assert!(matches!(
        stop,
        Err(ActorError::Panic(message)) if message.contains("local stream drop failed")
    ));
    system.as_ref().clone().shutdown().await.unwrap();
}

struct Capture {
    drops: Arc<AtomicUsize>,
    panic_drop: bool,
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic_drop, "constructor capture drop");
    }
}
async fn count_reaches(value: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while value.load(Ordering::SeqCst) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[actix::test]
async fn unpolled_spawn_retires_captures_without_constructing() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let drops = Arc::new(AtomicUsize::new(0));
    let captured = Capture {
        drops: drops.clone(),
        panic_drop: true,
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_in = calls.clone();
    let pending = system.spawn_local_typed::<LocalCounter, Add, _>(
        move || {
            let _ = &captured;
            calls_in.fetch_add(1, Ordering::SeqCst);
            LocalCounter::new(Events::default())
        },
        "/local/unpolled",
    );
    drop(pending);
    count_reaches(&drops, 1).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(system.status().active_actors, 0);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn duplicate_and_invalid_paths_do_not_invoke_constructor() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let actor = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            || LocalCounter::new(Events::default()),
            "/local/duplicate",
        )
        .await
        .unwrap();
    for path in ["/local/duplicate", "", "bad\npath"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in = calls.clone();
        let drops = Arc::new(AtomicUsize::new(0));
        let captured = Capture {
            drops: drops.clone(),
            panic_drop: false,
        };
        let result = system
            .spawn_local_typed::<LocalCounter, Add, _>(
                move || {
                    let _ = &captured;
                    calls_in.fetch_add(1, Ordering::SeqCst);
                    LocalCounter::new(Events::default())
                },
                path,
            )
            .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    assert_eq!(actor.ask(Add(2)).await.unwrap(), 2);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn failed_constructor_terminates_registered_reference() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let actor = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            || panic!("owner construction failure"),
            "/local/fail",
        )
        .await
        .unwrap();
    assert!(actor.ask(Add(1)).await.is_err());
    let error = actor.stop().await.unwrap_err();
    assert!(
        matches!(error, ActorError::Panic(ref message) if message.contains("owner construction failure")),
        "{error:?}"
    );
    assert!(system.get_actor(&"/local/fail".into()).await.is_none());
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn capture_cleanup_failure_disposes_local_actor_before_init() {
    let caller = std::thread::current().id();
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let events = Events::default();
    let factory_events = events.clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let captured = Capture {
        drops: drops.clone(),
        panic_drop: true,
    };
    let actor = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            move || {
                let _ = &captured;
                LocalCounter::new(factory_events.clone())
            },
            "/local/capture",
        )
        .await
        .unwrap();
    assert!(actor.ask(Add(1)).await.is_err());
    assert!(actor.stop().await.is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_owner(&events, caller);
    assert_eq!(events.lock().unwrap().len(), 2);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn messages_wait_for_local_init_and_deadline_covers_full_mailbox() {
    let system = Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            mailbox_capacity: 1,
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let gate = Arc::new(tokio::sync::Notify::new());
    let factory_gate = gate.clone();
    let actor = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            move || {
                let mut actor = LocalCounter::new(Events::default());
                actor.init_gate = Some(factory_gate.clone());
                actor
            },
            "/local/full",
        )
        .await
        .unwrap();
    actor.tell(Add(7)).await.unwrap();
    assert!(matches!(
        actor
            .ask_with_timeout(Add(100), Duration::from_millis(10))
            .await,
        Err(ActorError::TimeoutDetail(_))
    ));
    gate.notify_one();
    assert_eq!(
        actor.ask(Add(0)).await.unwrap(),
        7,
        "timed-out admission was delivered"
    );
    actor.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}

#[derive(Default)]
struct Gate {
    opened: Mutex<bool>,
    changed: std::sync::Condvar,
    entered: tokio::sync::Notify,
}
impl Gate {
    fn block(&self) {
        self.entered.notify_one();
        let mut opened = self.opened.lock().unwrap();
        while !*opened {
            opened = self.changed.wait(opened).unwrap();
        }
    }
    fn release(&self) {
        *self.opened.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[actix::test]
async fn stop_during_blocked_construction_keeps_owner_until_actual_retirement() {
    let caller = std::thread::current().id();
    let system = Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: Duration::from_millis(30),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let events = Events::default();
    let factory_events = events.clone();
    let gate = Arc::new(Gate::default());
    let factory_gate = gate.clone();
    let _release = Release(gate.clone());
    let actor = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            move || {
                factory_gate.block();
                LocalCounter::new(factory_events.clone())
            },
            "/local/blocked",
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    let stopped = actor.stop().await;
    assert!(
        matches!(stopped, Err(ActorError::TimeoutDetail(_))),
        "{stopped:?}"
    );
    assert!(!actor.is_alive());
    assert_eq!(
        system.status().active_actors,
        1,
        "blocked owner retired too early"
    );
    assert!(events.lock().unwrap().is_empty());
    gate.release();
    tokio::time::timeout(Duration::from_secs(3), async {
        while system.status().active_actors != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_owner(&events, caller);
    assert_eq!(
        events.lock().unwrap().len(),
        2,
        "stopped construction initialized actor"
    );
    assert!(
        matches!(actor.stop().await, Err(ActorError::TimeoutDetail(_))),
        "stop receipt must remain stable"
    );
    system.as_ref().clone().shutdown().await.unwrap();
}

#[derive(Clone, Copy)]
enum Fault {
    Hold,
    PollAndDrop,
    ReadyAndDrop,
    InitAndDrop,
    CancelAndDrop,
}
struct RecursivePayload;
impl Drop for RecursivePayload {
    fn drop(&mut self) {
        panic!("recursive local panic payload");
    }
}
struct LocalFuture {
    _local: Rc<RefCell<u64>>,
    events: Events,
    entered: Arc<tokio::sync::Notify>,
    fault: Fault,
}
impl std::future::Future for LocalFuture {
    type Output = ActorResult<u64>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        record(&self.events, "fault_poll");
        self.entered.notify_one();
        match self.fault {
            Fault::Hold | Fault::CancelAndDrop => std::task::Poll::Pending,
            Fault::PollAndDrop | Fault::InitAndDrop => panic!("first local poll failure"),
            Fault::ReadyAndDrop => std::task::Poll::Ready(Ok(42)),
        }
    }
}
struct LocalInitFuture(LocalFuture);
impl std::future::Future for LocalInitFuture {
    type Output = ActorResult<()>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0)
            .poll(cx)
            .map(|result| result.map(|_| ()))
    }
}
impl Drop for LocalFuture {
    fn drop(&mut self) {
        record(&self.events, "fault_drop");
        if !matches!(self.fault, Fault::Hold) {
            std::panic::panic_any(RecursivePayload);
        }
    }
}
struct LocalFault {
    local: Rc<RefCell<u64>>,
    events: Events,
    entered: Arc<tokio::sync::Notify>,
    fault: Fault,
}
impl LocalFault {
    fn future(&self) -> LocalFuture {
        LocalFuture {
            _local: self.local.clone(),
            events: self.events.clone(),
            entered: self.entered.clone(),
            fault: self.fault,
        }
    }
}
impl Drop for LocalFault {
    fn drop(&mut self) {
        record(&self.events, "actor_drop");
    }
}
struct FaultRequest;
impl Message for FaultRequest {
    type Result = u64;
}
impl LocalTypedDispatch for LocalFault {
    type Msg = FaultRequest;
    type Reply = u64;
    fn init<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        if matches!(self.fault, Fault::InitAndDrop) {
            Box::pin(LocalInitFuture(self.future()))
        } else {
            Box::pin(async { Ok(()) })
        }
    }
    fn dispatch<'a>(
        &'a mut self,
        _: FaultRequest,
        _: &'a TypedContext,
    ) -> LocalBoxedFuture<'a, ActorResult<u64>> {
        Box::pin(self.future())
    }
    fn before_stop<'a>(&'a mut self, _: &'a TypedContext) -> LocalBoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            record(&self.events, "before_stop");
            Ok(())
        })
    }
}
impl ParrotMsgVariant<LocalProtocol<LocalFault>> for FaultRequest {
    fn inject(message: Self) -> Self {
        message
    }
    fn extract(reply: u64) -> ActorResult<u64> {
        Ok(reply)
    }
}

fn isolated(test: &str) -> bool {
    if std::env::var("PARROT_LOCAL_CONTRACT_CHILD").as_deref() == Ok(test) {
        return false;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("PARROT_LOCAL_CONTRACT_CHILD", test)
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "isolated contract {test}: {status}");
            return true;
        }
        if start.elapsed() > Duration::from_secs(15) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("isolated contract timed out: {test}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
async fn fault_contract(fault: Fault) {
    let caller = std::thread::current().id();
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let events = Events::default();
    let factory_events = events.clone();
    let entered = Arc::new(tokio::sync::Notify::new());
    let factory_entered = entered.clone();
    let actor = system
        .spawn_local_typed::<LocalFault, FaultRequest, _>(
            move || {
                record(&factory_events, "construct");
                LocalFault {
                    local: Rc::new(RefCell::new(0)),
                    events: factory_events.clone(),
                    entered: factory_entered.clone(),
                    fault,
                }
            },
            "/local/fault",
        )
        .await
        .unwrap();
    let reply_ref = actor.clone();
    let reply = tokio::spawn(async move { reply_ref.ask(FaultRequest).await });
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    if matches!(fault, Fault::Hold | Fault::CancelAndDrop) {
        let stopped = actor.stop().await;
        assert_eq!(stopped.is_ok(), matches!(fault, Fault::Hold));
    }
    let reply = tokio::time::timeout(Duration::from_secs(3), reply)
        .await
        .unwrap()
        .unwrap();
    assert!(
        reply.is_err(),
        "a failed/cancelled handler cannot publish success"
    );
    if matches!(fault, Fault::PollAndDrop) {
        assert!(
            matches!(reply, Err(ActorError::Panic(ref message)) if message.contains("first local poll failure")),
            "{reply:?}"
        );
    }
    let stopped = actor.stop().await;
    assert_eq!(stopped.is_ok(), matches!(fault, Fault::Hold));
    system.as_ref().clone().shutdown().await.unwrap();
    assert_owner(&events, caller);
    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|(name, _)| *name == "fault_drop")
            .count(),
        1
    );
    if matches!(fault, Fault::CancelAndDrop) {
        assert!(
            !events.iter().any(|(name, _)| *name == "before_stop"),
            "failed cancellation entered normal stop hook"
        );
    }
}
macro_rules! fault_test {
    ($name:ident, $fault:expr) => {
        #[actix::test]
        async fn $name() {
            if isolated(stringify!($name)) {
                return;
            }
            fault_contract($fault).await;
        }
    };
}
fault_test!(pending_local_future_retires_before_normal_stop, Fault::Hold);
fault_test!(
    local_poll_and_drop_panics_keep_first_error_without_abort,
    Fault::PollAndDrop
);
fault_test!(
    successful_local_reply_is_suppressed_if_future_drop_fails,
    Fault::ReadyAndDrop
);
fault_test!(
    local_init_poll_and_drop_panics_do_not_abort,
    Fault::InitAndDrop
);
fault_test!(
    cancelled_local_future_cleanup_failure_skips_normal_stop,
    Fault::CancelAndDrop
);

#[actix::test]
async fn cancelled_stop_waiter_does_not_abandon_local_destructor() {
    let caller = std::thread::current().id();
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let events = Events::default();
    let factory_events = events.clone();
    let gate = Arc::new(Gate::default());
    let factory_gate = gate.clone();
    let _release = Release(gate.clone());
    let actor = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            move || {
                let mut actor = LocalCounter::new(factory_events.clone());
                actor.drop_gate = Some(factory_gate.clone());
                actor
            },
            "/local/drop",
        )
        .await
        .unwrap();
    assert_eq!(actor.ask(Add(1)).await.unwrap(), 1);
    let mut stopping = Box::pin(actor.stop());
    assert!(futures::poll!(&mut stopping).is_pending());
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    drop(stopping);
    assert_eq!(system.status().active_actors, 1);
    let mut other_waiter = Box::pin(actor.stop());
    assert!(futures::poll!(&mut other_waiter).is_pending());
    gate.release();
    other_waiter.await.unwrap();
    assert_owner(&events, caller);
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn cancelled_shutdown_still_joins_local_actor_and_worker() {
    let caller = std::thread::current().id();
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let events = Events::default();
    let factory_events = events.clone();
    let gate = Arc::new(Gate::default());
    let factory_gate = gate.clone();
    let _release = Release(gate.clone());
    let actor = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            move || {
                let mut actor = LocalCounter::new(factory_events.clone());
                actor.drop_gate = Some(factory_gate.clone());
                actor
            },
            "/local/shutdown",
        )
        .await
        .unwrap();
    assert_eq!(actor.ask(Add(2)).await.unwrap(), 2);
    let shutdown_system = system.as_ref().clone();
    let waiter = tokio::spawn(async move { shutdown_system.shutdown().await });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_eq!(
        system.status().state,
        parrot_api::system::SystemState::ShuttingDown
    );
    assert_eq!(system.status().active_actors, 1);
    gate.release();
    system.as_ref().clone().shutdown().await.unwrap();
    assert_eq!(
        system.status().state,
        parrot_api::system::SystemState::Stopped
    );
    assert_eq!(system.status().resources.thread_count, 0);
    assert!(!actor.is_alive());
    assert_owner(&events, caller);
}

struct WatchSink(tokio::sync::mpsc::UnboundedSender<parrot_api::context::ActorTerminated>);
impl parrot_api::death_watch::TerminationRecipient for WatchSink {
    fn deliver(
        &self,
        event: parrot_api::context::ActorTerminated,
    ) -> parrot_api::types::BoxedFuture<'_, ActorResult<()>> {
        Box::pin(async move { self.0.send(event).map_err(|_| ActorError::Stopped) })
    }
}
#[actix::test]
async fn death_notification_identifies_local_generation_after_retirement() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let events = Events::default();
    let factory_events = events.clone();
    let actor = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            move || LocalCounter::new(factory_events.clone()),
            "/local/watch",
        )
        .await
        .unwrap();
    assert_eq!(actor.ask(Add(0)).await.unwrap(), 0);
    let target = actor.clone().into_dyn().death_watch().unwrap();
    let watcher = parrot_api::death_watch::DeathWatch::new(1);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    watcher.set_recipient(Arc::new(WatchSink(sender))).unwrap();
    target.watch(&watcher).unwrap();
    actor.stop().await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.path, "/local/watch");
    assert_eq!(event.generation, target.id());
    assert!(matches!(
        event.reason,
        parrot_api::supervisor::DeathReason::Normal
    ));
    assert_eq!(events.lock().unwrap().last().unwrap().0, "actor_drop");
    assert!(receiver.try_recv().is_err());
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn stopped_queued_owner_does_not_run_constructor() {
    let system = Arc::new(
        ActixActorSystem::with_settings(ActixSystemSettings {
            arbiter_count: 1,
            shutdown_timeout: Duration::from_millis(30),
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    let gate = Arc::new(Gate::default());
    let factory_gate = gate.clone();
    let _release = Release(gate.clone());
    let blocker = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            move || {
                factory_gate.block();
                LocalCounter::new(Events::default())
            },
            "/local/blocker",
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let factory_calls = calls.clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let captured = Capture {
        drops: drops.clone(),
        panic_drop: false,
    };
    let queued = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            move || {
                let _ = &captured;
                factory_calls.fetch_add(1, Ordering::SeqCst);
                LocalCounter::new(Events::default())
            },
            "/local/queued",
        )
        .await
        .unwrap();
    assert!(matches!(
        queued.stop().await,
        Err(ActorError::TimeoutDetail(_))
    ));
    gate.release();
    assert_eq!(blocker.ask(Add(0)).await.unwrap(), 0);
    count_reaches(&drops, 1).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    tokio::time::timeout(Duration::from_secs(3), async {
        while system.get_actor(&"/local/queued".into()).await.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!queued.is_alive());
    system.as_ref().clone().shutdown().await.unwrap();
}

#[actix::test]
async fn shutdown_rejection_retires_factory_without_constructing_actor() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    system.as_ref().clone().shutdown().await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let factory_calls = calls.clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let captured = Capture {
        drops: drops.clone(),
        panic_drop: false,
    };
    let result = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            move || {
                let _ = &captured;
                factory_calls.fetch_add(1, Ordering::SeqCst);
                LocalCounter::new(Events::default())
            },
            "/local/closed",
        )
        .await;
    assert!(matches!(result, Err(ActorError::Stopped)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[actix::test]
async fn owner_constructor_can_capture_send_but_non_sync_state() {
    let system = Arc::new(ActixActorSystem::with_arbiter_count(1).await.unwrap());
    let called = std::cell::Cell::new(false);
    let actor = system
        .spawn_local_typed::<LocalCounter, Add, _>(
            move || {
                assert!(!called.replace(true), "one-shot constructor ran twice");
                LocalCounter::new(Events::default())
            },
            "/local/non-sync-constructor",
        )
        .await
        .unwrap();
    assert_eq!(actor.ask(Add(9)).await.unwrap(), 9);
    actor.stop().await.unwrap();
    system.as_ref().clone().shutdown().await.unwrap();
}
