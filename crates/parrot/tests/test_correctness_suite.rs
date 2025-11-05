//! 正确性专项套件（第九轮）：补齐压测未覆盖的语义不变量。
//!
//! C1 恰好一次：N 并发发送者 × M 条，零丢失、零重复
//! C2 FIFO：单 actor 内每发送者子序列严格有序
//! C3 计算完整性：burn_cpu 返回值与参考实现逐条一致
//! C4 回复路由：并发 asker 各自收到自己的回复（无串扰）
//! C5 跨模式等价：SharedPool/DedicatedThread/Sharded 同输入同输出
//! C6 背压语义：Error 精确拒收 / DropOldest 保留最新 K 条
//! C7 停止语义：stop 后发送失败、已处理计数稳定
//! C8 超时语义：阻塞时超时返回错误、空闲时成功

mod engine_stress_common;

use engine_stress_common::*;

use parrot::system::ParrotActorSystem;
use parrot::thread::config::{
    BackpressureStrategy, SchedulingMode, ThreadActorConfig, ThreadActorSystemConfig,
};
use parrot::thread::context::ThreadContext;
use parrot::thread::system::ThreadActorSystem;
use parrot_api::actor::{Actor, ActorState, EmptyConfig};
use parrot_api::address::ActorRef as ActorRefTrait;
use parrot_api::errors::ActorError;
use parrot_api::message::Message;
use parrot_api::system::ActorSystemConfig;
use parrot_api::types::{ActorResult, BoxedFuture, BoxedMessage};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 记录型 actor：把 (sender_id, seq) 与计算校验值记入共享日志
// ---------------------------------------------------------------------------

/// 复合校验消息：sender + 序号 + CPU 迭代数（结果用于计算完整性比对）
struct Tagged {
    sender: u64,
    seq: u64,
    iters: u64,
    salt: u64,
}
impl Message for Tagged {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}

struct EchoMsg {
    v: u64,
}
impl Message for EchoMsg {
    type Result = u64;
    fn extract_result(r: BoxedMessage) -> ActorResult<u64> {
        r.downcast::<u64>()
            .map(|b| *b)
            .map_err(|_| ActorError::MessageHandlingError("type".into()))
    }
}

/// 全局日志：(sender, seq) 有序记录 + burn 结果记录
struct Journal {
    events: Mutex<Vec<(u64, u64)>>,
    burn_results: Mutex<Vec<u64>>,
}

impl Journal {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            burn_results: Mutex::new(Vec::new()),
        })
    }
}

struct Recorder {
    journal: Arc<Journal>,
    /// Tagged 进入 handler 的即时标记（burn 之前置位）：
    /// 与外部共享（Arc），供测试做确定性门控（区分"已进入"与"已完成"）。
    started: Arc<AtomicU64>,
}

impl Actor for Recorder {
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
        if m.downcast_ref::<Tagged>().is_some() {
            // 进入即记（burn 之前），让外部 wait_until 能确定性观察到
            // "gate 已进入 handler"，消除 sleep 时序抖动。
            self.started.fetch_add(1, Ordering::Relaxed);
        }
        let r = if let Some(t) = m.downcast_ref::<Tagged>() {
            let v = burn_cpu(t.iters, t.salt);
            self.journal.events.lock().unwrap().push((t.sender, t.seq));
            self.journal.burn_results.lock().unwrap().push(v);
            Box::new(v) as BoxedMessage
        } else if let Some(e) = m.downcast_ref::<EchoMsg>() {
            self.journal.events.lock().unwrap().push((u64::MAX, e.v));
            Box::new(e.v) as BoxedMessage
        } else {
            Box::new(0u64) as BoxedMessage
        };
        Box::pin(async move { Ok(r) })
    }
    fn state(&self) -> ActorState {
        ActorState::Running
    }
}

async fn setup(name: &str) -> (ParrotActorSystem, Arc<ThreadActorSystem>) {
    let parrot = ParrotActorSystem::new(ActorSystemConfig::default())
        .await
        .unwrap();
    let ts = ThreadActorSystem::shared(ThreadActorSystemConfig::default());
    parrot
        .register_thread_system(name.into(), ts.clone(), true)
        .await
        .unwrap();
    (parrot, ts)
}

#[allow(dead_code)]
async fn ask_typed(
    r: &parrot::thread::address::ThreadActorRef<Recorder>,
    boxed: BoxedMessage,
) -> ActorResult<BoxedMessage> {
    r.ask(boxed).await
}

// ---------------------------------------------------------------------------
// C1+C2+C3：恰好一次 + 每发送者 FIFO + 计算完整性（一次跑全）
// ---------------------------------------------------------------------------

async fn queued_fifo_phase(ts: &Arc<ThreadActorSystem>) {
    const SENDERS: u64 = 8;
    const PER_SENDER: u64 = 128;
    enum QueuedMessage {
        Hold,
        Value { sender: u64, sequence: u64 },
        Snapshot(tokio::sync::oneshot::Sender<Vec<(u64, u64)>>),
    }
    struct Gate {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    struct QueuedRecorder {
        gate: Arc<Gate>,
        values: Vec<(u64, u64)>,
    }
    impl Actor for QueuedRecorder {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            _context: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                match *message
                    .downcast::<QueuedMessage>()
                    .expect("unexpected queued FIFO message")
                {
                    QueuedMessage::Hold => {
                        self.gate.entered.notify_one();
                        self.gate.release.notified().await;
                        Ok(Box::new(99u64) as BoxedMessage)
                    }
                    QueuedMessage::Value { sender, sequence } => {
                        self.values.push((sender, sequence));
                        Ok(Box::new(()) as BoxedMessage)
                    }
                    QueuedMessage::Snapshot(reply) => {
                        reply
                            .send(self.values.clone())
                            .expect("queued FIFO observer must be alive");
                        Ok(Box::new(()) as BoxedMessage)
                    }
                }
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }
    struct ReleaseOnDrop(Arc<Gate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release.notify_one();
        }
    }

    let gate = Arc::new(Gate {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let release = ReleaseOnDrop(gate.clone());
    let actor = ts
        .spawn_at::<QueuedRecorder>(
            QueuedRecorder {
                gate: gate.clone(),
                values: Vec::new(),
            },
            "/c1/queued",
            None,
            ThreadActorConfig {
                // 全部业务和最后的快照都在释放门之前入队。此处只测排序，
                // 背压容量和策略由独立C6契约验证。
                mailbox_capacity: Some((SENDERS * PER_SENDER + 1) as usize),
                backpressure_strategy: Some(BackpressureStrategy::Error),
                ..Default::default()
            },
        )
        .await
        .expect("spawn queued FIFO actor");
    let held_actor = actor.clone();
    let held = tokio::spawn(async move {
        held_actor
            .send_with_timeout(Box::new(QueuedMessage::Hold), None)
            .await
    });
    gate.entered.notified().await;
    let start = Arc::new(tokio::sync::Barrier::new(SENDERS as usize + 1));
    let mut producers = Vec::new();
    for sender in 0..SENDERS {
        let actor = actor.clone();
        let start = start.clone();
        producers.push(tokio::spawn(async move {
            start.wait().await;
            for sequence in 0..PER_SENDER {
                actor
                    .deliver(Box::new(QueuedMessage::Value { sender, sequence }))
                    .await
                    .expect("queued FIFO admission must succeed");
                if sequence % 7 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }
    start.wait().await;
    for producer in producers {
        producer.await.expect("queued FIFO producer panicked");
    }
    let (snapshot, result) = tokio::sync::oneshot::channel();
    actor
        .deliver(Box::new(QueuedMessage::Snapshot(snapshot)))
        .await
        .expect("queued FIFO snapshot must be admitted last");
    drop(release);
    let reply = held
        .await
        .expect("queued FIFO held caller panicked")
        .expect("queued FIFO held request failed");
    assert_eq!(
        *reply.downcast::<u64>().unwrap(),
        99,
        "queued FIFO held reply must be preserved"
    );
    let values = result
        .await
        .expect("queued FIFO snapshot must be delivered");
    assert_eq!(
        values.len(),
        (SENDERS * PER_SENDER) as usize,
        "queued FIFO snapshot must follow every admitted message"
    );
    for sender in 0..SENDERS {
        let sequence: Vec<u64> = values
            .iter()
            .filter(|(id, _)| *id == sender)
            .map(|(_, sequence)| *sequence)
            .collect();
        assert_eq!(
            sequence,
            (0..PER_SENDER).collect::<Vec<_>>(),
            "queued FIFO must preserve each sender's admitted sequence exactly"
        );
    }
    ts.stop_actor("/c1/queued")
        .await
        .expect("queued FIFO actor retirement");
}

#[test]
fn c1_c2_c3_exactly_once_fifo_integrity() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("corr1").await;
        const SENDERS: u64 = 8;
        const PER: u64 = 2_000;

        let journal = Journal::new();
        let actor = ts
            .spawn_at::<Recorder>(
                Recorder {
                    journal: journal.clone(),
                    started: Arc::new(AtomicU64::new(0)),
                },
                "/c1/rec",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();

        // 并发 ask（ask 保证处理完成才返回，用于精确统计）
        let mut hs = Vec::new();
        for s in 0..SENDERS {
            let a = actor.clone();
            hs.push(tokio::spawn(async move {
                for k in 0..PER {
                    let iters = 5_000 + (k % 7) * 1_000;
                    let rep = a
                        .ask(Box::new(Tagged {
                            sender: s,
                            seq: k,
                            iters,
                            salt: s * 1000 + k,
                        }) as BoxedMessage)
                        .await
                        .unwrap();
                    // C3a：ask 回复值也必须与参考实现一致（端到端校验）
                    let v = *rep.downcast::<u64>().unwrap();
                    assert_eq!(
                        v,
                        burn_cpu(iters, s * 1000 + k),
                        "ask reply must match reference (s={} k={})",
                        s,
                        k
                    );
                }
            }));
        }
        for h in hs {
            h.await.unwrap();
        }

        // C1：恰好一次 —— 无丢失、无重复
        let events = journal.events.lock().unwrap().clone();
        assert_eq!(
            events.len() as u64,
            SENDERS * PER,
            "exactly-once: total events"
        );
        let mut uniq = HashSet::new();
        for (s, k) in &events {
            assert!(uniq.insert((*s, *k)), "duplicate event ({},{})", s, k);
        }
        assert_eq!(uniq.len() as u64, SENDERS * PER);

        // C2：每发送者子序列严格 FIFO（actor 逐条处理 ⇒ seq 递增）
        for s in 0..SENDERS {
            let seqs: Vec<u64> = events
                .iter()
                .filter(|(ss, _)| *ss == s)
                .map(|(_, k)| *k)
                .collect();
            assert_eq!(seqs.len() as u64, PER, "sender {} count", s);
            for w in seqs.windows(2) {
                assert_eq!(w[1], w[0] + 1, "sender {} FIFO violated: {:?}", s, seqs);
            }
        }

        // C3b：日志记录的 burn 值按重放校验（顺序敏感性：同一输入序列）
        let mut expect_input = Vec::new();
        for (s, k) in &events {
            let iters = 5_000 + (k % 7) * 1_000;
            expect_input.push(burn_cpu(iters, s * 1000 + k));
        }
        assert_eq!(
            *journal.burn_results.lock().unwrap(),
            expect_input,
            "logged burn values must match replay"
        );

        println!(
            "[C1/C2/C3] {} events: exactly-once ✓ per-sender FIFO ✓ burn integrity ✓",
            SENDERS * PER
        );
        tokio::time::timeout(Duration::from_secs(15), queued_fifo_phase(&ts))
            .await
            .expect("queued FIFO exceeded its independent liveness watchdog");
        ts.shutdown_internal()
            .await
            .expect("C1 system shutdown must succeed");
    });
}

// ---------------------------------------------------------------------------
// C4：回复路由无串扰 —— 256 并发 asker 各自拿到自己的值
// ---------------------------------------------------------------------------

#[test]
fn c4_reply_routing_no_crosstalk() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("corr4").await;
        let journal = Journal::new();
        let actor = ts
            .spawn_at::<Recorder>(
                Recorder {
                    journal,
                    started: Arc::new(AtomicU64::new(0)),
                },
                "/c4/rec",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();

        const ASKERS: u64 = 256;
        let mut hs = Vec::new();
        for i in 0..ASKERS {
            let a = actor.clone();
            hs.push(tokio::spawn(async move {
                // 每个 asker 连发 3 个不同值，全部要求原值返回
                for k in 0..3u64 {
                    let token = i * 100 + k;
                    let rep = a
                        .ask(Box::new(EchoMsg { v: token }) as BoxedMessage)
                        .await
                        .unwrap();
                    let got = *rep.downcast::<u64>().unwrap();
                    assert_eq!(
                        got, token,
                        "reply crosstalk! asker {} token {} got {}",
                        i, token, got
                    );
                }
            }));
        }
        for h in hs {
            h.await.unwrap();
        }
        println!(
            "[C4] {} concurrent askers × 3 round-trips: zero crosstalk ✓",
            ASKERS
        );
        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// C5：跨模式等价 —— 三种调度模式同输入产生相同输出序列
// ---------------------------------------------------------------------------

#[test]
fn c5_mode_equivalence() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("corr5").await;

        // 确定性输入序列（固定种子）
        let inputs: Vec<(u64, u64)> = (0..500u64).map(|i| (5_000 + (i % 5) * 900, i)).collect();

        async fn run(
            ts: &Arc<ThreadActorSystem>,
            mode: ThreadActorConfig,
            path: &str,
            inputs: &[(u64, u64)],
        ) -> Vec<u64> {
            let journal = Journal::new();
            let a = ts.spawn_at::<Recorder>(Recorder { journal, started: Arc::new(AtomicU64::new(0)) }, path, None, mode).await.unwrap();
            let mut outs = Vec::new();
            for (iters, salt) in inputs {
                let rep = a.ask(Box::new(Tagged { sender: 0, seq: *salt, iters: *iters, salt: *salt }) as BoxedMessage).await.unwrap();
                outs.push(*rep.downcast::<u64>().unwrap());
            }
            outs
        }

        let shared = run(&ts, ThreadActorConfig::default(), "/c5/shared", &inputs).await;
        let dedicated = run(
            &ts,
            ThreadActorConfig { scheduling_mode: Some(SchedulingMode::DedicatedThread), ..Default::default() },
            "/c5/dedicated",
            &inputs,
        )
        .await;
        let sharded = run(
            &ts,
            ThreadActorConfig { scheduling_mode: Some(SchedulingMode::Sharded { affinity_key: "eq".into() }), ..Default::default() },
            "/c5/sharded",
            &inputs,
        )
        .await;

        assert_eq!(shared, dedicated, "SharedPool vs DedicatedThread outputs differ");
        assert_eq!(shared, sharded, "SharedPool vs Sharded outputs differ");
        // 且全部等于参考重放
        let reference: Vec<u64> = inputs.iter().map(|(i, s)| burn_cpu(*i, *s)).collect();
        assert_eq!(shared, reference, "outputs must equal reference replay");
        println!("[C5] 500-msg sequence identical across SharedPool/DedicatedThread/Sharded ✓ (matches reference)");
        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// C6：背压语义精确性
// ---------------------------------------------------------------------------

#[test]
fn c6_backpressure_semantics() {
    enum ContractMessage {
        Hold,
        Value(u64),
        Snapshot,
    }
    struct Gate {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        finished: AtomicU64,
    }
    struct ControlledRecorder {
        gate: Arc<Gate>,
        values: Vec<u64>,
    }
    impl Actor for ControlledRecorder {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            _c: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                match *message
                    .downcast::<ContractMessage>()
                    .expect("unexpected C6 message")
                {
                    ContractMessage::Hold => {
                        self.gate.entered.notify_one();
                        self.gate.release.notified().await;
                        self.gate.finished.store(1, Ordering::SeqCst);
                        Ok(Box::new(99u64) as BoxedMessage)
                    }
                    ContractMessage::Value(value) => {
                        self.values.push(value);
                        Ok(Box::new(value) as BoxedMessage)
                    }
                    ContractMessage::Snapshot => Ok(Box::new(self.values.clone()) as BoxedMessage),
                }
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }
    struct ReleaseOnDrop(Arc<Gate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release.notify_one();
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        tokio::time::timeout(Duration::from_secs(15), async move {
            let (_p, ts) = setup("corr6").await;
            for (strategy, path, count, expected) in [
                (
                    BackpressureStrategy::Error,
                    "/c6/err",
                    20u64,
                    vec![0u64, 1, 2, 3],
                ),
                (
                    BackpressureStrategy::DropOldest,
                    "/c6/dropold",
                    8u64,
                    vec![4u64, 5, 6, 7],
                ),
            ] {
                let rejects = matches!(strategy, BackpressureStrategy::Error);
                let gate = Arc::new(Gate {
                    entered: tokio::sync::Notify::new(),
                    release: tokio::sync::Notify::new(),
                    finished: AtomicU64::new(0),
                });
                let release = ReleaseOnDrop(gate.clone());
                let actor = ts
                    .spawn_at::<ControlledRecorder>(
                        ControlledRecorder {
                            gate: gate.clone(),
                            values: Vec::new(),
                        },
                        path,
                        None,
                        ThreadActorConfig {
                            mailbox_capacity: Some(4),
                            backpressure_strategy: Some(strategy),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                let held_actor = actor.clone();
                let held = tokio::spawn(async move {
                    held_actor
                        .send_with_timeout(Box::new(ContractMessage::Hold), None)
                        .await
                });
                gate.entered.notified().await;
                assert_eq!(gate.finished.load(Ordering::SeqCst), 0);

                // 门直到所有投递结束才释放，调用者的调度延迟不会改变满队列前提。
                for value in 0..count {
                    let result = actor.deliver(Box::new(ContractMessage::Value(value))).await;
                    if rejects && value >= 4 {
                        match result {
                            Err(ActorError::InternalError(error)) => assert!(
                                error.ends_with("Full { capacity: 4 }"),
                                "C6 every rejection must report Full with capacity 4: {error}"
                            ),
                            other => panic!(
                                "C6 excess Error delivery {value} must fail as Full: {other:?}"
                            ),
                        }
                    } else {
                        result.expect("C6 accepted delivery must succeed");
                    }
                }
                assert_eq!(gate.finished.load(Ordering::SeqCst), 0);
                drop(release);
                let reply = held
                    .await
                    .expect("C6 held caller panicked")
                    .expect("C6 held ask failed");
                assert_eq!(
                    *reply.downcast::<u64>().unwrap(),
                    99,
                    "C6 held reply must be preserved"
                );
                assert_eq!(gate.finished.load(Ordering::SeqCst), 1);

                // 查询使用Block策略，绝不能由查询自己挤掉尚未处理的业务。
                // 同一个普通FIFO队列的回复是排空屏障，比“处理至少四条”更严格。
                let snapshot = actor
                    .ask_with_strategy_and_timeout(
                        Box::new(ContractMessage::Snapshot),
                        BackpressureStrategy::Block,
                        Duration::from_secs(5),
                    )
                    .await
                    .expect("C6 FIFO snapshot must reply");
                assert_eq!(
                    *snapshot.downcast::<Vec<u64>>().unwrap(),
                    expected,
                    "C6 processed values must exactly match the admission policy"
                );

                let recovered = actor
                    .send_with_timeout(
                        Box::new(ContractMessage::Value(100)),
                        Some(Duration::from_secs(5)),
                    )
                    .await
                    .expect("C6 actor must accept new work after draining");
                assert_eq!(*recovered.downcast::<u64>().unwrap(), 100);
                let snapshot = actor
                    .ask_with_strategy_and_timeout(
                        Box::new(ContractMessage::Snapshot),
                        BackpressureStrategy::Block,
                        Duration::from_secs(5),
                    )
                    .await
                    .expect("C6 recovery snapshot must reply");
                let mut recovered_expected = expected;
                recovered_expected.push(100);
                assert_eq!(
                    *snapshot.downcast::<Vec<u64>>().unwrap(),
                    recovered_expected
                );
            }
            ts.shutdown_internal()
                .await
                .expect("C6 system shutdown failed");
        })
        .await
        .expect("C6 backpressure contract exceeded its independent liveness watchdog");
    });
}

// ---------------------------------------------------------------------------
// C7：停止语义
// ---------------------------------------------------------------------------

#[test]
fn c7_stop_semantics() {
    struct LifecycleState {
        values: Mutex<Vec<u64>>,
        hooks: AtomicU64,
        disposed: AtomicU64,
    }
    struct LifecycleRecorder(Arc<LifecycleState>);
    impl Drop for LifecycleRecorder {
        fn drop(&mut self) {
            self.0.disposed.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Actor for LifecycleRecorder {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                let echo = message
                    .downcast::<EchoMsg>()
                    .expect("unexpected C7 message");
                self.0.values.lock().unwrap().push(echo.v);
                Ok(Box::new(echo.v) as BoxedMessage)
            })
        }

        fn before_stop<'a>(
            &'a mut self,
            _ctx: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async move {
                self.0.hooks.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        tokio::time::timeout(Duration::from_secs(15), async move {
            let (_p, ts) = setup("corr7").await;
            let state = Arc::new(LifecycleState {
                values: Mutex::new(Vec::new()),
                hooks: AtomicU64::new(0),
                disposed: AtomicU64::new(0),
            });
            let actor = ts
                .spawn_at::<LifecycleRecorder>(
                    LifecycleRecorder(state.clone()),
                    "/c7/a",
                    None,
                    ThreadActorConfig::default(),
                )
                .await
                .unwrap();
            let retained_mailbox = ts
                .get_mailbox("/c7/a")
                .expect("C7 actor must be registered");
            for value in 0..10u64 {
                let reply = actor
                    .ask(Box::new(EchoMsg { v: value }) as BoxedMessage)
                    .await
                    .unwrap();
                assert_eq!(*reply.downcast::<u64>().unwrap(), value);
            }
            assert_eq!(
                *state.values.lock().unwrap(),
                (0..10u64).collect::<Vec<_>>()
            );

            ts.stop_actor("/c7/a").await.expect("C7 stop must succeed");
            // 成功stop本身就是退休屏障，不再以额外睡眠掩盖过早返回。
            assert_eq!(
                state.hooks.load(Ordering::SeqCst),
                1,
                "C7 stop must join its hook"
            );
            assert_eq!(
                state.disposed.load(Ordering::SeqCst),
                1,
                "C7 stop must dispose actor storage"
            );
            assert_eq!(ts.actor_count(), 0);
            assert!(!actor.is_alive().await);

            let ask = actor.ask(Box::new(EchoMsg { v: 99 }) as BoxedMessage).await;
            assert!(
                matches!(ask, Err(ActorError::Stopped)),
                "C7 post-stop ASK must report Stopped, got {ask:?}"
            );
            let tell = actor.deliver(Box::new(EchoMsg { v: 100 })).await;
            assert!(
                matches!(tell, Err(ActorError::Stopped)),
                "C7 post-stop TELL must report Stopped, got {tell:?}"
            );
            assert_eq!(
                *state.values.lock().unwrap(),
                (0..10u64).collect::<Vec<_>>()
            );

            // 保留mailbox及引用不能保留已停止actor的用户存储；系统shutdown也
            // 不能再次调用hook或析构。此处检查退休事实，不把有限采样当永久保证。
            ts.shutdown_internal()
                .await
                .expect("C7 system shutdown must succeed");
            drop(retained_mailbox);
            drop(actor);
            assert_eq!(state.hooks.load(Ordering::SeqCst), 1);
            assert_eq!(state.disposed.load(Ordering::SeqCst), 1);
            println!("[C7] stop joins hook and disposal; post-stop ASK/TELL report Stopped");
        })
        .await
        .expect("C7 retirement contract exceeded its independent liveness watchdog");
    });
}

// ---------------------------------------------------------------------------
// C8：超时语义
// ---------------------------------------------------------------------------

#[test]
fn c8_timeout_semantics() {
    struct Hold;
    struct Gate {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        finished: AtomicU64,
    }
    struct ControlledEcho(Arc<Gate>);
    impl Actor for ControlledEcho {
        type Config = EmptyConfig;
        type Context = ThreadContext<Self>;

        fn init<'a>(&'a mut self, _c: &'a mut Self::Context) -> BoxedFuture<'a, ActorResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn receive_message<'a>(
            &'a mut self,
            message: BoxedMessage,
            _c: &'a mut Self::Context,
        ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
            Box::pin(async move {
                if message.is::<Hold>() {
                    self.0.entered.notify_one();
                    self.0.release.notified().await;
                    self.0.finished.store(1, Ordering::SeqCst);
                    Ok(Box::new(99u64) as BoxedMessage)
                } else if let Some(echo) = message.downcast_ref::<EchoMsg>() {
                    Ok(Box::new(echo.v) as BoxedMessage)
                } else {
                    Err(ActorError::MessageHandlingError(
                        "unexpected C8 message".into(),
                    ))
                }
            })
        }

        fn state(&self) -> ActorState {
            ActorState::Running
        }
    }
    // 即使断言失败或外层watchdog取消测试，也释放正在等待的handler。
    struct ReleaseOnDrop(Arc<Gate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release.notify_one();
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        tokio::time::timeout(Duration::from_secs(10), async move {
            let (_p, ts) = setup("corr8").await;
            let gate = Arc::new(Gate {
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
                finished: AtomicU64::new(0),
            });
            let release = ReleaseOnDrop(gate.clone());
            let a = ts
                .spawn_at::<ControlledEcho>(
                    ControlledEcho(gate.clone()),
                    "/c8/a",
                    None,
                    ThreadActorConfig::default(),
                )
                .await
                .unwrap();

            // 成功路径验证实际回复。五秒只用于卡死保护，不是50ms性能承诺；
            // 已经完成的回复不会因为调用者随后被OS暂停而变为超时错误。
            let r = a
                .send_with_timeout(Box::new(EchoMsg { v: 7 }), Some(Duration::from_secs(5)))
                .await;
            assert_eq!(
                *r.expect("idle ask must reply").downcast::<u64>().unwrap(),
                7
            );

            // 持有真正的handler门直到观察到5ms超时；调用者被暂停也不会使
            // 前置任务自行完成。notify_one保留许可，不依赖两个线程的启动顺序。
            let ab = a.clone();
            let held =
                tokio::spawn(async move { ab.send_with_timeout(Box::new(Hold), None).await });
            gate.entered.notified().await;
            assert_eq!(gate.finished.load(Ordering::SeqCst), 0);
            let deadline = Duration::from_millis(5);
            let t1 = Instant::now();
            let r2 = a
                .send_with_timeout(Box::new(EchoMsg { v: 8 }), Some(deadline))
                .await;
            assert!(
                matches!(
                    &r2,
                    Err(ActorError::TimeoutDetail(_)) | Err(ActorError::Timeout)
                ),
                "blocked ask must time out, got {:?}",
                r2
            );
            assert!(
                t1.elapsed() >= deadline,
                "ask timed out before its deadline"
            );
            assert_eq!(gate.finished.load(Ordering::SeqCst), 0);

            drop(release);
            let reply = held
                .await
                .expect("held caller panicked")
                .expect("held ask failed");
            assert_eq!(*reply.downcast::<u64>().unwrap(), 99);
            assert_eq!(gate.finished.load(Ordering::SeqCst), 1);
            // Timeout不承诺撤回已经入队的业务；检查后续请求能独立得到自己的回复，
            // 不对超时请求是否已处理引入额外的取消语义。
            let resumed = a
                .send_with_timeout(Box::new(EchoMsg { v: 9 }), Some(Duration::from_secs(5)))
                .await
                .expect("actor must remain usable after caller timeout");
            assert_eq!(*resumed.downcast::<u64>().unwrap(), 9);
            ts.shutdown_internal()
                .await
                .expect("C8 system shutdown failed");
            println!(
                "[C8] timeout semantics: reply identity, held deadline and recovery verified ✓"
            );
        })
        .await
        .expect("C8 deadline contract exceeded its independent liveness watchdog");
    });
}
