use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Barrier,
};
use std::task::{Context, Poll, Wake, Waker};
use tokio::sync::mpsc::{
    self,
    error::{TryRecvError, TrySendError},
};

#[derive(Default)]
struct Counter(AtomicUsize);

impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn park<T>(receiver: &mut mpsc::Receiver<T>, counter: &Arc<Counter>) {
    let waker = Waker::from(counter.clone());
    assert!(receiver
        .poll_recv(&mut Context::from_waker(&waker))
        .is_pending());
}

#[test]
fn queue_insertion_and_fifo_finish_before_receiver_notification() {
    let (sender, mut receiver) = mpsc::channel(2);
    let counter = Arc::new(Counter::default());
    park(&mut receiver, &counter);
    let (result, notification) = sender.try_send_deferred(10);
    result.unwrap();
    sender.try_send(20).unwrap();
    assert_eq!(sender.capacity(), 0);
    assert_eq!(
        counter.0.load(Ordering::SeqCst),
        0,
        "deferred send invoked receiver before guard release"
    );
    assert_eq!(receiver.try_recv().unwrap(), 10);
    assert_eq!(receiver.try_recv().unwrap(), 20);
    assert_eq!(sender.capacity(), 2);
    drop(notification);
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn full_and_closed_errors_preserve_the_original_value_without_notification() {
    let (sender, mut receiver) = mpsc::channel(1);
    let (result, notification) = sender.try_send_deferred(String::from("first"));
    result.unwrap();
    drop(notification);
    let (result, notification) = sender.try_send_deferred(String::from("full"));
    assert_eq!(result, Err(TrySendError::Full(String::from("full"))));
    drop(notification);
    assert_eq!(receiver.try_recv().unwrap(), "first");
    let counter = Arc::new(Counter::default());
    park(&mut receiver, &counter);
    receiver.close();
    let before = counter.0.load(Ordering::SeqCst);
    let (result, notification) = sender.try_send_deferred(String::from("closed"));
    assert_eq!(result, Err(TrySendError::Closed(String::from("closed"))));
    drop(notification);
    assert_eq!(counter.0.load(Ordering::SeqCst), before);
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn reserved_notification_survives_the_last_sender() {
    let (sender, mut receiver) = mpsc::channel(1);
    let counter = Arc::new(Counter::default());
    park(&mut receiver, &counter);
    let (result, notification) = sender.try_send_deferred(42);
    result.unwrap();
    drop(sender);
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    drop(notification);
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert_eq!(receiver.try_recv().unwrap(), 42);
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn delayed_old_notification_does_not_steal_the_new_receiver_wake() {
    let (sender, mut receiver) = mpsc::channel(2);
    let first = Arc::new(Counter::default());
    let second = Arc::new(Counter::default());
    park(&mut receiver, &first);
    let (result, notification) = sender.try_send_deferred(1);
    result.unwrap();
    assert_eq!(receiver.try_recv().unwrap(), 1);
    park(&mut receiver, &second);
    sender.try_send(2).unwrap();
    assert_eq!(first.0.load(Ordering::SeqCst), 0);
    assert_eq!(second.0.load(Ordering::SeqCst), 1);
    drop(notification);
    assert_eq!(first.0.load(Ordering::SeqCst), 1);
    assert_eq!(second.0.load(Ordering::SeqCst), 1);
    assert_eq!(receiver.try_recv().unwrap(), 2);
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn send_racing_receiver_registration_neither_loses_a_value_nor_a_wake() {
    for value in 0..256 {
        let (sender, mut receiver) = mpsc::channel(1);
        let keep_alive = sender.clone();
        let counter = Arc::new(Counter::default());
        let barrier = Arc::new(Barrier::new(2));
        let other = barrier.clone();
        let producer = std::thread::spawn(move || {
            other.wait();
            let (result, notification) = sender.try_send_deferred(value);
            result.unwrap();
            drop(notification);
        });
        let waker = Waker::from(counter.clone());
        barrier.wait();
        let result = receiver.poll_recv(&mut Context::from_waker(&waker));
        producer.join().unwrap();
        match result {
            Poll::Ready(Some(actual)) => assert_eq!(actual, value),
            Poll::Pending => {
                assert!(
                    counter.0.load(Ordering::SeqCst) > 0,
                    "pending receiver lost its wake"
                );
                assert_eq!(receiver.try_recv().unwrap(), value);
            }
            other => panic!("sender committed one value: {other:?}"),
        }
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        drop(keep_alive);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Disconnected));
    }
}

struct Panicking;

impl Wake for Panicking {
    fn wake(self: Arc<Self>) {
        panic!("controlled deferred receiver wake");
    }
}

#[test]
fn wake_panic_preserves_committed_data_and_later_channel_progress() {
    let (sender, mut receiver) = mpsc::channel(1);
    let waker = Waker::from(Arc::new(Panicking));
    assert!(receiver
        .poll_recv(&mut Context::from_waker(&waker))
        .is_pending());
    let (result, notification) = sender.try_send_deferred(1);
    result.unwrap();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(notification))).is_err());
    assert_eq!(receiver.try_recv().unwrap(), 1);
    let counter = Arc::new(Counter::default());
    park(&mut receiver, &counter);
    let (result, notification) = sender.try_send_deferred(2);
    result.unwrap();
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    drop(notification);
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert_eq!(receiver.try_recv().unwrap(), 2);
}
