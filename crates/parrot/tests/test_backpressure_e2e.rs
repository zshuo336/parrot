//! E3 · 反压贯通 e2e（DEV_03 §4 / 06 §3.3）。
//!
//! 链路：端侧慢消费 → 端侧读循环挂起 → 传输窗口收紧 → 云编排 actor
//! deliver 挂起/失败 → 其邮箱策略（Block/Error）触发。
//!
//! mem 载体等价：端侧 SlowSink（每条 sleep）+ 小邮箱（Error 策略→ 快速
//! 失败可见；Block 策略 → 唤醒后最终全达）。

mod common;

#[allow(unused_imports)]
use common::*;
use parrot::system::ParrotActorSystem;
use parrot::thread::config::{BackpressureStrategy, ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, EmptyConfig};
use parrot_api::address::ActorPath;
use parrot_api::system::{ActorSystem, ActorSystemConfig};
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct BpTell(pub u64);
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct BpGetCount;

macro_rules! remote_msg {
    ($t:ty, $key:literal) => {
        parrot_api::message::inventory::submit! {
            parrot_api::message::CodecRegistration {
                type_key: $key,
                type_id: std::any::TypeId::of::<$t>(),
                encode: |msg: &parrot_api::types::BoxedMessage| {
                    let m = msg.downcast_ref::<$t>().ok_or(concat!("downcast ", $key))?;
                    parrot_api::message::serde_remote_serialize(&m)
                },
                decode: |b: &[u8]| {
                    let v: $t = parrot_api::message::serde_remote_deserialize(b)?;
                    Ok(Box::new(v) as parrot_api::types::BoxedMessage)
                },
            }
        }
    };
}

remote_msg!(BpTell, "bin:bp::BpTell#v1");
remote_msg!(BpGetCount, "bin:bp::BpGetCount#v1");

/// 慢消费端：每条 sleep（模拟端侧慢）。
struct SlowSink {
    count: u64,
    delay_ms: u64,
}

impl Actor for SlowSink {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if let Some(BpTell(_)) = msg.downcast_ref::<BpTell>() {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
                self.count += 1;
                return Ok(Box::new(BpTell(self.count)) as BoxedMessage);
            }
            if msg.downcast_ref::<BpGetCount>().is_some() {
                return Ok(Box::new(BpTell(self.count)) as BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "unhandled".into(),
            ))
        })
    }

    fn state(&self) -> parrot_api::actor::ActorState {
        parrot_api::actor::ActorState::Running
    }
}

async fn mk_facade_with_sink(
    mailbox: usize,
    strategy: BackpressureStrategy,
    delay_ms: u64,
) -> Arc<ParrotActorSystem> {
    let facade = Arc::new(
        ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap(),
    );
    let cfg = ThreadActorSystemConfig {
        default_mailbox_capacity: mailbox,
        default_backpressure_strategy: strategy.clone(),
        ..Default::default()
    };
    let ts = ThreadActorSystem::shared(cfg);
    facade
        .register_thread_system("eng".into(), ts.clone(), true)
        .await
        .unwrap();
    let actor_cfg = ThreadActorConfig {
        mailbox_capacity: Some(mailbox),
        backpressure_strategy: Some(strategy),
        ..Default::default()
    };
    ts.spawn_at(
        SlowSink { count: 0, delay_ms },
        "/user/slow",
        None,
        actor_cfg,
    )
    .await
    .unwrap();
    facade
}

/// Block 策略：端侧唤醒后最终全达。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backpressure_e2e_block_eventual_delivery() {
    let facade = mk_facade_with_sink(16, BackpressureStrategy::Block, 1).await;
    let sink = facade
        .get_actor(&ActorPath::placeholder("/user/slow"))
        .await
        .unwrap();

    // 200 条 tell（每条 1ms 慢消费——邮箱 16 会反复满，Block 挂起等待）
    let t0 = std::time::Instant::now();
    for i in 0..200u64 {
        sink.send(Box::new(BpTell(i)))
            .await
            .expect("Block must not drop");
    }
    // 全部投递完成（最后一条 ask 返回时 count 已含全部）
    let r = sink.send(Box::new(BpGetCount)).await.unwrap();
    let cnt = r.downcast_ref::<BpTell>().unwrap().0;
    assert_eq!(cnt, 200, "all 200 tells delivered after wakeup");
    assert!(
        t0.elapsed() > Duration::from_millis(150),
        "slow consumer actually throttled"
    );
}

/// Error 策略：邮箱满 → 编排侧 deliver（tell）收到确定性错误（不悬挂、不静默丢）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backpressure_e2e_error_fast_fail() {
    let facade = mk_facade_with_sink(4, BackpressureStrategy::Error, 50).await;
    let sink = facade
        .get_actor(&ActorPath::placeholder("/user/slow"))
        .await
        .unwrap();

    // deliver（tell 语义——不等待回复）：50ms 慢消费 + 容量 4 → 快速堆满
    let mut errors = 0;
    for i in 0..64u64 {
        if sink.deliver(Box::new(BpTell(i))).await.is_err() {
            errors += 1;
        }
    }
    assert!(
        errors > 0,
        "Error strategy must surface backpressure to orchestrator"
    );
}
