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
        let _ = ts.shutdown_internal().await;
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
                Recorder { journal, started: Arc::new(AtomicU64::new(0)) },
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
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("corr6").await;

        // --- Error 策略：容量 4，堵住后恰好拒收超额数 ---
        let ops_e = Arc::new(AtomicU64::new(0));
        struct Blocky {
            ops: Arc<AtomicU64>,
        }
        impl Actor for Blocky {
            type Config = EmptyConfig;
            type Context = ThreadContext<Self>;
            fn init<'a>(
                &'a mut self,
                _c: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<()>> {
                Box::pin(async { Ok(()) })
            }
            fn receive_message<'a>(
                &'a mut self,
                m: BoxedMessage,
                _c: &'a mut Self::Context,
            ) -> BoxedFuture<'a, ActorResult<BoxedMessage>> {
                let ops = self.ops.clone();
                Box::pin(async move {
                    if let Some(t) = m.downcast_ref::<Tagged>() {
                        ops.fetch_add(1, Ordering::Relaxed);
                        let v = burn_cpu(t.iters, t.salt);
                        ops.fetch_add(1, Ordering::Relaxed);
                        Ok(Box::new(v) as BoxedMessage)
                    } else {
                        Ok(m)
                    }
                })
            }
            fn state(&self) -> ActorState {
                ActorState::Running
            }
        }
        let cfg_err = ThreadActorConfig {
            mailbox_capacity: Some(4),
            backpressure_strategy: Some(BackpressureStrategy::Error),
            ..Default::default()
        };
        let a_err = ts
            .spawn_at::<Blocky>(Blocky { ops: ops_e.clone() }, "/c6/err", None, cfg_err)
            .await
            .unwrap();

        // 堵门：后台长任务（fire-and-forget ask，占住 handler）
        {
            let ab = a_err.clone();
            tokio::spawn(async move {
                let _ = ab
                    .ask(Box::new(Tagged {
                        sender: 0,
                        seq: 0,
                        iters: calibrated_iters(1.2),
                        salt: 1,
                    }) as BoxedMessage)
                    .await;
            });
        }
        let started = wait_until(
            || ops_e.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(10),
            Duration::from_millis(5),
        )
        .await;
        assert!(started);

        // 灌 20：in-flight = handler 内 1 条；容量 4 ⇒ 前 4 条 OK，后 16 条 Full
        let mut ok = 0;
        let mut full = 0;
        let mut full_cap = None;
        for i in 0..20u64 {
            match a_err.deliver(Box::new(EchoMsg { v: i })).await {
                Ok(_) => ok += 1,
                Err(ActorError::InternalError(e)) => {
                    full += 1;
                    if full_cap.is_none() && e.contains("Full") {
                        full_cap = Some(e);
                    }
                }
                Err(e) => panic!("unexpected error kind: {:?}", e),
            }
        }
        assert_eq!(
            ok, 4,
            "capacity-4 mailbox must accept exactly 4 while blocked (got {})",
            ok
        );
        assert_eq!(full, 16, "exactly the excess must be rejected");
        assert!(full_cap.is_some(), "rejections must be MailboxError::Full");
        println!("[C6] Error strategy: exact accept=4 reject=16 (cap=4) with Full error ✓");

        // --- DropOldest：容量 4，灌 8 条（值 0..8），应保留最新 4 条（4..8）---
        let gate_started2 = Arc::new(AtomicU64::new(0));
        let cfg_do = ThreadActorConfig {
            mailbox_capacity: Some(4),
            backpressure_strategy: Some(BackpressureStrategy::DropOldest),
            ..Default::default()
        };
        let journal2 = Journal::new();
        let a_do = ts
            .spawn_at::<Recorder>(
                Recorder {
                    journal: journal2.clone(),
                    started: gate_started2.clone(),
                },
                "/c6/dropold",
                None,
                cfg_do,
            )
            .await
            .unwrap();
        let gate = {
            let ab = a_do.clone();
            tokio::spawn(async move {
                let _ = ab
                    .ask(Box::new(Tagged {
                        sender: 7,
                        seq: 0,
                        iters: calibrated_iters(1.0),
                        salt: 1,
                    }) as BoxedMessage)
                    .await;
            })
        };
        let gate_in_handler = wait_until(
            || gate_started2.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(30),
            Duration::from_millis(10),
        )
        .await;
        assert!(
            gate_in_handler,
            "gate Tagged(7,0) must enter handler before flooding (started flag)"
        );
        for i in 0..8u64 {
            let _ = a_do.deliver(Box::new(EchoMsg { v: i })).await;
        }
        // 等堵门任务完成并排空邮箱后才可断言
        let _ = gate.await;
        let drained = wait_until(
            || {
                let ev = journal2.events.lock().unwrap();
                ev.iter().filter(|(s, _)| *s == u64::MAX).count() >= 4
            },
            Duration::from_secs(30),
            Duration::from_millis(50),
        )
        .await;
        assert!(
            drained,
            "retained messages must be processed after gate completes; got {:?}",
            journal2
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|(s, _)| *s == u64::MAX)
                .collect::<Vec<_>>()
        );
        // EchoMsg 事件记为 (u64::MAX, v)
        let kept: Vec<u64> = journal2
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| *s == u64::MAX)
            .map(|(_, v)| *v)
            .collect();
        assert_eq!(
            kept.len(),
            4,
            "DropOldest must retain exactly cap=4 (got {:?})",
            kept
        );
        assert_eq!(
            kept,
            vec![4, 5, 6, 7],
            "DropOldest must retain the NEWEST 4 (got {:?})",
            kept
        );
        println!("[C6] DropOldest: 8 into cap-4 keeps newest [4,5,6,7] ✓");

        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// C7：停止语义
// ---------------------------------------------------------------------------

#[test]
fn c7_stop_semantics() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("corr7").await;
        let journal = Journal::new();
        let a = ts
            .spawn_at::<Recorder>(
                Recorder {
                    journal: journal.clone(),
                    started: Arc::new(AtomicU64::new(0)),
                },
                "/c7/a",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();

        // 先正常处理 10 条
        for i in 0..10u64 {
            let rep = a
                .ask(Box::new(EchoMsg { v: i }) as BoxedMessage)
                .await
                .unwrap();
            assert_eq!(*rep.downcast::<u64>().unwrap(), i);
        }
        let before = journal.events.lock().unwrap().len();

        // 停止
        ts.stop_actor("/c7/a").await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 停止后 ask 必须失败
        let r = a.ask(Box::new(EchoMsg { v: 99 }) as BoxedMessage).await;
        assert!(r.is_err(), "ask after stop must fail, got {:?}", r);

        // 再等待一段时间，事件数不得增长（无幽灵处理）
        tokio::time::sleep(Duration::from_millis(300)).await;
        let after = journal.events.lock().unwrap().len();
        assert_eq!(after, before, "no ghost processing after stop");
        println!("[C7] stop semantics: post-stop ask fails, no ghost processing ✓");
        let _ = ts.shutdown_internal().await;
    });
}

// ---------------------------------------------------------------------------
// C8：超时语义
// ---------------------------------------------------------------------------

#[test]
fn c8_timeout_semantics() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let (_p, ts) = setup("corr8").await;
        let journal = Journal::new();
        let a_started = Arc::new(AtomicU64::new(0));
        let a = ts
            .spawn_at::<Recorder>(
                Recorder { journal, started: a_started.clone() },
                "/c8/a",
                None,
                ThreadActorConfig::default(),
            )
            .await
            .unwrap();

        // 空闲：50ms 超时的 echo 必须成功
        let t0 = Instant::now();
        let r = a
            .send_with_timeout(Box::new(EchoMsg { v: 7 }), Some(Duration::from_millis(50)))
            .await;
        assert!(r.is_ok(), "idle ask with 50ms timeout must succeed");
        assert!(
            t0.elapsed() < Duration::from_millis(50),
            "must not have hit the timeout"
        );

        // 阻塞：前置 ~1.2s 校准任务 + 5ms 超时探测 ⇒ 必须超时且及时返回
        // 确定性门控：等 heavy 真正进入 handler（Recorder.started 置位），
        // 消除 sleep(300ms) 在插桩/慢机下的时序抖动。
        let heavy_started = Arc::new(AtomicU64::new(0));
        let ab = a.clone();
        tokio::spawn(async move {
            let _ = ab
                .ask(Box::new(Tagged {
                    sender: 0,
                    seq: 0,
                    iters: calibrated_iters(1.5),
                    salt: 1,
                }) as BoxedMessage)
                .await;
        });
        let started = wait_until(
            || a_started.load(Ordering::Relaxed) >= 1,
            Duration::from_secs(30),
            Duration::from_millis(2),
        )
        .await;
        assert!(started, "heavy must enter handler before probing");
        let _ = heavy_started;
        let t1 = Instant::now();
        let r2 = a
            .send_with_timeout(Box::new(EchoMsg { v: 8 }), Some(Duration::from_millis(5)))
            .await;
        assert!(
            matches!(
                &r2,
                Err(ActorError::TimeoutDetail(_)) | Err(ActorError::Timeout)
            ),
            "blocked ask must time out, got {:?}",
            r2
        );
        let el = t1.elapsed();
        assert!(
            el < Duration::from_millis(500),
            "timeout must return promptly, took {:?}",
            el
        );
        println!(
            "[C8] timeout semantics: idle succeeds fast, blocked times out promptly ({:.0}ms) ✓",
            el.as_secs_f64() * 1000.0
        );
        let _ = ts.shutdown_internal().await;
    });
}
