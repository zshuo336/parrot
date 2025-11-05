//! M2 调度升级集成验证：优先级双车道 + reduction 公平性 + 系统消息可靠投递。
//!
//! POC 语义移植（remote-lab/poc/core-lab/beam-sched）：
//! 1. 慢 actor 不再饿死快 actor（reduction 预算让出）
//! 2. High 消息越过 Normal 积压（O(1) 插队）
//! 3. 死亡通知在邮箱满时仍送达（Block 不可丢）

use std::sync::Arc;
use std::time::{Duration, Instant};

use parrot::system::ParrotActorSystem;
use parrot::thread::config::{ThreadActorConfig, ThreadActorSystemConfig};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRefExt;
use parrot_api::message::{Message, MessageOptions, MessagePriority};
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use tokio::sync::Notify;

/// 每条消息固定耗时 ~2ms 的慢 actor（持续积压）。
struct SlowActor;

impl Actor for SlowActor {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn init<'a>(&'a mut self, _ctx: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn receive_message<'a>(
        &'a mut self,
        _msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(2)).await;
            Ok(Box::new(0u64) as BoxedMessage)
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

/// 快 actor：echo，微秒级。
struct FastActor;

impl Actor for FastActor {
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
        Box::pin(async move { Ok(msg) })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

struct ProbeMsg;
impl Message for ProbeMsg {
    type Result = ();
}

struct PriorityNormal;
struct PriorityHigh;

impl Message for PriorityNormal {
    type Result = ();
}
impl Message for PriorityHigh {
    type Result = ();
    fn message_options(&self) -> Option<MessageOptions> {
        Some(MessageOptions {
            priority: MessagePriority::HIGH,
            ..MessageOptions::default()
        })
    }
}

struct PriorityCollector {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    seen: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

impl Actor for PriorityCollector {
    type Config = EmptyConfig;
    type Context = ThreadContext<Self>;

    fn receive_message<'a>(
        &'a mut self,
        msg: BoxedMessage,
        _ctx: &'a mut Self::Context,
    ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
        Box::pin(async move {
            if msg.is::<ProbeMsg>() {
                self.entered.notify_one();
                self.release.notified().await;
                return Ok(Box::new(()) as BoxedMessage);
            }
            let label =
                if let Some(event) = msg.downcast_ref::<parrot::thread::system::Terminated>() {
                    assert_eq!(event.path, "/m2/victim");
                    "terminated"
                } else if msg.is::<PriorityHigh>() {
                    "high"
                } else {
                    "normal"
                };
            self.seen.lock().unwrap().push(label);
            Ok(Box::new(()) as BoxedMessage)
        })
    }

    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

async fn setup(name: &str) -> (Arc<ThreadActorSystem>, ParrotActorSystem) {
    let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
        .await
        .unwrap();
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig {
        shared_pool_size: 2, // 极小池：放大公平性效应
        shared_burst_workers_max: 0,
        ..Default::default()
    });
    parrot
        .register_thread_system(name.into(), ts.clone(), true)
        .await
        .unwrap();
    (ts, parrot)
}

/// 测试 1：慢 actor 持续积压时，快 actor 的 ask 延迟仍保持低位
/// （reduction 预算强制慢 actor 周期性让出 worker）。
#[test]
fn m2_slow_actor_does_not_starve_fast_actor() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (ts, _parrot) = setup("m2-fair").await;

        let slow = ts
            .spawn_at::<SlowActor>(SlowActor, "/m2/slow", None, ThreadActorConfig::default())
            .await
            .unwrap();
        let fast = ts
            .spawn_at::<FastActor>(FastActor, "/m2/fast", None, ThreadActorConfig::default())
            .await
            .unwrap();

        // 慢 actor 灌入持续积压（500 条 × 2ms = 1s 连续工作）
        for i in 0..500u64 {
            slow.send_msg(Box::new(i)).await.unwrap();
        }

        // 快 actor 串行 ask 20 次：每次都应在远小于慢 actor 总积压时间内完成
        let mut max_lat = Duration::ZERO;
        for i in 0..20u64 {
            let t = Instant::now();
            let v = fast
                .ask_with_timeout(Box::new(i), Duration::from_secs(10))
                .await
                .unwrap();
            assert_eq!(*v.downcast::<u64>().unwrap(), i);
            max_lat = max_lat.max(t.elapsed());
        }

        // 断言：快 actor 最大单次延迟 < 500ms（无预算让出时慢 actor
        // 可连续霸占两个 worker 数百 ms；预算生效后快 actor 每几十 ms
        // 必获得调度机会）。阈值宽松以防 CI 抖动。
        assert!(
            max_lat < Duration::from_millis(500),
            "fast actor starved: max latency {:?}",
            max_lat
        );
        println!(
            "[M2-1] fast actor max latency under slow backlog: {:?}",
            max_lat
        );
    });
}

#[test]
fn typed_message_priority_survives_actor_ref_boundary() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (ts, _parrot) = setup("m2-typed-prio").await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let actor = ts
            .spawn_at(
                PriorityCollector {
                    entered: entered.clone(),
                    release: release.clone(),
                    seen: seen.clone(),
                },
                "/m2/typed-prio",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();
        actor.tell(ProbeMsg).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        for _ in 0..20 {
            actor.tell(PriorityNormal).await.unwrap();
        }
        actor.tell(PriorityHigh).await.unwrap();
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if seen.lock().unwrap().len() >= 21 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let values = seen.lock().unwrap().clone();
        assert_eq!(
            values.first().copied(),
            Some("high"),
            "metadata priority was dropped: {values:?}"
        );
        ts.shutdown_internal().await.unwrap();
    });
}

/// A blocked watcher holds a genuinely full mailbox while the victim stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m2_death_notification_survives_full_mailbox() {
    let (ts, _parrot) = setup("m2-death").await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    ts.spawn_at(FastActor, "/m2/victim", None, Default::default())
        .await
        .unwrap();
    let watcher = ts
        .spawn_at(
            PriorityCollector {
                entered: entered.clone(),
                release: release.clone(),
                seen: seen.clone(),
            },
            "/m2/watcher",
            None,
            ThreadActorConfig {
                mailbox_capacity: Some(4),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    ts.watch("/m2/watcher".into(), "/m2/victim".into())
        .await
        .unwrap();
    watcher.tell(ProbeMsg).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    for _ in 0..4 {
        watcher.tell(PriorityNormal).await.unwrap();
    }
    assert_eq!(ts.get_mailbox("/m2/watcher").unwrap().len().await, 4);
    let mut stopping = Box::pin(ts.stop_actor("/m2/victim"));
    assert!(futures::poll!(&mut stopping).is_pending());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), stopping)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while seen.lock().unwrap().len() != 5 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter().filter(|label| **label == "terminated").count(),
        1
    );
    assert_eq!(seen.iter().filter(|label| **label == "normal").count(), 4);
    ts.shutdown_internal().await.unwrap();
}
