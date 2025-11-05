//! Host contract fixture: observable native work, configuration and lifecycle.
//! No borrowed buffers escape a call; no threads, callbacks or TLS are installed.
use parrot_abi::*;
use std::io::Write;
struct State {
    envelope: toml::Table,
    raw: String,
    messages: u64,
    value: u64,
    drained: bool,
    saved_parent: Vec<u8>,
}
impl State {
    fn config(&self, key: &str) -> Option<&str> {
        self.envelope.get("config")?.get(key)?.as_str()
    }
    fn path(&self) -> &str {
        self.envelope["self_path"].as_str().unwrap()
    }
    fn log(&self, event: &str) {
        if let Some(path) = self.config("trace") {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap()
                .write_all(format!("{event}:{}\n", self.path()).as_bytes())
                .unwrap();
        }
    }
    fn gate(&self, key: &str) -> bool {
        let Some(path) = self.config(key) else {
            return true;
        };
        let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !std::path::Path::new(path).exists() {
            if std::time::Instant::now() > until {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        true
    }
}
fn guarded(f: impl FnOnce() -> u32) -> u32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(ABI_ERR_PANIC)
}
unsafe extern "C" fn construct(cfg: AbiStr, out: *mut *mut AbiComponent) -> u32 {
    guarded(|| {
        let raw = unsafe { cfg.as_str() }.to_owned();
        let state = Box::new(State {
            envelope: toml::from_str(&raw).unwrap(),
            raw,
            messages: 0,
            value: 0,
            drained: false,
            saved_parent: Vec::new(),
        });
        state.log("construct-enter");
        if !state.gate("construct_gate") || state.config("fail_path") == Some(state.path()) {
            return ABI_ERR_STATE;
        }
        state.log("construct");
        unsafe {
            *out = Box::into_raw(Box::new(AbiComponent {
                vt: &VT,
                self_: Box::into_raw(state).cast(),
            }));
        }
        ABI_OK
    })
}
unsafe extern "C" fn message(ptr: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    guarded(|| {
        let state = unsafe { &mut *ptr.cast::<State>() };
        state.messages += 1;
        let bytes = match unsafe { msg.key() } {
            "echo" => unsafe { msg.bytes() }.to_vec(),
            "config" => state.raw.as_bytes().to_vec(),
            "messages" => state.messages.to_le_bytes().to_vec(),
            "trace-parent" => Vec::new(), // Legacy hosts provide no context.
            "trace-parent-set" => {
                state.saved_parent.clear();
                Vec::new()
            }
            "saved-parent" => state.saved_parent.clone(),
            "set" => {
                let Ok(bytes) = <[u8; 8]>::try_from(unsafe { msg.bytes() }) else {
                    return ABI_ERR_STATE;
                };
                state.value = u64::from_le_bytes(bytes);
                Vec::new()
            }
            "value" => state.value.to_le_bytes().to_vec(),
            "gate" => {
                state.log("message-enter");
                if !state.gate("message_gate") {
                    return ABI_ERR_STATE;
                }
                state.log("message");
                Vec::new()
            }
            "error" => return ABI_ERR_STATE,
            "invalid-length" => {
                unsafe {
                    (*out).written = (*out).cap + 1;
                }
                return ABI_OK;
            }
            _ => return ABI_ERR_STATE,
        };
        if unsafe { (*out).write(&bytes) } {
            ABI_OK
        } else {
            ABI_ERR_OVERFLOW
        }
    })
}
unsafe extern "C" fn traced_message(
    ptr: *mut (),
    msg: AbiMsg,
    out: *mut AbiReplyBuf,
    parent: *const AbiTraceParentV1,
) -> u32 {
    guarded(|| {
        let key = unsafe { msg.key() };
        if !matches!(key, "trace-parent" | "trace-parent-set" | "trace-gate") {
            return unsafe { message(ptr, msg, out) };
        }
        let mut bytes = Vec::new();
        if let Some(parent) = unsafe { parent.as_ref() } {
            if !parent.is_valid() {
                return ABI_ERR_STATE;
            }
            bytes.extend_from_slice(&parent.trace_id);
            bytes.extend_from_slice(&parent.span_id);
        }
        let state = unsafe { &mut *ptr.cast::<State>() };
        state.messages += 1;
        if key != "trace-parent" {
            state.saved_parent = bytes.clone();
        }
        if key == "trace-gate" {
            state.log("message-enter");
            if !state.gate("message_gate") {
                return ABI_ERR_STATE;
            }
            state.log("message");
        }
        if unsafe { (*out).write(&bytes) } {
            ABI_OK
        } else {
            ABI_ERR_OVERFLOW
        }
    })
}

#[no_mangle]
pub static PARROT_ABI_TRACE_V1: AbiTraceV1 = AbiTraceV1 {
    handle_msg: traced_message,
};

unsafe extern "C" fn drain(ptr: *mut ()) -> u32 {
    guarded(|| {
        let state = unsafe { &mut *ptr.cast::<State>() };
        if state.drained {
            state.log("duplicate-drain");
        }
        state.log("drain-enter");
        if !state.gate("drain_gate") {
            return ABI_ERR_STATE;
        }
        state.drained = true;
        state.log("drain");
        if state.config("drain_error") == Some("yes") {
            ABI_ERR_STATE
        } else {
            ABI_OK
        }
    })
}
unsafe extern "C" fn destroy(ptr: *mut AbiComponent) {
    guarded(|| {
        let component = unsafe { Box::from_raw(ptr) };
        let state = unsafe { Box::from_raw(component.self_.cast::<State>()) };
        if !state.drained {
            state.log("destroy-without-drain");
        }
        state.log("destroy");
        ABI_OK
    });
}
static VT: AbiVt = AbiVt {
    handle_msg: message,
    on_drain: drain,
};
#[no_mangle]
pub static PARROT_ABI_META: AbiMeta = AbiMeta {
    abi_version: PARROT_ABI_VERSION,
    parrot_min: 1,
    name: b"hostcomp".as_ptr(),
    name_len: 8,
    hooks: AbiHooks { construct, destroy },
};
