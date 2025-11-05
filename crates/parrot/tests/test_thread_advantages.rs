//! Thread 调度、背压、内存、嵌入及注册吞吐契约。
//!
//! A1/A5 是显式 release 性能实验，不是与 Actix 的同轮 A/B 比较。
//! A5 近期参考机门槛为 >50000/s，原 >300000/s 由 stretch 测试保留。
//! 调整依据与完整工作量要求见 docs/A5_PERFORMANCE.md。
//! DedicatedThread 提供每 actor 专属线程；不提供硬实时或进程隔离。
//! A2/A3 验证背压与消息所有权，A4 验证宿主 Tokio runtime 嵌入。

mod dedicated_isolation;
mod spawn_contracts;

mod engine_stress_common;

use parrot::system::ParrotActorSystem;
use parrot::thread::config::{
    BackpressureStrategy, SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig,
};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef;
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// A1: DedicatedThread —— 检查独立 owner、真实 CPU 重叠及共享任务进度
// -------------------------------------------------------------------------

#[test]
#[ignore = "explicit release performance gate: four 4G-iteration tasks <8s and probes <500ms"]
fn a1_dedicated_thread_isolation() {
    dedicated_isolation::run();
}

// ---------------------------------------------------------------------------
// A2: Exact backpressure payloads and ordering.
// -------------------------------------------------------------------------

struct BackpressureGate {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
struct BackpressureRecorder {
    observed: tokio::sync::mpsc::UnboundedSender<u64>,
    values: Vec<u64>,
}
impl Actor for BackpressureRecorder {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn state(&self) -> ActorState {
        ActorState::Running
    }
    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<BackpressureGate>() {
                let gate = *msg.downcast::<BackpressureGate>().unwrap();
                gate.entered.send(()).unwrap();
                gate.release.await.unwrap();
            } else if let Some(value) = msg.downcast_ref::<u64>() {
                self.values.push(*value);
                self.observed.send(*value).unwrap();
            }
            Ok(Box::new(self.values.clone()) as BoxedMessage)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2_backpressure_strategies() {
    // A gate proves the consumer is blocked; exact payloads prove which
    // messages survive. No CPU-speed assumptions or fixed startup sleeps.
    for mode in [
        SchedulingMode::SharedPool {
            max_messages_per_run: 10,
        },
        SchedulingMode::DedicatedThread,
        SchedulingMode::Sharded {
            affinity_key: "backpressure".into(),
        },
    ] {
        let system = ThreadActorSystem::shared(Default::default());
        for strategy in [
            BackpressureStrategy::Error,
            BackpressureStrategy::DropOldest,
            BackpressureStrategy::DropNewest,
            BackpressureStrategy::Block,
        ] {
            let (observed, mut received) = tokio::sync::mpsc::unbounded_channel();
            let actor = system
                .spawn_at(
                    BackpressureRecorder {
                        observed,
                        values: vec![],
                    },
                    "/bp",
                    None,
                    ThreadActorConfig {
                        scheduling_mode: Some(mode.clone()),
                        mailbox_capacity: Some(4),
                        backpressure_strategy: Some(strategy.clone()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let (entered, started) = tokio::sync::oneshot::channel();
            let (release, gate) = tokio::sync::oneshot::channel();
            actor
                .deliver(Box::new(BackpressureGate {
                    entered,
                    release: gate,
                }))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), started)
                .await
                .unwrap()
                .unwrap();
            for value in 0..4u64 {
                actor.deliver(Box::new(value)).await.unwrap();
            }
            let expected: Vec<u64> = if strategy == BackpressureStrategy::Block {
                let pending = actor.deliver(Box::new(4u64));
                tokio::pin!(pending);
                assert!(
                    futures::poll!(&mut pending).is_pending(),
                    "full mailbox must suspend Block send"
                );
                release.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(2), pending)
                    .await
                    .unwrap()
                    .unwrap();
                (0..5).collect()
            } else {
                for value in 4..10u64 {
                    let result = actor.deliver(Box::new(value)).await;
                    if strategy == BackpressureStrategy::Error {
                        assert!(result.unwrap_err().to_string().contains("Full"));
                    } else if strategy == BackpressureStrategy::DropNewest {
                        assert!(result.unwrap_err().to_string().contains("Dropped"));
                    } else {
                        result.unwrap();
                    }
                }
                release.send(()).unwrap();
                if strategy == BackpressureStrategy::DropOldest {
                    (6..10).collect()
                } else {
                    (0..4).collect()
                }
            };
            for expected_value in &expected {
                let actual = tokio::time::timeout(Duration::from_secs(2), received.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(actual, *expected_value, "{mode:?} / {strategy:?}");
            }
            // Query after all expected deliveries; adding the query while full
            // could itself legitimately evict a value under DropOldest.
            let reply = actor
                .send_with_timeout(Box::new(()), Some(Duration::from_secs(2)))
                .await
                .unwrap();
            assert_eq!(*reply.downcast::<Vec<u64>>().unwrap(), expected);
            system.stop_actor("/bp").await.unwrap();
        }
        system.shutdown_internal().await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// A3: Bounded payload retention and disposal, independent of CPU speed and RSS noise.
// -------------------------------------------------------------------------

struct RetainedPayload {
    live: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    _bytes: [u8; 1024],
}
impl Drop for RetainedPayload {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a3_bounded_mailbox_memory_guard() {
    let system = ThreadActorSystem::shared(Default::default());
    let (observed, _received) = tokio::sync::mpsc::unbounded_channel();
    let consumer = system
        .spawn_at(
            BackpressureRecorder {
                observed,
                values: vec![],
            },
            "/mem/c",
            None,
            ThreadActorConfig {
                mailbox_capacity: Some(256),
                backpressure_strategy: Some(BackpressureStrategy::Error),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    consumer
        .deliver(Box::new(BackpressureGate {
            entered,
            release: gate,
        }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), started)
        .await
        .unwrap()
        .unwrap();
    let live = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let mut accepted = 0;
    for _ in 0..200_000 {
        live.fetch_add(1, Ordering::SeqCst);
        let result = consumer
            .deliver(Box::new(RetainedPayload {
                live: live.clone(),
                dropped: dropped.clone(),
                _bytes: [0; 1024],
            }))
            .await;
        match result {
            Ok(()) => accepted += 1,
            Err(error) => assert!(error.to_string().contains("Full")),
        }
        assert!(
            live.load(Ordering::SeqCst) <= 256,
            "rejected payload must be released before send returns"
        );
    }
    assert_eq!(accepted, 256);
    assert_eq!(live.load(Ordering::SeqCst), 256);
    assert_eq!(dropped.load(Ordering::SeqCst), 200_000 - 256);
    release.send(()).unwrap();
    // Explicit Block preserves the drain barrier even if the queue is full.
    consumer
        .ask_with_strategy_and_timeout(
            Box::new(()),
            BackpressureStrategy::Block,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(dropped.load(Ordering::SeqCst), 200_000);
    system.shutdown_internal().await.unwrap();
}

// ---------------------------------------------------------------------------
// A4: Existing Tokio runtime; no Actix System is constructed.
// ---------------------------------------------------------------------------
#[test]
fn a4_embed_in_host_runtime() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let system = ThreadActorSystem::shared(Default::default());
        spawn_contracts::exercise(&system, 1, spawn_contracts::Behavior::Normal)
            .await
            .expect("embedded actor replies and retirement");
        println!(
            "[A4] direct and registry views replied correctly and retired without an Actix System"
        );
    });
}

// ---------------------------------------------------------------------------
// A5: Registration throughput with post-timing usability and retirement checks.
// ---------------------------------------------------------------------------
const A5_CURRENT_FLOOR: f64 = 50_000.0;
const A5_ORIGINAL_TARGET: f64 = 300_000.0;

#[test]
#[ignore = "explicit release reference-host gate: 20000 actors, >50000 registrations/s"]
fn a5_herd_spawn_throughput_thread() {
    run_a5_registration_gate("current", A5_CURRENT_FLOOR);
}

#[test]
#[ignore = "explicit release stretch target: 20000 actors, original >300000 registrations/s"]
fn a5_herd_spawn_throughput_thread_stretch() {
    run_a5_registration_gate("stretch", A5_ORIGINAL_TARGET);
}

fn run_a5_registration_gate(target: &str, limit: f64) {
    if cfg!(debug_assertions) {
        panic!(
            "A5 performance gate requires --release; small_herd_checks_all_views_and_retirement runs in both profiles"
        );
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let parrot = ParrotActorSystem::new(ActorSystemConfig::default()).await.unwrap();
        let system = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
        parrot.register_thread_system("herd".into(), system.clone(), true).await.unwrap();
        let measurement = spawn_contracts::exercise(&system, 20_000, spawn_contracts::Behavior::Normal).await;
        let facade_shutdown = parrot.internal_shutdown().await;
        let measurement = measurement.expect("all registered actors must reply and retire");
        facade_shutdown.expect("herd facade retirement");
        let rate = measurement.actors_per_second().unwrap();
        println!("[A5] target={target} actors={} registration_seconds={:.6} registrations_per_second={rate:.3} limit_exclusive={limit:.0} original_limit_exclusive={A5_ORIGINAL_TARGET:.0} original_target_met={} replies_verified=40000 actors_retired=20000 facade_retired=true", measurement.count, measurement.elapsed.as_secs_f64(), rate > A5_ORIGINAL_TARGET);
        assert!(rate > limit, "thread registration {target} target failed: {rate:.3} <= {limit:.0}");
    });
}
