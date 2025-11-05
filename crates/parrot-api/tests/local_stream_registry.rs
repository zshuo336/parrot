use futures::Stream;
use parrot_api::errors::ActorError;
use parrot_api::local_typed::{
    LocalStreamEventKind, LocalStreamRegistry, DEFAULT_MAX_LOCAL_STREAMS,
};
use std::{
    cell::Cell,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

struct CountingStream {
    polls: Rc<Cell<usize>>,
    value: Option<Rc<u32>>,
}

impl Stream for CountingStream {
    type Item = Rc<u32>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.polls.set(self.polls.get() + 1);
        Poll::Ready(self.value.take())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn registration_is_lazy_and_item_is_polled_on_owner() {
    let polls = Rc::new(Cell::new(0));
    let mut registry = LocalStreamRegistry::new();
    registry
        .add_stream(CountingStream {
            polls: polls.clone(),
            value: Some(Rc::new(7)),
        })
        .unwrap();
    assert_eq!(polls.get(), 0, "registration must not poll the source");

    let event = registry.next_event().await.unwrap();
    assert_eq!(event.kind(), LocalStreamEventKind::Started);
    assert_eq!(polls.get(), 0);
    event.complete(&mut registry, true);

    let mut event = registry.next_event().await.unwrap();
    assert_eq!(event.kind(), LocalStreamEventKind::Item);
    assert_eq!(polls.get(), 1);
    let item = event.take_item().unwrap().downcast::<Rc<u32>>().unwrap();
    assert_eq!(*item, Rc::new(7));
    event.complete(&mut registry, true);

    let event = registry.next_event().await.unwrap();
    assert_eq!(event.kind(), LocalStreamEventKind::Finished);
    event.complete(&mut registry, false);
    assert!(registry.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn ready_streams_rotate_fairly_between_entries() {
    let mut registry = LocalStreamRegistry::new();
    registry.add_stream(futures::stream::iter([1_u32])).unwrap();
    registry.add_stream(futures::stream::iter([2_u32])).unwrap();

    let first = registry.next_event().await.unwrap();
    assert_eq!(first.kind(), LocalStreamEventKind::Started);
    let first_id = first.id();
    first.complete(&mut registry, true);

    let second = registry.next_event().await.unwrap();
    assert_eq!(second.kind(), LocalStreamEventKind::Started);
    let second_id = second.id();
    assert_ne!(first_id, second_id);
    second.complete(&mut registry, true);

    let first_item = registry.next_event().await.unwrap();
    assert_eq!(first_item.id(), first_id);
    assert_eq!(first_item.kind(), LocalStreamEventKind::Item);
    first_item.complete(&mut registry, true);
    let second_item = registry.next_event().await.unwrap();
    assert_eq!(second_item.id(), second_id);
    assert_eq!(second_item.kind(), LocalStreamEventKind::Item);
}

#[test]
fn registration_has_a_finite_generation_bound() {
    let mut registry = LocalStreamRegistry::new();
    for _ in 0..DEFAULT_MAX_LOCAL_STREAMS {
        registry
            .add_stream(futures::stream::pending::<u32>())
            .unwrap();
    }
    assert_eq!(
        registry.add_stream(futures::stream::pending::<u32>()),
        Err("local stream capacity exhausted")
    );
}

struct PanickingPoll;

impl Stream for PanickingPoll {
    type Item = u32;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        panic!("source poll failed")
    }
}

#[tokio::test(flavor = "current_thread")]
async fn source_poll_panic_becomes_a_retirable_error_event() {
    let mut registry = LocalStreamRegistry::new();
    registry.add_stream(PanickingPoll).unwrap();
    let started = registry.next_event().await.unwrap();
    assert_eq!(started.kind(), LocalStreamEventKind::Started);
    started.complete(&mut registry, true);

    let mut error = registry.next_event().await.unwrap();
    assert_eq!(error.kind(), LocalStreamEventKind::Error);
    assert!(matches!(
        error.take_error(),
        Some(ActorError::Panic(message)) if message.contains("source poll failed")
    ));
    error.complete(&mut registry, false);
    assert!(registry.is_empty());
}
