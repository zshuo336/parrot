use parrot_abi::*;

struct State {
    destination: String,
    request: String,
    expected: String,
    service_key: String,
    drained: bool,
}
fn guarded(call: impl FnOnce() -> u32) -> u32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)).unwrap_or(ABI_ERR_PANIC)
}
unsafe extern "C" fn construct(config: AbiStr, out: *mut *mut AbiComponent) -> u32 {
    guarded(|| unsafe {
        let table: toml::Table = toml::from_str(config.as_str()).unwrap();
        let cfg = table["config"].as_table().unwrap();
        let string = |key: &str, default: &str| {
            cfg.get(key)
                .and_then(toml::Value::as_str)
                .unwrap_or(default)
                .to_string()
        };
        let state = State {
            destination: string("destination", "/user/target"),
            request: string("request", "bin:native.request#v1"),
            expected: string("expected", "bin:native.reply#v1"),
            service_key: string("service_key", "native/business"),
            drained: false,
        };
        *out = Box::into_raw(Box::new(AbiComponent {
            vt: &VT,
            self_: Box::into_raw(Box::new(state)).cast(),
        }));
        ABI_OK
    })
}
unsafe extern "C" fn legacy(ptr: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    guarded(|| unsafe {
        if (*ptr.cast::<State>()).drained {
            return ABI_ERR_STATE;
        }
        if (*out).write(msg.bytes()) {
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
    _: *const AbiTraceParentV1,
) -> u32 {
    unsafe { legacy(ptr, msg, out) }
}
unsafe extern "C" fn recorded(
    ptr: *mut (),
    msg: AbiMsg,
    out: *mut AbiReplyBuf,
    parent: *const AbiTraceParentV1,
    _: *const AbiRecordingHostV1,
) -> u32 {
    unsafe { traced(ptr, msg, out, parent) }
}
unsafe extern "C" fn messaging(
    ptr: *mut (),
    msg: AbiMsg,
    out: *mut AbiReplyBuf,
    parent: *const AbiTraceParentV1,
    recording: *const AbiRecordingHostV1,
    host: *const AbiMessagingHostV1,
) -> u32 {
    guarded(|| unsafe {
        let state = &mut *ptr.cast::<State>();
        if state.drained {
            return ABI_ERR_STATE;
        }
        if msg.key() == "echo" {
            return recorded(ptr, msg, out, parent, recording);
        }
        let Some(host) = host.as_ref() else {
            return ABI_ERR_STATE;
        };
        let key = msg.key();
        let mut bytes = vec![0; 4 * 1024 * 1024];
        let mut reply = AbiReplyBuf::new(&mut bytes);
        if key == "small" {
            reply.cap = 1;
        }
        let message = AbiMsg::new(&state.request, msg.bytes());
        let destination = AbiStr::of_str(&state.destination);
        let timeout = if key == "timeout" { 20 } else { 2000 };
        let count = match key {
            "budget" => 65,
            "byte-budget" => 5,
            _ => 1,
        };
        let mut result = Vec::new();
        let mut span = std::mem::MaybeUninit::<AbiSpanV1>::uninit();
        let span = recording.as_ref().and_then(|recording| {
            ((recording.start)(
                recording.context,
                0,
                AbiStr::of_str("native-message"),
                span.as_mut_ptr(),
            ) == ABI_RECORDING_OK)
                .then(|| span.assume_init())
        });
        for _ in 0..count {
            let code = if key == "invalid" {
                (host.ask)(
                    host.context,
                    AbiStr {
                        ptr: std::ptr::null(),
                        len: 1,
                    },
                    message,
                    AbiStr::of_str(&state.expected),
                    &mut reply,
                    timeout,
                )
            } else if matches!(key, "tell" | "budget" | "byte-budget") {
                (host.tell)(host.context, destination, message, timeout)
            } else {
                (host.ask)(
                    host.context,
                    destination,
                    message,
                    AbiStr::of_str(&state.expected),
                    &mut reply,
                    timeout,
                )
            };
            result.extend_from_slice(&code.to_le_bytes());
            if !matches!(key, "budget" | "byte-budget") && code == ABI_MESSAGE_OK {
                result.extend_from_slice(reply.written_slice());
            }
        }
        if let (Some(recording), Some(span)) = (recording.as_ref(), span) {
            assert_eq!(
                (recording.finish)(recording.context, span.handle, ABI_SPAN_OK),
                ABI_RECORDING_OK
            );
        }
        if (*out).write(&result) {
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
unsafe extern "C" fn services(
    ptr: *mut (),
    msg: AbiMsg,
    out: *mut AbiReplyBuf,
    parent: *const AbiTraceParentV1,
    recording: *const AbiRecordingHostV1,
    messages: *const AbiMessagingHostV1,
    host: *const AbiServicesHostV1,
) -> u32 {
    guarded(|| unsafe {
        let command = msg.key();
        if command == "service:fail" {
            return ABI_ERR_STATE;
        }
        if !command.starts_with("service:") {
            return messaging(ptr, msg, out, parent, recording, messages);
        }
        let state = &*ptr.cast::<State>();
        if state.drained {
            return ABI_ERR_STATE;
        }
        let Some(host) = host.as_ref() else {
            return ABI_ERR_STATE;
        };
        let key = AbiStr::of_str(&state.service_key);
        let handle = || u64::from_le_bytes(msg.bytes().try_into().unwrap());
        let mut bytes = [0; 4096];
        let mut path = AbiReplyBuf::new(&mut bytes);
        let mut kind = 0;
        let mut result = Vec::new();
        let code = match command {
            "service:register" => (host.register)(host.context, key, 2000),
            "service:deregister" => (host.deregister)(host.context, key, 2000),
            "service:subscribe" => {
                let mut handle = 99;
                let code = (host.subscribe)(host.context, key, 2000, &mut handle);
                result.extend_from_slice(&handle.to_le_bytes());
                code
            }
            "service:next" | "service:poll" | "service:small" => {
                if command == "service:small" {
                    path.cap = 1;
                }
                let code = (host.next)(
                    host.context,
                    handle(),
                    if command == "service:poll" { 5 } else { 2000 },
                    &mut path,
                    &mut kind,
                );
                result.extend_from_slice(&kind.to_le_bytes());
                result.extend_from_slice(path.written_slice());
                code
            }
            "service:close" => (host.close)(host.context, handle()),
            "service:invalid" => {
                let invalid = AbiStr {
                    ptr: std::ptr::null(),
                    len: 1,
                };
                for _ in 0..65 {
                    result.extend_from_slice(
                        &(host.register)(host.context, invalid, 2000).to_le_bytes(),
                    );
                }
                ABI_SERVICE_OK
            }
            "service:mixed" => {
                for _ in 0..63 {
                    result.extend_from_slice(&(host.register)(host.context, key, 0).to_le_bytes());
                }
                let messages = messages.as_ref().unwrap();
                for _ in 0..2 {
                    result.extend_from_slice(
                        &(messages.tell)(
                            messages.context,
                            AbiStr::of_str(&state.destination),
                            AbiMsg::new(&state.request, msg.bytes()),
                            2000,
                        )
                        .to_le_bytes(),
                    );
                }
                ABI_SERVICE_OK
            }
            "service:discover" => {
                let mut handle = 0;
                let code = (host.subscribe)(host.context, key, 2000, &mut handle);
                if code != ABI_SERVICE_OK {
                    result.extend_from_slice(&code.to_le_bytes());
                } else {
                    let code = (host.next)(host.context, handle, 2000, &mut path, &mut kind);
                    assert_eq!((host.close)(host.context, handle), ABI_SERVICE_OK);
                    if code != ABI_SERVICE_OK {
                        result.extend_from_slice(&code.to_le_bytes());
                    } else {
                        assert_eq!(kind, ABI_SERVICE_REGISTERED);
                        let messages = messages.as_ref().unwrap();
                        let destination = std::str::from_utf8(path.written_slice()).unwrap();
                        let mut data = vec![0; 4 * 1024 * 1024];
                        let mut reply = AbiReplyBuf::new(&mut data);
                        let code = (messages.ask)(
                            messages.context,
                            AbiStr::of_str(destination),
                            AbiMsg::new(&state.request, msg.bytes()),
                            AbiStr::of_str(&state.expected),
                            &mut reply,
                            2000,
                        );
                        result.extend_from_slice(&code.to_le_bytes());
                        result.extend_from_slice(reply.written_slice());
                    }
                }
                ABI_SERVICE_OK
            }
            _ => ABI_SERVICE_INVALID,
        };
        let mut output = code.to_le_bytes().to_vec();
        output.extend(result);
        if (*out).write(&output) {
            ABI_OK
        } else {
            ABI_ERR_OVERFLOW
        }
    })
}
unsafe extern "C" fn destroy(ptr: *mut AbiComponent) {
    unsafe {
        let component = Box::from_raw(ptr);
        drop(Box::from_raw(component.self_.cast::<State>()));
    }
}
static VT: AbiVt = AbiVt {
    handle_msg: legacy,
    on_drain: drain,
};
#[no_mangle]
pub static PARROT_ABI_META: AbiMeta = AbiMeta {
    abi_version: 1,
    parrot_min: 1,
    name: b"messaging".as_ptr(),
    name_len: 9,
    hooks: AbiHooks { construct, destroy },
};
#[no_mangle]
pub static PARROT_ABI_TRACE_V1: AbiTraceV1 = AbiTraceV1 { handle_msg: traced };
#[no_mangle]
pub static PARROT_ABI_RECORDING_V1: AbiRecordingV1 = AbiRecordingV1 {
    handle_msg: recorded,
};
#[no_mangle]
pub static PARROT_ABI_MESSAGING_V1: AbiMessagingV1 = AbiMessagingV1 {
    handle_msg: messaging,
};
#[no_mangle]
pub static PARROT_ABI_SERVICES_V1: AbiServicesV1 = AbiServicesV1 {
    handle_msg: services,
};
