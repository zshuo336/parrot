//! M0 性能基线（防倒退金标准，`docs/PERF_BASELINE.md` 的数据源）。
//!
//! 设计原则：
//! - 与具体引擎无关的 thread 引擎核心路径：ask 往返延迟、tell 吞吐
//! - 断言用**宽松下限**（防严重倒退），精确数字记录在 PERF_BASELINE.md
//! - 单独可跑：`cargo test --release -p parrot --test stress_baseline -- --nocapture`

use std::time::{Duration, Instant};

use parrot::system::ParrotActorSystem;
use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::message::Message;
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

struct EchoActor;

impl Actor for EchoActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.downcast_ref::<u64>().is_some() {
                Ok(msg)
            } else if let Some(e) = msg.downcast_ref::<BaselineEcho>() {
                Ok(Box::new(e.value) as BoxedMessage)
            } else {
                Ok(msg)
            }
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

struct BaselineEcho {
    value: u64,
}

impl Message for BaselineEcho {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| parrot_api::errors::ActorError::MessageHandlingError("type".into()))
    }
}

fn percentile(mut samples: Vec<u128>, p: f64) -> u128 {
    samples.sort_unstable();
    let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[idx.min(samples.len() - 1)]
}

/// ask 往返延迟基线：1 万次串行 ask 的 p50/p99 + 宽松下限断言。
#[test]
fn baseline_ask_roundtrip_latency() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();
        let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
        parrot
            .register_thread_system("baseline".into(), ts.clone(), true)
            .await
            .unwrap();

        let r = ts
            .spawn_at::<EchoActor>(
                EchoActor,
                "/baseline/echo",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();

        // 预热
        for i in 0..100u64 {
            let _ = r
                .send_with_timeout(
                    Box::new(BaselineEcho { value: i }),
                    Some(Duration::from_secs(10)),
                )
                .await
                .unwrap();
        }

        let mut samples = Vec::with_capacity(10_000);
        for i in 0..10_000u64 {
            let t = Instant::now();
            let v = r
                .send_with_timeout(
                    Box::new(BaselineEcho { value: i }),
                    Some(Duration::from_secs(10)),
                )
                .await
                .unwrap();
            let v = *v.downcast::<u64>().expect("echo returns u64");
            assert_eq!(v, i);
            samples.push(t.elapsed().as_micros());
        }

        let p50 = percentile(samples.clone(), 0.50);
        let p99 = percentile(samples.clone(), 0.99);
        let throughput = 1_000_000.0 / p50 as f64;
        println!(
            "[BASELINE ask] p50={}µs p99={}µs ≈{:.0} ask/s",
            p50, p99, throughput
        );

        // 宽松下限：p99 < 2000µs（倒退 10 倍量级才失败）
        assert!(p99 < 2000, "ask p99 regressed: {}µs", p99);
    });
}

/// tell（fire-and-forget）吞吐基线：10 万消息 drain 计时。
#[test]
fn baseline_tell_throughput() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
            .await
            .unwrap();
        let ts = ThreadActorSystem::shared(ThreadActorSystemConfig {
            default_mailbox_capacity: 100_000,
            ..Default::default()
        });
        parrot
            .register_thread_system("baseline2".into(), ts.clone(), true)
            .await
            .unwrap();

        let r = ts
            .spawn_at::<EchoActor>(
                EchoActor,
                "/baseline2/echo",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();

        const N: u64 = 100_000;
        let t0 = Instant::now();
        for i in 0..N {
            r.send_msg(Box::new(BaselineEcho { value: i }))
                .await
                .unwrap();
        }
        // send_msg 完成 = 入队完成；再 ask 一条做 drain 屏障
        let drained = r
            .ask_with_timeout(
                Box::new(BaselineEcho { value: u64::MAX }),
                Duration::from_secs(30),
            )
            .await;
        assert!(drained.is_ok(), "drain barrier failed");
        let wall = t0.elapsed();
        println!(
            "[BASELINE tell] {} msgs in {:.2}s = {:.0} msg/s",
            N,
            wall.as_secs_f64(),
            N as f64 / wall.as_secs_f64()
        );

        // 宽松下限：> 20k msg/s（倒退 10 倍量级才失败）
        assert!(
            N as f64 / wall.as_secs_f64() > 20_000.0,
            "tell throughput regressed"
        );
    });
}

/// 静态轨 vs 动态轨对照点（M4 后填充静态数字；现在先记录动态轨）。
#[test]
fn baseline_dynamic_track_reference() {
    // 见 baseline_ask_roundtrip_latency；此测试为 M4 提供同构对照入口，
    // M4 落地后在此追加静态轨同场景测量。
    baseline_ask_roundtrip_latency();
}
