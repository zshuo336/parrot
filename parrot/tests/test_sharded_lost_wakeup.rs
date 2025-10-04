//! Sharded 调度丢失唤醒回归测试（ScheduleState 竞态修复的守护）
//!
//! 背景：`ScheduleState::try_enqueue` 在 hook 线程设置 `pending_wake` 后，
//! 若 owner 的 `release` 恰在此 store 可见前完成最终检查并释放 slot，
//! 唤醒丢失 → mailbox 滞留消息但 shard 线程空转（actor 永久停摆）。
//! 修复：hook 失败路径用 RMW 重试 slot。
//!
//! 本测试以高并发 ask 风暴 + 尾部静默期复现该窗口：修复前 8 actor × 20k
//! 场景 ~2/3 概率挂死（5s 超时）；修复后必须 100% 通过且无超时。

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
            const N_ACTORS: usize = 8;
            const PER: u64 = 20_000;
            let mut refs = Vec::new();
            let mut ops = Vec::new();
            for i in 0..N_ACTORS {
                let o = Arc::new(std::sync::atomic::AtomicU64::new(0));
                ops.push(o.clone());
                refs.push(
                    ts.spawn_at::<Bench>(
                        Bench { ops: o },
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
                    let rep = r.ask(Box::new(Echo { value: 1 }) as BoxedMessage).await.unwrap();
                    let _ = Echo::extract_result(rep).unwrap();
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
                                Box::new(Echo { value: i as u64 * 1000 + k }) as BoxedMessage,
                                Duration::from_secs(30),
                            )
                            .await
                            .unwrap();
                        let v = Echo::extract_result(rep).unwrap();
                        assert_eq!(v, i as u64 * 1000 + k);
                    }
                }));
            }
            for h in hs {
                h.await.unwrap();
            }
            let wall = t0.elapsed();
            // 静默期：确认所有 ops 恰好 = PER+200（风暴+预热全部处理完）
            tokio::time::sleep(Duration::from_millis(200)).await;
            for (i, o) in ops.iter().enumerate() {
                let n = o.load(std::sync::atomic::Ordering::Relaxed);
                assert_eq!(
                    n, PER + 200,
                    "actor {} processed {} != {} — messages stranded (lost wakeup)",
                    i, n, PER + 200
                );
            }
            println!(
                "[LOST-WAKE round={}] 8×20k asks in {:.3}s, all drained",
                round,
                wall.as_secs_f64()
            );
            let _ = ts.shutdown_internal().await;
        });
    }
}
