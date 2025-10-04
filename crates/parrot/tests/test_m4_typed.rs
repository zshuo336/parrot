//! M4 静态类型轨集成测试（POC `typed-static` 三测试语义移植 + derive 宏用例）。
//!
//! 语义对照（POC → 正式版）：
//! - `static_track_zero_any` → `m4_static_track_ask_tell_zero_any`
//!   （静态 spawn + ask 累积语义 + tell 后 ask 可见 + 多协议 ref_for）
//! - `dual_track_bridge` → `m4_dual_track_bridge_into_dyn`
//!   （into_dyn 桥接：装箱只发生在桥接边界各一次）
//! - `dyn_track_baseline` → 动态轨不受影响（回归由存量 28 测试保证）
//! - 新增：derive 宏 `ParrotTypedActor` 枚举信封生成 + 单协议便捷入口

use std::sync::Arc;
use std::time::Duration;

use parrot::system::ParrotActorSystem;
use parrot::thread::config::ThreadActorSystemConfig;
use parrot::thread::system::ThreadActorSystem;
use parrot::thread::typed::{SingleVariant, TypedActorRef};
use parrot_api::actor::ActorState;
use parrot_api::message::Message;
use parrot_api::system::ActorSystemConfig;
use parrot_api::typed::TypedReceive;
use parrot_api::types::{ActorResult, BoxedFuture};

// ============ 消息协议 ============

pub struct Add(u64);
pub struct Get;
pub struct Ping(String);

impl Message for Add {
    type Result = u64;
}
impl Message for Get {
    type Result = u64;
}
impl Message for Ping {
    type Result = String;
}

// ============ 静态 actor（手写枚举信封路径） ============

#[derive(parrot_api::ParrotTypedActor)]
#[ParrotTypedActor(msgs(Add, Get, Ping))]
struct Calc {
    n: u64,
}

impl TypedReceive<Add> for Calc {
    fn receive_typed<'a>(&'a mut self, msg: Add) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move {
            self.n += msg.0;
            Ok(self.n)
        })
    }
}

impl TypedReceive<Get> for Calc {
    fn receive_typed<'a>(&'a mut self, _msg: Get) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move { Ok(self.n) })
    }
}

impl TypedReceive<Ping> for Calc {
    fn receive_typed<'a>(&'a mut self, msg: Ping) -> BoxedFuture<'a, ActorResult<String>> {
        Box::pin(async move { Ok(format!("{}:{}", msg.0, self.n)) })
    }
}

// ============ 测试系统搭建 ============

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

// ============ POC 三测试语义移植 ============

/// POC `static_track_zero_any`：静态 ask/tell 零 Any、多协议 ref_for。
#[tokio::test]
async fn m4_static_track_ask_tell_zero_any() {
    let ts = setup("m4-static").await;

    // 静态 spawn：一个 actor 一个通道，入口协议 Add
    let r: TypedActorRef<Calc, Add> = ts.spawn_typed(Calc { n: 0 }, "/m4/calc").await.unwrap();

    // 静态 ask：消息直接进类型化通道（M::inject → 枚举信封），
    // 回复经 extract 还原 u64——零 Any 装箱零 downcast
    assert_eq!(r.ask(Add(41)).await.unwrap(), 41, "0+41=41");
    assert_eq!(
        r.ask(Add(41)).await.unwrap(),
        82,
        "41+41=82（同一实例累积）"
    );

    // 多协议视图：同一 actor、同一通道（POC 的二次 spawn 妥协已消除）
    let get_r = r.ref_for::<Get>();
    assert_eq!(
        get_r.ask(Get).await.unwrap(),
        82,
        "Get 看到 Add 累积后的状态"
    );

    // 静态 tell：fire-and-forget
    r.tell(Add(1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        r.ask(Add(0)).await.unwrap(),
        83,
        "tell 后再 ask 应看到累积（Add(0) 不改变值）"
    );

    // 第三协议
    let ping_r = r.ref_for::<Ping>();
    assert_eq!(ping_r.ask(Ping("n".into())).await.unwrap(), "n:83");
}

/// POC `dual_track_bridge`：into_dyn 桥接，装箱只发生在边界各一次。
#[tokio::test]
async fn m4_dual_track_bridge_into_dyn() {
    let ts = setup("m4-bridge").await;

    let r: TypedActorRef<Calc, Add> = ts.spawn_typed(Calc { n: 0 }, "/m4/calc2").await.unwrap();

    // 桥接到动态轨 ActorRef 面
    let dyn_ref = r.into_dyn();

    // 动态轨 send：BoxedMessage → downcast M → 类型化通道 → 装回
    let out = dyn_ref
        .send(Box::new(Add(5)) as parrot_api::types::BoxedMessage)
        .await
        .unwrap();
    assert_eq!(*out.downcast::<u64>().unwrap(), 5);

    // 动态轨 deliver（tell 语义）
    dyn_ref
        .deliver(Box::new(Add(2)) as parrot_api::types::BoxedMessage)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let out = dyn_ref
        .send(Box::new(Add(0)) as parrot_api::types::BoxedMessage)
        .await
        .unwrap();
    assert_eq!(
        *out.downcast::<u64>().unwrap(),
        7,
        "deliver 的 Add(2) 已生效"
    );

    // 类型不匹配 → 错误（不 panic）
    let bad = dyn_ref
        .send(Box::new("str") as parrot_api::types::BoxedMessage)
        .await;
    assert!(bad.is_err(), "桥接边界类型不匹配应报错");
}

/// 单协议便捷入口：无需 derive，TypedReceive 即可 spawn。
#[tokio::test]
async fn m4_single_protocol_spawn() {
    let ts = setup("m4-single").await;

    struct Echo(u64);
    impl Message for Echo {
        type Result = u64;
    }

    struct Doubler;
    impl TypedReceive<Echo> for Doubler {
        fn receive_typed<'a>(&'a mut self, msg: Echo) -> BoxedFuture<'a, ActorResult<u64>> {
            Box::pin(async move { Ok(msg.0 * 2) })
        }
    }

    let r = ts.spawn_typed_single(Doubler, "/m4/doubler").await.unwrap();

    // 消息经 SingleVariant 包装（覆盖规则的本地绑定类型）
    assert_eq!(r.ask(SingleVariant::wrap(Echo(21))).await.unwrap(), 42);
    assert_eq!(r.ask(SingleVariant::wrap(Echo(0))).await.unwrap(), 0);
}

/// derive 宏生成的枚举信封分派覆盖全部声明消息。
#[tokio::test]
async fn m4_derive_envelope_dispatches_all_variants() {
    let ts = setup("m4-derive").await;

    let r: TypedActorRef<Calc, Add> = ts.spawn_typed(Calc { n: 10 }, "/m4/calc3").await.unwrap();

    // 三协议都经各自的 ParrotMsgVariant 注入同一枚举信封
    assert_eq!(r.ask(Add(5)).await.unwrap(), 15);
    assert_eq!(r.ref_for::<Get>().ask(Get).await.unwrap(), 15);
    assert_eq!(
        r.ref_for::<Ping>().ask(Ping("v".into())).await.unwrap(),
        "v:15"
    );
}

/// 静态轨与动态轨并存互不干扰（双轨编程模型）。
#[tokio::test]
async fn m4_dual_track_coexist() {
    let ts = setup("m4-coexist").await;

    // 静态轨 actor
    let typed_r: TypedActorRef<Calc, Add> =
        ts.spawn_typed(Calc { n: 100 }, "/m4/typed").await.unwrap();

    // 动态轨 actor（存量路径，完全不动）
    struct DynEcho;
    impl parrot_api::actor::Actor for DynEcho {
        type Config = parrot_api::actor::EmptyConfig;
        type Context = parrot::thread::context::ThreadContext<Self>;

        fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(
            &'a mut self,
            msg: parrot_api::types::BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
            Box::pin(async move {
                if let Some(v) = msg.downcast_ref::<u64>() {
                    Ok(Box::new(v + 1) as parrot_api::types::BoxedMessage)
                } else {
                    Ok(msg)
                }
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    let dyn_r = ts
        .spawn_root_typed_thread(DynEcho, parrot_api::actor::EmptyConfig)
        .await
        .unwrap();

    // 两轨同时工作
    assert_eq!(typed_r.ask(Add(1)).await.unwrap(), 101);
    let out = dyn_r.send_msg(Box::new(41u64)).await;
    // 动态轨：send_msg 是 tell 语义（fire-and-forget），不等回复
    let _ = out;

    // 静态轨继续工作（动态轨活动不影响静态轨）
    assert_eq!(typed_r.ask(Add(1)).await.unwrap(), 102);
}

// ============ 静态轨 vs 动态轨吞吐压测（release-only） ============

/// M4 量化收益：同语义 ask 往返，静态轨（零 Any）vs 动态轨
/// （Box + downcast + AskEnvelope）。宽松下限防倒退；精确数字记入
/// `docs/PERF_BASELINE.md`。与 `stress_baseline.rs` 同款结构。
#[test]
fn m4_static_vs_dynamic_throughput() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ts = setup("m4-bench").await;
        const N: u64 = 5_000;

        // ---- 静态轨 ----
        let r: TypedActorRef<Calc, Add> = ts
            .spawn_typed(Calc { n: 0 }, "/m4/bench-typed")
            .await
            .unwrap();
        // warmup
        for _ in 0..200 {
            r.ask(Add(1)).await.unwrap();
        }
        let t0 = std::time::Instant::now();
        for i in 0..N {
            r.ask(Add(1)).await.unwrap();
            debug_assert_eq!(r.ref_for::<Get>().ask(Get).await.unwrap(), 200 + i + 1);
        }
        let typed_elapsed = t0.elapsed();

        // ---- 动态轨（同语义 echo：收到 u64 回 u64） ----
        struct DynEcho;
        impl parrot_api::actor::Actor for DynEcho {
            type Config = parrot_api::actor::EmptyConfig;
            type Context = parrot::thread::context::ThreadContext<Self>;
            fn init<'a>(
                &'a mut self,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async { Ok(()) })
            }
            fn receive_message<'a>(
                &'a mut self,
                msg: parrot_api::types::BoxedMessage,
                _ctx: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<parrot_api::types::BoxedMessage>> {
                Box::pin(async move {
                    if let Some(v) = msg.downcast_ref::<u64>() {
                        Ok(Box::new(*v) as parrot_api::types::BoxedMessage)
                    } else {
                        Ok(msg)
                    }
                })
            }
            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }

        let dyn_r = ts
            .spawn_root_typed_thread(DynEcho, parrot_api::actor::EmptyConfig)
            .await
            .unwrap();
        // warmup
        for _ in 0..200 {
            dyn_r.ask(Box::new(1u64)).await.unwrap();
        }
        let t0 = std::time::Instant::now();
        for _ in 0..N {
            dyn_r.ask(Box::new(1u64)).await.unwrap();
        }
        let dyn_elapsed = t0.elapsed();

        println!(
            "m4 ask roundtrip (N={}): static={:?} dynamic={:?} ratio={:.2}x",
            N,
            typed_elapsed,
            dyn_elapsed,
            dyn_elapsed.as_secs_f64() / typed_elapsed.as_secs_f64()
        );

        // 宽松下限：静态轨 5k 次 ask 应在 5s 内（release 实测量级 ~0.1s）
        assert!(
            typed_elapsed < std::time::Duration::from_secs(5),
            "static track too slow: {:?}",
            typed_elapsed
        );

        // ---- tell 吞吐对比（分配敏感：静态轨零装箱 vs 动态轨每条 Box） ----
        const M: u64 = 50_000;
        let t0 = std::time::Instant::now();
        for _ in 0..M {
            r.tell(Add(1)).await.unwrap();
        }
        let typed_tell = t0.elapsed();

        let t0 = std::time::Instant::now();
        for _ in 0..M {
            let _ = dyn_r.send_msg(Box::new(1u64)).await;
        }
        let dyn_tell = t0.elapsed();

        println!(
            "m4 tell throughput (N={}): static={:?} ({:.0} msg/s) dynamic={:?} ({:.0} msg/s) ratio={:.2}x",
            M,
            typed_tell,
            M as f64 / typed_tell.as_secs_f64(),
            dyn_tell,
            M as f64 / dyn_tell.as_secs_f64(),
            dyn_tell.as_secs_f64() / typed_tell.as_secs_f64()
        );
    });
}
