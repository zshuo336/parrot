//! Safe callable erasure for stream callbacks. Runtime backends execute these
//! hooks with a view of the actual actor context on its serial dispatch path.
use crate::{
    context::ActorContext,
    errors::ActorError,
    stream::StreamHandler,
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use futures::Stream;
use std::{
    any::{Any, TypeId},
    marker::PhantomData,
};

/// Borrowed context access used by runtime stream dispatch.
#[doc(hidden)]
pub trait StreamContextAccess: Send {
    fn concrete(&mut self) -> &mut dyn Any;
    fn actor_context(&mut self) -> &mut (dyn ActorContext + 'static);
}

/// A view retains one exclusive borrow; conversions borrow it sequentially.
#[doc(hidden)]
pub struct StreamContextView<'a, C: ActorContext + 'static>(pub &'a mut C);
impl<C: ActorContext + 'static> StreamContextAccess for StreamContextView<'_, C> {
    fn concrete(&mut self) -> &mut dyn Any {
        self.0
    }
    fn actor_context(&mut self) -> &mut (dyn ActorContext + 'static) {
        self.0
    }
}

/// Resolves the context requested by a custom stream handler.
/// Sized context types use checked downcasts; `dyn ActorContext` uses the
/// common actor interface. Other unsized context interfaces can implement this
/// trait explicitly rather than relying on unchecked pointer casts.
pub trait StreamHandlerContext: Send {
    #[doc(hidden)]
    fn concrete_type() -> Option<TypeId>;
    #[doc(hidden)]
    fn resolve(view: &mut dyn StreamContextAccess) -> Option<&mut Self>;
}
impl<C: Any + Send> StreamHandlerContext for C {
    fn concrete_type() -> Option<TypeId> {
        Some(TypeId::of::<C>())
    }
    fn resolve(view: &mut dyn StreamContextAccess) -> Option<&mut Self> {
        view.concrete().downcast_mut()
    }
}
impl StreamHandlerContext for dyn ActorContext {
    fn concrete_type() -> Option<TypeId> {
        None
    }
    fn resolve(view: &mut dyn StreamContextAccess) -> Option<&mut Self> {
        Some(view.actor_context())
    }
}

/// The runtime emits these events in stream order on the actor dispatch path.
#[doc(hidden)]
pub enum StreamCallback {
    Started,
    Item(BoxedMessage),
    Finished,
    Error(ActorError),
}
trait ErasedHandler: Send {
    fn context_type(&self) -> Option<TypeId>;
    fn call<'a>(
        &'a mut self,
        event: StreamCallback,
        context: &'a mut dyn StreamContextAccess,
        cleanup: &'a mut Option<String>,
    ) -> BoxedFuture<'a, ActorResult<()>>;
}
struct Handler<S, H, C: ?Sized> {
    handler: H,
    types: PhantomData<fn(&C) -> S>,
}
impl<S, H, C> ErasedHandler for Handler<S, H, C>
where
    S: Stream + Send + 'static,
    S::Item: Send + 'static,
    H: StreamHandler<S, C> + 'static,
    C: StreamHandlerContext + ?Sized + 'static,
{
    fn context_type(&self) -> Option<TypeId> {
        C::concrete_type()
    }
    fn call<'a>(
        &'a mut self,
        event: StreamCallback,
        context: &'a mut dyn StreamContextAccess,
        cleanup: &'a mut Option<String>,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        let mut event = CallbackEvent {
            input: crate::owned_input::Input::new(event),
            cleanup,
        };
        Box::pin(async move {
            let ctx = C::resolve(context).ok_or_else(|| {
                ActorError::MessageHandlingError("stream handler context type mismatch".into())
            })?;
            let future = callback_call(|| -> ActorResult<_> {
                Ok(match event.input.take() {
                    StreamCallback::Started => self.handler.started(ctx),
                    StreamCallback::Item(item) => {
                        let item = match item.downcast::<S::Item>() {
                            Ok(item) => item,
                            Err(item) => {
                                let mut item = crate::owned_input::Input::new(item);
                                let retirement = item.retire();
                                record_event_cleanup(&retirement, event.cleanup);
                                retirement?;
                                return Err(ActorError::MessageHandlingError(
                                    "stream handler item type mismatch".into(),
                                ));
                            }
                        };
                        self.handler.handle(*item, ctx)
                    }
                    StreamCallback::Finished => self.handler.finished(ctx),
                    StreamCallback::Error(error) => self.handler.handle_error(error, ctx),
                })
            })?;
            CallbackFuture {
                future: crate::owned_input::Input::new(future),
                cleanup: &mut *event.cleanup,
            }
            .await
        })
    }
}

/// Callable custom stream handler registration for the erased registry API.
/// `StreamRegistryExt::add_stream_with_handler` constructs this automatically.
/// Direct erased registrations must pass this wrapper, not arbitrary `Any`.
pub struct StreamHandlerRegistration(Box<dyn ErasedHandler>);
impl StreamHandlerRegistration {
    pub fn new<S, H, C>(handler: H) -> Self
    where
        S: Stream + Send + 'static,
        S::Item: Send + 'static,
        H: StreamHandler<S, C> + 'static,
        C: StreamHandlerContext + ?Sized + 'static,
    {
        Self(Box::new(Handler::<S, H, C> {
            handler,
            types: PhantomData,
        }))
    }
    #[doc(hidden)]
    pub fn accepts_context(&self, context: TypeId) -> bool {
        self.0
            .context_type()
            .is_none_or(|expected| expected == context)
    }
    #[doc(hidden)]
    pub fn call<'a>(
        &'a mut self,
        event: StreamCallback,
        context: &'a mut dyn StreamContextAccess,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        // Capture ownership now: the compatibility future may never be polled.
        let mut event = crate::owned_input::Input::new(event);
        Box::pin(async move {
            let mut cleanup = None;
            self.call_with_cleanup(event.take(), context, &mut cleanup)
                .await
        })
    }
    /// Runtime cleanup sink, retained outside the returned future. Cancellation
    /// records a destructor failure before releasing the actor/context borrow.
    #[doc(hidden)]
    pub fn call_with_cleanup<'a>(
        &'a mut self,
        event: StreamCallback,
        context: &'a mut dyn StreamContextAccess,
        cleanup: &'a mut Option<String>,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        self.0.call(event, context, cleanup)
    }
}

// Own the event before the async body starts. A cancelled, unpolled call
// and a failed context resolution must still report event retirement failures.
struct CallbackEvent<'a> {
    input: crate::owned_input::Input<StreamCallback>,
    cleanup: &'a mut Option<String>,
}
fn record_event_cleanup(result: &ActorResult<()>, cleanup: &mut Option<String>) {
    if let Err(ActorError::Panic(message)) = result {
        cleanup.get_or_insert(message.clone());
    }
}
impl Drop for CallbackEvent<'_> {
    fn drop(&mut self) {
        let retirement = self.input.retire();
        record_event_cleanup(&retirement, self.cleanup);
    }
}

// Preserve actual callback panic semantics after normalizing a possibly
// panicking payload. Returning an ActorError here would bypass actor supervision.
fn callback_call<T>(callback: impl FnOnce() -> T) -> T {
    match crate::owned_input::call(callback) {
        Ok(value) => value,
        Err(ActorError::Panic(message)) => std::panic::resume_unwind(Box::new(message)),
        Err(_) => unreachable!("owned_input::call only returns panic errors"),
    }
}

struct CallbackFuture<'a> {
    future: crate::owned_input::Input<BoxedFuture<'a, ()>>,
    cleanup: &'a mut Option<String>,
}
impl CallbackFuture<'_> {
    fn retire(&mut self) -> ActorResult<()> {
        let result = self.future.retire();
        if let Err(ActorError::Panic(message)) = &result {
            self.cleanup.get_or_insert_with(|| message.clone());
        }
        result
    }
}
impl std::future::Future for CallbackFuture<'_> {
    type Output = ActorResult<()>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;
        let result = match crate::owned_input::call(|| self.future.value_mut().as_mut().poll(cx)) {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(())) => Ok(()),
            Err(ActorError::Panic(message)) => {
                let _ = self.retire();
                std::panic::resume_unwind(Box::new(message));
            }
            Err(_) => unreachable!("owned_input::call only returns panic errors"),
        };
        let cleanup = self.retire();
        Poll::Ready(result.and(cleanup))
    }
}
impl Drop for CallbackFuture<'_> {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

#[cfg(test)]
mod callback_cleanup_contracts {
    use super::*;
    use std::{
        pin::Pin,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        task::{Context, Poll},
        time::{Duration, Instant},
    };
    struct View(());
    impl StreamContextAccess for View {
        fn concrete(&mut self) -> &mut dyn Any {
            &mut self.0
        }
        fn actor_context(&mut self) -> &mut (dyn ActorContext + 'static) {
            panic!("concrete handler must not ask for dynamic context")
        }
    }
    struct FutureProbe {
        drops: Arc<AtomicUsize>,
        poll_panic: bool,
        ready: bool,
        drop_panic: bool,
    }
    impl std::future::Future for FutureProbe {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            assert!(!self.poll_panic, "first stream callback poll");
            if self.ready {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }
    impl Drop for FutureProbe {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            assert!(!self.drop_panic, "stream callback future drop");
        }
    }
    struct Handler {
        drops: Arc<AtomicUsize>,
        poll_panic: bool,
        ready: bool,
        drop_panic: bool,
    }
    // Explicit async-trait expansion exposes a user future with independent
    // poll and Drop behavior, without placing a second async wrapper around it.
    impl StreamHandler<futures::stream::Empty<u64>, ()> for Handler {
        fn handle<'life0, 'life1, 'async_trait>(
            &'life0 mut self,
            _: u64,
            _: &'life1 mut (),
        ) -> BoxedFuture<'async_trait, ()>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async {})
        }
        fn started<'life0, 'life1, 'async_trait>(
            &'life0 mut self,
            _: &'life1 mut (),
        ) -> BoxedFuture<'async_trait, ()>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(FutureProbe {
                drops: self.drops.clone(),
                poll_panic: self.poll_panic,
                ready: self.ready,
                drop_panic: self.drop_panic,
            })
        }
    }
    fn invoke<'a>(
        handler: &'a mut StreamHandlerRegistration,
        view: &'a mut dyn StreamContextAccess,
        cleanup: &'a mut Option<String>,
    ) -> BoxedFuture<'a, ActorResult<()>> {
        handler.call_with_cleanup(StreamCallback::Started, view, cleanup)
    }
    fn isolated(name: &str, body: impl FnOnce()) {
        const FLAG: &str = "PARROT_STREAM_CALLBACK_CLEANUP_CASE";
        if std::env::var(FLAG).as_deref() == Ok(name) {
            body();
            return;
        }
        let full = format!("stream_dispatch::callback_cleanup_contracts::{name}");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &full, "--nocapture"])
            .env(FLAG, name)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{name}: {status}");
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{name}: deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn check(poll_panic: bool, ready: bool, drop_panic: bool, poll: bool) {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut handler =
            StreamHandlerRegistration::new::<futures::stream::Empty<u64>, Handler, ()>(Handler {
                drops: drops.clone(),
                poll_panic,
                ready,
                drop_panic,
            });
        let mut view = View(());
        let mut cleanup = None;
        let mut future = invoke(&mut handler, &mut view, &mut cleanup);
        let polled = if poll {
            Some(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || {
                    future
                        .as_mut()
                        .poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
                },
            )))
        } else {
            None
        };
        drop(future);
        assert_eq!(drops.load(Ordering::SeqCst), usize::from(poll));
        if poll && drop_panic {
            assert!(cleanup.unwrap().contains("stream callback future drop"));
        } else {
            assert!(cleanup.is_none());
        }
        match polled {
            Some(Err(payload)) => {
                assert!(poll_panic, "only an actual poll panic may unwind");
                assert!(payload
                    .downcast_ref::<String>()
                    .unwrap()
                    .contains("first stream callback poll"));
            }
            Some(Ok(Poll::Ready(Err(error)))) => {
                assert!(!poll_panic && drop_panic);
                assert!(error.to_string().contains("stream callback future drop"));
            }
            Some(Ok(Poll::Ready(Ok(())))) => assert!(ready && !poll_panic && !drop_panic),
            Some(Ok(Poll::Pending)) => assert!(!ready && !poll_panic),
            None => {}
        }
    }

    #[test]
    fn poll_and_drop_failures_preserve_first_error() {
        isolated("poll_and_drop_failures_preserve_first_error", || {
            check(true, false, true, true)
        });
    }
    #[test]
    fn ready_future_cleanup_failure_suppresses_success() {
        isolated("ready_future_cleanup_failure_suppresses_success", || {
            check(false, true, true, true)
        });
    }
    #[test]
    fn cancelled_future_cleanup_failure_reaches_runtime_sink() {
        isolated(
            "cancelled_future_cleanup_failure_reaches_runtime_sink",
            || check(false, false, true, true),
        );
    }
    #[test]
    fn unpolled_call_never_constructs_callback() {
        check(false, true, true, false);
    }
    #[test]
    fn healthy_callback_success_has_no_cleanup_failure() {
        check(false, true, false, true);
    }
    struct EventProbe(Arc<AtomicUsize>);
    impl Drop for EventProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("unconsumed callback event drop");
        }
    }
    struct WrongView(bool);
    impl StreamContextAccess for WrongView {
        fn concrete(&mut self) -> &mut dyn Any {
            &mut self.0
        }
        fn actor_context(&mut self) -> &mut (dyn ActorContext + 'static) {
            panic!("unexpected dynamic view")
        }
    }
    fn event_cleanup(poll: bool, wrong_context: bool) {
        let drops = Arc::new(AtomicUsize::new(0));
        let callback_drops = Arc::new(AtomicUsize::new(0));
        let mut handler =
            StreamHandlerRegistration::new::<futures::stream::Empty<u64>, Handler, ()>(Handler {
                drops: callback_drops.clone(),
                poll_panic: false,
                ready: true,
                drop_panic: false,
            });
        let mut good = View(());
        let mut wrong = WrongView(false);
        let context: &mut dyn StreamContextAccess =
            if wrong_context { &mut wrong } else { &mut good };
        let mut cleanup = None;
        let mut future = handler.call_with_cleanup(
            StreamCallback::Item(Box::new(EventProbe(drops.clone()))),
            context,
            &mut cleanup,
        );
        if poll {
            let result = future
                .as_mut()
                .poll(&mut Context::from_waker(futures::task::noop_waker_ref()));
            match result {
                Poll::Ready(Err(ActorError::MessageHandlingError(message))) if wrong_context => {
                    assert!(message.contains("context type mismatch"))
                }
                Poll::Ready(Err(ActorError::Panic(message))) if !wrong_context => {
                    assert!(message.contains("unconsumed callback event drop"))
                }
                other => panic!("unexpected event result: {other:?}"),
            }
        }
        drop(future);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(callback_drops.load(Ordering::SeqCst), 0);
        assert!(
            cleanup
                .as_deref()
                .is_some_and(|message| message.contains("unconsumed callback event drop")),
            "event cleanup must reach the retained runtime sink: {cleanup:?}"
        );
    }
    #[test]
    fn unpolled_event_cleanup_reaches_runtime_sink() {
        isolated("unpolled_event_cleanup_reaches_runtime_sink", || {
            event_cleanup(false, false)
        });
    }
    #[test]
    fn mismatched_item_cleanup_reaches_runtime_sink() {
        isolated("mismatched_item_cleanup_reaches_runtime_sink", || {
            event_cleanup(true, false)
        });
    }
    #[test]
    fn mismatched_context_cleanup_reaches_runtime_sink() {
        isolated("mismatched_context_cleanup_reaches_runtime_sink", || {
            event_cleanup(true, true)
        });
    }

    #[test]
    fn legacy_unpolled_call_retires_event_without_unwinding() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut handler =
            StreamHandlerRegistration::new::<futures::stream::Empty<u64>, Handler, ()>(Handler {
                drops: Arc::default(),
                poll_panic: false,
                ready: true,
                drop_panic: false,
            });
        let mut view = View(());
        let future = handler.call(
            StreamCallback::Item(Box::new(EventProbe(drops.clone()))),
            &mut view,
        );
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(future)));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(
            outcome.is_ok(),
            "unpolled compatibility wrapper must own its event before first poll"
        );
    }
}
