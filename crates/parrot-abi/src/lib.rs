//! parrot-abi：Parrot dylib 组件 ABI（DEV_09 §3.4 / 09 §4.3 D1）。
//!
//! 边界纪律（09 §4.3 ①）：唯一导出形态 = `extern "C"` + 显式 `#[repr(C)]`
//! 布局——**禁止跨边界 Rust 类型**。dylib 与宿主共同依赖本 crate 的类型
//! 定义，但两侧各自编译（Rust 无稳定 ABI——布局由 repr(C) 钉死）。
//!
//! 消息边界 = bytes（`AbiMsg`）——与 wasm/网络路径同构，三种形态共用
//! 一套消息契约。
//!
//! 分层：
//! - 本文件：D1 纯类型（default feature，零 std 之外依赖）
//! - `loader`（feature gate）：D2 宿主侧加载/卸载（libloading）

/// 当前 ABI 版本（AbiMeta.abi_version 强校验值）。
pub const PARROT_ABI_VERSION: u32 = 1;

/// 结果码（错误即返回码——无 Result 跨界）。
pub const ABI_OK: u32 = 0;
/// dylib 侧 panic（catch_unwind 捕获后转此码）。
pub const ABI_ERR_PANIC: u32 = 1;
/// 组件状态错误（destroy 后使用 / 内部状态非法）。
pub const ABI_ERR_STATE: u32 = 2;
/// 载荷超限（回复缓冲不足）。
pub const ABI_ERR_OVERFLOW: u32 = 3;

/// C 字符串视图（ptr+len——非 NUL 终止，避免strlen 越界）。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AbiStr {
    pub ptr: *const u8,
    pub len: u32,
}

impl AbiStr {
    /// 从 Rust &str 构造（宿主侧——生命周期由调用方保证）。
    /// （命名 of_str——避免与 FromStr trait 混淆）
    pub fn of_str(s: &str) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: s.len() as u32,
        }
    }

    /// 读回 Rust &str（dylib 侧传入自身静态数据时安全）。
    /// # Safety
    /// ptr 必须指向 len 字节有效内存。
    pub unsafe fn as_str<'a>(&self) -> &'a str {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.ptr, self.len as usize))
    }
}

/// 消息（与 wasm WIT msg / Wire 帧 type_key+bytes 同构）。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AbiMsg {
    pub type_key: *const u8,
    pub key_len: u32,
    pub payload: *const u8,
    pub payload_len: u32,
}

impl AbiMsg {
    /// 从 Rust 视图构造（宿主侧）。
    pub fn new(type_key: &str, payload: &[u8]) -> Self {
        Self {
            type_key: type_key.as_ptr(),
            key_len: type_key.len() as u32,
            payload: payload.as_ptr(),
            payload_len: payload.len() as u32,
        }
    }

    /// type_key 读回。
    /// # Safety
    /// type_key 必须指向 key_len 字节有效内存。
    pub unsafe fn key<'a>(&self) -> &'a str {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(
            self.type_key,
            self.key_len as usize,
        ))
    }

    /// payload 读回。
    /// # Safety
    /// payload 必须指向 payload_len 字节有效内存。
    pub unsafe fn bytes<'a>(&self) -> &'a [u8] {
        std::slice::from_raw_parts(self.payload, self.payload_len as usize)
    }
}

/// 回复缓冲（宿主预分配——组件写入后回填 written；不足回 ABI_ERR_OVERFLOW）。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AbiReplyBuf {
    pub buf: *mut u8,
    pub cap: u32,
    pub written: u32,
}

impl AbiReplyBuf {
    /// 宿主侧构造（buf 生存期由调用方保证）。
    pub fn new(buf: &mut [u8]) -> Self {
        Self {
            buf: buf.as_mut_ptr(),
            cap: buf.len() as u32,
            written: 0,
        }
    }

    /// 组件侧写入回复（返回是否容纳）。
    /// # Safety
    /// buf 必须指向 cap 字节可写内存。
    pub unsafe fn write(&mut self, data: &[u8]) -> bool {
        if data.len() > self.cap as usize {
            return false;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), self.buf, data.len());
        self.written = data.len() as u32;
        true
    }

    /// 宿主侧读回已写段。
    /// # Safety
    /// written ≤ cap 且 buf 有效。
    pub unsafe fn written_slice<'a>(&self) -> &'a [u8] {
        std::slice::from_raw_parts(self.buf, self.written as usize)
    }
}

/// 组件虚表（dylib 提供——handle_msg/on_drain 两入口）。
#[repr(C)]
pub struct AbiVt {
    pub handle_msg: unsafe extern "C" fn(self_: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32,
    pub on_drain: unsafe extern "C" fn(self_: *mut ()) -> u32,
}

/// 组件实例（vt + 不透明自指针——宿主只透传 self_）。
#[repr(C)]
pub struct AbiComponent {
    pub vt: *const AbiVt,
    pub self_: *mut (),
}

/// 生命周期钩子（construct/destroy——meta 携带）。
#[repr(C)]
pub struct AbiHooks {
    pub construct: unsafe extern "C" fn(cfg: AbiStr, out: *mut *mut AbiComponent) -> u32,
    pub destroy: unsafe extern "C" fn(comp: *mut AbiComponent),
}

/// dylib 唯一导出符号 `parrot_abi_meta` 的形态（加载即校验）。
#[repr(C)]
pub struct AbiMeta {
    pub abi_version: u32,
    /// 兼容的最低宿主 ABI 版本（> 当前即拒载）。
    pub parrot_min: u32,
    pub name: *const u8,
    pub name_len: u32,
    pub hooks: AbiHooks,
}

impl AbiMeta {
    /// name 读回（dylib 侧静态数据）。
    /// # Safety
    /// name 必须指向 name_len 字节有效内存。
    pub unsafe fn name_str<'a>(&self) -> &'a str {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(
            self.name,
            self.name_len as usize,
        ))
    }
}

/// dylib 侧必须导出的符号名（`#[no_mangle] pub static PARROT_ABI_META`）。
pub const ABI_META_SYMBOL: &str = "PARROT_ABI_META";

// 静态导出需要（repr(C) 原始指针字段非 Sync——静态数据实为库内只读）。
unsafe impl Sync for AbiMeta {}
unsafe impl Send for AbiMeta {}

/// D2 宿主侧加载器（feature gate——libloading 重依赖）。
#[cfg(feature = "loader")]
pub mod loader;

#[cfg(feature = "loader")]
pub use loader::{
    DylibHandle, DylibLoader, InFlightGuard, LoadError, UnloadError, UnloadReport, Violation,
};

// ─────────────────────────────────────────────────────────────
// D1 面：纯类型单测（布局/常量——无 dylib 参与）
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_version_pinned() {
        assert_eq!(PARROT_ABI_VERSION, 1);
        assert_eq!(ABI_OK, 0);
        assert_eq!(ABI_ERR_PANIC, 1);
        assert_eq!(ABI_ERR_STATE, 2);
        assert_eq!(ABI_ERR_OVERFLOW, 3);
    }

    #[test]
    fn repr_c_layouts_stable() {
        // repr(C) 布局钉死断言（跨编译器版本漂移即破坏——CI 门禁数据源）。
        // 64 位目标：ptr=8B/align 8，u32=4B → 尾部 padding 到 8 倍数。
        let p = std::mem::size_of::<usize>();
        assert_eq!(std::mem::size_of::<AbiStr>(), p + 4 + 4); // ptr+len+pad
        assert_eq!(std::mem::align_of::<AbiStr>(), p);
        // AbiMsg：ptr+u32 ×2 → (8+4)+pad+(8+4)+pad
        assert_eq!(std::mem::size_of::<AbiMsg>(), (p + 4 + 4) * 2);
        assert_eq!(std::mem::size_of::<AbiReplyBuf>(), p + 4 + 4); // ptr+cap+written
        assert_eq!(std::mem::size_of::<AbiComponent>(), p * 2);
        assert_eq!(std::mem::size_of::<AbiVt>(), p * 2);
        // AbiMeta：u32+u32（8）+ptr(8)+u32(4)+pad(4)+hooks(16)
        assert_eq!(std::mem::size_of::<AbiMeta>(), 8 + p + 4 + 4 + p * 2);
        assert_eq!(std::mem::align_of::<AbiMeta>(), p);
    }

    #[test]
    fn abi_str_roundtrip() {
        let s = "comp-frontier";
        let a = AbiStr::of_str(s);
        unsafe {
            assert_eq!(a.as_str(), s);
        }
    }

    #[test]
    fn abi_msg_view() {
        let k = "echo";
        let p = b"payload";
        let m = AbiMsg::new(k, p);
        unsafe {
            assert_eq!(m.key(), k);
            assert_eq!(m.bytes(), p);
        }
    }

    #[test]
    fn reply_buf_write_read() {
        let mut storage = [0u8; 16];
        let mut rb = AbiReplyBuf::new(&mut storage);
        assert!(unsafe { rb.write(b"reply-bytes") });
        assert_eq!(rb.written, 11);
        assert_eq!(unsafe { rb.written_slice() }, b"reply-bytes");
        // 溢出拒绝
        let mut small = [0u8; 4];
        let mut rb2 = AbiReplyBuf::new(&mut small);
        assert!(!unsafe { rb2.write(b"too-long-for-buf") });
        assert_eq!(rb2.written, 0);
    }
}
