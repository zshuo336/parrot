use parrot_api::{
    address::{ActorRef, ActorRefExt},
    errors::ActorError,
    message::Message,
    types::{ActorResult, BoxedActorRef, BoxedFuture, BoxedMessage},
};
use std::{any::Any, time::Duration};

#[derive(Debug)]
struct Item(u64);
impl Message for Item {
    type Result = ();
}
#[derive(Clone, Debug)]
struct QueueRef(flume::Sender<BoxedMessage>);
impl ActorRef for QueueRef {
    fn send<'a>(&'a self, _: BoxedMessage) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("tell must not request a business reply")
    }
    fn send_with_timeout<'a>(
        &'a self,
        _: BoxedMessage,
        _: Option<Duration>,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        panic!("tell must not ask")
    }
    fn deliver<'a>(&'a self, msg: BoxedMessage) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async move {
            self.0
                .send_async(msg)
                .await
                .map_err(|_| ActorError::Stopped)
        })
    }
    fn stop<'a>(&'a self) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn path(&self) -> String {
        "/tell-contract".into()
    }
    fn is_alive<'a>(&'a self) -> BoxedFuture<'a, bool> {
        Box::pin(async move { !self.0.is_disconnected() })
    }
    fn clone_boxed(&self) -> BoxedActorRef {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[test]
fn tell_has_no_implicit_tokio_runtime_requirement() {
    let (tx, rx) = flume::bounded(1);
    let actor = QueueRef(tx);
    futures::executor::block_on(actor.tell(Item(7))).unwrap();
    assert_eq!(rx.try_recv().unwrap().downcast_ref::<Item>().unwrap().0, 7);
}
#[tokio::test]
async fn tell_completion_means_admission_not_detached_work() {
    let (tx, rx) = flume::bounded(1);
    let actor = QueueRef(tx);
    actor.tell(Item(11)).await.unwrap();
    assert_eq!(
        rx.try_recv()
            .expect("receipt before admission")
            .downcast_ref::<Item>()
            .unwrap()
            .0,
        11
    );
}
#[tokio::test]
async fn tell_propagates_closed_mailbox_error() {
    let (tx, rx) = flume::bounded(1);
    drop(rx);
    let actor = QueueRef(tx);
    assert!(matches!(
        actor.tell(Item(1)).await,
        Err(ActorError::Stopped)
    ));
}
#[tokio::test]
async fn tell_backpressure_is_owned_by_the_caller_and_cancellable() {
    let (tx, rx) = flume::bounded(1);
    tx.send(Box::new(Item(1)) as BoxedMessage).unwrap();
    let actor = QueueRef(tx);
    let mut pending = actor.tell(Item(2));
    assert!(
        futures::poll!(&mut pending).is_pending(),
        "full mailbox must apply caller backpressure"
    );
    drop(pending);
    assert_eq!(rx.recv().unwrap().downcast_ref::<Item>().unwrap().0, 1);
    tokio::task::yield_now().await;
    assert!(rx.is_empty(), "cancelled admission left detached work");
}

#[test]
fn dropping_an_unpolled_tell_has_no_delivery_side_effect() {
    let (tx, rx) = flume::bounded(1);
    let actor = QueueRef(tx);
    drop(actor.tell(Item(2)));
    assert!(rx.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_tell_preserves_twenty_thousand_admissions_across_clones() {
    let (tx, rx) = flume::bounded::<BoxedMessage>(2);
    let actor = QueueRef(tx);
    let clone = actor.clone_boxed();
    let receive = async move {
        let mut received = Vec::new();
        for _ in 0..20_000 {
            received.push(
                rx.recv_async()
                    .await
                    .unwrap()
                    .downcast_ref::<Item>()
                    .unwrap()
                    .0,
            );
        }
        received
    };
    let send = async {
        for value in 0..20_000 {
            if value % 2 == 0 {
                actor.tell(Item(value)).await.unwrap();
            } else {
                clone.tell(Item(value)).await.unwrap();
            }
        }
    };
    let ((), received) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(send, receive)
    })
    .await
    .unwrap();
    assert_eq!(received, (0..20_000).collect::<Vec<_>>());
}
