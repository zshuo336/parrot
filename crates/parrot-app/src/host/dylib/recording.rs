use parrot_abi::*;
use parrot_api::trace::{SpanGuard, SpanKind, SpanStatus, TraceContext};
use std::collections::BTreeMap;

const MAX_SPANS: usize = 256;
const MAX_TARGET_BYTES: usize = 256;

pub(super) struct Session<'a> {
    context: Option<TraceContext>,
    next: &'a mut u64,
    spans: BTreeMap<u64, (TraceContext, SpanGuard)>,
    admitted: usize,
    panicked: bool,
}
impl<'a> Session<'a> {
    pub(super) fn new(context: Option<TraceContext>, next: &'a mut u64) -> Self {
        Self {
            context,
            next,
            spans: BTreeMap::new(),
            admitted: 0,
            panicked: false,
        }
    }

    pub(super) fn host(&mut self) -> AbiRecordingHostV1 {
        AbiRecordingHostV1 {
            context: std::ptr::from_mut(self).cast(),
            start,
            finish,
        }
    }

    pub(super) fn panicked(&self) -> bool {
        self.panicked
    }

    fn start(&mut self, parent: u64, target: &str) -> Result<AbiSpanV1, u32> {
        if target.is_empty() || target.len() > MAX_TARGET_BYTES {
            return Err(ABI_RECORDING_INVALID);
        }
        if self.admitted == MAX_SPANS {
            return Err(ABI_RECORDING_LIMIT);
        }
        let next = self.next.checked_add(1).ok_or(ABI_RECORDING_EXHAUSTED)?;
        let parent = if parent == 0 {
            self.context.as_ref().ok_or(ABI_RECORDING_DISABLED)?
        } else {
            &self
                .spans
                .get(&parent)
                .ok_or(ABI_RECORDING_UNKNOWN_HANDLE)?
                .0
        };
        let context = parent.child();
        let guard = context.in_scope(|| SpanGuard::current(SpanKind::NativeGuest, target));
        if !guard.is_recording() {
            return Err(ABI_RECORDING_DISABLED);
        }
        let span = AbiSpanV1 {
            handle: *self.next,
            parent: AbiTraceParentV1 {
                trace_id: context.trace_id(),
                span_id: context.span_id(),
            },
        };
        self.spans.insert(span.handle, (context, guard));
        *self.next = next;
        self.admitted += 1;
        Ok(span)
    }

    fn finish(&mut self, handle: u64, status: u32) -> u32 {
        let status = match status {
            ABI_SPAN_OK => SpanStatus::Ok,
            ABI_SPAN_ERROR => SpanStatus::Error,
            ABI_SPAN_CANCELLED => SpanStatus::Cancelled,
            ABI_SPAN_PANICKED => SpanStatus::Panicked,
            _ => return ABI_RECORDING_INVALID,
        };
        let Some((_, guard)) = self.spans.remove(&handle) else {
            return ABI_RECORDING_UNKNOWN_HANDLE;
        };
        guard.finish(status);
        ABI_RECORDING_OK
    }

    fn boundary(&mut self, callback: impl FnOnce(&mut Self) -> u32) -> u32 {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(self))) {
            Ok(code) => code,
            Err(payload) => {
                self.panicked = true;
                if let Err(secondary) =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
                {
                    std::mem::forget(secondary);
                }
                ABI_RECORDING_PANIC
            }
        }
    }

    pub(super) fn complete(mut self, result: &mut Result<(), (u32, String)>) {
        if self.panicked {
            *result = Err((ABI_ERR_PANIC, "native recording callback panicked".into()));
        }
        let status = match result {
            Ok(()) => SpanStatus::Cancelled,
            Err((ABI_ERR_PANIC, _)) => SpanStatus::Panicked,
            Err(_) => SpanStatus::Error,
        };
        for (_, guard) in std::mem::take(&mut self.spans).into_values() {
            guard.finish(status);
        }
    }
}

unsafe extern "C" fn start(
    context: *mut (),
    parent: u64,
    target: AbiStr,
    out: *mut AbiSpanV1,
) -> u32 {
    if context.is_null()
        || out.is_null()
        || target.ptr.is_null()
        || target.len == 0
        || target.len as usize > MAX_TARGET_BYTES
    {
        return ABI_RECORDING_INVALID;
    }
    // The native ABI requires same-thread callbacks and live disjoint buffers.
    let session = unsafe { &mut *context.cast::<Session<'_>>() };
    session.boundary(|session| {
        let bytes = unsafe { std::slice::from_raw_parts(target.ptr, target.len as usize) };
        let Ok(target) = std::str::from_utf8(bytes) else {
            return ABI_RECORDING_INVALID;
        };
        match session.start(parent, target) {
            Ok(span) => {
                unsafe {
                    out.write(span);
                }
                ABI_RECORDING_OK
            }
            Err(code) => code,
        }
    })
}

unsafe extern "C" fn finish(context: *mut (), handle: u64, status: u32) -> u32 {
    if context.is_null() {
        return ABI_RECORDING_INVALID;
    }
    let session = unsafe { &mut *context.cast::<Session<'_>>() };
    session.boundary(|session| session.finish(handle, status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parrot_api::trace::TraceRecorder;
    use std::{path::PathBuf, sync::OnceLock};

    mod fixture_build {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../parrot-abi/tests/common/build.rs"
        ));
    }

    fn fixture() -> &'static PathBuf {
        static PATH: OnceLock<PathBuf> = OnceLock::new();
        PATH.get_or_init(|| {
            let root =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../parrot-abi/tests/fixtures");
            let output = fixture_build::cargo()
                .args(["build", "--release", "--locked", "--target-dir"])
                .arg(root.join("target"))
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            root.join("target/release").join(format!(
                "libparrot_abi_recordingcomp.{}",
                std::env::consts::DLL_EXTENSION
            ))
        })
    }

    struct Guest {
        library: Option<DylibHandle>,
        component: *mut AbiComponent,
        next: u64,
    }
    impl Guest {
        fn new() -> Self {
            // SAFETY: this test builds and owns the non-unwinding fixture.
            let library = unsafe { DylibLoader::load(fixture(), "") }.unwrap();
            assert!(library.supports_recording_v1());
            let component = DylibLoader.construct(&library, "[config]\n").unwrap();
            Self {
                library: Some(library),
                component,
                next: 1,
            }
        }
        fn call(
            &mut self,
            key: &str,
            payload: &[u8],
            context: Option<TraceContext>,
        ) -> Result<Vec<u8>, (u32, String)> {
            let parent = context.as_ref().map(|c| AbiTraceParentV1 {
                trace_id: c.trace_id(),
                span_id: c.span_id(),
            });
            let mut session = Session::new(context, &mut self.next);
            let host = session.host();
            let mut bytes = [0; 128];
            let mut out = AbiReplyBuf::new(&mut bytes);
            // SAFETY: exclusive live instance and synchronous disjoint borrowed buffers.
            let mut result = unsafe {
                DylibLoader.handle_msg_with_recording(
                    self.library.as_ref().unwrap(),
                    self.component,
                    AbiMsg::new(key, payload),
                    &mut out,
                    parent.as_ref(),
                    Some(&host),
                )
            };
            if result.is_ok() && out.written as usize > bytes.len() {
                result = Err((ABI_ERR_OVERFLOW, "invalid reply length".into()));
            }
            session.complete(&mut result);
            result.map(|()| bytes[..out.written as usize].to_vec())
        }
    }
    impl Drop for Guest {
        fn drop(&mut self) {
            DylibLoader
                .unload_blocking(self.library.take().unwrap())
                .unwrap();
        }
    }

    #[test]
    fn actual_library_records_nested_children_and_preserves_returned_identities() {
        let mut guest = Guest::new();
        let recorder = TraceRecorder::new(8).unwrap();
        let root = recorder.root();
        let bytes = guest.call("nested", b"", Some(root.clone())).unwrap();
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.started, 2);
        assert_eq!(snapshot.counts.in_flight, 0);
        let inner = &snapshot.records[0];
        let outer = &snapshot.records[1];
        assert_eq!(inner.parent_span_id, Some(outer.span_id));
        assert_eq!(outer.parent_span_id, Some(root.span_id()));
        assert_eq!(
            bytes,
            [root.trace_id().as_slice(), &outer.span_id, &inner.span_id].concat()
        );
        assert_ne!(inner.span_id, outer.span_id);
        assert_ne!(outer.span_id, root.span_id());
        for span in &snapshot.records {
            assert_eq!(span.trace_id, root.trace_id());
            assert_eq!(span.kind, SpanKind::NativeGuest);
            assert_eq!(span.status, SpanStatus::Ok);
        }
    }

    #[test]
    fn absent_identity_only_and_closed_recorders_do_not_admit_guest_records() {
        let mut guest = Guest::new();
        let recorder = TraceRecorder::new(8).unwrap();
        let root = recorder.root();
        root.in_scope(|| guest.call("disabled", b"", None)).unwrap();
        guest
            .call("disabled", b"", Some(TraceContext::root()))
            .unwrap();
        assert_eq!(recorder.snapshot().counts.started, 0);
        recorder.close();
        guest.call("disabled", b"", Some(root)).unwrap();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.started, 0);
        assert_eq!(snapshot.counts.rejected_after_close, 1);
    }

    #[test]
    fn completed_and_live_admission_budgets_are_independent_of_recorder_capacity() {
        let mut guest = Guest::new();
        let recorder = TraceRecorder::new(1024).unwrap();
        guest.call("budget", b"", Some(recorder.root())).unwrap();
        guest
            .call("active-budget", b"", Some(recorder.root()))
            .unwrap();
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.started, 512);
        assert_eq!(snapshot.counts.completed, 512);
        assert_eq!(snapshot.counts.ok, 256);
        assert_eq!(snapshot.counts.cancelled, 256);
        assert_eq!(snapshot.counts.in_flight, 0);
        assert_eq!(snapshot.counts.overwritten, 0);
        guest.call("nested", b"", Some(recorder.root())).unwrap();
        assert_eq!(recorder.snapshot().counts.completed, 514);
    }

    #[test]
    fn stale_handles_cannot_finish_or_parent_another_invocations_work() {
        let mut guest = Guest::new();
        let recorder = TraceRecorder::new(8).unwrap();
        let first = guest
            .call("unfinished", b"", Some(recorder.root()))
            .unwrap();
        guest.call("disabled", b"", None).unwrap();
        let second = guest.call("stale", &first, Some(recorder.root())).unwrap();
        assert!(
            u64::from_le_bytes(second.try_into().unwrap())
                > u64::from_le_bytes(first.try_into().unwrap())
        );
        let snapshot = recorder.snapshot();
        assert_eq!(snapshot.counts.cancelled, 1);
        assert_eq!(snapshot.counts.ok, 1);
        assert_eq!(snapshot.counts.in_flight, 0);
    }

    #[test]
    fn guest_error_panic_and_invalid_reply_retire_with_the_actual_outcome() {
        let mut guest = Guest::new();
        let recorder = TraceRecorder::new(8).unwrap();
        for (key, code, status) in [
            ("error", ABI_ERR_STATE, SpanStatus::Error),
            ("panic", ABI_ERR_PANIC, SpanStatus::Panicked),
            ("invalid-length", ABI_ERR_OVERFLOW, SpanStatus::Error),
        ] {
            assert_eq!(
                guest.call(key, b"", Some(recorder.root())).unwrap_err().0,
                code
            );
            assert_eq!(recorder.snapshot().records.last().unwrap().status, status);
            assert_eq!(recorder.snapshot().counts.in_flight, 0);
        }
        guest.call("nested", b"", Some(recorder.root())).unwrap();
        assert_eq!(recorder.snapshot().counts.completed, 5);
    }

    #[test]
    fn invalid_callback_views_do_not_admit_work_and_explicit_statuses_survive_ffi() {
        let mut guest = Guest::new();
        let recorder = TraceRecorder::new(8).unwrap();
        guest.call("invalid", b"", Some(recorder.root())).unwrap();
        assert_eq!(recorder.snapshot().counts.started, 0);
        guest.call("statuses", b"", Some(recorder.root())).unwrap();
        let snapshot = recorder.snapshot();
        assert_eq!(
            snapshot
                .records
                .iter()
                .map(|s| s.status)
                .collect::<Vec<_>>(),
            [
                SpanStatus::Ok,
                SpanStatus::Error,
                SpanStatus::Cancelled,
                SpanStatus::Panicked
            ]
        );
        assert!(snapshot
            .records
            .iter()
            .all(|s| s.target.len() == 256 && !s.target_truncated));
        assert_eq!(snapshot.counts.in_flight, 0);
    }

    #[test]
    fn exhausted_handles_and_callback_panics_do_not_escape_or_leak_owners() {
        let recorder = TraceRecorder::new(8).unwrap();
        let mut next = u64::MAX;
        let mut session = Session::new(Some(recorder.root()), &mut next);
        assert_eq!(
            session.start(0, "exhausted").unwrap_err(),
            ABI_RECORDING_EXHAUSTED
        );
        assert_eq!(*session.next, u64::MAX);
        *session.next = 1;
        session.start(0, "owned").unwrap();
        struct Payload;
        impl Drop for Payload {
            fn drop(&mut self) {
                panic!("host payload drop");
            }
        }
        assert_eq!(
            session.boundary(|_| std::panic::panic_any(Payload)),
            ABI_RECORDING_PANIC
        );
        let mut result = Ok(());
        session.complete(&mut result);
        assert_eq!(result.unwrap_err().0, ABI_ERR_PANIC);
        recorder.close();
        let snapshot = recorder.snapshot();
        assert!(snapshot.terminal);
        assert_eq!(snapshot.counts.panicked, 1);
    }
}
