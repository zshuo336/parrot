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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static CONSTRUCTS: AtomicU64 = AtomicU64::new(0);
static DRAINS: AtomicU64 = AtomicU64::new(0);
static DESTROYS: AtomicU64 = AtomicU64::new(0);
static ORDER_ERRORS: AtomicU64 = AtomicU64::new(0);
static FIRST_DESTROY_DRAINS: AtomicU64 = AtomicU64::new(0);
static GATE: AtomicBool = AtomicBool::new(true);

#[no_mangle]
pub extern "C" fn lifecycle_observe(index: u32) -> u64 {
    match index {
        0 => DRAINS.load(Ordering::Acquire),
        1 => DESTROYS.load(Ordering::Acquire),
        2 => ORDER_ERRORS.load(Ordering::Acquire),
        _ => FIRST_DESTROY_DRAINS.load(Ordering::Acquire),
    }
}

#[no_mangle]
pub extern "C" fn lifecycle_reset() {
    DRAINS.store(0, Ordering::Release);
    DESTROYS.store(0, Ordering::Release);
    ORDER_ERRORS.store(0, Ordering::Release);
    FIRST_DESTROY_DRAINS.store(0, Ordering::Release);
    GATE.store(false, Ordering::Release);
}

#[no_mangle]
pub extern "C" fn lifecycle_release() {
    GATE.store(true, Ordering::Release);
}

/// 组件状态（self_ 指向——消息计数跨调用）。
struct CompState {
    msgs: u64,
    drained: bool,
    gate: bool,
    fail: bool,
}

unsafe extern "C" fn construct(cfg: AbiStr, out: *mut *mut AbiComponent) -> u32 {
    CONSTRUCTS.fetch_add(1, Ordering::Relaxed);
    let state = Box::into_raw(Box::new(CompState {
        msgs: 0,
        drained: false,
        gate: cfg.as_str() == "gate",
        fail: cfg.as_str() == "error",
    }));
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
    if DESTROYS.load(Ordering::Acquire) == 0 {
        FIRST_DESTROY_DRAINS.store(DRAINS.load(Ordering::Acquire), Ordering::Release);
    }
    let boxed = Box::from_raw((*comp).self_ as *mut CompState);
    if !boxed.drained {
        ORDER_ERRORS.fetch_add(1, Ordering::AcqRel);
    }
    drop(boxed);
    drop(Box::from_raw(comp));
    DESTROYS.fetch_add(1, Ordering::Release);
}

unsafe extern "C" fn handle_msg(self_: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    // extern C unwind会abort；必须在组件内部捕获并转为错误码。
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
            if !out.write(&CONSTRUCTS.load(Ordering::Relaxed).to_le_bytes()) {
                return ABI_ERR_OVERFLOW;
            }
            ABI_OK
        }
        "msgs" => {
            if !out.write(&state.msgs.to_le_bytes()) {
                return ABI_ERR_OVERFLOW;
            }
            ABI_OK
        }
        _ => ABI_ERR_STATE,
    }
}

unsafe extern "C" fn on_drain(self_: *mut ()) -> u32 {
    let state = &mut *(self_ as *mut CompState);
    if state.drained {
        ORDER_ERRORS.fetch_add(1, Ordering::AcqRel);
    }
    DRAINS.fetch_add(1, Ordering::Release);
    while state.gate && !GATE.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
    state.drained = true;
    if state.fail {
        ABI_ERR_STATE
    } else {
        ABI_OK
    }
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
    name: c"testcomp".as_ptr().cast(),
    name_len: 8,
    hooks: AbiHooks { construct, destroy },
};
