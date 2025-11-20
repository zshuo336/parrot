use parrot_abi::*;
use std::mem::MaybeUninit;

struct State {
    config: toml::Table,
    calls: u64,
    drained: bool,
    lifecycle_handle: u64,
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(config) = self.config.get("config") {
            if config.get("lifecycle").and_then(toml::Value::as_bool) == Some(true) {
                if let Some(path) = config.get("lifecycle_events").and_then(toml::Value::as_str) {
                    use std::io::Write;
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
                        let _ = writeln!(file, "released");
                    }
                }
            }
        }
    }
}
fn guarded(call: impl FnOnce() -> u32) -> u32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)).unwrap_or(ABI_ERR_PANIC)
}
unsafe extern "C" fn construct(config: AbiStr, out: *mut *mut AbiComponent) -> u32 {
    guarded(|| unsafe {
        let state = Box::new(State {
            config: toml::from_str(config.as_str()).unwrap(),
            calls: 0,
            drained: false,
            lifecycle_handle: 0,
        });
        *out = Box::into_raw(Box::new(AbiComponent {
            vt: &VT,
            self_: Box::into_raw(state).cast(),
        }));
        ABI_OK
    })
}
unsafe extern "C" fn legacy(ptr: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    guarded(|| unsafe {
        let state = &mut *ptr.cast::<State>();
        if state.drained {
            return ABI_ERR_STATE;
        }
        state.calls += 1;
        let bytes = match msg.key() {
            "echo" => msg.bytes().to_vec(),
            "calls" => state.calls.to_le_bytes().to_vec(),
            "parent" | "disabled" => Vec::new(),
            _ => return ABI_ERR_STATE,
        };
        if (*out).write(&bytes) {
            ABI_OK
        } else {
            ABI_ERR_OVERFLOW
        }
    })
}
unsafe extern "C" fn traced(
    ptr: *mut (),
    msg: AbiMsg,
    out: *mut AbiReplyBuf,
    parent: *const AbiTraceParentV1,
) -> u32 {
    guarded(|| unsafe {
        if msg.key() != "parent" {
            return legacy(ptr, msg, out);
        }
        let state = &mut *ptr.cast::<State>();
        if state.drained {
            return ABI_ERR_STATE;
        }
        state.calls += 1;
        let bytes = parent.as_ref().map_or_else(Vec::new, |parent| {
            [parent.trace_id.as_slice(), &parent.span_id].concat()
        });
        if (*out).write(&bytes) {
            ABI_OK
        } else {
            ABI_ERR_OVERFLOW
        }
    })
}
unsafe fn begin(host: &AbiRecordingHostV1, parent: u64, target: &str) -> Result<AbiSpanV1, u32> {
    let mut out = MaybeUninit::uninit();
    let code = unsafe {
        (host.start)(
            host.context,
            parent,
            AbiStr::of_str(target),
            out.as_mut_ptr(),
        )
    };
    if code == ABI_RECORDING_OK {
        Ok(unsafe { out.assume_init() })
    } else {
        Err(code)
    }
}
unsafe fn end(host: &AbiRecordingHostV1, handle: u64, status: u32) -> u32 {
    unsafe { (host.finish)(host.context, handle, status) }
}
unsafe extern "C" fn recorded(
    ptr: *mut (),
    msg: AbiMsg,
    out: *mut AbiReplyBuf,
    parent: *const AbiTraceParentV1,
    host: *const AbiRecordingHostV1,
) -> u32 {
    guarded(|| unsafe {
        let state = &mut *ptr.cast::<State>();
        if state.drained {
            return ABI_ERR_STATE;
        }
        let key = msg.key();
        if key == "lifecycle-stale" {
            return lifecycle_phase(
                &state.config,
                "message",
                &mut state.lifecycle_handle,
                parent,
                host,
            );
        }
        if matches!(key, "parent" | "echo" | "calls") {
            return traced(ptr, msg, out, parent);
        }
        state.calls += 1;
        let Some(host) = host.as_ref() else {
            return if key == "disabled" {
                ABI_OK
            } else {
                ABI_ERR_STATE
            };
        };
        if key == "disabled" {
            assert_eq!(
                begin(host, 0, "disabled").unwrap_err(),
                ABI_RECORDING_DISABLED
            );
            return ABI_OK;
        }
        if key == "invalid" {
            let mut slot = MaybeUninit::<AbiSpanV1>::uninit();
            assert_eq!(
                (host.start)(
                    host.context,
                    0,
                    AbiStr::of_str("valid"),
                    std::ptr::null_mut()
                ),
                ABI_RECORDING_INVALID
            );
            assert_eq!(
                (host.start)(
                    std::ptr::null_mut(),
                    0,
                    AbiStr::of_str("valid"),
                    slot.as_mut_ptr()
                ),
                ABI_RECORDING_INVALID
            );
            assert_eq!(
                (host.start)(
                    host.context,
                    0,
                    AbiStr {
                        ptr: std::ptr::null(),
                        len: 1
                    },
                    slot.as_mut_ptr()
                ),
                ABI_RECORDING_INVALID
            );
            assert_eq!(
                (host.start)(
                    host.context,
                    0,
                    AbiStr {
                        ptr: b"x".as_ptr(),
                        len: 257
                    },
                    slot.as_mut_ptr()
                ),
                ABI_RECORDING_INVALID
            );
            assert_eq!(
                (host.start)(
                    host.context,
                    0,
                    AbiStr {
                        ptr: b"\xff".as_ptr(),
                        len: 1
                    },
                    slot.as_mut_ptr()
                ),
                ABI_RECORDING_INVALID
            );
            assert_eq!(begin(host, 0, "").unwrap_err(), ABI_RECORDING_INVALID);
            assert_eq!(end(host, 0, ABI_SPAN_OK), ABI_RECORDING_UNKNOWN_HANDLE);
            return ABI_OK;
        }
        if matches!(key, "budget" | "active-budget") {
            for _ in 0..256 {
                let span = begin(host, 0, "native/bounded").unwrap();
                if key == "budget" {
                    assert_eq!(end(host, span.handle, ABI_SPAN_OK), ABI_RECORDING_OK);
                }
            }
            assert_eq!(begin(host, 0, "too-many").unwrap_err(), ABI_RECORDING_LIMIT);
            return ABI_OK;
        }
        if key == "statuses" {
            for status in [
                ABI_SPAN_OK,
                ABI_SPAN_ERROR,
                ABI_SPAN_CANCELLED,
                ABI_SPAN_PANICKED,
            ] {
                let span = begin(host, 0, &"x".repeat(256)).unwrap();
                assert_eq!(end(host, span.handle, u32::MAX), ABI_RECORDING_INVALID);
                assert_eq!(end(host, span.handle, status), ABI_RECORDING_OK);
                assert_eq!(
                    begin(host, span.handle, "retired-parent").unwrap_err(),
                    ABI_RECORDING_UNKNOWN_HANDLE
                );
            }
            return ABI_OK;
        }
        let outer = begin(host, 0, "native/outer").unwrap();
        let mut bytes = outer.handle.to_le_bytes().to_vec();
        match key {
            "nested" => {
                let inner = begin(host, outer.handle, "native/inner").unwrap();
                assert_eq!(end(host, inner.handle, ABI_SPAN_OK), ABI_RECORDING_OK);
                assert_eq!(
                    end(host, inner.handle, ABI_SPAN_OK),
                    ABI_RECORDING_UNKNOWN_HANDLE
                );
                assert_eq!(end(host, outer.handle, ABI_SPAN_OK), ABI_RECORDING_OK);
                bytes = [
                    outer.parent.trace_id.as_slice(),
                    &outer.parent.span_id,
                    &inner.parent.span_id,
                ]
                .concat();
            }
            "stale" => {
                let previous = u64::from_le_bytes(msg.bytes().try_into().unwrap());
                assert!(outer.handle > previous);
                assert_eq!(
                    end(host, previous, ABI_SPAN_OK),
                    ABI_RECORDING_UNKNOWN_HANDLE
                );
                assert_eq!(
                    begin(host, previous, "stale-parent").unwrap_err(),
                    ABI_RECORDING_UNKNOWN_HANDLE
                );
                assert_eq!(end(host, outer.handle, ABI_SPAN_OK), ABI_RECORDING_OK);
            }
            "error" => return ABI_ERR_STATE,
            "panic" => panic!("native guest recording panic"),
            "invalid-length" => {
                (*out).written = (*out).cap + 1;
                return ABI_OK;
            }
            "gate" => {
                let config = state.config.get("config").unwrap();
                std::fs::write(config["entered"].as_str().unwrap(), b"entered").unwrap();
                let release = std::path::Path::new(config["release"].as_str().unwrap());
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                while !release.exists() {
                    assert!(std::time::Instant::now() < deadline, "native gate timeout");
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                assert_eq!(end(host, outer.handle, ABI_SPAN_OK), ABI_RECORDING_OK);
            }
            "unfinished" => {}
            _ => return ABI_ERR_STATE,
        }
        if (*out).write(&bytes) {
            ABI_OK
        } else {
            ABI_ERR_OVERFLOW
        }
    })
}
unsafe extern "C" fn drain(ptr: *mut ()) -> u32 {
    unsafe {
        (*ptr.cast::<State>()).drained = true;
    }
    ABI_OK
}
unsafe extern "C" fn destroy(component: *mut AbiComponent) {
    unsafe {
        let component = Box::from_raw(component);
        drop(Box::from_raw(component.self_.cast::<State>()));
    }
}
static VT: AbiVt = AbiVt {
    handle_msg: legacy,
    on_drain: drain,
};
#[no_mangle]
pub static PARROT_ABI_META: AbiMeta = AbiMeta {
    abi_version: PARROT_ABI_VERSION,
    parrot_min: 1,
    name: b"recordingcomp".as_ptr(),
    name_len: 13,
    hooks: AbiHooks { construct, destroy },
};
#[no_mangle]
pub static PARROT_ABI_TRACE_V1: AbiTraceV1 = AbiTraceV1 { handle_msg: traced };
#[no_mangle]
pub static PARROT_ABI_RECORDING_V1: AbiRecordingV1 = AbiRecordingV1 {
    handle_msg: recorded,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

unsafe fn lifecycle_phase(
    config: &toml::Table,
    phase: &str,
    previous: &mut u64,
    parent: *const AbiTraceParentV1,
    host: *const AbiRecordingHostV1,
) -> u32 {
    let Some(config) = config.get("config").and_then(toml::Value::as_table) else {
        return ABI_OK;
    };
    if config.get("lifecycle").and_then(toml::Value::as_bool) != Some(true) {
        return ABI_OK;
    }
    let identity = parent.as_ref().map_or_else(
        || "none".into(),
        |parent| hex(&[parent.trace_id.as_slice(), &parent.span_id].concat()),
    );
    let mut code = ABI_RECORDING_DISABLED;
    let mut handle = 0;
    let mut stale = ABI_RECORDING_UNKNOWN_HANDLE;
    if let Some(host) = host.as_ref() {
        if *previous != 0 {
            stale = end(host, *previous, ABI_SPAN_OK);
            assert_eq!(stale, ABI_RECORDING_UNKNOWN_HANDLE);
            assert_eq!(
                begin(host, *previous, "stale-lifecycle-parent").unwrap_err(),
                stale
            );
        }
        match begin(host, 0, &format!("guest/{phase}")) {
            Ok(span) => {
                code = ABI_RECORDING_OK;
                handle = span.handle;
                assert!(handle > *previous);
                *previous = handle;
                let nested = begin(host, handle, &format!("guest/{phase}/nested")).unwrap();
                assert_eq!(end(host, nested.handle, ABI_SPAN_OK), ABI_RECORDING_OK);
                assert_eq!(end(host, handle, ABI_SPAN_OK), ABI_RECORDING_OK);
                // The host must retire this one according to the actual result.
                begin(host, 0, &format!("guest/{phase}/unfinished")).unwrap();
                if config
                    .get("lifecycle_budget")
                    .and_then(toml::Value::as_bool)
                    == Some(true)
                {
                    for _ in 3..256 {
                        let span = begin(host, 0, &format!("guest/{phase}/budget")).unwrap();
                        assert_eq!(end(host, span.handle, ABI_SPAN_OK), ABI_RECORDING_OK);
                    }
                    assert_eq!(
                        begin(host, 0, "over-lifecycle-budget").unwrap_err(),
                        ABI_RECORDING_LIMIT
                    );
                }
            }
            Err(result) => {
                code = result;
                assert_eq!(result, ABI_RECORDING_DISABLED);
            }
        }
    }
    if let Some(path) = config.get("lifecycle_events").and_then(toml::Value::as_str) {
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap(),
            "{phase}|{identity}|{code}|{handle}|{stale}"
        )
        .unwrap();
    }
    if let Some(path) = config
        .get(&format!("{phase}_entered"))
        .and_then(toml::Value::as_str)
    {
        std::fs::write(path, b"entered").unwrap();
    }
    if let Some(path) = config
        .get(&format!("{phase}_release"))
        .and_then(toml::Value::as_str)
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !std::path::Path::new(path).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "guest lifecycle gate timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    if config
        .get(&format!("{phase}_panic"))
        .and_then(toml::Value::as_bool)
        == Some(true)
    {
        panic!("caught guest {phase} panic");
    }
    if config
        .get(&format!("{phase}_error"))
        .and_then(toml::Value::as_bool)
        == Some(true)
    {
        ABI_ERR_STATE
    } else {
        ABI_OK
    }
}

unsafe extern "C" fn lifecycle_construct(
    config: AbiStr,
    out: *mut *mut AbiComponent,
    parent: *const AbiTraceParentV1,
    host: *const AbiRecordingHostV1,
) -> u32 {
    guarded(|| unsafe {
        let config_table = toml::from_str(config.as_str()).unwrap();
        let mut previous = 0;
        let code = lifecycle_phase(&config_table, "construct", &mut previous, parent, host);
        if code != ABI_OK {
            return code;
        }
        let code = construct(config, out);
        if code == ABI_OK {
            (*(*(*out)).self_.cast::<State>()).lifecycle_handle = previous;
        }
        code
    })
}

unsafe extern "C" fn lifecycle_drain(
    ptr: *mut (),
    parent: *const AbiTraceParentV1,
    host: *const AbiRecordingHostV1,
) -> u32 {
    guarded(|| unsafe {
        let state = &mut *ptr.cast::<State>();
        state.drained = true;
        lifecycle_phase(
            &state.config,
            "drain",
            &mut state.lifecycle_handle,
            parent,
            host,
        )
    })
}

unsafe extern "C" fn lifecycle_destroy(
    component: *mut AbiComponent,
    parent: *const AbiTraceParentV1,
    host: *const AbiRecordingHostV1,
) -> u32 {
    let result = guarded(|| unsafe {
        let state = &mut *(*component).self_.cast::<State>();
        assert!(state.drained, "all drains finish before destruction");
        lifecycle_phase(
            &state.config,
            "destroy",
            &mut state.lifecycle_handle,
            parent,
            host,
        )
    });
    // Release the instance even after a caught diagnostic panic or error.
    unsafe {
        destroy(component);
    }
    result
}

#[no_mangle]
pub static PARROT_ABI_LIFECYCLE_V1: AbiLifecycleV1 = AbiLifecycleV1 {
    construct: lifecycle_construct,
    on_drain: lifecycle_drain,
    destroy: lifecycle_destroy,
};
