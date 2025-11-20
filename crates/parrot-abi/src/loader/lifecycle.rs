use super::*;

#[derive(Clone, Copy)]
pub(super) struct LifecycleCall<'a> {
    parent: Option<&'a AbiTraceParentV1>,
    recording: Option<&'a AbiRecordingHostV1>,
}
impl LifecycleCall<'_> {
    pub(super) fn parent_ptr(self) -> *const AbiTraceParentV1 {
        self.parent.map_or(std::ptr::null(), std::ptr::from_ref)
    }
    pub(super) fn recording_ptr(self) -> *const AbiRecordingHostV1 {
        self.recording.map_or(std::ptr::null(), std::ptr::from_ref)
    }
}

impl DylibHandle {
    pub(super) unsafe fn invoke_drain(
        &self,
        comp: *mut AbiComponent,
        call: Option<LifecycleCall<'_>>,
    ) -> u32 {
        if let (Some(extension), Some(call)) = (self.lifecycle_v1, call) {
            (extension.on_drain)((*comp).self_, call.parent_ptr(), call.recording_ptr())
        } else {
            ((*(*comp).vt).on_drain)((*comp).self_)
        }
    }

    pub(super) unsafe fn invoke_destroy(
        &self,
        comp: *mut AbiComponent,
        call: Option<LifecycleCall<'_>>,
    ) -> u32 {
        if let (Some(extension), Some(call)) = (self.lifecycle_v1, call) {
            (extension.destroy)(comp, call.parent_ptr(), call.recording_ptr())
        } else {
            ((*self.meta).hooks.destroy)(comp);
            ABI_OK
        }
    }
}

impl DylibLoader {
    /// Construct through the component-owned lifecycle extension, or fall back
    /// to the original constructor. Invalid parent IDs are rejected before the
    /// native call. No ambient context or recording capability is inferred.
    /// # Safety
    /// The host descriptor and its state must remain live and exclusively
    /// borrowed during synchronous same-thread callbacks, which must not unwind.
    /// All native trust obligations of load apply to the lifecycle descriptor.
    pub unsafe fn construct_with_recording(
        &self,
        h: &DylibHandle,
        cfg: &str,
        parent: Option<&AbiTraceParentV1>,
        recording: Option<&AbiRecordingHostV1>,
    ) -> Result<*mut AbiComponent, LoadError> {
        if parent.is_some_and(|parent| !parent.is_valid()) {
            return Err(LoadError::Construct {
                code: crate::ABI_ERR_STATE,
                detail: "invalid trace parent".into(),
            });
        }
        self.construct_with_lifecycle(h, cfg, Some(LifecycleCall { parent, recording }))
    }

    /// Drain once through the lifecycle extension, or the original vtable.
    /// Cached results never reinvoke the guest, even with different capabilities.
    /// Invalid parent IDs are rejected before changing instance state.
    /// # Safety
    /// The exclusive live-instance requirements of drain_instance and borrowed
    /// same-thread callback requirements of construct_with_recording apply.
    pub unsafe fn drain_instance_with_recording(
        &self,
        h: &DylibHandle,
        comp: *mut AbiComponent,
        parent: Option<&AbiTraceParentV1>,
        recording: Option<&AbiRecordingHostV1>,
    ) -> Result<(), (u32, String)> {
        if parent.is_some_and(|parent| !parent.is_valid()) {
            return Err((crate::ABI_ERR_STATE, "invalid trace parent".into()));
        }
        self.drain_with_lifecycle(h, comp, Some(LifecycleCall { parent, recording }))
    }

    /// Consume the handle, drain all remaining instances before destroying any,
    /// then close the library. Cached drains are reused. The one borrowed
    /// capability covers this synchronous unload invocation, including implicit
    /// drains and all destroys; it is never stored in the handle or used by Drop.
    /// Destroy errors are reported after every instance has been released.
    /// An invalid parent disables extension dispatch, completes legacy cleanup,
    /// then returns InvalidTraceParent (or the actual cleanup failure).
    /// # Safety
    /// The callback and native trust requirements of construct_with_recording
    /// apply. This may block indefinitely and must run on an owned native worker.
    pub unsafe fn unload_blocking_with_recording(
        &self,
        mut h: DylibHandle,
        parent: Option<&AbiTraceParentV1>,
        recording: Option<&AbiRecordingHostV1>,
    ) -> Result<UnloadReport, UnloadError> {
        if parent.is_some_and(|parent| !parent.is_valid()) {
            h.cleanup()?;
            return Err(UnloadError::InvalidTraceParent);
        }
        h.cleanup_with_lifecycle(Some(LifecycleCall { parent, recording }))
    }
}
