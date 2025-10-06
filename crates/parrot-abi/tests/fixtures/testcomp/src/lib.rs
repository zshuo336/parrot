//! 规范 dylib fixture（DEV_09 §3.4 / BD-2）：
//! - 导出 `PARROT_ABI_META`（abi_version=1）
//! - handle_msg：echo（回 payload 原样）/ up（u64 LE + 1）/ err（状态错误码）/
//!   panic（宿主 catch_unwind 路径验证）/ cnt（构造计数 u64 LE）
//! - 无 TLS 析构 / 无线程 / 无 signal——禁止清单合规
//! - construct 计数静态（卸载重建验证无残留的对照面：进程内 counter
//!   *会* 保留——dylib dlclose 后计数归零由 unload 测试断言）

use parrot_abi::{
    AbiComponent, AbiHooks, AbiMeta, AbiMsg, AbiReplyBuf, AbiStr, AbiVt, ABI_ERR_OVERFLOW,
    ABI_ERR_PANIC, ABI_ERR_STATE, ABI_OK, PARROT_ABI_VERSION,
};
use std::sync::atomic::{AtomicU64, Ordering};

static CONSTRUCTS: AtomicU64 = AtomicU64::new(0);

/// 组件状态（self_ 指向——消息计数跨调用）。
struct CompState {
    msgs: u64,
}

unsafe extern "C" fn construct(_cfg: AbiStr, out: *mut *mut AbiComponent) -> u32 {
    CONSTRUCTS.fetch_add(1, Ordering::Relaxed);
    let state = Box::into_raw(Box::new(CompState { msgs: 0 }));
    let comp = Box::into_raw(Box::new(AbiComponent {
        vt: &VT,
        self_: state as *mut (),
    }));
    *out = comp;
    ABI_OK
}

unsafe extern "C" fn destroy(comp: *mut AbiComponent) {
    if comp.is_null() {
        return;
    }
    let boxed = Box::from_raw((*comp).self_ as *mut CompState);
    drop(boxed);
    drop(Box::from_raw(comp));
}

unsafe extern "C" fn handle_msg(self_: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    // panic 边界（09 §4.3 双保险的 dylib 侧）：跨界 unwind 是 UB——
    // 库内 catch 后转错误码（宿主侧 catch_unwind 是第二道防线）
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_msg_inner(self_, msg, out)
    }));
    match r {
        Ok(code) => code,
        Err(_) => ABI_ERR_PANIC,
    }
}

fn handle_msg_inner(self_: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    unsafe { handle_msg_raw(self_, msg, out) }
}

unsafe fn handle_msg_raw(self_: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    let state = &mut *(self_ as *mut CompState);
    state.msgs += 1;
    let key = msg.key();
    let payload = msg.bytes();
    let out = &mut *out;
    match key {
        "echo" => {
            if !out.write(payload) {
                return ABI_ERR_OVERFLOW;
            }
            ABI_OK
        }
        "up" => {
            let n: [u8; 8] = payload.try_into().unwrap_or([0; 8]);
            let n = u64::from_le_bytes(n);
            if !out.write(&(n + 1).to_le_bytes()) {
                return ABI_ERR_OVERFLOW;
            }
            ABI_OK
        }
        "err" => ABI_ERR_STATE,
        "panic" => {
            panic!("testcomp deliberate panic");
        }
        "cnt" => {
            let _ = out.write(&CONSTRUCTS.load(Ordering::Relaxed).to_le_bytes());
            ABI_OK
        }
        "msgs" => {
            let _ = out.write(&state.msgs.to_le_bytes());
            ABI_OK
        }
        _ => ABI_ERR_STATE,
    }
}

unsafe extern "C" fn on_drain(_self_: *mut ()) -> u32 {
    ABI_OK
}

static VT: AbiVt = AbiVt {
    handle_msg,
    on_drain,
};

/// 唯一导出（加载器 get(ABI_META_SYMBOL)）。
#[no_mangle]
pub static PARROT_ABI_META: AbiMeta = AbiMeta {
    abi_version: PARROT_ABI_VERSION,
    parrot_min: 1,
    name: b"testcomp\0".as_ptr(),
    name_len: 8,
    hooks: AbiHooks { construct, destroy },
};
