//! M5 Phase B：单块信封（`SingleAllocEnvelope`）—— POC `erased-alloc` 移植。
//!
//! 一次堆分配同时容纳【擦除头 + payload】，payload 按对齐摆放在同一块
//! 内存上；downcast 只做 TypeId 比较 + 指针运算，零额外分配。
//!
//! 与 POC 的差异：
//! - header 额外容纳 **inline oneshot::Sender**（ADR-13 叠加：ask 全链路
//!   1 分配）。drop 顺序审计：**payload → oneshot → dealloc**（payload
//!   drop 可能引用 oneshot 之外的资源，必须先于内存释放；oneshot drop
//!   唤醒等待者，必须在 dealloc 之前完成发送语义）。
//! - POC `downcast` 后 `mem::forget(self)` 的双释放坑已随迁修正：本实现
//!   的 `into_payload` 用 `ManuallyDrop` 包装 self，确保 Drop 不会对已
//!   取走的块二次 dealloc。
//!
//! ## 布局
//!
//! ```text
//! [ Header (TypeId, drop_fn, payload_off, layout, reply: Option<oneshot::Sender>) ]
//! [ padding to payload align ]
//! [ payload ]
//! ```
//!
//! ## 门禁
//!
//! miri 全绿（`scripts/miri.sh`）+ 计数分配器常驻测试
//! （`test_m5_single_alloc.rs`）。

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::any::{Any, TypeId};
use std::ptr::NonNull;

use parrot_api::types::BoxedMessage;
use tokio::sync::oneshot;

/// 擦除头（布局在块首；`repr(C)` 保证字段顺序稳定）。
#[repr(C)]
struct Header {
    type_id: TypeId,
    drop_fn: unsafe fn(*mut u8),
    /// payload 相对块首的偏移（按消息对齐摆放）
    payload_off: usize,
    /// 整块 layout（dealloc 用）
    layout: Layout,
    /// ask 回复通道（inline，ADR-13；tell 路径为 None）
    reply: Option<oneshot::Sender<parrot_api::types::ActorResult<BoxedMessage>>>,
}

/// 单分配消息信封。
///
/// 构造 `new`（tell）/ `new_ask`（ask，带 inline oneshot）各一次分配。
pub struct SingleAllocEnvelope {
    ptr: NonNull<u8>,
}

unsafe impl Send for SingleAllocEnvelope {}

impl SingleAllocEnvelope {
    /// 单块构造（tell 语义，无回复通道）。
    pub fn new<M: Any + Send>(msg: M) -> Self {
        Self::alloc_with(msg, None)
    }

    /// 单块构造（ask 语义，inline oneshot 回复通道）。
    ///
    /// 返回 (envelope, reply_receiver)：asker 持 receiver 等回复。
    pub fn new_ask<M: Any + Send>(
        msg: M,
    ) -> (
        Self,
        oneshot::Receiver<parrot_api::types::ActorResult<BoxedMessage>>,
    ) {
        let (tx, rx) = oneshot::channel();
        (Self::alloc_with(msg, Some(tx)), rx)
    }

    fn alloc_with<M: Any + Send>(
        msg: M,
        reply: Option<oneshot::Sender<parrot_api::types::ActorResult<BoxedMessage>>>,
    ) -> Self {
        let type_id = TypeId::of::<M>();
        let payload_layout = Layout::new::<M>();
        // header 对齐 ≥ payload 对齐时直接前放；否则整体对齐取 max
        let align = payload_layout.align().max(Layout::new::<Header>().align());
        // 布局：[header][padding to payload align][payload]
        let header_size = Layout::new::<Header>().size();
        let payload_off =
            (header_size + payload_layout.align() - 1) & !(payload_layout.align() - 1);
        let total = payload_off + payload_layout.size();
        let layout = Layout::from_size_align(total, align).expect("layout overflow");

        unsafe {
            let raw = alloc(layout);
            let ptr = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));

            // 写 header（drop_fn 特化到 M）
            unsafe fn make_drop<M2>() -> unsafe fn(*mut u8) {
                unsafe fn dropper<M3>(p: *mut u8) {
                    unsafe {
                        let m = std::ptr::read(p as *const M3);
                        drop(m);
                    }
                }
                dropper::<M2>
            }
            std::ptr::write(
                ptr.as_ptr() as *mut Header,
                Header {
                    type_id,
                    drop_fn: make_drop::<M>(),
                    payload_off,
                    layout,
                    reply,
                },
            );

            // 写 payload（move 进去）
            let payload_ptr = ptr.as_ptr().add(payload_off) as *mut M;
            std::ptr::write(payload_ptr, msg);

            Self { ptr }
        }
    }

    /// 类型检查。
    pub fn type_id(&self) -> TypeId {
        unsafe { (*(self.ptr.as_ptr() as *const Header)).type_id }
    }

    /// 取出 payload（downcast 只是 TypeId 比较 + 指针运算）。
    ///
    /// 类型不匹配时原样返回 `Err(self)`（POC 语义）。
    pub fn downcast<M: Any + Send>(self) -> Result<M, Self> {
        if self.type_id() == TypeId::of::<M>() {
            Ok(self.take_payload())
        } else {
            Err(self)
        }
    }

    /// 不检查类型直接取 payload（消费侧已确认类型的快路径）。
    fn take_payload<M: Any + Send>(self) -> M {
        unsafe {
            let hdr = self.ptr.as_ptr() as *mut Header;
            let payload_ptr = self.ptr.as_ptr().add((*hdr).payload_off);
            let m = std::ptr::read(payload_ptr as *const M);

            // drop 审计（修正 POC 的 mem::forget 双释放坑）：
            // 1. payload 已 read 走 → 块内无 M 需 drop
            // 2. reply（若存在）read 走 → 唤醒语义归调用方
            // 3. ManuallyDrop 防 Drop::drop 二次 dealloc
            let reply = (*hdr).reply.take();
            let layout = (*hdr).layout;
            let raw = self.ptr.as_ptr();
            let _ = std::mem::ManuallyDrop::new(self);
            // 显式释放整块；reply 已 take，不会被 Drop 触发
            if let Some(reply) = reply {
                // reply 未被消费：drop 之（发送取消信号给等待者）
                drop(reply);
            }
            dealloc(raw, layout);
            m
        }
    }

    /// 取走 ask 回复通道（消费侧拿到 oneshot::Sender 回填回复）。
    pub fn take_reply(
        &mut self,
    ) -> Option<oneshot::Sender<parrot_api::types::ActorResult<BoxedMessage>>> {
        unsafe {
            let hdr = self.ptr.as_ptr() as *mut Header;
            (*hdr).reply.take()
        }
    }

    /// 块总大小（header + padding + payload；诊断用）。
    pub fn block_size(&self) -> usize {
        unsafe { (*(self.ptr.as_ptr() as *const Header)).layout.size() }
    }
}

impl Drop for SingleAllocEnvelope {
    fn drop(&mut self) {
        unsafe {
            let hdr = self.ptr.as_ptr() as *mut Header;
            let payload_ptr = self.ptr.as_ptr().add((*hdr).payload_off);
            // drop 顺序审计：payload → oneshot → dealloc。
            // payload 的 Drop 可能执行任意用户代码（包括 panic），此时
            // 内存尚未释放——顺序保证 1/3 两步异常安全。
            ((*hdr).drop_fn)(payload_ptr);
            if let Some(reply) = (*hdr).reply.take() {
                drop(reply); // 2: oneshot drop 唤醒等待者（RecvError）
            }
            dealloc(self.ptr.as_ptr(), (*hdr).layout); // 3
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BigMsg {
        _data: [u64; 32], // 256B，确保跨对齐边界
        tag: u64,
    }

    #[test]
    fn single_alloc_roundtrip() {
        let env = SingleAllocEnvelope::new(BigMsg {
            _data: [0; 32],
            tag: 42,
        });
        assert_eq!(env.type_id(), TypeId::of::<BigMsg>());
        let m = env.downcast::<BigMsg>().ok().expect("type match");
        assert_eq!(m.tag, 42);
    }

    #[test]
    fn single_alloc_wrong_type_keeps_envelope() {
        let env = SingleAllocEnvelope::new(1234u64);
        let back = match env.downcast::<String>() {
            Ok(_) => panic!("must fail"),
            Err(back) => back,
        };
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
        assert_eq!(
            DROPS.load(Ordering::SeqCst),
            before + 1,
            "payload Drop 必须执行"
        );
    }

    #[test]
    fn ask_reply_channel_roundtrip() {
        let (mut env, rx) = SingleAllocEnvelope::new_ask(7u64);
        let reply = env.take_reply().expect("ask envelope has reply");
        reply
            .send(Ok(Box::new(99u64) as BoxedMessage))
            .expect("receiver alive");
        let m = env.downcast::<u64>().ok().unwrap();
        assert_eq!(m, 7);
        let result = rx.blocking_recv().unwrap().unwrap();
        assert_eq!(*result.downcast::<u64>().unwrap(), 99);
    }

    #[test]
    fn dropped_envelope_cancels_awaiter() {
        let (env, rx) = SingleAllocEnvelope::new_ask(String::from("x"));
        drop(env); // 未消费：payload drop + oneshot drop
        assert!(
            rx.blocking_recv().is_err(),
            "receiver must observe cancellation"
        );
    }

    #[test]
    fn zero_sized_payload() {
        let env = SingleAllocEnvelope::new(());
        assert_eq!(env.type_id(), TypeId::of::<()>());
        assert!(env.downcast::<()>().is_ok());
    }
}
