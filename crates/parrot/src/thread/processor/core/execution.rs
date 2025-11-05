//! Own a scheduled processor future through polling AND destruction. Actor
//! storage cannot retire until every execution lease has released its future.
use super::*;
use std::task::{Context, Poll};

#[derive(Debug, Default)]
pub(super) struct Executions {
    state: std::sync::Mutex<(usize, Option<String>)>,
    finished: tokio::sync::Notify,
}
impl Executions {
    pub(super) fn own<T: Send + 'static>(
        self: &Arc<Self>,
        future: BoxedProcessorFuture<Result<T, SystemError>>,
        phase: &'static str,
    ) -> BoxedProcessorFuture<Result<T, SystemError>> {
        self.state.lock().unwrap().0 += 1;
        Box::pin(Execution {
            future: Some(future),
            state: self.clone(),
            phase,
        })
    }
    pub(super) async fn join(&self) -> Result<(), SystemError> {
        loop {
            let finished = self.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            {
                let state = self.state.lock().unwrap();
                if state.0 == 0 {
                    return state
                        .1
                        .as_ref()
                        .map_or(Ok(()), |error| Err(SystemError::Panic(error.clone())));
                }
            }
            finished.await;
        }
    }
}
struct Execution<T> {
    future: Option<BoxedProcessorFuture<Result<T, SystemError>>>,
    state: Arc<Executions>,
    phase: &'static str,
}
impl<T> Execution<T> {
    fn dispose(&mut self) -> Result<(), SystemError> {
        let Some(future) = self.future.take() else {
            return Ok(());
        };
        let disposed = panic::catch_unwind(AssertUnwindSafe(|| drop(future))).map_err(|payload| {
            format!(
                "{} future destructor panicked: {}",
                self.phase,
                crate::owned_payload::panic_message(payload)
            )
        });
        {
            let mut state = self.state.state.lock().unwrap();
            if let Err(error) = &disposed {
                state.1.get_or_insert(error.clone());
            }
            state.0 -= 1;
        }
        self.state.finished.notify_waiters();
        disposed.map_err(SystemError::Panic)
    }
}
impl<T> Drop for Execution<T> {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}
impl<T> std::future::Future for Execution<T> {
    type Output = Result<T, SystemError>;
    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let polled = panic::catch_unwind(AssertUnwindSafe(|| {
            self.future
                .as_mut()
                .expect("execution polled after completion")
                .as_mut()
                .poll(cx)
        }));
        let result = match polled {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => result,
            Err(payload) => Err(SystemError::Panic(format!(
                "{} panicked: {}",
                self.phase,
                crate::owned_payload::panic_message(payload)
            ))),
        };
        match self.dispose() {
            Ok(()) => Poll::Ready(result),
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}

#[cfg(test)]
mod recursive_panic_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Payload;
    impl Drop for Payload {
        fn drop(&mut self) {
            panic!("execution payload destructor");
        }
    }
    struct Probe {
        poll_panic: bool,
        drop_panic: bool,
        drops: Arc<AtomicUsize>,
    }
    impl std::future::Future for Probe {
        type Output = Result<(), SystemError>;
        fn poll(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            if self.poll_panic {
                std::panic::panic_any(Payload);
            }
            Poll::Ready(Ok(()))
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            if self.drop_panic {
                std::panic::panic_any(Payload);
            }
        }
    }
    async fn check(poll: bool, drop_panic: bool) {
        let owner = Arc::new(Executions::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let mut future = owner.own(
            Box::pin(Probe {
                poll_panic: !drop_panic,
                drop_panic,
                drops: drops.clone(),
            }),
            "probe",
        );
        if poll {
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
            }));
            let result = outcome.expect("secondary payload panic escaped execution poll");
            let Poll::Ready(Err(SystemError::Panic(message))) = result else {
                panic!("wrong execution error class: {result:?}");
            };
            assert!(
                message.contains("probe") && message.contains("panic payload destructor panicked"),
                "{message}"
            );
        }
        assert!(
            panic::catch_unwind(AssertUnwindSafe(|| drop(future))).is_ok(),
            "secondary payload panic escaped execution drop"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        for _ in 0..2 {
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), owner.join())
                .await
                .expect("execution lease never released");
            if drop_panic {
                let error = result.unwrap_err().to_string();
                assert!(
                    error.contains("panic payload destructor panicked"),
                    "{error}"
                );
            } else {
                result.unwrap();
            }
        }
    }
    #[tokio::test]
    async fn polling_secondary_panic_preserves_error_and_releases_execution() {
        check(true, false).await;
    }
    #[tokio::test]
    async fn cancelled_secondary_panic_preserves_retirement_and_releases_execution() {
        check(false, true).await;
    }
    #[tokio::test]
    async fn completed_secondary_panic_preserves_retirement_and_releases_execution() {
        check(true, true).await;
    }
}
