//! 第十二轮综合矩阵压测：消息轨道 × 消息大小 × 分配路径 × 通信模式。
//!
//! 矩阵维度：
//! - **轨道**：动态轨（`BoxedMessage` 擦除）vs 静态轨（M4 typed 枚举信封）
//! - **消息大小**：inline ≤16B（1 分配 oneshot）/ boxed u64（2 分配）/
//!   64B / 1KB / 64KB（均 2 分配，payload 大小阶梯）
//! - **通信模式**：串行 ask / c8 并发 ask / tell（纯投递）/ 大小混合流
//! - **分配路径**：inline SSO（1 alloc）vs boxed（2 alloc）vs typed（0 alloc）
//!
//! 关键对照点：
//! - A2 vs A1：同 u64 消息，boxed vs inline 的分配差价
//! - A4 vs C3：同 1KB 消息，动态轨（传 Box 指针，2 alloc + 0 copy）
//!   vs 静态轨（按值进枚举槽，0 alloc + 1KB copy/条）
//! - F1：混合协议枚举槽位膨胀效应（小消息付大槽 copy 代价）
//! - E 系列：计数分配器差分法量化每条消息全链路分配次数
//!
//! 运行（必须串行 + release）：
//! `cargo test -p parrot --release --test bench_msg_matrix -- --ignored --nocapture --test-threads=1`

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parrot::thread::config::ThreadActorSystemConfig;
use parrot::thread::system::ThreadActorSystem;
use parrot::thread::typed::TypedActorRef;
use parrot_api::ParrotTypedActor;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::message::Message;
use parrot_api::typed::TypedReceive;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

// ============ 计数分配器（E 系列；--test-threads=1 下无并行污染） ============

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL_ALLOC: CountingAlloc = CountingAlloc;

// ============ 消息类型 ============

/// 动态轨大消息阶梯（定长数组：Box 分配大小可控、无 Vec 双分配）。
#[derive(Clone)]
pub struct Msg64(pub [u8; 64]);
#[derive(Clone)]
pub struct Msg1K(pub [u8; 1024]);
#[derive(Clone)]
pub struct Msg64K(pub Box<[u8; 65536]>);

// ---- 静态轨消息（单协议） ----

#[derive(Clone, Copy, Debug)]
pub struct PingS(pub u64);
impl Message for PingS {
    type Result = u64;
}

#[derive(Clone, Debug)]
pub struct PingM(pub [u8; 64]);
impl Message for PingM {
    type Result = u64;
}

#[derive(Clone, Debug)]
pub struct PingL(pub [u8; 1024]);
impl Message for PingL {
    type Result = u64;
}

// ============ 动态轨 actor ============

struct DynEcho;

impl Actor for DynEcho {
    type Config = EmptyConfig;
    type Context = parrot::thread::context::ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(v) = m.downcast_ref::<u64>() {
                Ok(Box::new(*v) as BoxedMessage)
            } else if let Some(x) = m.downcast_ref::<Msg64>() {
                Ok(Box::new(x.0[0] as u64) as BoxedMessage)
            } else if let Some(x) = m.downcast_ref::<Msg1K>() {
                Ok(Box::new(x.0[0] as u64) as BoxedMessage)
            } else if let Some(x) = m.downcast_ref::<Msg64K>() {
                Ok(Box::new(x.0[0] as u64) as BoxedMessage)
            } else {
                Ok(m)
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

// ============ 静态轨 actor（单协议 × 3） ============

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(PingS))]
struct EchoS;
impl TypedReceive<PingS> for EchoS {
    fn receive_typed<'a>(&'a mut self, msg: PingS) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move { Ok(msg.0) })
    }
}

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(PingM))]
struct EchoM;
impl TypedReceive<PingM> for EchoM {
    fn receive_typed<'a>(&'a mut self, msg: PingM) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move { Ok(msg.0[0] as u64) })
    }
}

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(PingL))]
struct EchoL;
impl TypedReceive<PingL> for EchoL {
    fn receive_typed<'a>(&'a mut self, msg: PingL) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move { Ok(msg.0[0] as u64) })
    }
}

// ============ 静态轨混合协议 actor（F1：枚举槽位膨胀） ============

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(PingS, PingL))]
struct EchoMixed;
impl TypedReceive<PingS> for EchoMixed {
    fn receive_typed<'a>(&'a mut self, msg: PingS) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move { Ok(msg.0) })
    }
}
impl TypedReceive<PingL> for EchoMixed {
    fn receive_typed<'a>(&'a mut self, msg: PingL) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move { Ok(msg.0[0] as u64) })
    }
}

// ============ 基建 ============

async fn sys_default() -> Arc<ThreadActorSystem> {
    ThreadActorSystem::shared(ThreadActorSystemConfig::default())
}

fn row(name: &str, msgs: u64, wall: std::time::Duration, extra: &str) {
    let tput = msgs as f64 / wall.as_secs_f64();
    println!(
        "[matrix] {name} | msgs={msgs} | wall={:.3}s | tput={tput:.0}/s | {extra}",
        wall.as_secs_f64()
    );
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap()
}

const TMO: Duration = Duration::from_secs(30);

// ============ A 系列：动态轨 ask 大小阶梯（串行） ============

#[test]
#[ignore = "release-only 综合矩阵（--test-threads=1）"]
fn matrix_a_dyn_ask_size_ladder() {
    rt().block_on(async {
        let ts = sys_default().await;
        const N: u64 = 10_000;

        // A1 inline u64（1 分配：oneshot）
        let r = ts
            .spawn_root_typed_thread(DynEcho, EmptyConfig)
            .await
            .unwrap();
        for _ in 0..500 {
            let _ = r.ask_inline(1u64, TMO).await.unwrap();
        }
        let t = Instant::now();
        for i in 0..N {
            let out = r.ask_inline(i, TMO).await.unwrap();
            assert_eq!(*out.downcast::<u64>().unwrap(), i);
        }
        row("A1 dyn-ask-inline-u64", N, t.elapsed(), "1 alloc (oneshot)");

        // A2 boxed u64（2 分配：payload Box + oneshot）
        for _ in 0..500 {
            let _ = r.ask(Box::new(1u64)).await.unwrap();
        }
        let t = Instant::now();
        for i in 0..N {
            let out = r.ask(Box::new(i)).await.unwrap();
            assert_eq!(*out.downcast::<u64>().unwrap(), i);
        }
        row("A2 dyn-ask-boxed-u64", N, t.elapsed(), "2 alloc (Box+oneshot)");

        // A3 boxed 64B
        let m64 = Msg64([7u8; 64]);
        for _ in 0..500 {
            let _ = r.ask(Box::new(m64.clone())).await.unwrap();
        }
        let t = Instant::now();
        for _ in 0..N {
            let out = r.ask(Box::new(m64.clone())).await.unwrap();
            assert_eq!(*out.downcast::<u64>().unwrap(), 7);
        }
        row("A3 dyn-ask-boxed-64B", N, t.elapsed(), "2 alloc, 64B payload");

        // A4 boxed 1KB
        let m1k = Msg1K([7u8; 1024]);
        for _ in 0..500 {
            let _ = r.ask(Box::new(m1k.clone())).await.unwrap();
        }
        let t = Instant::now();
        for _ in 0..N {
            let out = r.ask(Box::new(m1k.clone())).await.unwrap();
            assert_eq!(*out.downcast::<u64>().unwrap(), 7);
        }
        row("A4 dyn-ask-boxed-1KB", N, t.elapsed(), "2 alloc, 1KB payload");

        // A5 boxed 64KB
        let m64k = Msg64K(Box::new([7u8; 65536]));
        for _ in 0..200 {
            let _ = r.ask(Box::new(m64k.clone())).await.unwrap();
        }
        const N5: u64 = 2_000;
        let t = Instant::now();
        for _ in 0..N5 {
            let out = r.ask(Box::new(m64k.clone())).await.unwrap();
            assert_eq!(*out.downcast::<u64>().unwrap(), 7);
        }
        row("A5 dyn-ask-boxed-64KB", N5, t.elapsed(), "2 alloc, 64KB payload");

        let _ = ts.shutdown_internal().await;
    });
}

// ============ B 系列：动态轨 tell（纯投递）大小阶梯 ============

#[test]
#[ignore = "release-only 综合矩阵（--test-threads=1）"]
fn matrix_b_dyn_tell_size_ladder() {
    rt().block_on(async {
        use parrot_api::address::ActorRef;
        let ts = sys_default().await;
        const N: u64 = 100_000;

        let r = ts
            .spawn_root_typed_thread(DynEcho, EmptyConfig)
            .await
            .unwrap();

        // B1 u64
        for _ in 0..1000 {
            let _ = r.deliver(Box::new(1u64)).await;
        }
        let t = Instant::now();
        for _ in 0..N {
            let _ = r.deliver(Box::new(1u64)).await;
        }
        row("B1 dyn-tell-u64", N, t.elapsed(), "1 alloc (payload Box)");

        // B2 64B
        let m64 = Msg64([7u8; 64]);
        let t = Instant::now();
        for _ in 0..N {
            let _ = r.deliver(Box::new(m64.clone())).await;
        }
        row("B2 dyn-tell-64B", N, t.elapsed(), "1 alloc, 64B");

        // B3 1KB
        let m1k = Msg1K([7u8; 1024]);
        let t = Instant::now();
        for _ in 0..N {
            let _ = r.deliver(Box::new(m1k.clone())).await;
        }
        row("B3 dyn-tell-1KB", N, t.elapsed(), "1 alloc, 1KB");

        let _ = ts.shutdown_internal().await;
    });
}

// ============ C 系列：静态轨 ask 大小阶梯（对照 A2-A4） ============

#[test]
#[ignore = "release-only 综合矩阵（--test-threads=1）"]
fn matrix_c_static_ask_size_ladder() {
    rt().block_on(async {
        let ts = sys_default().await;
        const N: u64 = 10_000;

        // C1 static u64（枚举槽 ~24B，0 alloc）
        let rs: TypedActorRef<EchoS, PingS> =
            ts.spawn_typed(EchoS, "/mx/echo-s").await.unwrap();
        for _ in 0..500 {
            let _ = rs.ask(PingS(1)).await.unwrap();
        }
        let t = Instant::now();
        for i in 0..N {
            assert_eq!(rs.ask(PingS(i)).await.unwrap(), i);
        }
        row("C1 static-ask-u64", N, t.elapsed(), "0 alloc, enum slot ~24B");

        // C2 static 64B
        let rm: TypedActorRef<EchoM, PingM> =
            ts.spawn_typed(EchoM, "/mx/echo-m").await.unwrap();
        let pm = PingM([7u8; 64]);
        for _ in 0..500 {
            let _ = rm.ask(pm.clone()).await.unwrap();
        }
        let t = Instant::now();
        for _ in 0..N {
            assert_eq!(rm.ask(pm.clone()).await.unwrap(), 7);
        }
        row("C2 static-ask-64B", N, t.elapsed(), "0 alloc, slot ~72B (copy)");

        // C3 static 1KB（枚举槽 1KB+：每条按值 copy 1KB 进 flume 槽）
        let rl: TypedActorRef<EchoL, PingL> =
            ts.spawn_typed(EchoL, "/mx/echo-l").await.unwrap();
        let pl = PingL([7u8; 1024]);
        for _ in 0..500 {
            let _ = rl.ask(pl.clone()).await.unwrap();
        }
        let t = Instant::now();
        for _ in 0..N {
            assert_eq!(rl.ask(pl.clone()).await.unwrap(), 7);
        }
        row("C3 static-ask-1KB", N, t.elapsed(), "0 alloc, slot ~1KB (copy)");

        let _ = ts.shutdown_internal().await;
    });
}

// ============ D 系列：c8 并发矩阵 ============

#[test]
#[ignore = "release-only 综合矩阵（--test-threads=1）"]
fn matrix_d_concurrent_c8() {
    rt().block_on(async {
        let ts = sys_default().await;
        const PER: u64 = 10_000; // × 8 task
        const TOTAL: u64 = PER * 8;

        // D1 dyn inline c8
        let r = Arc::new(
            ts.spawn_root_typed_thread(DynEcho, EmptyConfig)
                .await
                .unwrap(),
        );
        for _ in 0..500 {
            let _ = r.ask_inline(1u64, TMO).await.unwrap();
        }
        let t = Instant::now();
        let mut hs = Vec::new();
        for c in 0..8u64 {
            let r = r.clone();
            hs.push(tokio::spawn(async move {
                for i in 0..PER {
                    let out = r.ask_inline(i + c, TMO).await.unwrap();
                    assert_eq!(*out.downcast::<u64>().unwrap(), i + c);
                }
            }));
        }
        for h in hs {
            h.await.unwrap();
        }
        row("D1 dyn-ask-inline-c8", TOTAL, t.elapsed(), "8 tasks × 10k");

        // D2 dyn boxed 1KB c8
        let m1k = Msg1K([7u8; 1024]);
        let t = Instant::now();
        let mut hs = Vec::new();
        for _ in 0..8 {
            let r = r.clone();
            let m = m1k.clone();
            hs.push(tokio::spawn(async move {
                for _ in 0..PER {
                    let out = r.ask(Box::new(m.clone())).await.unwrap();
                    assert_eq!(*out.downcast::<u64>().unwrap(), 7);
                }
            }));
        }
        for h in hs {
            h.await.unwrap();
        }
        row("D2 dyn-ask-boxed-1KB-c8", TOTAL, t.elapsed(), "8 tasks × 10k × 1KB");

        // D3 static 1KB c8
        let rl = Arc::new(
            ts.spawn_typed::<EchoL, PingL>(EchoL, "/mx/c8-l")
                .await
                .unwrap(),
        );
        let pl = PingL([7u8; 1024]);
        for _ in 0..500 {
            let _ = rl.ask(pl.clone()).await.unwrap();
        }
        let t = Instant::now();
        let mut hs = Vec::new();
        for _ in 0..8 {
            let r = rl.clone();
            let m = pl.clone();
            hs.push(tokio::spawn(async move {
                for _ in 0..PER {
                    assert_eq!(r.ask(m.clone()).await.unwrap(), 7);
                }
            }));
        }
        for h in hs {
            h.await.unwrap();
        }
        row("D3 static-ask-1KB-c8", TOTAL, t.elapsed(), "8 tasks × 10k × 1KB slot");

        let _ = ts.shutdown_internal().await;
    });
}

// ============ E 系列：计数分配器全链路差分 ============

#[test]
#[ignore = "release-only 综合矩阵（--test-threads=1）"]
fn matrix_e_alloc_counting() {
    rt().block_on(async {
        use parrot_api::address::ActorRef;
        let ts = sys_default().await;
        const N: usize = 2_000;

        fn allocs_delta(before: usize) -> f64 {
            (ALLOCS.load(Ordering::Relaxed) - before) as f64
        }

        // E1 dyn inline ask（理论：oneshot 1 + 消费侧还原 Box 1 + 回复 Box 1 = 3）
        let r = ts
            .spawn_root_typed_thread(DynEcho, EmptyConfig)
            .await
            .unwrap();
        for _ in 0..300 {
            let _ = r.ask_inline(1u64, TMO).await.unwrap();
        }
        let b = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..N {
            let _ = r.ask_inline(7u64, TMO).await.unwrap();
        }
        let d = allocs_delta(b) / N as f64;
        row("E1 dyn-ask-inline", N as u64, Duration::from_secs(0), &format!("{d:.2} allocs/msg 全链路"));

        // E2 dyn boxed u64 ask（理论：payload Box 1 + oneshot 1 + 回复 Box 1 = 3）
        for _ in 0..300 {
            let _ = r.ask(Box::new(1u64)).await.unwrap();
        }
        let b = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..N {
            let _ = r.ask(Box::new(7u64)).await.unwrap();
        }
        let d = allocs_delta(b) / N as f64;
        row("E2 dyn-ask-boxed-u64", N as u64, Duration::from_secs(0), &format!("{d:.2} allocs/msg 全链路"));

        // E3 static ask u64（理论：reply channel 构造分配 + 槽位摊销 ≈ 1-2）
        let rs: TypedActorRef<EchoS, PingS> =
            ts.spawn_typed(EchoS, "/mx/e-s").await.unwrap();
        for _ in 0..300 {
            let _ = rs.ask(PingS(1)).await.unwrap();
        }
        let b = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..N {
            let _ = rs.ask(PingS(7)).await.unwrap();
        }
        let d = allocs_delta(b) / N as f64;
        row("E3 static-ask-u64", N as u64, Duration::from_secs(0), &format!("{d:.2} allocs/msg 全链路"));

        // E4 dyn tell（理论：payload Box 1 = 1）
        for _ in 0..300 {
            let _ = r.deliver(Box::new(1u64)).await;
        }
        let b = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..N {
            let _ = r.deliver(Box::new(1u64)).await;
        }
        let d = allocs_delta(b) / N as f64;
        row("E4 dyn-tell-u64", N as u64, Duration::from_secs(0), &format!("{d:.2} allocs/msg 全链路"));

        // E5 static tell（理论：0 —— flume 槽预分配按值入队）
        for _ in 0..300 {
            rs.tell(PingS(1)).await.unwrap();
        }
        let b = ALLOCS.load(Ordering::Relaxed);
        for _ in 0..N {
            rs.tell(PingS(1)).await.unwrap();
        }
        let d = allocs_delta(b) / N as f64;
        row("E5 static-tell", N as u64, Duration::from_secs(0), &format!("{d:.2} allocs/msg 全链路"));

        let _ = ts.shutdown_internal().await;
    });
}

// ============ F 系列：组合场景 ============

#[test]
#[ignore = "release-only 综合矩阵（--test-threads=1）"]
fn matrix_f_combo() {
    rt().block_on(async {
        use parrot_api::address::ActorRef;
        let ts = sys_default().await;
        const N: u64 = 10_000;

        // F1 混合协议枚举槽位膨胀：同一 actor 协议 {PingS, PingL}，
        // 枚举槽 = max(24B, 1KB+) → 小消息也付 1KB copy 代价。
        // 对照 C1（单协议小槽）与 C3（单协议大槽）。
        let rm: TypedActorRef<EchoMixed, PingS> =
            ts.spawn_typed(EchoMixed, "/mx/mixed").await.unwrap();
        for _ in 0..500 {
            let _ = rm.ask(PingS(1)).await.unwrap();
        }
        let t = Instant::now();
        for i in 0..N {
            assert_eq!(rm.ask(PingS(i)).await.unwrap(), i);
        }
        row("F1 static-ask-SMALL-in-MIXED-enum", N, t.elapsed(), "PingS via {S,L} enum (slot 1KB)");

        // 大消息协议经 ref_for 验证（对照 C3：同槽大小应持平）
        let rl: TypedActorRef<EchoMixed, PingL> = rm.ref_for::<PingL>();
        let pl = PingL([7u8; 1024]);
        for _ in 0..500 {
            let _ = rl.ask(pl.clone()).await.unwrap();
        }
        let t = Instant::now();
        for _ in 0..N {
            assert_eq!(rl.ask(pl.clone()).await.unwrap(), 7);
        }
        row("F1b static-ask-LARGE-in-MIXED-enum", N, t.elapsed(), "PingL via {S,L} enum (slot 1KB)");

        // F2 动态轨大小混合流：95% u64 + 5% 64KB 洪泛（N=20k deliver）
        let r = ts
            .spawn_root_typed_thread(DynEcho, EmptyConfig)
            .await
            .unwrap();
        let m64k = Msg64K(Box::new([7u8; 65536]));
        const N2: u64 = 20_000;
        for _ in 0..500 {
            let _ = r.deliver(Box::new(1u64)).await;
        }
        let t = Instant::now();
        for i in 0..N2 {
            if i % 20 == 0 {
                let _ = r.deliver(Box::new(m64k.clone())).await;
            } else {
                let _ = r.deliver(Box::new(1u64)).await;
            }
        }
        row("F2 dyn-tell-mixed-95small-5big", N2, t.elapsed(), "95% u64 + 5% 64KB");

        let _ = ts.shutdown_internal().await;
    });
}
