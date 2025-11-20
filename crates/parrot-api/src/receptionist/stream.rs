use super::ReceptionistEvent;
use std::{
    panic::{RefUnwindSafe, UnwindSafe},
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc::{error::TryRecvError, Receiver};

trait LifetimeGuard: Send + Sync + UnwindSafe + RefUnwindSafe {}
impl<T: Send + Sync + UnwindSafe + RefUnwindSafe> LifetimeGuard for T {}

/// An owned service-event receiver and its resource lifetime guards.
///
/// Closing or draining the stream does not release guards. They are released
/// only after the receiver is dropped, including its remaining queued events.
/// Custom gateways can convert an ordinary mpsc receiver with `.into()`.
/// The stream implements `futures::Stream` directly; no ReceiverStream adapter
/// is required. There is deliberately no way to extract an unguarded receiver.
pub struct ReceptionistStream {
    receiver: Receiver<ReceptionistEvent>,
    // Field order ensures channel retirement precedes permit release.
    guards: Vec<Box<dyn LifetimeGuard>>,
}

impl From<Receiver<ReceptionistEvent>> for ReceptionistStream {
    fn from(receiver: Receiver<ReceptionistEvent>) -> Self {
        Self {
            receiver,
            guards: Vec::new(),
        }
    }
}

impl std::fmt::Debug for ReceptionistStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceptionistStream")
            .field("receiver", &self.receiver)
            .finish_non_exhaustive()
    }
}

impl ReceptionistStream {
    /// Keep an additional owned resource until this receiver has retired.
    /// Adding another guard never releases previously retained guards.
    pub fn with_lifetime_guard<T>(mut self, guard: T) -> Self
    where
        T: Send + Sync + UnwindSafe + RefUnwindSafe + 'static,
    {
        self.guards.push(Box::new(guard));
        self
    }

    pub async fn recv(&mut self) -> Option<ReceptionistEvent> {
        self.receiver.recv().await
    }
    pub async fn recv_many(&mut self, buffer: &mut Vec<ReceptionistEvent>, limit: usize) -> usize {
        self.receiver.recv_many(buffer, limit).await
    }
    pub fn try_recv(&mut self) -> Result<ReceptionistEvent, TryRecvError> {
        self.receiver.try_recv()
    }
    pub fn blocking_recv(&mut self) -> Option<ReceptionistEvent> {
        self.receiver.blocking_recv()
    }
    pub fn blocking_recv_many(
        &mut self,
        buffer: &mut Vec<ReceptionistEvent>,
        limit: usize,
    ) -> usize {
        self.receiver.blocking_recv_many(buffer, limit)
    }
    pub fn close(&mut self) {
        self.receiver.close();
    }
    pub fn is_closed(&self) -> bool {
        self.receiver.is_closed()
    }
    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
    pub fn len(&self) -> usize {
        self.receiver.len()
    }
    pub fn capacity(&self) -> usize {
        self.receiver.capacity()
    }
    pub fn max_capacity(&self) -> usize {
        self.receiver.max_capacity()
    }
    pub fn sender_strong_count(&self) -> usize {
        self.receiver.sender_strong_count()
    }
    pub fn sender_weak_count(&self) -> usize {
        self.receiver.sender_weak_count()
    }
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<ReceptionistEvent>> {
        self.receiver.poll_recv(cx)
    }
    pub fn poll_recv_many(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut Vec<ReceptionistEvent>,
        limit: usize,
    ) -> Poll<usize> {
        self.receiver.poll_recv_many(cx, buffer, limit)
    }
}

impl futures::Stream for ReceptionistStream {
    type Item = ReceptionistEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct Guard(Arc<AtomicUsize>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn event(n: usize) -> ReceptionistEvent {
        ReceptionistEvent::Registered {
            key: super::super::ReceptionistKey::new("edge/stream").unwrap(),
            remote_path: format!("/actor/{n}"),
        }
    }

    #[tokio::test]
    async fn close_drain_and_eof_retain_all_guards_until_drop() {
        let count = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.send(event(1)).await.unwrap();
        let mut stream = ReceptionistStream::from(rx)
            .with_lifetime_guard(Guard(count.clone()))
            .with_lifetime_guard(Guard(count.clone()));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        stream.close();
        assert!(stream.is_closed());
        assert_eq!(stream.recv().await, Some(event(1)));
        assert_eq!(stream.next().await, None);
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "EOF must retain receiver guards"
        );
        drop(stream);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn receiver_retires_before_guard_callback_even_with_queued_events() {
        struct Observe(
            tokio::sync::mpsc::Sender<ReceptionistEvent>,
            Arc<AtomicUsize>,
        );
        impl Drop for Observe {
            fn drop(&mut self) {
                assert!(
                    self.0.is_closed(),
                    "receiver must retire before permit release"
                );
                self.1.fetch_add(1, Ordering::SeqCst);
            }
        }
        let count = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        tx.try_send(event(1)).unwrap();
        let stream = ReceptionistStream::from(rx).with_lifetime_guard(Observe(tx, count.clone()));
        drop(stream);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fifo_batch_and_cancelled_receive_preserve_channel_contract() {
        let (tx, rx) = tokio::sync::mpsc::channel(3);
        let mut stream: ReceptionistStream = rx.into();
        assert_eq!(stream.max_capacity(), 3);
        assert_eq!(stream.sender_strong_count(), 1);
        let weak = tx.downgrade();
        assert_eq!(stream.sender_weak_count(), 1);
        {
            let pending = stream.recv();
            tokio::pin!(pending);
            assert_eq!(futures::poll!(pending.as_mut()), Poll::Pending);
        }
        tx.send(event(1)).await.unwrap();
        tx.send(event(2)).await.unwrap();
        tx.send(event(3)).await.unwrap();
        assert_eq!(stream.capacity(), 0);
        assert_eq!(stream.len(), 3);
        assert_eq!(stream.next().await, Some(event(1)));
        let mut batch = Vec::new();
        assert_eq!(stream.recv_many(&mut batch, 2).await, 2);
        assert_eq!(batch, [event(2), event(3)]);
        assert!(stream.is_empty());
        tx.send(event(4)).await.unwrap();
        let waker = futures::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert_eq!(
            stream.poll_recv_many(&mut context, &mut batch, 1),
            Poll::Ready(1)
        );
        assert_eq!(batch.last(), Some(&event(4)));
        drop(tx);
        drop(weak);
        assert_eq!(stream.recv().await, None);
    }

    #[test]
    fn blocking_interfaces_and_auto_traits_are_preserved() {
        fn traits<T: Send + Sync + Unpin + UnwindSafe + RefUnwindSafe>() {}
        traits::<ReceptionistStream>();
        let (tx, rx) = tokio::sync::mpsc::channel(3);
        for n in 0..3 {
            tx.try_send(event(n)).unwrap();
        }
        drop(tx);
        let mut stream: ReceptionistStream = rx.into();
        assert_eq!(stream.blocking_recv(), Some(event(0)));
        let mut batch = Vec::new();
        assert_eq!(stream.blocking_recv_many(&mut batch, 3), 2);
        assert_eq!(batch, [event(1), event(2)]);
        assert_eq!(stream.blocking_recv(), None);
    }
}
