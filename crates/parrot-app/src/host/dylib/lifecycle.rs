use parrot_abi::{
    AbiComponent, AbiTraceParentV1, DylibHandle, DylibLoader, LoadError, UnloadError, UnloadReport,
    ABI_ERR_PANIC, ABI_ERR_STATE,
};
use parrot_api::trace::{self, SpanGuard, SpanKind, SpanStatus, TraceContext};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

#[derive(Default)]
pub(super) struct StopRequest {
    requested: AtomicBool,
    context: Mutex<Option<TraceContext>>,
}
impl StopRequest {
    pub(super) fn request(&self) {
        let context = TraceContext::current();
        let mut slot = self.context.lock().unwrap();
        if !self.requested.load(Ordering::Relaxed) {
            *slot = context;
            self.requested.store(true, Ordering::Release);
        }
    }

    pub(super) fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    pub(super) fn take_context(&self) -> Option<TraceContext> {
        self.context.lock().unwrap().take()
    }
}

pub(super) fn run<T, E>(
    parent: Option<TraceContext>,
    phase: &str,
    path: &str,
    call: impl FnOnce() -> Result<T, E>,
    status: impl FnOnce(&Result<T, E>) -> SpanStatus,
) -> Result<T, E> {
    let Some(context) = parent.map(|parent| parent.child()) else {
        return trace::in_scope(None, call);
    };
    context.in_scope(|| {
        let guard =
            SpanGuard::current(SpanKind::NativeLifecycle, &format!("native/{phase}:{path}"));
        let result = call();
        guard.finish(status(&result));
        result
    })
}

pub(super) fn load_status<T>(result: &Result<T, LoadError>) -> SpanStatus {
    match result {
        Ok(_) => SpanStatus::Ok,
        Err(LoadError::Construct {
            code: ABI_ERR_PANIC,
            ..
        }) => SpanStatus::Panicked,
        Err(_) => SpanStatus::Error,
    }
}

pub(super) fn drain_status(result: &Result<(), (u32, String)>) -> SpanStatus {
    match result {
        Ok(()) => SpanStatus::Ok,
        Err((ABI_ERR_PANIC, _)) => SpanStatus::Panicked,
        Err(_) => SpanStatus::Error,
    }
}

pub(super) fn unload_status<T>(result: &Result<T, UnloadError>) -> SpanStatus {
    match result {
        Ok(_) => SpanStatus::Ok,
        Err(UnloadError::DrainFailed { codes, .. }) if codes.contains(&ABI_ERR_PANIC) => {
            SpanStatus::Panicked
        }
        Err(UnloadError::DestroyFailed {
            codes, drain_codes, ..
        }) if codes.contains(&ABI_ERR_PANIC) || drain_codes.contains(&ABI_ERR_PANIC) => {
            SpanStatus::Panicked
        }
        Err(UnloadError::RecordingCallbackPanicked { .. }) => SpanStatus::Panicked,
        Err(_) => SpanStatus::Error,
    }
}

fn parent(context: &Option<TraceContext>) -> Option<AbiTraceParentV1> {
    context.as_ref().map(|context| AbiTraceParentV1 {
        trace_id: context.trace_id(),
        span_id: context.span_id(),
    })
}

pub(super) fn construct(
    handle: &DylibHandle,
    config: &str,
    next: &mut u64,
) -> Result<*mut AbiComponent, LoadError> {
    let context = TraceContext::current();
    let parent = parent(&context);
    let mut session = super::recording::Session::new(context, next);
    let host = session.host();
    // SAFETY: the worker owns this trusted handle and synchronous call session.
    let result = unsafe {
        DylibLoader.construct_with_recording(handle, config, parent.as_ref(), Some(&host))
    };
    let mut outcome = result.as_ref().map(|_| ()).map_err(|error| match error {
        LoadError::Construct { code, detail } => (*code, detail.clone()),
        other => (ABI_ERR_STATE, other.to_string()),
    });
    session.complete(&mut outcome);
    // A callback failure can follow successful allocation. The loader still
    // owns that instance; the worker's failed-start cleanup must unload it.
    outcome.map_err(|(code, detail)| LoadError::Construct { code, detail })?;
    result
}

pub(super) unsafe fn drain(
    handle: &DylibHandle,
    component: *mut AbiComponent,
    next: &mut u64,
) -> Result<(), (u32, String)> {
    let context = TraceContext::current();
    let parent = parent(&context);
    let mut session = super::recording::Session::new(context, next);
    let host = session.host();
    let mut result =
        DylibLoader.drain_instance_with_recording(handle, component, parent.as_ref(), Some(&host));
    session.complete(&mut result);
    result
}

pub(super) fn unload(handle: DylibHandle, next: &mut u64) -> Result<UnloadReport, UnloadError> {
    let context = TraceContext::current();
    let parent = parent(&context);
    let mut session = super::recording::Session::new(context, next);
    let host = session.host();
    // SAFETY: all native instances are exclusive to this worker; every borrowed
    // callback finishes before unload returns, including library finalizers.
    let result =
        unsafe { DylibLoader.unload_blocking_with_recording(handle, parent.as_ref(), Some(&host)) };
    let mut outcome = match unload_status(&result) {
        SpanStatus::Ok => Ok(()),
        SpanStatus::Panicked => Err((ABI_ERR_PANIC, "native cleanup panicked".into())),
        _ => Err((ABI_ERR_STATE, "native cleanup failed".into())),
    };
    let panicked = session.panicked();
    session.complete(&mut outcome);
    if panicked {
        return Err(UnloadError::RecordingCallbackPanicked {
            cleanup: format!("{result:?}"),
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_stop_identity_survives_later_requests_and_is_released_after_take() {
        let request = StopRequest::default();
        let first = TraceContext::root();
        first.in_scope(|| request.request());
        TraceContext::root().in_scope(|| request.request());
        assert!(request.requested());
        assert_eq!(request.take_context().unwrap().span_id(), first.span_id());
        first.in_scope(|| request.request());
        assert!(request.take_context().is_none());
    }

    #[test]
    fn explicitly_untraced_first_stop_cannot_acquire_a_later_identity() {
        let request = StopRequest::default();
        trace::in_scope(None, || request.request());
        TraceContext::root().in_scope(|| request.request());
        assert!(request.requested());
        assert!(request.take_context().is_none());
    }
}
