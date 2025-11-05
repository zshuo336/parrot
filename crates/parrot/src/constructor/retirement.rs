//! Capture retirement has one receipt, including executor/task cancellation.
use super::*;

type Finished = watch::Sender<Option<Result<(), String>>>;

// Moved out of FactoryState under its lock, then destroyed outside that lock.
// Installed before waiting on capacity or scheduling blocking work.
struct CaptureDisposal<A> {
    context: Option<parrot_api::trace::TraceContext>,
    function: Option<Arc<dyn Fn() -> A + Send + Sync>>,
    failure: Option<String>,
    finished: Option<Finished>,
}
impl<A> CaptureDisposal<A> {
    fn take(state: &mut FactoryState<A>, finished: Option<Finished>) -> Self {
        Self {
            context: state.capture_context.take(),
            function: state.function.take(),
            failure: state.retirement_failure.clone(),
            finished,
        }
    }
    fn retire(&mut self) -> Result<(), String> {
        parrot_api::trace::in_scope(self.context.clone(), || {
            let captures = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drop(self.function.take());
            }))
            .map_err(|payload| {
                format!(
                    "factory capture disposal failed: {}",
                    panic_message(payload)
                )
            });
            // An earlier unpublished-output failure must not be hidden by either
            // successful capture retirement or a later capture failure.
            let result = self.failure.take().map_or(captures, Err);
            if let Some(finished) = self.finished.take() {
                finished.send_replace(Some(result.clone()));
            }
            result
        })
    }
}
impl<A> Drop for CaptureDisposal<A> {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}
impl<A> Drop for FactoryState<A> {
    fn drop(&mut self) {
        drop(CaptureDisposal {
            context: self.capture_context.take(),
            function: self.function.take(),
            failure: self.retirement_failure.take(),
            finished: self.fallback.take(),
        });
    }
}

struct Retirement<A> {
    state: Arc<Mutex<FactoryState<A>>>,
    finished: Option<Finished>,
}
impl<A> Retirement<A> {
    fn take_if_idle(&mut self) -> Option<CaptureDisposal<A>> {
        let mut state = self.state.lock().unwrap();
        if state.active != 0 {
            return None;
        }
        Some(CaptureDisposal::take(
            &mut state,
            Some(self.finished.take().expect("one retirement owner")),
        ))
    }
}
impl<A> Drop for Retirement<A> {
    fn drop(&mut self) {
        let Some(finished) = self.finished.take() else {
            return;
        };
        let captures = {
            let mut state = self.state.lock().unwrap();
            if state.active == 0 {
                Some(CaptureDisposal::take(&mut state, Some(finished)))
            } else {
                // A prepared output or blocking constructor still holds an
                // invocation. Its last release must finish actual cleanup;
                // cancelling this task is not a successful retirement.
                state.fallback = Some(finished);
                None
            }
        };
        drop(captures);
    }
}

pub(super) fn invocation_finished<A>(invocation: &mut Invocation<A>) -> Result<(), String> {
    // Both explicit retirement and Drop may visit the same invocation.
    if invocation.function.is_none() {
        return Ok(());
    }
    drop(invocation.function.take());
    let captures = {
        let mut state = invocation.state.lock().unwrap();
        state.active -= 1;
        if state.active == 0 {
            if let Some(finished) = state.fallback.take() {
                Some(CaptureDisposal::take(&mut state, Some(finished)))
            } else if Arc::strong_count(&invocation.state) == 1 {
                // No Factory or retirement owner survives: the prepared
                // output's discard receipt must include this last capture.
                Some(CaptureDisposal::take(&mut state, None))
            } else {
                None
            }
        } else {
            None
        }
    };
    invocation.changed.notify_waiters();
    captures.map_or(Ok(()), |mut captures| captures.retire())
}

impl<A: Send + 'static> Factory<A> {
    /// Start once at call time. The waiter never owns cleanup execution.
    pub(crate) fn retire(&self) -> BoxedFuture<'static, Result<(), String>> {
        self.retire_on(tokio::runtime::Handle::try_current().ok())
    }
    pub(super) fn retire_on(
        &self,
        runtime: Option<tokio::runtime::Handle>,
    ) -> BoxedFuture<'static, Result<(), String>> {
        let (receipt, owner) = {
            let mut state = self.state.lock().unwrap();
            if let Some(receipt) = &state.disposal {
                (receipt.clone(), None)
            } else {
                let (finished, receipt) = watch::channel(None);
                state.disposal = Some(receipt.clone());
                (
                    receipt,
                    Some(Retirement {
                        state: self.state.clone(),
                        finished: Some(finished),
                    }),
                )
            }
        };
        if let Some(mut owner) = owner {
            if let Some(runtime) = runtime {
                let changed = self.changed.clone();
                let slots = self.cleanup.clone();
                runtime.spawn(async move {
                    let captures = loop {
                        let notified = changed.notified();
                        tokio::pin!(notified);
                        notified.as_mut().enable();
                        if let Some(captures) = owner.take_if_idle() {
                            break captures;
                        }
                        notified.await;
                    };
                    let permit = slots.acquire_owned().await;
                    let _ = tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        drop(captures);
                    })
                    .await
                    .map_err(crate::owned_payload::join_error);
                });
            } else {
                // No executor: retire synchronously if idle, otherwise transfer
                // the receipt to the last invocation. No hidden worker/runtime.
                drop(owner);
            }
        }
        wait(receipt)
    }
}
fn wait(
    mut receipt: watch::Receiver<Option<Result<(), String>>>,
) -> BoxedFuture<'static, Result<(), String>> {
    Box::pin(async move {
        loop {
            if let Some(result) = receipt.borrow_and_update().clone() {
                return result;
            }
            receipt
                .changed()
                .await
                .map_err(|_| "factory disposal owner ended without a receipt".to_owned())?;
        }
    })
}
