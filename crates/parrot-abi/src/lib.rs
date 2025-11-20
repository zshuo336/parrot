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
    /// # Panics
    /// Panics if the UTF-8 byte length cannot be represented by the ABI's u32.
    pub fn of_str(s: &str) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: u32::try_from(s.len()).expect("ABI string length exceeds u32"),
        }
    }

    /// 读回 Rust &str（dylib 侧传入自身静态数据时安全）。
    /// # Safety
    /// `ptr` must be non-null and reference `len` initialized, valid UTF-8 bytes
    /// for the returned lifetime. That memory must not be mutated while borrowed.
    pub unsafe fn as_str<'a>(&self) -> &'a str {
        // Keep the historical ABI-facing method, but turn malformed foreign
        // input into a contained panic instead of creating an invalid `str`.
        unsafe { self.try_as_str() }.expect("invalid AbiStr UTF-8 or null pointer")
    }

    /// Read a foreign string without assuming that the producer obeyed the
    /// UTF-8 contract. A null pointer is accepted only for an empty string.
    ///
    /// # Safety
    /// A null `ptr` is rejected when `len != 0`, without dereferencing it.
    /// Otherwise, nonempty input must reference `len` initialized readable
    /// bytes in one allocation for the returned lifetime, with no mutation
    /// while borrowed. This method cannot validate a dangling non-null pointer.
    pub unsafe fn try_as_str<'a>(&self) -> Option<&'a str> {
        if self.len == 0 {
            return Some("");
        }
        if self.ptr.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(self.ptr, self.len as usize) };
        std::str::from_utf8(bytes).ok()
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
    /// # Panics
    /// Panics if either byte length cannot be represented by the ABI's u32.
    pub fn new(type_key: &str, payload: &[u8]) -> Self {
        Self {
            type_key: type_key.as_ptr(),
            key_len: u32::try_from(type_key.len()).expect("ABI message key length exceeds u32"),
            payload: payload.as_ptr(),
            payload_len: u32::try_from(payload.len())
                .expect("ABI message payload length exceeds u32"),
        }
    }

    /// type_key 读回。
    /// # Safety
    /// `type_key` must be non-null and reference `key_len` initialized, valid
    /// UTF-8 bytes for the returned lifetime, without mutation while borrowed.
    pub unsafe fn key<'a>(&self) -> &'a str {
        unsafe { self.try_key() }.expect("invalid AbiMsg type key UTF-8 or null pointer")
    }

    /// Checked counterpart to [`Self::key`].
    ///
    /// # Safety
    /// A null `type_key` is rejected when `key_len != 0`, without dereferencing it.
    /// Otherwise, nonempty input must reference `key_len` initialized readable
    /// bytes in one allocation for the returned lifetime, without mutation
    /// while borrowed. Non-null pointers are not checked for validity.
    pub unsafe fn try_key<'a>(&self) -> Option<&'a str> {
        if self.key_len == 0 {
            return Some("");
        }
        if self.type_key.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(self.type_key, self.key_len as usize) };
        std::str::from_utf8(bytes).ok()
    }

    /// payload 读回。
    /// # Safety
    /// `payload` must be non-null and reference `payload_len` initialized bytes
    /// for the returned lifetime, without mutation while borrowed.
    pub unsafe fn bytes<'a>(&self) -> &'a [u8] {
        unsafe { self.try_bytes() }.expect("invalid AbiMsg payload pointer")
    }

    /// Reject a null nonempty payload before constructing a slice.
    /// A null pointer is accepted for an empty payload.
    ///
    /// # Safety
    /// A null `payload` is rejected when `payload_len != 0`, without dereferencing it.
    /// Otherwise, nonempty input must reference `payload_len` initialized readable
    /// bytes in one allocation for the returned lifetime, without mutation
    /// while borrowed. Non-null pointers are not checked for validity.
    pub unsafe fn try_bytes<'a>(&self) -> Option<&'a [u8]> {
        if self.payload_len == 0 {
            return Some(&[]);
        }
        if self.payload.is_null() {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts(self.payload, self.payload_len as usize) })
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
    /// # Panics
    /// Panics if the capacity cannot be represented by the ABI's u32.
    pub fn new(buf: &mut [u8]) -> Self {
        Self {
            buf: buf.as_mut_ptr(),
            cap: u32::try_from(buf.len()).expect("ABI reply capacity exceeds u32"),
            written: 0,
        }
    }

    /// 组件侧写入回复（返回是否容纳）。
    /// # Safety
    /// If `data` is nonempty, fits in `cap`, and `buf` is non-null, `buf` must
    /// be valid for writes to `data.len()` bytes in one live allocation and
    /// exclusively writable during the copy. The destination must not overlap
    /// `data`. Oversized data and null nonempty destinations are rejected before
    /// any access; no memory is accessed for empty data.
    pub unsafe fn write(&mut self, data: &[u8]) -> bool {
        if data.len() > self.cap as usize || (!data.is_empty() && self.buf.is_null()) {
            return false;
        }
        if !data.is_empty() {
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.buf, data.len()) };
        }
        self.written = data.len() as u32;
        true
    }

    /// 宿主侧读回已写段。
    /// # Safety
    /// `written` must not exceed `cap`. `buf` must be non-null and reference
    /// `written` initialized bytes in one live allocation for the returned
    /// lifetime, without mutation while borrowed.
    pub unsafe fn written_slice<'a>(&self) -> &'a [u8] {
        unsafe { self.try_written_slice() }.expect("invalid AbiReplyBuf written range")
    }

    /// Read back a reply only when the foreign component reported a valid
    /// initialized range. A null buffer is accepted for an empty reply.
    ///
    /// # Safety
    /// If `0 < written <= cap` and `buf` is non-null, `buf` must reference
    /// `written` initialized readable bytes in one allocation for the returned
    /// lifetime, without mutation while borrowed. Other ranges and null nonempty
    /// buffers are rejected before access. Non-null pointers are not checked
    /// for validity.
    pub unsafe fn try_written_slice<'a>(&self) -> Option<&'a [u8]> {
        let written = self.written as usize;
        if written > self.cap as usize {
            return None;
        }
        if written == 0 {
            return Some(&[]);
        }
        if self.buf.is_null() {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts(self.buf, written) })
    }
}

/// 组件虚表（dylib 提供——handle_msg/on_drain 两入口）。
#[repr(C)]
pub struct AbiVt {
    pub handle_msg: unsafe extern "C" fn(self_: *mut (), msg: AbiMsg, out: *mut AbiReplyBuf) -> u32,
    pub on_drain: unsafe extern "C" fn(self_: *mut ()) -> u32,
}

/// Optional causal parent passed to a native message invocation. Identifiers
/// have the same byte order as W3C trace/span IDs; neither may be all zero.
/// This metadata is not authentication, sampling state or a completed span.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbiTraceParentV1 {
    pub trace_id: [u8; 16],
    pub span_id: [u8; 8],
}
impl AbiTraceParentV1 {
    pub fn is_valid(&self) -> bool {
        self.trace_id != [0; 16] && self.span_id != [0; 8]
    }
}

/// Optional extension exported as a static `PARROT_ABI_TRACE_V1`. The versioned
/// symbol freezes this layout independently of ABI v1's metadata and vtable.
/// Older hosts ignore it; hosts without this symbol use the original vtable.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AbiTraceV1 {
    /// Replaces `AbiVt::handle_msg` for this call, with identical ownership,
    /// exclusivity, return-code and non-unwinding obligations. `parent` is null
    /// for an untraced call, otherwise a valid borrowed parent for this call
    /// only. The guest must not retain the pointer or install ambient context
    /// beyond the invocation. To record guest work, create a child of this ID.
    pub handle_msg: unsafe extern "C" fn(
        self_: *mut (),
        msg: AbiMsg,
        out: *mut AbiReplyBuf,
        parent: *const AbiTraceParentV1,
    ) -> u32,
}

pub const ABI_TRACE_V1_SYMBOL: &str = "PARROT_ABI_TRACE_V1";

/// Optional guest recording extension, independent of the frozen ABI v1 layouts.
pub const ABI_RECORDING_V1_SYMBOL: &str = "PARROT_ABI_RECORDING_V1";
/// Optional lifecycle hooks; the original metadata, vtable and extensions keep
/// their frozen layouts. Old hosts continue to use the original hooks.
pub const ABI_LIFECYCLE_V1_SYMBOL: &str = "PARROT_ABI_LIFECYCLE_V1";

/// Export immutable component-owned storage as PARROT_ABI_LIFECYCLE_V1.
/// These hooks replace only their corresponding explicit loader entry points.
/// Both optional pointers are borrowed for the synchronous call, may be null,
/// and must never be retained. Callbacks run on the invoking thread only.
/// Constructors have the original unique-instance/clean-on-failure contract;
/// drain must leave destroy valid even on failure. Destroy MUST release the
/// instance on every return code, including ABI_ERR_PANIC, and cannot be retried.
/// Guests must catch panics internally before crossing any C boundary.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AbiLifecycleV1 {
    pub construct: unsafe extern "C" fn(
        AbiStr,
        *mut *mut AbiComponent,
        *const AbiTraceParentV1,
        *const AbiRecordingHostV1,
    ) -> u32,
    pub on_drain:
        unsafe extern "C" fn(*mut (), *const AbiTraceParentV1, *const AbiRecordingHostV1) -> u32,
    pub destroy: unsafe extern "C" fn(
        *mut AbiComponent,
        *const AbiTraceParentV1,
        *const AbiRecordingHostV1,
    ) -> u32,
}
pub const ABI_RECORDING_OK: u32 = 0;
pub const ABI_RECORDING_DISABLED: u32 = 1;
pub const ABI_RECORDING_LIMIT: u32 = 2;
pub const ABI_RECORDING_INVALID: u32 = 3;
pub const ABI_RECORDING_UNKNOWN_HANDLE: u32 = 4;
pub const ABI_RECORDING_EXHAUSTED: u32 = 5;
pub const ABI_RECORDING_PANIC: u32 = 6;
pub const ABI_SPAN_OK: u32 = 0;
pub const ABI_SPAN_ERROR: u32 = 1;
pub const ABI_SPAN_CANCELLED: u32 = 2;
pub const ABI_SPAN_PANICKED: u32 = 3;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AbiSpanV1 {
    pub handle: u64,
    pub parent: AbiTraceParentV1,
}

/// Borrowed recording capability for one synchronous native invocation.
/// All callbacks must run on its native dispatch thread before the hook
/// returns. No pointer in this descriptor may be retained or used asynchronously.
/// Native components remain trusted code, not sandboxed code.
#[repr(C)]
pub struct AbiRecordingHostV1 {
    pub context: *mut (),
    /// Parent handle 0 chooses the invocation parent; nonzero must still be active
    /// in this invocation. Target is 1..256 UTF-8 bytes. `out` must point to
    /// writable, aligned AbiSpanV1 storage disjoint from all borrowed inputs.
    /// Output is initialized only on ABI_RECORDING_OK. At most 256 operations
    /// are admitted per invocation, including operations already finished.
    pub start: unsafe extern "C" fn(*mut (), u64, AbiStr, *mut AbiSpanV1) -> u32,
    /// Status is an ABI_SPAN_* value; unknown status/retired handle is rejected.
    pub finish: unsafe extern "C" fn(*mut (), u64, u32) -> u32,
}

/// Export immutable static storage as PARROT_ABI_RECORDING_V1. Replaces the
/// message hook only through the explicit recording loader entry point. Both
/// optional arguments may be null. Original vtable/trace hooks must retain
/// their business behavior for hosts which do not implement this extension.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AbiRecordingV1 {
    pub handle_msg: unsafe extern "C" fn(
        *mut (),
        AbiMsg,
        *mut AbiReplyBuf,
        *const AbiTraceParentV1,
        *const AbiRecordingHostV1,
    ) -> u32,
}

pub const ABI_MESSAGING_V1_SYMBOL: &str = "PARROT_ABI_MESSAGING_V1";
pub const ABI_MESSAGE_OK: u32 = 0;
pub const ABI_MESSAGE_DENIED: u32 = 1;
pub const ABI_MESSAGE_INVALID: u32 = 2;
pub const ABI_MESSAGE_LIMIT: u32 = 3;
pub const ABI_MESSAGE_TIMEOUT: u32 = 4;
pub const ABI_MESSAGE_TARGET: u32 = 5;
pub const ABI_MESSAGE_CODEC: u32 = 6;
pub const ABI_MESSAGE_REPLY: u32 = 7;
pub const ABI_MESSAGE_FAILED: u32 = 8;
pub const ABI_MESSAGE_PANIC: u32 = 9;

/// Borrowed messaging capability for one synchronous native message call.
/// Callbacks run only on that call's dispatch thread and may block up to the
/// supplied timeout. Descriptors, pointers and buffers must not be retained.
/// Trusted native code must provide live, aligned, disjoint views for their
/// entire declared lengths. ASK initializes `written` to zero on failure;
/// replies and their effects are never retried automatically on overflow.
#[repr(C)]
pub struct AbiMessagingHostV1 {
    pub context: *mut (),
    /// Exact destination, request and expected reply key, with timeout in ms.
    /// Output capacity must be at most 4 MiB; timeout must be 1..=30000 ms.
    pub ask: unsafe extern "C" fn(*mut (), AbiStr, AbiMsg, AbiStr, *mut AbiReplyBuf, u32) -> u32,
    /// Success acknowledges mailbox admission, not business completion.
    pub tell: unsafe extern "C" fn(*mut (), AbiStr, AbiMsg, u32) -> u32,
}

/// Component-owned immutable static PARROT_ABI_MESSAGING_V1. This entry composes
/// trace, recording and messaging without changing any previous ABI layouts.
/// Only the explicit capabilities loader entry invokes it; older entries keep
/// using recording/trace/original hooks. Null capabilities grant no authority.
/// Hooks and callbacks must never unwind across the C boundary.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AbiMessagingV1 {
    pub handle_msg: unsafe extern "C" fn(
        *mut (),
        AbiMsg,
        *mut AbiReplyBuf,
        *const AbiTraceParentV1,
        *const AbiRecordingHostV1,
        *const AbiMessagingHostV1,
    ) -> u32,
}

pub const ABI_SERVICES_V1_SYMBOL: &str = "PARROT_ABI_SERVICES_V1";
pub const ABI_SERVICE_OK: u32 = 0;
pub const ABI_SERVICE_DENIED: u32 = 1;
pub const ABI_SERVICE_INVALID: u32 = 2;
pub const ABI_SERVICE_LIMIT: u32 = 3;
pub const ABI_SERVICE_TIMEOUT: u32 = 4;
pub const ABI_SERVICE_CLOSED: u32 = 5;
pub const ABI_SERVICE_HANDLE: u32 = 6;
pub const ABI_SERVICE_FAILED: u32 = 7;
pub const ABI_SERVICE_PANIC: u32 = 8;
pub const ABI_SERVICE_REGISTERED: u32 = 1;
pub const ABI_SERVICE_UNREGISTERED: u32 = 2;

/// Borrowed discovery capability; callbacks obey AbiMessagingHostV1's pointer,
/// same-thread and non-unwinding contract. Keys are 1..=1024 UTF-8 bytes and
/// timeouts 1..=30000 ms. Registration always publishes this actor's actual path
/// and generation; discovery grants never authorize business messages.
#[repr(C)]
pub struct AbiServicesHostV1 {
    pub context: *mut (),
    pub register: unsafe extern "C" fn(*mut (), AbiStr, u32) -> u32,
    pub deregister: unsafe extern "C" fn(*mut (), AbiStr, u32) -> u32,
    /// On failure writes handle 0. At most 16 live, monotonically allocated
    /// handles per native actor instance; handles survive message returns.
    pub subscribe: unsafe extern "C" fn(*mut (), AbiStr, u32, *mut u64) -> u32,
    /// On failure clears written/kind. Timeout does not consume an event.
    /// Insufficient output capacity returns LIMIT and retains the event for
    /// another next call. Path capacity must be <=4096. CLOSED retires a stream;
    /// slow-consumer closure requires resubscription and rebuilding a snapshot.
    pub next: unsafe extern "C" fn(*mut (), u64, u32, *mut AbiReplyBuf, *mut u32) -> u32,
    pub close: unsafe extern "C" fn(*mut (), u64) -> u32,
}

/// Optional component-owned immutable PARROT_ABI_SERVICES_V1. Selected only by
/// handle_msg_with_services; composes discovery with all earlier capabilities.
/// All descriptors are borrowed for this invocation and may be null.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AbiServicesV1 {
    pub handle_msg: unsafe extern "C" fn(
        *mut (),
        AbiMsg,
        *mut AbiReplyBuf,
        *const AbiTraceParentV1,
        *const AbiRecordingHostV1,
        *const AbiMessagingHostV1,
        *const AbiServicesHostV1,
    ) -> u32,
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
    /// `name` must be non-null and reference `name_len` initialized, valid UTF-8
    /// bytes for the returned lifetime, without mutation while borrowed.
    pub unsafe fn name_str<'a>(&self) -> &'a str {
        unsafe { self.try_name_str() }.expect("invalid AbiMeta name UTF-8 or null pointer")
    }

    /// Checked counterpart to [`Self::name_str`] for untrusted metadata.
    ///
    /// # Safety
    /// A null `name` is rejected when `name_len != 0`, without dereferencing it.
    /// Otherwise, nonempty input must reference `name_len` initialized readable
    /// bytes in one allocation for the returned lifetime, without mutation
    /// while borrowed. Non-null pointers are not checked for validity.
    pub unsafe fn try_name_str<'a>(&self) -> Option<&'a str> {
        if self.name_len == 0 {
            return Some("");
        }
        if self.name.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(self.name, self.name_len as usize) };
        std::str::from_utf8(bytes).ok()
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
    DylibHandle, DylibLoader, InFlightGuard, LoadError, NativeArtifact, NativeArtifactLimits,
    UnloadError, UnloadReport, Violation,
};

// ─────────────────────────────────────────────────────────────
// D1 面：纯类型单测（布局/常量——无 dylib 参与）
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(name: *const u8, name_len: u32) -> AbiMeta {
        unsafe extern "C" fn construct(_: AbiStr, _: *mut *mut AbiComponent) -> u32 {
            ABI_ERR_STATE
        }
        unsafe extern "C" fn destroy(_: *mut AbiComponent) {}
        AbiMeta {
            abi_version: PARROT_ABI_VERSION,
            parrot_min: PARROT_ABI_VERSION,
            name,
            name_len,
            hooks: AbiHooks { construct, destroy },
        }
    }

    #[test]
    fn checked_text_views_match_utf8_for_every_explicit_byte_length() {
        for bytes in [
            &b"name\0suffix"[..],
            &b"p\xc3\xa9\xe4\xb8\xad\xf0\x9f\x8c\x8d"[..],
            &b"\xff\xc0\xaf"[..],
        ] {
            for length in 0..=bytes.len() {
                let expected = std::str::from_utf8(&bytes[..length]).ok();
                let text = AbiStr {
                    ptr: bytes.as_ptr(),
                    len: length as u32,
                };
                let message = AbiMsg {
                    type_key: bytes.as_ptr(),
                    key_len: length as u32,
                    payload: std::ptr::null(),
                    payload_len: 0,
                };
                let meta = metadata(bytes.as_ptr(), length as u32);
                unsafe {
                    assert_eq!(text.try_as_str(), expected);
                    assert_eq!(message.try_key(), expected);
                    assert_eq!(meta.try_name_str(), expected);
                    if let Some(expected) = expected.filter(|value| !value.is_empty()) {
                        let actual = meta.try_name_str().unwrap();
                        assert_eq!(actual.as_ptr(), expected.as_ptr());
                        assert_eq!(meta.name_str(), expected);
                    }
                }
            }
        }
    }

    #[test]
    fn checked_text_views_handle_null_ranges_without_dereferencing_them() {
        for length in [0, 1, 4, u32::MAX] {
            let expected = (length == 0).then_some("");
            let text = AbiStr {
                ptr: std::ptr::null(),
                len: length,
            };
            let message = AbiMsg {
                type_key: std::ptr::null(),
                key_len: length,
                payload: std::ptr::null(),
                payload_len: length,
            };
            let meta = metadata(std::ptr::null(), length);
            unsafe {
                assert_eq!(text.try_as_str(), expected);
                assert_eq!(message.try_key(), expected);
                assert_eq!(meta.try_name_str(), expected);
                assert_eq!(message.try_bytes(), (length == 0).then_some(&[][..]));
            }
        }
    }

    #[test]
    fn checked_binary_views_preserve_bytes_and_enforce_reported_ranges() {
        let mut bytes = [0, 0xff, 0xc0, 0xaf, 0, 7];
        for length in 0..=bytes.len() {
            let message = AbiMsg {
                type_key: std::ptr::null(),
                key_len: 0,
                payload: bytes.as_ptr(),
                payload_len: length as u32,
            };
            assert_eq!(unsafe { message.try_bytes() }, Some(&bytes[..length]));
            for capacity in 0..=bytes.len() {
                let reply = AbiReplyBuf {
                    buf: bytes.as_mut_ptr(),
                    cap: capacity as u32,
                    written: length as u32,
                };
                let expected = (length <= capacity).then_some(&bytes[..length]);
                assert_eq!(unsafe { reply.try_written_slice() }, expected);
            }
        }
    }

    #[test]
    fn rejected_reply_writes_preserve_previous_length_and_storage() {
        let mut storage = [0xa5; 4];
        let mut reply = AbiReplyBuf::new(&mut storage);
        unsafe {
            assert!(reply.write(&[0, 0xff]));
            assert!(!reply.write(&[1, 2, 3, 4, 5]));
            assert_eq!(reply.written, 2);
            assert_eq!(reply.try_written_slice(), Some(&[0, 0xff][..]));
        }
        assert_eq!(storage, [0, 0xff, 0xa5, 0xa5]);
        unsafe {
            assert!(reply.write(&[1, 2, 3, 4]));
            assert_eq!(reply.try_written_slice(), Some(&[1, 2, 3, 4][..]));
            assert!(reply.write(&[9]));
            assert_eq!(reply.written, 1);
            assert_eq!(reply.try_written_slice(), Some(&[9][..]));
        }
        let mut null = AbiReplyBuf {
            buf: std::ptr::null_mut(),
            cap: 4,
            written: 2,
        };
        assert!(!unsafe { null.write(&[1]) });
        assert_eq!(null.written, 2);
        assert!(unsafe { null.write(&[]) });
        assert_eq!(null.written, 0);
        assert_eq!(unsafe { null.try_written_slice() }, Some(&[][..]));
    }

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
            assert_eq!(a.try_as_str(), Some(s));
        }
    }

    #[test]
    fn foreign_text_views_reject_invalid_utf8_and_null_nonempty_inputs() {
        let invalid = [0xffu8];
        let raw = AbiStr {
            ptr: invalid.as_ptr(),
            len: invalid.len() as u32,
        };
        let null = AbiStr {
            ptr: std::ptr::null(),
            len: 1,
        };
        unsafe {
            assert_eq!(raw.try_as_str(), None);
            assert_eq!(null.try_as_str(), None);
            assert_eq!(
                AbiStr {
                    ptr: std::ptr::null(),
                    len: 0
                }
                .try_as_str(),
                Some("")
            );
        }

        let msg = AbiMsg {
            type_key: invalid.as_ptr(),
            key_len: 1,
            payload: std::ptr::null(),
            payload_len: 0,
        };
        unsafe {
            assert_eq!(msg.try_key(), None);
            assert_eq!(msg.try_bytes(), Some(&[][..]));
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

    #[test]
    fn abi_empty_foreign_buffers_do_not_require_non_null_pointers() {
        let msg = AbiMsg {
            type_key: std::ptr::null(),
            key_len: 0,
            payload: std::ptr::null(),
            payload_len: 0,
        };
        unsafe {
            assert_eq!(msg.try_key(), Some(""));
            assert_eq!(msg.try_bytes(), Some(&[][..]));
        }

        let mut out = AbiReplyBuf {
            buf: std::ptr::null_mut(),
            cap: 0,
            written: 0,
        };
        assert!(unsafe { out.write(&[]) });
        assert_eq!(unsafe { out.try_written_slice() }, Some(&[][..]));
        assert!(!unsafe { out.write(b"non-empty") });
    }

    #[test]
    fn abi_reply_rejects_foreign_written_overflow_and_null_storage() {
        let out = AbiReplyBuf {
            buf: std::ptr::null_mut(),
            cap: 8,
            written: 9,
        };
        unsafe {
            assert_eq!(out.try_written_slice(), None);
        }
        let out = AbiReplyBuf {
            buf: std::ptr::null_mut(),
            cap: 8,
            written: 1,
        };
        unsafe {
            assert_eq!(out.try_written_slice(), None);
        }
    }
}
