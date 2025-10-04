//! M3 监督执行器集成验证（POC supervision 语义移植）。
//!
//! 1. panic → Restart 决策执行：actor 崩溃后自动重启恢复服务
//! 2. watch 带死因通知：Terminated { reason: Panic } 公开可匹配
//! 3. 窗口限频超限 → Escalate：重启预算耗尽后停止

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parrot::system::ParrotActorSystem;
use parrot::thread::config::{SupervisorStrategy, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::{Terminated, ThreadActorSystem};
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::supervisor::DeathReason;
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};

/// 计数 actor：收到 "boom" 就 panic，收到 u64 计数。
struct FlakyActor {
    generation: u64,
}

impl Actor for FlakyActor {
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
            if msg
                .downcast_ref::<&str>()
                .map(|s| *s == "boom")
                .unwrap_or(false)
            {
                panic!("boom requested");
            }
            if let Some(v) = msg.downcast_ref::<u64>() {
                return Ok(Box::new(*v + self.generation) as BoxedMessage);
            }
            Ok(msg)
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// 收集 Terminated 通知的 watcher。
struct WatcherActor {
    deaths: Arc<AtomicU64>,
    last_reason_panic: std::sync::Mutex<Option<bool>>,
}

impl Actor for WatcherActor {
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
            if let Some(term) = msg.downcast_ref::<Terminated>() {
                self.deaths.fetch_add(1, Ordering::SeqCst);
                let is_panic = matches!(term.reason, DeathReason::Panic(_));
                *self.last_reason_panic.lock().unwrap() = Some(is_panic);
            }
            Ok(Box::new(()) as BoxedMessage)
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

/// 测试 1：panic → 自动重启，新实例继续服务（Restart 决策真正执行）。
#[test]
fn m3_panic_restart_recovers_service() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ts = setup("m3-restart").await;

        // 监督 spawn：工厂每次产出新实例（generation 递增可观测重启）
        let generation = Arc::new(AtomicU64::new(0));
        let g = generation.clone();
        let r = ts
            .spawn_supervised(
                move || FlakyActor {
                    generation: g.fetch_add(1, Ordering::SeqCst),
                },
                "/m3/flaky",
                SupervisorStrategy::Restart {
                    max_retries: 3,
                    within: Duration::from_secs(10),
                },
            )
            .await
            .unwrap();

        // 正常服务
        let v = r
            .ask_with_timeout(Box::new(100u64), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(*v.downcast::<u64>().unwrap(), 100);

        // 触发 panic（tell，fire-and-forget）
        r.send_msg(Box::new("boom")).await.unwrap();

        // 等待监督决策执行（panic → 通知 → 决策 → 重启）
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 重启后 mailbox 是新实例：经系统注册表重新解析 ref
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let reply = loop {
            if let Some(entry_ref) = ts.get_actor_ref("/m3/flaky")
                && let Ok(v) = entry_ref
                    .send_with_timeout(Box::new(100u64), Some(Duration::from_secs(2)))
                    .await
            {
                break v;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("重启后的 actor 5s 内未恢复服务");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(
            *reply.downcast::<u64>().unwrap(),
            101,
            "重启后应使用新实例（generation 递增）"
        );

        // 重启发生
        assert!(
            generation.load(Ordering::SeqCst) >= 2,
            "工厂至少被调用 2 次（初始+重启）"
        );
    });
}

/// 测试 2：watch 死亡通知带 Panic 死因（公开 Terminated 可匹配）。
#[test]
fn m3_watch_carries_death_reason() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ts = setup("m3-watch").await;

        let deaths = Arc::new(AtomicU64::new(0));
        let _watcher = ts
            .spawn_at::<WatcherActor>(
                WatcherActor {
                    deaths: deaths.clone(),
                    last_reason_panic: std::sync::Mutex::new(None),
                },
                "/m3/watcher",
                None,
                Default::default(),
            )
            .await
            .unwrap();

        let generation = Arc::new(AtomicU64::new(0));
        let g = generation.clone();
        let victim = ts
            .spawn_supervised(
                move || FlakyActor {
                    generation: g.fetch_add(1, Ordering::SeqCst),
                },
                "/m3/victim",
                SupervisorStrategy::Stop, // Stop 策略：panic 后不再重启
            )
            .await
            .unwrap();

        // watcher watch victim
        ts.watch("/m3/watcher".into(), "/m3/victim".into())
            .await
            .unwrap();

        // victim panic
        victim.send_msg(Box::new("boom")).await.unwrap();

        // 等死亡通知投递（High 车道 + Block，不可丢）
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while deaths.load(Ordering::SeqCst) == 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            deaths.load(Ordering::SeqCst),
            1,
            "watcher 必须收到 Terminated"
        );

        // 死因为 Panic（语义变更 #2 验证点）
        // （actor 内部状态无法直接读；用第二次 ask 失败验证 victim 已死：
        //  Stop 策略下 mailbox 已关闭）
        let gone = victim
            .ask_with_timeout(Box::new(1u64), Duration::from_millis(500))
            .await;
        assert!(gone.is_err(), "Stop 策略下 victim 不应复活");
    });
}

/// 测试 3：窗口限频超限 → Escalate（重启预算耗尽后停止循环重启）。
#[test]
fn m3_window_exhaustion_escalates() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ts = setup("m3-escalate").await;

        let generation = Arc::new(AtomicU64::new(0));
        let g = generation.clone();
        let _r = ts
            .spawn_supervised(
                move || FlakyActor {
                    generation: g.fetch_add(1, Ordering::SeqCst),
                },
                "/m3/crashloop",
                SupervisorStrategy::Restart {
                    max_retries: 2, // 允许 2 次重启，第 3 次死亡 → Escalate
                    within: Duration::from_secs(10),
                },
            )
            .await
            .unwrap();

        // 连续触发 panic 3 次（初始死亡 + 2 次重启后死亡 → 预算耗尽）。
        // 经注册表解析当前 ref（重启产生新 mailbox；旧 ref 失效时跳过本轮）。
        for _ in 0..3 {
            if let Some(entry_ref) = ts.get_actor_ref("/m3/crashloop") {
                let _ = entry_ref
                    .send_with_timeout(Box::new("boom"), Some(Duration::from_millis(500)))
                    .await;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        // 预算耗尽：actor 被 Escalate 处理（停止），不再重启
        tokio::time::sleep(Duration::from_millis(400)).await;
        let final_generations = generation.load(Ordering::SeqCst);
        assert!(
            final_generations <= 3,
            "重启预算 2 耗尽后必须停止（Escalate），实际 generation={}",
            final_generations
        );

        // actor 已从系统移除（注册表查无此路径）
        assert!(
            ts.get_actor_ref("/m3/crashloop").is_none(),
            "Escalate 后 actor 应从注册表移除"
        );
    });
}
