//! The default serial batch owns each delegated handler through disposal.
use crate::{
    actor::Actor,
    errors::ActorError,
    owned_input::{call, Input},
    types::{ActorResult, BoxedFuture, BoxedMessage},
};
use std::{
    sync::{Arc, Mutex},
    task::Poll,
};

/// Cleanup sink supplied by an engine for the default batch's nested handlers.
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct BatchCleanup(Arc<Mutex<Option<String>>>);
impl BatchCleanup {
    pub fn failure(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
    fn record(&self, result: &ActorResult<()>) {
        if let Err(ActorError::Panic(message)) = result {
            self.0
                .lock()
                .unwrap()
                .get_or_insert_with(|| message.clone());
        }
    }
}
tokio::task_local! { static CURRENT: BatchCleanup; }

/// The default callback captures this sink at construction, including for Drop.
#[doc(hidden)]
pub fn scope<F: std::future::Future>(
    cleanup: BatchCleanup,
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    CURRENT.scope(cleanup, future)
}

struct Handler<'a> {
    future: Input<BoxedFuture<'a, ActorResult<BoxedMessage>>>,
    cleanup: BatchCleanup,
}
struct BatchInput<T> {
    input: Input<T>,
    cleanup: BatchCleanup,
}
impl<T> BatchInput<T> {
    fn new(value: T, cleanup: &BatchCleanup) -> Self {
        Self {
            input: Input::new(value),
            cleanup: cleanup.clone(),
        }
    }
    fn take(&mut self) -> T {
        self.input.take()
    }
}
impl<T> Drop for BatchInput<T> {
    fn drop(&mut self) {
        let result = self.input.retire();
        self.cleanup.record(&result);
    }
}
impl Handler<'_> {
    fn retire(&mut self) -> ActorResult<()> {
        let result = self.future.retire();
        self.cleanup.record(&result);
        result
    }
}
impl Drop for Handler<'_> {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}
impl std::future::Future for Handler<'_> {
    type Output = ActorResult<BoxedMessage>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        let result = match call(|| self.future.value_mut().as_mut().poll(cx)) {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => result,
            Err(error) => Err(error),
        };
        let cleanup = self.retire();
        Poll::Ready(match result {
            Err(error) => Err(error),
            Ok(value) => match cleanup {
                Ok(()) => Ok(value),
                Err(error) => {
                    drop(Input::new(value));
                    Err(error)
                }
            },
        })
    }
}

pub(crate) fn serial<'a, A: Actor + ?Sized>(
    actor: &'a mut A,
    messages: Vec<BoxedMessage>,
    ctx: &'a mut A::Context,
) -> BoxedFuture<'a, Vec<ActorResult<BoxedMessage>>> {
    let cleanup = CURRENT.try_with(Clone::clone).unwrap_or_default();
    let messages = messages
        .into_iter()
        .map(|message| BatchInput::new(message, &cleanup))
        .collect::<Vec<_>>();
    Box::pin(async move {
        let mut results = Vec::with_capacity(messages.len());
        let mut messages = messages.into_iter();
        while let Some(mut message) = messages.next() {
            let result = match call(|| actor.receive_message(message.take(), ctx)) {
                Ok(future) => {
                    Handler {
                        future: Input::new(future),
                        cleanup: cleanup.clone(),
                    }
                    .await
                }
                Err(error) => Err(error),
            };
            let fatal = matches!(result, Err(ActorError::Panic(_))) || cleanup.failure().is_some();
            results.push(BatchInput::new(result, &cleanup));
            if fatal {
                results
                    .extend(messages.map(|_| BatchInput::new(Err(ActorError::Stopped), &cleanup)));
                break;
            }
        }
        results
            .into_iter()
            .map(|mut result| result.take())
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::Context,
    };
    struct Message(Arc<AtomicUsize>, bool);
    impl Drop for Message {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
            assert!(!self.1, "batch abandoned input panic");
        }
    }
    struct Probe {
        calls: usize,
        mode: u8,
    }
    struct Work {
        message: Option<BoxedMessage>,
        mode: u8,
    }
    impl std::future::Future for Work {
        type Output = ActorResult<BoxedMessage>;
        fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            match self.mode {
                0 => Poll::Ready(Ok(self.message.take().unwrap())),
                1 | 2 => Poll::Ready(Err(ActorError::InternalError(
                    "original batch error".into(),
                ))),
                3 => panic!("batch nested poll panic"),
                4 => Poll::Pending,
                _ => unreachable!(),
            }
        }
    }
    impl Drop for Work {
        fn drop(&mut self) {
            drop(self.message.take());
            assert!(!matches!(self.mode, 2 | 4), "batch nested disposal panic");
        }
    }
    impl Actor for Probe {
        type Config = crate::actor::EmptyConfig;
        type Context = ();
        fn state(&self) -> crate::actor::ActorState {
            crate::actor::ActorState::Running
        }
        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            _: &'a mut (),
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            self.calls += 1;
            Box::pin(Work {
                message: Some(message),
                mode: if self.calls == 1 { self.mode } else { 0 },
            })
        }
    }
    fn inputs(drops: &Arc<AtomicUsize>, hostile: bool) -> Vec<BoxedMessage> {
        (0..3)
            .map(|_| Box::new(Message(drops.clone(), hostile)) as BoxedMessage)
            .collect()
    }
    #[tokio::test]
    async fn ordinary_error_continues_without_cleanup_failure() {
        let drops = Arc::new(AtomicUsize::new(0));
        let cleanup = BatchCleanup::default();
        let mut actor = Probe { calls: 0, mode: 1 };
        let results = scope(cleanup.clone(), async {
            serial(&mut actor, inputs(&drops, false), &mut ()).await
        })
        .await;
        assert_eq!(actor.calls, 3);
        assert!(
            matches!(&results[0], Err(ActorError::InternalError(message)) if message == "original batch error")
        );
        assert!(results[1..].iter().all(Result::is_ok));
        drop(results);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        assert!(cleanup.failure().is_none());
    }
    #[tokio::test]
    async fn failed_cleanup_preserves_error_but_stops_remaining_handlers() {
        let drops = Arc::new(AtomicUsize::new(0));
        let cleanup = BatchCleanup::default();
        let mut actor = Probe { calls: 0, mode: 2 };
        let results = scope(cleanup.clone(), async {
            serial(&mut actor, inputs(&drops, false), &mut ()).await
        })
        .await;
        assert_eq!(actor.calls, 1);
        assert_eq!(results.len(), 3);
        assert!(
            matches!(&results[0], Err(ActorError::InternalError(message)) if message == "original batch error")
        );
        assert!(results[1..]
            .iter()
            .all(|result| matches!(result, Err(ActorError::Stopped))));
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        assert_eq!(
            cleanup.failure().as_deref(),
            Some("batch nested disposal panic")
        );
    }
    #[tokio::test]
    async fn direct_default_batch_stops_after_cleanup_failure_without_engine_scope() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut actor = Probe { calls: 0, mode: 2 };
        let results = serial(&mut actor, inputs(&drops, false), &mut ()).await;
        assert_eq!(
            actor.calls, 1,
            "direct API use must not resume after failed cleanup"
        );
        assert_eq!(results.len(), 3);
        assert!(
            matches!(&results[0], Err(ActorError::InternalError(message)) if message == "original batch error")
        );
        assert!(results[1..]
            .iter()
            .all(|result| matches!(result, Err(ActorError::Stopped))));
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    }
    #[tokio::test]
    async fn nested_poll_panic_stops_without_claiming_cleanup_failed() {
        let drops = Arc::new(AtomicUsize::new(0));
        let cleanup = BatchCleanup::default();
        let mut actor = Probe { calls: 0, mode: 3 };
        let results = scope(cleanup.clone(), async {
            serial(&mut actor, inputs(&drops, false), &mut ()).await
        })
        .await;
        assert_eq!(actor.calls, 1);
        assert!(
            matches!(&results[0], Err(ActorError::Panic(message)) if message == "batch nested poll panic")
        );
        assert!(results[1..]
            .iter()
            .all(|result| matches!(result, Err(ActorError::Stopped))));
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        assert!(cleanup.failure().is_none());
    }
    #[tokio::test]
    async fn cancellation_records_nested_disposal_and_retires_queued_inputs() {
        let drops = Arc::new(AtomicUsize::new(0));
        let cleanup = BatchCleanup::default();
        let mut actor = Probe { calls: 0, mode: 4 };
        let mut context = ();
        let mut future = Box::pin(scope(cleanup.clone(), async {
            serial(&mut actor, inputs(&drops, false), &mut context).await
        }));
        assert!(futures::poll!(future.as_mut()).is_pending());
        drop(future);
        assert_eq!(actor.calls, 1);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        assert_eq!(
            cleanup.failure().as_deref(),
            Some("batch nested disposal panic")
        );
    }
    #[tokio::test]
    async fn unpolled_default_batch_retires_each_hostile_input_independently() {
        let drops = Arc::new(AtomicUsize::new(0));
        let cleanup = BatchCleanup::default();
        let mut actor = Probe { calls: 0, mode: 0 };
        scope(cleanup.clone(), async {
            drop(serial(&mut actor, inputs(&drops, true), &mut ()));
        })
        .await;
        assert_eq!(actor.calls, 0);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        assert_eq!(
            cleanup.failure().as_deref(),
            Some("batch abandoned input panic")
        );
    }
}
