//! POC 4 补充：分配次数实测（计数全局分配器）。
//! 断言：
//!   SingleAllocEnvelope：构造恰好 1 次堆分配，downcast 零分配；
//!   Box<dyn Any> 基线（消息 Box + 信封 Box）：≥ 2 次。
//! 注意：两段断言在同一测试内顺序执行（共享计数器不可并行）。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

#[test]
fn alloc_count_single_vs_baseline() {
    struct Msg {
        _data: [u64; 31],
        tag: u64,
    }

    // ---- 段 1：SingleAllocEnvelope 恰好 1 次 ----
    {
        let warm = erased_alloc::SingleAllocEnvelope::new(Msg { _data: [0; 31], tag: 0 });
        drop(warm);
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    let before = ALLOCS.load(Ordering::SeqCst);

    let env = erased_alloc::SingleAllocEnvelope::new(Msg { _data: [0; 31], tag: 7 });
    let during = ALLOCS.load(Ordering::SeqCst);
    let m = env.downcast::<Msg>().ok().unwrap();
    let after = ALLOCS.load(Ordering::SeqCst);

    assert_eq!(m.tag, 7);
    assert_eq!(during - before, 1, "构造必须恰好 1 次堆分配，实际 {}", during - before);
    assert_eq!(after - during, 0, "downcast 必须零分配，实际 {}", after - during);
    std::thread::sleep(std::time::Duration::from_millis(20)); // 冷却，隔离后台噪音

    // ---- 段 2：Box<dyn Any> 基线 ≥ 2 次 ----
    {
        let warm: Box<dyn std::any::Any + Send> = Box::new(Msg { _data: [0; 31], tag: 0 });
        drop(warm);
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
    let before2 = ALLOCS.load(Ordering::SeqCst);

    let inner: Box<Msg> = Box::new(Msg { _data: [0; 31], tag: 7 }); // 分配 1：消息
    let envelope: Box<dyn std::any::Any + Send> = Box::new(inner); // 分配 2：信封擦除
    let during2 = ALLOCS.load(Ordering::SeqCst);
    drop(envelope);

    assert!(
        during2 - before2 >= 2,
        "基线（消息 Box + 信封 Box）应 ≥2 次分配，实际 {}",
        during2 - before2
    );
}
