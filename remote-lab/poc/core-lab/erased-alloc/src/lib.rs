//! POC 4：单块内存擦除（一 alloc 模式）。
//!
//! 问题：现状每消息 2 次装箱（消息 Box + Any 擦除），堆分配 2 次。
//! 方案：SingleAllocEnvelope —— 一次分配同时容纳【擦除头 + 消息本体】，
//! 消息按对齐摆放在同一块内存上，downcast 只做指针运算，零额外分配。
//!
//! 对标：SmallBox / thin-box / header-allocated slab（Erlang term 的
//! boxed header 思想：header 与 payload 连续分配）。

use std::alloc::{alloc, dealloc, Layout};
use std::any::{Any, TypeId};
use std::ptr::NonNull;

/// 擦除头（布局在块首）。
#[repr(C)]
struct Header {
    type_id: TypeId,
    drop_fn: unsafe fn(*mut u8),
    /// payload 相对块首的偏移（按消息对齐摆放）
    payload_off: usize,
    /// 整块 layout（dealloc 用）
    layout: Layout,
}

/// 单分配消息信封。
pub struct SingleAllocEnvelope {
    ptr: NonNull<u8>,
}

impl SingleAllocEnvelope {
    /// 一次分配：header + padding + payload 连续。
    pub fn new<M: Any + Send>(msg: M) -> Self {
        let type_id = TypeId::of::<M>();
        let payload_layout = Layout::new::<M>();
        // header 对齐 ≥ payload 对齐时直接前放；否则整体对齐取 max
        let align = payload_layout.align().max(Layout::new::<Header>().align());
        // 布局：[header][padding to payload align][payload]
        let header_size = Layout::new::<Header>().size();
        let payload_off = (header_size + payload_layout.align() - 1) & !(payload_layout.align() - 1);
        let total = payload_off + payload_layout.size();
        let layout = Layout::from_size_align(total, align).expect("layout");

        unsafe {
            let raw = alloc(layout);
            let ptr = NonNull::new(raw).expect("alloc failed");

            // 写 header
            let hdr = ptr.as_ptr() as *mut Header;
            // 特化 drop：包装为裸函数（M 类型在编译期已知）
            unsafe fn make_drop<M2>() -> unsafe fn(*mut u8) {
                unsafe fn dropper<M3>(p: *mut u8) {
                    let m = std::ptr::read(p as *const M3);
                    drop(m);
                }
                dropper::<M2>
            }
            std::ptr::write(
                hdr,
                Header {
                    type_id,
                    drop_fn: make_drop::<M>(),
                    payload_off,
                    layout,
                },
            );

            // 写 payload（move 进去）
            let payload_ptr = ptr.as_ptr().add(payload_off) as *mut M;
            std::ptr::write(payload_ptr, msg);

            Self { ptr }
        }
    }

    /// 类型检查 + 取回（downcast 只是指针运算 + TypeId 比较）。
    pub fn downcast<M: Any + Send>(mut self) -> Result<M, Self> {
        if self.type_id() == TypeId::of::<M>() {
            unsafe {
                let hdr = self.ptr.as_ptr() as *const Header;
                let payload_ptr = self.ptr.as_ptr().add((*hdr).payload_off) as *const M;
                let m = std::ptr::read(payload_ptr);
                // 手动释放内存；forget 防止 Drop::drop 再次 dealloc/drop
                let layout = (*hdr).layout;
                let raw = self.ptr.as_ptr();
                std::mem::forget(self);
                dealloc(raw, layout);
                Ok(m)
            }
        } else {
            Err(self)
        }
    }

    pub fn type_id(&self) -> TypeId {
        unsafe { (*(self.ptr.as_ptr() as *const Header)).type_id }
    }
}

impl Drop for SingleAllocEnvelope {
    fn drop(&mut self) {
        unsafe {
            let hdr = self.ptr.as_ptr() as *const Header;
            let payload_ptr = self.ptr.as_ptr().add((*hdr).payload_off);
            ((*hdr).drop_fn)(payload_ptr);
            dealloc(self.ptr.as_ptr(), (*hdr).layout);
        }
    }
}

// 修正：Drop 用 &mut self（self.ptr 已 NonNull，as_ptr 需解引用）

// Send 声明：块内 payload 是 M: Send（构造时 move 独占）
unsafe impl Send for SingleAllocEnvelope {}

// ============ 基线对照（现状：两次分配） ============

pub fn two_alloc_baseline<M: Any + Send>(msg: M) -> Box<dyn Any + Send> {
    Box::new(msg) // 1 次外层分配（内含 M）。现状路径还要 envelope Box → 2 次
}

// ============ POC 验证 ============

#[cfg(test)]
mod tests {
    use super::*;

    struct BigMsg {
        _data: [u64; 32], // 256B，确保跨对齐边界
        tag: u64,
    }

    #[test]
    fn single_alloc_roundtrip() {
        let env = SingleAllocEnvelope::new(BigMsg { _data: [0; 32], tag: 42 });
        assert_eq!(env.type_id(), TypeId::of::<BigMsg>());
        let m = env.downcast::<BigMsg>().ok().expect("type match");
        assert_eq!(m.tag, 42);
    }

    #[test]
    fn single_alloc_wrong_type_keeps_envelope() {
        let env = SingleAllocEnvelope::new(1234u64);
        let back = env.downcast::<String>().err().expect("must fail");
        // 原 envelope 仍可用
        let m = back.downcast::<u64>().ok().unwrap();
        assert_eq!(m, 1234);
    }

    #[test]
    fn drop_runs_payload_destructor() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct Tracked;
        impl Drop for Tracked {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
        let before = DROPS.load(Ordering::SeqCst);
        {
            let env = SingleAllocEnvelope::new(Tracked);
            drop(env);
        }
        assert_eq!(DROPS.load(Ordering::SeqCst), before + 1, "payload Drop 必须执行");
    }
}
