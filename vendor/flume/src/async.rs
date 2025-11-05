//! Futures and other types that allow asynchronous interaction with channels.

#[cfg(test)]
#[path = "clone_contracts.rs"]
mod clone_contracts;

use crate::*;
use futures_core::{
    future::FusedFuture,
    stream::{FusedStream, Stream},
};
use futures_sink::Sink;
use spin1::Mutex as Spinlock;
use std::fmt::{Debug, Formatter};
use std::{
    any::Any,
    future::Future,
    ops::Deref,
    pin::Pin,
    task::{Context, Poll, Waker},
};

struct AsyncSignal {
    // A notification takes ownership instead of cloning a user RawWaker while
    // holding this lock. A later poll installs its current waker again.
    waker: Spinlock<Option<Waker>>,
    woken: AtomicBool,
    stream: bool,
}

impl AsyncSignal {
    fn new(cx: &Context, stream: bool) -> Self {
        AsyncSignal {
            waker: Spinlock::new(Some(cx.waker().clone())),
            woken: AtomicBool::new(false),
            stream,
        }
    }
}

impl Signal for AsyncSignal {
    fn prepare(&self) {
        self.woken.store(true, Ordering::SeqCst);
    }
    fn is_stream(&self) -> bool {
        self.stream
    }
    fn fire(&self) -> bool {
        // A custom waker can synchronously poll or dispose its receive future.
        // Never invoke it while holding the signal's registration lock.
        let waker = self.waker.lock().take();
        if let Some(waker) = waker {
            waker.wake();
        }
        self.stream
    }

    fn as_any(&self) -> &(dyn Any + 'static) {
        self
    }
    fn as_ptr(&self) -> *const () {
        self as *const _ as *const ()
    }
}

impl<T> Hook<T, AsyncSignal> {
    // Update the hook to point to the given Waker.
    // Returns whether the hook has been previously awakened
    fn update_waker(&self, cx_waker: &Waker) -> bool {
        {
            let waker = self.1.waker.lock();
            if waker.as_ref().is_some_and(|old| old.will_wake(cx_waker)) {
                return self.1.woken.load(Ordering::SeqCst);
            }
        }
        // clone() invokes a user RawWaker vtable. It may commit a send and
        // consume the previous registration, so inspect woken again afterwards.
        let replacement = cx_waker.clone();
        let (previous, woken) = {
            let mut waker = self.1.waker.lock();
            let previous = waker.replace(replacement);
            (previous, self.1.woken.load(Ordering::SeqCst))
        };
        if woken {
            cx_waker.wake_by_ref();
        }
        drop(previous);
        woken
    }
}

#[derive(Clone)]
enum OwnedOrRef<'a, T> {
    Owned(T),
    Ref(&'a T),
}

impl<'a, T> Deref for OwnedOrRef<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        match self {
            OwnedOrRef::Owned(arc) => arc,
            OwnedOrRef::Ref(r) => r,
        }
    }
}

impl<T> Sender<T> {
    /// Asynchronously send a value into the channel, returning an error if all receivers have been
    /// dropped. If the channel is bounded and is full, the returned future will yield to the async
    /// runtime.
    ///
    /// In the current implementation, the returned future will not yield to the async runtime if the
    /// channel is unbounded. This may change in later versions.
    pub fn send_async(&self, item: T) -> SendFut<'_, T> {
        SendFut {
            sender: OwnedOrRef::Ref(self),
            hook: Some(SendState::NotYetSent(item)),
        }
    }

    /// Convert this sender into a future that asynchronously sends a single message into the channel,
    /// returning an error if all receivers have been dropped. If the channel is bounded and is full,
    /// this future will yield to the async runtime.
    ///
    /// In the current implementation, the returned future will not yield to the async runtime if the
    /// channel is unbounded. This may change in later versions.
    pub fn into_send_async<'a>(self, item: T) -> SendFut<'a, T> {
        SendFut {
            sender: OwnedOrRef::Owned(self),
            hook: Some(SendState::NotYetSent(item)),
        }
    }

    /// Create an asynchronous sink that uses this sender to asynchronously send messages into the
    /// channel. The sender will continue to be usable after the sink has been dropped.
    ///
    /// In the current implementation, the returned sink will not yield to the async runtime if the
    /// channel is unbounded. This may change in later versions.
    pub fn sink(&self) -> SendSink<'_, T> {
        SendSink(SendFut {
            sender: OwnedOrRef::Ref(self),
            hook: None,
        })
    }

    /// Convert this sender into a sink that allows asynchronously sending messages into the channel.
    ///
    /// In the current implementation, the returned sink will not yield to the async runtime if the
    /// channel is unbounded. This may change in later versions.
    pub fn into_sink<'a>(self) -> SendSink<'a, T> {
        SendSink(SendFut {
            sender: OwnedOrRef::Owned(self),
            hook: None,
        })
    }
}

enum SendState<T> {
    NotYetSent(T),
    QueuedItem(Arc<Hook<T, AsyncSignal>>),
}

/// A future that sends a value into a channel.
///
/// Can be created via [`Sender::send_async`] or [`Sender::into_send_async`].
#[must_use = "futures/streams/sinks do nothing unless you `.await` or poll them"]
pub struct SendFut<'a, T> {
    sender: OwnedOrRef<'a, Sender<T>>,
    // Only none after dropping
    hook: Option<SendState<T>>,
}

impl<'a, T> Debug for SendFut<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendFut").finish()
    }
}

impl<T> std::marker::Unpin for SendFut<'_, T> {}

impl<'a, T> SendFut<'a, T> {
    /// Reset the hook, clearing it and removing it from the waiting sender's queue. This is called
    /// on drop and just before `start_send` in the `Sink` implementation.
    fn reset_hook(&mut self) {
        if let Some(SendState::QueuedItem(hook)) = self.hook.take() {
            let hook: Arc<Hook<T, dyn Signal>> = hook;
            wait_lock(&self.sender.shared.chan)
                .sending
                .as_mut()
                .unwrap()
                .1
                .retain(|s| s.signal().as_ptr() != hook.signal().as_ptr());
        }
    }

    /// See [`Sender::is_disconnected`].
    pub fn is_disconnected(&self) -> bool {
        self.sender.is_disconnected()
    }

    /// See [`Sender::is_empty`].
    pub fn is_empty(&self) -> bool {
        self.sender.is_empty()
    }

    /// See [`Sender::is_full`].
    pub fn is_full(&self) -> bool {
        self.sender.is_full()
    }

    /// See [`Sender::len`].
    pub fn len(&self) -> usize {
        self.sender.len()
    }

    /// See [`Sender::capacity`].
    pub fn capacity(&self) -> Option<usize> {
        self.sender.capacity()
    }
}

impl<'a, T> Drop for SendFut<'a, T> {
    fn drop(&mut self) {
        self.reset_hook()
    }
}

impl<'a, T> Future for SendFut<'a, T> {
    type Output = Result<(), SendError<T>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(SendState::QueuedItem(hook)) = self.hook.as_ref() {
            if hook.is_empty() {
                Poll::Ready(Ok(()))
            } else if self.sender.shared.is_disconnected() {
                let item = hook.try_take();
                self.hook = None;
                match item {
                    Some(item) => Poll::Ready(Err(SendError(item))),
                    None => Poll::Ready(Ok(())),
                }
            } else {
                hook.update_waker(cx.waker());
                Poll::Pending
            }
        } else if let Some(SendState::NotYetSent(item)) = self.hook.take() {
            let this = self.get_mut();
            let (shared, this_hook) = (&this.sender.shared, &mut this.hook);
            let signal = AsyncSignal::new(cx, false);

            shared
                .send(
                    // item
                    item,
                    // should_block
                    true,
                    // make_signal
                    |msg| Hook::slot(Some(msg), signal),
                    // do_block
                    |hook| {
                        *this_hook = Some(SendState::QueuedItem(hook));
                        Poll::Pending
                    },
                )
                .map(|r| {
                    r.map_err(|err| match err {
                        TrySendTimeoutError::Disconnected(msg) => SendError(msg),
                        _ => unreachable!(),
                    })
                })
        } else {
            // Nothing to do
            Poll::Ready(Ok(()))
        }
    }
}

impl<'a, T> FusedFuture for SendFut<'a, T> {
    fn is_terminated(&self) -> bool {
        self.sender.shared.is_disconnected()
    }
}

/// A sink that allows sending values into a channel.
///
/// Can be created via [`Sender::sink`] or [`Sender::into_sink`].
pub struct SendSink<'a, T>(SendFut<'a, T>);

impl<'a, T> SendSink<'a, T> {
    /// Returns a clone of a sending half of the channel of this sink.
    pub fn sender(&self) -> &Sender<T> {
        &self.0.sender
    }

    /// See [`Sender::is_disconnected`].
    pub fn is_disconnected(&self) -> bool {
        self.0.is_disconnected()
    }

    /// See [`Sender::is_empty`].
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// See [`Sender::is_full`].
    pub fn is_full(&self) -> bool {
        self.0.is_full()
    }

    /// See [`Sender::len`].
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// See [`Sender::capacity`].
    pub fn capacity(&self) -> Option<usize> {
        self.0.capacity()
    }

    /// Returns whether the SendSinks are belong to the same channel.
    pub fn same_channel(&self, other: &Self) -> bool {
        self.sender().same_channel(other.sender())
    }
}

impl<'a, T> Debug for SendSink<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendSink").finish()
    }
}

impl<'a, T> Sink<T> for SendSink<'a, T> {
    type Error = SendError<T>;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0).poll(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        self.0.reset_hook();
        self.0.hook = Some(SendState::NotYetSent(item));

        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0).poll(cx) // TODO: A different strategy here?
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0).poll(cx) // TODO: A different strategy here?
    }
}

impl<'a, T> Clone for SendSink<'a, T> {
    fn clone(&self) -> SendSink<'a, T> {
        SendSink(SendFut {
            sender: self.0.sender.clone(),
            hook: None,
        })
    }
}

impl<T> Receiver<T> {
    /// Asynchronously receive a value from the channel, returning an error if all senders have been
    /// dropped. If the channel is empty, the returned future will yield to the async runtime.
    pub fn recv_async(&self) -> RecvFut<'_, T> {
        RecvFut::new(OwnedOrRef::Ref(self))
    }

    /// Convert this receiver into a future that asynchronously receives a single message from the
    /// channel, returning an error if all senders have been dropped. If the channel is empty, this
    /// future will yield to the async runtime.
    pub fn into_recv_async<'a>(self) -> RecvFut<'a, T> {
        RecvFut::new(OwnedOrRef::Owned(self))
    }

    /// Create an asynchronous stream that uses this receiver to asynchronously receive messages
    /// from the channel. The receiver will continue to be usable after the stream has been dropped.
    pub fn stream(&self) -> RecvStream<'_, T> {
        RecvStream(RecvFut::new(OwnedOrRef::Ref(self)))
    }

    /// Convert this receiver into a stream that allows asynchronously receiving messages from the channel.
    pub fn into_stream<'a>(self) -> RecvStream<'a, T> {
        RecvStream(RecvFut::new(OwnedOrRef::Owned(self)))
    }
}

/// A future which allows asynchronously receiving a message.
///
/// Can be created via [`Receiver::recv_async`] or [`Receiver::into_recv_async`].
#[must_use = "futures/streams/sinks do nothing unless you `.await` or poll them"]
pub struct RecvFut<'a, T> {
    receiver: OwnedOrRef<'a, Receiver<T>>,
    hook: Option<Arc<Hook<T, AsyncSignal>>>,
}

impl<'a, T> RecvFut<'a, T> {
    fn new(receiver: OwnedOrRef<'a, Receiver<T>>) -> Self {
        Self {
            receiver,
            hook: None,
        }
    }

    /// Reset the hook, clearing it and removing it from the waiting receivers queue and waking
    /// another receiver if this receiver has been woken, so as not to cause any missed wakeups.
    /// This is called on drop and after a new item is received in `Stream::poll_next`.
    fn reset_hook(&mut self) {
        if let Some(hook) = self.hook.take() {
            let hook: Arc<Hook<T, dyn Signal>> = hook;
            let mut wakes = DeferredSendWakes::new();
            let mut chan = wait_lock(&self.receiver.shared.chan);
            // We'd like to use `Arc::ptr_eq` here but it doesn't seem to work consistently with wide pointers?
            chan.waiting
                .retain(|s| s.signal().as_ptr() != hook.signal().as_ptr());
            if hook
                .signal()
                .as_any()
                .downcast_ref::<AsyncSignal>()
                .unwrap()
                .woken
                .load(Ordering::SeqCst)
            {
                // If this signal has been fired, but we're being dropped (and so not listening to it),
                // pass the signal on to another receiver
                chan.try_wake_receiver_if_pending(&mut wakes);
            }
        }
    }

    fn poll_inner(
        self: Pin<&mut Self>,
        cx: &mut Context,
        stream: bool,
    ) -> Poll<Result<T, RecvError>> {
        if self.hook.is_some() {
            match self.receiver.shared.recv_sync(None) {
                Ok(msg) => return Poll::Ready(Ok(msg)),
                Err(TryRecvTimeoutError::Disconnected) => {
                    return Poll::Ready(Err(RecvError::Disconnected))
                }
                _ => (),
            }

            let hook = self.hook.as_ref().map(Arc::clone).unwrap();
            hook.update_waker(cx.waker());
            // Recheck the queue and rearm exactly once under the same lock.
            // A sender may have committed after the first receive attempt.
            let mut wakes = DeferredSendWakes::new();
            let mut chan = wait_lock(&self.receiver.shared.chan);
            chan.pull_pending(true, &mut wakes);
            if let Some(msg) = chan.queue.pop_front() {
                Poll::Ready(Ok(msg))
            } else if self.receiver.shared.is_disconnected() {
                Poll::Ready(Err(RecvError::Disconnected))
            } else {
                if hook.signal().woken.swap(false, Ordering::SeqCst) {
                    chan.waiting.push_back(hook);
                }
                Poll::Pending
            }
        } else {
            let mut_self = self.get_mut();
            let (shared, this_hook) = (&mut_self.receiver.shared, &mut mut_self.hook);
            let signal = AsyncSignal::new(cx, stream);

            shared
                .recv(
                    // should_block
                    true,
                    // make_signal
                    || Hook::trigger(signal),
                    // do_block
                    |hook| {
                        *this_hook = Some(hook);
                        Poll::Pending
                    },
                )
                .map(|r| {
                    r.map_err(|err| match err {
                        TryRecvTimeoutError::Disconnected => RecvError::Disconnected,
                        _ => unreachable!(),
                    })
                })
        }
    }

    /// See [`Receiver::is_disconnected`].
    pub fn is_disconnected(&self) -> bool {
        self.receiver.is_disconnected()
    }

    /// See [`Receiver::is_empty`].
    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }

    /// See [`Receiver::is_full`].
    pub fn is_full(&self) -> bool {
        self.receiver.is_full()
    }

    /// See [`Receiver::len`].
    pub fn len(&self) -> usize {
        self.receiver.len()
    }

    /// See [`Receiver::capacity`].
    pub fn capacity(&self) -> Option<usize> {
        self.receiver.capacity()
    }
}

impl<'a, T> Debug for RecvFut<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecvFut").finish()
    }
}

impl<'a, T> Drop for RecvFut<'a, T> {
    fn drop(&mut self) {
        self.reset_hook();
    }
}

impl<'a, T> Future for RecvFut<'a, T> {
    type Output = Result<T, RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = self.as_mut().poll_inner(cx, false);
        if result.is_ready() {
            self.reset_hook();
        }
        result
    }
}

impl<'a, T> FusedFuture for RecvFut<'a, T> {
    fn is_terminated(&self) -> bool {
        self.receiver.shared.is_disconnected() && self.receiver.shared.is_empty()
    }
}

/// A stream which allows asynchronously receiving messages.
///
/// Can be created via [`Receiver::stream`] or [`Receiver::into_stream`].
pub struct RecvStream<'a, T>(RecvFut<'a, T>);

impl<'a, T> RecvStream<'a, T> {
    /// See [`Receiver::is_disconnected`].
    pub fn is_disconnected(&self) -> bool {
        self.0.is_disconnected()
    }

    /// See [`Receiver::is_empty`].
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// See [`Receiver::is_full`].
    pub fn is_full(&self) -> bool {
        self.0.is_full()
    }

    /// See [`Receiver::len`].
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// See [`Receiver::capacity`].
    pub fn capacity(&self) -> Option<usize> {
        self.0.capacity()
    }

    /// Returns whether the SendSinks are belong to the same channel.
    pub fn same_channel(&self, other: &Self) -> bool {
        self.0.receiver.same_channel(&*other.0.receiver)
    }
}

impl<'a, T> Debug for RecvStream<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecvStream").finish()
    }
}

impl<'a, T> Clone for RecvStream<'a, T> {
    fn clone(&self) -> RecvStream<'a, T> {
        RecvStream(RecvFut::new(self.0.receiver.clone()))
    }
}

impl<'a, T> Stream for RecvStream<'a, T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.0).poll_inner(cx, true) {
            // stream = true
            Poll::Pending => Poll::Pending,
            Poll::Ready(item) => {
                self.0.reset_hook();
                Poll::Ready(item.ok())
            }
        }
    }
}

impl<'a, T> FusedStream for RecvStream<'a, T> {
    fn is_terminated(&self) -> bool {
        self.0.is_terminated()
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;

    #[test]
    fn cancelled_async_receiver_hands_message_to_blocking_receiver() {
        let (tx, rx) = crate::bounded(1);
        let waker = futures::task::noop_waker();
        let mut first = Box::pin(rx.recv_async());
        assert!(first
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        let other = rx.clone();
        let (completed_tx, completed_rx) = std::sync::mpsc::channel();
        let blocked = std::thread::spawn(move || {
            completed_tx.send(other.recv()).unwrap();
        });
        // Wait for actual registration, not a guessed scheduling delay.
        let deadline = Instant::now() + Duration::from_secs(2);
        while wait_lock(&rx.shared.chan).waiting.len() != 2 {
            if Instant::now() >= deadline {
                drop(tx);
                blocked.thread().unpark();
                blocked.join().unwrap();
                panic!("blocking receiver never registered");
            }
            std::thread::yield_now();
        }
        let (result, wakes) = tx.try_send_deferred(42);
        result.unwrap();
        drop(first);
        drop(wakes);
        // recv_timeout would fall back to polling the shared queue when its
        // deadline expires, masking a missing synchronous slot handoff.
        let completed = completed_rx.recv_timeout(Duration::from_secs(2));
        // Always reclaim the owned thread, even if a mutant stranded its hook.
        drop(tx);
        blocked.thread().unpark();
        blocked.join().unwrap();
        assert_eq!(
            completed,
            Ok(Ok(42)),
            "handoff must complete before disconnect"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn replacement_notification_releases_registration_lock() {
        let old = futures::task::noop_waker();
        let hook = Hook::<(), _>::trigger(AsyncSignal::new(&Context::from_waker(&old), false));
        let weak = Arc::downgrade(&hook);
        let observed = Arc::new(AtomicBool::new(false));
        let callback_observed = observed.clone();
        let new = waker_fn::waker_fn(move || {
            let hook = weak.upgrade().unwrap();
            assert!(
                hook.signal().waker.try_lock().is_some(),
                "callback owns registration lock"
            );
            callback_observed.store(true, Ordering::SeqCst);
        });
        hook.signal().prepare();
        assert!(hook.update_waker(&new));
        assert!(observed.load(Ordering::SeqCst));
    }
}
