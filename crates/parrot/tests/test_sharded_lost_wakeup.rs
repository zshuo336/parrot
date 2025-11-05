//! Sharded 调度丢失唤醒回归测试（ScheduleState 竞态修复的守护）
//!
//! 背景：`ScheduleState::try_enqueue` 在 hook 线程设置 `pending_wake` 后，
//! 若 owner 的 `release` 恰在此 store 可见前完成最终检查并释放 slot，
//! 唤醒丢失 → mailbox 滞留消息但 shard 线程空转（actor 永久停摆）。
//! 修复：hook 失败路径用 RMW 重试 slot。
//!
//! 本测试以三轮 8 actor × 20k ask 压力和尾部静默期守护此路径。
//! 每次请求有30秒上限；检查回复、全部任务、处理数及真实退休。
//! 压力测试通过不是对所有线程交错均无丢失唤醒的形式化证明。

use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parrot::thread::config::{SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

struct Echo {
    value: u64,
}
impl Message for Echo {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}

struct Bench {
    ops: Arc<std::sync::atomic::AtomicU64>,
    drops: Arc<std::sync::atomic::AtomicU64>,
}
impl Drop for Bench {
    fn drop(&mut self) {
        self.drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
impl Actor for Bench {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;
    fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn receive_message<'a>(
        &'a mut self,
        m: BoxedMessage,
        _c: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        use std::sync::atomic::Ordering;
        self.ops.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            if let Some(e) = m.downcast_ref::<Echo>() {
                Ok(Box::new(e.value) as BoxedMessage)
            } else {
                Ok(m)
            }
        })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

fn cfg_sharded(key: &str) -> ThreadActorConfig {
    ThreadActorConfig {
        scheduling_mode: Some(SchedulingMode::Sharded {
            affinity_key: key.into(),
        }),
        ..Default::default()
    }
}

/// 高并发风暴后必须完全排空（静默期无残留消息、无停摆 actor）。
#[test]
fn sharded_no_lost_wakeup_under_concurrent_ask_storm() {
    for round in 0..3 {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(8)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
            let mut drops = Vec::new();
            let exercise = AssertUnwindSafe(async {
                const N_ACTORS: usize = 8;
                const PER: u64 = 20_000;
                let mut refs = Vec::new();
                let mut ops = Vec::new();
                for i in 0..N_ACTORS {
                    let o = Arc::new(std::sync::atomic::AtomicU64::new(0));
                    ops.push(o.clone());
                    let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));
                    drops.push(dropped.clone());
                    refs.push(
                        ts.spawn_at::<Bench>(
                            Bench {
                                ops: o,
                                drops: dropped,
                            },
                            &format!("/lost-wake/{}/{}", round, i),
                            None,
                            cfg_sharded(&format!("domain-{}", i % 4)),
                        )
                        .await
                        .unwrap(),
                    );
                }
                // warmup
                for r in &refs {
                    for _ in 0..200 {
                        let rep = r
                            .ask_with_timeout(
                                Box::new(Echo { value: 1 }) as BoxedMessage,
                                Duration::from_secs(30),
                            )
                            .await
                            .unwrap();
                        assert_eq!(Echo::extract_result(rep).unwrap(), 1);
                    }
                }
                // 风暴
                let t0 = Instant::now();
                let mut hs = Vec::new();
                for (i, r) in refs.into_iter().enumerate() {
                    hs.push(tokio::spawn(async move {
                        for k in 0..PER {
                            let rep = r
                                .ask_with_timeout(
                                    Box::new(Echo {
                                        value: i as u64 * PER + k,
                                    }) as BoxedMessage,
                                    Duration::from_secs(30),
                                )
                                .await
                                .unwrap();
                            let v = Echo::extract_result(rep).unwrap();
                            assert_eq!(v, i as u64 * PER + k);
                        }
                    }));
                }
                // Join every sender before inspecting any failure. Dropping a later
                // JoinHandle would detach work and let cleanup race the remaining asks.
                let results = futures::future::join_all(hs).await;
                for result in results {
                    result.expect("ask sender failed");
                }
                let wall = t0.elapsed();
                // 静默期：确认所有 ops 恰好 = PER+200（风暴+预热全部处理完）
                tokio::time::sleep(Duration::from_millis(200)).await;
                for (i, o) in ops.iter().enumerate() {
                    let n = o.load(std::sync::atomic::Ordering::Relaxed);
                    assert_eq!(
                        n,
                        PER + 200,
                        "actor {} processed {} != {} — messages stranded (lost wakeup)",
                        i,
                        n,
                        PER + 200
                    );
                }
                wall
            })
            .catch_unwind()
            .await;
            // Even a warmup/spawn/ask/count assertion must retire the system.
            let shutdown = ts.shutdown_internal().await;
            let wall = match exercise {
                Ok(wall) => wall,
                Err(panic) => {
                    if let Err(error) = shutdown {
                        eprintln!("shutdown also failed after workload failure: {error:?}");
                    }
                    std::panic::resume_unwind(panic);
                }
            };
            shutdown.expect("sharded stress system failed to retire");
            assert!(
                ts.actor_paths().is_empty(),
                "registrations survived shutdown"
            );
            for dropped in drops {
                assert_eq!(
                    dropped.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "each actor must be disposed exactly once before success"
                );
            }
            println!(
                "[LOST-WAKE round={round}] 8×20k asks in {:.3}s, all drained and retired",
                wall.as_secs_f64()
            );
        });
    }
}
