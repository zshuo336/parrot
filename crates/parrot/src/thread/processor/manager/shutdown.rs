use super::ActorProcessorManager;
use crate::{
    error_receipt::{SystemReceipt, system_summary},
    owned_payload::{PublicationInput, panic_message},
    thread::{SystemError, processor::core::BoxedProcessorFuture},
};
use futures::{StreamExt, stream::FuturesUnordered};
use std::{
    fmt,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Debug)]
struct Failure {
    path: String,
    stage: &'static str,
    error: SystemReceipt,
}

#[derive(Debug)]
struct ShutdownFailures(Vec<Failure>);
impl fmt::Display for ShutdownFailures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("processor snapshot shutdown failed")?;
        for failure in &self.0 {
            write!(
                f,
                "; {} ({}): {}",
                failure.path,
                failure.stage,
                system_summary(failure.error.error())
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for ShutdownFailures {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.first().map(|failure| failure.error.error() as _)
    }
}

fn panic_receipt(payload: Box<dyn std::any::Any + Send>) -> SystemReceipt {
    SystemReceipt::new(SystemError::Panic(panic_message(payload)))
}

// Keep the pinned delegate outside its poll unwind boundary: otherwise a poll
// panic and a separate destructor panic would unwind together and abort.
struct StopFuture(Option<BoxedProcessorFuture<Result<(), SystemError>>>);
impl StopFuture {
    fn dispose(&mut self) -> Option<SystemReceipt> {
        catch_unwind(AssertUnwindSafe(|| drop(self.0.take())))
            .err()
            .map(panic_receipt)
    }
}
impl Future for StopFuture {
    type Output = (Result<(), SystemReceipt>, Option<SystemReceipt>);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let polled = catch_unwind(AssertUnwindSafe(|| {
            self.0
                .as_mut()
                .expect("live processor stop")
                .as_mut()
                .poll(cx)
        }));
        let result = match polled {
            Ok(Poll::Pending) => return Poll::Pending,
            Ok(Poll::Ready(result)) => result.map_err(SystemReceipt::new),
            Err(payload) => Err(panic_receipt(payload)),
        };
        Poll::Ready((result, self.dispose()))
    }
}
impl Drop for StopFuture {
    fn drop(&mut self) {
        if let Some(error) = self.dispose() {
            tracing::warn!(
                error = %system_summary(error.error()),
                "processor stop future disposal failed during cancellation"
            );
        }
    }
}

impl ActorProcessorManager {
    pub(super) async fn shutdown_snapshot(&self) -> Result<(), SystemError> {
        let _shutdown = self.shutdown.lock().await;
        let snapshot: Vec<_> = self
            .processors
            .lock()
            .unwrap()
            .iter()
            .map(|(path, processor)| (path.clone(), PublicationInput::new(processor.clone())))
            .collect();
        let mut stops = FuturesUnordered::new();
        for (path, processor) in snapshot {
            stops.push(async move {
                let mut failures = Vec::new();
                let (result, disposal) = match catch_unwind(AssertUnwindSafe(|| {
                    processor.value().clone().stop_erased()
                })) {
                    Ok(future) => StopFuture(Some(future)).await,
                    Err(payload) => (Err(panic_receipt(payload)), None),
                };
                if result.is_ok() && disposal.is_none() {
                    let removed = {
                        let mut map = self.processors.lock().unwrap();
                        if map
                            .get(&path)
                            .is_some_and(|current| Arc::ptr_eq(current, processor.value()))
                        {
                            map.remove(&path)
                        } else {
                            None
                        }
                    };
                    // The snapshot still owns an Arc, so releasing this registry
                    // handle cannot execute the processor's last destructor.
                    drop(removed);
                }
                for (stage, error) in [("stop", result.err()), ("stop future disposal", disposal)] {
                    if let Some(error) = error {
                        failures.push(Failure {
                            path: path.clone(),
                            stage,
                            error,
                        });
                    }
                }
                if let Err(error) = processor.retire() {
                    failures.push(Failure {
                        path,
                        stage: "processor handle disposal",
                        error: SystemReceipt::new(SystemError::Panic(
                            crate::error_receipt::actor_summary(&error),
                        )),
                    });
                }
                failures
            });
        }
        let mut failures = Vec::new();
        while let Some(mut result) = stops.next().await {
            failures.append(&mut result);
        }
        if failures.is_empty() {
            Ok(())
        } else {
            failures.sort_by(|a, b| (&a.path, a.stage).cmp(&(&b.path, b.stage)));
            Err(SystemError::Other(anyhow::Error::new(ShutdownFailures(
                failures,
            ))))
        }
    }
}
