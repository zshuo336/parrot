//! 违反禁止清单 fixture（DEV_09 §3.4 施工裁定 BD-2）：
//!
//! `#[used]` static 模拟 TLS 析构注册（链接器保留符号——nm -u 可扫描），
//! **不真制造 UB**：不实际注册 TLS 回调，只让产物携带 `__tlv_atexit`/
//! `pthread_create` 引用形态供 scan_violations 检出。
//!
//! 检出路径：`std::thread` 内部引用 pthread_create；`thread_local!` with
//! destructor 引用 `__tlv_atexit`（macOS）。两者均为静态可扫引用。

use parrot_abi::{AbiComponent, AbiHooks, AbiMeta, AbiMsg, AbiReplyBuf, AbiStr, AbiVt, ABI_OK, PARROT_ABI_VERSION};

thread_local! {
    /// 带析构的 TLS（dtor 引用 __tlv_atexit——禁止清单目标）。
    static MARK: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::new());
}

/// 保引用：链接器不可剥离 TLS dtor 路径（BD-2——仅静态引用形态）。
#[used]
static KEEP_TLS: fn() = touch_tls;

/// 保引用：pthread_create 进入符号表（BD-2——KEEP 保留但宿主永不调用）。
#[used]
static KEEP_THREAD: fn(u32) = touch_thread;

/// 触达带 dtor 的 TLS（迫使 __tlv_atexit 进入未定义符号表）。
fn touch_tls() {
    MARK.with(|m| m.borrow_mut().push(1));
}

/// 引用线程 spawn（pthread_create 未定义引用——扫描目标）。
/// 仅经 KEEP_THREAD 静态引用可达——宿主不会调用。
fn touch_thread(n: u32) {
    let h = std::thread::spawn(move || {
        let _ = n;
    });
    let _ = h.join();
}

unsafe extern "C" fn construct(_cfg: AbiStr, out: *mut *mut AbiComponent) -> u32 {
    // 触发 TLS 访问（dtor 注册形态进入符号引用）
    MARK.with(|m| m.borrow_mut().push(2));
    let state = Box::into_raw(Box::new(0u64));
    *out = Box::into_raw(Box::new(AbiComponent {
        vt: &VT,
        self_: state as *mut (),
    }));
    ABI_OK
}

unsafe extern "C" fn destroy(comp: *mut AbiComponent) {
    if !comp.is_null() {
        drop(Box::from_raw((*comp).self_ as *mut u64));
        drop(Box::from_raw(comp));
    }
}

unsafe extern "C" fn handle_msg(self_: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32 {
    *(self_ as *mut u64) += 1;
    let out = &mut *out;
    let _ = out.write(msg.bytes());
    ABI_OK
}

unsafe extern "C" fn on_drain(_self_: *mut ()) -> u32 {
    ABI_OK
}

static VT: AbiVt = AbiVt { handle_msg, on_drain };

#[no_mangle]
pub static PARROT_ABI_META: AbiMeta = AbiMeta {
    abi_version: PARROT_ABI_VERSION,
    parrot_min: 1,
    name: b"violating\0".as_ptr(),
    name_len: 9,
    hooks: AbiHooks { construct, destroy },
};
