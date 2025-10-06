//! crawler-lab parrot 组件（cdylib 形态——R2 双形态之一）。
//!
//! R1/R2（应用体系架构纠正）：本 crate 是 apps/crawler-lab 的业务代码。
//! 构建产物 libcrawler_lab_dylib.dylib 经 parrot 标准包分发：
//! crawler.app.toml 声明 `artifact = { Dylib = { uri, digest, abi } }`，
//! parrot 网关 deploy 时经 parrot-abi loader 载入（四步卸载协议管 lifecycle）。
//!
//! 消息契约（与 ThreadActorSystem 版 hub 同键——run_regression golden 锚定）：
//!   "crawl/Tick"    → 编排一跳（驱动 hub 主循环——预留）
//!   "crawl/Stat"    → [handled u64] 处理计数
//!   "crawl/Echo"    → payload 原样回（链路探活）
//!
//! 注：frontier/index/search 的 fan-out 逻辑在 Rust hub（apps/crawler-lab
//! src/main.rs 的编排循环）——本 dylib 承载 hub 状态面（计数/探活），
//! 为 deploy 分发形态的锚点组件。

use parrot_abi::{
    AbiComponent, AbiHooks, AbiMeta, AbiMsg, AbiReplyBuf, AbiStr, AbiVt, ABI_ERR_OVERFLOW,
    ABI_ERR_PANIC, ABI_ERR_STATE, ABI_OK, PARROT_ABI_VERSION,
};
use std::sync::atomic::{AtomicU64, Ordering};

static HANDLED: AtomicU64 = AtomicU64::new(0);

struct HubState {
    msgs: u64,
}

unsafe extern "C" fn construct(_cfg: AbiStr, out: *mut *mut AbiComponent) -> u32 {
    let state = Box::into_raw(Box::new(HubState { msgs: 0 }));
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
    drop(Box::from_raw((*comp).self_ as *mut HubState));
    drop(Box::from_raw(comp));
}

unsafe extern "C" fn handle_msg(self_: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    // 跨界 unwind 是 UB——库内 catch 转错误码（宿主 catch_unwind 第二道防线）
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_inner(self_, msg, out)
    }));
    match r {
        Ok(code) => code,
        Err(_) => ABI_ERR_PANIC,
    }
}

fn handle_inner(self_: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    unsafe { handle_raw(self_, msg, out) }
}

unsafe fn handle_raw(self_: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    let state = &mut *(self_ as *mut HubState);
    state.msgs += 1;
    HANDLED.fetch_add(1, Ordering::Relaxed);
    let key = msg.key();
    let payload = msg.bytes();
    let out = &mut *out;
    match key {
        "crawl/Echo" => {
            if !out.write(payload) {
                return ABI_ERR_OVERFLOW;
            }
            ABI_OK
        }
        "crawl/Stat" => {
            let _ = out.write(&state.msgs.to_le_bytes());
            ABI_OK
        }
        "crawl/Tick" => {
            // 编排一跳（预留——hub 主循环由 Rust 编排进程驱动）
            let _ = out.write(&0u64.to_le_bytes());
            ABI_OK
        }
        _ => ABI_ERR_STATE,
    }
}

unsafe extern "C" fn on_drain(_self_: *mut ()) -> u32 {
    ABI_OK
}

static VT: AbiVt = AbiVt { handle_msg, on_drain };

#[no_mangle]
pub static PARROT_ABI_META: AbiMeta = AbiMeta {
    abi_version: PARROT_ABI_VERSION,
    parrot_min: 1,
    name: b"crawler-hub\0".as_ptr(),
    name_len: 12,
    hooks: AbiHooks { construct, destroy },
};
