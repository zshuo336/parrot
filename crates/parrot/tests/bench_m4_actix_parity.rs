//! M4 性能对标：parrot 静态类型轨（actix 引擎）vs 纯 actix 原生。
//!
//! M4 设计目标：**静态轨性能与纯 actix 持平或更好**。本 bench 用
//! 同一语义（echo ask 往返）对比：
//!
//! - **纯 actix**：原生 `Actor` + `Handler<Msg>` + `Addr::send`
//!   （actix Envelope 装箱路径，无 parrot 层）
//! - **parrot 静态轨**：`spawn_typed` 类型化通道（零 Any、零 Envelope）
//!
//! 断言策略（release-only，`#[ignore]` + CI release 补跑）：
//! 静态轨单消息往返延迟 ≤ 纯 actix × 1.15（容忍调度抖动）。
//! debug 模式跑通仅验证语义（不断言比值）。

use actix::prelude::*;
use parrot::actix::ActixActorSystem;
use parrot_api::ParrotTypedActor;
use parrot_api::message::Message;
use parrot_api::typed::TypedReceive;
use parrot_api::types::{ActorResult, BoxedFuture};
use std::sync::Arc;
use std::time::Instant;

// ============ 消息 ============
// 两套消息类型分开（actix Message derive 与 parrot Message trait 的
// impl 冲突不能共存于同一类型）。

/// 纯 actix 侧消息（actix derive）。
#[derive(Clone, Copy, Debug, MessageResponse)]
pub struct PureEcho(u64);

#[derive(Clone, Copy, Debug, Message)]
#[rtype(result = "PureEcho")]
pub struct PurePing(u64);

/// parrot 静态轨消息（parrot Message trait 手写 impl）。
#[derive(Clone, Copy, Debug)]
pub struct Ping(u64);

impl Message for Ping {
    type Result = u64;
}

// ============ 纯 actix 基准 actor ============

struct PureActixEcho;

impl Actor for PureActixEcho {
    type Context = Context<Self>;
}

impl Handler<PurePing> for PureActixEcho {
    type Result = PureEcho;
    fn handle(&mut self, msg: PurePing, _ctx: &mut Self::Context) -> PureEcho {
        PureEcho(msg.0)
    }
}

// ============ parrot 静态轨 actor ============

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Ping))]
pub struct TypedEcho;

impl TypedReceive<Ping> for TypedEcho {
    fn receive_typed<'a>(&'a mut self, msg: Ping) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move { Ok(msg.0) })
    }
}

// ============ bench ============

const N: u64 = 50_000;

#[test]
#[ignore = "release-only：CI release 补跑（debug 下比值无意义）"]
fn m4_static_track_matches_pure_actix() {
    System::new().block_on(async {
        // ---- 纯 actix 基线 ----
        let pure_addr = PureActixEcho.start();

        // warmup
        for i in 0..1_000u64 {
            let _ = pure_addr.send(PurePing(i)).await.unwrap();
        }
        let t0 = Instant::now();
        for i in 0..N {
            let PureEcho(v) = pure_addr.send(PurePing(i)).await.unwrap();
            debug_assert_eq!(v, i);
        }
        let pure_elapsed = t0.elapsed();

        // ---- parrot 静态轨（actix 引擎） ----
        let sys = Arc::new(ActixActorSystem::new().await.expect("system"));
        let typed = sys
            .spawn_typed::<TypedEcho, Ping>(TypedEcho, "/bench/typed")
            .await
            .unwrap();

        for i in 0..1_000u64 {
            let v = typed.ask(Ping(i)).await.unwrap();
            debug_assert_eq!(v, i);
        }
        let t1 = Instant::now();
        for i in 0..N {
            let v = typed.ask(Ping(i)).await.unwrap();
            debug_assert_eq!(v, i);
        }
        let typed_elapsed = t1.elapsed();

        let ratio = typed_elapsed.as_secs_f64() / pure_elapsed.as_secs_f64();
        println!(
            "pure actix : {pure_elapsed:?} ({:.0} ns/msg)",
            pure_elapsed.as_nanos() as f64 / N as f64
        );
        println!(
            "typed track: {typed_elapsed:?} ({:.0} ns/msg)",
            typed_elapsed.as_secs_f64() / N as f64 * 1e9
        );
        println!("ratio      : {ratio:.3}x");

        // M4 目标：静态轨与纯 actix 持平（容忍 15% 调度抖动）
        assert!(
            ratio <= 1.15,
            "typed track must match pure actix: ratio={ratio:.3}x \
             (typed={typed_elapsed:?} vs pure={pure_elapsed:?})"
        );
    });
}
