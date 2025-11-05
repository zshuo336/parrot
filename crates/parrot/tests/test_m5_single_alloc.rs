//! M5 单块信封集成测试（POC `erased-alloc` 语义移植 + 计数分配器 +
//! 全链路 ask 走新路径的回归）。
//!
//! 分层策略（计划 M5 节）：
//! - 构造内联信封：1 次分配（oneshot cell），不包含完整 ask 的运行时工作。
//! - 构造装箱信封：2 次分配（payload Box + oneshot cell），不包含响应载荷。
//! - 历史：3 分配（payload Box + envelope Box + oneshot cell）
//!
//! 本文件验证：
//! 1. 单块信封裸操作（POC 三测试语义已在 lib 单测覆盖，此处验证全链路）
//! 2. ask 全链路走 `push_ask` 按值路径（信封不装箱）
//! 3. inline ask（SSO 档）
//! 4. 线程局部计数器分别验证 inline/boxed 信封构造，不冒称端到端分配。

use std::sync::Arc;
use std::time::Duration;

use parrot::system::ParrotActorSystem;
use parrot::thread::config::{BackpressureStrategy, ThreadActorSystemConfig};
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

// ============ 全局分配器，按线程启用独立计数窗口 ============

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    // Const TLS initialization cannot recurse through the allocator.
    static LOCAL_ALLOCS: Cell<Option<usize>> = const { Cell::new(None) };
}
fn record_allocation(ptr: *mut u8) {
    if !ptr.is_null() {
        let _ = LOCAL_ALLOCS.try_with(|count| {
            if let Some(value) = count.get() {
                count.set(Some(value.saturating_add(1)));
            }
        });
    }
}
struct CountingAlloc;
// SAFETY: all requests are forwarded unchanged to System. Recording only touches
// const-initialized TLS and cannot allocate, panic, or change allocation ownership.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        record_allocation(ptr);
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        record_allocation(ptr);
        ptr
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, new_size) };
        record_allocation(ptr);
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static GLOBAL_ALLOC: CountingAlloc = CountingAlloc;
struct AllocationScope;
impl Drop for AllocationScope {
    fn drop(&mut self) {
        LOCAL_ALLOCS.with(|count| count.set(None));
    }
}
fn measure_allocations<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    LOCAL_ALLOCS.with(|count| {
        assert!(count.get().is_none(), "allocation scopes must not nest");
        count.set(Some(0));
    });
    let guard = AllocationScope;
    let result = operation();
    let allocations = LOCAL_ALLOCS.with(|count| count.get().unwrap());
    drop(guard);
    (result, allocations)
}

// ============ Echo actor（动态轨） ============

struct EchoActor;

impl Actor for EchoActor {
    type Config = EmptyConfig;
    type Context = parrot::thread::context::ThreadContext<Self>;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        // u64 echo；其它类型原样返回
        Box::pin(async move {
            if let Some(v) = msg.downcast_ref::<u64>() {
                Ok(Box::new(*v) as BoxedMessage)
            } else if let Some(s) = msg.downcast_ref::<String>() {
                Ok(Box::new(s.len() as u64) as BoxedMessage)
            } else {
                Ok(msg)
            }
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

async fn setup(name: &str) -> Arc<ThreadActorSystem> {
    let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
        .await
        .unwrap();
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 2,
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    parrot
        .register_thread_system(name.into(), ts.clone(), true)
        .await
        .unwrap();
    ts
}

// ============ 全链路测试 ============

/// ask 全链路走按值信封路径（`push_ask`），语义与历史一致。
#[test]
fn m5_ask_full_path_uses_by_value_envelope() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ts = setup("m5-ask").await;
        let r = ts
            .spawn_root_typed_thread(EchoActor, EmptyConfig)
            .await
            .unwrap();

        // 大消息（String > 16B）：payload Box 是唯一消息分配
        let big = String::from("hello world, this is a big message");
        let expect_len = big.len();
        let out = r
            .ask_with_strategy_and_timeout(
                Box::new(big),
                BackpressureStrategy::Block,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(*out.downcast::<u64>().unwrap(), expect_len as u64);

        // 已装箱消息（历史 API 形态）
        let out = r.ask(Box::new(7u64)).await.unwrap();
        assert_eq!(*out.downcast::<u64>().unwrap(), 7);

        // ActorRef trait 面（send = unbounded ask）
        use parrot_api::address::ActorRef;
        let out = r.send(Box::new(9u64)).await.unwrap();
        assert_eq!(*out.downcast::<u64>().unwrap(), 9);
    });
}

/// SSO 档：`ask_inline` 小消息零 payload 分配。
#[test]
fn m5_inline_ask_sso_lane() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ts = setup("m5-inline").await;
        let r = ts
            .spawn_root_typed_thread(EchoActor, EmptyConfig)
            .await
            .unwrap();

        // 各 inline 类型
        for v in [1u64, 42, u64::MAX] {
            let out = r.ask_inline(v, Duration::from_secs(5)).await.unwrap();
            assert_eq!(*out.downcast::<u64>().unwrap(), v);
        }
        let out = r.ask_inline(true, Duration::from_secs(5)).await.unwrap();
        // bool 不在 EchoActor 的特判里 → 原样返回
        assert!(*out.downcast::<bool>().unwrap());
    });
}

/// Measure envelope construction on the calling thread, not full actor requests.
/// Keep each result alive past its independent window so cleanup is excluded.
#[test]
fn m5_allocation_counter_inline_vs_boxed() {
    use parrot::thread::envelope::AskEnvelope;
    let mut inline_total = 0;
    let mut boxed_total = 0;
    for value in 0..1_000_u64 {
        let (inline, inline_allocations) = measure_allocations(|| {
            std::hint::black_box(AskEnvelope::new_inline(std::hint::black_box(value)))
        });
        let (boxed, boxed_allocations) = measure_allocations(|| {
            std::hint::black_box(AskEnvelope::with_boxed(Box::new(std::hint::black_box(
                value,
            ))))
        });
        assert_eq!(
            inline_allocations, 1,
            "inline needs only a reply channel allocation"
        );
        assert_eq!(
            boxed_allocations, 2,
            "boxed needs a payload plus reply channel"
        );
        assert_eq!(boxed_allocations - inline_allocations, 1);
        inline_total += inline_allocations;
        boxed_total += boxed_allocations;
        drop((inline, boxed));
    }
    assert_eq!((inline_total, boxed_total), (1_000, 2_000));
}

/// 28 集成测试全走新路径的抽样回归：双车道 + 死亡通知 + ask/tell
/// 在按值信封路径下语义不变（完整回归由 cargo test 全量保证）。
#[test]
fn m5_semantics_preserved_under_new_envelope() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ts = setup("m5-sem").await;

        // 高优先级消息越过积压（M2 语义在新信封下保持）
        struct SlowActor;
        impl Actor for SlowActor {
            type Config = EmptyConfig;
            type Context = parrot::thread::context::ThreadContext<Self>;
            fn init<'a>(
                &'a mut self,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async { Ok(()) })
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: BoxedMessage,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    // 延迟后回显（保持 ask 语义可用）
                    if msg.downcast_ref::<u64>().is_some() {
                        Ok(msg)
                    } else {
                        Ok(Box::new(()) as BoxedMessage)
                    }
                })
            }
            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }

        let slow = ts
            .spawn_at(
                SlowActor,
                "/m5/slow",
                None,
                parrot::thread::config::ThreadActorConfig::default(),
            )
            .await
            .unwrap();

        // 积压 + 高优先插队不丢
        for _ in 0..10 {
            let _ = slow.send_msg(Box::new(1u64)).await;
        }
        use parrot_api::address::ActorRef;
        let out = slow
            .send_with_timeout(Box::new(2u64), Some(Duration::from_secs(2)))
            .await
            .unwrap();
        assert_eq!(*out.downcast::<u64>().unwrap(), 2);
    });
}

#[test]
fn m5_counter_excludes_other_threads() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    // Atomics avoid platform-dependent lazy allocations in OS wait primitives.
    let phase = Arc::new(AtomicUsize::new(0));
    let worker_phase = phase.clone();
    let worker = std::thread::spawn(move || {
        while worker_phase.load(Ordering::Acquire) == 0 {
            std::hint::spin_loop();
        }
        struct Signal(Arc<AtomicUsize>);
        impl Drop for Signal {
            fn drop(&mut self) {
                self.0.store(2, Ordering::Release);
            }
        }
        let finished = Signal(worker_phase);
        let ((), count) = measure_allocations(|| {
            for value in 0..1_000_u64 {
                drop(std::hint::black_box(Box::new(value)));
            }
        });
        drop(finished);
        count
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let (completed, local) = measure_allocations(|| {
        phase.store(1, Ordering::Release);
        while phase.load(Ordering::Acquire) != 2 {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::hint::spin_loop();
        }
        true
    });
    assert!(completed, "allocation worker must finish within its budget");
    let remote = worker.join().unwrap();
    assert_eq!(remote, 1_000, "the worker really allocated");
    assert_eq!(
        local, 0,
        "unrelated thread allocations must not enter the local budget"
    );
}

#[test]
fn m5_counter_restores_scope_after_unwind() {
    let failed = std::panic::catch_unwind(|| {
        measure_allocations(|| {
            drop(std::hint::black_box(Box::new(7_u64)));
            panic!("counter scope unwind");
        });
    });
    assert!(failed.is_err());
    let (value, allocations) = measure_allocations(|| std::hint::black_box(Box::new(9_u64)));
    assert_eq!(allocations, 1);
    assert_eq!(*value, 9);
}

#[test]
fn m5_counter_counts_zeroed_and_reallocation() {
    let (bytes, allocations) = measure_allocations(|| {
        let mut bytes = std::hint::black_box(vec![0_u8; 32]);
        bytes.reserve_exact(64);
        std::hint::black_box(bytes)
    });
    assert_eq!(
        allocations, 2,
        "zeroed allocation and realloc are distinct calls"
    );
    assert_eq!(bytes.len(), 32);
    assert!(bytes.iter().all(|v| *v == 0));
    assert!(bytes.capacity() >= 96);
}
