//! M4 静态类型轨 —— **actix 引擎**侧语义与性能验证。
//!
//! 与 `test_m4_typed.rs`（thread 引擎）逐语义对齐：
//! - 静态 ask/tell 零 Any、多协议 `ref_for`、累积可见性
//! - `into_dyn` 桥接（装箱只在边界）
//! - 单协议便捷入口（`spawn_typed_single` + `SingleVariant` 包装）
//! - 双轨并存
//! - **并行**：多个静态轨 actor 分布在不同 arbiter 上真并行
//!
//! 引导方式与 `test_actix_adapter_coverage.rs` 一致：每个测试独立
//! `actix::System`（arbiter 池随 `ActixActorSystem::new` 建立）。

use parrot::actix as __parrot_engine;
use parrot::actix::ActixActorSystem;
use parrot::thread::typed::SingleVariant;
use parrot_api::ParrotTypedActor;
use parrot_api::message::Message;
use parrot_api::typed::TypedReceive;
use parrot_api::types::{ActorResult, BoxedFuture};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ============ 消息协议（与 thread 侧同款） ============

pub struct Add(u64);
pub struct Get;
pub struct Ping(String);

impl Message for Add {
    type Result = u64;
}
impl Message for Get {
    type Result = u64;
}
impl Message for Ping {
    type Result = String;
}

// ============ 静态 actor（derive 枚举信封） ============

#[derive(ParrotTypedActor)]
#[ParrotTypedActor(msgs(Add, Get, Ping))]
struct Calc {
    n: u64,
}

impl TypedReceive<Add> for Calc {
    fn receive_typed<'a>(&'a mut self, msg: Add) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move {
            self.n += msg.0;
            Ok(self.n)
        })
    }
}

impl TypedReceive<Get> for Calc {
    fn receive_typed<'a>(&'a mut self, _msg: Get) -> BoxedFuture<'a, ActorResult<u64>> {
        Box::pin(async move { Ok(self.n) })
    }
}

impl TypedReceive<Ping> for Calc {
    fn receive_typed<'a>(&'a mut self, msg: Ping) -> BoxedFuture<'a, ActorResult<String>> {
        Box::pin(async move { Ok(format!("{}:{}", msg.0, self.n)) })
    }
}

// ============ 测试 ============

/// 静态 ask/tell 零 Any、多协议 ref_for（actix 引擎）。
#[test]
fn m4_actix_static_track_ask_tell_zero_any() {
    actix::System::new().block_on(async {
        let sys = Arc::new(ActixActorSystem::new().await.expect("system"));

        let r: parrot::actix::ActixTypedActorRef<Calc, Add> = sys
            .spawn_typed(Calc { n: 0 }, "/m4a/calc")
            .await
            .expect("spawn");

        assert_eq!(r.ask(Add(41)).await.unwrap(), 41, "0+41=41");
        assert_eq!(r.ask(Add(41)).await.unwrap(), 82, "同一实例累积");

        let get_r = r.ref_for::<Get>();
        assert_eq!(get_r.ask(Get).await.unwrap(), 82, "Get 看到 Add 累积");

        r.tell(Add(1)).await.unwrap();
        assert_eq!(r.ask(Add(0)).await.unwrap(), 83, "tell 后再 ask 应看到累积");

        let ping_r = r.ref_for::<Ping>();
        assert_eq!(ping_r.ask(Ping("n".into())).await.unwrap(), "n:83");
        r.stop().await.unwrap();
        sys.as_ref().clone().shutdown().await.unwrap();
    });
}

/// `into_dyn` 桥接：装箱只发生在边界各一次（actix 引擎）。
#[test]
fn m4_actix_dual_track_bridge_into_dyn() {
    actix::System::new().block_on(async {
        let sys = Arc::new(ActixActorSystem::new().await.expect("system"));

        let r = sys
            .spawn_typed::<Calc, Add>(Calc { n: 0 }, "/m4a/calc2")
            .await
            .unwrap();

        let dyn_ref = r.into_dyn();

        let out = dyn_ref
            .send(Box::new(Add(5)) as parrot_api::types::BoxedMessage)
            .await
            .unwrap();
        assert_eq!(*out.downcast::<u64>().unwrap(), 5);

        dyn_ref
            .deliver(Box::new(Add(2)) as parrot_api::types::BoxedMessage)
            .await
            .unwrap();

        let out = dyn_ref
            .send(Box::new(Add(0)) as parrot_api::types::BoxedMessage)
            .await
            .unwrap();
        assert_eq!(
            *out.downcast::<u64>().unwrap(),
            7,
            "deliver 的 Add(2) 已生效"
        );

        // 类型不匹配 → 错误（不 panic）
        let bad = dyn_ref
            .send(Box::new("str") as parrot_api::types::BoxedMessage)
            .await;
        assert!(bad.is_err(), "mismatched type must error, not panic");
        dyn_ref.stop().await.unwrap();
        sys.as_ref().clone().shutdown().await.unwrap();
    });
}

/// 单协议便捷入口（`TypedReceive` 直达，无需 derive）。
#[test]
fn m4_actix_spawn_typed_single() {
    actix::System::new().block_on(async {
        let sys = Arc::new(ActixActorSystem::new().await.expect("system"));

        struct Doubler;
        impl TypedReceive<Add> for Doubler {
            fn receive_typed<'a>(&'a mut self, msg: Add) -> BoxedFuture<'a, ActorResult<u64>> {
                Box::pin(async move { Ok(msg.0 * 2) })
            }
        }

        let r = sys
            .spawn_typed_single(Doubler, "/m4a/doubler")
            .await
            .unwrap();
        assert_eq!(r.ask(SingleVariant::wrap(Add(21))).await.unwrap(), 42);
        assert_eq!(r.ask(SingleVariant::wrap(Add(0))).await.unwrap(), 0);
        r.stop().await.unwrap();
        sys.as_ref().clone().shutdown().await.unwrap();
    });
}

/// Four handlers must be active on four distinct OS threads before any is released.
/// The original arithmetic timing bound remains a non-regression check only:
/// a compiler may simplify this sum, so its duration alone cannot prove parallelism.
#[test]
fn m4_actix_static_actors_run_in_parallel() {
    use futures::FutureExt;
    use std::collections::HashSet;
    use std::sync::{
        Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    #[derive(Default)]
    struct Gate {
        released: Mutex<bool>,
        changed: Condvar,
    }
    impl Gate {
        fn wait(&self) {
            let released = self.released.lock().unwrap();
            // A broken observer must fail the test instead of hanging a worker.
            let (released, _) = self
                .changed
                .wait_timeout_while(released, Duration::from_secs(10), |released| !*released)
                .unwrap();
            assert!(*released, "observer did not release the overlap gate");
        }
        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }
    struct ReleaseOnDrop(Arc<Gate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    struct Observe {
        gate: Arc<Gate>,
        entered: mpsc::Sender<(usize, std::thread::ThreadId)>,
    }
    impl Message for Observe {
        type Result = usize;
    }
    struct Burn(u64);
    impl Message for Burn {
        type Result = u64;
    }
    #[derive(ParrotTypedActor)]
    #[ParrotTypedActor(msgs(Burn, Observe))]
    struct Burner {
        id: usize,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for Burner {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl TypedReceive<Observe> for Burner {
        fn receive_typed<'a>(&'a mut self, msg: Observe) -> BoxedFuture<'a, ActorResult<usize>> {
            Box::pin(async move {
                // No await/yield between announcing entry and blocking. One
                // arbiter cannot start its next actor until this gate opens.
                msg.entered
                    .send((self.id, std::thread::current().id()))
                    .unwrap();
                msg.gate.wait();
                Ok(self.id)
            })
        }
    }
    impl TypedReceive<Burn> for Burner {
        fn receive_typed<'a>(&'a mut self, msg: Burn) -> BoxedFuture<'a, ActorResult<u64>> {
            Box::pin(async move {
                let mut acc: u64 = 0;
                for i in 0..msg.0 {
                    acc = acc.wrapping_add(i);
                }
                Ok(acc)
            })
        }
    }
    fn expected_sum(n: u64) -> u64 {
        (u128::from(n) * u128::from(n.saturating_sub(1)) / 2) as u64
    }
    const ACTORS: usize = 4;
    actix::System::new().block_on(async {
        let sys = Arc::new(
            ActixActorSystem::with_arbiter_count(4)
                .await
                .expect("system"),
        );
        let dropped = Arc::new(AtomicUsize::new(0));
        let result = std::panic::AssertUnwindSafe(async {
            let probe = sys
                .spawn_typed::<Burner, Burn>(
                    Burner {
                        id: ACTORS,
                        dropped: dropped.clone(),
                    },
                    "/m4a/burn-probe",
                )
                .await
                .unwrap();
            let t = Instant::now();
            let probe_reply = probe.ask(Burn(2_000_000)).await.unwrap();
            let per_iter = t.elapsed().as_secs_f64() / 2_000_000.0;
            assert_eq!(
                probe_reply,
                expected_sum(2_000_000),
                "probe arithmetic result"
            );
            let work = ((0.06 / per_iter) as u64).max(1_000_000);
            let mut refs = Vec::new();
            for id in 0..ACTORS {
                refs.push(
                    sys.spawn_typed::<Burner, Burn>(
                        Burner {
                            id,
                            dropped: dropped.clone(),
                        },
                        &format!("/m4a/burn/{id}"),
                    )
                    .await
                    .unwrap(),
                );
            }

            let gate = Arc::new(Gate::default());
            let (entered, entries) = mpsc::channel();
            let release = ReleaseOnDrop(gate.clone());
            let observer = std::thread::spawn(move || {
                let _release = release;
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut observed = Vec::new();
                while observed.len() < ACTORS {
                    match entries.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(entry) => observed.push(entry),
                        Err(_) => break,
                    }
                }
                (observed, entries)
            });
            let observing: Vec<_> = refs
                .iter()
                .map(|reference| reference.ref_for::<Observe>())
                .collect();
            let replies = futures::future::join_all(observing.iter().map(|reference| {
                reference.ask_with_timeout(
                    Observe {
                        gate: gate.clone(),
                        entered: entered.clone(),
                    },
                    Duration::from_secs(15),
                )
            }))
            .await;
            let (observed, _entries) = observer.join().expect("independent overlap observer");
            for (id, reply) in replies.into_iter().enumerate() {
                assert_eq!(reply.unwrap(), id, "overlap handler identity");
            }
            assert_eq!(
                observed.len(),
                ACTORS,
                "all four handlers must enter before any gate is released"
            );
            assert_eq!(
                observed.iter().map(|(id, _)| *id).collect::<HashSet<_>>(),
                (0..ACTORS).collect()
            );
            assert_eq!(
                observed
                    .iter()
                    .map(|(_, thread)| *thread)
                    .collect::<HashSet<_>>()
                    .len(),
                ACTORS,
                "simultaneous handlers must occupy distinct OS threads"
            );

            let t0 = Instant::now();
            let results = futures::future::join_all(refs.iter().map(|r| r.ask(Burn(work)))).await;
            let parallel = t0.elapsed();
            for reply in results {
                assert_eq!(reply.unwrap(), expected_sum(work));
            }
            let one = sys
                .spawn_typed::<Burner, Burn>(
                    Burner {
                        id: ACTORS + 1,
                        dropped: dropped.clone(),
                    },
                    "/m4a/burn-serial",
                )
                .await
                .unwrap();
            let serial_work = work
                .checked_mul(ACTORS as u64)
                .expect("serial workload fits u64");
            let t1 = Instant::now();
            let serial_reply = one.ask(Burn(serial_work)).await.unwrap();
            let serial = t1.elapsed();
            assert_eq!(serial_reply, expected_sum(serial_work));
            let (ratio, slack) = if std::env::var("CARGO_LLVM_COV").is_ok() {
                (6.0, 0.5)
            } else {
                (1.2, 0.05)
            };
            assert!(
                parallel.as_secs_f64() < serial.as_secs_f64() * ratio + slack,
                "arithmetic timing regression: parallel={parallel:?} serial={serial:?}"
            );
            for reference in &refs {
                reference.stop().await.unwrap();
            }
            probe.stop().await.unwrap();
            one.stop().await.unwrap();
            assert_eq!(
                dropped.load(Ordering::SeqCst),
                ACTORS + 2,
                "stop must retire every actor"
            );
        })
        .catch_unwind()
        .await;
        let shutdown = sys.as_ref().clone().shutdown().await;
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
        shutdown.expect("parallel test system retirement");
        assert_eq!(dropped.load(Ordering::SeqCst), ACTORS + 2);
    });
}

/// 双轨并存：静态轨 actor 与动态轨（actix 原生）actor 同系统工作。
#[test]
fn m4_actix_dual_track_coexist() {
    use parrot::actix::{ActorBase, IntoActorBase};
    use parrot_api::actor::{Actor as ParrotActor, EmptyConfig};
    use parrot_api::address::ActorRefExt;
    use parrot_api_derive::{Message, ParrotActor};

    // 动态轨 actor 定义（derive 标准形态，与 adapter coverage 测试同款）
    #[derive(Clone, Debug, Message)]
    #[message(result = "u64")]
    struct DynAdd(u64);

    #[derive(ParrotActor)]
    #[ParrotActor(engine = "actix", config = "EmptyConfig")]
    struct DynCalc {
        v: u64,
    }

    impl IntoActorBase for DynCalc {
        fn into_actor_base(self) -> ActorBase<Self> {
            ActorBase::new(self)
        }
    }

    impl DynCalc {
        async fn handle_message(
            &mut self,
            msg: parrot_api::types::BoxedMessage,
            _ctx: &mut <Self as ParrotActor>::Context,
        ) -> ActorResult<parrot_api::types::BoxedMessage> {
            if let Some(m) = msg.downcast_ref::<DynAdd>() {
                self.v += m.0;
                return Ok(Box::new(self.v) as parrot_api::types::BoxedMessage);
            }
            Err(parrot_api::errors::ActorError::MessageHandlingError(
                "unknown".into(),
            ))
        }

        fn handle_message_engine(
            &mut self,
            msg: parrot_api::types::BoxedMessage,
            _ctx: &mut <Self as ParrotActor>::Context,
            _engine_ctx: parrot_api::actor::EngineContextHandle,
        ) -> Option<ActorResult<parrot_api::types::BoxedMessage>> {
            if let Some(m) = msg.downcast_ref::<DynAdd>() {
                self.v += m.0;
                return Some(Ok(Box::new(self.v) as parrot_api::types::BoxedMessage));
            }
            None
        }
    }

    actix::System::new().block_on(async {
        let sys = Arc::new(ActixActorSystem::new().await.expect("system"));

        // 静态轨
        let typed_r = sys
            .spawn_typed::<Calc, Add>(Calc { n: 100 }, "/m4a/typed")
            .await
            .unwrap();
        assert_eq!(typed_r.ask(Add(1)).await.unwrap(), 101);

        // 动态轨（actix 原生 spawn，存量路径）
        let dyn_ref = sys
            .spawn_root_typed(DynCalc { v: 0 }, EmptyConfig)
            .await
            .expect("dynamic spawn");

        let v: u64 = dyn_ref.ask(DynAdd(41)).await.expect("dynamic ask");
        assert_eq!(v, 41);

        // 静态轨继续工作（动态轨活动不影响静态轨）
        assert_eq!(typed_r.ask(Add(1)).await.unwrap(), 102);
        assert_eq!(dyn_ref.ask::<DynAdd>(DynAdd(1)).await.unwrap(), 42);
        typed_r.stop().await.unwrap();
        dyn_ref.stop().await.unwrap();
        sys.as_ref().clone().shutdown().await.unwrap();
    });
}
